use super::responses::encode_error;
use super::App;
use crate::api::schema::{Method, Request};
use crate::api::ApiRequestContext;
use crate::app::terminal_targets::{InputOrigin, TerminalTarget};

fn session_checked_guard_error(
    supported: bool,
    method: &Method,
) -> Option<(&'static str, &'static str)> {
    let prompt_params = match method {
        Method::AgentPrompt(params)
        | Method::AgentPromptSessionChecked(params)
        | Method::AgentPromptStatusChecked(params) => Some(params),
        _ => None,
    };
    // Invalid status/session pairing takes precedence on every platform.
    if prompt_params.is_some_and(|params| {
        params.expected_agent_status.is_some() && params.expected_agent_session_id.is_none()
    }) {
        return Some((
            "invalid_request",
            "expected_agent_status requires expected_agent_session_id",
        ));
    }
    let missing = match method {
        Method::AgentPromptStatusChecked(params) => {
            params.expected_agent_session_id.is_none() || params.expected_agent_status.is_none()
        }
        Method::AgentPromptSessionChecked(params) => {
            params.expected_agent_session_id.is_none() && params.expected_pane_id.is_none()
        }
        Method::PaneSendTextSessionChecked(params) => params.expected_agent_session_id.is_none(),
        Method::PaneSendKeysSessionChecked(params) => params.expected_agent_session_id.is_none(),
        _ => return None,
    };
    if !supported {
        return Some((
            "expected_agent_session_unsupported",
            "expected agent session guards are supported only on Linux",
        ));
    }
    missing.then_some((
        "invalid_request",
        "checked method requires its input guard expectations",
    ))
}

impl App {
    pub(super) fn session_checked_guard_denial(&self, request: &Request) -> Option<String> {
        session_checked_guard_error(
            crate::platform::expected_agent_session_guard_supported(),
            &request.method,
        )
        .map(|(code, message)| encode_error(request.id.clone(), code, message))
    }

    // This is a policy guard, not caller authentication.
    pub(super) fn cross_pane_input_denial(
        &self,
        request: &Request,
        context: ApiRequestContext,
    ) -> Option<String> {
        // Decide from the method first: attribution walks every agent pane's session in
        // /proc, so doing it for each API request kept the server busy (smarty-dev#931).
        // Only a content write that does not allow cross-pane input needs it.
        if Self::allows_cross_pane(&request.method) {
            return None;
        }
        let target = self.content_write_target(&request.method)?;
        let origin = self.input_origin_for_context(context);
        let (code, message) = match origin {
            InputOrigin::Ordinary => return None,
            InputOrigin::Agent(source) if source.terminal_id == target.terminal_id => return None,
            InputOrigin::Agent(_) => (
                "cross_pane_input_denied",
                "agent-originated input cannot target a different pane",
            ),
            InputOrigin::Unknown => (
                "input_origin_unknown",
                "cannot validate input origin; retry from a live attributable caller or explicitly opt in with CLI --allow-cross-pane (API allow_cross_pane: true)",
            ),
        };
        Some(encode_error(request.id.clone(), code, message))
    }

    fn allows_cross_pane(method: &Method) -> bool {
        match method {
            Method::AgentStart(params) | Method::AgentStartGuarded(params) => {
                params.allow_cross_pane
            }
            Method::AgentPrompt(params)
            | Method::AgentPromptSessionChecked(params)
            | Method::AgentPromptStatusChecked(params) => params.allow_cross_pane,
            Method::AgentSendKeys(params) => params.allow_cross_pane,
            Method::PaneReportAgent(params) => params.allow_cross_pane,
            Method::PaneReportAgentSession(params) => params.allow_cross_pane,
            Method::PaneSendText(params) | Method::PaneSendTextSessionChecked(params) => {
                params.allow_cross_pane
            }
            Method::PaneSendKeys(params) | Method::PaneSendKeysSessionChecked(params) => {
                params.allow_cross_pane
            }
            Method::PaneSendInput(params) | Method::PaneSendInputGuarded(params) => {
                params.allow_cross_pane
            }
            _ => false,
        }
    }

    fn content_write_target(&self, method: &Method) -> Option<TerminalTarget> {
        match method {
            Method::AgentStart(params) | Method::AgentStartGuarded(params) => {
                self.pane_target(&params.pane_id)
            }
            Method::AgentPrompt(params)
            | Method::AgentPromptSessionChecked(params)
            | Method::AgentPromptStatusChecked(params) => {
                self.resolve_agent_target(&params.target).ok()
            }
            Method::AgentSendKeys(params) => self.resolve_agent_target(&params.target).ok(),
            Method::PaneSendText(params) | Method::PaneSendTextSessionChecked(params) => {
                self.pane_target(&params.pane_id)
            }
            Method::PaneSendKeys(params) | Method::PaneSendKeysSessionChecked(params) => {
                self.pane_target(&params.pane_id)
            }
            Method::PaneSendInput(params) | Method::PaneSendInputGuarded(params) => {
                self.pane_target(&params.pane_id)
            }
            Method::PaneReportAgent(params) if params.resume_argv.is_some() => {
                self.pane_target(&params.pane_id)
            }
            Method::PaneReportAgentSession(params) if params.resume_argv.is_some() => {
                self.pane_target(&params.pane_id)
            }
            _ => None,
        }
    }

