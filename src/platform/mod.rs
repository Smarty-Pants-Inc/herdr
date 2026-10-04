//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

mod diagnostic_owner;
pub(crate) use diagnostic_owner::{
    diagnostic_directory, diagnostic_owner_identity, diagnostic_snapshot_name,
    diagnostic_temporary_name, parse_diagnostic_name, DiagnosticName,
    DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION,
};

#[cfg(unix)]
pub(crate) mod ssh_agent;

pub(crate) struct HostShutdownMonitor {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HostShutdownMonitor {
    pub(crate) fn start(
        requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let task = monitor_host_shutdown(requested, wake);
        Self { task }
    }
}

impl Drop for HostShutdownMonitor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn monitor_host_shutdown(
    _requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _wake: impl Fn() + Send + Sync + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

/// Stable identity for a process instance. PIDs can be reused after a process exits;
/// the start time makes ancestry transitions reject a reused PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

/// Why a pane runtime ended, before application persistence policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    Exited,
    Interrupted,
    /// Imported runtimes have no child wait handle in the replacement server.
    #[cfg(unix)]
    Handoff,
    WaitFailed,
}

impl ChildExitReason {
    pub(crate) fn requires_session_checkpoint(self) -> bool {
        match self {
            Self::Interrupted => true,
            #[cfg(unix)]
            Self::Handoff => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
pub(crate) use unix_common::{
    classify_child_exit, poll_fd_readable, process_identity_for_pty, read_fd,
    shared_ssh_control_path,
};

#[cfg(all(unix, test))]
pub(crate) use unix_common::process_in_pane_session;

/// Capture process metadata across two checks of the transport's liveness
/// primitive (native pidfd/audit token, or connected socket for the older Linux
/// credential fallback), never a newly opened numeric PID handle.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn capture_bound_peer_identity(
    original_alive: impl Fn() -> bool,
    identity_now: impl FnOnce() -> Option<ProcessIdentity>,
) -> Option<ProcessIdentity> {
    if !original_alive() {
        return None;
    }
    let identity = identity_now()?;
    original_alive().then_some(identity)
}

/// Whether checked pane membership validates ancestry itself, including the full
/// chain to a process-tree root for a negative result. POSIX session membership
/// does not cover detached descendants and needs a separate ancestor walk.
pub(crate) fn checked_membership_covers_ancestry() -> bool {
    #[cfg(test)]
    if let Some(covers) = TEST_MEMBERSHIP_ANCESTRY.with(|value| value.get()) {
        return covers;
    }
    cfg!(windows)
}

/// Accept-time pane provenance is a restriction, not an identity or authorization
/// token. A marker never binds a public pane ID to its current terminal owner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PeerPaneOrigin {
    Absent,
    HasPane,
    #[default]
    Unknown,
}

pub(crate) fn process_initial_pane_origin(peer: ProcessIdentity) -> PeerPaneOrigin {
    if process_identity(peer.pid) != Some(peer) {
        return PeerPaneOrigin::Unknown;
    }
    let environment = process_initial_environment(peer);
    if process_identity(peer.pid) != Some(peer) {
        return PeerPaneOrigin::Unknown;
    }
    environment
        .as_deref()
        .map_or(PeerPaneOrigin::Unknown, pane_origin_from_environment)
}

fn process_initial_environment(peer: ProcessIdentity) -> Option<Vec<(String, String)>> {
    #[cfg(target_os = "linux")]
    return linux::process_initial_environment(peer);
    #[cfg(target_os = "macos")]
    return macos::process_initial_environment(peer);
    #[cfg(windows)]
    return windows::process_initial_environment(peer);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = peer;
        None
    }
}

/// Missing both markers is readable absence. Partial, duplicated, or malformed
/// markers cannot be used as proof of ordinary origin. Platform readers must
/// return None for an unreadable or incomplete environment, not an empty list.
fn pane_origin_from_environment(environment: &[(String, String)]) -> PeerPaneOrigin {
    let mut herdr_env = None;
    let mut pane_id = None;
    for (key, value) in environment {
        let slot = match key.as_str() {
            "HERDR_ENV" => &mut herdr_env,
            "HERDR_PANE_ID" => &mut pane_id,
            _ => continue,
        };
        if slot.replace(value.as_str()).is_some() {
            return PeerPaneOrigin::Unknown;
        }
    }
    match (herdr_env, pane_id) {
        (None, None) => PeerPaneOrigin::Absent,
        (Some("1"), Some(pane)) if valid_pane_marker(pane.as_bytes()) => PeerPaneOrigin::HasPane,
        _ => PeerPaneOrigin::Unknown,
    }
}

fn valid_pane_marker(pane: &[u8]) -> bool {
    let Ok(pane) = std::str::from_utf8(pane) else {
        return false;
    };
    let Some((workspace, pane)) = pane.split_once(":p") else {
        return false;
    };
    let Some(workspace) = workspace.strip_prefix('w') else {
        return false;
    };
    !workspace.is_empty()
        && !pane.is_empty()
        && workspace.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && pane.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Windows can prove a process is outside this server's descendants before pane
/// roots are inspected. Other platforms keep their existing attribution path.
// Individual variants are constructed only by their applicable platform (or tests).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerAncestry {
    NotApplicable,
    ReachedServer,
    Outside,
    Unknown,
}

pub(crate) fn process_identity_server_ancestry(peer: ProcessIdentity) -> ServerAncestry {
    #[cfg(test)]
    if let Some((expected, observation)) = TEST_SERVER_ANCESTRY.with(|value| value.get()) {
        if peer == expected {
            return if process_identity(peer.pid) == Some(peer) {
                observation
            } else {
                ServerAncestry::Unknown
            };
        }
    }
    #[cfg(windows)]
    return server_ancestry_observation(windows::process_identity_outside_server_ancestry(peer));
    #[cfg(not(windows))]
    {
        let _ = peer;
        ServerAncestry::NotApplicable
    }
}

#[cfg(any(windows, test))]
fn server_ancestry_observation(observation: Option<bool>) -> ServerAncestry {
    match observation {
        Some(true) => ServerAncestry::Outside,
        Some(false) => ServerAncestry::ReachedServer,
        None => ServerAncestry::Unknown,
    }
}

/// Identity-aware boundary around the numeric OS session observation.
pub(crate) fn process_identity_in_pane_session(
    root: ProcessIdentity,
    peer: ProcessIdentity,
) -> Option<bool> {
    #[cfg(test)]
    if let Some(observation) = TEST_MEMBERSHIP.with(|value| value.get()) {
        return observe_pane_session(root, peer, process_identity, |_, _| observation);
    }
    #[cfg(unix)]
    return observe_pane_session(
        root,
        peer,
        process_identity,
        unix_common::process_in_pane_session_checked,
    );
    #[cfg(windows)]
    return observe_pane_session(root, peer, process_identity, |_, _| {
        windows::process_identity_in_pane_session_checked(root, peer)
    });
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, peer);
        None
    }
}

