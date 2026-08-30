//! Server-owned private OMP guest PTY and authenticated loopback bridge.

use std::io::{self, BufRead, BufReader, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::Digest as _;
use tokio::sync::{mpsc as tokio_mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::pane::{AgentDetection, PaneLaunchEnv};
use crate::render_signal::RenderSignal;
use crate::server::omp_route::OmpRouteKey;
use crate::terminal::TerminalRuntime;
use crate::terminal_theme::{HostAppearance, TerminalTheme};

/// Maximum bytes in one guest NDJSON record, including its newline.
/// Normal OMP graphics frames exceed one MiB, so this deliberately permits two.
pub(crate) const MAX_PRIVATE_OMP_RECORD_BYTES: u64 = 2 * 1024 * 1024;

const MAX_ANNOUNCE_BYTES: u64 = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const OUTBOUND_QUEUE_CAPACITY: usize = 256;
const OMP_GUEST_BRIDGE_TOKEN_ENV: &str = "HERDR_OMP_GUEST_BRIDGE_TOKEN";

const INBOUND_QUEUE_CAPACITY: usize = 256;

/// Static inputs for a server-private OMP guest and its private PTY.
pub(crate) struct PrivateOmpGuestConfig {
    pub(crate) route: OmpRouteKey,
    /// Retained through launch so managed identity can be reverified at the spawn boundary.
    pub(crate) omp_executable: crate::update::OmpExecutable,
    pub(crate) attachment_epoch: u64,
    pub(crate) controller: bool,
    pub(crate) pane_id: PaneId,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) cwd: PathBuf,
    pub(crate) launch_env: PaneLaunchEnv,
    pub(crate) scrollback_limit_bytes: usize,
    pub(crate) terminal_theme: TerminalTheme,
    pub(crate) terminal_appearance: Option<HostAppearance>,
    pub(crate) events: tokio_mpsc::Sender<AppEvent>,
    pub(crate) render_notify: Arc<Notify>,
    pub(crate) render_dirty: Arc<RenderSignal>,
}

/// An OMP guest request received over the private bridge.
#[derive(Debug)]
pub(crate) enum PrivateOmpGuestRecord {
    Frame {
        frame: Box<RawValue>,
        mutation: bool,
    },
    Control(PrivateOmpGuestControl),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivateOmpGuestControl {
    RequestController,
    ReleaseController,
}

/// A standalone terminal runtime plus the server's authenticated OMP bridge.
///
/// All socket work is background-threaded. Shutdown signals those threads and
/// releases the PTY, but never joins a reader that may be blocked in socket IO.
pub(crate) struct PrivateOmpGuest {
    route: OmpRouteKey,
    runtime_pane_id: PaneId,
    attachment_epoch: AtomicU64,
    controller: AtomicBool,
    runtime: Option<TerminalRuntime>,
    _listener: Arc<TcpListener>,
    guest: Arc<Mutex<Option<TcpStream>>>,
    outbound: mpsc::SyncSender<OutboundRecord>,
    inbound: mpsc::Receiver<PrivateOmpGuestRecord>,
    bridge_ready: Arc<AtomicBool>,
    bridge_failed: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    #[cfg(test)]
    _test_input: Option<tokio_mpsc::Receiver<bytes::Bytes>>,
    #[cfg(test)]
    _test_outbound: Option<mpsc::Receiver<OutboundRecord>>,
    #[cfg(test)]
    _test_inbound: Option<mpsc::SyncSender<PrivateOmpGuestRecord>>,
}

impl PrivateOmpGuest {
    pub(crate) fn spawn(config: PrivateOmpGuestConfig) -> io::Result<Self> {
        config.omp_executable.verify().map_err(io::Error::other)?;
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0")?);
        listener.set_nonblocking(true)?;
        let token = bridge_token()?;
        let argv = guest_argv(
            config.omp_executable.executable().to_path_buf(),
            listener.local_addr()?.to_string(),
            room_id(&config.route, config.attachment_epoch),
        );
        let launch_env = omp_guest_launch_env(config.launch_env, token.clone());
        let runtime = TerminalRuntime::spawn_argv_command(
            config.pane_id,
            config.rows,
            config.cols,
            config.cwd,
            &argv,
            &launch_env,
            AgentDetection::Disabled,
            config.scrollback_limit_bytes,
            config.terminal_theme,
            config.terminal_appearance,
            config.events,
            config.render_notify,
            config.render_dirty,
        )?;
        let (outbound, outbound_rx) = mpsc::sync_channel(OUTBOUND_QUEUE_CAPACITY);
        let (inbound_tx, inbound) = mpsc::sync_channel(INBOUND_QUEUE_CAPACITY);
        let shutting_down = Arc::new(AtomicBool::new(false));
        let bridge_ready = Arc::new(AtomicBool::new(false));
        let bridge_failed = Arc::new(AtomicBool::new(false));
        let guest = Arc::new(Mutex::new(None));
        spawn_bridge_thread(
            Arc::clone(&listener),
            token,
            Arc::clone(&guest),
            outbound_rx,
            inbound_tx,
            Arc::clone(&bridge_ready),
            Arc::clone(&bridge_failed),
            Arc::clone(&shutting_down),
        );
        Ok(Self {
            route: config.route,
            runtime_pane_id: config.pane_id,
            attachment_epoch: AtomicU64::new(config.attachment_epoch),
            controller: AtomicBool::new(config.controller),
            runtime: Some(runtime),
            _listener: listener,
            guest,
            outbound,
            inbound,
            bridge_ready,
            bridge_failed,
            shutting_down,
            #[cfg(test)]
            _test_input: None,
            #[cfg(test)]
            _test_outbound: None,
            #[cfg(test)]
            _test_inbound: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn spawn_test_stub(config: PrivateOmpGuestConfig) -> io::Result<Self> {
        config.omp_executable.verify().map_err(io::Error::other)?;
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0")?);
        listener.set_nonblocking(true)?;
        let (runtime, test_input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            config.cols,
            config.rows,
            config.scrollback_limit_bytes,
            &[],
            4,
        );
        let (outbound, test_outbound) = mpsc::sync_channel(OUTBOUND_QUEUE_CAPACITY);
        let (test_inbound, inbound) = mpsc::sync_channel(INBOUND_QUEUE_CAPACITY);
        Ok(Self {
            route: config.route,
            runtime_pane_id: config.pane_id,
            attachment_epoch: AtomicU64::new(config.attachment_epoch),
            controller: AtomicBool::new(config.controller),
            runtime: Some(runtime),
            _listener: listener,
            guest: Arc::new(Mutex::new(None)),
            outbound,
            inbound,
            bridge_ready: Arc::new(AtomicBool::new(false)),
            bridge_failed: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            _test_input: Some(test_input),
            _test_outbound: Some(test_outbound),
            _test_inbound: Some(test_inbound),
        })
    }

    pub(crate) fn runtime(&self) -> &TerminalRuntime {
        self.runtime
            .as_ref()
            .expect("private OMP runtime is live until shutdown")
    }
    pub(crate) fn runtime_pane_id(&self) -> PaneId {
        self.runtime_pane_id
    }
    pub(crate) fn route(&self) -> &OmpRouteKey {
        &self.route
    }
    pub(crate) fn attachment_epoch(&self) -> u64 {
        self.attachment_epoch.load(Ordering::Acquire)
    }
    pub(crate) fn set_attachment_epoch(&self, epoch: u64) {
        self.attachment_epoch.store(epoch, Ordering::Release)
    }
    pub(crate) fn set_controller(&self, controller: bool) {
        self.controller.store(controller, Ordering::Release)
    }

    pub(crate) fn input(&self, bytes: Bytes) -> Result<(), tokio_mpsc::error::TrySendError<Bytes>> {
        match &self.runtime {
            Some(runtime) => runtime.try_send_bytes(bytes),
            None => Err(tokio_mpsc::error::TrySendError::Closed(bytes)),
        }
    }

    pub(crate) fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        if let Some(runtime) = &self.runtime {
            runtime.resize(rows, cols, cell_width_px, cell_height_px);
        }
    }

    pub(crate) fn drain_guest_records(&mut self) -> Vec<PrivateOmpGuestRecord> {
        self.inbound.try_iter().collect()
    }

    /// True after bridge setup, reader, or writer failure. Explicit teardown
    /// does not set this, so callers can distinguish an expected shutdown.
    pub(crate) fn bridge_failed(&self) -> bool {
        self.bridge_failed.load(Ordering::Acquire)
    }

    /// True once the guest has authenticated and the private route can receive input.
    pub(crate) fn bridge_ready(&self) -> bool {
        self.bridge_ready.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_set_bridge_ready(&self) {
        self.bridge_ready.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn test_input_is_empty(&mut self) -> bool {
        self._test_input
            .as_mut()
            .is_none_or(|input| input.try_recv().is_err())
    }

    /// Writes an already formed host bridge record verbatim, followed by one newline.
    pub(crate) fn send_host_frame(&self, record: &str) -> io::Result<()> {
        self.queue_outbound(OutboundRecord::Raw(record.to_owned()))
    }

    /// Bounded teardown: no join is attempted for potentially blocked socket threads.
    pub(crate) fn shutdown(&mut self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.outbound.try_send(OutboundRecord::Shutdown);
        if let Ok(mut guest) = self.guest.lock() {
            if let Some(stream) = guest.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }

    fn queue_outbound(&self, record: OutboundRecord) -> io::Result<()> {
        self.outbound.try_send(record).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => io::Error::new(
                io::ErrorKind::WouldBlock,
                "OMP guest outbound queue is full",
            ),
            mpsc::TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "OMP guest bridge is unavailable")
            }
        })
    }
}

impl Drop for PrivateOmpGuest {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum OutboundRecord {
    Raw(String),
    Shutdown,
}

#[derive(Deserialize)]
struct GuestAnnouncement {
    t: String,
    token: String,
}

#[derive(Deserialize)]
struct GuestRecordWire {
    t: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    frame: Option<Box<RawValue>>,
    #[serde(default)]
    mutation: bool,
}

fn bridge_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn guest_argv(omp_executable: PathBuf, address: String, room: String) -> Vec<String> {
    vec![
        omp_executable.to_string_lossy().into_owned(),
        "__collab-guest-bridge".into(),
        address,
        room,
        "--token-env".into(),
        "--no-tools".into(),
        "--no-lsp".into(),
        "--no-skills".into(),
        "--no-rules".into(),
        "--no-extensions".into(),
    ]
}

fn omp_guest_launch_env(launch_env: PaneLaunchEnv, token: String) -> PaneLaunchEnv {
    launch_env
        .without_env("BUN_OPTIONS")
        .without_env("BUN_INSPECT_PRELOAD")
        .without_env("BUN_BE_BUN")
        .without_env("NODE_OPTIONS")
        .without_env("HERDR_OMP_BRIDGE")
        .without_env("HERDR_OMP_BRIDGE_TOKEN")
        .without_env(crate::integration::HERDR_PANE_ID_ENV_VAR)
        .with_extra(OMP_GUEST_BRIDGE_TOKEN_ENV, token)
}

fn room_id(route: &OmpRouteKey, attachment_epoch: u64) -> String {
    let identity = format!(
        "{}\0{}\0{}\0{attachment_epoch}",
        route.pane_id, route.omp_session_id, route.route_generation
    );
    format!("herdr-{:x}", sha2::Sha256::digest(identity.as_bytes()))
}

fn spawn_bridge_thread(
    listener: Arc<TcpListener>,
    token: String,
    guest: Arc<Mutex<Option<TcpStream>>>,
    outbound: mpsc::Receiver<OutboundRecord>,
    inbound: mpsc::SyncSender<PrivateOmpGuestRecord>,
    bridge_ready: Arc<AtomicBool>,
    bridge_failed: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let Ok((stream, reader)) = accept_authenticated_guest(&listener, &token, &shutting_down)
        else {
            fail_bridge(&guest, &bridge_failed, &shutting_down);
            return;
        };
        if stream.set_nodelay(true).is_err() {
            fail_bridge(&guest, &bridge_failed, &shutting_down);
            return;
        }
        if let Ok(mut slot) = guest.lock() {
            *slot = Some(stream);
        } else {
            fail_bridge(&guest, &bridge_failed, &shutting_down);
            return;
        }
        bridge_ready.store(true, Ordering::Release);
        let reader_failed = Arc::clone(&bridge_failed);
        let reader_shutdown = Arc::clone(&shutting_down);
        let reader_guest = Arc::clone(&guest);
        std::thread::spawn(move || {
            if let Err(error) = read_guest_records(reader, inbound, Arc::clone(&reader_shutdown)) {
                tracing::warn!(%error, "private OMP guest reader failed");
            }
            fail_bridge(&reader_guest, &reader_failed, &reader_shutdown);
        });
        write_guest_records(Arc::clone(&guest), outbound, Arc::clone(&shutting_down));
        fail_bridge(&guest, &bridge_failed, &shutting_down);
    });
}

fn fail_bridge(
    guest: &Mutex<Option<TcpStream>>,
    bridge_failed: &AtomicBool,
    shutting_down: &AtomicBool,
) {
    if shutting_down.load(Ordering::Acquire) {
        return;
    }
    bridge_failed.store(true, Ordering::Release);
    if let Ok(mut slot) = guest.lock() {
        if let Some(stream) = slot.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn read_candidate_announcement(
    stream: &mut TcpStream,
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
            ACCEPT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
        ))?;
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(String::from_utf8(bytes).ok());
                }
                if bytes.len() as u64 >= MAX_ANNOUNCE_BYTES {
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

fn accept_authenticated_guest(
    listener: &TcpListener,
    token: &str,
    shutting_down: &AtomicBool,
) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
    accept_authenticated_guest_with_timeouts(
        listener,
        token,
        shutting_down,
        CONNECT_TIMEOUT,
        CANDIDATE_TIMEOUT,
    )
}

fn accept_authenticated_guest_with_timeouts(
    listener: &TcpListener,
    token: &str,
    shutting_down: &AtomicBool,
    connect_timeout: Duration,
    candidate_timeout: Duration,
) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
    let deadline = Instant::now() + connect_timeout;
    while !shutting_down.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OMP guest bridge did not connect within deadline",
            ));
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Darwin inherits O_NONBLOCK from the listener; bridge readers require blocking IO.
                stream.set_nonblocking(false)?;
                let Ok(mut read_stream) = stream.try_clone() else {
                    continue;
                };
                let candidate_deadline = Instant::now()
                    + candidate_timeout.min(deadline.saturating_duration_since(Instant::now()));
                let announced =
                    read_candidate_announcement(&mut read_stream, candidate_deadline, || {
                        if shutting_down.load(Ordering::Acquire) {
                            Err(io::Error::new(
                                io::ErrorKind::Interrupted,
                                "OMP guest bridge shut down before connecting",
                            ))
                        } else {
                            Ok(())
                        }
                    })?;
                if !announced
                    .as_deref()
                    .is_some_and(|line| authenticated_announcement(line, token))
                    || read_stream.set_read_timeout(None).is_err()
                {
                    continue;
                }
                return Ok((stream, BufReader::new(read_stream)));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL)
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "OMP guest bridge shut down before connecting",
    ))
}

