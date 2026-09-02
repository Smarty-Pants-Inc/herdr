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
const LEGACY_HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const PREVIOUS_HANDOFF_VERSION: u32 = 2;
#[cfg(unix)]
const HANDOFF_VERSION: u32 = 3;
/// Outer handoff fence. Older importers reject unknown versions before they restore the snapshot.
#[cfg(unix)]
const EXTERNAL_SNAPSHOT_HANDOFF_VERSION: u32 = 4;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
pub(crate) const MAX_FDS_PER_HANDOFF: usize = 64;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

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
    /// Host-wide OMP admission lease validated by the importing server.
    /// Absent from manifests written before the maintenance gate existed.
    #[serde(default)]
    pub omp_maintenance: Option<crate::server::omp_maintenance::OmpMaintenanceHandoffState>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
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
    for key in crate::integration::HERDR_OMP_BRIDGE_ENV_VARS {
        command.env_remove(key);
    }
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

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            info!(pid, status = %status, "handoff import server exited during rollback");
            return;
        }
        Ok(None) => {}
        Err(err) => {
            warn!(pid, err = %err, "failed to inspect handoff import server before rollback");
        }
    }

    if let Err(err) = child.kill() {
        warn!(pid, err = %err, "failed to kill handoff import server during rollback");
    }
    match child.wait() {
        Ok(status) => {
            info!(pid, status = %status, "handoff import server reaped during rollback");
        }
        Err(err) => {
            warn!(pid, err = %err, "failed to reap handoff import server during rollback");
        }
    }
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
) -> io::Result<UnixStream> {
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
    if validated.trim_end() != "validated" {
        return Err(io::Error::other("handoff import did not validate manifest"));
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(stream)
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS_PER_HANDOFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("handoff supports at most {MAX_FDS_PER_HANDOFF} pane file descriptors at once"),
        ));
    }
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

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let ready = read_line_unbuffered(&mut *stream)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other("handoff import did not report ready"));
    }
    Ok(())
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
    validate_manifest_compatibility(&manifest)?;
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
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
    })
}

