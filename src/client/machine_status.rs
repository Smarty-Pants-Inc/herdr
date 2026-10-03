//! Bounded, client-owned diagnostics for `status client`. This is not a server protocol.
//!
//! The foreground loop publishes at most once per second, from its Timer branch (never
//! from render). Readers require both a live owner PID and a recent atomic snapshot.
//! Multiple live owners require explicit selection; missing/old clients fail closed.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsStr;

use crate::platform::{DiagnosticDirectoryScan, PrivateDiagnosticDirectory};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointRegistry, ProfileId, SavedSshEndpoint,
};
use super::shell::{ClientShellEndpoint, ClientShellState};

const SCHEMA_VERSION: u32 = 1;
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_AGE_MS: u64 = 5_000;
const MAX_BYTES: u64 = 64 * 1024;
const MAX_FILES: usize = 128;
// Recovery has a separate, finite budget from live inventory and retained storage.
// This accommodates a full legacy directory plus crashed atomic-write leftovers.
const MAX_SCAN_FILES: usize = 1024;
const MAX_MACHINES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MachineStatus {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) enabled: bool,
    pub(crate) connected: bool,
    /// Membership in the machine sidebar model, not visibility in its scrolled viewport.
    pub(crate) listed: bool,
    /// Number in the retained snapshot; `ready` distinguishes current from retained data.
    pub(crate) workspace_count: usize,
    pub(crate) ready: bool,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    schema_version: u32,
    client_id: String,
    pid: u32,
    updated_at_ms: u64,
    version: String,
    endpoints: Vec<MachineStatus>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ReadoutCandidate {
    client_id: String,
    pid: u32,
    fresh: bool,
    age_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Readout {
    schema_version: u32,
    pub(crate) status: &'static str,
    pub(crate) available: bool,
    pub(crate) fresh: bool,
    client_id: Option<String>,
    pid: Option<u32>,
    updated_at_ms: Option<u64>,
    age_ms: Option<u64>,
    max_age_ms: u64,
    version: Option<String>,
    candidates: Vec<ReadoutCandidate>,
    error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RuntimeStatus {
    pub(crate) running: bool,
    pub(crate) endpoints: Vec<MachineStatus>,
    pub(crate) readout: Readout,
}

impl RuntimeStatus {
    fn unavailable(status: &'static str, error: Option<String>) -> Self {
        Self {
            running: false,
            endpoints: Vec::new(),
            readout: Readout {
                schema_version: SCHEMA_VERSION,
                status,
                available: false,
                fresh: false,
                client_id: None,
                pid: None,
                updated_at_ms: None,
                age_ms: None,
                max_age_ms: MAX_AGE_MS,
                version: None,
                candidates: Vec::new(),
                error,
            },
        }
    }
}

pub(crate) fn read_runtime_status(client_id: Option<&str>) -> RuntimeStatus {
    match unix_ms() {
        Ok(now) => read_from_directory(
            &diagnostic_directory(&crate::server::socket_paths::client_socket_path()),
            client_id,
            now,
            crate::platform::diagnostic_process_exists,
        ),
        Err(error) => RuntimeStatus::unavailable("invalid", Some(error.to_string())),
    }
}

fn diagnostic_directory(socket: &Path) -> PathBuf {
    socket.with_extension("machine-status")
}

/// Owns exactly one foreground client's file; dropping it does not affect other clients.
pub(super) struct MachineStatusPublisher {
    path: PathBuf,
    client_id: String,
    last_attempt: Option<Instant>,
    reclamation_scan: RefCell<Option<DiagnosticDirectoryScan>>,
}

impl MachineStatusPublisher {
    pub(super) fn new(socket: &Path) -> Self {
        let client_id = ProfileId::generate().to_string();
        Self {
            path: diagnostic_directory(socket).join(format!("{client_id}.json")),
            client_id,
            last_attempt: None,
            reclamation_scan: RefCell::new(None),
        }
    }

    /// Call only from the foreground Timer branch. Even collection/encoding is 1 Hz,
    /// independent of terminal output volume, render frequency and workspace/pane count.
    pub(super) fn publish_if_due(
        &mut self,
        now: Instant,
        profiles: &[SavedSshEndpoint],
        shell: &ClientShellState,
        registry: &EndpointRegistry,
    ) {
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) < PUBLISH_INTERVAL)
        {
            return;
        }
        // Also rate-limit failed writes, so an unavailable directory cannot create a hot loop.
        self.last_attempt = Some(now);
        let result = unix_ms().and_then(|updated_at_ms| {
            self.write(&Snapshot {
                schema_version: SCHEMA_VERSION,
                client_id: self.client_id.clone(),
                pid: std::process::id(),
                updated_at_ms,
                version: crate::build_info::version(),
                endpoints: collect_machines(profiles, shell.endpoint_views(), registry),
            })
        });
        if let Err(error) = result {
            tracing::warn!(%error, "client machine diagnostic snapshot could not be published");
        }
    }

    fn write(&self, snapshot: &Snapshot) -> io::Result<()> {
        let content = serde_json::to_vec(snapshot)?;
        if content.len() as u64 > MAX_BYTES {
            return Err(io::Error::other(
                "client machine diagnostic exceeds storage limit",
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("invalid diagnostic path"))?;
        let directory = PrivateDiagnosticDirectory::open(parent, true)?;
        directory.lock()?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| io::Error::other("invalid diagnostic filename"))?;
        // The PID is in the temporary filename so even a partial write has an
        // identifiable owner. Reclamation never guesses that an old timestamp is dead.
        let temporary = format!("{}.{}.tmp", self.client_id, std::process::id());
        let reclamation = {
            let mut scan = self.reclamation_scan.borrow_mut();
            reclaim_abandoned(
                &directory,
                name,
                &mut scan,
                crate::platform::diagnostic_process_exists,
            )
        };
        let retained = match reclamation {
            Ok(retained) => retained,
            Err(error) => {
                if error.kind() != io::ErrorKind::WouldBlock {
                    self.reclamation_scan.borrow_mut().take();
                }
                return Err(error);
            }
        };
        let existing = match directory.open_file(name) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        // Allow at most MAX_FILES retained entries plus one atomic-write staging
        // entry. The nonblocking lock makes admission and replacement one transaction.
        if retained + usize::from(!existing) + 1 > MAX_FILES + 1 {
            return Err(io::Error::other(
                "client machine diagnostic storage is full",
            ));
        }
        let mut file = directory.create_file(OsStr::new(&temporary))?;
        let result = (|| {
            file.write_all(&content)?;
            drop(file);
            // Diagnostics are ephemeral: atomic replacement, not durability/fsync, is required.
            directory.replace(OsStr::new(&temporary), name)
        })();
        if result.is_err() {
            let _ = directory.remove(OsStr::new(&temporary));
        }
        result
    }
}

impl Drop for MachineStatusPublisher {
    fn drop(&mut self) {
        // Reopen and validate instead of unlinking through a possibly substituted path.
        if let Some(parent) = self.path.parent() {
            if let Ok(directory) = PrivateDiagnosticDirectory::open(parent, false) {
                if directory.lock().is_ok() {
                    if let Some(name) = self.path.file_name() {
                        let _ = directory.remove(name);
                    }
                    let temporary = format!("{}.{}.tmp", self.client_id, std::process::id());
                    let _ = directory.remove(OsStr::new(&temporary));
                }
            }
        }
    }
}

fn collect_machines(
    profiles: &[SavedSshEndpoint],
    views: &[ClientShellEndpoint],
    registry: &EndpointRegistry,
) -> Vec<MachineStatus> {
    profiles
        .iter()
        .map(|profile| {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let view = views.iter().find(|view| view.endpoint_id == endpoint_id);
            let connection = registry.connection(&endpoint_id);
            // A successful handshake is connected even before its first snapshot arrives.
            // Keeping this independent of Online exposes connected-but-unready regressions.
            let connected = connection.is_some();
            MachineStatus {
                id: profile.id.to_string(),
                label: profile.label.clone(),
                enabled: profile.enabled,
                connected,
                listed: view.is_some(),
                workspace_count: view
                    .and_then(|view| view.snapshot.as_deref())
                    .map_or(0, |snapshot| snapshot.workspaces.len()),
                ready: connected
                    && view.is_some_and(|view| {
                        view.status == ClientEndpointStatus::Online
                            && view.snapshot.is_some()
                            && view.snapshot_generation
                                == connection.map(|connection| connection.generation)
                    }),
            }
        })
        .collect()
}

fn unix_ms() -> io::Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    u64::try_from(elapsed.as_millis()).map_err(io::Error::other)
}

