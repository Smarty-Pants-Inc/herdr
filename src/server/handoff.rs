#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::{info, warn};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
// ponytail: the Smarty fork's 0.9.0 line (d86f0976, managed layout idempotency) sends
// version 3. Its manifest only adds optional snapshot fields that this build ignores, and
// the socket handshake is unchanged, so accept it to move running fork servers onto this
// build without ending pane processes. Remove once no fork 0.9.0 server remains.
#[cfg(unix)]
const FORK_MANAGED_LAYOUT_HANDOFF_VERSION: u32 = 3;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
// Descriptors are transferred in batches of this size. A single SCM_RIGHTS
// control message caps out at 253 descriptors on Linux and 254 on macOS, so the
// batch stays well below both limits and the number of panes stays unbounded.
#[cfg(unix)]
const FDS_PER_MESSAGE: usize = 64;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    /// An outer window title set over the API outlives the server that took the
    /// call, so a handoff carries it rather than falling back to the config.
    /// Absent from manifests written before this field existed.
    #[serde(default)]
    pub api_window_title: Option<String>,
    /// The source asks the replacement to report each public socket it binds
    /// (`bound <api|client> <dev> <ino>`), so a failed handoff removes only the
    /// replacement's own socket files. A replacement that supports this answers
    /// `validated bound-sockets`; older replacements ignore the field.
    #[serde(default)]
    pub report_bound_sockets: bool,
}

#[cfg(unix)]
const BOUND_SOCKETS_CAPABILITY: &str = "bound-sockets";

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
    /// Whether this replacement agreed to report the sockets it binds.
    pub report_bound_sockets: bool,
}

/// What the replacement said it supports when it validated the manifest.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReplacementCapabilities {
    pub reports_bound_sockets: bool,
}

/// A public socket path the replacement binds.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundSocketKind {
    Api,
    Client,
}

#[cfg(unix)]
impl BoundSocketKind {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Client => "client",
        }
    }
}

/// The socket files the replacement reported binding, recorded as it binds
/// them. Only these may be removed when the handoff fails.
#[cfg(unix)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BoundSockets {
    pub api: Option<crate::ipc::SocketFileIdentity>,
    pub client: Option<crate::ipc::SocketFileIdentity>,
}

#[cfg(unix)]
impl BoundSockets {
    pub(crate) fn get(&self, kind: BoundSocketKind) -> Option<&crate::ipc::SocketFileIdentity> {
        match kind {
            BoundSocketKind::Api => self.api.as_ref(),
            BoundSocketKind::Client => self.client.as_ref(),
        }
    }

    /// Records a `bound <api|client> <dev> <ino>` line. Returns false for any
    /// other line.
    fn record(&mut self, line: &str) -> bool {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("bound") {
            return false;
        }
        let (Some(kind), Some(dev), Some(ino), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
            return false;
        };
        let identity = crate::ipc::SocketFileIdentity::from_parts(dev, ino);
        match kind {
            "api" => self.api = Some(identity),
            "client" => self.client = Some(identity),
            _ => return false,
        }
        true
    }
}

