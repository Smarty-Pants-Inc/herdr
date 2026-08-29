//! Private loopback bridge between OMP processes and Herdr's semantic route.

use serde::Deserialize;
use serde_json::value::RawValue;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;

use crate::server::client_transport::{OmpHostAdmission, ServerEvent};

static NEXT_HOST_ID: AtomicU64 = AtomicU64::new(1);
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
const ANNOUNCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ROUTE_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HOST_OUTBOUND_QUEUE_CAPACITY: usize = 64;

const MAX_UNAUTHENTICATED_HANDSHAKES: usize = 32;

pub(crate) struct HandshakeLimiter {
    permits: std_mpsc::Receiver<()>,
    returned: std_mpsc::SyncSender<()>,
}

impl HandshakeLimiter {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "handshake capacity must be positive");
        let (returned, permits) = std_mpsc::sync_channel(capacity);
        for _ in 0..capacity {
            returned.send(()).expect("empty handshake permit channel");
        }
        Self { permits, returned }
    }

    fn try_acquire(&self) -> Option<HandshakePermit> {
        self.permits
            .try_recv()
            .ok()
            .map(|()| HandshakePermit(self.returned.clone()))
    }
}

pub(crate) fn handshake_limiter() -> HandshakeLimiter {
    HandshakeLimiter::new(MAX_UNAUTHENTICATED_HANDSHAKES)
}

struct HandshakePermit(std_mpsc::SyncSender<()>);

impl Drop for HandshakePermit {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn read_record(reader: &mut BufReader<TcpStream>, line: &mut String) -> io::Result<usize> {
    line.clear();
    let read = reader.by_ref().take(MAX_RECORD_BYTES + 1).read_line(line)?;
    if read as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "OMP bridge record too large",
        ));
    }
    Ok(read)
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum HostRecord {
    #[serde(rename = "host")]
    Host {
        #[serde(rename = "paneId")]
        pane_id: String,
        #[serde(rename = "ompSessionId")]
        omp_session_id: String,
        #[serde(rename = "routeGeneration")]
        route_generation: u64,
        token: String,
        #[serde(default, rename = "ompBuildId")]
        omp_build_id: String,
    },
}

#[derive(Deserialize)]
struct HostFrameRecord {
    t: String,
    #[serde(rename = "targetPeer")]
    target_peer: u64,
    frame: Box<RawValue>,
}

pub(crate) fn bind() -> io::Result<(TcpListener, crate::pane::OmpBridgeEnv)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.to_string();
    let bridge = crate::pane::OmpBridgeEnv::generate(address)?;
    Ok((listener, bridge))
}

pub(crate) fn accept_pending(
    listener: &TcpListener,
    bridge: &crate::pane::OmpBridgeEnv,
    event_tx: tokio::sync::mpsc::Sender<ServerEvent>,
    handshakes: &HandshakeLimiter,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let Some(permit) = handshakes.try_acquire() else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                if let Err(err) = stream.set_nonblocking(false) {
                    tracing::warn!(err = %err, "OMP bridge stream setup failed");
                    continue;
                }
                spawn_host(
                    stream,
                    bridge.clone(),
                    event_tx.clone(),
                    permit,
                    crate::build_info::omp_build_id(),
                );
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
            Err(err) => {
                tracing::warn!(err = %err, "OMP bridge accept failed");
                return;
            }
        }
    }
}

