//! Herdr's API input log (smarty-dev#931).
//!
//! Every API call that writes to a pane's input appends one JSON line to
//! `<state dir>/api-input.jsonl` before the write: the caller (from socket peer attribution,
//! never from request text), the target pane, the time and the byte count. It never records the
//! text. A reader of a pane's turns can then tell sent input from typed input without anything
//! in the terminal byte stream. The log fails closed: if the line cannot be written, the API
//! write is refused.
//!
//! Retention is bounded: a log that has reached `MAX_LOG_BYTES` is renamed to `<log>.1`
//! (replacing the previous generation) before the next record is appended. If that rename
//! fails, the record is refused (`input_log_full`) rather than growing the log past the cap.

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
        self.record_api_input(method, ws_idx, pane_id, context, bytes)
            .map_err(|(code, message)| {
                super::responses::encode_error(id.to_string(), code, message)
            })
    }

    /// Appends the log line for an API write; on error returns the refusal code
    /// (`input_log_full` when the log is at its cap and cannot be rotated, otherwise
    /// `input_log_unavailable`) and message. The caller must not write on error.
    pub(super) fn record_api_input(
        &self,
        method: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        context: ApiRequestContext,
        bytes: usize,
    ) -> Result<(), (&'static str, String)> {
        let line = self.api_input_log_line(method, ws_idx, pane_id, context, bytes);
        append_line(&self.api_input_log, &line).map_err(|err| {
            tracing::warn!(err = %err, path = %self.api_input_log.display(), "api input log write failed");
            (
                refusal_code(&err),
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

/// The size at which a log is rotated to `<log>.1` before the next append.
/// ponytail: 32 MiB per generation, two generations kept, so each log stays near 64 MiB while
/// holding far more API writes than a review of recent turns needs.
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

/// A log at or over its cap whose rotation to `<log>.1` failed. Appending would grow the log
/// past the cap, so the record is refused instead.
#[derive(Debug)]
struct InputLogFull {
    path: std::path::PathBuf,
    rotation: std::io::Error,
}

impl std::fmt::Display for InputLogFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the input log {} is full and could not be rotated: {}",
            self.path.display(),
            self.rotation
        )
    }
}

impl std::error::Error for InputLogFull {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.rotation)
    }
}

/// The API error code for a failed append.
fn refusal_code(err: &std::io::Error) -> &'static str {
    if err
        .get_ref()
        .is_some_and(|inner| inner.is::<InputLogFull>())
    {
        "input_log_full"
    } else {
        "input_log_unavailable"
    }
}

/// `path` with `suffix` appended to its file name (`api-input.jsonl` -> `api-input.jsonl.1`).
fn sibling_path(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Appends `line` as one JSONL record and makes it durable before returning.
fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    append_line_with_cap(path, line, MAX_LOG_BYTES)
}

/// Writers serialize on `<log>.lock`, a separate private file that is never renamed or
/// deleted, so every writer (including other Herdr processes sharing the state directory)
/// locks the same object. Under that lock a full log is rotated, then the log path is opened
/// fresh, the record appended and the handle closed. No writer keeps a log handle across
/// appends, so none can write to a generation that was already rotated away.
///
/// An older Herdr writer (the pre-upgrade binary during a live handoff) does not know
/// `<log>.lock`; it locks the log file itself. Rotation therefore also takes that lock on the
/// current log before renaming it, so such a writer in the middle of an append finishes first.
/// On Unix, a legacy writer that was already waiting for the old inode's lock may still append
/// its record to `<log>.1` after the rename; the record is kept there until the next rotation.
/// That is an accepted limit of the upgrade transition only (see
/// `platform::rotate_private_log`).
///
/// The cap is never bypassed: if the log is at or over the cap and the rotation fails, the
/// record is refused with an `InputLogFull` error and the log does not grow, so the caller's
/// input is refused like any other unloggable write. A symlink or foreign file at the log path
/// is neither rotated nor written (the private open refuses it), as before.
fn append_line_with_cap(path: &std::path::Path, line: &str, cap: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = crate::platform::open_private_append_file(&sibling_path(path, ".lock"))?;
    lock.lock()?;
    let written = rotate_full_log(path, cap)
        .and_then(|()| write_record(path, line))
        .and_then(|()| publish_directories(path));
    // Closing the handle also releases the lock; unlock explicitly so Windows releases it now.
    let _ = lock.unlock();
    written
}

