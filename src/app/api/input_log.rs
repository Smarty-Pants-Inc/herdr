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
            .local_peer_pid
            .and_then(|pid| self.pane_target_for_peer_pid(pid));
        let caller_pane = caller
            .as_ref()
            .and_then(|target| self.public_pane_id(target.ws_idx, target.pane_id));
        let caller_agent = caller
            .as_ref()
            .and_then(|target| self.agent_info(target.ws_idx, target.pane_id));
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
            "caller": {
                "pid": context.local_peer_pid,
                "pane": caller_pane,
                "agent": caller_agent.as_ref().and_then(|agent| agent.name.clone()),
                "session": caller_agent
                    .as_ref()
                    .and_then(|agent| agent.agent_session.as_ref())
                    .map(|session| session.value.clone()),
            },
        })
        .to_string()
    }
}

/// Appends `line` as one JSONL record and makes it durable before returning.
///
/// An exclusive file lock serializes writers, including other Herdr processes that share the
/// state directory. If an earlier write left a partial record (a short write before an error),
/// the record starts on a new line, so every accepted record stays separately parseable; the
/// fragment is a malformed line that readers skip. When this call creates the log file or its
/// directories, their directory entries are synced too.
fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut created_dirs = Vec::new();
    if let Some(parent) = path.parent() {
        let mut dir = Some(parent);
        while let Some(current) = dir.filter(|current| !current.as_os_str().is_empty()) {
            if current.exists() {
                break;
            }
            created_dirs.push(current.to_path_buf());
            dir = current.parent();
        }
        std::fs::create_dir_all(parent)?;
    }
    let created_file = !path.exists();
    let mut file = crate::platform::open_private_append_file(path)?;
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
    file.write_all(&record)?;
    file.sync_data()?;
    if created_file {
        if let Some(parent) = path.parent() {
            crate::platform::sync_parent_directory(parent)?;
        }
    }
    // Deepest first: each created directory's entry lives in its parent.
    for dir in &created_dirs {
        if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            crate::platform::sync_parent_directory(parent)?;
        }
    }
    Ok(())
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
        target_rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
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
            ApiRequestContext {
                local_peer_pid: Some(std::process::id()),
            },
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
