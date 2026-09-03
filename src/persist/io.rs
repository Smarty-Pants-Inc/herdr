use std::io::Write as _;
use std::path::{Path, PathBuf};

use tracing::warn;

use super::snapshot::{
    content_has_external_resume_policy, parse_history_snapshot, parse_snapshot,
    snapshot_file_version, SessionHistorySnapshot, SessionSnapshot,
    EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION, SNAPSHOT_VERSION,
};

fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
}

pub enum SessionLoad {
    Missing,
    Loaded(SessionSnapshot),
    Unsupported { version: u32 },
}

// Follow symlinks manually so a write through a (possibly dangling) symlink
// lands on the target. `fs::canonicalize` requires the target to exist, which
// excludes the dangling-symlink case stow users hit on the very first save.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    save_json_to_path(path, snapshot)
}

fn write_bytes_to_path(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = target.with_extension("json.tmp");
    let write_result = (|| {
        let mut tmp = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;
        tmp.write_all(bytes)?;
        tmp.sync_all()
    })();
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Some(parent) = parent {
        sync_directory(parent)?;
    }
    Ok(())
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(snapshot)?;
    write_bytes_to_path(path, &json)
}

fn existing_file_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let target = resolve_write_target(path)?;
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if !metadata.file_type().is_file() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "persistence target is not a regular file: {}",
                target.display()
            ),
        )),
        Ok(_) => std::fs::read(target).map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn restore_file(path: &Path, previous: Option<&[u8]>) -> std::io::Result<()> {
    match previous {
        Some(bytes) => write_bytes_to_path(path, bytes),
        None => clear_path(&resolve_write_target(path)?),
    }
}

fn restore_pair_after_failure(
    error: std::io::Error,
    session_path: &Path,
    session_before: Option<&[u8]>,
    history_path: &Path,
    history_before: Option<&[u8]>,
) -> std::io::Result<()> {
    let mut restore_errors = Vec::new();
    if let Err(err) = restore_file(session_path, session_before) {
        restore_errors.push(format!("session restore failed: {err}"));
    }
    if let Err(err) = restore_file(history_path, history_before) {
        restore_errors.push(format!("history restore failed: {err}"));
    }
    if restore_errors.is_empty() {
        Err(error)
    } else {
        Err(std::io::Error::other(format!(
            "{error}; {}",
            restore_errors.join("; ")
        )))
    }
}

pub(super) fn save_to_paths(
    session_path: &Path,
    history_path: &Path,
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    let session_before = existing_file_bytes(session_path)?;
    let history_before = existing_file_bytes(history_path)?;
    let result = save_to_path(session_path, snapshot).and_then(|_| {
        if let Some(history) = history {
            save_json_to_path(history_path, history)
        } else {
            clear_path(&resolve_write_target(history_path)?)
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => restore_pair_after_failure(
            error,
            session_path,
            session_before.as_deref(),
            history_path,
            history_before.as_deref(),
        ),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn save(snapshot: &SessionSnapshot, history: Option<&SessionHistorySnapshot>) {
    let path = session_path();
    let history_path = session_history_path();
    if let Err(err) = save_to_paths(&path, &history_path, snapshot, history) {
        crate::logging::session_save_failed(&path, &err.to_string());
        return;
    }
    crate::logging::session_saved(&path, snapshot.workspaces.len());
}

pub fn clear() {
    let path = session_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
        return;
    }
    clear_history();
    if let Err(err) = super::idempotency::clear_layout_apply_ledger() {
        crate::logging::session_clear_failed(&path, &err.to_string());
        return;
    }
    crate::logging::session_cleared(&path);
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load() -> SessionLoad {
    load_from_path(&session_path())
}

fn load_from_path(path: &Path) -> SessionLoad {
    if !path.exists() {
        return SessionLoad::Missing;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session file");
            return SessionLoad::Missing;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => SessionLoad::Loaded(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION
                    || (version < EXTERNAL_RESUME_POLICY_SNAPSHOT_VERSION
                        && content_has_external_resume_policy(&content))
                {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session file is incompatible with this herdr version; persistence is blocked"
                    );
                    return SessionLoad::Unsupported { version };
                }
            }
            warn!(err = %err, "failed to parse session file, ignoring");
            SessionLoad::Missing
        }
    }
}

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session history file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    fn temp_session_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-session-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            idempotency_epoch: None,
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        }
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_paths(
            &session_path,
            &history_path,
            &empty_snapshot(),
            Some(&history_snapshot("split-secret")),
        )
        .unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&session).unwrap()["version"],
            SNAPSHOT_VERSION
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&history).unwrap()["version"],
            SNAPSHOT_VERSION
        );
    }

    #[test]
    fn save_to_paths_preserves_native_v3_pair() {
        let (session_path, history_path) = temp_session_paths("native-v3-pair");
        let mut snapshot = empty_snapshot();
        snapshot.version = 3;
        let mut history = history_snapshot("native-history");
        history.version = snapshot.version;

        save_to_paths(&session_path, &history_path, &snapshot, Some(&history)).unwrap();

        for path in [session_path, history_path] {
            let saved: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(saved["version"], 3);
        }
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_paths(
            &session_path,
            &history_path,
            &empty_snapshot(),
            Some(&history_snapshot("stale-secret")),
        )
        .unwrap();

        save_to_paths(&session_path, &history_path, &empty_snapshot(), None).unwrap();

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(parsed.selected, 7);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[test]
    fn future_snapshot_is_reported_unsupported_without_modification() {
        let path = temp_session_path("future-version");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = format!(
            r#"{{"version":{},"workspaces":[],"active":null,"selected":0}}"#,
            SNAPSHOT_VERSION + 1
        );
        std::fs::write(&path, content.as_bytes()).unwrap();

        match load_from_path(&path) {
            SessionLoad::Unsupported { version } => assert_eq!(version, SNAPSHOT_VERSION + 1),
            _ => panic!("future snapshot must block persistence"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
    }

    #[test]
    fn v5_external_policy_snapshot_blocks_persistence_without_modification() {
        let path = temp_session_path("v5-external-policy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 5,
            "workspaces": [{
                "identity_cwd": "/tmp",
                "tabs": [{
                    "layout": {"Pane": 1},
                    "panes": {"1": {
                        "cwd": "/tmp",
                        "agent_session": {
                            "source": "herdr:omp",
                            "agent": "omp",
                            "kind": "id",
                            "value": "external-session",
                            "resume_policy": "external"
                        }
                    }},
                    "zoomed": false
                }]
            }],
            "active": 0,
            "selected": 0
        }))
        .unwrap();
        std::fs::write(&path, &content).unwrap();

        match load_from_path(&path) {
            SessionLoad::Unsupported { version } => assert_eq!(version, 5),
            _ => panic!("v5 external-policy snapshot must block persistence"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }
}