fn authenticated_announcement(line: &str, token: &str) -> bool {
    serde_json::from_str::<GuestAnnouncement>(line)
        .ok()
        .is_some_and(|record| record.t == "guest" && record.token == token)
}

fn read_guest_records(
    mut reader: BufReader<TcpStream>,
    inbound: mpsc::SyncSender<PrivateOmpGuestRecord>,
    shutting_down: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut line = String::new();
    while !shutting_down.load(Ordering::Acquire) {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_PRIVATE_OMP_RECORD_BYTES + 1)
            .read_line(&mut line)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "private OMP guest bridge closed",
            ));
        }
        if read as u64 > MAX_PRIVATE_OMP_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private OMP guest record too large",
            ));
        }
        inbound.try_send(parse_guest_record(&line)?).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "private OMP guest queue unavailable",
            )
        })?;
    }
    Ok(())
}

fn parse_guest_record(line: &str) -> io::Result<PrivateOmpGuestRecord> {
    let record = serde_json::from_str::<GuestRecordWire>(line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid private OMP guest record: {error}"),
        )
    })?;
    match record.t.as_str() {
        "frame" => record
            .frame
            .map(|frame| PrivateOmpGuestRecord::Frame {
                frame,
                mutation: record.mutation,
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "private OMP frame missing payload",
                )
            }),
        "control" => match record.action.as_deref() {
            Some("request-controller") => Ok(PrivateOmpGuestRecord::Control(
                PrivateOmpGuestControl::RequestController,
            )),
            Some("release-controller") => Ok(PrivateOmpGuestRecord::Control(
                PrivateOmpGuestControl::ReleaseController,
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid private OMP control action",
            )),
        },
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown private OMP guest record type",
        )),
    }
}

