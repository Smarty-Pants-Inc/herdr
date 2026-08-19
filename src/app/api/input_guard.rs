use super::responses::encode_error;
use super::App;
use crate::api::schema::{Method, Request};
use crate::api::ApiRequestContext;
use crate::app::terminal_targets::TerminalTarget;

impl App {
    // This is a policy guard, not caller authentication.
    pub(super) fn cross_pane_input_denial(
        &self,
        request: &Request,
        context: ApiRequestContext,
    ) -> Option<String> {
        // Missing attribution retains the normal compatibility path.
        let peer_pid = context.local_peer_pid?;
        let Some(source) = self.agent_terminal_target_for_peer_pid(peer_pid) else {
            // PID attribution, managed runtime state, and session membership are all
            // best-effort. Unknown, non-agent, and out-of-pane callers fail open.
            return None;
        };
        if Self::allows_cross_pane(&request.method) {
            return None;
        }
        let target = self.content_write_target(&request.method)?;
        if source.terminal_id == target.terminal_id {
            return None;
        }

        Some(encode_error(
            request.id.clone(),
            "cross_pane_input_denied",
            "agent-originated input cannot target a different pane",
        ))
    }

    fn allows_cross_pane(method: &Method) -> bool {
        match method {
            Method::AgentStart(params) => params.allow_cross_pane,
            Method::AgentPrompt(params) => params.allow_cross_pane,
            Method::AgentSendKeys(params) => params.allow_cross_pane,
            Method::PaneSendText(params) => params.allow_cross_pane,
            Method::PaneSendKeys(params) => params.allow_cross_pane,
            Method::PaneSendInput(params) => params.allow_cross_pane,
            _ => false,
        }
    }

    fn content_write_target(&self, method: &Method) -> Option<TerminalTarget> {
        match method {
            Method::AgentStart(params) => self.pane_target(&params.pane_id),
            Method::AgentPrompt(params) => self.resolve_agent_target(&params.target).ok(),
            Method::AgentSendKeys(params) => self.resolve_agent_target(&params.target).ok(),
            Method::PaneSendText(params) => self.pane_target(&params.pane_id),
            Method::PaneSendKeys(params) => self.pane_target(&params.pane_id),
            Method::PaneSendInput(params) => self.pane_target(&params.pane_id),
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct ChildGuard(std::process::Child);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn attributed_agent_fixture() -> Fixture {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
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
        ApiRequestContext {
            local_peer_pid: Some(std::process::id()),
        }
    }

    fn assert_denied(response: &str) {
        let response: ErrorResponse = serde_json::from_str(response).expect("denial response");
        assert_eq!(response.error.code, "cross_pane_input_denied");
    }

    fn assert_ok(response: &str) {
        let response: SuccessResponse = serde_json::from_str(response).expect("success response");
        assert!(matches!(response.result, ResponseResult::Ok {}));
    }

    #[tokio::test]
    async fn attributed_agent_cannot_inject_content_into_a_different_pane() {
        let mut fixture = attributed_agent_fixture();
        let methods = vec![
            Method::AgentStart(AgentStartParams {
                name: "new-agent".into(),
                kind: "pi".into(),
                pane_id: fixture.target_pane_id.clone(),
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
                keys: vec!["enter".into()],
                allow_cross_pane: false,
            }),
        ];

        for (index, method) in methods.into_iter().enumerate() {
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn session_detached_agent_descendant_cannot_prompt_a_different_pane() {
        use std::os::unix::process::CommandExt as _;

        let mut fixture = attributed_agent_fixture();
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = ChildGuard(command.spawn().expect("spawn detached descendant"));
        let peer_pid = child.0.id();
        assert!(crate::platform::process_is_descendant_of(
            peer_pid,
            std::process::id()
        ));
        assert!(!crate::platform::session_processes(std::process::id()).contains(&peer_pid));

        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "detached-cross-pane".into(),
                method: Method::AgentPrompt(AgentPromptParams {
                    target: "target-agent".into(),
                    text: "blocked".into(),
                    wait: None,
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext {
                local_peer_pid: Some(peer_pid),
            },
        );

        assert_denied(&response);
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
    async fn unknown_non_agent_and_out_of_pane_origins_remain_compatible() {
        let mut fixture = attributed_agent_fixture();
        for (id, context) in [
            ("unknown", ApiRequestContext::default()),
            (
                "out-of-pane",
                ApiRequestContext {
                    local_peer_pid: Some(u32::MAX),
                },
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
            assert_ok(&response);
            assert_eq!(
                fixture
                    .target_rx
                    .try_recv()
                    .expect("compatible input bytes"),
                Bytes::from(id)
            );
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
    async fn gui_message_dispatch_uses_the_same_origin_context() {
        let mut fixture = attributed_agent_fixture();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        fixture
            .app
            .handle_api_request_message(crate::api::ApiRequestMessage {
                request: Request {
                    id: "gui-cross-pane".into(),
                    method: Method::PaneSendText(PaneSendTextParams {
                        pane_id: fixture.target_pane_id.clone(),
                        text: "blocked".into(),
                        allow_cross_pane: false,
                    }),
                },
                context: attributed_context(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            });

        assert_denied(&response_rx.recv().expect("gui response"));
        assert!(fixture.target_rx.try_recv().is_err());
    }
}