/// Renames the log to `<log>.1` when it is a regular file we own of at least `cap` bytes.
/// The caller holds the writer lock. Before the rename the log is opened through the private
/// open (restricting it to owner-only) and locked against legacy writers that lock the log
/// itself. A failure returns an `InputLogFull` error, so the caller refuses the record.
fn rotate_full_log(path: &std::path::Path, cap: u64) -> std::io::Result<()> {
    // Cheap no-follow check first, so a log below the cap is not opened an extra time.
    if crate::platform::private_log_len(path).is_none_or(|len| len < cap) {
        return Ok(());
    }
    let rotated = sibling_path(path, ".1");
    crate::platform::rotate_private_log(path, cap, || rotate_file(path, &rotated))
        .map(|_| ())
        .map_err(|rotation| {
            tracing::warn!(err = %rotation, path = %path.display(), "input log rotation failed; refusing the record");
            std::io::Error::other(InputLogFull {
                path: path.to_path_buf(),
                rotation,
            })
        })
}

#[cfg(not(test))]
fn rotate_file(path: &std::path::Path, rotated: &std::path::Path) -> std::io::Result<()> {
    crate::platform::replace_file(path, rotated)
}

#[cfg(test)]
fn rotate_file(path: &std::path::Path, rotated: &std::path::Path) -> std::io::Result<()> {
    ROTATIONS.set(ROTATIONS.get() + 1);
    crate::platform::replace_file(path, rotated)
}

