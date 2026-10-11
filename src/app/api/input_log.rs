//! Herdr's API input log (smarty-dev#931).
//!
//! Every API call that writes to a pane's input appends one JSON line to
//! `<state dir>/api-input.jsonl` before the write: the caller (from socket peer attribution,
//! never from request text), the target pane, the time and the byte count. It never records the
//! text. A reader of a pane's turns can then tell sent input from typed input without anything
//! in the terminal byte stream. The log fails closed: if the line cannot be written, the API
//! write is refused.

use std::io::Write;

use crate::api::ApiRequestContext;
use crate::app::App;

/// The log file for the running server.
pub(crate) fn default_api_input_log_path() -> std::path::PathBuf {
    crate::config::state_dir().join("api-input.jsonl")
}

impl App {
    /// The actor invokes this durable sink before releasing an authoritative cut.
    /// Only metadata is representable in AuditRecord; capabilities, nonce and raw
    /// input cannot accidentally enter this log. Failure is handled by the actor
    /// as unknown(input_log_unavailable) plus epoch poison, not by this transport.
    pub(super) fn input_consumer_audit_sink(&self) -> crate::pty::input_consumer::AuditSink {
        let path = self.api_input_log.with_file_name("input-consumer.jsonl");
        std::sync::Arc::new(move |record| {
            let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
            append_line(&path, &line).inspect_err(|err| {
                tracing::warn!(err = %err, path = %path.display(), "input consumer audit log write failed");
            })
        })
    }

    /// Appends the log line for an API write of `bytes` bytes to a pane's input. On error the
    /// caller must not write, and returns the encoded error.
    pub(super) fn log_api_input(
        &self,
        id: &str,
        method: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        context: ApiRequestContext,
        bytes: usize,
    ) -> Result<(), String> {
        let line = self.api_input_log_line(method, ws_idx, pane_id, context, bytes);
        append_line(&self.api_input_log, &line).map_err(|err| {
            tracing::warn!(err = %err, path = %self.api_input_log.display(), "api input log write failed");
            super::responses::encode_error(
                id.to_string(),
                "input_log_unavailable",
                format!("cannot record this input in the API input log, so it was not sent: {err}"),
            )
        })
    }

    fn api_input_log_line(
        &self,
        method: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        context: ApiRequestContext,
        bytes: usize,
    ) -> String {
        let caller = context
            .local_peer_identity
            .and_then(|identity| self.pane_target_for_peer_identity(identity));
        let caller_pane = caller
            .as_ref()
            .and_then(|target| self.public_pane_id(target.ws_idx, target.pane_id));
        let caller_agent = caller
            .as_ref()
            .and_then(|target| self.agent_info(target.ws_idx, target.pane_id));
        let mut caller_fields = serde_json::json!({
            "pid": context.local_peer_pid(),
            "pane": caller_pane,
            "agent": caller_agent.as_ref().and_then(|agent| agent.name.clone()),
            "session": caller_agent
                .as_ref()
                .and_then(|agent| agent.agent_session.as_ref())
                .map(|session| session.value.clone()),
        });
        // Attribution only (admission was decided by the guard): the grant id is
        // the plugin command log id, never the token. Uses the minted record,
        // so a child exiting after admission keeps the accepted write attributed.
        if let Some(grant) = self.plugin_action_grants.attribution(context.plugin_action) {
            caller_fields["plugin_action"] = serde_json::json!({
                "plugin_id": grant.plugin_id,
                "action_id": grant.action_id,
                "invoking_pane": grant.invoking_pane,
                "grant": grant.grant_id,
            });
        }
        // One best-effort snapshot per logged write, independent of pane attribution.
        if let Some(metadata) = context
            .local_peer_identity
            .and_then(crate::platform::process_caller_metadata)
        {
            if let Some(exe) = metadata.exe {
                caller_fields["exe"] = exe.into();
            }
            if let Some(ppid) = metadata.ppid {
                caller_fields["ppid"] = ppid.into();
            }
            if let Some(unit) = metadata.unit {
                caller_fields["unit"] = unit.into();
            }
            #[cfg(target_os = "linux")]
            if let Some(sid) = metadata.sid {
                caller_fields["sid"] = sid.into();
            }
        }
        if let Some(peer) = context.local_peer_identity {
            // The guard may have skipped its chronology proof (explicit opt-in
            // or captured pane markers). Otherwise reuse its request-local
            // observation: no additional /proc walk to log verdict or root.
            let ancestry = context
                .server_ancestry
                .unwrap_or_else(|| crate::platform::process_identity_server_ancestry(peer));
            add_server_ancestry_fields(&mut caller_fields, ancestry);
            let herdr_exe = crate::platform::launch_executable()
                .ok()
                .and_then(|path| path.file_name()?.to_str().map(str::to_owned));
            if let Some((exe, pid)) = nearest_non_herdr_ancestor(
                peer,
                herdr_exe.as_deref(),
                crate::platform::process_identity,
                crate::platform::parent_process_identity,
                crate::platform::process_caller_metadata,
            ) {
                caller_fields["ancestor_exe"] = exe.into();
                caller_fields["ancestor_pid"] = pid.into();
            }
            if let Some(present) = crate::platform::process_initial_pane_env_present(peer) {
                caller_fields["pane_env_present"] = present.into();
            }
        } else {
            // smarty-dev#931: every null-caller line is classified. Without a peer there is
            // nothing to walk, so the verdict is unknown and no caller identity is invented.
            add_server_ancestry_fields(
                &mut caller_fields,
                crate::platform::ServerAncestryObservation {
                    verdict: crate::platform::ServerAncestry::Unknown,
                    #[cfg(target_os = "linux")]
                    root_pid: None,
                },
            );
        }
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default();
        serde_json::json!({
            "ts_ms": ts_ms,
            "method": method,
            "herdr_session": crate::session::active_name(),
            "target_pane": self.public_pane_id(ws_idx, pane_id),
            "target_terminal": self
                .state
                .terminal_id_for_pane(ws_idx, pane_id)
                .map(|terminal| terminal.to_string()),
            "bytes": bytes,
            "caller": caller_fields,
        })
        .to_string()
    }
}