#[cfg(test)]
thread_local! {
    static TEST_MEMBERSHIP: std::cell::Cell<Option<Option<bool>>> = const { std::cell::Cell::new(None) };
    static TEST_MEMBERSHIP_ANCESTRY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static TEST_SERVER_ANCESTRY: std::cell::Cell<Option<(ProcessIdentity, ServerAncestry)>> = const { std::cell::Cell::new(None) };
}

/// Peer-keyed, thread-local OS-observation seam; never changes other callers or
/// bypasses live endpoint checks. Drop restores the prior observation on panic.
#[cfg(test)]
pub(crate) fn with_server_ancestry_for_test<T>(
    peer: ProcessIdentity,
    observation: Option<bool>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<(ProcessIdentity, ServerAncestry)>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_SERVER_ANCESTRY.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(
        TEST_SERVER_ANCESTRY
            .with(|value| value.replace(Some((peer, server_ancestry_observation(observation))))),
    );
    run()
}

#[cfg(all(windows, test))]
pub(crate) use windows::test_outside_server_ancestry_sequence;

/// Scoped OS-observation seam: still runs endpoint checks and the actual app guard.
#[cfg(test)]
pub(crate) fn with_checked_membership_for_test<T>(
    observation: Option<bool>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<Option<bool>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_MEMBERSHIP.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(TEST_MEMBERSHIP.with(|value| value.replace(Some(observation))));
    run()
}

