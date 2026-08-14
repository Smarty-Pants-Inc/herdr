use std::io::{self, BufRead, Read as _, Write as _};
use std::net::{Shutdown, TcpListener};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::Digest as _;

use super::{do_handshake, init_logging, initial_terminal_geometry, write_to_server};
use crate::protocol::{
    self, ClientLaunchMode, ClientMessage, OmpControlAction, OmpFrameDirection,
    OmpRendererCapabilities, OmpRendererRequest, RenderEncoding, ServerMessage, MAX_FRAME_SIZE,
};
use crate::server::socket_paths::client_socket_path;
use interprocess::TryClone as _;

const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
const SERVER_TO_GUEST_QUEUE_CAPACITY: usize = 64;
#[derive(Deserialize)]
struct GuestRecord {
    t: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    frame: Option<Box<RawValue>>,
    #[serde(default)]
    mutation: bool,
}
struct OmpGuestChild(std::process::Child);

impl std::ops::Deref for OmpGuestChild {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OmpGuestChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OmpGuestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn send_to_guest_or_fail_close<T>(
    sender: &mpsc::SyncSender<T>,
    message: T,
    fail_close: impl FnOnce(),
) -> bool {
    match sender.try_send(message) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_) | mpsc::TrySendError::Disconnected(_)) => {
            fail_close();
            false
        }
    }
}

fn encode_guest_payload(payload: &[u8]) -> io::Result<Vec<u8>> {
    protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, payload)
        .map_err(|error| io::Error::other(error.to_string()))
}

fn validate_guest_record(record: &GuestRecord) -> io::Result<()> {
    match record.t.as_str() {
        "frame" if record.frame.is_some() => Ok(()),
        "frame" => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "OMP guest bridge frame missing payload",
        )),
        "control"
            if matches!(
                record.action.as_deref(),
                Some("request-controller" | "release-controller")
            ) =>
        {
            Ok(())
        }
        "control" => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid OMP guest bridge control",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown OMP guest bridge record type",
        )),
    }
}

fn finished_worker_result(
    worker: &mut Option<std::thread::JoinHandle<io::Result<()>>>,
) -> Option<io::Result<()>> {
    worker
        .as_ref()
        .is_some_and(|worker| worker.is_finished())
        .then(|| {
            worker
                .take()
                .expect("finished worker exists")
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("OMP guest bridge worker panicked")))
        })
}

fn stop_and_join_guest_forwarder(
    interrupt_guest_reader: impl FnOnce(),
    worker: &mut Option<std::thread::JoinHandle<io::Result<()>>>,
) -> Option<io::Result<()>> {
    interrupt_guest_reader();
    worker.take().map(|worker| {
        worker
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("OMP guest bridge worker panicked")))
    })
}

fn shutdown_herdr_socket(stream: &crate::ipc::LocalStream) {
    #[cfg(unix)]
    match stream {
        crate::ipc::LocalStream::UdSocket(stream) => {
            let _ = stream.inner().shutdown(Shutdown::Both);
        }
    }
}

