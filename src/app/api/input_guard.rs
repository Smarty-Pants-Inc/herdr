use super::responses::encode_error;
use super::App;
use crate::api::schema::{Method, Request};
use crate::api::ApiRequestContext;
use crate::app::terminal_targets::{InputOrigin, TerminalTarget};

impl App {
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
        let origin = context
            .local_peer_identity
            .map_or(InputOrigin::Unknown, |peer| {
                self.input_origin_for_peer_identity(peer)
            });
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
            Method::AgentPrompt(params) => params.allow_cross_pane,
            Method::AgentSendKeys(params) => params.allow_cross_pane,
            Method::PaneSendText(params) => params.allow_cross_pane,
            Method::PaneSendKeys(params) => params.allow_cross_pane,
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
            Method::AgentPrompt(params) => self.resolve_agent_target(&params.target).ok(),
            Method::AgentSendKeys(params) => self.resolve_agent_target(&params.target).ok(),
            Method::PaneSendText(params) => self.pane_target(&params.pane_id),
            Method::PaneSendKeys(params) => self.pane_target(&params.pane_id),
            Method::PaneSendInput(params) | Method::PaneSendInputGuarded(params) => {
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
        PaneSendInputParams, PaneSendKeysParams, PaneSendTextParams, ResponseResult,
        SuccessResponse,
    };
    use crate::app::Mode;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;
    use bytes::Bytes;
    use tokio::sync::mpsc::Receiver;

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
                    pane_id: fixture.target_pane_id.clone(),
                    text: "foreign".into(),
                    allow_cross_pane: false,
                }),
            };
            let denial = fixture.app.cross_pane_input_denial(&request, context);
            if expected.is_some() {
                assert_denied(&denial.expect("imported guard applies"));
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
    fn detached_sleep_child() -> std::process::Child {
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        crate::platform::detach_server_daemon_command(&mut command);
        command.spawn().expect("spawn detached sleep child")
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
                target: "target-agent".into(),
                text: "prompt".into(),
                wait: None,
                allow_cross_pane: false,
            }),
            Method::AgentSendKeys(AgentSendKeysParams {
                target: "target-agent".into(),
                keys: vec!["enter".into()],
                allow_cross_pane: false,
            }),
            Method::PaneSendText(PaneSendTextParams {
                pane_id: fixture.target_pane_id.clone(),
                text: "text".into(),
                allow_cross_pane: false,
            }),
            Method::PaneSendKeys(PaneSendKeysParams {
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
            let unknown_response = fixture.app.handle_api_request_with_context(
                Request {
                    id: format!("unknown-origin-{index}"),
                    method: method.clone(),
                },
                ApiRequestContext::default(),
            );
            assert_unknown(&unknown_response);
            let response = fixture.app.handle_api_request_with_context(
                Request {
                    id: format!("cross-pane-{index}"),
                    method,
                },
                attributed_context(),
            );
            assert_denied(&response);
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
    async fn attributed_agent_can_send_content_to_its_own_pane() {
        let mut fixture = attributed_agent_fixture();
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "same-pane".into(),
                method: Method::PaneSendText(PaneSendTextParams {
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
                    pane_id: fixture.target_pane_id.clone(),
                    text: "unattributed".into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext {
                local_peer_identity: Some(stale),
            },
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
                pane_id: fixture.target_pane_id.clone(),
                text: "ordinary".into(),
                allow_cross_pane: false,
            }),
        };
        let context = attributed_context();
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

    #[cfg(windows)]
    struct WindowsRealChild {
        child: std::process::Child,
        pid: u32,
        request_line: String,
    }

    #[cfg(windows)]
    impl WindowsRealChild {
        fn spawn(request: &str) -> Self {
            use std::io::BufRead;
            use std::process::{Command, Stdio};

            let child = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$p=$PID; [Console]::Out.WriteLine('{\"pid\":'+$p+'}'); [Console]::Out.WriteLine($env:HERDR_GUARD_TEST_REQUEST); [Console]::Out.Flush(); Start-Sleep -Seconds 60",
                ])
                .env("HERDR_GUARD_TEST_REQUEST", request)
                // Retain the owned stdin pipe alongside the live child handle.
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn real Windows helper child");
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
        let source_root = WindowsRealChild::spawn("{}");
        let target_root = WindowsRealChild::spawn("{}");
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
        assert!(
            !matches!(origin, InputOrigin::Agent(_)),
            "real sibling caller was wrongly attributed to a managed pane: {origin:?}"
        );
        if matches!(origin, InputOrigin::Unknown) {
            assert_unknown(&response);
            assert!(response.contains("--allow-cross-pane"));
            assert!(fixture.target_rx.try_recv().is_err());
        } else {
            assert_ok(&response);
            assert_eq!(
                fixture.target_rx.try_recv().expect("ordinary child bytes"),
                Bytes::from_static(b"windows real child")
            );
        }
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
    async fn reused_windows_intermediate_parent_is_unknown_in_actual_guard() {
        let mut fixture = attributed_agent_fixture();
        let observation = crate::platform::test_reused_intermediate_parent_membership();
        assert_eq!(observation, None);
        let request = Request {
            id: "reused-windows-parent".into(),
            method: Method::PaneSendText(PaneSendTextParams {
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
        use std::os::fd::AsRawFd;
        let mut fixture = attributed_agent_fixture();
        let (server, client) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let context = ApiRequestContext {
            local_peer_identity: crate::platform::local_socket_peer_identity_without_pidfd(
                server.as_raw_fd(),
            ),
        };
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
        let missing = ApiRequestContext {
            local_peer_identity: crate::platform::local_socket_peer_identity_without_pidfd(
                server.as_raw_fd(),
            ),
        };
        assert_eq!(missing.local_peer_identity, None);
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "unsupported-and-disconnected".into(),
                method: Method::PaneSendText(PaneSendTextParams {
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
                    pane_id: fixture.target_pane_id.clone(),
                    text: "refused".into(),
                    allow_cross_pane: false,
                }),
            },
            context,
            respond_to,
            response_write_complete: None,
            stream_active: None,
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
        let context = attributed_context();
        assert_eq!(
            fixture
                .app
                .input_origin_for_peer_identity(context.local_peer_identity.expect("peer")),
            InputOrigin::Ordinary
        );
        let request = Request {
            id: "outside".into(),
            method: Method::PaneSendText(PaneSendTextParams {
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
    }

    fn report_working(pane_id: &str) -> Method {
        Method::PaneReportAgent(crate::api::schema::PaneReportAgentParams {
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