#[cfg(test)]
fn load_snapshot(path: &Path) -> io::Result<Snapshot> {
    let directory = PrivateDiagnosticDirectory::open(
        path.parent()
            .ok_or_else(|| io::Error::other("invalid diagnostic path"))?,
        false,
    )?;
    load_snapshot_at(
        &directory,
        path.file_name()
            .ok_or_else(|| io::Error::other("invalid diagnostic filename"))?,
    )
}

fn load_snapshot_at(directory: &PrivateDiagnosticDirectory, name: &OsStr) -> io::Result<Snapshot> {
    let file = directory.open_file(name)?;
    if file.metadata()?.len() > MAX_BYTES {
        return Err(io::Error::other("diagnostic exceeds storage limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("diagnostic exceeds storage limit"));
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    let mut ids = HashSet::new();
    if snapshot.schema_version != SCHEMA_VERSION
        || snapshot.pid == 0
        || ProfileId::parse(&snapshot.client_id).is_err()
        || Path::new(name).file_stem().and_then(|stem| stem.to_str())
            != Some(snapshot.client_id.as_str())
        || snapshot.endpoints.len() > MAX_MACHINES
        || snapshot
            .endpoints
            .iter()
            .any(|endpoint| ProfileId::parse(&endpoint.id).is_err() || !ids.insert(&endpoint.id))
    {
        return Err(io::Error::other(
            "unsupported or invalid client machine diagnostic",
        ));
    }
    Ok(snapshot)
}

/// Reclaim only authenticated regular files with a demonstrably dead owner. Invalid
/// legacy temporaries with no attributable PID are retained, never guessed dead.
/// The admission limit bounds such unreclaimable storage instead of accumulating more.
fn reclaim_abandoned(
    directory: &PrivateDiagnosticDirectory,
    own_name: &OsStr,
    scan: &mut Option<DiagnosticDirectoryScan>,
    process_exists: impl Fn(u32) -> bool,
) -> io::Result<usize> {
    if !scan
        .as_ref()
        .map(|scan| scan.matches(directory))
        .transpose()?
        .unwrap_or(false)
    {
        *scan = Some(directory.scan()?);
    }
    let (names, complete) = scan
        .as_mut()
        .ok_or_else(|| io::Error::other("missing diagnostic scan"))?
        .next_batch(MAX_SCAN_FILES)?;
    for name in &names {
        let path = Path::new(name);
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        // Reject insecure files even if their content is corrupt or not ours.
        match directory.open_file(name) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        if name == own_name {
            continue; // Our current file may be repaired without trusting its payload.
        }
        if let Ok(snapshot) = load_snapshot_at(directory, name) {
            if !process_exists(snapshot.pid) {
                directory.remove(name)?;
                // A paired legacy partial temporary need not be in this scan batch.
                // Its client ID was unique to the demonstrably dead snapshot owner.
                let legacy_tmp = format!("{}.tmp", snapshot.client_id);
                match directory.open_file(OsStr::new(&legacy_tmp)) {
                    Ok(_) => {
                        if !load_snapshot_at(directory, OsStr::new(&legacy_tmp))
                            .is_ok_and(|tmp| process_exists(tmp.pid))
                        {
                            directory.remove(OsStr::new(&legacy_tmp))?;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
    for name in &names {
        let path = Path::new(name);
        if path.extension() != Some(OsStr::new("tmp")) {
            continue;
        }
        match directory.open_file(name) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        let dead = if let Some((id, pid)) = stem.rsplit_once('.') {
            ProfileId::parse(id).is_ok()
                && pid
                    .parse::<u32>()
                    .ok()
                    .is_some_and(|pid| pid != 0 && !process_exists(pid))
        } else {
            load_snapshot_at(directory, name).is_ok_and(|snapshot| !process_exists(snapshot.pid))
        };
        if dead {
            directory.remove(name)?;
        }
    }
    if !complete {
        // Keep the handle-backed cursor: the next Timer attempt continues past
        // retained live/invalid entries, rather than restarting at the first batch.
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "diagnostic reclamation is in progress",
        ));
    }
    *scan = None;
    // A fresh count under the publisher lock catches files admitted by other
    // publishers between batches; never rely on an old cursor's cumulative count.
    Ok(directory.names(MAX_FILES)?.len())
}

fn read_from_directory(
    directory: &Path,
    client_id: Option<&str>,
    now: u64,
    process_exists: impl Fn(u32) -> bool,
) -> RuntimeStatus {
    let mut snapshots = Vec::new();
    let result = (|| -> io::Result<()> {
        let directory = match PrivateDiagnosticDirectory::open(directory, false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        // Exact selection never depends on a full directory scan, including at the
        // recovery budget boundary. Validate before constructing the filename.
        let names = if let Some(id) = client_id {
            if ProfileId::parse(id).is_err() {
                return Ok(());
            }
            vec![format!("{id}.json").into()]
        } else {
            directory.names(MAX_SCAN_FILES)?
        };
        for name in names {
            if Path::new(&name).extension() != Some(OsStr::new("json")) {
                continue;
            }
            let snapshot = match load_snapshot_at(&directory, &name) {
                Ok(snapshot) => snapshot,
                // A client may exit and unlink after read_dir; that is not a corrupt readout.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if process_exists(snapshot.pid) {
                if snapshots.len() == MAX_FILES {
                    return Err(io::Error::other("too many live client machine diagnostics"));
                }
                snapshots.push(snapshot);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        return RuntimeStatus::unavailable("invalid", Some(error.to_string()));
    }
    snapshots.sort_by(|left, right| left.client_id.cmp(&right.client_id));
    let candidates = snapshots
        .iter()
        .map(|snapshot| {
            let age_ms = now.checked_sub(snapshot.updated_at_ms);
            ReadoutCandidate {
                client_id: snapshot.client_id.clone(),
                pid: snapshot.pid,
                fresh: age_ms.is_some_and(|age| age <= MAX_AGE_MS),
                age_ms,
            }
        })
        .collect();
    let selected = if let Some(client_id) = client_id {
        snapshots
            .iter()
            .position(|snapshot| snapshot.client_id == client_id)
    } else if snapshots.len() == 1 {
        Some(0)
    } else {
        None
    };
    let Some(index) = selected else {
        let ambiguous = client_id.is_none() && snapshots.len() > 1;
        let mut status = RuntimeStatus::unavailable(
            if ambiguous {
                "ambiguous"
            } else {
                "unavailable"
            },
            Some(if ambiguous {
                "multiple live client readouts; select one with --client-id".into()
            } else {
                "no live client readout; client may be absent or too old to publish diagnostics"
                    .into()
            }),
        );
        status.running = !snapshots.is_empty();
        status.readout.candidates = candidates;
        return status;
    };
    let snapshot = snapshots.swap_remove(index);
    let age_ms = now.checked_sub(snapshot.updated_at_ms);
    let fresh = age_ms.is_some_and(|age| age <= MAX_AGE_MS);
    RuntimeStatus {
        running: true,
        endpoints: if fresh {
            snapshot.endpoints
        } else {
            Vec::new()
        },
        readout: Readout {
            schema_version: SCHEMA_VERSION,
            status: if fresh { "fresh" } else { "stale" },
            available: fresh,
            fresh,
            client_id: Some(snapshot.client_id),
            pid: Some(snapshot.pid),
            updated_at_ms: Some(snapshot.updated_at_ms),
            age_ms,
            max_age_ms: MAX_AGE_MS,
            version: Some(snapshot.version),
            candidates,
            error: (!fresh)
                .then(|| "client readout is expired or its timestamp is in the future".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::endpoint::{EndpointCatalog, EndpointNegotiation, EndpointTransport};
    use super::super::shell::ClientShellConfig;
    use super::*;
    use crate::protocol::{ClientMessage, ClientShellSnapshot, ClientShellWorkspace};

    struct Sink;
    impl EndpointTransport for Sink {
        fn send(&mut self, _message: &ClientMessage) -> io::Result<()> {
            Ok(())
        }
    }

    fn fixture() -> (EndpointCatalog, ClientShellState, EndpointRegistry) {
        let mut catalog = EndpointCatalog::default();
        catalog.add_ssh("Enabled", "build", "default").unwrap();
        let disabled = catalog.add_ssh("Disabled", "offline", "default").unwrap();
        catalog.set_enabled(&disabled, false);
        let mut shell = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        shell.set_endpoint_catalog(&catalog.ssh);
        (catalog, shell, EndpointRegistry::empty())
    }

    fn snapshot(workspaces: usize) -> Box<ClientShellSnapshot> {
        Box::new(ClientShellSnapshot {
            boot_id: "diagnostic-test".into(),
            revision: 1,
            config_diagnostic: None,
            product_announcement: None,
            update_available: None,
            update_install_command: String::new(),
            server_keybindings_toml: None,
            latest_release_notes_available: false,
            integration_updates_available: false,
            worktree_directory: String::new(),
            release_notes: None,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: String::new(),
            agent_view_label: None,
            agent_order: Vec::new(),
            workspaces: (0..workspaces)
                .map(|number| ClientShellWorkspace {
                    workspace_id: format!("w{number}"),
                    active_tab_id: String::new(),
                    new_workspace_cwd: String::new(),
                    number,
                    label: format!("empty-{number}"),
                    custom_label: false,
                    branch: None,
                    git_ahead_behind: None,
                    tokens: Vec::new(),
                    worktree: None,
                    focused: false,
                    agent_status: crate::api::schema::AgentStatus::Unknown,
                })
                .collect(),
            // No panes/tabs: workspace_count must not silently exclude empty workspaces.
            tabs: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
            commands: Vec::new(),
        })
    }

    fn storage_sample(client_id: &str, pid: u32) -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            client_id: client_id.into(),
            pid,
            updated_at_ms: 20_000,
            version: "storage-test".into(),
            endpoints: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_directory_is_rejected_by_reader_and_publisher() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let publisher = MachineStatusPublisher::new(&root.join("client.sock"));
        let sample = storage_sample(&publisher.client_id, std::process::id());
        publisher.write(&sample).unwrap();
        let directory = publisher.path.parent().unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read_from_directory(directory, None, 20_000, |_| true)
                .readout
                .status,
            "invalid"
        );
        assert!(
            publisher.write(&sample).is_err(),
            "must not adopt a public directory"
        );
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.join("alias.machine-status");
        symlink(directory, &alias).unwrap();
        assert_eq!(
            read_from_directory(&alias, None, 20_000, |_| true)
                .readout
                .status,
            "invalid"
        );
        let mut redirected = MachineStatusPublisher::new(&root.join("alias.sock"));
        redirected.path = alias.join(publisher.path.file_name().unwrap());
        assert!(
            redirected.write(&sample).is_err(),
            "must not follow a diagnostic directory symlink"
        );
        drop(redirected);
        drop(publisher);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_files_are_rejected_without_following_or_repairing_them() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let publisher = MachineStatusPublisher::new(&root.join("client.sock"));
        let sample = storage_sample(&publisher.client_id, std::process::id());
        publisher.write(&sample).unwrap();
        std::fs::set_permissions(&publisher.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_from_directory(publisher.path.parent().unwrap(), None, 20_000, |_| true)
                .readout
                .status,
            "invalid"
        );
        assert!(
            publisher.write(&sample).is_err(),
            "must not replace an untrusted diagnostic file"
        );
        std::fs::remove_file(&publisher.path).unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, &publisher.path).unwrap();
        assert!(publisher.write(&sample).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        drop(publisher);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owned_storage_is_rejected_before_json_or_pid_checks() {
        use std::os::unix::fs::MetadataExt;
        // A real foreign-owned regular file, rather than a JSON parse error masquerading
        // as an ownership test. No root/chown privilege is needed for this probe.
        let foreign = Path::new("/etc/passwd");
        if std::fs::metadata(foreign).unwrap().uid() == unsafe { libc::geteuid() } {
            return; // Root runners cannot use this foreign-owner fixture.
        }
        assert_eq!(
            load_snapshot(foreign).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn abandoned_snapshots_and_temporaries_do_not_hide_a_live_client() {
        let root = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let healthy = MachineStatusPublisher::new(&root.join("client.sock"));
        let directory = healthy.path.parent().unwrap();
        // Build the crash debris directly: producer cleanup must not sanitize the
        // failing-first reproduction before the reader has seen it.
        let storage = PrivateDiagnosticDirectory::open(directory, true).unwrap();
        let dead_pid = i32::MAX as u32;
        assert!(!crate::platform::diagnostic_process_exists(dead_pid));
        for _ in 0..MAX_FILES {
            let id = ProfileId::generate().to_string();
            let content = serde_json::to_vec(&storage_sample(&id, dead_pid)).unwrap();
            for extension in ["json", "tmp"] {
                let mut file = crate::platform::create_private_state_file(
                    &directory.join(format!("{id}.{extension}")),
                )
                .unwrap();
                file.write_all(&content).unwrap();
            }
            // New-format interrupted writes can be reclaimed even if JSON is partial.
            let mut file = crate::platform::create_private_state_file(
                &directory.join(format!("{id}.{dead_pid}.tmp")),
            )
            .unwrap();
            file.write_all(b"{").unwrap();
        }
        let content =
            serde_json::to_vec(&storage_sample(&healthy.client_id, std::process::id())).unwrap();
        crate::platform::create_private_state_file(&healthy.path)
            .unwrap()
            .write_all(&content)
            .unwrap();
        for selector in [None, Some(healthy.client_id.as_str())] {
            let readout =
                read_from_directory(directory, selector, 20_000, |pid| pid == std::process::id());
            assert!(
                readout.readout.fresh,
                "abandoned files blocked healthy selection: {:?}",
                readout.readout
            );
        }
        let other = MachineStatusPublisher::new(&root.join("client.sock"));
        other
            .write(&storage_sample(&other.client_id, std::process::id()))
            .unwrap();
        healthy
            .write(&storage_sample(&healthy.client_id, std::process::id()))
            .unwrap();
        assert!(
            other.path.exists(),
            "reclamation must preserve another live client"
        );
        assert_eq!(storage.names(MAX_FILES).unwrap().len(), 2);
        assert_eq!(
            read_from_directory(directory, None, 20_000, |_| true)
                .readout
                .status,
            "ambiguous"
        );
        assert!(
            read_from_directory(directory, Some(&healthy.client_id), 20_000, |_| true)
                .readout
                .fresh
        );
        drop(other);
        drop(healthy);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn producer_reclaims_beyond_one_scan_budget_and_preserves_live_temporaries() {
        let root = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let healthy = MachineStatusPublisher::new(&root.join("client.sock"));
        let other = MachineStatusPublisher::new(&root.join("client.sock"));
        let directory = healthy.path.parent().unwrap();
        let storage = PrivateDiagnosticDirectory::open(directory, true).unwrap();
        let write_fixture = |name: &OsStr, content: &[u8]| {
            storage
                .create_file(name)
                .unwrap()
                .write_all(content)
                .unwrap();
        };
        for publisher in [&healthy, &other] {
            write_fixture(
                publisher.path.file_name().unwrap(),
                &serde_json::to_vec(&storage_sample(&publisher.client_id, std::process::id()))
                    .unwrap(),
            );
        }
        let live_tmp = format!("{}.{}.tmp", ProfileId::generate(), std::process::id());
        write_fixture(OsStr::new(&live_tmp), b"{");
        let legacy_id = ProfileId::generate().to_string();
        let legacy_live_tmp = format!("{legacy_id}.tmp");
        write_fixture(
            OsStr::new(&legacy_live_tmp),
            &serde_json::to_vec(&storage_sample(&legacy_id, std::process::id())).unwrap(),
        );
        let unowned_tmp = format!("{}.tmp", ProfileId::generate());
        write_fixture(OsStr::new(&unowned_tmp), b"{");
        let dead_pid = i32::MAX as u32;
        assert!(!crate::platform::diagnostic_process_exists(dead_pid));
        for _ in 0..(2 * MAX_SCAN_FILES + 17) {
            let id = ProfileId::generate().to_string();
            write_fixture(
                OsStr::new(&format!("{id}.json")),
                &serde_json::to_vec(&storage_sample(&id, dead_pid)).unwrap(),
            );
        }
        let selected = read_from_directory(
            directory,
            Some(&healthy.client_id),
            20_000,
            crate::platform::diagnostic_process_exists,
        );
        assert!(
            selected.readout.fresh,
            "exact selection must not require a directory scan"
        );
        assert_eq!(
            read_from_directory(
                directory,
                None,
                20_000,
                crate::platform::diagnostic_process_exists
            )
            .readout
            .status,
            "invalid"
        );
        let mut attempts = 0;
        loop {
            attempts += 1;
            match healthy.write(&storage_sample(&healthy.client_id, std::process::id())) {
                Ok(()) => break,
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                    assert!(attempts < 5, "bounded scans must continue making progress");
                }
            }
        }
        assert!(attempts > 1, "fixture must exceed one reclamation batch");
        for name in [&live_tmp, &legacy_live_tmp, &unowned_tmp] {
            assert!(
                directory.join(name).exists(),
                "must not guess a live or unattributable temporary is dead"
            );
        }
        assert!(other.path.exists());
        assert_eq!(storage.names(MAX_FILES).unwrap().len(), 5);
        assert_eq!(
            read_from_directory(
                directory,
                None,
                20_000,
                crate::platform::diagnostic_process_exists
            )
            .readout
            .status,
            "ambiguous"
        );
        assert!(
            read_from_directory(
                directory,
                Some(&healthy.client_id),
                20_000,
                crate::platform::diagnostic_process_exists
            )
            .readout
            .fresh
        );
        drop(other);
        drop(healthy);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn producer_storage_admission_is_bounded_without_deleting_live_clients() {
        let root = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let publisher = MachineStatusPublisher::new(&root.join("client.sock"));
        let directory = publisher.path.parent().unwrap();
        let storage = PrivateDiagnosticDirectory::open(directory, true).unwrap();
        let mut existing_id = String::new();
        for _ in 0..MAX_FILES {
            let id = ProfileId::generate().to_string();
            let bytes = serde_json::to_vec(&storage_sample(&id, std::process::id())).unwrap();
            storage
                .create_file(OsStr::new(&format!("{id}.json")))
                .unwrap()
                .write_all(&bytes)
                .unwrap();
            existing_id = id;
        }
        assert!(publisher
            .write(&storage_sample(&publisher.client_id, std::process::id()))
            .is_err());
        assert_eq!(storage.names(MAX_FILES).unwrap().len(), MAX_FILES);
        assert!(!publisher.path.exists());
        // At the same quota an existing live client can still refresh atomically.
        let mut existing = MachineStatusPublisher::new(&root.join("client.sock"));
        existing.client_id = existing_id.clone();
        existing.path = directory.join(format!("{existing_id}.json"));
        existing
            .write(&storage_sample(&existing_id, std::process::id()))
            .unwrap();
        assert_eq!(storage.names(MAX_FILES).unwrap().len(), MAX_FILES);
        drop(existing);
        drop(publisher);
        drop(storage);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_disabled_and_unreachable_machines_are_listed_without_snapshots() {
        let (catalog, shell, registry) = fixture();
        let rows = collect_machines(&catalog.ssh, shell.endpoint_views(), &registry);
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| row.listed && !row.connected && !row.ready && row.workspace_count == 0));
        assert!(rows[0].enabled);
        assert!(!rows[1].enabled);
        let missing = collect_machines(&catalog.ssh, &[], &registry);
        assert!(missing.iter().all(|row| !row.listed));
    }

    #[test]
    fn connected_without_snapshot_exposes_handoff_gap_and_generation_readiness() {
        let (catalog, mut shell, mut registry) = fixture();
        let id = ClientEndpointId::Ssh(catalog.ssh[0].id.clone());
        registry.insert(id.clone(), Sink, 1, EndpointNegotiation::default(), false);
        let rows = collect_machines(&catalog.ssh, shell.endpoint_views(), &registry);
        assert!(rows[0].connected);
        assert!(!rows[0].ready);
        shell.cache_endpoint_snapshot_for_generation(&id, 1, snapshot(110));
        shell.set_endpoint_status(&id, ClientEndpointStatus::Online);
        let rows = collect_machines(&catalog.ssh, shell.endpoint_views(), &registry);
        assert_eq!(rows[0].workspace_count, 110);
        assert!(rows[0].ready);
        // A replacement handshakes, but the old generation's snapshot is retained.
        registry.insert(id.clone(), Sink, 2, EndpointNegotiation::default(), false);
        let rows = collect_machines(&catalog.ssh, shell.endpoint_views(), &registry);
        assert!(rows[0].connected && rows[0].listed);
        assert_eq!(rows[0].workspace_count, 110);
        assert!(!rows[0].ready);
        shell.cache_endpoint_snapshot_for_generation(&id, 2, snapshot(0));
        let rows = collect_machines(&catalog.ssh, shell.endpoint_views(), &registry);
        assert!(rows[0].ready && rows[0].listed);
        assert_eq!(rows[0].workspace_count, 0);
        // Online shell status alone cannot imply transport connectivity.
        let rows = collect_machines(
            &catalog.ssh,
            shell.endpoint_views(),
            &EndpointRegistry::empty(),
        );
        assert!(!rows[0].connected && !rows[0].ready);
    }

    #[test]
    fn publisher_is_rate_limited_atomic_private_and_removes_only_own_file() {
        let (mut catalog, shell, registry) = fixture();
        let dir = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let mut publisher = MachineStatusPublisher::new(&dir.join("herdr-client.sock"));
        let other = publisher.path.parent().unwrap().join("other.json");
        let now = Instant::now();
        publisher.publish_if_due(now, &catalog.ssh, &shell, &registry);
        let first = std::fs::read(&publisher.path).unwrap();
        assert_eq!(load_snapshot(&publisher.path).unwrap().endpoints.len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&publisher.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        catalog.ssh[0].label = "Changed".into();
        publisher.publish_if_due(
            now + Duration::from_millis(999),
            &catalog.ssh,
            &shell,
            &registry,
        );
        assert_eq!(std::fs::read(&publisher.path).unwrap(), first);
        publisher.publish_if_due(
            now + Duration::from_secs(1),
            &catalog.ssh,
            &shell,
            &registry,
        );
        assert_eq!(
            load_snapshot(&publisher.path).unwrap().endpoints[0].label,
            "Changed"
        );
        assert!(!publisher.path.with_extension("tmp").exists());
        std::fs::write(&other, b"unrelated").unwrap();
        let path = publisher.path.clone();
        drop(publisher);
        assert!(!path.exists());
        assert!(other.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_dead_stale_future_and_ambiguous_writers_never_produce_usable_inventory() {
        let dir = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let first = MachineStatusPublisher::new(&dir.join("herdr-client.sock"));
        let second = MachineStatusPublisher::new(&dir.join("herdr-client.sock"));
        let directory = first.path.parent().unwrap();
        let read = |now, alive| read_from_directory(directory, None, now, |_| alive);
        assert_eq!(read(20_000, true).readout.status, "unavailable");
        let sample = |client_id: String, updated_at_ms| Snapshot {
            schema_version: 1,
            client_id,
            pid: std::process::id(),
            updated_at_ms,
            version: "test".into(),
            endpoints: vec![MachineStatus {
                id: ProfileId::generate().to_string(),
                label: "Saved".into(),
                enabled: true,
                connected: true,
                listed: true,
                workspace_count: 0,
                ready: true,
            }],
        };
        first
            .write(&sample(first.client_id.clone(), 20_000))
            .unwrap();
        let live = read(20_000, true);
        assert!(live.running && live.readout.available && live.readout.fresh);
        assert_eq!(live.endpoints.len(), 1);
        let dead = read(20_000, false);
        assert!(!dead.running && !dead.readout.fresh && dead.endpoints.is_empty());
        for now in [19_999, 25_001] {
            let stale = read(now, true);
            assert!(stale.running && !stale.readout.available && stale.endpoints.is_empty());
            assert_eq!(stale.readout.status, "stale");
        }
        second
            .write(&sample(second.client_id.clone(), 20_000))
            .unwrap();
        let ambiguous = read(20_000, true);
        assert!(ambiguous.running && !ambiguous.readout.fresh && ambiguous.endpoints.is_empty());
        assert_eq!(ambiguous.readout.status, "ambiguous");
        let selected = read_from_directory(directory, Some(&first.client_id), 20_000, |_| true);
        assert!(selected.readout.fresh);
        assert_eq!(
            selected.readout.client_id.as_deref(),
            Some(first.client_id.as_str())
        );
        assert_eq!(
            read_from_directory(directory, Some("absent"), 20_000, |_| true)
                .readout
                .status,
            "unavailable"
        );
        drop(first);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsupported_corrupt_oversized_and_duplicate_machine_readouts_fail_closed() {
        let dir = std::env::temp_dir().join(format!("herdr-diagnostic-{}", ProfileId::generate()));
        let publisher = MachineStatusPublisher::new(&dir.join("herdr-client.sock"));
        let mut sample = Snapshot {
            schema_version: 99,
            client_id: publisher.client_id.clone(),
            pid: 12,
            updated_at_ms: 20_000,
            version: "future".into(),
            endpoints: Vec::new(),
        };
        publisher.write(&sample).unwrap();
        let assert_invalid = || {
            let readout =
                read_from_directory(publisher.path.parent().unwrap(), None, 20_000, |_| true);
            assert_eq!(readout.readout.status, "invalid");
            assert!(!readout.readout.fresh && readout.endpoints.is_empty());
        };
        assert_invalid();
        std::fs::write(&publisher.path, b"{").unwrap();
        assert_invalid();
        std::fs::write(&publisher.path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert_invalid();
        sample.schema_version = 1;
        sample.endpoints = vec![
            MachineStatus {
                id: ProfileId::generate().to_string(),
                label: "same".into(),
                enabled: true,
                connected: false,
                listed: true,
                workspace_count: 0,
                ready: false,
            };
            2
        ];
        publisher.write(&sample).unwrap();
        assert_invalid();
        drop(publisher);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