fn write_guest_records(
    guest: Arc<Mutex<Option<TcpStream>>>,
    outbound: mpsc::Receiver<OutboundRecord>,
    shutting_down: Arc<AtomicBool>,
) {
    while !shutting_down.load(Ordering::Acquire) {
        let record = match outbound.recv_timeout(Duration::from_millis(100)) {
            Ok(record) => record,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if matches!(record, OutboundRecord::Shutdown) {
            return;
        }
        let Ok(guest) = guest.lock() else {
            return;
        };
        // Never hold this lock across network IO: shutdown must be able to close
        // the retained socket even when a peer stops reading.
        let Some(stream) = guest.as_ref() else {
            return;
        };
        let Ok(mut stream) = stream.try_clone() else {
            return;
        };
        drop(guest);
        let result = match record {
            OutboundRecord::Raw(record) => stream
                .write_all(record.as_bytes())
                .and_then(|_| stream.write_all(b"\n"))
                .and_then(|_| stream.flush()),
            OutboundRecord::Shutdown => unreachable!("handled before guest socket clone"),
        };
        if result.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guest_bridge_token_is_not_exposed_in_argv() {
        let argv = guest_argv(
            PathBuf::from("/tmp/omp"),
            "127.0.0.1:1234".into(),
            "room".into(),
        );
        assert!(argv.iter().any(|arg| arg == "--token-env"));
        assert!(!argv.iter().any(|arg| arg == "bridge-secret"));
    }

    #[test]
    fn guest_launch_strips_host_startup_and_identity_env() {
        let launch_env = omp_guest_launch_env(PaneLaunchEnv::default(), "bridge-secret".into());
        assert_eq!(
            launch_env,
            PaneLaunchEnv::default()
                .without_env("BUN_OPTIONS")
                .without_env("BUN_INSPECT_PRELOAD")
                .without_env("BUN_BE_BUN")
                .without_env("NODE_OPTIONS")
                .without_env("HERDR_OMP_BRIDGE")
                .without_env("HERDR_OMP_BRIDGE_TOKEN")
                .without_env(crate::integration::HERDR_PANE_ID_ENV_VAR)
                .with_extra(OMP_GUEST_BRIDGE_TOKEN_ENV, "bridge-secret")
        );
    }

    #[test]
    fn trickling_candidate_expires_before_authenticated_guest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let trickle = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for byte in b"{\"t\":" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let valid = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(180));
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(b"{\"t\":\"guest\",\"token\":\"expected\"}\n")
                .unwrap();
            stream
        });

        let shutting_down = AtomicBool::new(false);
        let (accepted, mut reader) = accept_authenticated_guest_with_timeouts(
            &listener,
            "expected",
            &shutting_down,
            Duration::from_secs(2),
            Duration::from_millis(80),
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
    fn authenticates_only_the_expected_guest_announcement() {
        assert!(authenticated_announcement(
            r#"{"t":"guest","token":"secret"}"#,
            "secret"
        ));
        assert!(!authenticated_announcement(
            r#"{"t":"guest","token":"other"}"#,
            "secret"
        ));
        assert!(!authenticated_announcement(
            r#"{"t":"host","token":"secret"}"#,
            "secret"
        ));
    }

    #[test]
    fn parses_control_and_preserves_frame_json() {
        assert!(matches!(
            parse_guest_record(r#"{"t":"control","action":"request-controller"}"#),
            Ok(PrivateOmpGuestRecord::Control(
                PrivateOmpGuestControl::RequestController
            ))
        ));
        let Ok(PrivateOmpGuestRecord::Frame { frame, mutation }) =
            parse_guest_record(r#"{"t":"frame","mutation":true,"frame":{"x": 1}}"#)
        else {
            panic!("expected frame")
        };
        assert!(mutation);
        assert_eq!(frame.get(), r#"{"x": 1}"#);
    }

    #[test]
    fn malformed_unknown_and_missing_frame_records_fail_closed() {
        for record in [
            "not-json",
            r#"{"t":"unknown"}"#,
            r#"{"t":"frame"}"#,
            r#"{"t":"control","action":"unknown"}"#,
        ] {
            let error = parse_guest_record(record).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn bridge_failure_ignores_deliberate_shutdown() {
        let guest = Mutex::new(None);
        let failed = AtomicBool::new(false);
        let shutting_down = AtomicBool::new(true);
        fail_bridge(&guest, &failed, &shutting_down);
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn bridge_failure_marks_an_active_bridge() {
        let guest = Mutex::new(None);
        let failed = AtomicBool::new(false);
        let shutting_down = AtomicBool::new(false);
        fail_bridge(&guest, &failed, &shutting_down);
        assert!(failed.load(Ordering::Acquire));
    }
}