fn spawn_host(
    mut stream: TcpStream,
    bridge: crate::pane::OmpBridgeEnv,
    event_tx: tokio::sync::mpsc::Sender<ServerEvent>,
    permit: HandshakePermit,
    expected_omp_build_id: Option<&'static str>,
) {
    std::thread::spawn(move || {
        let Ok(read_stream) = stream.try_clone() else {
            return;
        };
        if read_stream
            .set_read_timeout(Some(ANNOUNCE_TIMEOUT))
            .is_err()
        {
            return;
        }
        let mut reader = BufReader::new(read_stream);
        let mut line = String::new();
        if read_record(&mut reader, &mut line)
            .ok()
            .filter(|read| *read > 0)
            .is_none()
        {
            write_host_error(
                &mut stream,
                "host-announcement-invalid",
                "OMP host announcement was missing or exceeded the deadline",
            );
            return;
        }
        let Ok(HostRecord::Host {
            pane_id,
            omp_session_id,
            mut route_generation,
            token,
            omp_build_id,
        }) = serde_json::from_str(&line)
        else {
            write_host_error(
                &mut stream,
                "host-announcement-invalid",
                "OMP host announcement is invalid",
            );
            return;
        };
        let announced_generation = route_generation;
        if !bridge.validates(&pane_id, &token) {
            tracing::warn!(pane_id, "rejected unauthenticated OMP bridge host");
            write_host_error(
                &mut stream,
                "host-authentication-failed",
                "OMP host bridge token was rejected",
            );
            return;
        }
        if expected_omp_build_id.is_some_and(|expected| omp_build_id != expected) {
            let expected = expected_omp_build_id.expect("paired build ID checked above");
            tracing::warn!(
                pane_id,
                expected_omp_build_id = expected,
                announced_omp_build_id = omp_build_id,
                "rejected mismatched OMP bridge host build"
            );
            let message = format!(
                "OMP build mismatch: Herdr requires {expected}, host announced {}",
                if omp_build_id.is_empty() {
                    "no build ID"
                } else {
                    omp_build_id.as_str()
                }
            );
            write_host_error(&mut stream, "omp-build-mismatch", &message);
            return;
        }
        drop(permit);
        if reader.get_mut().set_read_timeout(None).is_err() {
            write_host_error(
                &mut stream,
                "host-announcement-invalid",
                "OMP host bridge could not clear its announcement timeout",
            );
            return;
        }
        let Ok(socket) = reader.get_ref().try_clone() else {
            write_host_error(
                &mut stream,
                "server-unavailable",
                "Herdr could not retain the OMP host bridge",
            );
            return;
        };
        let host_id = NEXT_HOST_ID.fetch_add(1, Ordering::Relaxed);
        let (outbound, outbound_rx) =
            std_mpsc::sync_channel::<String>(HOST_OUTBOUND_QUEUE_CAPACITY);
        let (admission, admitted) = std_mpsc::sync_channel(1);
        if event_tx
            .blocking_send(ServerEvent::OmpHostStarted {
                pane_id: pane_id.clone(),
                omp_session_id: omp_session_id.clone(),
                route_generation,
                host_id,
                outbound,
                socket,
                admission,
            })
            .is_err()
        {
            write_host_error(
                &mut stream,
                "server-unavailable",
                "Herdr is not accepting OMP host routes",
            );
            return;
        }
        match admitted.recv_timeout(ROUTE_ADMISSION_TIMEOUT) {
            Ok(OmpHostAdmission::Accepted {
                route_generation: assigned_generation,
            }) => {
                route_generation = assigned_generation;
                if write_host_ready(&mut stream, route_generation).is_err() {
                    let _ = stream.shutdown(Shutdown::Both);
                    notify_host_stopped(
                        &event_tx,
                        &pane_id,
                        &omp_session_id,
                        announced_generation,
                        host_id,
                        false,
                    );
                    return;
                }
            }
            Ok(OmpHostAdmission::Rejected { code, message }) => {
                write_host_error(&mut stream, &code, &message);
                notify_host_stopped(
                    &event_tx,
                    &pane_id,
                    &omp_session_id,
                    announced_generation,
                    host_id,
                    false,
                );
                return;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                write_host_error(
                    &mut stream,
                    "route-admission-timeout",
                    "Herdr did not admit the OMP host route before the deadline",
                );
                notify_host_stopped(
                    &event_tx,
                    &pane_id,
                    &omp_session_id,
                    announced_generation,
                    host_id,
                    false,
                );
                return;
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                write_host_error(
                    &mut stream,
                    "server-unavailable",
                    "Herdr stopped before admitting the OMP host route",
                );
                notify_host_stopped(
                    &event_tx,
                    &pane_id,
                    &omp_session_id,
                    announced_generation,
                    host_id,
                    false,
                );
                return;
            }
        }
        std::thread::spawn(move || {
            for line in outbound_rx {
                if stream
                    .write_all(line.as_bytes())
                    .and_then(|_| stream.write_all(b"\n"))
                    .and_then(|_| stream.flush())
                    .is_err()
                {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        while let Ok(read) = read_record(&mut reader, &mut line) {
            if read == 0 {
                break;
            }
            let Ok(HostFrameRecord {
                t,
                target_peer,
                frame,
            }) = serde_json::from_str::<HostFrameRecord>(&line)
            else {
                tracing::warn!("invalid OMP host record; closing bridge");
                break;
            };
            if t != "frame" {
                tracing::warn!(record_type = %t, "unexpected OMP host record; closing bridge");
                break;
            }
            let payload = frame.get().as_bytes();
            let frame = match crate::protocol::encode_omp_frame(
                crate::protocol::OmpFrameDirection::HostToGuest,
                payload,
            ) {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::warn!(%error, "invalid OMP host frame; closing bridge");
                    break;
                }
            };
            let _ = event_tx.blocking_send(ServerEvent::OmpHostFrame {
                pane_id: pane_id.clone(),
                omp_session_id: omp_session_id.clone(),
                route_generation,
                host_id,
                target_client_id: (target_peer != 0).then_some(target_peer),
                frame,
            });
        }
        notify_host_stopped(
            &event_tx,
            &pane_id,
            &omp_session_id,
            route_generation,
            host_id,
            true,
        );
    });
}

fn notify_host_stopped(
    event_tx: &tokio::sync::mpsc::Sender<ServerEvent>,
    pane_id: &str,
    omp_session_id: &str,
    route_generation: u64,
    host_id: u64,
    ready: bool,
) {
    let _ = event_tx.blocking_send(ServerEvent::OmpHostStopped {
        pane_id: pane_id.to_owned(),
        omp_session_id: omp_session_id.to_owned(),
        route_generation,
        host_id,
        ready,
    });
}

fn write_host_ready(stream: &mut TcpStream, route_generation: u64) -> io::Result<()> {
    let record = serde_json::json!({
        "t": "ready",
        "routeGeneration": route_generation,
    });
    writeln!(stream, "{record}").and_then(|_| stream.flush())
}

fn write_host_error(stream: &mut TcpStream, code: &str, message: &str) {
    let record = serde_json::json!({
        "t": "error",
        "code": code,
        "message": message,
    });
    if let Err(error) = writeln!(stream, "{record}").and_then(|_| stream.flush()) {
        tracing::debug!(%error, "failed to report OMP host bridge rejection");
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub(crate) fn guest_record(
    from_peer: u64,
    frame: &[u8],
    display_name: &str,
    display_name_revision: u64,
) -> Option<String> {
    let frame = std::str::from_utf8(frame).ok()?;
    serde_json::from_str::<serde::de::IgnoredAny>(frame).ok()?;
    Some(format!(
        r#"{{"t":"frame","fromPeer":{from_peer},"displayName":{},"displayNameRevision":{display_name_revision},"frame":{frame}}}"#,
        serde_json::to_string(display_name).expect("display name is serializable"),
    ))
}

pub(crate) fn peer_authority_record(peer: u64, can_write: bool) -> String {
    format!(r#"{{"t":"peer-authority","peer":{peer},"canWrite":{can_write}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_record_adds_attribution_as_sibling_metadata_without_rewriting_frame() {
        let opaque_frame = br#"{"t":"prompt","text":"keep [bytes] exact"}"#;
        let record = guest_record(7, opaque_frame, "Ada", 3).expect("valid JSON payload");
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["fromPeer"], 7);
        assert_eq!(record["displayName"], "Ada");
        assert_eq!(record["displayNameRevision"], 3);
        assert_eq!(
            record["frame"],
            serde_json::from_slice::<serde_json::Value>(opaque_frame).unwrap()
        );

        let renamed = guest_record(7, opaque_frame, "Grace", 4).expect("valid JSON payload");
        let renamed: serde_json::Value = serde_json::from_str(&renamed).unwrap();
        assert_eq!(renamed["displayName"], "Grace");
        assert_eq!(renamed["displayNameRevision"], 4);
        assert_eq!(renamed["frame"], record["frame"]);
        assert!(guest_record(7, b"not-json", "Ada", 3).is_none());
    }

    #[test]
    fn unauthenticated_handshake_cap_closes_excess_and_admits_after_release() {
        let (listener, bridge) = bind().unwrap();
        let limiter = HandshakeLimiter::new(1);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        let address = listener.local_addr().unwrap();

        let first = TcpStream::connect(address).unwrap();
        accept_pending(&listener, &bridge, event_tx.clone(), &limiter);

        let mut excess = TcpStream::connect(address).unwrap();
        accept_pending(&listener, &bridge, event_tx.clone(), &limiter);
        excess
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        let mut byte = [0];
        match excess.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("excess handshake remained open: {result:?}"),
        }

        first.shutdown(Shutdown::Both).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if let Some(permit) = limiter.try_acquire() {
                drop(permit);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "handshake permit was not released"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mut later = TcpStream::connect(address).unwrap();
        let token = bridge.token("pane");
        writeln!(
            later,
            r#"{{"t":"host","paneId":"pane","ompSessionId":"session","routeGeneration":1,"token":"{token}"}}"#
        )
        .unwrap();
        accept_pending(&listener, &bridge, event_tx, &limiter);
        let admission = match event_rx.blocking_recv() {
            Some(ServerEvent::OmpHostStarted { admission, .. }) => admission,
            event => panic!("expected OMP host start, got {event:?}"),
        };
        admission
            .send(OmpHostAdmission::Accepted {
                route_generation: 1,
            })
            .unwrap();
        later
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut ready = String::new();
        BufReader::new(later.try_clone().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["t"], "ready");
        assert_eq!(ready["routeGeneration"], 1);
    }

    #[test]
    fn mismatched_omp_build_is_rejected_before_route_activation() {
        let (listener, bridge) = bind().unwrap();
        listener.set_nonblocking(false).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let token = bridge.token("pane");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_host(
            server,
            bridge,
            event_tx,
            HandshakeLimiter::new(1).try_acquire().unwrap(),
            Some("required-build"),
        );

        writeln!(
            client,
            r#"{{"t":"host","paneId":"pane","ompSessionId":"session","routeGeneration":1,"token":"{token}","ompBuildId":"other-build"}}"#
        )
        .unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut response = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["code"], "omp-build-mismatch");
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn host_route_admission_rejection_reports_structured_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let bridge = crate::pane::OmpBridgeEnv::generate("unused".into()).unwrap();
        let token = bridge.token("pane");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        spawn_host(
            server,
            bridge,
            event_tx,
            HandshakeLimiter::new(1).try_acquire().unwrap(),
            None,
        );

        writeln!(
            client,
            r#"{{"t":"host","paneId":"pane","ompSessionId":"session","routeGeneration":1,"token":"{token}"}}"#
        )
        .unwrap();
        let admission = match event_rx.blocking_recv() {
            Some(ServerEvent::OmpHostStarted { admission, .. }) => admission,
            event => panic!("expected OMP host start, got {event:?}"),
        };
        admission
            .send(OmpHostAdmission::Rejected {
                code: "route_busy".into(),
                message: "OMP host route is already active".into(),
            })
            .unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut response = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["t"], "error");
        assert_eq!(response["code"], "route_busy");
        assert_eq!(response["message"], "OMP host route is already active");
    }

    #[test]
    fn host_frame_keeps_raw_json_spacing_and_key_order() {
        let record = r#"{"t":"frame","targetPeer":9,"frame": { "z" : 1, "a" : [ 2,3 ] }}"#;
        let HostFrameRecord {
            t,
            target_peer,
            frame,
        } = serde_json::from_str(record).unwrap();
        assert_eq!(t, "frame");
        assert_eq!(target_peer, 9);
        let envelope = crate::protocol::encode_omp_frame(
            crate::protocol::OmpFrameDirection::HostToGuest,
            frame.get().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            crate::protocol::validate_omp_frame(
                &envelope,
                crate::protocol::OmpFrameDirection::HostToGuest,
            )
            .unwrap(),
            frame.get().as_bytes(),
        );
    }

    #[test]
    fn oversized_host_json_payload_cannot_form_an_envelope() {
        let payload = vec![b' '; crate::protocol::MAX_OMP_FRAME_PAYLOAD + 1];
        assert!(matches!(
            crate::protocol::encode_omp_frame(
                crate::protocol::OmpFrameDirection::HostToGuest,
                &payload,
            ),
            Err(crate::protocol::OmpFrameError::Oversized { .. })
        ));
    }

    #[test]
    fn oversized_host_record_stops_the_bridge() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let bridge = crate::pane::OmpBridgeEnv::generate("unused".into()).unwrap();
        let token = bridge.token("pane");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
        spawn_host(
            server,
            bridge,
            event_tx,
            HandshakeLimiter::new(1).try_acquire().unwrap(),
            None,
        );

        writeln!(
            client,
            r#"{{"t":"host","paneId":"pane","ompSessionId":"session","routeGeneration":2,"token":"{token}"}}"#
        )
        .unwrap();
        let admission = match event_rx.blocking_recv() {
            Some(ServerEvent::OmpHostStarted {
                route_generation: 2,
                admission,
                ..
            }) => admission,
            event => panic!("expected OMP host start, got {event:?}"),
        };
        admission
            .send(OmpHostAdmission::Accepted {
                route_generation: 3,
            })
            .unwrap();
        let mut ready = String::new();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["t"], "ready");
        assert_eq!(ready["routeGeneration"], 3);

        let payload = "x".repeat(crate::protocol::MAX_OMP_FRAME_PAYLOAD);
        let _ = writeln!(
            client,
            r#"{{"t":"frame","targetPeer":0,"frame":"{payload}"}}"#
        );
        assert!(matches!(
            event_rx.blocking_recv(),
            Some(ServerEvent::OmpHostStopped {
                route_generation: 3,
                ..
            })
        ));
    }
}
