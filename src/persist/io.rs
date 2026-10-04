use std::path::{Path, PathBuf};

use tracing::warn;

use super::snapshot::{
    parse_history_snapshot, parse_snapshot, snapshot_file_version, SessionHistorySnapshot,
    SessionSnapshot, SNAPSHOT_VERSION,
};

pub(super) fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
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

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    let tmp_path = target.with_extension("json.tmp");
    // Session state can hold workspace launch env (secrets): create it owner-only
    // (0600 on unix) so the rename also replaces any older, wider file mode.
    clear_path(&tmp_path)?;
    let written = crate::platform::create_config_temporary(&tmp_path, true).and_then(|mut file| {
        std::io::Write::write_all(&mut file, json.as_bytes())?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&tmp_path, &target)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

pub(super) fn save_history_to_path(
    path: &Path,
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    match history {
        Some(history) => save_json_to_path(path, history),
        None => clear_path(path),
    }
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load() -> Option<SessionSnapshot> {
    load_from_path(&session_path())
}

fn load_from_path(path: &Path) -> Option<SessionSnapshot> {
    let (content, trust) = match crate::platform::read_session_snapshot_with_trust(path) {
        Ok(result) => result,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return None;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), err = %err, "failed to read session file"
            );
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(mut snapshot) => {
            if let Some(reason) = trust.refusal_reason() {
                let mut refused = 0;
                for workspace in &mut snapshot.workspaces {
                    for tab in &mut workspace.tabs {
                        for pane in tab.panes.values_mut() {
                            refused += usize::from(pane.cold_restore_argv);
                            pane.cold_restore_argv = false;
                        }
                    }
                }
                if refused > 0 {
                    warn!(
                        event = "persist.restore", subsystem = "persist", outcome = "argv_trust_refused",
                        path = %path.display(), reason, panes = refused,
                        "refusing cold restore argv from untrusted snapshot"
                    );
                }
            }
            Some(snapshot)
        }
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        event = "persist.restore", subsystem = "persist", outcome = "unsupported_version",
                        path = %path.display(), file_version = version, supported = SNAPSHOT_VERSION,
                        "session file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), err = %err, "failed to parse session file, ignoring"
            );
            None
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
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
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

    fn marked_snapshot() -> SessionSnapshot {
        parse_snapshot(r#"{
            "version":3, "workspaces":[{"identity_cwd":"/tmp","tabs":[{
                "layout":{"Pane":0},"zoomed":false,"panes":{
                    "0":{"cwd":"/tmp","label":"retained","launch_argv":["/program","secret argument"],"cold_restore_argv":true},
                    "1":{"cwd":"/tmp","launch_argv":["/other"],"cold_restore_argv":true}
                }
            }]}], "active":0,"selected":0
        }"#).unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cold_restore_file_trust_strips_marks_before_private_autosave() {
        use std::os::unix::fs::PermissionsExt as _;
        for mode in [0o600, 0o644, 0o620, 0o602, 0o666] {
            let path = temp_session_path("argv-trust");
            save_to_path(&path, &marked_snapshot()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let loaded = load_from_path(&path).unwrap();
            let panes = &loaded.workspaces[0].tabs[0].panes;
            let trusted = mode & 0o022 == 0;
            assert!(panes.values().all(|pane| pane.cold_restore_argv == trusted));
            assert_eq!(panes[&0].label.as_deref(), Some("retained"));
            assert_eq!(
                panes[&0].launch_argv.as_ref().unwrap()[1],
                "secret argument"
            );
            // Authorization must not repair the original mode before deciding.
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                mode
            );
            save_to_path(&path, &loaded).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(load_from_path(&path).unwrap().workspaces[0].tabs[0]
                .panes
                .values()
                .all(|pane| pane.cold_restore_argv == trusted));
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cold_restore_macos_disk_marks_fail_closed_before_autosave_and_second_restart() {
        use std::os::unix::fs::PermissionsExt as _;

        // Even owner-only, ACL-free files are refused until input ACL validation
        // exists. Wider modes must not change that policy or mutate the input.
        for mode in [0o600, 0o644, 0o666] {
            let path = temp_session_path("argv-trust-macos");
            let original = marked_snapshot();
            save_to_path(&path, &original).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let log_path = path.with_file_name("restore.log");
            let log_file = std::fs::File::create(&log_path).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(move || log_file.try_clone().unwrap())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let loaded = load_from_path(&path).unwrap();
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    mode,
                    "authorization must not repair the original input"
                );
                let assert_sanitized = |snapshot: &SessionSnapshot| {
                    let panes = &snapshot.workspaces[0].tabs[0].panes;
                    for (id, pane) in panes {
                        let saved = &original.workspaces[0].tabs[0].panes[id];
                        assert!(!pane.cold_restore_argv);
                        assert_eq!(pane.launch_argv, saved.launch_argv);
                        assert_eq!(pane.label, saved.label);
                        assert_eq!(pane.cwd, saved.cwd);
                    }
                };
                assert_sanitized(&loaded);
                // Model the private autosave of sanitized state, then the next
                // cold load: private replacement must never launder consent.
                save_to_path(&path, &loaded).unwrap();
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                assert_sanitized(&load_from_path(&path).unwrap());
            });
            let log = std::fs::read_to_string(&log_path).unwrap();
            assert_eq!(log.lines().count(), 1, "{log}");
            assert!(log.contains("argv_trust_refused"), "{log}");
            assert!(
                log.contains("refusing cold restore argv from untrusted snapshot"),
                "{log}"
            );
            assert!(
                log.contains("snapshot owner/ACL verification unavailable on macOS"),
                "{log}"
            );
            assert!(log.contains("panes=2"), "{log}");
            assert!(!log.contains("secret argument"), "{log}");
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn cold_restore_windows_disk_marks_fail_closed() {
        let path = temp_session_path("argv-trust-windows");
        save_to_path(&path, &marked_snapshot()).unwrap();
        assert!(load_from_path(&path).unwrap().workspaces[0].tabs[0]
            .panes
            .values()
            .all(|pane| !pane.cold_restore_argv));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret"))).unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret"))).unwrap();

        save_history_to_path(&history_path, None).unwrap();

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
    fn save_to_path_makes_session_files_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (session_path, history_path) = temp_session_paths("mode");
        std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        std::fs::write(&session_path, "{}").unwrap();
        std::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("s"))).unwrap();

        for path in [&session_path, &history_path] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
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
}