    fn pane_target(&self, pane_id: &str) -> Option<TerminalTarget> {
        let (ws_idx, pane_id) = self.parse_pane_id(pane_id)?;
        self.terminal_target_for_pane(ws_idx, pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        AgentPromptParams, AgentSendKeysParams, AgentStartParams, ErrorResponse,
        PaneReportAgentParams, PaneSendInputParams, PaneSendKeysParams, PaneSendTextParams,
        ResponseResult, SuccessResponse,
    };
    use crate::app::Mode;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;
    use bytes::Bytes;
    use tokio::sync::mpsc::Receiver;

    #[test]
    fn session_checked_prompt_support_is_required_regardless_of_expectations() {
        for (session, pane) in [
            (None, None),
            (None, Some("w1:p1")),
            (Some("session"), None),
            (Some("session"), Some("w1:p1")),
        ] {
            for status in [None, Some(crate::api::schema::AgentStatus::Idle)] {
                let params = AgentPromptParams {
                    expected_agent_session_id: session.map(str::to_owned),
                    expected_pane_id: pane.map(str::to_owned),
                    expected_agent_status: status,
                    target: "agent".into(),
                    text: "prompt".into(),
                    wait: None,
                    allow_cross_pane: true,
                };
                let methods = [
                    Method::AgentPromptSessionChecked(params.clone()),
                    Method::AgentPromptStatusChecked(params),
                ];
                for method in methods {
                    if status.is_some() && session.is_none() {
                        for supported in [false, true] {
                            assert_eq!(
                                session_checked_guard_error(supported, &method),
                                Some((
                                    "invalid_request",
                                    "expected_agent_status requires expected_agent_session_id",
                                ))
                            );
                        }
                        continue;
                    }
                    assert_eq!(
                        session_checked_guard_error(false, &method),
                        Some((
                            "expected_agent_session_unsupported",
                            "expected agent session guards are supported only on Linux",
                        ))
                    );
                    let missing = if matches!(method, Method::AgentPromptStatusChecked(_)) {
                        session.is_none() || status.is_none()
                    } else {
                        session.is_none() && pane.is_none()
                    };
                    assert_eq!(
                        session_checked_guard_error(true, &method),
                        missing.then_some((
                            "invalid_request",
                            "checked method requires its input guard expectations",
                        ))
                    );
                }
            }
        }
    }

    #[test]
    fn session_checked_pane_send_support_is_required_regardless_of_expectations() {
        for session in [None, Some("session")] {
            let methods = [
                Method::PaneSendTextSessionChecked(PaneSendTextParams {
                    expected_agent_session_id: session.map(str::to_owned),
                    pane_id: "w1:p1".into(),
                    text: "text".into(),
                    allow_cross_pane: true,
                }),
                Method::PaneSendKeysSessionChecked(PaneSendKeysParams {
                    expected_agent_session_id: session.map(str::to_owned),
                    pane_id: "w1:p1".into(),
                    keys: vec!["enter".into()],
                    allow_cross_pane: true,
                }),
            ];
            for method in methods {
                assert_eq!(
                    session_checked_guard_error(false, &method).map(|(code, _)| code),
                    Some("expected_agent_session_unsupported")
                );
                assert_eq!(
                    session_checked_guard_error(true, &method),
                    session.is_none().then_some((
                        "invalid_request",
                        "checked method requires its input guard expectations",
                    ))
                );
            }
        }
    }

    #[test]
    fn session_checked_guard_does_not_change_unchecked_methods() {
        let methods = [
            Method::AgentPrompt(AgentPromptParams {
                expected_agent_session_id: Some("session".into()),
                expected_pane_id: Some("w1:p1".into()),
                expected_agent_status: Some(crate::api::schema::AgentStatus::Idle),
                target: "agent".into(),
                text: "prompt".into(),
                wait: None,
                allow_cross_pane: true,
            }),
            Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: Some("session".into()),
                pane_id: "w1:p1".into(),
                text: "text".into(),
                allow_cross_pane: true,
            }),
            Method::PaneSendKeys(PaneSendKeysParams {
                expected_agent_session_id: Some("session".into()),
                pane_id: "w1:p1".into(),
                keys: vec!["enter".into()],
                allow_cross_pane: true,
            }),
        ];
        for method in methods {
            for supported in [false, true] {
                assert_eq!(session_checked_guard_error(supported, &method), None);
            }
        }
    }

    #[test]
    fn unchecked_prompt_status_requires_session_on_every_platform() {
        for pane in [None, Some("w1:p1")] {
            let method = Method::AgentPrompt(AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: pane.map(str::to_owned),
                expected_agent_status: Some(crate::api::schema::AgentStatus::Idle),
                target: "agent".into(),
                text: "prompt".into(),
                wait: None,
                allow_cross_pane: true,
            });
            for supported in [false, true] {
                assert_eq!(
                    session_checked_guard_error(supported, &method),
                    Some((
                        "invalid_request",
                        "expected_agent_status requires expected_agent_session_id",
                    ))
                );
            }
        }
    }

    struct Fixture {
        app: App,
        source_pane_id: String,
        target_pane_id: String,
        source_rx: Receiver<Bytes>,
        target_rx: Receiver<Bytes>,
    }

    fn attributed_agent_fixture() -> Fixture {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("input-guard");
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
        let target_terminal_id = app.state.workspaces[0]
            .terminal_id(target_pane)
            .cloned()
            .expect("target terminal");
        let source_terminal = app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state");
        source_terminal.set_agent_name("source-agent".into());
        source_terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target_terminal = app
            .state
            .terminals
            .get_mut(&target_terminal_id)
            .expect("target state");
        target_terminal.set_agent_name("target-agent".into());
        target_terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);

        let (source_runtime, source_rx) =
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
            source_rx,
            target_rx,
        }
    }

    fn attributed_context() -> ApiRequestContext {
        ApiRequestContext::for_local_peer_pid(Some(std::process::id()))
    }

    fn assert_denied(response: &str) {
        let response: ErrorResponse = serde_json::from_str(response).expect("denial response");
        assert_eq!(response.error.code, "cross_pane_input_denied");
    }

    fn assert_unknown(response: &str) {
        let response: ErrorResponse = serde_json::from_str(response).expect("unknown response");
        assert_eq!(response.error.code, "input_origin_unknown");
    }

    fn assert_ok(response: &str) {
        let response: SuccessResponse = serde_json::from_str(response).expect("success response");
        assert!(matches!(response.result, ResponseResult::Ok {}));
    }

    #[tokio::test]
    async fn expected_agent_status_admission_requires_session_and_checked_alias_requires_status() {
        let mut fixture = attributed_agent_fixture();
        for method in [
            "agent.prompt",
            "agent.prompt_session_checked",
            "agent.prompt_status_checked",
        ] {
            for (session, status, pane) in [
                (false, false, false),
                (false, false, true),
                (false, true, false),
                (false, true, true),
                (true, false, false),
                (true, true, false),
            ] {
                let mut params = serde_json::json!({
                    "target": fixture.target_pane_id, "text": "bad", "allow_cross_pane": true,
                });
                if session {
                    params["expected_agent_session_id"] = "opaque-session".into();
                }
                if status {
                    params["expected_agent_status"] = "idle".into();
                }
                if pane {
                    params["expected_pane_id"] = fixture.target_pane_id.clone().into();
                }
                let request: Request = serde_json::from_value(serde_json::json!({
                    "id": "admission", "method": method, "params": params,
                }))
                .expect("prompt request");
                assert!(
                    App::api_request_requires_deferred_input(&request),
                    "{method}"
                );
                let expected_error = if status && !session {
                    Some((
                        "invalid_request",
                        "expected_agent_status requires expected_agent_session_id",
                    ))
                } else if method != "agent.prompt"
                    && !crate::platform::expected_agent_session_guard_supported()
                {
                    Some((
                        "expected_agent_session_unsupported",
                        "expected agent session guards are supported only on Linux",
                    ))
                } else {
                    let missing = match method {
                        "agent.prompt_status_checked" => !status || !session,
                        "agent.prompt_session_checked" => !session && !pane,
                        _ => false,
                    };
                    missing.then_some((
                        "invalid_request",
                        "checked method requires its input guard expectations",
                    ))
                };
                let denial = fixture.app.session_checked_guard_denial(&request);
                assert_eq!(
                    denial.is_some(),
                    expected_error.is_some(),
                    "{method}: session={session}, status={status}, pane={pane}"
                );
                if let Some((code, message)) = expected_error {
                    let assert_error = |response: &str| {
                        let error: ErrorResponse =
                            serde_json::from_str(response).expect("denial response");
                        assert_eq!(error.error.code, code);
                        assert_eq!(error.error.message, message);
                    };
                    assert_error(&denial.expect("denial"));
                    // Both direct and deferred app admission reject before lookup or input attribution.
                    assert_error(
                        &fixture
                            .app
                            .handle_api_request_with_context(request.clone(), Default::default()),
                    );
                    let (tx, rx) = std::sync::mpsc::channel();
                    assert!(fixture.app.handle_deferred_agent_api_request(
                        request,
                        Default::default(),
                        tx
                    ));
                    assert_error(
                        &rx.recv_timeout(std::time::Duration::from_secs(1))
                            .expect("denial deferred response"),
                    );
                    assert!(fixture.app.accepted_api_inputs.is_empty());
                    assert!(fixture.source_rx.try_recv().is_err());
                    assert!(fixture.target_rx.try_recv().is_err());
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn imported_legacy_pty_pin_enforces_guard_and_preserves_own_override_paths() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        struct LogWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut fixture = attributed_agent_fixture();
        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source");
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("real transferred PTY");
        let mut command = portable_pty::CommandBuilder::new("sh");
        command.args(["-c", "read line"]);
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("live original leader");
        let pid = child.process_id().expect("root PID");
        let original = crate::platform::process_identity(pid).expect("original root");
        let context = ApiRequestContext::for_local_peer_pid(Some(pid));
        let mut writer = pair.master.take_writer().expect("root input");
        // Legacy match, actual unrelated live PID, supplied stale pin (must NOT
        // fall back to legacy), new-to-new original pin, then legacy exited root.
        for (index, (candidate_pid, start_time, expected)) in [
            (pid, None, Some(original)),
            (std::process::id(), None, None),
            (pid, Some(original.start_time.wrapping_add(1)), None),
            (pid, Some(original.start_time), Some(original)),
            (pid, None, None),
        ]
        .into_iter()
        .enumerate()
        {
            if index == 4 {
                writer.write_all(b"\n").expect("release owned root");
                child.wait().expect("reap root");
            }
            let mut json = serde_json::json!({
                "pane_id": source_pane.raw(), "child_pid": candidate_pid,
                "rows": 24, "cols": 80, "cell_width_px": 0, "cell_height_px": 0
            });
            if let Some(start_time) = start_time {
                json["child_start_time"] = start_time.into();
            }
            let master_fd = unsafe { libc::dup(pair.master.as_raw_fd().expect("master")) };
            assert!(master_fd >= 0);
            let logs = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::WARN)
                .with_writer({
                    let logs = logs.clone();
                    move || LogWriter(logs.clone())
                })
                .finish();
            let (events, _events_rx) = tokio::sync::mpsc::channel(8);
            let (runtime, repeat_runtime) = tracing::subscriber::with_default(subscriber, || {
                let runtime = crate::terminal::TerminalRuntime::from_handoff_fd(
                    crate::handoff_runtime::ImportedHandoffRuntime {
                        master_fd,
                        state: serde_json::from_value(json).expect("manifest"),
                    },
                    4096,
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    events,
                    Arc::new(tokio::sync::Notify::new()),
                    Arc::new(crate::render_signal::RenderSignal::new()),
                )
                .expect("import runtime");
                assert_eq!(runtime.child_process_identity(), expected);
                assert_eq!(runtime.child_process_identity(), expected);
                if index == 2 {
                    // Exercise the production import -> export -> reimport path.  Before
                    // the repair, the rejected pin is omitted and this second import
                    // incorrectly adopts the live PTY owner through legacy bootstrap.
                    let exported = runtime.handoff_runtime_state(source_pane.raw());
                    let second_master_fd =
                        unsafe { libc::dup(pair.master.as_raw_fd().expect("master")) };
                    assert!(second_master_fd >= 0);
                    let second = crate::terminal::TerminalRuntime::from_handoff_fd(
                        crate::handoff_runtime::ImportedHandoffRuntime {
                            master_fd: second_master_fd,
                            state: exported.clone(),
                        },
                        4096,
                        crate::terminal_theme::TerminalTheme::default(),
                        None,
                        tokio::sync::mpsc::channel(8).0,
                        Arc::new(tokio::sync::Notify::new()),
                        Arc::new(crate::render_signal::RenderSignal::new()),
                    )
                    .expect("reimport runtime");
                    assert_eq!(second.child_pid(), Some(pid));
                    assert_eq!(second.child_process_identity(), None);
                    let original_start_time = Some(start_time.expect("stale timestamp"));
                    assert_eq!(exported.child_start_time, original_start_time);
                    assert_eq!(
                        second
                            .handoff_runtime_state(source_pane.raw())
                            .child_start_time,
                        original_start_time
                    );
                    (runtime, Some(second))
                } else if expected.is_some() {
                    assert_eq!(
                        runtime
                            .handoff_runtime_state(source_pane.raw())
                            .child_start_time,
                        Some(original.start_time)
                    );
                    (runtime, None)
                } else {
                    (runtime, None)
                }
            });
            let logs = String::from_utf8(logs.lock().expect("logs").clone()).expect("UTF8 log");
            assert_eq!(
                logs.matches("handoff root identity unavailable").count(),
                usize::from(expected.is_none()) + usize::from(index == 2),
                "case {index}: {logs}"
            );
            fixture.app.state.insert_test_runtime(source_pane, runtime);
            let mut request = Request {
                id: format!("import-{index}"),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "foreign".into(),
                    allow_cross_pane: false,
                }),
            };
            let denial = fixture.app.cross_pane_input_denial(&request, context);
            if expected.is_some() {
                let denial = denial.expect("imported guard applies");
                // Name the failing branch under load: capture-time origin,
                // live root identity, and whether the owned root exited.
                assert!(
                    denial.contains("cross_pane_input_denied"),
                    "case {index}: {denial}; origin={:?}; live={:?}; original={original:?}; root_exit={:?}; recaptured={:?}; environ={:?}",
                    context.local_peer_pane_origin,
                    crate::platform::process_identity(pid),
                    child.try_wait(),
                    ApiRequestContext::for_local_peer_pid(Some(pid)).local_peer_pane_origin,
                    std::fs::read(format!("/proc/{pid}/environ")).map(|bytes| (
                        bytes.len(),
                        bytes.last().copied(),
                        bytes
                            .split(|&byte| byte == 0)
                            .filter(|record| record.starts_with(b"HERDR") || !record.contains(&b'='))
                            .map(|record| String::from_utf8_lossy(record).into_owned())
                            .collect::<Vec<_>>()
                    )),
                );
                assert_denied(&denial);
            } else {
                assert_unknown(&denial.expect("unprovable import refuses input"));
            }
            if let Some(repeated_runtime) = repeat_runtime {
                fixture
                    .app
                    .state
                    .insert_test_runtime(source_pane, repeated_runtime);
                let repeated_request = Request {
                    id: format!("reimport-{index}"),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: fixture.target_pane_id.clone(),
                        text: "foreign-again".into(),
                        allow_cross_pane: false,
                    }),
                };
                assert_unknown(
                    &fixture
                        .app
                        .cross_pane_input_denial(&repeated_request, context)
                        .expect("rejected reimport refuses input"),
                );
            }
            if let Method::PaneSendText(params) = &mut request.method {
                params.pane_id = fixture.source_pane_id.clone();
            }
            let own_denial = fixture.app.cross_pane_input_denial(&request, context);
            if expected.is_some() {
                assert!(own_denial.is_none());
            } else {
                assert_unknown(&own_denial.expect("unproven own pane is also protected"));
            }
            request.method = Method::AgentPrompt(AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                expected_agent_status: None,
                target: "target-agent".into(),
                text: "explicit".into(),
                wait: None,
                allow_cross_pane: true,
            });
            assert!(fixture
                .app
                .cross_pane_input_denial(&request, context)
                .is_none());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct GuardTestChild(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl std::ops::Deref for GuardTestChild {
        type Target = std::process::Child;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl std::ops::DerefMut for GuardTestChild {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for GuardTestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // A real exec of this unsigned test binary preserves observable initial
    // environment on Darwin, unlike protected system sleep/Python binaries.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn guard_exec_peer_helper() {
        use std::io::Write;
        let Ok(mode) = std::env::var("HERDR_GUARD_EXEC_HELPER") else {
            return;
        };
        let marked = std::env::var("HERDR_ENV").as_deref() == Ok("1")
            && std::env::var("HERDR_PANE_ID").is_ok_and(|value| !value.is_empty());
        match mode.as_str() {
            "marked" => assert!(marked, "exec-inherited markers missing"),
            "ordinary" => {
                for key in [
                    "HERDR_ENV",
                    "HERDR_PANE_ID",
                    "HERDR_WORKSPACE_ID",
                    "HERDR_TAB_ID",
                ] {
                    assert!(
                        std::env::var_os(key).is_none(),
                        "ordinary exec inherited markers"
                    );
                }
            }
            _ => panic!("invalid helper mode"),
        }
        println!("guard-exec-ready {}", std::process::id());
        std::io::stdout().flush().expect("readiness receipt");
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn detached_sleep_child() -> GuardTestChild {
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        crate::platform::detach_server_daemon_command(&mut command);
        GuardTestChild(command.spawn().expect("spawn detached sleep child"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn setsid_child_of_agent_is_denied_cross_pane_input() {
        let mut fixture = attributed_agent_fixture();
        let mut child = detached_sleep_child();
        let peer_pid = child.id();
        assert!(!crate::platform::process_in_pane_session(
            std::process::id(),
            peer_pid,
        ));
        let source = fixture
            .app
            .pane_target(&fixture.source_pane_id)
            .expect("source target");
        let attributed = fixture
            .app
            .agent_terminal_target_for_peer_identity(
                crate::platform::process_identity(peer_pid).expect("live detached identity"),
            )
            .expect("detached child attributed to source agent");
        assert_eq!(attributed.terminal_id, source.terminal_id);
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "setsid-cross-pane".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "setsid child".into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext::for_local_peer_pid(Some(peer_pid)),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_denied(&response);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn setsid_agent_can_explicitly_allow_cross_pane_agent_prompt() {
        let mut fixture = attributed_agent_fixture();
        let mut child = detached_sleep_child();
        let context = ApiRequestContext::for_local_peer_pid(Some(child.id()));
        let mut request = Request {
            id: "setsid-allow-cross-pane-check".into(),
            method: Method::AgentPrompt(AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                expected_agent_status: None,
                target: "target-agent".into(),
                text: "explicitly allowed".into(),
                wait: None,
                allow_cross_pane: false,
            }),
        };
        assert_denied(
            &fixture
                .app
                .cross_pane_input_denial(&request, context)
                .expect("attributed detached child must be denied without opt-in"),
        );
        if let Method::AgentPrompt(params) = &mut request.method {
            params.allow_cross_pane = true;
        }
        assert!(fixture
            .app
            .cross_pane_input_denial(&request, context)
            .is_none());

        // The prompt fixture lacks detector runtime; prove actual opt-in delivery
        // through the text receiver using the very same attributed child PID.
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "setsid-allow-cross-pane-text".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "explicitly allowed".into(),
                    allow_cross_pane: true,
                }),
            },
            context,
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("opt-in target bytes"),
            Bytes::from_static(b"explicitly allowed")
        );
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn setsid_agent_can_send_to_its_own_pane() {
        let mut fixture = attributed_agent_fixture();
        let mut child = detached_sleep_child();
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "setsid-same-pane".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.source_pane_id.clone(),
                    text: "setsid own pane".into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext::for_local_peer_pid(Some(child.id())),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_ok(&response);
        assert_eq!(
            fixture.source_rx.try_recv().expect("same-pane bytes"),
            Bytes::from_static(b"setsid own pane")
        );
    }

    #[tokio::test]
    async fn attributed_agent_cannot_inject_content_into_a_different_pane() {
        let mut fixture = attributed_agent_fixture();
        let methods = vec![
            Method::AgentStart(AgentStartParams {
                name: "new-agent".into(),
                kind: "pi".into(),
                pane_id: fixture.target_pane_id.clone(),
                expected_terminal: None,
                args: Vec::new(),
                timeout_ms: None,
                allow_cross_pane: false,
            }),
            Method::AgentPrompt(AgentPromptParams {
                expected_agent_session_id: None,
                expected_pane_id: None,
                expected_agent_status: None,
                target: "target-agent".into(),
                text: "prompt".into(),
                wait: None,
                allow_cross_pane: false,
            }),
            Method::AgentPromptStatusChecked(AgentPromptParams {
                expected_agent_session_id: Some("opaque-session".into()),
                expected_pane_id: None,
                expected_agent_status: Some(crate::api::schema::AgentStatus::Idle),
                target: "target-agent".into(),
                text: "guarded prompt".into(),
                wait: None,
                allow_cross_pane: false,
            }),
            Method::AgentSendKeys(AgentSendKeysParams {
                target: "target-agent".into(),
                keys: vec!["enter".into()],
                allow_cross_pane: false,
            }),
            Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "text".into(),
                allow_cross_pane: false,
            }),
            Method::PaneSendKeys(PaneSendKeysParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                keys: vec!["enter".into()],
                allow_cross_pane: false,
            }),
            Method::PaneSendInput(PaneSendInputParams {
                pane_id: fixture.target_pane_id.clone(),
                text: "run".into(),
                expected_terminal: None,
                keys: vec!["enter".into()],
                allow_cross_pane: false,
            }),
        ];

        let guarded_methods: Vec<_> = methods
            .iter()
            .filter_map(|method| match method {
                Method::AgentStart(params) => {
                    let mut params = params.clone();
                    params.expected_terminal = Some("term_guarded".into());
                    Some(Method::AgentStartGuarded(params))
                }
                Method::PaneSendInput(params) => {
                    let mut params = params.clone();
                    params.expected_terminal = Some("term_guarded".into());
                    Some(Method::PaneSendInputGuarded(params))
                }
                _ => None,
            })
            .collect();
        for (index, method) in methods.into_iter().chain(guarded_methods).enumerate() {
            let unsupported_guard = matches!(&method, Method::AgentPromptStatusChecked(_))
                && !crate::platform::expected_agent_session_guard_supported();
            for (context, origin_code) in [
                (ApiRequestContext::default(), "input_origin_unknown"),
                (attributed_context(), "cross_pane_input_denied"),
            ] {
                let response = fixture.app.handle_api_request_with_context(
                    Request {
                        id: format!("denied-{origin_code}-{index}"),
                        method: method.clone(),
                    },
                    context,
                );
                let response: ErrorResponse =
                    serde_json::from_str(&response).expect("denial response");
                assert_eq!(
                    response.error.code,
                    if unsupported_guard {
                        "expected_agent_session_unsupported"
                    } else {
                        origin_code
                    },
                    "method {index}: {origin_code}"
                );
                assert!(fixture.source_rx.try_recv().is_err());
                assert!(fixture.target_rx.try_recv().is_err());
                assert!(fixture.app.accepted_api_inputs.is_empty());
            }
        }

        assert!(fixture.source_rx.try_recv().is_err());
        assert!(fixture.target_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_start_override_bypasses_cross_pane_denial() {
        let fixture = attributed_agent_fixture();
        let request = Request {
            id: "allowed-agent-start".into(),
            method: Method::AgentStart(AgentStartParams {
                name: "new-agent".into(),
                kind: "pi".into(),
                pane_id: fixture.target_pane_id.clone(),
                expected_terminal: None,
                args: Vec::new(),
                timeout_ms: None,
                allow_cross_pane: true,
            }),
        };

        assert!(fixture
            .app
            .cross_pane_input_denial(&request, attributed_context())
            .is_none());
    }

    #[tokio::test]
    async fn attributed_agent_can_deliberately_target_a_different_pane() {
        let mut fixture = attributed_agent_fixture();
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "allowed-cross-pane".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "deliberate".into(),
                    allow_cross_pane: true,
                }),
            },
            attributed_context(),
        );

        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("cross-pane bytes"),
            Bytes::from_static(b"deliberate")
        );
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn resume_reports_are_cross_pane_content_writes_but_own_pane_reports_work() {
        let mut fixture = attributed_agent_fixture();
        let target_response = fixture.app.handle_api_request_with_context(
            Request {
                id: "foreign-resume".into(),
                method: Method::PaneReportAgent(PaneReportAgentParams {
                    allow_cross_pane: false,
                    pane_id: fixture.target_pane_id.clone(),
                    source: "custom:pi".into(),
                    agent: "pi".into(),
                    state: crate::api::schema::PaneAgentState::Working,
                    message: None,
                    seq: Some(1),
                    agent_session_id: None,
                    agent_session_path: None,
                    resume_argv: Some(vec!["attacker".into()]),
                }),
            },
            attributed_context(),
        );
        assert_denied(&target_response);
        let (_, target_pane) = fixture.app.parse_pane_id(&fixture.target_pane_id).unwrap();
        let target_terminal_id = fixture.app.state.workspaces[0]
            .terminal_id(target_pane)
            .unwrap()
            .clone();
        assert!(fixture.app.state.terminals[&target_terminal_id]
            .reported_resume()
            .is_none());

        let own_response = fixture.app.handle_api_request_with_context(
            Request {
                id: "own-resume".into(),
                method: Method::PaneReportAgent(PaneReportAgentParams {
                    allow_cross_pane: false,
                    pane_id: fixture.source_pane_id.clone(),
                    source: "custom:pi".into(),
                    agent: "pi".into(),
                    state: crate::api::schema::PaneAgentState::Working,
                    message: None,
                    seq: Some(1),
                    agent_session_id: None,
                    agent_session_path: None,
                    resume_argv: Some(vec!["pi".into()]),
                }),
            },
            attributed_context(),
        );
        assert_ok(&own_response);
    }

    #[tokio::test]
    async fn attributed_agent_can_send_content_to_its_own_pane() {
        let mut fixture = attributed_agent_fixture();
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "same-pane".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.source_pane_id.clone(),
                    text: "same pane".into(),
                    allow_cross_pane: false,
                }),
            },
            attributed_context(),
        );

        assert_ok(&response);
        assert_eq!(
            fixture.source_rx.try_recv().expect("same-pane bytes"),
            Bytes::from_static(b"same pane")
        );
        assert!(fixture.target_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_pinned_peer_is_not_replaced_by_current_pid_owner() {
        let mut fixture = attributed_agent_fixture();
        let live =
            crate::platform::process_identity(std::process::id()).expect("live test peer identity");
        let stale = crate::platform::ProcessIdentity {
            start_time: live.start_time.wrapping_add(1),
            ..live
        };
        assert!(fixture
            .app
            .agent_terminal_target_for_peer_identity(stale)
            .is_none());
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "stale-peer".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "unattributed".into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext::capture(Some(stale)),
        );
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_origins_are_refused_but_known_ordinary_shell_is_allowed() {
        let mut fixture = attributed_agent_fixture();
        for (id, context) in [
            ("unknown", ApiRequestContext::default()),
            (
                "out-of-pane",
                ApiRequestContext::for_local_peer_pid(Some(u32::MAX)),
            ),
        ] {
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: id.into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: fixture.target_pane_id.clone(),
                        text: id.into(),
                        allow_cross_pane: false,
                    }),
                },
                context,
            );
            assert_unknown(&response);
            assert!(fixture.target_rx.try_recv().is_err());
            assert!(fixture.source_rx.try_recv().is_err());
        }

        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source pane");
        let source_terminal_id = fixture.app.state.workspaces[0]
            .terminal_id(source_pane)
            .cloned()
            .expect("source terminal");
        let source_terminal = fixture
            .app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state");
        source_terminal.clear_agent_name();
        source_terminal.set_detected_state(None, AgentState::Unknown);

        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "non-agent".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "non-agent".into(),
                    allow_cross_pane: false,
                }),
            },
            attributed_context(),
        );
        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("non-agent input bytes"),
            Bytes::from_static(b"non-agent")
        );
    }

    #[tokio::test]
    async fn failed_checked_membership_is_unknown_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let request = Request {
            id: "invalid-membership".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "refused".into(),
                allow_cross_pane: false,
            }),
        };
        let response = crate::platform::with_checked_membership_for_test(None, || {
            fixture
                .app
                .handle_api_request_with_context(request, attributed_context())
        });
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn complete_ancestry_negative_membership_allows_ordinary_shell_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let (_, target_pane) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target pane");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(std::process::id());
        let request = Request {
            id: "complete-ancestry-outside".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "ordinary".into(),
                allow_cross_pane: false,
            }),
        };
        // Deliberate scoped observation fixture, not a claim about this test
        // process's inherited launch environment.
        let context = ApiRequestContext {
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..attributed_context()
        };
        // Simulate the checked Windows observer's complete negative answer, not
        // native Windows process behavior. Both real endpoint pins still validate.
        let response = crate::platform::with_ancestry_membership_for_test(Some(false), || {
            assert_eq!(
                fixture
                    .app
                    .input_origin_for_peer_identity(context.local_peer_identity.expect("peer")),
                InputOrigin::Ordinary
            );
            fixture
                .app
                .handle_api_request_with_context(request.clone(), context)
        });
        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("ordinary shell bytes"),
            Bytes::from_static(b"ordinary")
        );
        assert!(fixture.source_rx.try_recv().is_err());

        // Incomplete/reused-edge observation is not a negative membership answer.
        let response = crate::platform::with_ancestry_membership_for_test(None, || {
            fixture
                .app
                .handle_api_request_with_context(request.clone(), context)
        });
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(0);
        let response = crate::platform::with_ancestry_membership_for_test(Some(false), || {
            fixture
                .app
                .handle_api_request_with_context(request, context)
        });
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn captured_markers_require_live_pane_link_and_never_select_a_public_pane() {
        let mut fixture = attributed_agent_fixture();
        let mut context = attributed_context();
        context.local_peer_pane_origin = crate::platform::PeerPaneOrigin::Unknown;
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Unknown
        );
        #[cfg(target_os = "macos")]
        assert!(matches!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Agent(_)
        ));
        context.local_peer_pane_origin = crate::platform::PeerPaneOrigin::HasPane;
        assert!(matches!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Agent(_)
        ));
        let (_, source) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source");
        let terminal = fixture.app.state.workspaces[0]
            .terminal_id(source)
            .cloned()
            .expect("source terminal");
        let source = fixture
            .app
            .state
            .terminals
            .get_mut(&terminal)
            .expect("source state");
        source.clear_agent_name();
        source.set_detected_state(None, AgentState::Unknown);
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Ordinary
        );
        let (_, target) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target)
            .expect("target runtime")
            .test_set_child_pid(std::process::id());
        let peer = context.local_peer_identity.expect("live captured peer");
        // Both runtime roots and peer stay live, but no pane relationship can be
        // established. An otherwise positive Windows outside proof cannot turn
        // this captured marked caller into ordinary origin.
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            crate::platform::with_ancestry_membership_for_test(Some(false), || {
                assert_eq!(
                    fixture.app.input_origin_for_context(context),
                    InputOrigin::Unknown
                );
                let request = Request {
                    id: "unlinked-marker".into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: fixture.target_pane_id.clone(),
                        text: "blocked".into(),
                        allow_cross_pane: false,
                    }),
                };
                assert_unknown(
                    &fixture
                        .app
                        .handle_api_request_with_context(request, context),
                );
                let (respond_to, response) = std::sync::mpsc::channel();
                assert!(fixture.app.handle_deferred_agent_api_request(
                    Request {
                        id: "deferred-unlinked-marker".into(),
                        method: Method::AgentPrompt(AgentPromptParams {
                            expected_agent_session_id: None,
                            expected_pane_id: None,
                            expected_agent_status: None,
                            target: "target-agent".into(),
                            text: "blocked".into(),
                            wait: None,
                            allow_cross_pane: false,
                        })
                    },
                    context,
                    respond_to,
                ));
                assert_unknown(&response.recv().expect("deferred unknown"));
                assert!(fixture.source_rx.try_recv().is_err());
                assert!(fixture.target_rx.try_recv().is_err());
            });
        });
    }

    #[tokio::test]
    async fn unknown_pane_origin_recovery_only_returns_a_proven_agent_link() {
        let mut fixture = attributed_agent_fixture();
        let peer = attributed_context().local_peer_identity.expect("live peer");

        // This is the portable seam used by Darwin's protected-executable path.
        assert!(matches!(
            fixture.app.input_origin_for_unknown_pane_origin(peer),
            InputOrigin::Agent(_)
        ));

        // A linked non-agent is not ordinary when the environment is unknown.
        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source pane");
        let source_terminal_id = fixture.app.state.workspaces[0]
            .terminal_id(source_pane)
            .cloned()
            .expect("source terminal");
        let source_terminal = fixture
            .app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state");
        source_terminal.clear_agent_name();
        source_terminal.set_detected_state(None, AgentState::Unknown);
        assert_eq!(
            fixture.app.input_origin_for_unknown_pane_origin(peer),
            InputOrigin::Unknown
        );

        // Missing roots, stale peers, and an unlinked peer are all non-proofs;
        // outside-server evidence must not turn this recovery into Ordinary.
        let (_, target_pane) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target pane");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(0);
        assert_eq!(
            fixture.app.input_origin_for_unknown_pane_origin(peer),
            InputOrigin::Unknown
        );
        let stale = crate::platform::ProcessIdentity {
            start_time: peer.start_time.wrapping_add(1),
            ..peer
        };
        assert_eq!(
            fixture.app.input_origin_for_unknown_pane_origin(stale),
            InputOrigin::Unknown
        );
    }

    fn exercise_server_ancestry_sequence(observation: Option<bool>) {
        let mut fixture = attributed_agent_fixture();
        // The ancestry sequence seam explicitly models a marker-free caller.
        let context = ApiRequestContext {
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..attributed_context()
        };
        let peer = context.local_peer_identity.expect("live pinned peer");
        crate::platform::with_server_ancestry_for_test(peer, observation, || {
            let expected = match observation {
                Some(true) => InputOrigin::Ordinary,
                Some(false) => InputOrigin::Agent(
                    fixture
                        .app
                        .resolve_terminal_target(&fixture.source_pane_id)
                        .expect("source"),
                ),
                None => InputOrigin::Unknown,
            };
            assert_eq!(fixture.app.input_origin_for_peer_identity(peer), expected);
            for (own, opt_in) in [(false, false), (true, false), (false, true)] {
                let pane_id = if own {
                    fixture.source_pane_id.clone()
                } else {
                    fixture.target_pane_id.clone()
                };
                let response = fixture.app.handle_api_request_with_context(
                    Request {
                        id: "server-ancestry-sequence".into(),
                        method: Method::PaneSendText(PaneSendTextParams {
                            expected_agent_session_id: None,
                            pane_id,
                            text: "sequence".into(),
                            allow_cross_pane: opt_in,
                        }),
                    },
                    context,
                );
                let allowed =
                    opt_in || observation == Some(true) || (own && observation == Some(false));
                if allowed {
                    assert_ok(&response);
                    let rx = if own {
                        &mut fixture.source_rx
                    } else {
                        &mut fixture.target_rx
                    };
                    assert_eq!(
                        rx.try_recv().expect("approved delivery"),
                        Bytes::from_static(b"sequence")
                    );
                } else if observation.is_none() {
                    assert_unknown(&response);
                } else {
                    assert_denied(&response);
                }
                assert!(fixture.source_rx.try_recv().is_err());
                assert!(fixture.target_rx.try_recv().is_err());
            }

            let (respond_to, response_rx) = std::sync::mpsc::channel();
            assert!(fixture.app.handle_deferred_agent_api_request(
                Request {
                    id: "deferred-server-ancestry-sequence".into(),
                    method: Method::AgentPrompt(AgentPromptParams {
                        expected_agent_session_id: None,
                        expected_pane_id: None,
                        expected_agent_status: None,
                        target: fixture.target_pane_id.clone(),
                        text: "deferred".into(),
                        wait: None,
                        allow_cross_pane: false,
                    }),
                },
                context,
                respond_to,
            ));
            if observation != Some(true) {
                let response = response_rx.recv().expect("deferred refusal");
                if observation.is_none() {
                    assert_unknown(&response);
                } else {
                    assert_denied(&response);
                }
                assert!(fixture.target_rx.try_recv().is_err());
            }
        });
    }

    #[tokio::test]
    async fn server_ancestry_policy_preserves_ordinary_agent_and_unknown_guard_outcomes() {
        for observation in [Some(true), Some(false), None] {
            exercise_server_ancestry_sequence(observation);
        }
    }

    #[tokio::test]
    async fn portable_darwin_chronology_sequences_feed_actual_attribution_and_guard() {
        for sequence in 0..=15 {
            let observation = crate::platform::test_server_chronology_sequence(sequence);
            let expected = match sequence {
                0 | 2 => Some(true),
                1 => Some(false),
                _ => None,
            };
            assert_eq!(observation, expected, "chronology sequence {sequence}");
            exercise_server_ancestry_sequence(observation);
        }
    }

    #[tokio::test]
    async fn marker_free_outside_proof_precedes_missing_roots_but_failed_evidence_refuses_delivery()
    {
        let mut fixture = attributed_agent_fixture();
        let context = ApiRequestContext {
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..attributed_context()
        };
        let peer = context.local_peer_identity.expect("live caller");
        // An older-than-server witness proves outside ancestry without requiring
        // visibility of any pane root. Reached-server is not that exemption.
        for pane in [&fixture.source_pane_id, &fixture.target_pane_id] {
            let (_, pane) = fixture.app.parse_pane_id(pane).expect("pane");
            fixture
                .app
                .state
                .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, pane)
                .expect("runtime")
                .test_set_child_pid(0);
        }
        let request = Request {
            id: "outside-missing-roots".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "ordinary outside".into(),
                allow_cross_pane: false,
            }),
        };
        for sequence in [0, 1, 3] {
            let observation = crate::platform::test_server_chronology_sequence(sequence);
            crate::platform::with_server_ancestry_for_test(peer, observation, || {
                let response = fixture
                    .app
                    .handle_api_request_with_context(request.clone(), context);
                if observation == Some(true) {
                    assert_ok(&response);
                    assert_eq!(
                        fixture
                            .target_rx
                            .try_recv()
                            .expect("default ordinary delivery"),
                        Bytes::from_static(b"ordinary outside")
                    );
                } else {
                    assert_unknown(&response);
                }
                assert!(fixture.source_rx.try_recv().is_err());
                assert!(fixture.target_rx.try_recv().is_err());
            });
        }
        // Captured markers are checked before outside/age evidence. Neither a
        // marked orphan nor unreadable markers may take the ordinary exemption.
        crate::platform::with_server_ancestry_for_test(
            peer,
            crate::platform::test_server_chronology_sequence(0),
            || {
                for marker in [
                    crate::platform::PeerPaneOrigin::HasPane,
                    crate::platform::PeerPaneOrigin::Unknown,
                ] {
                    let marked = ApiRequestContext {
                        local_peer_pane_origin: marker,
                        ..context
                    };
                    assert_unknown(
                        &fixture
                            .app
                            .handle_api_request_with_context(request.clone(), marked),
                    );
                    assert!(fixture.source_rx.try_recv().is_err());
                    assert!(fixture.target_rx.try_recv().is_err());
                    let mut explicit = request.clone();
                    if let Method::PaneSendText(params) = &mut explicit.method {
                        params.allow_cross_pane = true;
                    }
                    assert_ok(
                        &fixture
                            .app
                            .handle_api_request_with_context(explicit, marked),
                    );
                    assert_eq!(
                        fixture
                            .target_rx
                            .try_recv()
                            .expect("explicit marked opt-in"),
                        Bytes::from_static(b"ordinary outside")
                    );
                }
            },
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_outside_proof_ignores_one_failed_pane_root_but_marked_agent_stays_linked() {
        let mut fixture = attributed_agent_fixture();
        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source pane");
        let (_, target_pane) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target pane");
        // A third restored pane has no runtime. Both managed panes stay healthy.
        fixture.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        fixture.app.state.ensure_test_terminals();
        let target_root = detached_sleep_child();
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(target_root.id());
        // Spawn can return before /proc exposes the exec-installed environment.
        // Wait for the fixture's actual marker state before freezing its context.
        let ready_context = |child: &mut GuardTestChild, expected| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                assert!(child.try_wait().expect("fixture child liveness").is_none());
                let context = ApiRequestContext::for_local_peer_pid(Some(child.id()));
                if context.local_peer_identity.is_some()
                    && context.local_peer_pane_origin == expected
                {
                    break context;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture marker readiness timed out: expected {expected:?}, got {context:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let mut marked_child = GuardTestChild(
            std::process::Command::new("sleep")
                .arg("60")
                .env("HERDR_ENV", "1")
                .env("HERDR_PANE_ID", &fixture.source_pane_id)
                .spawn()
                .expect("actual marked agent child"),
        );
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, source_pane)
            .expect("healthy agent runtime")
            .test_set_child_pid(marked_child.id());
        let marked_context =
            ready_context(&mut marked_child, crate::platform::PeerPaneOrigin::HasPane);
        assert_eq!(
            marked_context.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::HasPane
        );
        let peer = marked_context
            .local_peer_identity
            .expect("live marked caller");
        let server = crate::platform::process_identity(std::process::id()).expect("server");
        let mut ordinary_peer =
            crate::platform::parent_process_identity(server).expect("real parent");
        // A freshly spawned runner can share the server's /proc start-time tick.
        // Use a real older ancestor, not an assumed strictly older direct parent.
        for _ in 0..64 {
            if ordinary_peer.start_time < server.start_time {
                break;
            }
            ordinary_peer = crate::platform::parent_process_identity(ordinary_peer)
                .expect("real older ancestor");
        }
        assert!(ordinary_peer.start_time < server.start_time);
        let request = |id: &str, pane_id: String, text: &str| Request {
            id: id.into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id,
                text: text.into(),
                allow_cross_pane: false,
            }),
        };
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            // A marked live agent remains attributable to its actual healthy
            // source pane; outside proof never grants cross-pane delivery.
            {
                let marked = marked_context;
                let own = fixture.app.handle_api_request_with_context(
                    request("marked-own", fixture.source_pane_id.clone(), "own"),
                    marked,
                );
                assert_ok(&own);
                assert_eq!(
                    fixture.source_rx.try_recv().expect("own delivery"),
                    Bytes::from_static(b"own")
                );
                let denied = fixture.app.handle_api_request_with_context(
                    request("marked-cross", fixture.target_pane_id.clone(), "cross"),
                    marked,
                );
                assert_denied(&denied);
                assert!(fixture.target_rx.try_recv().is_err());
            }

            // Check the native adapter against a real strictly older ancestor.
            assert_eq!(
                crate::platform::process_identity_server_ancestry(ordinary_peer),
                crate::platform::ServerAncestry::Outside
            );
            // Capture readable absence from an actual sanitized exec. Model its
            // older-server ancestry separately so this test remains valid when
            // nextest itself inherited pane markers from the enclosing harness.
            let mut ordinary_child = GuardTestChild(
                std::process::Command::new("sleep")
                    .arg("60")
                    .env_remove("HERDR_ENV")
                    .env_remove("HERDR_PANE_ID")
                    .spawn()
                    .expect("marker-free ordinary child"),
            );
            let ordinary =
                ready_context(&mut ordinary_child, crate::platform::PeerPaneOrigin::Absent);
            assert_eq!(
                ordinary.local_peer_pane_origin,
                crate::platform::PeerPaneOrigin::Absent
            );
            let response = crate::platform::with_server_ancestry_for_test(
                ordinary.local_peer_identity.expect("ordinary pin"),
                Some(true),
                || {
                    fixture.app.handle_api_request_with_context(
                        request(
                            "ordinary-failed-root",
                            fixture.target_pane_id.clone(),
                            "ordinary",
                        ),
                        ordinary,
                    )
                },
            );
            assert_ok(&response);
            assert_eq!(
                fixture.target_rx.try_recv().expect("ordinary delivery"),
                Bytes::from_static(b"ordinary")
            );

            // Unknown marker evidence cannot use the same outside proof.
            let unknown = ApiRequestContext {
                local_peer_pane_origin: crate::platform::PeerPaneOrigin::Unknown,
                local_peer_identity: Some(peer),
            };
            let response = fixture.app.handle_api_request_with_context(
                request(
                    "unknown-failed-root",
                    fixture.target_pane_id.clone(),
                    "blocked",
                ),
                unknown,
            );
            assert_unknown(&response);
            assert!(fixture.target_rx.try_recv().is_err());

            // Losing the real pane link cannot promote the marked caller even
            // with Outside observation available for that exact live peer.
            fixture
                .app
                .state
                .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, source_pane)
                .expect("source runtime")
                .test_set_child_pid(0);
            let response = fixture.app.handle_api_request_with_context(
                request("marked-orphan", fixture.target_pane_id.clone(), "blocked"),
                marked_context,
            );
            assert_unknown(&response);
            assert!(fixture.source_rx.try_recv().is_err());
            assert!(fixture.target_rx.try_recv().is_err());
        });
    }

    #[tokio::test]
    async fn stale_peer_cannot_use_outside_age_exemption_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let live = crate::platform::process_identity(std::process::id()).expect("live caller");
        let stale = crate::platform::ProcessIdentity {
            start_time: live.start_time.wrapping_add(1),
            ..live
        };
        let context = ApiRequestContext {
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..ApiRequestContext::capture(Some(stale))
        };
        crate::platform::with_server_ancestry_for_test(stale, Some(true), || {
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: "stale-outside-proof".into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: fixture.target_pane_id.clone(),
                        text: "blocked".into(),
                        allow_cross_pane: false,
                    }),
                },
                context,
            );
            assert_unknown(&response);
            assert!(fixture.source_rx.try_recv().is_err());
            assert!(fixture.target_rx.try_recv().is_err());
        });
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_snapshot_sequences_feed_actual_attribution_and_guard() {
        for (sequence, expected) in [
            (0, Some(true)),
            (1, None),
            (2, None),
            (3, Some(false)),
            (4, None),
        ] {
            let observation = crate::platform::test_outside_server_ancestry_sequence(sequence);
            assert_eq!(observation, expected, "sequence {sequence}");
            exercise_server_ancestry_sequence(observation);
        }
    }

    // This in-process App fixture makes the caller a child of the server itself.
    // Darwin's outside-server path is covered by the separate-server api_ping
    // integrations, not by treating this different topology as outside.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn native_accepted_ordinary_child_requires_default_guard_delivery() {
        use std::io::BufRead;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};

        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut fixture = attributed_agent_fixture();
        let mut roots = Vec::new();
        for pane in [&fixture.source_pane_id, &fixture.target_pane_id] {
            let (_, pane) = fixture.app.parse_pane_id(pane).expect("pane");
            let root = detached_sleep_child();
            fixture
                .app
                .state
                .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, pane)
                .expect("runtime")
                .test_set_child_pid(root.0.id());
            roots.push(root);
        }
        struct OwnedDirectory(std::path::PathBuf);
        impl Drop for OwnedDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        use std::os::unix::fs::DirBuilderExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let directory =
            std::path::PathBuf::from(format!("/tmp/hdg-{}-{nonce}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .expect("exclusive short native socket directory");
        let directory = OwnedDirectory(directory);
        let socket = directory.0.join("s");
        let listener = UnixListener::bind(&socket).expect("native listener");
        listener.set_nonblocking(true).expect("bounded accept");
        let request = Request {
            id: "darwin-real-accepted-caller".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "native ordinary".into(),
                allow_cross_pane: false,
            }),
        };
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall((sys.argv[2]+'\\n').encode()); sys.stdin.readline()",
        ])
            .arg(&socket)
            .arg(serde_json::to_string(&request).expect("child request"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        for key in [
            "HERDR_ENV",
            "HERDR_PANE_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_TAB_ID",
        ] {
            command.env_remove(key);
        }
        let mut caller = OwnedChild(command.spawn().expect("native socket caller"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(caller.0.try_wait().expect("caller liveness").is_none());
                    assert!(
                        std::time::Instant::now() < deadline,
                        "native accept deadline"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("native accept: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("blocking request stream");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("request deadline");
        // Match the production accept path: capture generation-bound transport
        // identity first, then initial marker provenance. Never fall back to PID.
        let context = ApiRequestContext::capture(crate::platform::local_socket_peer_identity(
            stream.as_raw_fd(),
        ));
        let peer = context
            .local_peer_identity
            .expect("native accepted transport identity");
        assert_eq!(peer.pid, caller.0.id());
        assert_eq!(crate::platform::process_identity(peer.pid), Some(peer));
        assert_eq!(
            context.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::Absent
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            crate::platform::process_identity_server_ancestry(peer),
            crate::platform::ServerAncestry::ReachedServer
        );
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .expect("native request bytes");
        let request: Request = serde_json::from_str(&line).expect("native child request JSON");
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Ordinary
        );
        assert_ok(
            &fixture
                .app
                .handle_api_request_with_context(request, context),
        );
        assert_eq!(
            fixture
                .target_rx
                .try_recv()
                .expect("default native ordinary delivery"),
            Bytes::from_static(b"native ordinary")
        );
        assert!(fixture.source_rx.try_recv().is_err());
        assert_eq!(crate::platform::process_identity(peer.pid), Some(peer));
        assert_eq!(roots.len(), 2, "both pane roots retained through dispatch");
    }

    #[cfg(windows)]
    struct WindowsRealChild {
        child: std::process::Child,
        pid: u32,
        request_line: String,
    }

    #[cfg(windows)]
    impl WindowsRealChild {
        fn spawn(request: &str) -> Self {
            Self::spawn_with_pane(request, None)
        }

        fn spawn_with_pane(request: &str, pane: Option<&str>) -> Self {
            use std::io::BufRead;
            use std::process::{Command, Stdio};

            let mut command = Command::new("powershell.exe");
            command.args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$p=$PID; [Console]::Out.WriteLine('{\"pid\":'+$p+'}'); [Console]::Out.WriteLine($env:HERDR_GUARD_TEST_REQUEST); [Console]::Out.Flush(); Start-Sleep -Seconds 60",
                ])
                .env("HERDR_GUARD_TEST_REQUEST", request)
                // Retain the owned stdin pipe alongside the live child handle.
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            for key in [
                "HERDR_ENV",
                "HERDR_PANE_ID",
                "HERDR_WORKSPACE_ID",
                "HERDR_TAB_ID",
            ] {
                command.env_remove(key);
            }
            if let Some(pane) = pane {
                command.env("HERDR_ENV", "1").env("HERDR_PANE_ID", pane);
            }
            let child = command.spawn().expect("spawn real Windows helper child");
            // Install cleanup before reading/asserting anything about the child.
            let mut owned = Self {
                pid: child.id(),
                child,
                request_line: String::new(),
            };
            let pid = {
                let stdout = owned.child.stdout.as_mut().expect("helper stdout");
                let mut reader = std::io::BufReader::new(stdout);
                let mut line = String::new();
                reader.read_line(&mut line).expect("read helper identity");
                reader
                    .read_line(&mut owned.request_line)
                    .expect("read child JSON request");
                serde_json::from_str::<serde_json::Value>(&line)
                    .expect("helper identity JSON")
                    .get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok())
                    .expect("helper PID")
            };
            assert_eq!(owned.pid, pid, "stdout PID must be the spawned child");
            assert_eq!(
                crate::platform::process_identity(pid).map(|identity| identity.pid),
                Some(pid)
            );
            owned
        }
    }

    #[cfg(windows)]
    impl Drop for WindowsRealChild {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_non_pane_windows_child_uses_os_identity_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let server =
            crate::platform::process_identity(std::process::id()).expect("live actual server");
        let source_root = WindowsRealChild::spawn("{}");
        let target_root = WindowsRealChild::spawn("{}");
        // The actual server is a live strictly older ancestor than either pane
        // root, so sibling-root exclusion has a controlled chronology witness.
        for pid in [source_root.pid, target_root.pid] {
            assert!(
                crate::platform::process_identity(pid)
                    .expect("live pane root")
                    .start_time
                    > server.start_time
            );
        }
        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source pane");
        let (_, target_pane) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target pane");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, source_pane)
            .expect("source runtime")
            .test_set_child_pid(source_root.pid);
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(target_root.pid);

        let request = Request {
            id: "windows-real-non-pane-child".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "windows real child".into(),
                allow_cross_pane: false,
            }),
        };
        let mut caller = WindowsRealChild::spawn(
            &serde_json::to_string(&request).expect("serialize child request"),
        );
        let request: Request =
            serde_json::from_str(&caller.request_line).expect("parse child's JSON request");
        let context = ApiRequestContext::for_local_peer_pid(Some(caller.pid));
        let peer = context
            .local_peer_identity
            .expect("live OS caller identity");
        assert_ne!(peer.pid, source_root.pid);
        assert_ne!(peer.pid, target_root.pid);
        assert_eq!(
            context.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::Absent
        );
        assert_eq!(
            crate::platform::process_identity_server_ancestry(peer),
            crate::platform::ServerAncestry::ReachedServer
        );
        assert!(caller
            .child
            .try_wait()
            .expect("check caller liveness")
            .is_none());
        let response = fixture
            .app
            .handle_api_request_with_context(request.clone(), context);
        let origin = fixture
            .app
            .input_origin_for_peer_identity(context.local_peer_identity.expect("caller identity"));
        assert_eq!(
            origin,
            InputOrigin::Ordinary,
            "live older-server sibling exclusion must be positive"
        );
        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("ordinary child bytes"),
            Bytes::from_static(b"windows real child")
        );
        assert!(fixture.source_rx.try_recv().is_err());

        let explicit = match request.method {
            Method::PaneSendText(mut params) => {
                params.allow_cross_pane = true;
                Request {
                    id: "windows-real-non-pane-child-opt-in".into(),
                    method: Method::PaneSendText(params),
                }
            }
            _ => unreachable!("fixed test request method"),
        };
        let response = fixture
            .app
            .handle_api_request_with_context(explicit, context);
        assert_ok(&response);
        assert_eq!(
            fixture.target_rx.try_recv().expect("explicit opt-in bytes"),
            Bytes::from_static(b"windows real child")
        );
        assert!(fixture.source_rx.try_recv().is_err());

        // Keep all three owned helpers alive until both normal dispatches have
        // completed; their identities were captured from their live OS processes.
        assert_eq!(crate::platform::process_identity(peer.pid), Some(peer));
        assert!(caller
            .child
            .try_wait()
            .expect("caller still alive")
            .is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn marked_windows_source_with_live_sibling_root_keeps_same_cross_and_opt_in_policy() {
        let mut fixture = attributed_agent_fixture();
        let source = WindowsRealChild::spawn_with_pane("{}", Some(&fixture.source_pane_id));
        let target = WindowsRealChild::spawn_with_pane("{}", Some(&fixture.target_pane_id));
        let server =
            crate::platform::process_identity(std::process::id()).expect("live actual server");
        for (pane, pid) in [
            (&fixture.source_pane_id, source.pid),
            (&fixture.target_pane_id, target.pid),
        ] {
            assert!(
                crate::platform::process_identity(pid)
                    .expect("live pane root")
                    .start_time
                    > server.start_time
            );
            let (_, pane) = fixture.app.parse_pane_id(pane).expect("pane");
            fixture
                .app
                .state
                .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, pane)
                .expect("runtime")
                .test_set_child_pid(pid);
        }
        let context = ApiRequestContext::for_local_peer_pid(Some(source.pid));
        assert_eq!(
            context.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::HasPane
        );
        assert!(matches!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Agent(_)
        ));
        for (own, opt_in, allowed) in [
            (true, false, true),
            (false, false, false),
            (false, true, true),
        ] {
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: "windows-two-live-roots".into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: if own {
                            fixture.source_pane_id.clone()
                        } else {
                            fixture.target_pane_id.clone()
                        },
                        text: "live roots".into(),
                        allow_cross_pane: opt_in,
                    }),
                },
                context,
            );
            if allowed {
                assert_ok(&response);
                let rx = if own {
                    &mut fixture.source_rx
                } else {
                    &mut fixture.target_rx
                };
                assert_eq!(
                    rx.try_recv().expect("approved input"),
                    Bytes::from_static(b"live roots")
                );
            } else {
                assert_denied(&response);
            }
            assert!(fixture.source_rx.try_recv().is_err());
            assert!(fixture.target_rx.try_recv().is_err());
        }
        let (respond_to, response) = std::sync::mpsc::channel();
        assert!(fixture.app.handle_deferred_agent_api_request(
            Request {
                id: "windows-two-live-roots-deferred".into(),
                method: Method::AgentPrompt(AgentPromptParams {
                    expected_agent_session_id: None,
                    expected_pane_id: None,
                    expected_agent_status: None,
                    target: "target-agent".into(),
                    text: "blocked".into(),
                    wait: None,
                    allow_cross_pane: false,
                })
            },
            context,
            respond_to,
        ));
        assert_denied(&response.recv().expect("deferred cross-pane refusal"));
        assert!(fixture.target_rx.try_recv().is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn reused_windows_intermediate_parent_is_unknown_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let observation = crate::platform::test_reused_intermediate_parent_membership();
        assert_eq!(observation, None);
        let request = Request {
            id: "reused-windows-parent".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "refused".into(),
                allow_cross_pane: false,
            }),
        };
        let response = crate::platform::with_checked_membership_for_test(observation, || {
            fixture
                .app
                .handle_api_request_with_context(request, attributed_context())
        });
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        assert!(fixture.source_rx.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unsupported_pidfd_uses_accept_time_pin_through_actual_guard() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut fixture = attributed_agent_fixture();
        let (mut server, client) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let context = ApiRequestContext::capture(
            crate::platform::local_socket_peer_identity_without_pidfd(server.as_raw_fd()),
        );
        assert_eq!(
            context.local_peer_identity,
            attributed_context().local_peer_identity
        );
        assert!(context.local_peer_identity.is_some());
        for (pane, opt_in, permitted) in [
            (fixture.target_pane_id.clone(), false, false),
            (fixture.source_pane_id.clone(), false, true),
            (fixture.target_pane_id.clone(), true, true),
        ] {
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: "fallback".into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        expected_agent_session_id: None,
                        pane_id: pane.clone(),
                        text: "fallback".into(),
                        allow_cross_pane: opt_in,
                    }),
                },
                context,
            );
            if permitted {
                assert_ok(&response);
                let rx = if pane == fixture.source_pane_id {
                    &mut fixture.source_rx
                } else {
                    &mut fixture.target_rx
                };
                assert_eq!(
                    rx.try_recv().expect("approved delivery"),
                    Bytes::from_static(b"fallback")
                );
            } else {
                assert_denied(&response);
                assert!(fixture.target_rx.try_recv().is_err());
            }
        }
        drop(client);
        // Parallel child spawns can hold a fork-inherited CLOEXEC client fd until
        // exec. Dropping our copy is not proof of disconnect: wait for actual EOF.
        server.set_nonblocking(true).expect("nonblocking EOF probe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match server.read(&mut [0]) {
                Ok(0) => break,
                Ok(_) => panic!("unexpected data on unused peer socket"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("peer disconnect probe: {error}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "peer socket did not disconnect after dropping client"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let missing = ApiRequestContext::capture(
            crate::platform::local_socket_peer_identity_without_pidfd(server.as_raw_fd()),
        );
        assert_eq!(missing.local_peer_identity, None);
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "unsupported-and-disconnected".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "refused".into(),
                    allow_cross_pane: false,
                }),
            },
            missing,
        );
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn captured_peer_exit_before_queued_dispatch_refuses_input() {
        let mut fixture = attributed_agent_fixture();
        let mut child = detached_sleep_child();
        let context = ApiRequestContext::for_local_peer_pid(Some(child.id()));
        assert!(context.local_peer_identity.is_some());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (respond_to, _response) = std::sync::mpsc::channel();
        tx.send(crate::api::ApiRequestMessage {
            request: Request {
                id: "queued-peer-exit".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    expected_agent_session_id: None,
                    pane_id: fixture.target_pane_id.clone(),
                    text: "refused".into(),
                    allow_cross_pane: false,
                }),
            },
            context,
            respond_to,
            response_write_complete: None,
        })
        .expect("queued request");
        child.kill().expect("stop peer");
        child.wait().expect("reap peer");
        let queued = rx.try_recv().expect("receive after exit");
        assert_eq!(queued.context, context);
        let response = fixture
            .app
            .handle_api_request_with_context(queued.request.clone(), queued.context);
        assert_unknown(&response);
        assert!(fixture.target_rx.try_recv().is_err());
        let mut explicit = queued.request;
        if let Method::PaneSendText(params) = &mut explicit.method {
            params.allow_cross_pane = true;
        }
        assert_ok(
            &fixture
                .app
                .handle_api_request_with_context(explicit, context),
        );
        assert_eq!(
            fixture.target_rx.try_recv().expect("explicit opt-in"),
            Bytes::from_static(b"refused")
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct OrphanTree {
        root: GuardTestChild,
        output: std::io::BufReader<std::process::ChildStdout>,
        peer: crate::platform::ProcessIdentity,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl OrphanTree {
        fn spawn(pane: &str) -> Self {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let script = r#"
import os, signal, subprocess, sys
assert os.getsid(os.getpid()) == os.getpid(), 'pane root must lead its own session'
middle_script = r'''
import os, subprocess, sys
p = None
try:
    env = dict(os.environ, HERDR_GUARD_EXEC_HELPER='marked')
    p = subprocess.Popen([sys.argv[1], '--exact', 'app::api::input_guard::tests::guard_exec_peer_helper', '--nocapture'], env=env, stdout=subprocess.PIPE, text=True, start_new_session=True)
    for line in p.stdout:
        if 'guard-exec-ready ' in line:
            assert int(line.rsplit('guard-exec-ready ', 1)[1]) == p.pid
            break
    else:
        raise RuntimeError('exec marker readiness missing')
    print(p.pid, flush=True)
    sys.stdin.readline()
finally:
    if p is not None:
        p.terminate()
        p.wait()
'''
middle = subprocess.Popen([sys.executable, '-c', middle_script, sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
peer = int(middle.stdout.readline())
try:
    print(peer, os.getsid(os.getpid()), os.getsid(peer), flush=True)
    if sys.stdin.readline().strip() == 'orphan':
        middle.terminate()
        middle.wait()
        print('gone', flush=True)
        sys.stdin.readline()
finally:
    try: os.kill(peer, signal.SIGTERM)
    except ProcessLookupError: pass
    if middle.poll() is None: middle.terminate()
    middle.wait()
"#;
            let mut command = Command::new("python3");
            command
                .args(["-c", script])
                .arg(std::env::current_exe().expect("test helper executable"))
                .env("HERDR_ENV", "1")
                .env("HERDR_PANE_ID", pane)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            crate::platform::detach_server_daemon_command(&mut command);
            let mut root = GuardTestChild(
                command
                    .spawn()
                    .expect("owned pane root and detached child tree"),
            );
            let output = std::io::BufReader::new(root.stdout.take().expect("root stdout"));
            // Cleanup is installed before reading, parsing, or asserting stdout.
            let mut owned = Self {
                root,
                output,
                peer: crate::platform::ProcessIdentity {
                    pid: 0,
                    start_time: 0,
                },
            };
            let mut line = String::new();
            owned
                .output
                .read_line(&mut line)
                .expect("peer PID and real session identities");
            let ids = line
                .split_whitespace()
                .map(|value| value.parse::<u32>().expect("numeric identity"))
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 3, "peer/root SID/peer SID");
            assert_eq!(ids[1], owned.root.id(), "root is an actual session leader");
            assert_eq!(
                ids[2], ids[0],
                "detached peer leads its own different session"
            );
            assert_ne!(ids[1], ids[2]);
            owned.peer = crate::platform::process_identity(ids[0]).expect("live peer");
            owned
        }

        fn orphan(&mut self) {
            use std::io::{BufRead, Write};
            writeln!(self.root.stdin.as_mut().expect("root control"), "orphan")
                .expect("exit intermediate");
            let mut line = String::new();
            self.output
                .read_line(&mut line)
                .expect("intermediate exit receipt");
            assert_eq!(line.trim(), "gone");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for OrphanTree {
        fn drop(&mut self) {
            use std::io::Write;
            if let Some(input) = self.root.stdin.as_mut() {
                let _ = writeln!(input, "cleanup");
            }
            let _ = self.root.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn marked_detached_orphan_never_becomes_ordinary_in_normal_or_deferred_dispatch() {
        let mut fixture = attributed_agent_fixture();
        let mut tree = OrphanTree::spawn(&fixture.source_pane_id);
        let (_, source_pane) = fixture
            .app
            .parse_pane_id(&fixture.source_pane_id)
            .expect("source");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, source_pane)
            .expect("source runtime")
            .test_set_child_pid(tree.root.id());
        let mut target_root = detached_sleep_child();
        let (_, target_pane) = fixture
            .app
            .parse_pane_id(&fixture.target_pane_id)
            .expect("target");
        fixture
            .app
            .state
            .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, target_pane)
            .expect("target runtime")
            .test_set_child_pid(target_root.id());
        let root = crate::platform::process_identity(tree.root.id()).expect("live root");
        let intermediate =
            crate::platform::parent_process_identity(tree.peer).expect("live intermediate");
        let accepted = ApiRequestContext::for_local_peer_pid(Some(tree.peer.pid));
        assert_eq!(
            accepted.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::HasPane
        );
        assert!(matches!(
            fixture.app.input_origin_for_context(accepted),
            InputOrigin::Agent(_)
        ));
        let own = Request {
            id: "marked-own-before-orphan".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.source_pane_id.clone(),
                text: "own".into(),
                allow_cross_pane: false,
            }),
        };
        assert_ok(&fixture.app.handle_api_request_with_context(own, accepted));
        assert_eq!(
            fixture.source_rx.try_recv().expect("own bytes"),
            Bytes::from_static(b"own")
        );
        let request = Request {
            id: "queued-marked-orphan".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "orphan".into(),
                allow_cross_pane: false,
            }),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (respond_to, _) = std::sync::mpsc::channel();
        tx.send(crate::api::ApiRequestMessage {
            request,
            context: accepted,
            respond_to,
            response_write_complete: None,
        })
        .expect("queued before intermediate exit");
        tree.orphan();
        assert_eq!(crate::platform::process_identity(root.pid), Some(root));
        assert_eq!(
            crate::platform::process_identity(tree.peer.pid),
            Some(tree.peer)
        );
        assert_ne!(
            crate::platform::parent_process_identity(tree.peer),
            Some(intermediate)
        );
        let queued = rx.try_recv().expect("dispatch after reparenting");
        let after_orphan = ApiRequestContext::for_local_peer_pid(Some(tree.peer.pid));
        assert_eq!(
            after_orphan.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::HasPane
        );
        for context in [queued.context, after_orphan] {
            assert_eq!(
                fixture.app.input_origin_for_context(context),
                InputOrigin::Unknown
            );
            assert_unknown(
                &fixture
                    .app
                    .handle_api_request_with_context(queued.request.clone(), context),
            );
            let (respond_to, response) = std::sync::mpsc::channel();
            assert!(fixture.app.handle_deferred_agent_api_request(
                Request {
                    id: "deferred-marked-orphan".into(),
                    method: Method::AgentPrompt(AgentPromptParams {
                        expected_agent_session_id: None,
                        expected_pane_id: None,
                        expected_agent_status: None,
                        target: "target-agent".into(),
                        text: "orphan".into(),
                        wait: None,
                        allow_cross_pane: false,
                    })
                },
                context,
                respond_to,
            ));
            assert_unknown(&response.recv().expect("deferred refusal"));
            assert!(fixture.target_rx.try_recv().is_err());
            assert!(fixture.source_rx.try_recv().is_err());
        }
        let mut explicit = queued.request;
        if let Method::PaneSendText(params) = &mut explicit.method {
            params.allow_cross_pane = true;
        }
        assert_ok(
            &fixture
                .app
                .handle_api_request_with_context(explicit, accepted),
        );
        assert_eq!(
            fixture
                .target_rx
                .try_recv()
                .expect("explicit orphan opt-in"),
            Bytes::from_static(b"orphan")
        );
        let _ = target_root.kill();
        let _ = target_root.wait();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn validated_outside_shell_is_ordinary_but_missing_roots_are_unknown() {
        let mut fixture = attributed_agent_fixture();
        let mut roots = Vec::new();
        for pane in [&fixture.source_pane_id, &fixture.target_pane_id] {
            let (_, pane) = fixture.app.parse_pane_id(pane).expect("pane");
            let child = detached_sleep_child();
            fixture
                .app
                .state
                .runtime_for_pane_in_workspace(&fixture.app.terminal_runtimes, 0, pane)
                .expect("runtime")
                .test_set_child_pid(child.id());
            roots.push(child);
        }
        // A genuine ordinary caller has no inherited pane launch markers. Do
        // not rely on this test process's environment under a Herdr harness.
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "app::api::input_guard::tests::guard_exec_peer_helper",
                "--nocapture",
            ])
            .env("HERDR_GUARD_EXEC_HELPER", "ordinary")
            .stdout(std::process::Stdio::piped());
        for key in [
            "HERDR_ENV",
            "HERDR_PANE_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_TAB_ID",
        ] {
            command.env_remove(key);
        }
        crate::platform::detach_server_daemon_command(&mut command);
        let mut ordinary_peer =
            GuardTestChild(command.spawn().expect("marker-free ordinary caller"));
        use std::io::BufRead;
        let output = std::io::BufReader::new(ordinary_peer.stdout.take().expect("helper stdout"));
        let ready = output.lines().any(|line| {
            line.expect("helper receipt")
                .ends_with(&format!("guard-exec-ready {}", ordinary_peer.id()))
        });
        assert!(ready, "marker-free exec readiness missing");
        let context = ApiRequestContext::for_local_peer_pid(Some(ordinary_peer.id()));
        assert_eq!(
            context.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::Absent
        );
        assert_eq!(
            fixture.app.input_origin_for_context(context),
            InputOrigin::Ordinary
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            crate::platform::process_identity_server_ancestry(
                context.local_peer_identity.expect("native Darwin caller")
            ),
            crate::platform::ServerAncestry::ReachedServer,
            "a real ordinary child reaches the server; it is not an outside-proof seam"
        );
        let request = Request {
            id: "outside".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                expected_agent_session_id: None,
                pane_id: fixture.target_pane_id.clone(),
                text: "ordinary".into(),
                allow_cross_pane: false,
            }),
        };
        assert_ok(
            &fixture
                .app
                .handle_api_request_with_context(request.clone(), context),
        );
        assert_eq!(
            fixture.target_rx.try_recv().expect("ordinary shell"),
            Bytes::from_static(b"ordinary")
        );
        for child in &mut roots {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_unknown(
            &fixture
                .app
                .handle_api_request_with_context(request, context),
        );
        assert!(fixture.target_rx.try_recv().is_err());
        let _ = ordinary_peer.kill();
        let _ = ordinary_peer.wait();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn resume_reports_from_agent_child_guard_own_foreign_opt_in_and_metadata() {
        for method in ["pane.report_agent", "pane.report_agent_session"] {
            let mut fixture = attributed_agent_fixture();
            let child = detached_sleep_child();
            let context = ApiRequestContext::for_local_peer_pid(Some(child.id()));
            let source = fixture
                .app
                .pane_target(&fixture.source_pane_id)
                .expect("source");
            let origin = fixture.app.input_origin_for_context(context);
            assert!(
                matches!(origin, InputOrigin::Agent(ref target) if target.terminal_id == source.terminal_id)
            );
            for (pane_id, opt_in, resume, denied) in [
                (fixture.source_pane_id.clone(), false, true, false),
                (fixture.target_pane_id.clone(), false, true, true),
                (fixture.target_pane_id.clone(), true, true, false),
                (fixture.target_pane_id.clone(), false, false, false),
            ] {
                let mut params = serde_json::json!({
                    "pane_id": pane_id, "source": "custom:pi", "agent": "pi",
                    "state": "working", "allow_cross_pane": opt_in,
                });
                if resume {
                    params["resume_argv"] = serde_json::json!(["pi", "--resume", "guard-session"]);
                }
                let request: Request = serde_json::from_value(serde_json::json!({
                    "id": "child-resume", "method": method, "params": params,
                }))
                .expect("report request");
                let response = fixture
                    .app
                    .handle_api_request_with_context(request, context);
                if denied {
                    assert_denied(&response);
                    let target = fixture
                        .app
                        .pane_target(&fixture.target_pane_id)
                        .expect("target");
                    assert!(fixture.app.state.terminals[target.terminal_id.as_str()]
                        .reported_resume()
                        .is_none());
                } else {
                    assert_ok(&response);
                    if resume {
                        let target = fixture.app.pane_target(&pane_id).expect("report target");
                        assert!(fixture.app.state.terminals[target.terminal_id.as_str()]
                            .reported_resume()
                            .is_some());
                    }
                }
            }
        }
    }

    fn report_working(pane_id: &str) -> Method {
        Method::PaneReportAgent(crate::api::schema::PaneReportAgentParams {
            allow_cross_pane: false,
            pane_id: pane_id.into(),
            // A plain hook source: the rebind is source-agnostic, and the Pi
            // lifecycle source adds session-anchoring rules this test does not need.
            source: "custom:pi".into(),
            agent: "pi".into(),
            state: crate::api::schema::PaneAgentState::Working,
            message: None,
            seq: Some(1),
            agent_session_id: None,
            agent_session_path: None,
            resume_argv: None,
        })
    }

    fn terminal_state(app: &App, public_pane_id: &str) -> AgentState {
        let (ws_idx, pane_id) = app.parse_pane_id(public_pane_id).expect("pane");
        let terminal_id = app.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .expect("terminal");
        app.state.terminals[terminal_id].state
    }

    // smarty-dev#509: a Pi keeps reporting under the HERDR_PANE_ID it inherited
    // after its pane was renumbered by a recovery.
    #[tokio::test]
    async fn stale_pane_id_report_rebinds_to_the_reporting_process_pane() {
        let mut fixture = attributed_agent_fixture();
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "stale".into(),
                method: report_working("w42:p1F"),
            },
            attributed_context(),
        );

        assert_ok(&response);
        assert_eq!(
            terminal_state(&fixture.app, &fixture.source_pane_id),
            AgentState::Working
        );
        assert_eq!(
            terminal_state(&fixture.app, &fixture.target_pane_id),
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn stale_pane_id_report_from_unknown_process_is_pane_not_found() {
        let mut fixture = attributed_agent_fixture();
        for (id, context) in [
            ("unattributed", ApiRequestContext::default()),
            (
                "outside-every-pane",
                ApiRequestContext::for_local_peer_pid(Some(u32::MAX)),
            ),
        ] {
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: id.into(),
                    method: report_working("w42:p1F"),
                },
                context,
            );
            let response: ErrorResponse = serde_json::from_str(&response).expect("error");
            assert_eq!(response.error.code, "pane_not_found", "{id}");
            assert_eq!(response.error.message, "pane w42:p1F not found", "{id}");
        }
        assert_eq!(
            terminal_state(&fixture.app, &fixture.source_pane_id),
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn known_pane_id_report_is_not_rebound_to_the_reporting_process_pane() {
        let fixture = attributed_agent_fixture();
        let mut request = Request {
            id: "known".into(),
            method: report_working(&fixture.target_pane_id),
        };
        fixture
            .app
            .rebind_stale_report_pane(&mut request, attributed_context());
        assert_eq!(request.method, report_working(&fixture.target_pane_id));

        let mut stale = Request {
            id: "stale".into(),
            method: report_working("w42:p1F"),
        };
        fixture
            .app
            .rebind_stale_report_pane(&mut stale, attributed_context());
        assert_eq!(stale.method, report_working(&fixture.source_pane_id));
    }
}