#[cfg(unix)]
pub(crate) fn handoff_socket_path() -> PathBuf {
    crate::session::data_dir().join(format!("herdr-handoff-{}.sock", std::process::id()))
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine herdr executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if crate::session::explicit_session_requested() {
        // The import child no longer has the original `--session` argument, so
        // stale socket overrides must not mask the inherited HERDR_SESSION.
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
    }
    crate::platform::detach_server_daemon_command(&mut command);
    command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

/// Ends a debug import server when the test process that owns it exits.
///
/// The import server is a detached daemon, so it outlives a killed or aborted
/// test process and no test-side cleanup remains to reap it. Tests pass their
/// PID in `HERDR_TEST_HANDOFF_OWNER_PID`; release builds ignore it.
#[cfg(all(unix, debug_assertions))]
pub(crate) fn start_test_owner_watchdog() {
    let Some(owner_pid) = std::env::var("HERDR_TEST_HANDOFF_OWNER_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0 && *pid != std::process::id())
    else {
        return;
    };
    let watchdog = std::thread::Builder::new()
        .name("herdr-test-owner-watchdog".to_string())
        .spawn(move || loop {
            if !crate::platform::process_exists(owner_pid) {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(100));
        });
    if let Err(err) = watchdog {
        warn!(%err, "failed to start test owner watchdog");
    }
}

#[cfg(unix)]
const IMPORT_GROUP_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Stops a replacement server whose handoff failed, for certain.
///
/// The import child runs in its own session, so its pid is also its process
/// group. The whole group is killed, not only the direct child: an import
/// executable that is a wrapper, or anything the child started in its group,
/// must not survive holding the public sockets. Pane processes run in sessions
/// of their own and are not in this group.
///
/// The group id is only ever signalled while the leader is unreaped (running or
/// a zombie): the kernel cannot reuse its pid, so it cannot name another group.
/// The leader is reaped last, and nothing is signalled after that.
#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    let mut ops = RealImportGroup { child };
    stop_import_group(&mut ops, IMPORT_GROUP_EXIT_TIMEOUT);
    info!(pid, "handoff import process group stopped during rollback");
}

/// The operations `stop_import_group` needs, so tests can check their order.
#[cfg(unix)]
trait ImportGroupOps {
    /// The leader has not been reaped yet, so its pid still pins the group id.
    fn leader_unreaped(&mut self) -> bool;
    /// SIGKILL the group (and the leader, in case it never became a leader).
    fn kill_group(&mut self);
    /// The leader has exited; it stays unreaped.
    fn leader_exited(&mut self) -> bool;
    /// A process other than the leader is still alive in the group.
    fn group_has_live_members(&mut self) -> bool;
    /// Reaps the leader. The group id must not be signalled after this.
    fn reap(&mut self);
}

#[cfg(unix)]
fn stop_import_group(ops: &mut impl ImportGroupOps, timeout: Duration) {
    if !ops.leader_unreaped() {
        // Someone already reaped it: its pid, and so its group id, may belong
        // to another process now. Never signal it.
        warn!("handoff import server was already reaped; not signalling its process group");
        ops.reap();
        return;
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        ops.kill_group();
        if ops.leader_exited() && !ops.group_has_live_members() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            tracing::error!(
                "handoff import process group is still alive after SIGKILL; continuing rollback"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ops.reap();
}

#[cfg(unix)]
struct RealImportGroup<'a> {
    child: &'a mut Child,
}

#[cfg(unix)]
impl RealImportGroup<'_> {
    fn pid(&self) -> libc::pid_t {
        self.child.id() as libc::pid_t
    }

    /// `waitid(WNOWAIT)`: Some(exited) while the leader is ours and unreaped,
    /// None once it has been reaped (ECHILD).
    fn peek_leader(&self) -> Option<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return None;
        }
        Some(siginfo_pid(&info) != 0)
    }
}

#[cfg(unix)]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    unsafe { info.si_pid() }
}

#[cfg(unix)]
impl ImportGroupOps for RealImportGroup<'_> {
    fn leader_unreaped(&mut self) -> bool {
        self.peek_leader().is_some()
    }

    fn kill_group(&mut self) {
        let pid = self.pid();
        if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                warn!(pid, err = %err, "failed to kill handoff import process group");
            }
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    fn leader_exited(&mut self) -> bool {
        // Reaped by someone else counts as exited; the loop then stops signalling.
        self.peek_leader().unwrap_or(true)
    }

    fn group_has_live_members(&mut self) -> bool {
        process_group_has_live_members(self.pid(), self.pid())
    }

    fn reap(&mut self) {
        match self.child.wait() {
            Ok(status) => {
                info!(pid = self.child.id(), status = %status, "handoff import server reaped during rollback");
            }
            Err(err) => {
                warn!(pid = self.child.id(), err = %err, "failed to reap handoff import server during rollback");
            }
        }
    }
}

/// Whether any process other than `leader` is alive (not a zombie) in `pgid`.
/// Only reads process tables; it never signals.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_group_has_live_members(pgid: libc::pid_t, leader: libc::pid_t) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if pid == leader {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // pid (comm) state ppid pgrp ...; comm may contain spaces or parens.
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().unwrap_or("Z");
        let pgrp = fields
            .nth(1)
            .and_then(|field| field.parse::<libc::pid_t>().ok());
        if pgrp == Some(pgid) && state != "Z" && state != "X" {
            return true;
        }
    }
    false
}

/// `PROC_PGRP_ONLY` from `<libproc.h>`; not exported by the libc crate.
#[cfg(target_os = "macos")]
const PROC_PGRP_ONLY: u32 = 2;

