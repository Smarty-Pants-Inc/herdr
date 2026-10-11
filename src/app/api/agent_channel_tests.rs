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
        None,
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
        None,
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

// Full App attribution/registration + native socket/foreground PTY. The peer is
// the same live process throughout, and guarded prompts never touch its PTY.
#[cfg(target_os = "linux")]
mod rollover_transport {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    struct Harness {
        path: std::path::PathBuf,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        _pty: portable_pty::PtyPair,
        stop: Arc<AtomicBool>,
        _server: crate::api::ServerHandle,
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn service(app: &mut App) {
        while let Ok(message) = app.api_rx.try_recv() {
            if matches!(message.request.method, Method::AgentPromptGuarded(_)) {
                assert!(app.handle_deferred_guarded_agent_prompt(
                    message.request,
                    message.context,
                    message.respond_to
                ));
            } else {
                let response =
                    app.handle_api_request_with_context(message.request, message.context);
                let _ = message.respond_to.send(response);
            }
        }
    }
    fn until(app: &mut App, mut predicate: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate(app) {
            service(app);
            assert!(
                Instant::now() < deadline,
                "App/transport condition timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn harness(
        mode: &str,
    ) -> (
        App,
        crate::layout::PaneId,
        tokio::sync::mpsc::Receiver<bytes::Bytes>,
        Harness,
    ) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "r176-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut app, pane, input) = super::app();
        app.api_rx = rx;
        let server_stop = crate::server::shutdown::ServerStop::default();
        let stop = server_stop.flag().clone();
        let server = crate::api::start_server_at_with_stop_control(
            path.join("s"),
            tx,
            crate::api::EventHub::default(),
            server_stop,
        )
        .unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "api::agent_channel::tests::transport::channel_transport_peer",
            "--nocapture",
            "--test-threads=1",
        ]);
        command.env("CHANNEL_TEST_PATH", &path);
        command.env("CHANNEL_TEST_MODE", mode);
        let child = pair.slave.spawn_command(command).unwrap();
        let terminal = app.state.workspaces[0].terminal_id(pane).unwrap();
        app.terminal_runtimes
            .get(terminal)
            .unwrap()
            .test_set_child_pid(child.process_id().unwrap());
        let harness = Harness {
            path,
            child,
            _pty: pair,
            stop,
            _server: server,
        };
        (app, pane, input, harness)
    }
    fn params(
        app: &App,
        pane: crate::layout::PaneId,
        epoch: &str,
        index: usize,
    ) -> AgentPromptGuardedParams {
        AgentPromptGuardedParams {
            target: app.public_pane_id(0, pane).unwrap(),
            expected_terminal: app.state.workspaces[0]
                .terminal_id(pane)
                .unwrap()
                .to_string(),
            expected_registration_epoch: epoch.into(),
            request_id: format!("prompt-{index}"),
            text: format!("literal-{index}"),
            timeout_ms: Some(2000),
            allow_cross_pane: true,
        }
    }
    fn rollover_prompts(mode: &str) {
        let (mut app, pane, mut input, harness) = harness(mode);
        let terminal = app.state.workspaces[0]
            .terminal_id(pane)
            .unwrap()
            .to_string();
        until(&mut app, |app| {
            app.agent_channels
                .owners
                .get(&terminal)
                .is_some_and(|channel| channel.is_ready())
        });
        let first_epoch = app.agent_channels.owners[&terminal].epoch.clone();
        let mut epoch = first_epoch.clone();
        let mut first = None;
        let mut unknown = None;
        for index in 0..300 {
            let reservation = loop {
                let prompt = params(&app, pane, &epoch, index);
                match app.reserve_guarded_prompt(&prompt) {
                    Ok(reservation) => break reservation,
                    Err(Outcome::Failure {
                        code:
                            "agent_channel_rotating"
                            | "registration_epoch_mismatch"
                            | "agent_channel_unavailable",
                        ..
                    }) => {
                        until(&mut app, |app| {
                            app.agent_channels
                                .owners
                                .get(&terminal)
                                .is_some_and(|channel| channel.is_ready() && channel.epoch != epoch)
                        });
                        epoch = app.agent_channels.owners[&terminal].epoch.clone();
                    }
                    Err(other) => panic!("prompt {index} failed: {other:?}"),
                }
            };
            let (delivery, duplicate, _waiter) = reservation;
            assert!(!duplicate);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(delivery.wait());
            });
            let mut receipt = None;
            until(&mut app, |_| {
                receipt = rx.try_recv().ok();
                receipt.is_some()
            });
            let receipt = receipt.unwrap();
            if mode == "rollover-unknown" && index == 191 {
                assert!(matches!(
                    &receipt,
                    Outcome::Failure {
                        code: "delivery_unknown",
                        ..
                    }
                ));
                unknown = Some(receipt.clone());
            } else {
                assert!(
                    matches!(&receipt, Outcome::Receipt(value) if value["status"] == if index % 2 == 0 { "accepted" } else { "queued" })
                );
            }
            if index == 0 {
                first = Some(receipt);
            }
        }
        assert_ne!(
            epoch, first_epoch,
            "300 prompts must cross a clean rotation"
        );
        let (old, duplicate, _waiter) = app
            .reserve_guarded_prompt(&params(&app, pane, &first_epoch, 0))
            .unwrap();
        assert!(duplicate);
        assert_eq!(old.wait(), first.unwrap());
        if let Some(unknown) = unknown {
            let (old, duplicate, _waiter) = app
                .reserve_guarded_prompt(&params(&app, pane, &first_epoch, 191))
                .unwrap();
            assert!(duplicate);
            assert_eq!(old.wait(), unknown);
            let response = value(old.wait().response("unknown-retry".into(), duplicate));
            assert_eq!(response["error"]["code"], "delivery_unknown");
            assert_eq!(response["error"]["duplicate"], true);
        }
        let mut mismatch = params(&app, pane, &first_epoch, 0);
        mismatch.text = "changed".into();
        assert!(matches!(
            app.reserve_guarded_prompt(&mismatch),
            Err(Outcome::Failure {
                code: "payload_mismatch",
                ..
            })
        ));
        assert!(matches!(
            app.reserve_guarded_prompt(&params(&app, pane, &first_epoch, 300)),
            Err(Outcome::Failure {
                code: "registration_epoch_mismatch",
                ..
            })
        ));
        until(&mut app, |_| {
            std::fs::read_to_string(harness.path.join("delivered"))
                .is_ok_and(|count| count == "300")
        });
        assert_eq!(
            std::fs::read_to_string(harness.path.join("delivered")).unwrap(),
            "300"
        );
        assert!(harness.path.join("rotated").exists());
        assert!(
            input.try_recv().is_err(),
            "guarded socket delivery wrote PTY bytes"
        );
        // A third same-peer registration makes the oldest epoch unconditionally stale.
        // Even its formerly known key cannot replay, while the previous epoch stays readable.
        let transport = crate::api::agent_channel::RegistrationTransport::default();
        let registration = value(app.handle_agent_register_self(
            "third".into(),
            AgentRegisterSelfParams {
                pane_id: None,
                session_generation: "new-session-generation".into(),
                transport: Some(transport.clone()),
            },
            crate::api::ApiRequestContext::for_local_peer_pid(harness.child.process_id()),
        ));
        assert_eq!(registration["result"]["ready"], true, "{registration}");
        let (current, _receiver) = transport.take().unwrap();
        current.mark_ready();
        assert!(matches!(
            app.reserve_guarded_prompt(&params(&app, pane, &first_epoch, 0)),
            Err(Outcome::Failure {
                code: "registration_epoch_mismatch",
                ..
            })
        ));
        let (previous, duplicate, _waiter) = app
            .reserve_guarded_prompt(&params(&app, pane, &epoch, 192))
            .unwrap();
        assert!(duplicate);
        assert!(
            matches!(previous.wait(), Outcome::Receipt(value) if value["status"] == "accepted" && value["session_generation"] == "session_test")
        );
        assert_eq!(app.agent_channels.retired.len(), 1);
        std::fs::write(harness.path.join("done"), b"").unwrap();
        app.revoke_agent_channels();
    }
    #[tokio::test]
    async fn channel_transport_300_prompts_rotate_and_retain_old_first_receipt() {
        rollover_prompts("rollover");
    }
    #[tokio::test]
    async fn channel_transport_unknown_at_rotation_is_retained_and_never_replayed() {
        rollover_prompts("rollover-unknown");
    }
    #[tokio::test]
    async fn channel_transport_300_successive_registrations_same_live_peer() {
        let (mut app, _pane, mut input, harness) = harness("registrations");
        until(&mut app, |_| {
            std::fs::read_to_string(harness.path.join("registered"))
                .is_ok_and(|count| count == "300")
        });
        assert_eq!(
            std::fs::read_to_string(harness.path.join("registered")).unwrap(),
            "300"
        );
        assert!(input.try_recv().is_err());
        std::fs::write(harness.path.join("done"), b"").unwrap();
        app.revoke_agent_channels();
    }
}
