use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

// Codex's Windows input reader does not surface bracketed paste. It detects the prompt as a
// "paste burst" and, while that burst is buffered, rewrites a following Enter into a newline
// instead of submitting. The burst only flushes after an idle timeout, so any size-based delay is
// a timing guess that fails when ConPTY delivery lags it. Codex flushes a buffered burst
// synchronously when it receives a non-character key, so appending one after the paste gives the
// submission a deterministic paste boundary regardless of prompt size or delivery speed.
#[cfg(windows)]
fn append_codex_paste_boundary(runtime: &crate::terminal::TerminalRuntime, text: &mut Vec<u8>) {
    let keys = match crate::app::api_helpers::encode_api_keys(runtime, &["right".to_string()]) {
        Ok(keys) => keys,
        Err(key) => {
            tracing::warn!(key = %key, "failed to encode Codex paste boundary key");
            return;
        }
    };
    if let Some(key) = keys.into_iter().find(|bytes| !bytes.is_empty()) {
        text.extend_from_slice(&key);
    }
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    /// Both published aliases and legacy methods with an optional expectation
    /// must use actor completion, on every server/client API invocation route.
    pub(crate) fn api_request_requires_deferred_input(
        request: &crate::api::schema::Request,
    ) -> bool {
        use crate::api::schema::Method;
        match &request.method {
            Method::AgentPrompt(_)
            | Method::AgentPromptSessionChecked(_)
            | Method::PaneSendTextSessionChecked(_)
            | Method::PaneSendKeysSessionChecked(_) => true,
            Method::PaneSendText(params) => params.expected_agent_session_id.is_some(),
            Method::PaneSendKeys(params) => params.expected_agent_session_id.is_some(),
            _ => false,
        }
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        context: crate::api::ApiRequestContext,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        if !Self::api_request_requires_deferred_input(&request) {
            return false;
        }
        if let Some(response) = self.session_checked_guard_denial(&request) {
            let _ = respond_to.send(response);
            return true;
        }
        if let Some(response) = self.cross_pane_input_denial(&request, context) {
            let _ = respond_to.send(response);
            return true;
        }
        let params = match request.method {
            crate::api::schema::Method::AgentPrompt(params)
            | crate::api::schema::Method::AgentPromptSessionChecked(params) => params,
            crate::api::schema::Method::PaneSendText(params)
            | crate::api::schema::Method::PaneSendTextSessionChecked(params) => {
                self.defer_session_checked_pane_input(
                    request.id,
                    super::panes::SessionCheckedPaneInput::Text(params),
                    context,
                    respond_to,
                );
                return true;
            }
            crate::api::schema::Method::PaneSendKeys(params)
            | crate::api::schema::Method::PaneSendKeysSessionChecked(params) => {
                self.defer_session_checked_pane_input(
                    request.id,
                    super::panes::SessionCheckedPaneInput::Keys(params),
                    context,
                    respond_to,
                );
                return true;
            }
            _ => return false,
        };
        match self.queue_agent_prompt(request.id, params, context) {
            Ok((id, agent, completion)) => {
                std::thread::spawn(move || {
                    let response = match completion.recv() {
                        Ok(Ok(())) => encode_success(id, ResponseResult::AgentPrompted { agent }),
                        Ok(Err(err)) if crate::pty::actor::is_agent_session_lost(&err) => {
                            super::panes::session_loss_response(id, &err)
                        }
                        Ok(Err(err)) if crate::pty::actor::is_pane_input_poisoned(&err) => {
                            encode_error(id, "pane_input_poisoned", err.to_string())
                        }
                        Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
                            encode_error(id, "timeout", err.to_string())
                        }
                        Ok(Err(err)) => encode_error(id, "agent_prompt_failed", err.to_string()),
                        Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
                    };
                    let _ = respond_to.send(response);
                });
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    fn defer_session_checked_pane_input(
        &mut self,
        id: String,
        input: super::panes::SessionCheckedPaneInput,
        context: crate::api::ApiRequestContext,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        match self.queue_session_checked_pane_input(id.clone(), input, context) {
            Ok(completion) => {
                std::thread::spawn(move || {
                    let result = completion.recv().unwrap_or_else(|_| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "pty actor closed",
                        ))
                    });
                    let _ =
                        respond_to.send(super::panes::pane_input_completion_response(id, result));
                });
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
    }

    fn queue_agent_prompt(
        &mut self,
        id: String,
        params: AgentPromptParams,
        context: crate::api::ApiRequestContext,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        self.check_expected_pane(params.expected_pane_id.as_deref(), &resolved)
            .map_err(|error| encode_error_body(id.clone(), error))?;
        let guard = self
            .capture_expected_agent_session(
                params.expected_agent_session_id.as_deref(),
                resolved.ws_idx,
                resolved.pane_id,
            )
            .map_err(|error| encode_error_body(id.clone(), error))?;
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready(id, &params.target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready(id, &params.target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            ));
        }
        // Before any write to the pane, including the Copilot focus event.
        self.log_api_input(
            &id,
            "agent.prompt",
            resolved.ws_idx,
            resolved.pane_id,
            context,
            params.text.len(),
        )?;
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        let mut guarded_prefix = Vec::new();
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            };
            if guard.is_some() {
                // Focus and prompt are one guarded request: never send an
                // unguarded packet before the actor's ownership proof.
                guarded_prefix = focus;
            } else if let Err(err) = runtime.try_send_bytes_with_source(
                Bytes::from(focus),
                crate::pty::input_consumer::InputSource::Api,
            ) {
                return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        #[cfg(windows)]
        let text = if expected_agent == crate::detect::Agent::Codex {
            let mut text = text;
            append_codex_paste_boundary(runtime, &mut text);
            text
        } else {
            text
        };
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let completion = if let Some(guard) = guard {
            guarded_prefix.extend_from_slice(&text);
            runtime.queue_guarded_user_input_submission_with_source(
                Bytes::from(guarded_prefix),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                guard,
                crate::pty::input_consumer::InputSource::Api,
            )
        } else {
            runtime.queue_user_input_submission_with_source(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                submit_deadline,
                crate::pty::input_consumer::InputSource::Api,
            )
        }
        .map_err(|err| {
            if crate::pty::actor::is_agent_session_lost(&err) {
                super::panes::session_loss_response(id.clone(), &err)
            } else if crate::pty::actor::is_pane_input_poisoned(&err) {
                encode_error(id.clone(), "pane_input_poisoned", err.to_string())
            } else {
                encode_error(id.clone(), "agent_prompt_failed", err.to_string())
            }
        })?;
        // Receipt is issued on enqueue, before the asynchronous submission completes.
        self.accepted_api_inputs.push(resolved.pane_id);
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
        context: crate::api::ApiRequestContext,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(response) = self.log_api_input(
            &id,
            "agent.send_keys",
            resolved.ws_idx,
            resolved.pane_id,
            context,
            bytes.len(),
        ) {
            return response;
        }
        let has_input = !bytes.is_empty();
        if let Err(err) = runtime.try_send_bytes_with_source(
            Bytes::from(bytes),
            crate::pty::input_consumer::InputSource::Api,
        ) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }
        if has_input {
            self.accepted_api_inputs.push(resolved.pane_id);
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    fn pane_request_params(
        pane_id: &str,
        expected_agent_session_id: Option<&str>,
        allow_cross_pane: bool,
    ) -> serde_json::Value {
        let mut params = serde_json::json!({
            "pane_id": pane_id,
            "text": "hello",
            "keys": ["enter"],
            "allow_cross_pane": allow_cross_pane,
        });
        if let Some(expected) = expected_agent_session_id {
            params["expected_agent_session_id"] = serde_json::json!(expected);
        }
        params
    }

    #[test]
    fn deferred_input_route_covers_aliases_and_optional_base_expectations() {
        for method in [
            "pane.send_text",
            "pane.send_text_session_checked",
            "pane.send_keys",
            "pane.send_keys_session_checked",
        ] {
            for expected in [None, Some("opaque-session")] {
                let request: crate::api::schema::Request =
                    serde_json::from_value(serde_json::json!({
                        "id": "route", "method": method,
                        "params": pane_request_params(
                            "w1:p1",
                            expected,
                            false,
                        ),
                    }))
                    .expect("pane request");
                assert_eq!(
                    App::api_request_requires_deferred_input(&request),
                    method.ends_with("_session_checked") || expected.is_some(),
                    "{method}: {expected:?}",
                );
            }
        }
    }

    #[tokio::test]
    async fn deferred_guarded_pane_denials_enqueue_nothing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let pane = app.public_pane_id(0, pane_id).expect("pane ID");
        assert!(app.check_expected_agent_session(None, 0, pane_id).is_ok());
        let (runtime, mut input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);
        for method in [
            "pane.send_text",
            "pane.send_keys",
            "pane.send_text_session_checked",
            "pane.send_keys_session_checked",
        ] {
            for expected in [None, Some("opaque-session")] {
                if expected.is_none() && !method.ends_with("_session_checked") {
                    continue;
                }
                let request = serde_json::from_value(serde_json::json!({
                    "id": "denied", "method": method,
                    "params": pane_request_params(&pane, expected, true),
                }))
                .expect("pane request");
                let (respond_to, response_rx) = std::sync::mpsc::channel();
                assert!(app.handle_deferred_agent_api_request(
                    request,
                    Default::default(),
                    respond_to
                ));
                let response: crate::api::schema::ErrorResponse = serde_json::from_str(
                    &response_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    response.error.code,
                    if !crate::platform::expected_agent_session_guard_supported() {
                        "expected_agent_session_unsupported"
                    } else if expected.is_some() {
                        "agent_session_unknown"
                    } else {
                        "invalid_request"
                    },
                    "{method}: {expected:?}",
                );
                assert!(input_rx.try_recv().is_err());
                assert!(app.accepted_api_inputs.is_empty());
            }
        }
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            crate::api::ApiRequestContext::default(),
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_codex_prompt_flushes_paste_burst_before_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                allow_cross_pane: true,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        // The non-character key must precede Enter so Codex commits the paste burst first.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B\x1b[C"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces the registration loss reported on #3225 by rszrszrsz:
        // a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(
            "req:before".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&found).is_ok(),
            "the assigned name must resolve while the agent is running: {found}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at,
        });
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(
            "req:after".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&after).is_ok(),
            "a live agent must stay reachable by its assigned name: {after}"
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
                // Synthetic channel fixture isolates submission, not caller attribution.
                allow_cross_pane: true,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                allow_cross_pane: true,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
                allow_cross_pane: true,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
                // Isolate the blocked-agent check with an explicit synthetic opt-in.
                allow_cross_pane: true,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                // Synthetic channel fixture isolates focus/paste/submit ordering.
                allow_cross_pane: true,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
                allow_cross_pane: false,
            },
            Default::default(),
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
                allow_cross_pane: false,
            },
            Default::default(),
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
                // Isolate managed startup readiness from caller attribution.
                allow_cross_pane: true,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