#[cfg(target_os = "macos")]
fn process_group_has_live_members(pgid: libc::pid_t, leader: libc::pid_t) -> bool {
    let mut pids = vec![0 as libc::pid_t; 1024];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            pgid as u32,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return false;
    }
    let count = bytes as usize / std::mem::size_of::<libc::pid_t>();
    pids.iter().take(count).any(|&pid| {
        if pid == 0 || pid == leader {
            return false;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        got == size && info.pbi_status != libc::SZOMB
    })
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn process_group_has_live_members(_pgid: libc::pid_t, _leader: libc::pid_t) -> bool {
    false
}

/// Refuses a handoff whose caller expected a different source server.
///
/// A rollback must hand off from the original server, never from whatever
/// server currently answers the socket (for example an orphaned import child).
#[cfg(unix)]
pub(crate) fn check_expected_source(
    expected_pid: Option<u32>,
    expected_socket_inode: Option<u64>,
    own_pid: u32,
    own_socket_inode: Option<u64>,
    public_socket_inode: Option<u64>,
) -> io::Result<()> {
    if let Some(expected) = expected_pid {
        if expected != own_pid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("handoff source is pid {own_pid}, not the expected pid {expected}"),
            ));
        }
    }
    if let Some(expected) = expected_socket_inode {
        if own_socket_inode != Some(expected) || public_socket_inode != Some(expected) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "handoff source does not own the expected api socket inode {expected} (owns {own_socket_inode:?}, public path has {public_socket_inode:?})"
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(socket_path)?;
    Ok(listener)
}

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    socket_path: &Path,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<(UnixStream, ReplacementCapabilities)> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let validated = read_line_unbuffered(&mut stream)?;
    let capabilities = parse_validated_line(&validated)
        .ok_or_else(|| io::Error::other("handoff import did not validate manifest"))?;
    let _ = std::fs::remove_file(socket_path);
    Ok((stream, capabilities))
}

/// Parses `validated` or `validated <capability>...` from the replacement.
#[cfg(unix)]
fn parse_validated_line(line: &str) -> Option<ReplacementCapabilities> {
    let mut words = line.split_whitespace();
    if words.next() != Some("validated") {
        return None;
    }
    let mut capabilities = ReplacementCapabilities::default();
    for word in words {
        if word == BOUND_SOCKETS_CAPABILITY {
            capabilities.reports_bound_sockets = true;
        }
    }
    Some(capabilities)
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    send_fds(stream, fds)?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let restored = read_line_unbuffered(&mut *stream)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

/// Waits for `ready`, recording each `bound ...` report on the way.
#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream, bound: &mut BoundSockets) -> io::Result<()> {
    let deadline = std::time::Instant::now() + ready_timeout();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handoff import did not report ready in time",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        let line = match read_line_unbuffered(&mut *stream) {
            Ok(line) => line,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "handoff import did not report ready within {}ms",
                        ready_timeout().as_millis()
                    ),
                ));
            }
            Err(err) => return Err(err),
        };
        let line = line.trim_end();
        if line == "ready" {
            return Ok(());
        }
        if !bound.record(line) {
            return Err(io::Error::other("handoff import did not report ready"));
        }
    }
}

/// Reads any `bound ...` reports a stopped replacement wrote before it died,
/// up to end of stream. Call only after the replacement has been stopped.
#[cfg(unix)]
pub(crate) fn drain_bound_reports(stream: &mut UnixStream, bound: &mut BoundSockets) {
    if stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return;
    }
    while let Ok(line) = read_line_unbuffered(&mut *stream) {
        if line.is_empty() {
            return;
        }
        bound.record(line.trim_end());
    }
}

/// Tells the source which public socket file this replacement just bound.
#[cfg(unix)]
pub(crate) fn report_bound(
    stream: &mut UnixStream,
    kind: BoundSocketKind,
    identity: &crate::ipc::SocketFileIdentity,
) -> io::Result<()> {
    writeln!(
        stream,
        "bound {} {} {}",
        kind.wire_name(),
        identity.dev(),
        identity.inode()
    )?;
    stream.flush()
}

/// Debug builds let tests shorten the readiness wait for a replacement that
/// never becomes ready.
#[cfg(unix)]
fn ready_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("HERDR_TEST_HANDOFF_READY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    READY_TIMEOUT
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    if let Err(err) = stream.set_read_timeout(Some(OWNED_ACK_TIMEOUT)) {
        warn!(err = %err, "failed to set handoff ownership ack timeout");
        return;
    }
    match read_line_unbuffered(&mut *stream) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            warn!(
                response = %other.trim_end(),
                "handoff import sent unexpected ownership ack after commit"
            );
        }
        Err(err) => {
            warn!(err = %err, "handoff import ownership ack was not received after commit");
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let manifest_line = read_line_unbuffered(&mut stream)?;
    let manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    validate_manifest_version(&manifest)?;
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected herdr v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    let report_bound_sockets = manifest.report_bound_sockets && !test_legacy_replacement();
    if report_bound_sockets {
        writeln!(stream, "validated {BOUND_SOCKETS_CAPABILITY}")?;
    } else {
        stream.write_all(b"validated\n")?;
    }
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
        report_bound_sockets,
    })
}