/// Exercise the Windows complete-ancestry policy on any host, without bypassing
/// endpoint validation or the real app guard. Observations remain explicitly scoped.
#[cfg(test)]
pub(crate) fn with_ancestry_membership_for_test<T>(
    observation: Option<bool>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_MEMBERSHIP_ANCESTRY.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(TEST_MEMBERSHIP_ANCESTRY.with(|value| value.replace(Some(true))));
    with_checked_membership_for_test(observation, run)
}

#[cfg(any(unix, windows, test))]
fn observe_pane_session(
    root: ProcessIdentity,
    peer: ProcessIdentity,
    identity_of: impl Fn(u32) -> Option<ProcessIdentity>,
    membership: impl FnOnce(u32, u32) -> Option<bool>,
) -> Option<bool> {
    if identity_of(peer.pid) != Some(peer) || identity_of(root.pid) != Some(root) {
        return None;
    }
    let belongs = membership(root.pid, peer.pid)?;
    // The OS observation reopens numeric PIDs. Never accept its answer for a
    // replacement instance, including a successful hit that ends the ancestor walk.
    (identity_of(peer.pid) == Some(peer) && identity_of(root.pid) == Some(root)).then_some(belongs)
}

/// Validate a parent link read together with the child's instance identity.
/// Shared by the Unix implementations; an exited/reused endpoint ends the walk.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn checked_parent_process_identity(
    child: ProcessIdentity,
    observed_child: ProcessIdentity,
    parent_pid: u32,
) -> Option<ProcessIdentity> {
    observe_parent_identity(child, observed_child, parent_pid, process_identity)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn observe_parent_identity(
    child: ProcessIdentity,
    observed_child: ProcessIdentity,
    parent_pid: u32,
    identity_of: impl Fn(u32) -> Option<ProcessIdentity>,
) -> Option<ProcessIdentity> {
    if child != observed_child {
        return None;
    }
    let parent = identity_of(parent_pid)?;
    (parent.start_time <= child.start_time
        && identity_of(child.pid) == Some(child)
        && identity_of(parent.pid) == Some(parent))
    .then_some(parent)
}

#[cfg(test)]
mod pane_origin_tests {
    use super::*;

    #[test]
    fn marker_presence_distinguishes_readable_absence_from_malformed_markers() {
        let pairs = |values: &[(&str, &str)]| {
            values
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        };
        for values in [vec![], vec![("PATH", "/bin")]] {
            assert_eq!(
                pane_origin_from_environment(&pairs(&values)),
                PeerPaneOrigin::Absent
            );
        }
        assert_eq!(
            pane_origin_from_environment(&pairs(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w9V:p1")
            ])),
            PeerPaneOrigin::HasPane
        );
        for values in [
            vec![("HERDR_ENV", "1")],
            vec![("HERDR_PANE_ID", "w9V:p1")],
            vec![("HERDR_ENV", "0"), ("HERDR_PANE_ID", "w9V:p1")],
            vec![("HERDR_ENV", "1"), ("HERDR_PANE_ID", "")],
            vec![("HERDR_ENV", "1"), ("HERDR_PANE_ID", "not-a-pane")],
            vec![
                ("HERDR_ENV", "1"),
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w9V:p1"),
            ],
        ] {
            assert_eq!(
                pane_origin_from_environment(&pairs(&values)),
                PeerPaneOrigin::Unknown
            );
        }
    }

    #[test]
    fn invalid_process_marker_capture_is_unknown_not_readable_absence() {
        assert_eq!(
            process_initial_pane_origin(ProcessIdentity {
                pid: 0,
                start_time: 0
            }),
            PeerPaneOrigin::Unknown
        );
    }

    #[test]
    fn scoped_server_observation_is_peer_keyed_and_restores_after_panic() {
        let peer = process_identity(std::process::id()).expect("test process");
        let other = ProcessIdentity {
            start_time: peer.start_time.saturating_add(1),
            ..peer
        };
        let before = process_identity_server_ancestry(peer);
        let panic = std::panic::catch_unwind(|| {
            with_server_ancestry_for_test(peer, Some(true), || {
                assert_eq!(
                    process_identity_server_ancestry(peer),
                    ServerAncestry::Outside
                );
                assert_ne!(
                    process_identity_server_ancestry(other),
                    ServerAncestry::Outside
                );
                panic!("exercise restoration");
            });
        });
        assert!(panic.is_err());
        assert_eq!(process_identity_server_ancestry(peer), before);
    }
}

