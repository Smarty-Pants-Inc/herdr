use super::*;
use crate::platform::{ProcessIdentity, ProcessLiveness};

#[test]
fn channel_owner_pin_survives_disconnect_unknown_liveness_and_child_takeover() {
    let owner = ProcessIdentity {
        pid: 100,
        start_time: 1,
    };
    for other in [
        ProcessIdentity {
            pid: 101,
            start_time: 2,
        },
        ProcessIdentity {
            pid: 100,
            start_time: 3,
        },
    ] {
        for liveness in [ProcessLiveness::Alive, ProcessLiveness::Unknown] {
            assert!(!owner_allows_replacement(owner, other, true, liveness));
        }
        assert!(owner_allows_replacement(
            owner,
            other,
            true,
            ProcessLiveness::Dead
        ));
        assert!(owner_allows_replacement(
            owner,
            other,
            false,
            ProcessLiveness::Unknown
        ));
    }
    // Only the exact same peer generation may reconnect to a reserved owner.
    assert!(owner_allows_replacement(
        owner,
        owner,
        true,
        ProcessLiveness::Alive
    ));
    assert!(owner_allows_replacement(
        owner,
        owner,
        true,
        ProcessLiveness::Unknown
    ));
}
fn app() -> (
    App,
    crate::layout::PaneId,
    tokio::sync::mpsc::Receiver<bytes::Bytes>,
) {
    let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    let workspace = crate::workspace::Workspace::test_new("undetected-channel");
    let pane = workspace.tabs[0].root_pane;
    app.state.workspaces = vec![workspace];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    let (runtime, receiver) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    runtime.test_set_child_pid(std::process::id());
    let terminal = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    app.terminal_runtimes.insert(terminal, runtime);
    (app, pane, receiver)
}
fn value(response: String) -> serde_json::Value {
    serde_json::from_str(&response).unwrap()
}
#[tokio::test]
async fn channel_discovery_undetected_terminal_and_no_transport_have_zero_pty_effects() {
    let (mut app, pane, mut input) = app();
    let target = app.public_pane_id(0, pane).unwrap();
    assert!(app.resolve_agent_target(&target).is_err());
    let info =
        value(app.handle_agent_channel_info("info".into(), AgentChannelInfoParams { target }));
    assert_eq!(info["result"]["ready"], false);
    assert!(info["result"]["terminal_id"].is_string());
    let registration = value(app.handle_agent_register_self(
        "register".into(),
        AgentRegisterSelfParams {
            pane_id: None,
            session_generation: "generation".into(),
            transport: None,
        },
        crate::api::ApiRequestContext::for_local_peer_pid(Some(std::process::id())),
    ));
    assert!(registration.get("error").is_some());
    assert!(input.try_recv().is_err());
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn channel_expected_terminal_epoch_origin_and_attachment_fail_closed_without_pty() {
    let (mut app, pane, mut input) = app();
    let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    let target = app.public_pane_id(0, pane).unwrap();
    let root = crate::platform::process_identity(std::process::id()).unwrap();
    let (channel, _receiver) = Channel::new(
        terminal_id.to_string(),
        "epoch".into(),
        "session".into(),
        root,
        root,
        app.state.workspaces[0].id.clone(),
        pane,
    );
    channel.mark_ready();
    app.agent_channels
        .owners
        .insert(terminal_id.to_string(), channel.clone());
    app.terminal_runtimes
        .bind_agent_channel(terminal_id.clone(), &channel);
    let mut params = AgentPromptGuardedParams {
        target,
        text: "literal; shell-command\n".into(),
        expected_terminal: "wrong-terminal".into(),
        expected_registration_epoch: "epoch".into(),
        request_id: "r".into(),
        timeout_ms: Some(100),
        allow_cross_pane: true,
    };
    let outcome = app.reserve_guarded_prompt(&params).err().unwrap();
    assert!(matches!(
        outcome,
        Outcome::Failure {
            code: "terminal_identity_mismatch",
            ..
        }
    ));
    params.expected_terminal = terminal_id.to_string();
    params.expected_registration_epoch = "old-epoch".into();
    assert!(matches!(
        app.reserve_guarded_prompt(&params).err().unwrap(),
        Outcome::Failure {
            code: "registration_epoch_mismatch",
            ..
        }
    ));
    let other_pane = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.ensure_test_terminals();
    let cross = Request {
        id: "registered-undetected-cross".into(),
        method: Method::AgentPromptGuarded(AgentPromptGuardedParams {
            target: app.public_pane_id(0, other_pane).unwrap(),
            allow_cross_pane: false,
            ..params.clone()
        }),
    };
    assert_eq!(
        value(
            app.cross_pane_input_denial(
                &cross,
                crate::api::ApiRequestContext::for_local_peer_pid(Some(root.pid))
            )
            .unwrap()
        )["error"]["code"],
        "cross_pane_input_denied"
    );
    params.allow_cross_pane = false;
    let request = Request {
        id: "unknown-origin".into(),
        method: Method::AgentPromptGuarded(params.clone()),
    };
    assert_eq!(
        value(
            app.cross_pane_input_denial(&request, crate::api::ApiRequestContext::default())
                .unwrap()
        )["error"]["code"],
        "input_origin_unknown"
    );
    params.expected_registration_epoch = "epoch".into();
    let request = Request {
        id: "explicit".into(),
        method: Method::AgentPromptGuarded(AgentPromptGuardedParams {
            allow_cross_pane: true,
            ..params
        }),
    };
    assert!(app
        .cross_pane_input_denial(&request, crate::api::ApiRequestContext::default())
        .is_none());
    // App attachment replacement retires the old pin, including pending requests.
    app.state.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane)
        .unwrap()
        .attached_terminal_id = crate::terminal::TerminalId::alloc();
    assert!(!app.channel_attached(&channel));
    app.revoke_agent_channels();
    assert!(!channel.is_active());
    assert!(input.try_recv().is_err());
}
#[tokio::test]
async fn channel_runtime_replacement_revokes_before_new_root_and_handoff_cancels_all() {
    let (mut app, pane, mut input) = app();
    let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
    let root = crate::platform::process_identity(std::process::id()).unwrap();
    let (channel, _receiver) = Channel::new(
        terminal_id.to_string(),
        "epoch".into(),
        "session".into(),
        root,
        root,
        app.state.workspaces[0].id.clone(),
        pane,
    );
    channel.mark_ready();
    app.agent_channels
        .owners
        .insert(terminal_id.to_string(), channel.clone());
    app.terminal_runtimes
        .bind_agent_channel(terminal_id.clone(), &channel);
    let (delivery, _, _waiter) = channel
        .reserve("r".into(), "test".into(), Duration::from_secs(1))
        .unwrap();
    let (replacement, _input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    let _old = app.terminal_runtimes.insert(terminal_id, replacement);
    assert!(!channel.is_active());
    assert!(matches!(
        delivery.wait(),
        Outcome::Failure {
            code: "agent_channel_unavailable",
            ..
        }
    ));
    app.revoke_agent_channels();
    assert!(app.agent_channels.owners.is_empty());
    assert!(input.try_recv().is_err());
}