fn read_candidate_announcement(
    stream: &mut std::net::TcpStream,
    deadline: Instant,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        check_cancelled()?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        stream.set_read_timeout(Some(
            Duration::from_millis(20).min(deadline.saturating_duration_since(now)),
        ))?;
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(String::from_utf8(bytes).ok());
                }
                if bytes.len() >= 4096 {
                    return Ok(None);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

fn accept_omp_guest(
    listener: &TcpListener,
    token: &str,
    deadline: Instant,
    candidate_timeout: Duration,
    mut child_status: impl FnMut() -> io::Result<Option<std::process::ExitStatus>>,
) -> io::Result<(std::net::TcpStream, io::BufReader<std::net::TcpStream>)> {
    loop {
        if let Some(status) = child_status()? {
            return Err(io::Error::other(format!(
                "OMP guest bridge exited before connecting: {status}"
            )));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OMP guest bridge did not connect within 30 seconds",
            ));
        }
        match listener.accept() {
            Ok((guest, _)) => {
                // Darwin inherits O_NONBLOCK from the listener; bridge readers require blocking IO.
                guest.set_nonblocking(false)?;
                let Ok(mut read_stream) = guest.try_clone() else {
                    continue;
                };
                let candidate_deadline = Instant::now()
                    + candidate_timeout.min(deadline.saturating_duration_since(Instant::now()));
                let announced =
                    read_candidate_announcement(&mut read_stream, candidate_deadline, || {
                        if let Some(status) = child_status()? {
                            Err(io::Error::other(format!(
                                "OMP guest bridge exited before connecting: {status}"
                            )))
                        } else {
                            Ok(())
                        }
                    })?;
                let valid = announced.as_deref().is_some_and(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .is_some_and(|record| {
                            record.get("t").and_then(serde_json::Value::as_str) == Some("guest")
                                && record.get("token").and_then(serde_json::Value::as_str)
                                    == Some(token)
                        })
                });
                if !valid || read_stream.set_read_timeout(None).is_err() {
                    continue;
                }
                return Ok((guest, io::BufReader::new(read_stream)));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Runs the hidden OMP logical-pane client. The spawned OMP guest owns stdin,
/// stdout, terminal sizing, and rendering; Herdr carries only semantic frames.
pub(super) fn run(
    pane_id: String,
    omp_session_id: String,
    route_generation: u64,
    target_app_client_id: Option<u64>,
) -> io::Result<()> {
    init_logging();
    let mut stream = crate::ipc::connect_local_stream(&client_socket_path())?;
    let (cols, rows, _, _, _) = initial_terminal_geometry(false);
    do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::SemanticFrame,
        ClientLaunchMode::OmpPane,
        &super::load_client_identity_or_exit(),
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    write_to_server(
        &mut stream,
        &ClientMessage::OmpPaneAttach {
            pane_id: pane_id.clone(),
            omp_session_id: omp_session_id.clone(),
            route_generation,
            target_app_client_id,
            renderer_capabilities: OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_request: OmpRendererRequest::Independent,
        },
    )?;

    let (initial_attachment_epoch, _controller) = loop {
        match protocol::read_message::<_, ServerMessage>(&mut stream, MAX_FRAME_SIZE)
            .map_err(|error| io::Error::other(error.to_string()))?
        {
            ServerMessage::OmpPane {
                pane_id: attached_pane,
                omp_session_id: attached_session,
                route_generation: attached_generation,
                attachment_epoch,
                controller,
                ..
            } if attached_pane == pane_id
                && attached_session == omp_session_id
                && attached_generation == route_generation =>
            {
                break (attachment_epoch, controller)
            }
            ServerMessage::OmpError { code, message, .. } => {
                return Err(io::Error::other(format!("{code}: {message}")));
            }
            _ => {}
        }
    };
    let attachment_epoch = Arc::new(AtomicU64::new(initial_attachment_epoch));

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    let room_identity = format!(
        "{}\0{pane_id}\0{omp_session_id}\0{route_generation}\0{initial_attachment_epoch}",
        client_socket_path().display()
    );
    let room_id = format!(
        "herdr-{}",
        sha2::Sha256::digest(room_identity.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut bridge_token_bytes = [0u8; 32];
    getrandom::fill(&mut bridge_token_bytes)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let bridge_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bridge_token_bytes);
    let omp_bin = std::env::var("OMP_BIN").unwrap_or_else(|_| "omp".to_owned());
    let mut child = OmpGuestChild(
        Command::new(omp_bin)
            .args([
                "__collab-guest-bridge",
                &address,
                &room_id,
                &bridge_token,
                "--no-tools",
                "--no-lsp",
                "--no-skills",
                "--no-rules",
                "--no-extensions",
            ])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    listener.set_nonblocking(true)?;
    let accept_deadline = Instant::now() + Duration::from_secs(30);
    let (guest, mut guest_reader) = accept_omp_guest(
        &listener,
        &bridge_token,
        accept_deadline,
        Duration::from_secs(5),
        || child.try_wait(),
    )?;
    guest.set_nodelay(true)?;
    let guest_reader_shutdown = guest_reader.get_ref().try_clone()?;

    let mut server_writer = stream.try_clone()?;
    let write_pane = pane_id.clone();
    let write_session = omp_session_id.clone();
    let write_epoch = Arc::clone(&attachment_epoch);
    let mut guest_to_server = Some(std::thread::spawn(move || -> io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = guest_reader
                .by_ref()
                .take(MAX_RECORD_BYTES + 1)
                .read_line(&mut line)?;
            if read == 0 {
                break;
            }
            if read as u64 > MAX_RECORD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OMP guest bridge record too large",
                ));
            }
            let record: GuestRecord = serde_json::from_str(&line).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid OMP guest bridge record: {error}"),
                )
            })?;
            validate_guest_record(&record)?;
            let message = if record.t == "control" {
                let action = match record.action.as_deref() {
                    Some("request-controller") => OmpControlAction::RequestController,
                    Some("release-controller") => OmpControlAction::ReleaseController,
                    _ => unreachable!("guest record validated above"),
                };
                ClientMessage::OmpControl {
                    pane_id: write_pane.clone(),
                    omp_session_id: write_session.clone(),
                    route_generation,
                    attachment_epoch: write_epoch.load(Ordering::Acquire),
                    action,
                }
            } else {
                let frame = record.frame.expect("guest record validated above");
                let envelope = encode_guest_payload(frame.get().as_bytes())?;
                if record.mutation {
                    ClientMessage::OmpControl {
                        pane_id: write_pane.clone(),
                        omp_session_id: write_session.clone(),
                        route_generation,
                        attachment_epoch: write_epoch.load(Ordering::Acquire),
                        action: OmpControlAction::Mutation { frame: envelope },
                    }
                } else {
                    ClientMessage::OmpFrame {
                        pane_id: write_pane.clone(),
                        omp_session_id: write_session.clone(),
                        route_generation,
                        attachment_epoch: write_epoch.load(Ordering::Acquire),
                        frame: envelope,
                    }
                }
            };
            write_to_server(&mut server_writer, &message)?;
        }
        Ok(())
    }));

    let (server_tx, server_rx) = mpsc::sync_channel(SERVER_TO_GUEST_QUEUE_CAPACITY);
    let mut server_reader = stream.try_clone()?;
    let guest_shutdown = guest.try_clone()?;
    let server_to_guest = std::thread::spawn(move || {
        while let Ok(message) =
            protocol::read_message::<_, ServerMessage>(&mut server_reader, MAX_FRAME_SIZE)
        {
            if !send_to_guest_or_fail_close(&server_tx, message, || {
                shutdown_herdr_socket(&server_reader);
                let _ = guest_shutdown.shutdown(Shutdown::Both);
            }) {
                break;
            }
        }
    });

    let mut guest_forward_error = None;
    let mut guest_writer = guest;
    let loop_result = loop {
        if let Some(result) = finished_worker_result(&mut guest_to_server) {
            if let Err(error) = result {
                guest_forward_error = Some(error);
            }
            break Ok(());
        }
        match child.try_wait() {
            Ok(Some(_)) => break Ok(()),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
        match server_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::OmpFrame {
                pane_id: frame_pane,
                omp_session_id: frame_session,
                route_generation: frame_generation,
                attachment_epoch: frame_epoch,
                frame,
            }) if frame_pane == pane_id
                && frame_session == omp_session_id
                && frame_generation == route_generation
                && frame_epoch == attachment_epoch.load(Ordering::Acquire) =>
            {
                let payload =
                    match protocol::validate_omp_frame(&frame, OmpFrameDirection::HostToGuest) {
                        Ok(payload) => payload,
                        Err(error) => break Err(io::Error::other(error.to_string())),
                    };
                if let Err(error) = guest_writer
                    .write_all(br#"{"t":"frame","fromPeer":0,"frame":"#)
                    .and_then(|()| guest_writer.write_all(payload))
                    .and_then(|()| guest_writer.write_all(b"}\n"))
                    .and_then(|()| guest_writer.flush())
                {
                    break Err(error);
                }
            }
            Ok(ServerMessage::OmpPane {
                pane_id: updated_pane,
                omp_session_id: updated_session,
                route_generation: updated_generation,
                attachment_epoch: updated_epoch,
                state,
                ..
            }) if updated_pane == pane_id
                && updated_session == omp_session_id
                && updated_generation == route_generation =>
            {
                attachment_epoch.store(updated_epoch, Ordering::Release);
                if matches!(state, crate::protocol::OmpPaneState::Failed { .. }) {
                    eprintln!("herdr OMP: host unavailable");
                    break Ok(());
                }
            }
            Ok(ServerMessage::OmpError { code, message, .. }) => {
                eprintln!("herdr OMP: {code}: {message}");
            }
            Ok(ServerMessage::ServerShutdown { reason }) => {
                eprintln!(
                    "herdr server stopped{}",
                    reason.map(|value| format!(": {value}")).unwrap_or_default()
                );
                break Ok(());
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    if let Some(Err(error)) = stop_and_join_guest_forwarder(
        || {
            let _ = guest_reader_shutdown.shutdown(Shutdown::Both);
        },
        &mut guest_to_server,
    ) {
        if guest_forward_error.is_none() {
            guest_forward_error = Some(error);
        }
    }
    let final_attachment_epoch = attachment_epoch.load(Ordering::Acquire);
    let _ = write_to_server(
        &mut stream,
        &ClientMessage::OmpPaneDetach {
            pane_id,
            omp_session_id,
            route_generation,
            attachment_epoch: final_attachment_epoch,
        },
    );
    let _ = write_to_server(&mut stream, &ClientMessage::Detach);
    let _ = crate::ipc::shutdown_local_stream_write(&stream);
    shutdown_herdr_socket(&stream);
    drop(guest_writer);
    let _ = child.kill();
    let _ = server_to_guest.join();
    match loop_result {
        Err(error) => Err(error),
        Ok(()) => match guest_forward_error {
            Some(error) => Err(error),
            None => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trickling_candidate_expires_before_authenticated_guest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let trickle = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            for byte in b"{\"t\":" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let valid = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(180));
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream
                .write_all(b"{\"t\":\"guest\",\"token\":\"expected\"}\n")
                .unwrap();
            stream
        });

        let (accepted, mut reader) = accept_omp_guest(
            &listener,
            "expected",
            Instant::now() + Duration::from_secs(2),
            Duration::from_millis(80),
            || Ok(None),
        )
        .unwrap();
        let mut writer = valid.join().unwrap();
        writer.write_all(b"later\n").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "later\n");

        accepted.shutdown(Shutdown::Both).unwrap();
        line.clear();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        trickle.join().unwrap();
    }

    #[test]
    fn guest_frame_keeps_raw_json_spacing_and_key_order() {
        let record = r#"{"t":"frame","frame": { "z" : 1, "a" : [ 2,3 ] }}"#;
        let record: GuestRecord = serde_json::from_str(record).unwrap();
        assert!(!record.mutation);
        let frame = record.frame.expect("frame record carries raw payload");
        assert_eq!(frame.get(), r#"{ "z" : 1, "a" : [ 2,3 ] }"#);
        let envelope =
            protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, frame.get().as_bytes())
                .unwrap();
        assert_eq!(
            protocol::validate_omp_frame(&envelope, OmpFrameDirection::GuestToHost).unwrap(),
            frame.get().as_bytes(),
        );
    }

    #[test]
    fn malformed_unknown_and_missing_native_guest_records_fail_closed() {
        for record in [
            "not-json",
            r#"{"t":"unknown","frame":{}}"#,
            r#"{"t":"frame"}"#,
            r#"{"t":"control","action":"unknown"}"#,
        ] {
            let result = serde_json::from_str::<GuestRecord>(record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                .and_then(|record| validate_guest_record(&record));
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn oversized_guest_payload_fails_the_forwarder() {
        let payload = vec![b' '; protocol::MAX_OMP_FRAME_PAYLOAD + 1];
        assert!(encode_guest_payload(&payload).is_err());
    }

    #[test]
    fn guest_worker_failure_is_observed_by_main_forwarder() {
        let mut worker = Some(std::thread::spawn(|| {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized guest payload",
            ))
        }));
        while worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            std::thread::yield_now();
        }
        let error = finished_worker_result(&mut worker).unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(worker.is_none());
    }

    #[test]
    fn cleanup_frames_wait_for_inflight_guest_frame() {
        use parking_lot::{Condvar, Mutex};

        type BlockingWriteState = (Vec<u8>, bool, bool);
        type SharedBlockingWriteState = Arc<(Mutex<BlockingWriteState>, Condvar)>;

        #[derive(Clone)]
        struct BlockingWriter {
            state: SharedBlockingWriteState,
        }

        impl std::io::Write for BlockingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (state, ready) = &*self.state;
                let mut state = state.lock();
                if !state.1 {
                    state.1 = true;
                    ready.notify_all();
                    while !state.2 {
                        ready.wait(&mut state);
                    }
                }
                state.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let state = Arc::new((Mutex::new((Vec::new(), false, false)), Condvar::new()));
        let mut cleanup_writer = BlockingWriter {
            state: Arc::clone(&state),
        };
        let mut guest_writer = cleanup_writer.clone();
        let mut guest_to_server = Some(std::thread::spawn(move || {
            protocol::write_message(
                &mut guest_writer,
                &ClientMessage::OmpFrame {
                    pane_id: "pane".into(),
                    omp_session_id: "session".into(),
                    route_generation: 1,
                    attachment_epoch: 1,
                    frame: encode_guest_payload(b"guest").unwrap(),
                },
            )
            .map_err(|error| io::Error::other(error.to_string()))
        }));
        let (state_lock, ready) = &*state;
        let mut guard = state_lock.lock();
        while !guard.1 {
            ready.wait(&mut guard);
        }
        drop(guard);

        stop_and_join_guest_forwarder(
            || {
                let mut state = state_lock.lock();
                state.2 = true;
                ready.notify_all();
            },
            &mut guest_to_server,
        )
        .unwrap()
        .unwrap();
        protocol::write_message(&mut cleanup_writer, &ClientMessage::Detach).unwrap();

        let bytes = state_lock.lock().0.clone();
        let mut bytes = bytes.as_slice();
        assert!(matches!(
            protocol::read_message::<_, ClientMessage>(&mut bytes, MAX_FRAME_SIZE).unwrap(),
            ClientMessage::OmpFrame { .. }
        ));
        assert!(matches!(
            protocol::read_message::<_, ClientMessage>(&mut bytes, MAX_FRAME_SIZE).unwrap(),
            ClientMessage::Detach
        ));
        assert!(bytes.is_empty());
    }

    #[test]
    fn full_server_to_guest_queue_fails_closed() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(()).unwrap();
        let closed = std::sync::atomic::AtomicBool::new(false);

        assert!(!send_to_guest_or_fail_close(&tx, (), || {
            closed.store(true, Ordering::Release);
        }));
        assert!(closed.load(Ordering::Acquire));
    }

    #[test]
    fn disconnected_server_to_guest_queue_fails_closed() {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        drop(rx);
        let closed = std::sync::atomic::AtomicBool::new(false);

        assert!(!send_to_guest_or_fail_close(&tx, (), || {
            closed.store(true, Ordering::Release);
        }));
        assert!(closed.load(Ordering::Acquire));
    }
}