#[cfg(test)]
mod membership_identity_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn original_generation_liveness_rejects_numeric_peer_replacement() {
        let alive = Cell::new(true);
        let replacement = ProcessIdentity {
            pid: 100,
            start_time: 30,
        };
        assert_eq!(
            capture_bound_peer_identity(
                || alive.get(),
                || {
                    alive.set(false);
                    Some(replacement)
                }
            ),
            None
        );
        assert_eq!(
            capture_bound_peer_identity(
                || false,
                || panic!("dead original must not inspect a numeric replacement")
            ),
            None
        );
        assert_eq!(
            capture_bound_peer_identity(|| true, || Some(replacement)),
            Some(replacement)
        );
    }

    #[test]
    fn session_observation_rejects_root_swap_and_exited_endpoints() {
        let root = ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        let peer = ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        for changed in [root, peer] {
            for replacement in [
                None,
                Some(ProcessIdentity {
                    start_time: 30,
                    ..changed
                }),
            ] {
                let current = Cell::new(Some(changed));
                assert_eq!(
                    observe_pane_session(
                        root,
                        peer,
                        |pid| if pid == changed.pid {
                            current.get()
                        } else if pid == root.pid {
                            Some(root)
                        } else {
                            Some(peer)
                        },
                        |_, _| {
                            current.set(replacement);
                            Some(true)
                        },
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn failed_session_observation_is_not_a_negative_match() {
        let peer = ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let root = ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        assert_eq!(
            observe_pane_session(
                root,
                peer,
                |pid| Some(if pid == root.pid { root } else { peer }),
                |_, _| None
            ),
            None
        );
    }

    #[test]
    fn session_observation_accepts_only_stable_pinned_instances() {
        let root = ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        let peer = ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        for belongs in [false, true] {
            assert_eq!(
                observe_pane_session(
                    root,
                    peer,
                    |pid| if pid == root.pid {
                        Some(root)
                    } else {
                        Some(peer)
                    },
                    |_, _| Some(belongs)
                ),
                Some(belongs)
            );
        }
        assert_eq!(
            observe_pane_session(
                root,
                peer,
                |pid| if pid == root.pid {
                    Some(root)
                } else {
                    Some(ProcessIdentity {
                        start_time: 30,
                        ..peer
                    })
                },
                |_, _| panic!("reused peer must not reach OS observation")
            ),
            None
        );
    }

    #[test]
    fn parent_link_rejects_reused_exited_and_younger_instances() {
        let child = ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let parent = ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        for replacement in [
            None,
            Some(ProcessIdentity {
                start_time: 30,
                ..parent
            }),
        ] {
            let reads = Cell::new(0);
            assert_eq!(
                observe_parent_identity(child, child, parent.pid, |pid| {
                    if pid == child.pid {
                        return Some(child);
                    }
                    let count = reads.get();
                    reads.set(count + 1);
                    if count == 0 {
                        Some(parent)
                    } else {
                        replacement
                    }
                }),
                None
            );
        }
        assert_eq!(
            observe_parent_identity(child, child, parent.pid, |pid| if pid == child.pid {
                None
            } else {
                Some(parent)
            }),
            None
        );
        assert_eq!(
            observe_parent_identity(child, child, parent.pid, |pid| if pid == child.pid {
                Some(child)
            } else {
                Some(ProcessIdentity {
                    start_time: 30,
                    ..parent
                })
            }),
            None
        );
        assert_eq!(
            observe_parent_identity(child, child, parent.pid, |pid| if pid == child.pid {
                Some(child)
            } else {
                Some(parent)
            }),
            Some(parent)
        );
        assert_eq!(
            observe_parent_identity(
                child,
                ProcessIdentity {
                    start_time: 30,
                    ..child
                },
                parent.pid,
                |_| panic!("replacement child snapshot must stop the transition")
            ),
            None
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn live_parent_link_preserves_instance_and_rejects_stale_child() {
        let child = process_identity(std::process::id()).expect("live child");
        // SAFETY: getppid only reads the calling process's parent PID.
        let parent_pid = unsafe { libc::getppid() } as u32;
        let parent = parent_process_identity(child).expect("live parent link");
        assert_eq!(parent.pid, parent_pid);
        assert_eq!(process_identity(parent_pid), Some(parent));
        assert!(parent_process_identity(ProcessIdentity {
            start_time: child.start_time.wrapping_add(1),
            ..child
        })
        .is_none());
    }

    #[test]
    fn session_observation_rejects_peer_swap_after_precheck() {
        let root = ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        let peer = ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let current = Cell::new(Some(peer));
        assert_eq!(
            observe_pane_session(
                root,
                peer,
                |pid| if pid == root.pid {
                    Some(root)
                } else {
                    current.get()
                },
                |_, _| {
                    current.set(Some(ProcessIdentity {
                        start_time: 30,
                        ..peer
                    }));
                    Some(true)
                },
            ),
            None
        );
    }
}

/// Registers a Unix local listener with Tokio's native readiness reactor
/// (epoll on Linux, kqueue on macOS), without taking ownership of the listener.
/// The duplicate stays alive until the registration is dropped.
#[cfg(unix)]
pub(crate) fn local_listener_readiness(
    listener: &crate::ipc::LocalListener,
) -> std::io::Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::AsFd as _;

    let crate::ipc::LocalListener::UdSocket(listener) = listener;
    let fd = listener.as_fd().try_clone_to_owned()?;
    tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn classify_child_exit(_status: &portable_pty::ExitStatus) -> ChildExitReason {
    ChildExitReason::Exited
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_executable() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

pub(crate) fn detached_custom_command_process(command: &str) -> std::process::Command {
    let mut process = detached_custom_command_process_platform(command);
    configure_background_command(&mut process);
    process
}

pub(crate) fn pane_custom_command_pty_builder(command: &str) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_platform(command)
}

pub(crate) fn apply_pane_runtime_marker(command: &mut portable_pty::CommandBuilder) {
    apply_pane_runtime_marker_platform(command);
}

pub(crate) fn prepare_paste_text_for_pty(text: String) -> String {
    prepare_paste_text_for_pty_platform(text)
}

pub(crate) fn plugin_runtime_path(path: &std::path::Path) -> std::path::PathBuf {
    plugin_runtime_path_platform(path)
}

pub(crate) fn normalize_cwd_for_launch(path: &std::path::Path) -> std::path::PathBuf {
    normalize_cwd_for_launch_platform(path)
}

#[cfg(not(windows))]
fn normalize_cwd_for_launch_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(not(windows))]
fn plugin_runtime_path_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(not(windows))]
fn prepare_paste_text_for_pty_platform(text: String) -> String {
    text
}

#[cfg(not(windows))]
pub(crate) fn terminal_title_for_presentation(title: &str) -> &str {
    title
}

#[cfg(not(windows))]
fn apply_pane_runtime_marker_platform(_command: &mut portable_pty::CommandBuilder) {}

pub(crate) fn configure_background_command(command: &mut std::process::Command) {
    configure_background_command_platform(command);
}

#[cfg(not(windows))]
fn configure_background_command_platform(_command: &mut std::process::Command) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCapabilities {
    pub(crate) live_handoff: bool,
    pub(crate) direct_terminal_attach: bool,
    pub(crate) preserve_legacy_doubled_escape_input: bool,
}

pub(crate) const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        live_handoff: cfg!(unix),
        direct_terminal_attach: cfg!(unix),
        preserve_legacy_doubled_escape_input: cfg!(target_os = "macos"),
    }
}

pub(crate) fn terminal_grid_size() -> std::io::Result<(u16, u16)> {
    #[cfg(unix)]
    let (cols, rows) = unix_common::read_terminal_grid_size()?;
    #[cfg(windows)]
    let (cols, rows) = windows::read_terminal_grid_size()?;
    #[cfg(not(any(unix, windows)))]
    let (cols, rows) = fallback::read_terminal_grid_size()?;

    if cols == 0 || rows == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal reported a zero-sized grid",
        ));
    }
    Ok((cols, rows))
}