#[cfg(unix)]
fn validate_manifest_compatibility(manifest: &HandoffManifest) -> io::Result<()> {
    let has_epoch = manifest.snapshot.idempotency_epoch.is_some();
    let has_valid_epoch = manifest
        .snapshot
        .idempotency_epoch
        .as_deref()
        .is_some_and(|epoch| !epoch.trim().is_empty());
    if manifest.snapshot.version > crate::persist::SNAPSHOT_VERSION {
        return Err(io::Error::other(format!(
            "handoff snapshot version {} is newer than supported {}",
            manifest.snapshot.version,
            crate::persist::SNAPSHOT_VERSION,
        )));
    }
    if manifest.snapshot.version >= crate::persist::EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION
        && manifest.version != EXTERNAL_SNAPSHOT_HANDOFF_VERSION
    {
        return Err(io::Error::other(
            "external resume snapshot requires handoff version 4",
        ));
    }
    match manifest.version {
        EXTERNAL_SNAPSHOT_HANDOFF_VERSION if manifest.snapshot.version >= crate::persist::EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION && has_valid_epoch => Ok(()),
        EXTERNAL_SNAPSHOT_HANDOFF_VERSION => Err(io::Error::other(format!(
            "handoff version {EXTERNAL_SNAPSHOT_HANDOFF_VERSION} requires an external snapshot and idempotency epoch"
        ))),
        HANDOFF_VERSION if has_valid_epoch => Ok(()),
        HANDOFF_VERSION => Err(io::Error::other(format!(
            "handoff version {HANDOFF_VERSION} requires an idempotency epoch"
        ))),
        PREVIOUS_HANDOFF_VERSION if !has_epoch => Ok(()),
        PREVIOUS_HANDOFF_VERSION => Err(io::Error::other(format!(
            "handoff version {PREVIOUS_HANDOFF_VERSION} cannot carry an idempotency epoch"
        ))),
        LEGACY_HANDOFF_VERSION if has_epoch => Err(io::Error::other(
            "legacy handoff manifests cannot carry an idempotency epoch",
        )),
        LEGACY_HANDOFF_VERSION if manifest.omp_maintenance.is_none() => Ok(()),
        LEGACY_HANDOFF_VERSION => Err(io::Error::other(
            "legacy handoff manifests cannot carry OMP maintenance state",
        )),
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
    omp_maintenance: Option<crate::server::omp_maintenance::OmpMaintenanceHandoffState>,
) -> HandoffManifest {
    HandoffManifest {
        version: if snapshot.version >= crate::persist::EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION {
            EXTERNAL_SNAPSHOT_HANDOFF_VERSION
        } else {
            HANDOFF_VERSION
        },
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
        api_window_title,
        omp_maintenance,
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
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    if expected == 0 {
        return Ok(Vec::new());
    }
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = expected * std::mem::size_of::<RawFd>();
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
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("handoff fd control message was truncated"));
    }

    let mut out = Vec::new();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
        }
        let data_len = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
        let count = data_len / std::mem::size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg) as *const RawFd;
        for idx in 0..count {
            out.push(*data.add(idx));
        }
    }
    if out.len() != expected {
        for fd in out {
            let _ = unsafe { libc::close(fd) };
        }
        return Err(io::Error::other(format!(
            "expected {expected} handoff fds, received fewer"
        )));
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
            idempotency_epoch: Some("test-epoch".into()),
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    fn external_snapshot() -> crate::persist::SessionSnapshot {
        let mut snapshot = empty_snapshot();
        snapshot.version = crate::persist::EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION;
        snapshot
    }

    fn validate_v5_handoff_fixture(encoded: &str) -> Result<(), String> {
        let version = serde_json::from_str::<serde_json::Value>(encoded)
            .map_err(|error| error.to_string())?
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "handoff version is missing".to_string())?;
        match version {
            1..=3 => Ok(()),
            version => Err(format!("unsupported handoff version {version}")),
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
            None,
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn current_handoff_schema_carries_the_idempotency_epoch() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
            None,
        );

        assert_eq!(manifest.version, 3);
        assert_eq!(
            manifest.snapshot.idempotency_epoch.as_deref(),
            Some("test-epoch")
        );
        validate_manifest_compatibility(&manifest).expect("current handoff must be compatible");
    }

    #[test]
    fn external_resume_snapshot_uses_an_outer_version_that_v5_importers_reject() {
        let manifest = manifest_for(external_snapshot(), Vec::new(), None, None, None, None);

        assert_eq!(manifest.version, EXTERNAL_SNAPSHOT_HANDOFF_VERSION);
        validate_manifest_compatibility(&manifest)
            .expect("v6 importer must accept its fenced handoff");
        let encoded = serde_json::to_string(&manifest).expect("handoff should serialize");
        assert_eq!(
            validate_v5_handoff_fixture(&encoded).unwrap_err(),
            "unsupported handoff version 4"
        );
    }

    #[test]
    fn legacy_outer_handoff_versions_cannot_smuggle_an_external_resume_snapshot() {
        for version in [
            LEGACY_HANDOFF_VERSION,
            PREVIOUS_HANDOFF_VERSION,
            HANDOFF_VERSION,
        ] {
            let mut manifest =
                manifest_for(external_snapshot(), Vec::new(), None, None, None, None);
            manifest.version = version;

            let error = validate_manifest_compatibility(&manifest).unwrap_err();
            assert_eq!(
                error.to_string(),
                "external resume snapshot requires handoff version 4"
            );
        }
    }

    #[test]
    fn handoff_rejects_a_future_nested_snapshot_version() {
        let mut manifest = manifest_for(external_snapshot(), Vec::new(), None, None, None, None);
        manifest.snapshot.version = crate::persist::SNAPSHOT_VERSION + 1;

        let error = validate_manifest_compatibility(&manifest).unwrap_err();
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn previous_handoff_version_cannot_claim_an_idempotency_epoch() {
        let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        manifest.version = PREVIOUS_HANDOFF_VERSION;

        let error = validate_manifest_compatibility(&manifest).unwrap_err();
        assert!(error
            .to_string()
            .contains("handoff version 2 cannot carry an idempotency epoch"));
    }

    #[test]
    fn genuine_previous_handoff_without_an_epoch_remains_compatible() {
        let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        manifest.version = PREVIOUS_HANDOFF_VERSION;
        manifest.snapshot.idempotency_epoch = None;

        validate_manifest_compatibility(&manifest).expect("genuine v2 handoff remains compatible");
    }

    #[test]
    fn current_handoff_without_idempotency_epoch_is_rejected() {
        let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        manifest.snapshot.idempotency_epoch = None;

        let error = validate_manifest_compatibility(&manifest).unwrap_err();
        assert!(error.to_string().contains("requires an idempotency epoch"));
    }

    #[test]
    fn a_handoff_carries_an_armed_omp_maintenance_permit() {
        let maintenance = crate::server::omp_maintenance::OmpMaintenanceHandoffState {
            owner_hash: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into(),
            permit: Some(crate::api::schema::ServerOmpMaintenancePermit {
                session: "proof".into(),
                pane_id: "w1:p1".into(),
            }),
        };
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            None,
            Some(maintenance.clone()),
        );

        assert_eq!(manifest.omp_maintenance, Some(maintenance));
    }

    #[test]
    fn legacy_handoff_version_cannot_claim_maintenance_capability() {
        let mut manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            None,
            Some(crate::server::omp_maintenance::OmpMaintenanceHandoffState {
                owner_hash: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".into(),
                permit: None,
            }),
        );
        manifest.version = LEGACY_HANDOFF_VERSION;
        manifest.snapshot.idempotency_epoch = None;

        let error = validate_manifest_compatibility(&manifest).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot carry OMP maintenance state"));
    }

    #[test]
    fn genuine_legacy_handoff_without_new_state_remains_compatible() {
        let mut manifest = manifest_for(empty_snapshot(), Vec::new(), None, None, None, None);
        manifest.version = LEGACY_HANDOFF_VERSION;
        manifest.snapshot.idempotency_epoch = None;

        validate_manifest_compatibility(&manifest).expect("genuine v1 handoff remains compatible");
    }
}