fn add_server_ancestry_fields(
    caller: &mut serde_json::Value,
    observation: crate::platform::ServerAncestryObservation,
) {
    caller["ancestry"] = match observation.verdict {
        crate::platform::ServerAncestry::Outside => "outside",
        crate::platform::ServerAncestry::ReachedServer => "reached",
        crate::platform::ServerAncestry::Unknown
        | crate::platform::ServerAncestry::NotApplicable => "unknown",
    }
    .into();
    #[cfg(target_os = "linux")]
    if let Some(root) = observation.root_pid {
        caller["ancestry_root_pid"] = root.into();
    }
}

/// Best-effort diagnostic ancestry, never authorization. Start at the parent, not
/// the caller, and omit both fields if the nearest non-Herdr executable is unknown.
/// The actual server basename also covers renamed Herdr binaries.
fn nearest_non_herdr_ancestor(
    peer: crate::platform::ProcessIdentity,
    herdr_exe: Option<&str>,
    identity_of: impl Fn(u32) -> Option<crate::platform::ProcessIdentity>,
    parent_of: impl Fn(crate::platform::ProcessIdentity) -> Option<crate::platform::ProcessIdentity>,
    metadata_of: impl Fn(crate::platform::ProcessIdentity) -> Option<crate::platform::CallerMetadata>,
) -> Option<(String, u32)> {
    const MAX_DEPTH: usize = 32;
    let mut current = peer;
    let mut seen = Vec::with_capacity(MAX_DEPTH + 1);
    seen.push(peer);
    for _ in 0..MAX_DEPTH {
        if peer.pid == 0
            || identity_of(peer.pid) != Some(peer)
            || identity_of(current.pid) != Some(current)
        {
            return None;
        }
        let parent = parent_of(current)?;
        if parent.pid == 0
            || parent.start_time > current.start_time
            || seen.contains(&parent)
            || identity_of(peer.pid) != Some(peer)
            || identity_of(current.pid) != Some(current)
            || identity_of(parent.pid) != Some(parent)
        {
            return None;
        }
        let exe = metadata_of(parent)?.exe?;
        if identity_of(peer.pid) != Some(peer)
            || identity_of(current.pid) != Some(current)
            || identity_of(parent.pid) != Some(parent)
        {
            return None;
        }
        if Some(exe.as_str()) != herdr_exe
            && !matches!(
                exe.as_str(),
                "herdr" | "herdr-dev" | "herdr.exe" | "herdr-dev.exe"
            )
        {
            return Some((exe, parent.pid));
        }
        seen.push(parent);
        current = parent;
    }
    None
}

/// Appends `line` as one JSONL record and makes it durable before returning.
///
/// An exclusive file lock serializes writers, including other Herdr processes that share the
/// state directory. If an earlier write left a partial record (a short write before an error),
/// the record starts on a new line, so every accepted record stays separately parseable; the
/// fragment is a malformed line that readers skip.
///
/// Every call then syncs the directory entries of the log and of every directory above it: a
/// path that exists is not proof that it was made durable (an earlier attempt or another
/// process may have failed after creating it, or the log may have been moved aside and
/// recreated). ponytail: a few directory syncs per API write; API writes are infrequent.
///
/// Retention is bounded: see [`append_line_capped`].
fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    append_line_capped(path, line, LOG_ROTATE_BYTES)
}

/// ponytail: one 32 MiB generation plus one rotated `.1` bounds the log near 64 MiB, which
/// keeps months of metadata-only records; raise it if forensics need a longer window.
const LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;

/// The rotated generation: `<name>.1` beside the live log.
fn rotated_log_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// [`append_line`] with an explicit cap (tests pass a small one).
///
/// Identity rule: the log path is renamed only after it was checked, under the lock of the file
/// held, to name that same file (same dev and inode, a regular file owned by this user). If it
/// names another file (a writer rotated while this one waited), that file is opened privately
/// and locked in turn and the check repeats; an open that refuses it refuses the write.
///
/// A verified log at or above `cap` is renamed to `<name>.1` (replacing an older `.1`) and the
/// record goes to a fresh private log. Rotation itself never fails the write: if the rename or
/// the fresh open fails, the record is appended to the file already open.
fn append_line_capped(path: &std::path::Path, line: &str, cap: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    /// Live-path changes followed before appending to the file last locked.
    const MAX_LIVE_REOPENS: usize = 8;
    let open_locked = || {
        let file = crate::platform::open_private_append_file(path)?;
        file.lock()?;
        Ok::<_, std::io::Error>(file)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_locked()?;
    after_first_lock();
    for _ in 0..MAX_LIVE_REOPENS {
        match crate::platform::path_names_open_file(path, &file) {
            Some(false) => file = open_locked()?,
            Some(true) if file.metadata()?.len() >= cap => {
                match std::fs::rename(path, rotated_log_path(path)).and_then(|()| open_locked()) {
                    Ok(fresh) => file = fresh,
                    Err(err) => {
                        tracing::warn!(
                            err = %err,
                            path = %path.display(),
                            "log rotation failed; appending to the file already open"
                        );
                        break;
                    }
                }
            }
            // `None`: this platform does not check identity, so it never rotates (Windows).
            Some(true) | None => break,
        }
    }
    let mut record = Vec::with_capacity(line.len() + 2);
    if file.metadata()?.len() > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            record.push(b'\n');
        }
    }
    record.extend_from_slice(line.as_bytes());
    record.push(b'\n');
    // The lock is held, so the end cannot move before the write.
    file.seek(SeekFrom::End(0))?;
    file.write_all(&record)?;
    file.sync_data()?;
    publish_directories(path)
}