/// Debug builds let tests run a replacement that behaves like a build from
/// before socket reports: it answers plain `validated` and reports nothing.
#[cfg(unix)]
fn test_legacy_replacement() -> bool {
    cfg!(debug_assertions)
        && std::env::var("HERDR_TEST_HANDOFF_IMPORT_LEGACY").as_deref() == Ok("1")
}

#[cfg(unix)]
fn validate_manifest_version(manifest: &HandoffManifest) -> io::Result<()> {
    match manifest.version {
        HANDOFF_VERSION | FORK_MANAGED_LAYOUT_HANDOFF_VERSION => Ok(()),
        version => Err(io::Error::other(format!(
            "unsupported handoff version {version}"
        ))),
    }
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let committed = read_line_unbuffered(&mut *stream)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    expected_protocol: Option<u32>,
    expected_version: Option<String>,
    api_window_title: Option<String>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
        api_window_title,
        report_bound_sockets: true,
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed while reading line",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    for batch in fds.chunks(FDS_PER_MESSAGE) {
        send_fd_batch(stream, batch)?;
    }
    Ok(())
}

#[cfg(unix)]
fn send_fd_batch(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn close_raw_fds(fds: &[RawFd]) {
    for fd in fds {
        let _ = unsafe { libc::close(*fd) };
    }
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    let mut out: Vec<RawFd> = Vec::with_capacity(expected);
    while out.len() < expected {
        let wanted = (expected - out.len()).min(FDS_PER_MESSAGE);
        let batch = match recv_fd_batch(stream, wanted) {
            Ok(batch) => batch,
            Err(err) => {
                close_raw_fds(&out);
                return Err(err);
            }
        };
        if batch.is_empty() {
            let received = out.len();
            close_raw_fds(&out);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "handoff stream closed after {received} of {expected} pane file descriptors"
                ),
            ));
        }
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(unix)]
fn recv_fd_batch(stream: &UnixStream, wanted: usize) -> io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = wanted * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut out = Vec::new();
    unsafe {
        let control_end = control.as_ptr() as usize + msg.msg_controllen as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                // Bound the payload by both the header's own length and the
                // bytes the kernel wrote into `control`, so the read below can
                // never run past the buffer.
                let available = control_end.saturating_sub(data as usize);
                let data_len = ((*cmsg).cmsg_len as usize)
                    .saturating_sub(libc::CMSG_LEN(0) as usize)
                    .min(available);
                let count = data_len / std::mem::size_of::<RawFd>();
                let data = data as *const RawFd;
                for idx in 0..count {
                    out.push(*data.add(idx));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    // Truncation means the kernel closed the descriptors that did not fit, so
    // the batch is unrecoverable rather than merely short.
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        close_raw_fds(&out);
        return Err(io::Error::other("handoff fd control message was truncated"));
    }
    if read == 0 {
        close_raw_fds(&out);
        return Ok(Vec::new());
    }
    if out.len() > wanted {
        let received = out.len();
        close_raw_fds(&out);
        return Err(io::Error::other(format!(
            "handoff fd message carried {received} descriptors, expected at most {wanted}"
        )));
    }
    if out.is_empty() {
        return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
    }
    Ok(out)
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn empty_snapshot() -> crate::persist::SessionSnapshot {
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: Vec::new(),
            active: None,
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    #[test]
    fn a_handoff_carries_an_api_set_window_title() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("api_window_title");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.api_window_title.is_none());
    }

    #[test]
    fn a_fork_managed_layout_manifest_is_accepted() {
        // Shape sent by the fork 0.9.0 line: version 3 plus layout idempotency fields.
        let manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value["version"] = serde_json::json!(3);
        value["snapshot"]["idempotency_epoch"] = serde_json::json!("ab".repeat(16));
        let fork: HandoffManifest =
            serde_json::from_value(value).expect("a fork manifest should load");

        assert!(validate_manifest_version(&fork).is_ok());
    }

    #[test]
    fn a_handoff_from_an_unexpected_source_is_refused() {
        assert!(check_expected_source(None, None, 10, None, None).is_ok());
        assert!(check_expected_source(Some(10), Some(7), 10, Some(7), Some(7)).is_ok());
        assert!(check_expected_source(Some(11), None, 10, Some(7), Some(7)).is_err());
        // Another server's socket at the public path, or a socket we no longer own.
        assert!(check_expected_source(None, Some(7), 10, Some(7), Some(8)).is_err());
        assert!(check_expected_source(None, Some(7), 10, Some(8), Some(7)).is_err());
        assert!(check_expected_source(None, Some(7), 10, None, Some(7)).is_err());
    }

    #[test]
    fn a_failed_import_child_is_stopped_with_its_process_group() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 600 & echo $!; wait")
            .stdout(std::process::Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn stub import child");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let helper: libc::pid_t = line.trim().parse().expect("helper pid");
        let pgid = child.id() as libc::pid_t;

        cleanup_failed_import_child(&mut child);

        // The leader is reaped, and no live process is left in its group. The
        // group id is only inspected, never signalled, after the reap.
        assert!(child.try_wait().unwrap().is_some());
        assert!(
            !process_group_has_live_members(pgid, pgid),
            "process group survived"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(helper, 0) } == 0 && std::time::Instant::now() < deadline {
            // Reparented to init as a zombie until it is reaped there.
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_group_has_live_members(pgid, pgid),
            "helper survived"
        );
    }

    #[derive(Default)]
    struct FakeGroup {
        reaped_already: bool,
        exits_after_kills: usize,
        members_after_kills: usize,
        kills: usize,
        events: Vec<&'static str>,
    }

    impl ImportGroupOps for FakeGroup {
        fn leader_unreaped(&mut self) -> bool {
            !self.reaped_already && !self.events.contains(&"reap")
        }
        fn kill_group(&mut self) {
            self.kills += 1;
            self.events.push("kill");
        }
        fn leader_exited(&mut self) -> bool {
            self.kills >= self.exits_after_kills
        }
        fn group_has_live_members(&mut self) -> bool {
            self.kills < self.members_after_kills
        }
        fn reap(&mut self) {
            self.events.push("reap");
        }
    }

    #[test]
    fn a_failed_import_group_is_never_signalled_after_its_leader_is_reaped() {
        // A helper outlives the leader for a few rounds: every kill still comes
        // before the single reap.
        let mut group = FakeGroup {
            exits_after_kills: 1,
            members_after_kills: 4,
            ..FakeGroup::default()
        };
        stop_import_group(&mut group, Duration::from_secs(5));
        assert_eq!(group.events, ["kill", "kill", "kill", "kill", "reap"]);

        // Even when the group outlives the deadline, it is reaped last.
        let mut stubborn = FakeGroup {
            exits_after_kills: usize::MAX,
            members_after_kills: usize::MAX,
            ..FakeGroup::default()
        };
        stop_import_group(&mut stubborn, Duration::from_millis(50));
        assert_eq!(stubborn.events.last(), Some(&"reap"));
        assert_eq!(stubborn.events.iter().filter(|e| **e == "reap").count(), 1);
        assert!(stubborn.kills >= 1);

        // A leader someone else already reaped no longer pins its group id:
        // nothing is signalled at all.
        let mut reaped = FakeGroup {
            reaped_already: true,
            ..FakeGroup::default()
        };
        stop_import_group(&mut reaped, Duration::from_secs(5));
        assert_eq!(reaped.events, ["reap"]);
    }

    #[test]
    fn a_replacement_advertises_socket_reports_when_it_validates() {
        assert_eq!(
            parse_validated_line("validated\n"),
            Some(ReplacementCapabilities::default())
        );
        assert_eq!(
            parse_validated_line("validated bound-sockets\n"),
            Some(ReplacementCapabilities {
                reports_bound_sockets: true
            })
        );
        assert_eq!(parse_validated_line("ready\n"), None);
    }

    #[test]
    fn bound_socket_reports_are_recorded_by_kind() {
        let mut bound = BoundSockets::default();
        assert!(bound.record("bound api 7 42"));
        assert!(bound.record("bound client 7 43"));
        assert!(!bound.record("bound other 7 44"));
        assert!(!bound.record("bound api 7"));
        assert!(!bound.record("ready"));
        assert_eq!(
            bound.get(BoundSocketKind::Api),
            Some(&crate::ipc::SocketFileIdentity::from_parts(7, 42))
        );
        assert_eq!(
            bound.get(BoundSocketKind::Client),
            Some(&crate::ipc::SocketFileIdentity::from_parts(7, 43))
        );
    }

    #[test]
    fn unknown_handoff_versions_are_rejected() {
        let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None);
        for version in [0, 2, 4] {
            manifest.version = version;
            assert!(validate_manifest_version(&manifest).is_err());
        }
    }
}