/// Capture a process instance once at local-transport acceptance. Linux prefers
/// a native socket-bound pidfd and falls back to connected-socket SO_PEERCRED plus
/// a start-time pin. Missing/invalid evidence remains None and policy refuses it.
#[cfg(unix)]
pub(crate) fn local_socket_peer_identity(fd: std::os::fd::RawFd) -> Option<ProcessIdentity> {
    #[cfg(target_os = "linux")]
    return linux::local_socket_peer_identity_platform(fd);

    #[cfg(target_os = "macos")]
    return macos::local_socket_peer_identity_platform(fd);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// Returns the PID connected to a Unix-domain socket when the platform exposes
/// it. Unsupported or unavailable attribution deliberately returns `None`.
#[cfg(all(unix, test))]
pub(crate) fn local_socket_peer_pid(fd: std::os::fd::RawFd) -> Option<u32> {
    #[cfg(target_os = "linux")]
    return linux::local_socket_peer_pid_platform(fd);

    #[cfg(target_os = "macos")]
    return macos::local_socket_peer_pid_platform(fd);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// A handle on the process at the other end of a connected Unix-domain
/// socket that cannot be confused with a later process reusing its PID.
#[cfg(target_os = "linux")]
pub(crate) use linux::LocalSocketPeerProcess;

/// Platforms without a PID-reuse-safe process handle never get one: callers
/// must not signal the peer by numeric PID instead.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) enum LocalSocketPeerProcess {}

#[cfg(all(unix, not(target_os = "linux")))]
impl LocalSocketPeerProcess {
    pub(crate) fn pid(&self) -> u32 {
        match *self {}
    }

    pub(crate) fn kill(&self) -> std::io::Result<bool> {
        match *self {}
    }
}

/// Opens a PID-reuse-safe handle on the process connected to `fd`, and only
/// while that process still holds its end of the connection. `None` when the
/// peer is gone or the platform has no such handle.
#[cfg(unix)]
pub(crate) fn local_socket_peer_process(fd: std::os::fd::RawFd) -> Option<LocalSocketPeerProcess> {
    // Deterministic unavailable-handle injection; release builds ignore it.
    #[cfg(debug_assertions)]
    if std::env::var("HERDR_TEST_HANDOFF_NO_PEER_PROCESS").as_deref() == Ok("1") {
        return None;
    }

    #[cfg(target_os = "linux")]
    return linux::local_socket_peer_process_platform(fd);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = fd;
        None
    }
}

#[cfg(not(windows))]
pub fn launch_server_daemon_command(command: &mut std::process::Command) -> std::io::Result<u32> {
    command.spawn().map(|child| child.id())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn prepare_server_process(_handoff_import: bool) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    #[cfg(target_os = "macos")]
    macos::configure_server_daemon_context(command);

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn current_process_is_detached_server_daemon() -> bool {
    unsafe { libc::getsid(0) == libc::getpid() }
}

/// Raised by the SIGWINCH handler, consumed by the host resize watcher.
#[cfg(unix)]
static TERMINAL_RESIZE_SIGNALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn record_terminal_resize_signal(_signal: libc::c_int) {
    TERMINAL_RESIZE_SIGNALLED.store(true, std::sync::atomic::Ordering::Release);
}

/// Records SIGWINCH events that size polling can miss.
#[cfg(unix)]
pub(crate) fn watch_terminal_resize_signal() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        record_terminal_resize_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Keep blocking stdin and socket reads from failing with EINTR.
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
pub(crate) fn watch_terminal_resize_signal() {}

/// Returns whether a terminal size change was signalled since the last call.
#[cfg(unix)]
pub(crate) fn take_terminal_resize_signal() -> bool {
    TERMINAL_RESIZE_SIGNALLED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Windows relies on size polling.
#[cfg(not(unix))]
pub(crate) fn take_terminal_resize_signal() -> bool {
    false
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

pub(crate) fn read_limited_reader(
    mut reader: impl std::io::Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<std::path::PathBuf>,
    pub(crate) system_config: Option<std::path::PathBuf>,
    pub(crate) multiplexing: bool,
}

pub(crate) const REMOTE_BRIDGE_IDLE_TIMEOUT_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(unix)]
mod remote_bridge;
#[cfg(all(test, unix))]
mod remote_bridge_tests;
#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
pub(crate) use unix_common::{
    begin_cli_output, end_cli_output, forward_remote_bridge_stdio, DiagnosticDirectoryScan,
    PrivateDiagnosticDirectory, RemoteBridgeWake,
};

#[cfg(not(any(unix, windows)))]
mod unsupported_diagnostics;
#[cfg(not(any(unix, windows)))]
pub(crate) use unsupported_diagnostics::{DiagnosticDirectoryScan, PrivateDiagnosticDirectory};

mod client_state;
pub(crate) use client_state::{
    create_private_state_file, open_private_append_file, replace_file, sync_parent_directory,
};

#[cfg(not(unix))]
pub(crate) fn begin_cli_output() {}

#[cfg(not(unix))]
pub(crate) fn end_cli_output() {}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

fn normalized_process_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn is_powershell_process_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn interactive_unix_shell_command(
    argv: &[String],
    shell_name: &str,
    quote_posix_arg: fn(&str) -> String,
) -> Option<String> {
    let quote = if is_powershell_process_name(shell_name) {
        quote_powershell_arg
    } else {
        quote_posix_arg
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

pub(crate) fn quote_powershell_arg(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote_windows_command_line_arg(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return value.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
        }
        backslashes = 0;
        quoted.push(ch);
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = normalized_process_name(name);
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_agent_hint(_pid: u32) -> Option<crate::detect::Agent> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"HERDR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
pub(crate) struct InputSourceRestore;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn pump_input_source_runloop() {}

/// Switches the host keyboard input source while prefix mode is active.
///
/// `App` drives this through a trait so the prefix-mode transitions can be
/// tested with a fake, without touching the real macOS APIs or leaking a
/// platform-specific restore type into `App`.
pub(crate) trait PrefixInputSource {
    /// Switch to an ASCII-capable input source for prefix commands. No-op if
    /// the current source is already ASCII-capable, the platform is
    /// unsupported, or the switch fails. Calling it again before `restore`
    /// keeps the source saved by the first call.
    fn switch_to_ascii(&mut self);

    /// Restore whatever `switch_to_ascii` saved. No-op if nothing was switched.
    fn restore(&mut self);
}

/// Production [`PrefixInputSource`] backed by the per-platform API.
#[derive(Default)]
pub(crate) struct RealPrefixInputSource {
    restore: Option<InputSourceRestore>,
}

impl PrefixInputSource for RealPrefixInputSource {
    fn switch_to_ascii(&mut self) {
        if self.restore.is_none() {
            // Drain pending input-source-change notifications so the read below is fresh (see
            // `pump_input_source_runloop`); a no-op on non-macOS.
            pump_input_source_runloop();
            self.restore = switch_to_ascii_input_source();
        }
    }

    fn restore(&mut self) {
        let _ = self.restore.take();
    }
}

#[cfg(all(test, any(unix, windows)))]
#[test]
fn child_exit_classification_only_checkpoints_interruptions() {
    for code in [0, 1, 130, 255, 0xC0000005] {
        let reason = classify_child_exit(&portable_pty::ExitStatus::with_exit_code(code));
        assert_eq!(reason, ChildExitReason::Exited, "exit code {code:#x}");
        assert!(!reason.requires_session_checkpoint());
    }
    #[cfg(windows)]
    let status = portable_pty::ExitStatus::with_exit_code(0xC000013A);
    #[cfg(not(windows))]
    let status = portable_pty::ExitStatus::with_signal("Terminated: 15");
    assert_eq!(classify_child_exit(&status), ChildExitReason::Interrupted);
    assert!(classify_child_exit(&status).requires_session_checkpoint());
    #[cfg(unix)]
    assert!(ChildExitReason::Handoff.requires_session_checkpoint());
    assert!(!ChildExitReason::WaitFailed.requires_session_checkpoint());
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn terminal_resize_signal_is_recorded_once_per_delivery() {
        watch_terminal_resize_signal();
        assert!(!take_terminal_resize_signal());

        unsafe {
            libc::raise(libc::SIGWINCH);
        }

        assert!(take_terminal_resize_signal());
        assert!(!take_terminal_resize_signal());
    }

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "pwsh", "powershell.exe"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }

    #[test]
    fn detached_custom_command_preserves_unix_login_shell_flag() {
        let cmd = detached_custom_command_process("echo hello");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-lc"),
                std::ffi::OsStr::new("echo hello")
            ]
        );
    }

    #[test]
    fn pane_custom_command_builder_preserves_unix_shell_flag() {
        let expected: Vec<std::ffi::OsString> =
            vec!["/bin/sh".into(), "-c".into(), "echo hello".into()];
        assert_eq!(
            pane_custom_command_pty_builder("echo hello").get_argv(),
            &expected
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_accepts_known_agents() {
        assert_eq!(
            parse_agent_env_hint(b"PATH=/bin\0HERDR_AGENT=claude\0TERM=xterm\0"),
            Some(crate::detect::Agent::Claude)
        );
        assert_eq!(
            parse_agent_env_hint(b"HERDR_AGENT=codex"),
            Some(crate::detect::Agent::Codex)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
        assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
        assert_eq!(parse_agent_env_hint(b"HERDR_AGENT=not-an-agent\0"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_shell_command_quotes_for_posix_and_powershell() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv, "bash").as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
        assert_eq!(
            interactive_shell_command(&argv, "pwsh").as_deref(),
            Some("pi '' 'two words' 'a''b' '$HOME' 'semi;colon' '@options'")
        );
    }

    #[test]
    fn read_limited_reader_returns_complete_data_under_limit() {
        let input = std::io::Cursor::new(b"image".to_vec());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_returns_empty_for_empty_input() {
        let input = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Empty
        );
    }

    #[test]
    fn read_limited_reader_accepts_data_exactly_at_limit() {
        let input = std::io::Cursor::new(b"four".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Complete(b"four".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_rejects_data_over_limit() {
        let input = std::io::Cursor::new(b"oversized".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Oversized
        );
    }

    #[test]
    fn read_limited_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }

        let input = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"image".to_vec()),
        };
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }
}

#[cfg(not(unix))]
pub(crate) fn shared_ssh_control_path(
    _namespace: &std::path::Path,
    _target: &str,
) -> std::io::Result<std::path::PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "interactive SSH recovery requires Unix OpenSSH multiplexing",
    ))
}