/// Opens the log fresh through the private open, appends one record, syncs it and closes it.
///
/// The writer lock is held, so the end cannot move before the write. If an earlier write left
/// a partial record (a short write before an error), the record starts on a new line, so every
/// accepted record stays separately parseable; the fragment is a malformed line that readers
/// skip.
///
/// The caller then syncs the directory entries of the log and of every directory above it: a
/// path that exists is not proof that it was made durable (an earlier attempt or another
/// process may have failed after creating it, or the log may have been rotated and
/// recreated). ponytail: a few directory syncs per API write; API writes are infrequent.
fn write_record(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = crate::platform::open_private_append_file(path)?;
    // Also excludes a writer from an older Herdr that locks the log itself.
    file.lock()?;
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
    file.seek(SeekFrom::End(0))?;
    file.write_all(&record)?;
    file.sync_data()
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
    /// Rotation renames attempted on this thread, for tests.
    static ROTATIONS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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

    /// herdr#225 review: a full log whose rotation fails refuses the API write as
    /// `input_log_full` and does not grow.
    #[tokio::test]
    async fn a_write_to_a_full_log_that_cannot_rotate_is_refused() {
        let mut fixture = fixture();
        let log = fixture.app.api_input_log.clone();
        let file = std::fs::File::create(&log).unwrap();
        file.set_len(super::MAX_LOG_BYTES).unwrap();
        drop(file);
        std::fs::create_dir(rotated(&log)).unwrap();
        std::fs::write(rotated(&log).join("keep"), b"").unwrap();
        let response = send_text(&mut fixture, "x");
        let response: ErrorResponse = serde_json::from_str(&response).expect("error");
        assert_eq!(response.error.code, "input_log_full");
        assert!(
            response.error.message.contains(&log.display().to_string()),
            "{}",
            response.error.message
        );
        assert!(
            fixture.target_rx.try_recv().is_err(),
            "unlogged input was sent"
        );
        assert_eq!(std::fs::metadata(&log).unwrap().len(), super::MAX_LOG_BYTES);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    /// A fresh, empty directory for one rotation test; the log inside it is absent.
    fn rotation_log(name: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-input-log-rotation-{name}-{}-{stamp}-{next}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir.join("api-input.jsonl")
    }

    fn rotated(log: &std::path::Path) -> std::path::PathBuf {
        super::sibling_path(log, ".1")
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Every name in the log's directory, sorted.
    fn names(log: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(log.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// 40-byte records (with the newline) against a 64-byte cap: the third append crosses it.
    fn record(n: u32) -> String {
        format!("{{\"n\":{},\"pad\":\"{}\"}}", n + 100, "x".repeat(21))
    }

    const CAP: u64 = 64;

    #[cfg(unix)]
    fn assert_private(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(metadata.file_type().is_file(), "{}", path.display());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o600,
            "{}",
            path.display()
        );
    }

    #[test]
    fn crossing_the_cap_rotates_once_and_keeps_the_new_record() {
        let log = rotation_log("once");
        assert_eq!(record(1).len() + 1, 40);
        super::ROTATIONS.set(0);
        super::append_line_with_cap(&log, &record(1), CAP).unwrap();
        super::append_line_with_cap(&log, &record(2), CAP).unwrap();
        assert_eq!(super::ROTATIONS.get(), 0, "below the cap");
        assert!(!rotated(&log).exists());
        #[cfg(unix)]
        let lock_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(super::sibling_path(&log, ".lock"))
                .unwrap()
                .ino()
        };
        super::append_line_with_cap(&log, &record(3), CAP).unwrap();
        assert_eq!(super::ROTATIONS.get(), 1);
        assert_eq!(
            read(&rotated(&log)),
            format!("{}\n{}\n", record(1), record(2))
        );
        assert_eq!(read(&log), format!("{}\n", record(3)));
        assert_eq!(
            names(&log),
            [
                "api-input.jsonl",
                "api-input.jsonl.1",
                "api-input.jsonl.lock"
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let lock = super::sibling_path(&log, ".lock");
            assert_eq!(
                std::fs::metadata(&lock).unwrap().ino(),
                lock_inode,
                "lock file moved"
            );
            for path in [&log, &rotated(&log), &lock] {
                assert_private(path);
            }
        }
        // The fresh generation is below the cap again: no further rotation.
        super::append_line_with_cap(&log, &record(4), CAP).unwrap();
        assert_eq!(super::ROTATIONS.get(), 1);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_second_crossing_replaces_the_rotated_generation() {
        let log = rotation_log("twice");
        for n in 1..=5 {
            super::append_line_with_cap(&log, &record(n), CAP).unwrap();
        }
        // 1+2 rotated at 3; 3+4 rotated at 5, replacing them.
        assert_eq!(
            read(&rotated(&log)),
            format!("{}\n{}\n", record(3), record(4))
        );
        assert_eq!(read(&log), format!("{}\n", record(5)));
        assert!(!super::sibling_path(&log, ".2").exists());
        assert!(!super::sibling_path(&log, ".1.1").exists());
        #[cfg(unix)]
        for path in [&log, &rotated(&log), &super::sibling_path(&log, ".lock")] {
            assert_private(path);
        }
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_writers_lose_no_record_across_a_rotation() {
        const THREADS: u32 = 2;
        const PER_THREAD: u32 = 100;
        // 19-byte records: 3800 bytes in total against a 2500-byte cap, so exactly one rotation.
        const THREAD_CAP: u64 = 2500;
        let line = |t: u32, seq: u32| format!("{{\"t\":{t},\"seq\":{}}}", seq + 1000);
        assert_eq!(line(0, 0).len() + 1, 19);
        let log = rotation_log("threads");
        let barrier = std::sync::Barrier::new(THREADS as usize);
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let (log, barrier) = (&log, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for seq in 0..PER_THREAD {
                        super::append_line_with_cap(log, &line(t, seq), THREAD_CAP).unwrap();
                    }
                });
            }
        });
        let parse = |raw: String| -> Vec<(u32, u32)> {
            raw.lines()
                .map(|raw| {
                    let value: serde_json::Value = serde_json::from_str(raw).expect(raw);
                    (
                        value["t"].as_u64().unwrap() as u32,
                        value["seq"].as_u64().unwrap() as u32 - 1000,
                    )
                })
                .collect()
        };
        let old_raw = read(&rotated(&log));
        let old = parse(old_raw.clone());
        let new = parse(read(&log));
        // Nothing lost or duplicated.
        let mut all: Vec<(u32, u32)> = old.iter().chain(&new).copied().collect();
        all.sort();
        let expected: Vec<(u32, u32)> = (0..THREADS)
            .flat_map(|t| (0..PER_THREAD).map(move |seq| (t, seq)))
            .collect();
        assert_eq!(all, expected);
        // `.1` is exactly the previous generation: it crossed the cap with its last record,
        // and for each writer every rotated record precedes every current one.
        let old_len = old_raw.len() as u64;
        assert!(
            old_len >= THREAD_CAP && old_len - 19 < THREAD_CAP,
            "{old_len}"
        );
        assert!(!new.is_empty());
        for t in 0..THREADS {
            let last_old = old.iter().filter(|r| r.0 == t).map(|r| r.1).max();
            let first_new = new.iter().filter(|r| r.0 == t).map(|r| r.1).min();
            if let (Some(last_old), Some(first_new)) = (last_old, first_new) {
                assert_eq!(last_old + 1, first_new, "writer {t}");
            }
        }
        assert_eq!(
            names(&log),
            [
                "api-input.jsonl",
                "api-input.jsonl.1",
                "api-input.jsonl.lock"
            ]
        );
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_log_path_is_refused_not_rotated() {
        let log = rotation_log("symlink");
        let target = log.with_extension("elsewhere");
        let full = "x".repeat(CAP as usize * 2);
        std::fs::write(&target, &full).unwrap();
        std::os::unix::fs::symlink(&target, &log).unwrap();
        super::ROTATIONS.set(0);
        assert!(super::append_line_with_cap(&log, &record(1), CAP).is_err());
        assert_eq!(super::ROTATIONS.get(), 0);
        assert!(!rotated(&log).exists());
        assert!(std::fs::symlink_metadata(&log)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&target), full);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_rotation_at_the_cap_refuses_the_record() {
        let log = rotation_log("rename-fails");
        // A non-empty directory at `.1` cannot be replaced by a file.
        std::fs::create_dir(rotated(&log)).unwrap();
        std::fs::write(rotated(&log).join("keep"), b"").unwrap();
        super::ROTATIONS.set(0);
        // Below the cap no rotation is attempted, so the blocked `.1` does not matter.
        super::append_line_with_cap(&log, &record(1), CAP).unwrap();
        super::append_line_with_cap(&log, &record(2), CAP).unwrap();
        let full = format!("{}\n{}\n", record(1), record(2));
        assert_eq!(read(&log), full);
        assert_eq!(super::ROTATIONS.get(), 0, "below the cap");
        // At the cap the rotation fails: the record is refused and the log does not grow.
        for attempt in 1..=2 {
            let err = super::append_line_with_cap(&log, &record(3), CAP).unwrap_err();
            assert_eq!(super::refusal_code(&err), "input_log_full");
            let message = err.to_string();
            assert!(message.contains(&log.display().to_string()), "{message}");
            assert!(message.contains("could not be rotated"), "{message}");
            assert_eq!(super::ROTATIONS.get(), attempt, "rotation was attempted");
            assert_eq!(read(&log), full);
        }
        assert!(rotated(&log).is_dir());
        #[cfg(unix)]
        assert_private(&log);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    /// herdr#225 review: a group- and world-readable log already at the cap is restricted by
    /// the private open before it is renamed, so the rotated generation is owner-only.
    #[cfg(unix)]
    #[test]
    fn a_readable_log_at_the_cap_rotates_to_a_private_generation() {
        use std::os::unix::fs::PermissionsExt;
        let log = rotation_log("readable");
        let full = format!("{}\n{}\n", record(1), record(2));
        std::fs::write(&log, &full).unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o644
        );
        super::ROTATIONS.set(0);
        super::append_line_with_cap(&log, &record(3), CAP).unwrap();
        assert_eq!(super::ROTATIONS.get(), 1);
        assert_eq!(read(&rotated(&log)), full);
        assert_eq!(read(&log), format!("{}\n", record(3)));
        assert_private(&rotated(&log));
        assert_private(&log);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    /// herdr#225 review: an older Herdr writer locks the log file itself, not `<log>.lock`.
    /// Rotation waits for that lock, so the legacy writer's append completes in the full
    /// generation before it is renamed, and the new record goes to the fresh log.
    #[test]
    fn a_legacy_lock_on_the_log_delays_rotation_until_released() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        let log = rotation_log("legacy-lock");
        let full = format!("{}\n{}\n", record(1), record(2));
        std::fs::write(&log, &full).unwrap();
        let legacy = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&log)
            .unwrap();
        legacy.lock().unwrap();
        let done = AtomicBool::new(false);
        let (finished_early, rotated_early) = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let result = super::append_line_with_cap(&log, &record(3), CAP);
                done.store(true, Ordering::SeqCst);
                result
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            let observed = (done.load(Ordering::SeqCst), rotated(&log).exists());
            // The legacy writer finishes its append on the full log, then releases its lock.
            (&legacy)
                .write_all(format!("{}\n", record(9)).as_bytes())
                .unwrap();
            legacy.unlock().unwrap();
            drop(legacy);
            writer.join().unwrap().unwrap();
            observed
        });
        assert!(
            !finished_early,
            "the append did not wait for the legacy lock"
        );
        assert!(!rotated_early, "the log was rotated under the legacy lock");
        assert_eq!(read(&rotated(&log)), format!("{full}{}\n", record(9)));
        assert_eq!(read(&log), format!("{}\n", record(3)));
        #[cfg(unix)]
        assert_private(&rotated(&log));
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    /// The production cap through the API path: reaching 32 MiB rotates before the next write.
    #[tokio::test]
    async fn the_api_log_rotates_at_its_cap() {
        let mut fixture = fixture();
        let log = fixture.app.api_input_log.clone();
        let file = std::fs::File::create(&log).unwrap();
        file.set_len(super::MAX_LOG_BYTES - 1).unwrap();
        drop(file);
        send_text(&mut fixture, "first");
        assert!(fixture.target_rx.try_recv().is_ok());
        assert!(!rotated(&log).exists(), "below the cap");
        send_text(&mut fixture, "second");
        assert!(fixture.target_rx.try_recv().is_ok());
        assert!(std::fs::metadata(rotated(&log)).unwrap().len() >= super::MAX_LOG_BYTES);
        let lines = log_lines(&fixture.app);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["bytes"], 6);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }
}
