use super::*;
use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};
use crate::api::schema::{
    ErrorResponse, Method, PaneMoveDestination, PaneMoveParams, ResponseResult, SuccessResponse,
    TabMoveParams,
};

fn project_server() -> HeadlessServer {
    let mut server = test_headless_server();
    let mut source = crate::workspace::Workspace::test_new("source");
    source.test_add_tab(Some("remaining"));
    source.switch_tab(0);
    server.app.state.workspaces = vec![source, crate::workspace::Workspace::test_new("target")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server
}

fn public_method(
    server: &mut HeadlessServer,
    method: Method,
) -> Result<ResponseResult, ErrorResponse> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "project-check".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    decode_response(&response_rx.recv().expect("project check response"))
}

fn decode_response(response: &str) -> Result<ResponseResult, ErrorResponse> {
    serde_json::from_str::<SuccessResponse>(response)
        .map(|response| response.result)
        .map_err(|_| serde_json::from_str(response).expect("error response"))
}

fn root(server: &HeadlessServer, workspace: usize, tab: usize) -> crate::layout::PaneId {
    server.app.state.workspaces[workspace].tabs[tab].root_pane
}

fn cwd(server: &mut HeadlessServer, pane: crate::layout::PaneId, path: &std::path::Path) {
    let id = server
        .app
        .state
        .workspaces
        .iter()
        .find_map(|ws| {
            let tab = ws.find_tab_index_for_pane(pane)?;
            ws.tabs[tab].terminal_id(pane).cloned()
        })
        .unwrap();
    server.app.state.terminals.get_mut(&id).unwrap().cwd = path.to_path_buf();
    if let Some(runtime) = server.app.terminal_runtimes.get(&id) {
        runtime.test_set_foreground_cwd(Some(path.to_path_buf()));
    }
}

fn session(server: &mut HeadlessServer, pane: crate::layout::PaneId, name: &str) -> String {
    let path = std::env::temp_dir()
        .join("herdr-project-check-sessions")
        .join(format!("{name}.jsonl"));
    let value = path.display().to_string();
    let id = server
        .app
        .state
        .workspaces
        .iter()
        .find_map(|ws| {
            let tab = ws.find_tab_index_for_pane(pane)?;
            ws.tabs[tab].terminal_id(pane).cloned()
        })
        .unwrap();
    server
        .app
        .state
        .terminals
        .get_mut(&id)
        .unwrap()
        .set_persisted_agent_session(PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: AgentSessionRef::path(value.clone()).expect("actual Pi Path session"),
        });
    let terminal = server.app.state.terminals.get_mut(&id).unwrap();
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Idle,
    );
    let foreground = terminal.cwd.clone();
    if let Some(runtime) = server.app.terminal_runtimes.get(&id) {
        runtime.test_set_foreground_cwd(Some(foreground));
    } else {
        let (runtime, _input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        runtime.test_set_foreground_cwd(Some(foreground));
        server.app.terminal_runtimes.insert(id, runtime);
    }
    value
}

fn metadata(server: &mut HeadlessServer, workspace: usize, org: Option<&str>, project: &str) {
    let mut tokens = std::collections::HashMap::from([
        ("smarty_project_id".into(), Some(project.into())),
        (
            "smarty_project_label".into(),
            Some(format!("Project {project}")),
        ),
    ]);
    if let Some(org) = org {
        tokens.insert("smarty_org_id".into(), Some(org.into()));
    }
    server.app.state.workspaces[workspace]
        .metadata_tokens
        .patch(tokens, None, std::time::Instant::now());
}

fn membership(server: &mut HeadlessServer, workspace: usize, checkout: &std::path::Path) {
    server.app.state.workspaces[workspace].worktree_space =
        Some(crate::workspace::WorktreeSpaceMembership {
            // Intentionally shared repository identity, but distinct checkout paths.
            key: "same-repository-common-dir".into(),
            label: "smarty-pants".into(),
            repo_root: checkout.parent().unwrap().to_path_buf(),
            checkout_path: checkout.to_path_buf(),
            is_linked_worktree: true,
        });
}