#[cfg(not(test))]
fn after_first_lock() {}

#[cfg(test)]
thread_local! {
    /// Runs once after the first lock, standing in for a writer that held the lock first.
    static AFTER_FIRST_LOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn after_first_lock() {
    if let Some(hook) = AFTER_FIRST_LOCK.take() {
        hook();
    }
}

fn publish_directories(path: &std::path::Path) -> std::io::Result<()> {
    for dir in path.ancestors().skip(1) {
        if dir.as_os_str().is_empty() {
            break;
        }
        sync_directory(dir)?;
    }
    Ok(())
}

#[cfg(not(test))]
fn sync_directory(dir: &std::path::Path) -> std::io::Result<()> {
    crate::platform::sync_parent_directory(dir)
}

#[cfg(test)]
thread_local! {
    /// Directory syncs that fail before any succeeds, and the syncs done, for tests.
    static FAIL_DIRECTORY_SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static DIRECTORY_SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn sync_directory(dir: &std::path::Path) -> std::io::Result<()> {
    if FAIL_DIRECTORY_SYNCS.get() > 0 {
        FAIL_DIRECTORY_SYNCS.set(FAIL_DIRECTORY_SYNCS.get() - 1);
        return Err(std::io::Error::other("injected directory sync failure"));
    }
    DIRECTORY_SYNCS.set(DIRECTORY_SYNCS.get() + 1);
    crate::platform::sync_parent_directory(dir)
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{
        ErrorResponse, Method, PaneSendKeysParams, PaneSendTextParams, Request,
    };
    use crate::api::ApiRequestContext;
    use crate::app::{App, Mode};
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    struct Fixture {
        app: App,
        source_pane_id: String,
        target_pane_id: String,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        source_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
        target_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    }

    fn audit_record() -> crate::pty::input_consumer::AuditRecord {
        crate::pty::input_consumer::AuditRecord {
            epoch: "epoch-id".into(),
            seq: 1,
            token: "submission-token".into(),
            cut: 2,
            digest: "ab".repeat(32),
            kind: crate::pty::input_consumer::CutKind::Submit,
            result: crate::pty::input_consumer::CutResult::Client { principal: None },
        }
    }

    #[tokio::test]
    async fn input_consumer_audit_is_owner_only_metadata_and_durable() {
        let fixture = fixture();
        let path = fixture
            .app
            .api_input_log
            .with_file_name("input-consumer.jsonl");
        let sink = fixture.app.input_consumer_audit_sink();
        sink(&audit_record()).expect("durable audit");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        let record: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(record["epoch"], "epoch-id");
        assert_eq!(record["seq"], 1);
        assert_eq!(record["token"], "submission-token");
        assert_eq!(record["cut"], 2);
        assert_eq!(record["kind"], "submit");
        assert_eq!(
            record["result"],
            serde_json::json!({"result":"client", "principal":null})
        );
        assert_eq!(record.as_object().unwrap().len(), 7);
        for forbidden in ["raw", "epoch_key", "nonce", "text"] {
            assert!(record.get(forbidden).is_none());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn input_consumer_audit_propagates_append_and_directory_sync_failure() {
        let fixture = fixture();
        let path = fixture
            .app
            .api_input_log
            .with_file_name("input-consumer.jsonl");
        std::fs::create_dir(&path).unwrap();
        let sink = fixture.app.input_consumer_audit_sink();
        assert!(
            sink(&audit_record()).is_err(),
            "actor must observe append failure"
        );
        std::fs::remove_dir(&path).unwrap();
        super::FAIL_DIRECTORY_SYNCS.set(1);
        assert!(
            sink(&audit_record()).is_err(),
            "actor must observe durability failure"
        );
        super::FAIL_DIRECTORY_SYNCS.set(0);
        std::fs::remove_file(path).unwrap();
    }

    /// A caller pane (this test process, an agent named "sender") and a target pane.
    fn fixture() -> Fixture {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("input-log");
        let source_pane = workspace.tabs[0].root_pane;
        let target_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        let source_terminal_id = app.state.workspaces[0]
            .terminal_id(source_pane)
            .cloned()
            .expect("source terminal");
        let source = app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state");
        source.set_agent_name("sender".into());
        source.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (source_runtime, _source_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        source_runtime.test_set_child_pid(std::process::id());
        let (target_runtime, target_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(source_pane, source_runtime);
        app.state.insert_test_runtime(target_pane, target_runtime);
        Fixture {
            source_pane_id: app.public_pane_id(0, source_pane).expect("source pane id"),
            target_pane_id: app.public_pane_id(0, target_pane).expect("target pane id"),
            app,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            source_rx: _source_rx,
            target_rx,
        }
    }

    fn send_text(fixture: &mut Fixture, text: &str) -> String {
        fixture.app.handle_api_request_with_context(
            Request {
                id: "send".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    pane_id: fixture.target_pane_id.clone(),
                    text: text.into(),
                    allow_cross_pane: true,
                }),
            },
            ApiRequestContext::for_local_peer_pid(Some(std::process::id())),
        )
    }

    fn log_lines(app: &App) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&app.api_input_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect()
    }

    #[tokio::test]
    async fn an_api_write_is_logged_with_its_caller_and_never_its_text() {
        let mut fixture = fixture();
        send_text(&mut fixture, "secret prompt\r");
        assert_eq!(
            fixture.target_rx.try_recv().expect("sent"),
            bytes::Bytes::from_static(b"secret prompt\r")
        );
        let lines = log_lines(&fixture.app);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert_eq!(line["method"], "pane.send_text");
        assert_eq!(line["target_pane"], fixture.target_pane_id.as_str());
        assert_eq!(line["bytes"], 14);
        assert_eq!(line["caller"]["pid"], std::process::id());
        assert_eq!(line["caller"]["pane"], fixture.source_pane_id.as_str());
        assert_eq!(line["caller"]["agent"], "sender");
        assert!(line["ts_ms"].as_u64().is_some());
        let raw = std::fs::read_to_string(&fixture.app.api_input_log).unwrap();
        assert!(!raw.contains("secret"), "{raw}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&fixture.app.api_input_log)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    /// herdr-8592: a granted plugin action write names plugin, action,
    /// invoking pane and the non-secret grant id, never the token.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_plugin_action_write_is_attributed_without_its_token() {
        use crate::plugin_action_origin::{PluginActionClaim, PluginActionGrant};
        let mut fixture = fixture();
        let (token, key) = fixture
            .app
            .plugin_action_grants
            .mint(PluginActionGrant {
                plugin_id: "example.explorr".into(),
                action_id: "open".into(),
                invoking_pane: Some(fixture.source_pane_id.clone()),
                grant_id: "plugin-log-9".into(),
            })
            .expect("mint");
        let (ws_idx, pane_id) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("invoking pane");
        let target = fixture
            .app
            .terminal_target_for_pane(ws_idx, pane_id)
            .expect("invoking terminal");
        assert!(fixture
            .app
            .plugin_action_grants
            .bind_target(key, &target.terminal_id));
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("action child");
        assert!(fixture
            .app
            .plugin_action_grants
            .activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "granted".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    pane_id: fixture.source_pane_id.clone(),
                    text: "open".into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext::default().with_plugin_action(PluginActionClaim::Presented(token)),
        );
        assert!(response.contains("\"ok\""), "{response}");
        assert_eq!(fixture.source_rx.try_recv().expect("input"), "open");
        assert!(fixture.target_rx.try_recv().is_err());
        let lines = log_lines(&fixture.app);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let action = &lines[0]["caller"]["plugin_action"];
        assert_eq!(action["plugin_id"], "example.explorr");
        assert_eq!(action["action_id"], "open");
        assert_eq!(action["invoking_pane"], fixture.source_pane_id.as_str());
        assert_eq!(action["grant"], "plugin-log-9");
        let raw = std::fs::read_to_string(&fixture.app.api_input_log).unwrap();
        let hex = token.expose_hex();
        assert!(!raw.contains(&hex) && !raw.contains(&hex[..16]), "{raw}");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    /// herdr-8592 timing edge: the recorded child exits after the guard admitted
    /// the write but before the durable log line. The accepted write keeps its
    /// attribution; any later request with that token is refused.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn grant_revoked_between_admission_and_log_keeps_attribution() {
        use crate::app::terminal_targets::InputOrigin;
        use crate::plugin_action_origin::{PluginActionClaim, PluginActionGrant};
        let fixture = fixture();
        let grant = PluginActionGrant {
            plugin_id: "example.explorr".into(),
            action_id: "open".into(),
            invoking_pane: Some(fixture.source_pane_id.clone()),
            grant_id: "plugin-log-10".into(),
        };
        let (token, key) = fixture
            .app
            .plugin_action_grants
            .mint(grant.clone())
            .expect("mint");
        let (ws_idx, pane_id) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("invoking pane");
        let target = fixture
            .app
            .terminal_target_for_pane(ws_idx, pane_id)
            .expect("invoking terminal");
        assert!(fixture
            .app
            .plugin_action_grants
            .bind_target(key, &target.terminal_id));
        let request = Request {
            id: "late".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                pane_id: fixture.source_pane_id.clone(),
                text: "open".into(),
                allow_cross_pane: false,
            }),
        };
        let context =
            ApiRequestContext::default().with_plugin_action(PluginActionClaim::Presented(token));
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("action child");
        assert!(fixture
            .app
            .plugin_action_grants
            .activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::PluginAction(grant),
            "live plugin attribution"
        );
        assert!(fixture
            .app
            .cross_pane_input_denial(&request, context)
            .is_none());
        // The child exits and the worker revokes before the log line.
        child.kill().expect("stop own child");
        child.wait().expect("reap");
        fixture.app.plugin_action_grants.revoke(key);
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Unknown,
            "a request after exit is refused"
        );
        let denied = fixture
            .app
            .cross_pane_input_denial(&request, context)
            .expect("a later write is refused");
        assert!(denied.contains("input_origin_unknown"), "{denied}");
        fixture
            .app
            .log_api_input("late", "pane.send_text", ws_idx, pane_id, context, 4)
            .expect("logged");
        let lines = log_lines(&fixture.app);
        let action = &lines.last().expect("line")["caller"]["plugin_action"];
        assert_eq!(action["plugin_id"], "example.explorr");
        assert_eq!(action["grant"], "plugin-log-10");
        assert_eq!(action["invoking_pane"], fixture.source_pane_id.as_str());
        let raw = std::fs::read_to_string(&fixture.app.api_input_log).unwrap();
        assert!(!raw.contains(&token.expose_hex()));
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct PresencePeer(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for PresencePeer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assert_logged_pane_env_presence(marker: Option<&str>, expected: Option<bool>) {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "read line"])
            .env_clear()
            .stdin(std::process::Stdio::piped());
        if let Some(marker) = marker {
            command.env("HERDR_PANE_ID", marker);
        }
        let mut peer = PresencePeer(command.spawn().expect("live environment peer"));
        assert!(peer.0.try_wait().expect("peer status").is_none());
        let context = ApiRequestContext::for_local_peer_pid(Some(peer.0.id()));
        let identity = context.local_peer_identity.expect("pinned live peer");
        if expected.is_none() {
            peer.0.kill().expect("stop peer");
            peer.0.wait().expect("reap peer");
            assert_ne!(
                crate::platform::process_identity(identity.pid),
                Some(identity)
            );
        }
        let fixture = fixture();
        let pane = fixture.app.state.workspaces[0].tabs[0].root_pane;
        let raw = fixture
            .app
            .api_input_log_line("pane.send_text", 0, pane, context, 1);
        let line: serde_json::Value = serde_json::from_str(&raw).expect("actual log line");
        assert_eq!(line["caller"]["pid"], identity.pid);
        assert!(!raw.contains("HERDR_PANE_ID"), "{raw}");
        if let Some(marker) = marker.filter(|value| !value.is_empty()) {
            assert!(!raw.contains(marker), "marker value leaked: {raw}");
        }
        let presence = line["caller"].get("pane_env_present");
        if let Some(expected) = expected {
            assert_eq!(presence, Some(&serde_json::json!(expected)), "{raw}");
        } else {
            assert!(presence.is_none_or(serde_json::Value::is_null), "{raw}");
            assert_eq!(line["caller"]["ancestry"], "unknown", "{raw}");
            assert!(line["caller"].get("sid").is_none(), "{raw}");
            assert!(line["caller"].get("ancestry_root_pid").is_none(), "{raw}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn diagnostic_pane_env_presence_empty_marker_is_true() {
        assert_logged_pane_env_presence(Some(""), Some(true));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn diagnostic_pane_env_presence_malformed_marker_is_true_and_private() {
        assert_logged_pane_env_presence(Some("931-private-invalid-pane-marker"), Some(true));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn diagnostic_pane_env_presence_absent_marker_is_false() {
        assert_logged_pane_env_presence(None, Some(false));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn diagnostic_pane_env_presence_stale_caller_is_unknown_and_private() {
        assert_logged_pane_env_presence(Some("931-private-stale-pane-marker"), None);
    }

    fn assert_ancestry_field(verdict: crate::platform::ServerAncestry, expected: &str) {
        let mut caller = serde_json::json!({});
        super::add_server_ancestry_fields(
            &mut caller,
            crate::platform::ServerAncestryObservation {
                verdict,
                #[cfg(target_os = "linux")]
                root_pid: Some(421),
            },
        );
        assert_eq!(caller["ancestry"], expected);
        #[cfg(target_os = "linux")]
        assert_eq!(caller["ancestry_root_pid"], 421);
        #[cfg(not(target_os = "linux"))]
        assert!(caller.get("ancestry_root_pid").is_none());
    }

    #[test]
    fn diagnostic_ancestry_outside_is_logged() {
        assert_ancestry_field(crate::platform::ServerAncestry::Outside, "outside");
    }

    #[test]
    fn diagnostic_ancestry_unknown_is_logged() {
        assert_ancestry_field(crate::platform::ServerAncestry::Unknown, "unknown");
        assert_ancestry_field(crate::platform::ServerAncestry::NotApplicable, "unknown");
    }

    #[test]
    fn diagnostic_ancestry_reached_is_logged() {
        assert_ancestry_field(crate::platform::ServerAncestry::ReachedServer, "reached");
    }

    #[tokio::test]
    async fn diagnostic_ancestry_is_unknown_without_a_peer() {
        let fixture = fixture();
        let pane = fixture.app.state.workspaces[0].tabs[0].root_pane;
        let raw = fixture.app.api_input_log_line(
            "pane.send_text",
            0,
            pane,
            ApiRequestContext::default(),
            1,
        );
        let line: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // smarty-dev#931: a null-caller line is still classified, with no invented identity.
        assert_eq!(line["caller"]["ancestry"], "unknown", "{raw}");
        assert!(line["caller"]["pid"].is_null(), "{raw}");
        for field in [
            "sid",
            "ancestry_root_pid",
            "ancestor_pid",
            "ancestor_exe",
            "pane_env_present",
            "exe",
            "ppid",
            "unit",
        ] {
            assert!(line["caller"].get(field).is_none(), "{raw}");
        }
    }

    /// A unique log path beside the fixture's, and its rotated generation.
    fn rotation_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = fixture().app.api_input_log;
        let path = base.with_extension(format!("{tag}.jsonl"));
        let rotated = super::rotated_log_path(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        (path, rotated)
    }

    /// Appends `prefix` records without rotating until the log reaches `cap` bytes.
    fn fill_to_cap(path: &std::path::Path, prefix: &str, cap: u64) {
        let mut n = 0;
        while std::fs::metadata(path).map_or(0, |meta| meta.len()) < cap {
            super::append_line_capped(path, &format!("{{\"{prefix}\":{n}}}"), u64::MAX).unwrap();
            n += 1;
        }
    }

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    // Rotation needs the Unix identity check; on Windows it is a documented no-op.
    #[cfg(unix)]
    #[tokio::test]
    async fn crossing_the_cap_rotates_once_and_keeps_the_new_record() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-once");
        fill_to_cap(&path, "old", CAP);
        let before = std::fs::read(&path).unwrap();
        super::append_line_capped(&path, "{\"new\":0}", CAP).unwrap();
        assert_eq!(
            std::fs::read(&rotated).unwrap(),
            before,
            "old generation moved intact"
        );
        assert_eq!(read_lines(&path), ["{\"new\":0}"]);
        // Below the cap again: the next record appends, and .1 is untouched.
        super::append_line_capped(&path, "{\"new\":1}", CAP).unwrap();
        assert_eq!(read_lines(&path), ["{\"new\":0}", "{\"new\":1}"]);
        assert_eq!(std::fs::read(&rotated).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [&path, &rotated] {
                let mode = std::fs::metadata(file).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{}", file.display());
            }
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_crossing_replaces_the_rotated_generation() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-twice");
        fill_to_cap(&path, "first", CAP);
        super::append_line_capped(&path, "{\"second\":-1}", CAP).unwrap();
        fill_to_cap(&path, "second", CAP);
        let second = std::fs::read(&path).unwrap();
        super::append_line_capped(&path, "{\"third\":0}", CAP).unwrap();
        assert_eq!(std::fs::read(&rotated).unwrap(), second);
        assert!(!String::from_utf8(second).unwrap().contains("first"));
        assert_eq!(read_lines(&path), ["{\"third\":0}"]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rotation_preserves_the_private_mode_of_both_generations() {
        use std::os::unix::fs::PermissionsExt;
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-mode");
        fill_to_cap(&path, "old", CAP);
        // A stale readable .1 is replaced by the private generation, not kept.
        std::fs::write(&rotated, b"stale\n").unwrap();
        std::fs::set_permissions(&rotated, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::append_line_capped(&path, "{\"new\":0}", CAP).unwrap();
        assert!(!std::fs::read_to_string(&rotated).unwrap().contains("stale"));
        for file in [&path, &rotated] {
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    #[tokio::test]
    async fn a_failed_rotation_still_appends_the_record() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-blocked");
        // A non-empty directory at `.1` makes the rename fail on every platform.
        std::fs::create_dir_all(rotated.join("occupied")).unwrap();
        fill_to_cap(&path, "old", CAP);
        let mut expected = read_lines(&path);
        super::append_line_capped(&path, "{\"new\":0}", CAP)
            .expect("rotation alone never fails closed");
        expected.push("{\"new\":0}".into());
        assert_eq!(read_lines(&path), expected);
        assert!(rotated.join("occupied").is_dir());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&rotated);
    }

    /// herdr#225 review: two writers race across one rotation, many times over.
    #[cfg(unix)]
    #[tokio::test]
    async fn racing_writers_rotate_once_and_lose_no_record() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-race");
        for round in 0..40 {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&rotated);
            fill_to_cap(&path, "old", CAP);
            let previous = std::fs::read(&path).unwrap();
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let writers: Vec<_> = ["a", "b"]
                .into_iter()
                .map(|writer| {
                    let (path, start) = (path.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        let line = format!("{{\"{writer}\":{round}}}");
                        super::append_line_capped(&path, &line, CAP).unwrap();
                        line
                    })
                })
                .collect();
            let mut expected: Vec<String> =
                writers.into_iter().map(|w| w.join().unwrap()).collect();
            expected.sort();
            let mut live = read_lines(&path);
            live.sort();
            assert_eq!(live, expected, "round {round}");
            assert_eq!(
                std::fs::read(&rotated).unwrap(),
                previous,
                "round {round}: .1 is exactly the previous generation"
            );
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    /// A writer that waited while another rotated holds `.1`: it appends to the live log
    /// instead, and `.1` keeps exactly the previous generation.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stale_waiter_appends_to_the_live_log_without_rotating_again() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-stale");
        fill_to_cap(&path, "old", CAP);
        let previous = std::fs::read(&path).unwrap();
        let (hook_path, hook_rotated) = (path.clone(), rotated.clone());
        super::AFTER_FIRST_LOCK.set(Some(Box::new(move || {
            std::fs::rename(&hook_path, &hook_rotated).unwrap();
            super::append_line_capped(&hook_path, "{\"other\":0}", CAP).unwrap();
        })));
        super::append_line_capped(&path, "{\"mine\":0}", CAP).unwrap();
        assert_eq!(std::fs::read(&rotated).unwrap(), previous);
        assert_eq!(read_lines(&path), ["{\"other\":0}", "{\"mine\":0}"]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    /// herdr#225 review: an entry at the live path with the held file's length but another
    /// identity (here a symlink, as a foreign file would be) is never moved into `.1`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_same_size_foreign_replacement_is_refused_not_rotated() {
        const CAP: u64 = 64;
        let (path, rotated) = rotation_paths("rotate-foreign");
        fill_to_cap(&path, "old", CAP);
        let previous = std::fs::read(&path).unwrap();
        // A symlink's own length is its target's: give it exactly the held file's length.
        let mut target = String::from("rotate-foreign-decoy-");
        while target.len() < previous.len() {
            target.push('x');
        }
        assert_eq!(target.len(), previous.len());
        let decoy = path.with_file_name(&target);
        std::fs::write(&decoy, b"decoy\n").unwrap();
        let (hook_path, hook_rotated, hook_target) =
            (path.clone(), rotated.clone(), target.clone());
        super::AFTER_FIRST_LOCK.set(Some(Box::new(move || {
            std::fs::rename(&hook_path, &hook_rotated).unwrap();
            std::os::unix::fs::symlink(&hook_target, &hook_path).unwrap();
        })));
        assert!(super::append_line_capped(&path, "{\"mine\":0}", CAP).is_err());
        assert_eq!(
            std::fs::symlink_metadata(&path).unwrap().len(),
            previous.len() as u64
        );
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&rotated).unwrap(), previous, ".1 kept");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"decoy\n");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::remove_file(&decoy);
    }

    #[tokio::test]
    async fn diagnostic_ancestry_reuses_the_guard_observation_without_another_walk() {
        let fixture = fixture();
        let mut context = ApiRequestContext {
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..ApiRequestContext::for_local_peer_pid(Some(std::process::id()))
        };
        let peer = context.local_peer_identity.expect("live peer");
        let request = Request {
            id: "cached-ancestry".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                pane_id: fixture.target_pane_id.clone(),
                text: "secret-not-logged".into(),
                allow_cross_pane: false,
            }),
        };
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            assert!(fixture
                .app
                .cross_pane_input_denial_with_ancestry(&request, &mut context)
                .is_none());
        });
        let saved = context.server_ancestry.expect("guard retained observation");
        assert_eq!(saved.verdict, crate::platform::ServerAncestry::Outside);
        let pane = fixture.app.state.workspaces[0].tabs[0].root_pane;
        // A second observation would now return unknown. The logged outside
        // result proves the fallback is lazy, not a duplicate guard walk.
        crate::platform::with_server_ancestry_for_test(peer, None, || {
            let raw = fixture
                .app
                .api_input_log_line("pane.send_text", 0, pane, context, 1);
            let line: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(line["caller"]["ancestry"], "outside");
            assert!(!raw.contains("secret-not-logged"));
            // The original accept context is not modified: the next request
            // observes its own proof, rather than retaining a prior request's.
            let fresh = ApiRequestContext::for_local_peer_pid(Some(peer.pid));
            assert!(fresh.server_ancestry.is_none());
            let raw = fixture
                .app
                .api_input_log_line("pane.send_text", 0, pane, fresh, 1);
            let line: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(line["caller"]["ancestry"], "unknown");
        });
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn diagnostic_linux_session_and_walk_root_are_logged_before_exit() {
        let fixture = fixture();
        let pane = fixture.app.state.workspaces[0].tabs[0].root_pane;
        let context = ApiRequestContext::for_local_peer_pid(Some(std::process::id()));
        let raw = fixture
            .app
            .api_input_log_line("pane.send_text", 0, pane, context, 1);
        let line: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let metadata =
            crate::platform::process_caller_metadata(context.local_peer_identity.unwrap()).unwrap();
        assert_eq!(line["caller"]["sid"], metadata.sid.unwrap());
        assert_eq!(line["caller"]["ancestry"], "reached");
        assert_eq!(line["caller"]["ancestry_root_pid"], std::process::id());
    }

    fn diagnostic_identity(pid: u32) -> crate::platform::ProcessIdentity {
        crate::platform::ProcessIdentity { pid, start_time: 1 }
    }

    fn diagnostic_metadata(exe: Option<&str>) -> crate::platform::CallerMetadata {
        crate::platform::CallerMetadata {
            exe: exe.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostic_ancestor_skips_herdr_wrappers_and_never_inspects_the_caller_exe() {
        let result = super::nearest_non_herdr_ancestor(
            diagnostic_identity(6),
            Some("renamed-herdr"),
            |pid| Some(diagnostic_identity(pid)),
            |child| Some(diagnostic_identity(child.pid - 1)),
            |parent| {
                Some(diagnostic_metadata(Some(match parent.pid {
                    5 => "herdr",
                    4 => "herdr-dev",
                    3 => "herdr.exe",
                    2 => "renamed-herdr",
                    1 => "node",
                    _ => panic!("must inspect only ancestors"),
                })))
            },
        );
        assert_eq!(result, Some(("node".into(), 1)));
    }

    #[test]
    fn diagnostic_ancestor_is_bounded_and_rejects_cycles() {
        let reads = std::cell::Cell::new(0);
        assert!(super::nearest_non_herdr_ancestor(
            diagnostic_identity(100),
            Some("herdr"),
            |pid| Some(diagnostic_identity(pid)),
            |child| {
                reads.set(reads.get() + 1);
                Some(diagnostic_identity(child.pid - 1))
            },
            |_| Some(diagnostic_metadata(Some("herdr"))),
        )
        .is_none());
        assert_eq!(reads.get(), 32);
        assert!(super::nearest_non_herdr_ancestor(
            diagnostic_identity(2),
            Some("herdr"),
            |pid| Some(diagnostic_identity(pid)),
            |child| Some(diagnostic_identity(if child.pid == 2 { 1 } else { 2 })),
            |_| Some(diagnostic_metadata(Some("herdr"))),
        )
        .is_none());
    }

    #[test]
    fn diagnostic_ancestor_omits_unknown_exe_parent_and_stale_original() {
        for metadata in [None, Some(diagnostic_metadata(None))] {
            assert!(super::nearest_non_herdr_ancestor(
                diagnostic_identity(2),
                Some("herdr"),
                |pid| Some(diagnostic_identity(pid)),
                |_| Some(diagnostic_identity(1)),
                |_| metadata
                    .as_ref()
                    .map(|value| diagnostic_metadata(value.exe.as_deref())),
            )
            .is_none());
        }
        assert!(super::nearest_non_herdr_ancestor(
            diagnostic_identity(2),
            Some("herdr"),
            |pid| Some(diagnostic_identity(pid)),
            |_| None,
            |_| panic!("no parent to inspect"),
        )
        .is_none());
        assert!(super::nearest_non_herdr_ancestor(
            diagnostic_identity(2),
            Some("herdr"),
            |_| None,
            |_| panic!("stale caller must stop before parent query"),
            |_| panic!("stale caller must stop before metadata query"),
        )
        .is_none());
    }

    #[test]
    fn diagnostic_ancestor_revalidates_original_child_and_parent_after_metadata() {
        for replaced_pid in [3, 2, 1] {
            let changed = std::cell::Cell::new(false);
            assert!(
                super::nearest_non_herdr_ancestor(
                    diagnostic_identity(3),
                    Some("herdr"),
                    |pid| {
                        let mut identity = diagnostic_identity(pid);
                        if changed.get() && pid == replaced_pid {
                            identity.start_time += 1;
                        }
                        Some(identity)
                    },
                    |child| Some(diagnostic_identity(child.pid - 1)),
                    |parent| {
                        if parent.pid == 1 {
                            changed.set(true);
                            Some(diagnostic_metadata(Some("node")))
                        } else {
                            Some(diagnostic_metadata(Some("herdr")))
                        }
                    },
                )
                .is_none(),
                "reused pid {replaced_pid}"
            );
        }
    }

    #[test]
    fn diagnostic_ancestor_revalidates_original_after_parent_query() {
        let exited = std::cell::Cell::new(false);
        assert!(super::nearest_non_herdr_ancestor(
            diagnostic_identity(2),
            Some("herdr"),
            |pid| (!exited.get()).then_some(diagnostic_identity(pid)),
            |_| {
                exited.set(true);
                Some(diagnostic_identity(1))
            },
            |_| panic!("exited original must stop before metadata"),
        )
        .is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnostic_ancestor_is_the_live_parent_not_the_caller() {
        let peer = crate::platform::process_identity(std::process::id()).expect("live caller");
        let Some(parent) = crate::platform::parent_process_identity(peer) else {
            return; // Unsupported platforms cannot observe ancestry.
        };
        let metadata = crate::platform::process_caller_metadata(parent).expect("parent metadata");
        let expected_exe = metadata.exe.expect("parent executable basename");
        let caller = crate::platform::process_caller_metadata(peer).expect("caller metadata");
        let mut fixture = fixture();
        send_text(&mut fixture, "x");
        let lines = log_lines(&fixture.app);
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
        assert_eq!(lines[0]["caller"]["ancestor_pid"], parent.pid);
        assert_eq!(lines[0]["caller"]["ancestor_exe"], expected_exe);
        assert_ne!(lines[0]["caller"]["ancestor_pid"], peer.pid);
        if let Some(exe) = caller.exe {
            assert_eq!(lines[0]["caller"]["exe"], exe);
        }
        if let Some(ppid) = caller.ppid {
            assert_eq!(lines[0]["caller"]["ppid"], ppid);
        }
        if let Some(unit) = caller.unit {
            assert_eq!(lines[0]["caller"]["unit"], unit);
        }
    }

    #[tokio::test]
    async fn an_unattributed_caller_is_logged_without_a_caller_pane() {
        let mut fixture = fixture();
        fixture.app.handle_api_request_with_context(
            Request {
                id: "keys".into(),
                method: Method::PaneSendKeys(PaneSendKeysParams {
                    pane_id: fixture.target_pane_id.clone(),
                    keys: vec!["enter".into()],
                    allow_cross_pane: true,
                }),
            },
            ApiRequestContext::default(),
        );
        let lines = log_lines(&fixture.app);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["method"], "pane.send_keys");
        assert!(lines[0]["caller"]["pid"].is_null());
        assert!(lines[0]["caller"]["pane"].is_null());
        assert!(lines[0]["caller"].get("ancestor_pid").is_none());
        assert!(lines[0]["caller"].get("ancestor_exe").is_none());
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[tokio::test]
    async fn a_partial_record_does_not_swallow_the_next_one() {
        // Astra review of herdr#84: a short write left a record without its newline.
        let mut fixture = fixture();
        std::fs::write(&fixture.app.api_input_log, b"{\"ts_ms\":1,\"meth").unwrap();
        send_text(&mut fixture, "x");
        let raw = std::fs::read_to_string(&fixture.app.api_input_log).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "{raw:?}");
        assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_err());
        let record: serde_json::Value = serde_json::from_str(lines[1]).expect("record");
        assert_eq!(record["method"], "pane.send_text");
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[tokio::test]
    async fn the_first_record_creates_the_log_directory() {
        let mut fixture = fixture();
        let dir = fixture.app.api_input_log.with_extension("dir");
        fixture.app.api_input_log = dir.join("nested").join("api-input.jsonl");
        send_text(&mut fixture, "x");
        assert_eq!(log_lines(&fixture.app).len(), 1);
        assert!(fixture.target_rx.try_recv().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_directory_sync_is_completed_before_the_next_write() {
        // Astra review of herdr#84: the path exists after a failed first attempt, but its
        // directory entry may not be durable.
        let mut fixture = fixture();
        super::FAIL_DIRECTORY_SYNCS.set(1);
        let response: ErrorResponse =
            serde_json::from_str(&send_text(&mut fixture, "first")).expect("error");
        assert_eq!(response.error.code, "input_log_unavailable");
        assert!(fixture.target_rx.try_recv().is_err());
        super::DIRECTORY_SYNCS.set(0);
        send_text(&mut fixture, "second");
        assert!(
            super::DIRECTORY_SYNCS.get() > 0,
            "the retry did not sync the directory"
        );
        assert!(fixture.target_rx.try_recv().is_ok());
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[tokio::test]
    async fn a_recreated_log_is_published_again() {
        // Astra review of herdr#84: after a successful append, the log is moved aside; the
        // replacement's directory entry must be synced before input is sent.
        let mut fixture = fixture();
        send_text(&mut fixture, "first");
        assert!(fixture.target_rx.try_recv().is_ok());
        let aside = fixture.app.api_input_log.with_extension("aside");
        std::fs::rename(&fixture.app.api_input_log, &aside).unwrap();
        super::FAIL_DIRECTORY_SYNCS.set(1);
        let response: ErrorResponse =
            serde_json::from_str(&send_text(&mut fixture, "second")).expect("error");
        assert_eq!(response.error.code, "input_log_unavailable");
        assert!(fixture.target_rx.try_recv().is_err());
        let _ = std::fs::remove_file(&aside);
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_existing_readable_log_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = fixture();
        std::fs::write(&fixture.app.api_input_log, b"").unwrap();
        std::fs::set_permissions(
            &fixture.app.api_input_log,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        send_text(&mut fixture, "x");
        let mode = std::fs::metadata(&fixture.app.api_input_log)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(log_lines(&fixture.app).len(), 1);
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_log_is_refused() {
        let mut fixture = fixture();
        let target = fixture.app.api_input_log.with_extension("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, &fixture.app.api_input_log).unwrap();
        let response: ErrorResponse =
            serde_json::from_str(&send_text(&mut fixture, "x")).expect("error");
        assert_eq!(response.error.code, "input_log_unavailable");
        assert!(fixture.target_rx.try_recv().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"");
        let _ = std::fs::remove_file(&fixture.app.api_input_log);
        let _ = std::fs::remove_file(&target);
    }

    #[tokio::test]
    async fn a_write_that_cannot_be_logged_is_refused() {
        let mut fixture = fixture();
        // A path below a regular file cannot be created.
        let blocker = fixture.app.api_input_log.with_extension("blocker");
        std::fs::write(&blocker, b"").unwrap();
        fixture.app.api_input_log = blocker.join("api-input.jsonl");
        let response = send_text(&mut fixture, "x");
        let response: ErrorResponse = serde_json::from_str(&response).expect("error");
        assert_eq!(response.error.code, "input_log_unavailable");
        assert!(
            fixture.target_rx.try_recv().is_err(),
            "unlogged input was sent"
        );
        let _ = std::fs::remove_file(&blocker);
    }
}