fn move_params(
    server: &HeadlessServer,
    pane: crate::layout::PaneId,
    allow: bool,
) -> PaneMoveParams {
    let ws = server
        .app
        .state
        .workspaces
        .iter()
        .position(|ws| ws.find_tab_index_for_pane(pane).is_some())
        .unwrap();
    PaneMoveParams {
        pane_id: server.app.public_pane_id(ws, pane).unwrap(),
        destination: PaneMoveDestination::NewTab {
            workspace_id: Some(server.app.public_workspace_id(1)),
            label: None,
        },
        focus: true,
        allow_project_change: allow,
    }
}

fn fingerprint(server: &HeadlessServer) -> String {
    let state = &server.app.state;
    let topology: Vec<_> = state
        .workspaces
        .iter()
        .map(|ws| {
            (
                ws.id.clone(),
                ws.active_tab,
                ws.metadata_tokens
                    .values()
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ws.tabs
                    .iter()
                    .map(|tab| {
                        (
                            tab.number,
                            tab.root_pane,
                            tab.layout.pane_ids(),
                            tab.layout.focused(),
                            tab.layout
                                .pane_ids()
                                .iter()
                                .map(|id| (*id, tab.panes[id].attached_terminal_id.clone()))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    // HashMap debug order is stable while these maps are untouched on refusal.
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        topology,
        state.active,
        state.selected,
        state.public_pane_id_aliases,
        server.app.event_hub.events_after(0)
    )
}

#[derive(Clone)]
struct LogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn project_change_reported_same_repo_checkout_sequence_refuses_then_allows_once() {
    let mut server = project_server();
    let pane = root(&server, 0, 0);
    let session_path = session(&mut server, pane, "org-deputy");
    let base = std::env::temp_dir().join("smarty-pants");
    let old_checkout = base.join("smarty-chief");
    let new_checkout = base.join("worktrees").join("org-deputy");
    membership(&mut server, 0, &old_checkout);
    // The Pi cwd differs from its source workspace's registered checkout.
    cwd(&mut server, pane, &new_checkout);
    let mut params = move_params(&server, pane, false);
    params.destination = PaneMoveDestination::NewWorkspace {
        label: None,
        tab_label: None,
    };
    // Keep the policy sequence independent of focused-move history (#4456).
    params.focus = false;
    let (control_rx, _render_rx) = connect_test_shell(&mut server, 9, 80, 23);
    let _initial = client_shell_snapshot(&control_rx);
    let client_location = server.clients[&9].shell_location.clone();
    let client_revision = server.clients[&9].shell_projection_revision;
    let before = fingerprint(&server);
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let error = public_method(&mut server, Method::PaneMove(params.clone())).unwrap_err();
        assert_eq!(error.error.code, "project_change_refused");
        for name in [
            params.pane_id.as_str(),
            "Path",
            session_path.as_str(),
            old_checkout.to_str().unwrap(),
            new_checkout.to_str().unwrap(),
        ] {
            assert!(
                error.error.message.contains(name),
                "missing {name}: {}",
                error.error.message
            );
        }
        assert_eq!(fingerprint(&server), before, "refusal is atomic");
        assert_eq!(server.clients[&9].shell_location, client_location);
        assert_eq!(
            server.clients[&9].shell_projection_revision,
            client_revision
        );
        assert!(logs.lock().unwrap().is_empty());
        params.allow_project_change = true;
        let ResponseResult::PaneMove { move_result } =
            public_method(&mut server, Method::PaneMoveProjectChecked(params)).unwrap()
        else {
            panic!("pane move")
        };
        assert!(move_result.changed);
        assert_ne!(
            move_result.pane.workspace_id,
            move_result.previous_workspace_id
        );
        assert_eq!(
            server.clients[&9]
                .shell_location
                .as_ref()
                .unwrap()
                .focused_workspace_id
                .as_deref(),
            client_location
                .as_ref()
                .unwrap()
                .focused_workspace_id
                .as_deref()
        );
        assert_eq!(move_result.pane.agent_session.unwrap().value, session_path);
    });
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("intentional project change allowed").count(),
        1,
        "{logs}"
    );
    assert_eq!(
        logs.lines()
            .filter(|line| line.contains("intentional project change allowed"))
            .count(),
        1
    );
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_complete_metadata_overrides_checkout_but_org_is_part_of_identity() {
    for same_org in [true, false] {
        let mut server = project_server();
        let pane = root(&server, 0, 0);
        session(&mut server, pane, "metadata");
        membership(&mut server, 0, &std::env::temp_dir().join("checkout-a"));
        membership(&mut server, 1, &std::env::temp_dir().join("checkout-b"));
        metadata(&mut server, 0, Some("smarty-pants"), "herdr");
        metadata(
            &mut server,
            1,
            Some(if same_org {
                "smarty-pants"
            } else {
                "other-org"
            }),
            "herdr",
        );
        let mut params = move_params(&server, pane, false);
        // Policy-only move: focused transfers have a pre-existing stale-history
        // bug independent of project identity. Keep the full invariant below.
        params.focus = false;
        let before = fingerprint(&server);
        let result = public_method(&mut server, Method::PaneMove(params));
        if same_org {
            assert!(
                matches!(result.unwrap(), ResponseResult::PaneMove { move_result } if move_result.changed)
            );
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.error.code, "project_change_refused");
            assert!(error.error.message.contains("smarty-pants/herdr"));
            assert!(error.error.message.contains("other-org/herdr"));
            assert_eq!(fingerprint(&server), before);
        }
        assert_eq!(
            server.app.state.active,
            Some(0),
            "policy move does not navigate"
        );
        assert!(server.app.state.previous_pane_focus.is_none());
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_partial_metadata_falls_back_on_both_sides_not_labels_or_repo_key() {
    for same_checkout in [true, false] {
        let mut server = project_server();
        let pane = root(&server, 0, 0);
        session(&mut server, pane, "partial");
        let base = std::env::temp_dir().join("partial-a");
        membership(&mut server, 0, &base);
        membership(
            &mut server,
            1,
            &if same_checkout {
                base
            } else {
                std::env::temp_dir().join("partial-b")
            },
        );
        metadata(&mut server, 0, Some("smarty-pants"), "herdr");
        metadata(&mut server, 1, None, "herdr");
        let params = move_params(&server, pane, false);
        let result = public_method(&mut server, Method::PaneMove(params));
        assert_eq!(result.is_ok(), same_checkout);
        if let Err(error) = result {
            assert_eq!(error.error.code, "project_change_refused");
        }
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_shell_moves_and_same_project_session_moves_are_unchanged() {
    for has_session in [true, false] {
        let mut server = project_server();
        let pane = root(&server, 0, 0);
        if has_session {
            session(&mut server, pane, "same-project");
        }
        metadata(&mut server, 0, Some("smarty-pants"), "herdr");
        metadata(
            &mut server,
            1,
            Some("smarty-pants"),
            if has_session { "herdr" } else { "other" },
        );
        let mut params = move_params(&server, pane, true);
        // Focus-history defects are tracked separately in #4456.
        params.focus = false;
        // Even an explicit override must not log when there is no actual change.
        let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || LogWriter(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            params.destination = PaneMoveDestination::Tab {
                tab_id: server.app.public_tab_id(1, 0).unwrap(),
                target_pane_id: None,
                split: api::schema::SplitDirection::Right,
                ratio: None,
            };
            assert!(
                matches!(public_method(&mut server, Method::PaneMoveProjectChecked(params)).unwrap(),
                ResponseResult::PaneMove { move_result } if move_result.changed)
            );
        });
        assert!(!String::from_utf8(logs.lock().unwrap().clone())
            .unwrap()
            .contains("intentional project change allowed"));
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_root_pane_or_tab_removal_protects_surviving_source_agents() {
    for split_root in [true, false] {
        for same_workspace in [true, false] {
            let mut server = project_server();
            let pane = root(&server, 0, 0);
            let survivor = if split_root {
                server.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal)
            } else {
                root(&server, 0, 1)
            };
            server.app.state.ensure_test_terminals();
            let old_project = std::env::temp_dir().join("source-project-a");
            let new_project = std::env::temp_dir().join("source-project-b");
            cwd(&mut server, pane, &old_project);
            cwd(&mut server, survivor, &new_project);
            // Removing the first Pi re-projects the surviving source Pi session.
            session(&mut server, pane, "removed-first-pi");
            let survivor_session = session(&mut server, survivor, "source-survivor");
            let mut params = move_params(&server, pane, false);
            if same_workspace {
                params.destination = PaneMoveDestination::NewTab {
                    workspace_id: None,
                    label: None,
                };
            }
            let before = fingerprint(&server);
            let error = public_method(&mut server, Method::PaneMove(params)).unwrap_err();
            assert_eq!(error.error.code, "project_change_refused");
            assert!(error.error.message.contains(&survivor_session));
            assert!(error.error.message.contains(old_project.to_str().unwrap()));
            assert!(error.error.message.contains(new_project.to_str().unwrap()));
            assert_eq!(fingerprint(&server), before);
            server.app.state.assert_invariants_for_test();
            shutdown_test_runtimes(&mut server);
        }
    }
}

#[tokio::test]
async fn project_change_client_tab_drag_method_reaches_shared_server_precheck() {
    let mut server = project_server();
    let first = root(&server, 0, 0);
    let next = root(&server, 0, 1);
    cwd(
        &mut server,
        first,
        &std::env::temp_dir().join("tab-project-a"),
    );
    cwd(
        &mut server,
        next,
        &std::env::temp_dir().join("tab-project-b"),
    );
    let session_path = session(&mut server, first, "drag-survivor");
    session(&mut server, next, "drag-next-pi");
    let tab_id = server.app.public_tab_id(0, 1).unwrap();
    let (control_rx, _render_rx) = connect_test_shell(&mut server, 41, 80, 23);
    let _initial = client_shell_snapshot(&control_rx);
    let before = fingerprint(&server);
    let location = server.clients[&41].shell_location.clone();
    server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
        client_id: 41,
        boot_id: server.client_shell_boot_id.clone(),
        request: Box::new(api::schema::Request {
            id: "drag-tab".into(),
            method: Method::TabMove(TabMoveParams {
                tab_id,
                insert_index: 0,
            }),
        }),
    });
    let ready = tokio::time::timeout(Duration::from_secs(2), server.server_event_rx.recv())
        .await
        .expect("bounded endpoint response")
        .expect("endpoint response ready");
    server.handle_server_event(ready);
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } = read_server_message(
        control_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("client refusal"),
    ) else {
        panic!("endpoint refusal")
    };
    let error = decode_response(std::str::from_utf8(&data).unwrap()).unwrap_err();
    assert_eq!(error.error.code, "project_change_refused");
    assert!(error.error.message.contains(&session_path));
    assert_eq!(fingerprint(&server), before);
    assert_eq!(server.clients[&41].shell_location, location);
    assert!(
        server.clients.contains_key(&41),
        "refusal never disconnects client"
    );
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_unknown_affected_identity_denies_but_unrelated_unknown_does_not() {
    let mut server = project_server();
    let pane = root(&server, 0, 0);
    session(&mut server, pane, "unknown-target");
    let target = root(&server, 1, 0);
    session(&mut server, target, "unknown-target-first-pi");
    cwd(
        &mut server,
        target,
        std::path::Path::new("relative-unknown"),
    );
    let params = move_params(&server, pane, false);
    let error = public_method(&mut server, Method::PaneMove(params)).unwrap_err();
    assert_eq!(error.error.code, "project_change_refused");
    assert!(error.error.message.contains("unknown project"));
    // An unrelated workspace with an unknown identity must not block shell work.
    session(&mut server, target, "unrelated-unknown");
    let shell = root(&server, 0, 1);
    let mut params = move_params(&server, shell, false);
    params.destination = PaneMoveDestination::NewTab {
        workspace_id: None,
        label: None,
    };
    assert!(public_method(&mut server, Method::PaneMove(params)).is_ok());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_legacy_override_requires_distinct_checked_method() {
    let mut server = project_server();
    let pane = root(&server, 0, 0);
    let params = move_params(&server, pane, true);
    let before = fingerprint(&server);
    let error = public_method(&mut server, Method::PaneMove(params)).unwrap_err();
    assert_eq!(error.error.code, "project_change_capability_required");
    assert!(error.error.message.contains("pane.move_project_checked"));
    assert_eq!(fingerprint(&server), before);
    assert!(
        crate::server::client_commands::supports_client_shell_method_name(
            "pane.move_project_checked"
        )
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_tab_checked_override_allows_reroot_and_logs_all_sessions_once() {
    let mut server = project_server();
    let first = root(&server, 0, 0);
    let next = root(&server, 0, 1);
    cwd(
        &mut server,
        first,
        &std::env::temp_dir().join("tab-project-a"),
    );
    cwd(
        &mut server,
        next,
        &std::env::temp_dir().join("tab-project-b"),
    );
    let first_session = session(&mut server, first, "tab-first");
    let next_session = session(&mut server, next, "tab-next");
    let params = api::schema::TabMoveProjectCheckedParams {
        tab_id: server.app.public_tab_id(0, 1).unwrap(),
        insert_index: 0,
        allow_project_change: false,
    };
    let before = fingerprint(&server);
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let error =
            public_method(&mut server, Method::TabMoveProjectChecked(params.clone())).unwrap_err();
        assert_eq!(error.error.code, "project_change_refused");
        assert_eq!(fingerprint(&server), before);
        assert!(logs.lock().unwrap().is_empty());
        let mut params = params;
        params.allow_project_change = true;
        assert!(matches!(
            public_method(&mut server, Method::TabMoveProjectChecked(params)).unwrap(),
            ResponseResult::TabList { .. }
        ));
        assert_eq!(root(&server, 0, 0), next);
    });
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("intentional project change allowed").count(),
        1,
        "{logs}"
    );
    assert!(logs.contains(&first_session));
    assert!(logs.contains(&next_session));
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_same_identity_and_shell_tab_reorders_do_not_log() {
    for has_session in [true, false] {
        let mut server = project_server();
        let first = root(&server, 0, 0);
        let next = root(&server, 0, 1);
        cwd(
            &mut server,
            first,
            &std::env::temp_dir().join("tab-project-a"),
        );
        cwd(
            &mut server,
            next,
            &std::env::temp_dir().join("tab-project-b"),
        );
        if has_session {
            session(&mut server, first, "same-project-tab");
            metadata(&mut server, 0, Some("smarty-pants"), "herdr");
        }
        let params = api::schema::TabMoveProjectCheckedParams {
            tab_id: server.app.public_tab_id(0, 1).unwrap(),
            insert_index: 0,
            allow_project_change: true,
        };
        let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || LogWriter(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert!(public_method(&mut server, Method::TabMoveProjectChecked(params)).is_ok());
        });
        assert_eq!(root(&server, 0, 0), next);
        assert!(!String::from_utf8(logs.lock().unwrap().clone())
            .unwrap()
            .contains("intentional project change allowed"));
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_checked_noop_and_invalid_destination_never_log() {
    let mut server = project_server();
    let pane = root(&server, 0, 0);
    session(&mut server, pane, "no-op");
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        for tab_id in [
            server.app.public_tab_id(0, 0).unwrap(),
            "missing-tab".into(),
        ] {
            let mut params = move_params(&server, pane, true);
            params.destination = PaneMoveDestination::Tab {
                tab_id,
                target_pane_id: None,
                split: api::schema::SplitDirection::Right,
                ratio: None,
            };
            match public_method(&mut server, Method::PaneMoveProjectChecked(params)) {
                Ok(ResponseResult::PaneMove { move_result }) => assert!(!move_result.changed),
                Err(error) => assert_eq!(error.error.code, "tab_not_found"),
                other => panic!("unexpected no-op {other:?}"),
            }
        }
    });
    assert!(!String::from_utf8(logs.lock().unwrap().clone())
        .unwrap()
        .contains("intentional project change allowed"));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_checkout_identity_is_canonical_not_spelling_or_label() {
    let mut server = project_server();
    let pane = root(&server, 0, 0);
    session(&mut server, pane, "canonical-checkout");
    let checkout = std::env::temp_dir();
    membership(&mut server, 0, &checkout.join("."));
    membership(&mut server, 1, &checkout);
    server.app.state.workspaces[1].set_custom_name("different label".into());
    let mut params = move_params(&server, pane, false);
    // Exercise checkout policy without the pre-existing focused-move history bug.
    params.focus = false;
    assert!(
        matches!(public_method(&mut server, Method::PaneMove(params)).unwrap(),
        ResponseResult::PaneMove { move_result } if move_result.changed)
    );
    assert_eq!(
        server.app.state.active,
        Some(0),
        "policy move does not navigate"
    );
    assert!(server.app.state.previous_pane_focus.is_none());
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_uses_first_pi_foreground_not_root_shell_or_pane_tokens() {
    let mut server = project_server();
    let source_shell = root(&server, 0, 0);
    let source_pi =
        server.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    let target_shell = root(&server, 1, 0);
    let target_pi =
        server.app.state.workspaces[1].test_split(ratatui::layout::Direction::Horizontal);
    server.app.state.ensure_test_terminals();
    let shell_cwd = std::env::temp_dir().join("shared-root-shell");
    let project_a = std::env::temp_dir().join("pi-project-a");
    let project_b = std::env::temp_dir().join("pi-project-b");
    cwd(&mut server, source_shell, &shell_cwd);
    cwd(&mut server, target_shell, &shell_cwd);
    cwd(&mut server, source_pi, &project_a);
    cwd(&mut server, target_pi, &project_b);
    let source_session = session(&mut server, source_pi, "divergent-source");
    session(&mut server, target_pi, "divergent-target");
    for pane in [source_pi, target_pi] {
        let id = server
            .app
            .state
            .workspaces
            .iter()
            .find_map(|ws| ws.terminal_id(pane))
            .unwrap()
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&id)
            .unwrap()
            .metadata_tokens
            .patch(
                std::collections::HashMap::from([
                    ("smarty_org_id".into(), Some("same-pane-org".into())),
                    ("smarty_project_id".into(), Some("same-pane-project".into())),
                ]),
                None,
                std::time::Instant::now(),
            );
    }
    let params = move_params(&server, source_pi, false);
    let before = fingerprint(&server);
    let error = public_method(&mut server, Method::PaneMove(params)).unwrap_err();
    assert_eq!(error.error.code, "project_change_refused");
    assert!(error.error.message.contains(&source_session));
    assert!(error.error.message.contains(project_a.to_str().unwrap()));
    assert!(error.error.message.contains(project_b.to_str().unwrap()));
    assert!(!error.error.message.contains(shell_cwd.to_str().unwrap()));
    assert!(!error.error.message.contains("same-pane-project"));
    assert_eq!(fingerprint(&server), before);
    let ResponseResult::AgentList { agents } = public_method(
        &mut server,
        Method::AgentList(api::schema::EmptyParams::default()),
    )
    .unwrap() else {
        panic!("agent list")
    };
    assert_eq!(
        agents
            .iter()
            .find(
                |agent| agent.workspace_id == server.app.public_workspace_id(0)
                    && agent.agent.as_deref() == Some("pi")
            )
            .unwrap()
            .foreground_cwd
            .as_deref(),
        Some(project_a.to_str().unwrap())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_insertion_before_target_first_pi_protects_target_survivors() {
    for split in [
        api::schema::SplitDirection::Right,
        api::schema::SplitDirection::Down,
    ] {
        let mut server = project_server();
        let source = root(&server, 0, 0);
        let target_shell = root(&server, 1, 0);
        let target_pi =
            server.app.state.workspaces[1].test_split(ratatui::layout::Direction::Horizontal);
        server.app.state.ensure_test_terminals();
        cwd(
            &mut server,
            source,
            &std::env::temp_dir().join("insert-project-a"),
        );
        cwd(
            &mut server,
            target_pi,
            &std::env::temp_dir().join("insert-project-b"),
        );
        let source_session = session(&mut server, source, "insert-source");
        let target_session = session(&mut server, target_pi, "insert-target-survivor");
        let mut params = move_params(&server, source, false);
        // Insertion/project projection is independent of navigation; avoid the
        // pre-existing focused-move history bug and retain the full invariant.
        params.focus = false;
        params.destination = PaneMoveDestination::Tab {
            tab_id: server.app.public_tab_id(1, 0).unwrap(),
            target_pane_id: Some(server.app.public_pane_id(1, target_shell).unwrap()),
            split,
            ratio: None,
        };
        let before = fingerprint(&server);
        let error = public_method(&mut server, Method::PaneMove(params.clone())).unwrap_err();
        assert_eq!(error.error.code, "project_change_refused");
        assert!(error.error.message.contains(&target_session));
        assert!(
            !error.error.message.contains(&source_session),
            "moved Pi stays project A; target Pi is endangered"
        );
        assert_eq!(fingerprint(&server), before);
        params.allow_project_change = true;
        let ResponseResult::PaneMove { move_result } =
            public_method(&mut server, Method::PaneMoveProjectChecked(params)).unwrap()
        else {
            panic!("pane move")
        };
        let ResponseResult::AgentList { agents } = public_method(
            &mut server,
            Method::AgentList(api::schema::EmptyParams::default()),
        )
        .unwrap() else {
            panic!("agent list")
        };
        let first = agents
            .iter()
            .find(|agent| {
                agent.workspace_id == move_result.pane.workspace_id
                    && agent.agent.as_deref() == Some("pi")
            })
            .unwrap();
        assert_eq!(first.agent_session.as_ref().unwrap().value, source_session);
        assert_eq!(
            server.app.state.workspaces[1].tabs[0].layout.pane_ids(),
            vec![target_shell, source, target_pi]
        );
        assert_eq!(
            server.app.state.workspaces[1].tabs[0].layout.focused(),
            target_pi
        );
        assert_eq!(
            server.app.state.active,
            Some(0),
            "policy move does not navigate"
        );
        assert!(server.app.state.previous_pane_focus.is_none());
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_tab_reorder_with_same_first_pi_ignores_root_shell_cwd() {
    let mut server = project_server();
    let shell = root(&server, 0, 0);
    let pi = root(&server, 0, 1);
    cwd(
        &mut server,
        shell,
        &std::env::temp_dir().join("unrelated-shell-root"),
    );
    cwd(
        &mut server,
        pi,
        &std::env::temp_dir().join("only-pi-project"),
    );
    session(&mut server, pi, "only-pi");
    let params = TabMoveParams {
        tab_id: server.app.public_tab_id(0, 1).unwrap(),
        insert_index: 0,
    };
    assert!(public_method(&mut server, Method::TabMove(params)).is_ok());
    assert_eq!(root(&server, 0, 0), pi);
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_unknown_first_pi_does_not_scan_later_known_pi() {
    let mut server = project_server();
    let source = root(&server, 0, 0);
    let first = root(&server, 1, 0);
    let next_tab = server.app.state.workspaces[1].test_add_tab(Some("later Pi"));
    let later = root(&server, 1, next_tab);
    server.app.state.ensure_test_terminals();
    let project = std::env::temp_dir().join("known-later-pi-project");
    cwd(&mut server, source, &project);
    cwd(&mut server, later, &project);
    session(&mut server, source, "unknown-first-source");
    session(&mut server, first, "unknown-first-target");
    session(&mut server, later, "known-later-target");
    let terminal = server.app.state.workspaces[1].terminal_id(first).unwrap();
    server
        .app
        .terminal_runtimes
        .get(terminal)
        .unwrap()
        .test_set_foreground_cwd(None);
    let params = move_params(&server, source, false);
    let error = public_method(&mut server, Method::PaneMove(params)).unwrap_err();
    assert_eq!(error.error.code, "project_change_refused");
    assert!(error.error.message.contains("unknown project"));
    assert!(error.error.message.contains(project.to_str().unwrap()));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn project_change_checked_schemas_default_to_deny_without_changing_legacy_tab_shape() {
    let pane: PaneMoveParams = serde_json::from_value(serde_json::json!({
        "pane_id": "w1:p1", "destination": { "type": "new_workspace" }
    }))
    .unwrap();
    assert!(!pane.allow_project_change);
    let tab: api::schema::TabMoveProjectCheckedParams = serde_json::from_value(serde_json::json!({
        "tab_id": "w1:t1", "insert_index": 0
    }))
    .unwrap();
    assert!(!tab.allow_project_change);
    let legacy = serde_json::to_value(TabMoveParams {
        tab_id: "w1:t1".into(),
        insert_index: 0,
    })
    .unwrap();
    assert_eq!(legacy.as_object().unwrap().len(), 2);
    assert!(legacy.get("allow_project_change").is_none());
    for method in ["pane.move_project_checked", "tab.move_project_checked"] {
        assert!(crate::server::client_commands::supports_client_shell_method_name(method));
    }
}
