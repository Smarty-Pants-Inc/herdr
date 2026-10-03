use super::*;
use crate::client::endpoint::{ProfileId, SavedSshEndpoint};
use crate::client::shell::grouped_projection::{GroupKey, GroupedRow};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

fn profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        label: "Not a physical host".into(),
        target: "dev@ryzen3".into(),
        session: "agents".into(),
        enabled: true,
    }
}

fn stamp(workspace: &mut ClientShellWorkspace, org: &str, project: &str, lane: bool) {
    workspace.tokens = vec![
        ("smarty_org_id".into(), org.into()),
        ("smarty_org_label".into(), org.into()),
        ("smarty_project_id".into(), project.into()),
        ("smarty_project_label".into(), project.into()),
        (
            "smarty_role".into(),
            if lane {
                "worktree-agent"
            } else {
                "project-agent"
            }
            .into(),
        ),
    ];
    if lane {
        workspace.tokens.extend([
            ("smarty_lane_id".into(), "lane-4283".into()),
            ("smarty_lane_purpose".into(), "Task purpose".into()),
            ("smarty_issue".into(), "example/repo#4283".into()),
            // Deliberately wrong hint; transport identity and host tag cannot follow it.
            ("smarty_host".into(), "ryzen1".into()),
        ]);
    }
}

fn grouped_state() -> (ClientShellState, ClientEndpointId) {
    let mut config = Config::default();
    config.ui.sidebar.grouping.enabled = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let profile = profile();
    let remote_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    let mut local = snapshot();
    local.workspaces[0].label = "lead".into();
    stamp(&mut local.workspaces[0], "org", "project", false);
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.workspaces[0].label = "opaque-lane-label".into();
    stamp(&mut remote.workspaces[0], "org", "project", true);
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    (state, remote_id)
}

fn leaf_targets(state: &mut ClientShellState) -> Vec<(ClientEndpointId, String)> {
    state
        .grouped_navigation_targets()
        .into_iter()
        .map(|target| (target.endpoint_id, target.workspace_id))
        .collect()
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }),
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }),
    ])
}

#[test]
fn grouped_render_preserves_endpoint_routing_and_actual_host_tags() {
    let (mut state, remote) = grouped_state();
    let frame = state.compose(120, 50).unwrap();
    let rows = frame_rows(&frame);
    assert!(rows.iter().any(|row| row.contains("projects")));
    assert!(rows.iter().any(|row| row.contains("project")));
    assert!(rows.iter().any(|row| row.contains("Task")));
    assert!(rows.iter().any(|row| row.contains("[Ryzen 3]")));
    assert_eq!(state.hits.grouped_headings.len(), 2);
    assert_eq!(state.hits.workspaces.len(), 2);
    assert_eq!(
        state.hits.workspaces[0].endpoint_id,
        ClientEndpointId::Local
    );
    assert_eq!(state.hits.workspaces[1].endpoint_id, remote);
    assert_eq!(
        state.hits.workspaces[0].workspace_id,
        state.hits.workspaces[1].workspace_id
    );
    let rect = state.hits.workspaces[1].rect;
    let outcome = click(&mut state, rect);
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
        endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
    }] if endpoint_id == &remote && workspace_id == "ws_1")
    );
    assert!(outcome.requests.is_empty());
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert!(
        !state.endpoint_workspace_is_draggable(&ClientWorkspacePress {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws_1".into(),
            start_column: 0,
            start_row: 0,
        })
    );
    assert!(
        state.grouped_rows.iter().any(|row| matches!(row,
        GroupedRow::Workspace { label, host, .. } if label.contains("#4283") && host == "Ryzen 3"))
    );
}

#[test]
fn grouped_heading_only_collapses_and_keyboard_reopens_every_group() {
    let (mut state, remote) = grouped_state();
    state.compose(120, 50).unwrap();
    let project = GroupKey::Project("org".into(), "project".into());
    let rect = state
        .hits
        .grouped_headings
        .iter()
        .find(|(_, key)| key == &project)
        .unwrap()
        .0;
    let outcome = click(&mut state, rect);
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.is_empty());
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert!(state.grouped_collapsed.contains(&project));
    state.compose(120, 50).unwrap();
    assert!(state.hits.workspaces.is_empty());
    let org = GroupKey::Org("org".into());
    state.toggle_grouped_heading(org.clone());
    assert!(leaf_targets(&mut state).is_empty());
    state.mode = ClientShellMode::Navigate;
    state.navigate_workspace_id = state.focused_navigation_target();
    for key in [b"=".as_slice(), b"+".as_slice()] {
        state
            .grouped_collapsed
            .extend([org.clone(), project.clone()]);
        state.invalidate_grouped_projection();
        let outcome = state.handle_input_bytes(key);
        assert!(outcome.repaint);
        assert!(outcome.actions.is_empty());
        assert!(outcome.requests.is_empty());
        assert!(state.grouped_collapsed.is_empty());
        assert_eq!(
            leaf_targets(&mut state),
            [
                (ClientEndpointId::Local, "ws_1".into()),
                (remote.clone(), "ws_1".into())
            ]
        );
        state.compose(120, 50).unwrap();
        assert_eq!(state.hits.workspaces.len(), 2);
    }
    let collapsed = state.handle_input_bytes(b"-");
    assert!(collapsed.actions.is_empty());
    assert!(collapsed.requests.is_empty());
    assert!(state.grouped_collapsed.contains(&project));
    state.compose(120, 50).unwrap();
    assert!(
        state.hits.workspaces.is_empty(),
        "manual keyboard collapse survives reveal"
    );
    // Prefix+w-style reveal opens only the selected leaf's ancestors.
    state.reveal_navigation_workspace = true;
    state.compose(120, 50).unwrap();
    assert!(!state.grouped_collapsed.contains(&project));
    assert_eq!(state.hits.workspaces.len(), 2);
}

#[test]
fn grouped_navigation_uses_visible_display_order_and_never_selects_headings() {
    let (mut state, remote) = grouped_state();
    state.compose(120, 50).unwrap();
    state.mode = ClientShellMode::Navigate;
    state.navigate_workspace_id = state.focused_navigation_target();
    state.move_navigate_workspace(1);
    assert_eq!(
        state.navigate_workspace_id.as_ref().unwrap().endpoint_id,
        remote
    );
    let mut accepted = ClientShellInput::default();
    state.accept_navigate_workspace(&mut accepted);
    assert!(
        matches!(accepted.actions.as_slice(), [ClientShellAction::ActivateEndpoint { endpoint_id,
        target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)) }] if endpoint_id == &remote && workspace_id == "ws_1")
    );
    let mut next = ClientShellInput::default();
    assert!(state.handle_endpoint_navigation(crate::input::KeybindAction::NextWorkspace, &mut next));
    assert!(
        matches!(next.actions.as_slice(), [ClientShellAction::ActivateEndpoint { endpoint_id,
        target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)) }] if endpoint_id == &remote && workspace_id == "ws_1")
    );
    assert!(
        state.navigate_workspace_id.is_none(),
        "relative focus must not leave a navigation preview"
    );
    state.compose(120, 50).unwrap();
    assert_eq!(state.hits.workspaces.len(), leaf_targets(&mut state).len());
    state.toggle_grouped_heading(GroupKey::Project("org".into(), "project".into()));
    let mut collapsed = ClientShellInput::default();
    state.handle_endpoint_navigation(crate::input::KeybindAction::NextWorkspace, &mut collapsed);
    assert!(collapsed.actions.is_empty());
}

fn configure_workspace_index_binding(state: &mut ClientShellState, binding: &str) {
    let config: Config =
        toml::from_str(&format!("[keys]\nswitch_workspace = \"{binding}\"\n")).unwrap();
    assert!(config.collect_diagnostics().is_empty());
    state.config.keybinds = ClientShellConfig::from_config(&config).keybinds;
}

fn assert_workspace_activation(
    outcome: &ClientShellInput,
    endpoint: &ClientEndpointId,
    workspace: &str,
) {
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Workspace(workspace_id)),
        }] if endpoint_id == endpoint && workspace_id == workspace
    ));
    assert!(outcome.requests.is_empty());
}

#[test]
fn grouped_configured_shifted_workspace_index_selects_remote_second_leaf() {
    // Cover the reported legacy byte sequence, its semantic equivalent, and bare digits.
    for input in [
        b"\x02w@".as_slice(),
        b"\x02w2".as_slice(),
        b"\x02w".as_slice(),
    ] {
        let (mut state, remote) = grouped_state();
        configure_workspace_index_binding(&mut state, "prefix+shift+1..9");
        state.compose(120, 50).unwrap();
        assert_eq!(state.snapshot.as_ref().unwrap().workspaces.len(), 1);
        assert_eq!(state.hits.workspaces[1].endpoint_id, remote);
        assert_eq!(
            state.hits.workspaces[0].workspace_id, state.hits.workspaces[1].workspace_id,
            "colliding IDs must retain endpoint attribution"
        );
        let mut outcome = state.handle_input_bytes(input);
        if input == b"\x02w" {
            assert!(outcome.actions.is_empty());
            outcome = state.handle_raw_events(vec![RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('2'), KeyModifiers::SHIFT),
            )]);
        }
        assert_workspace_activation(&outcome, &remote, "ws_1");
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.navigate_workspace_id.is_none());
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    }
}

#[test]
fn grouped_configured_workspace_index_rejects_hidden_or_offline_destination_then_recovers() {
    for collapsed in [true, false] {
        let (mut state, remote) = grouped_state();
        configure_workspace_index_binding(&mut state, "prefix+shift+1..9");
        // Keep the Local preview visible while independently hiding the second leaf.
        let mut remote_snapshot = state.endpoints[1].snapshot.clone().unwrap();
        remote_snapshot.revision += 1;
        stamp(&mut remote_snapshot.workspaces[0], "org", "z-project", true);
        state.set_endpoint_snapshot(&remote, remote_snapshot);
        state.compose(120, 50).unwrap();
        assert_eq!(state.hits.workspaces[1].endpoint_id, remote);
        let project = GroupKey::Project("org".into(), "z-project".into());
        if collapsed {
            state.toggle_grouped_heading(project.clone());
        } else {
            state.mark_endpoint_disconnected(&remote);
            assert!(state.endpoint_has_snapshot(&remote), "cached is not online");
            assert!(state.grouped_projection_dirty);
        }
        // No intervening compose/helper refresh: the input guard must consume current state.
        let rejected = state.handle_input_bytes(b"\x02w@");
        assert!(rejected.actions.is_empty());
        assert!(rejected.requests.is_empty());
        assert_eq!(state.mode, ClientShellMode::Navigate);
        assert_eq!(
            state.navigate_workspace_id.as_ref().unwrap().endpoint_id,
            ClientEndpointId::Local
        );
        if collapsed {
            assert!(state.grouped_collapsed.contains(&project));
            state.toggle_grouped_heading(project);
        } else {
            state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
            assert!(state.grouped_projection_dirty);
        }
        // Restoration is immediately actionable, without waiting for another rendered frame.
        let accepted = state.handle_input_bytes(b"@");
        assert_workspace_activation(&accepted, &remote, "ws_1");
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    }
}

#[test]
fn grouped_arbitrary_configured_workspace_index_follows_display_not_endpoint_order() {
    for (number, local_target) in [('1', false), ('2', true)] {
        let (mut state, remote) = grouped_state();
        configure_workspace_index_binding(&mut state, "prefix+alt+1..9");
        // Endpoint storage stays Local first; a remote lead renders before its Local lane.
        let mut local = state.snapshot.clone().unwrap();
        local.revision += 1;
        stamp(&mut local.workspaces[0], "org", "project", true);
        // Present the matching Local projection before composing the new display order.
        // A snapshot-only revision advance intentionally leaves compose unavailable.
        let mut local_surface = surface();
        local_surface.projection_revision = local.revision;
        state.set_snapshot(local);
        state.set_pane_surface(local_surface);
        let mut remote_snapshot = state.endpoints[1]
            .snapshot
            .clone()
            .expect("a same-boot Local projection update must retain the remote cache");
        remote_snapshot.revision += 1;
        stamp(&mut remote_snapshot.workspaces[0], "org", "project", false);
        state.set_endpoint_snapshot(&remote, remote_snapshot);
        state.compose(120, 50).unwrap();
        assert_eq!(state.endpoints[0].endpoint_id, ClientEndpointId::Local);
        assert_eq!(state.hits.workspaces[0].endpoint_id, remote);
        assert_eq!(
            state.hits.workspaces[1].endpoint_id,
            ClientEndpointId::Local
        );
        assert_eq!(
            state.hits.workspaces[0].workspace_id, state.hits.workspaces[1].workspace_id,
            "colliding IDs must retain endpoint attribution in reversed display order"
        );
        let entered = state.handle_input_bytes(b"\x02w");
        assert!(entered.actions.is_empty());
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char(number), KeyModifiers::ALT),
        )]);
        let expected = if local_target {
            ClientEndpointId::Local
        } else {
            remote
        };
        assert_workspace_activation(&outcome, &expected, "ws_1");
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.navigate_workspace_id.is_none());
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    }
}

#[test]
fn machines_default_and_configured_workspace_indices_stay_server_local() {
    for configured in [false, true] {
        let (mut state, remote) = grouped_state();
        state.apply_client_live_config(&Config::default(), &[], &[]);
        assert!(!state.config.grouping.enabled);
        if configured {
            configure_workspace_index_binding(&mut state, "prefix+shift+1..9");
        }
        state.compose(120, 50).unwrap();
        assert!(state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.endpoint_id == remote));
        assert_eq!(state.snapshot.as_ref().unwrap().workspaces.len(), 1);
        let rejected = state.handle_input_bytes(if configured { b"\x02w@" } else { b"\x02w2" });
        assert!(rejected.actions.is_empty());
        assert!(rejected.requests.is_empty());
        assert_eq!(state.mode, ClientShellMode::Navigate);
        let accepted = state.handle_input_bytes(if configured { b"!" } else { b"1" });
        assert!(
            matches!(accepted.actions.as_slice(), [ClientShellAction::Endpoint {
            endpoint_id: ClientEndpointId::Local, boot_id, request,
        }] if boot_id == "boot-1" && matches!(&request.method,
            crate::api::schema::Method::WorkspaceFocus(params) if params.workspace_id == "ws_1"))
        );
        assert!(accepted.requests.is_empty());
        assert_eq!(state.mode, ClientShellMode::Terminal);
    }
}

#[test]
fn grouped_configured_workspace_index_keeps_foreign_preview_action_gate() {
    let (mut state, remote) = grouped_state();
    configure_workspace_index_binding(&mut state, "prefix+shift+1..9");
    state.compose(120, 50).unwrap();
    let rejected = state.handle_input_bytes(b"\x02w\x1b[B@");
    assert!(rejected.actions.is_empty());
    assert!(rejected.requests.is_empty());
    assert_eq!(state.mode, ClientShellMode::Navigate);
    assert_eq!(
        state.navigate_workspace_id.as_ref().unwrap().endpoint_id,
        remote
    );
    assert!(state.workspace_preview_action_blocked());
    assert!(state.visible_endpoint_notice.is_some());
    let accepted = state.handle_input_bytes(b"\r");
    assert_workspace_activation(&accepted, &remote, "ws_1");
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn grouped_stale_leaves_are_dim_nonactionable_but_connection_diagnostics_remain() {
    let (mut state, remote) = grouped_state();
    state.compose(120, 50).unwrap();
    // An old press must not outlive the snapshot/status that supplied its identity.
    let rect = state.hits.workspaces[1].rect;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    })]);
    assert!(state.workspace_press.is_some());
    state.mark_endpoint_disconnected(&remote);
    assert!(state.workspace_press.is_none());
    let frame = state.compose(120, 50).unwrap();
    assert_eq!(state.hits.workspaces.len(), 1);
    assert_eq!(
        leaf_targets(&mut state),
        [(ClientEndpointId::Local, "ws_1".into())]
    );
    let buffer = frame.to_ratatui_buffer().unwrap();
    let (x, y) = cell_symbol_position(&frame, state.layout(120, 50).sidebar, "Ryzen 3");
    assert!(buffer[(x, y)].modifier.contains(Modifier::DIM));
    assert!(state
        .hits
        .machines
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    state.retire_endpoint(&remote);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Attention);
    state.set_machine_diagnostic(&remote, "Permission denied".into());
    state.compose(120, 50).unwrap();
    let badge = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .unwrap()
        .status_badge;
    let outcome = click(&mut state, badge);
    assert!(outcome.actions.is_empty());
    assert!(state
        .visible_endpoint_notice
        .as_ref()
        .unwrap()
        .body
        .contains("Permission denied"));
    assert!(state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == remote)
        .unwrap()
        .snapshot
        .is_none());
}

#[test]
fn grouped_compact_and_mobile_share_leaf_order_and_collapse_keys() {
    let (mut state, remote) = grouped_state();
    state.sidebar_collapsed = true;
    state.compose(120, 50).unwrap();
    let compact = state
        .hits
        .workspaces
        .iter()
        .map(|hit| (hit.endpoint_id.clone(), hit.workspace_id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(compact, leaf_targets(&mut state));
    state.mode = ClientShellMode::Navigate;
    state.navigate_workspace_id = state.focused_navigation_target();
    state.compose(40, 80).unwrap();
    let mobile = state
        .hits
        .mobile_targets
        .iter()
        .filter_map(|(_, target)| {
            if let ClientMobileTarget::Workspace {
                endpoint_id,
                workspace_id,
            } = target
            {
                Some((endpoint_id.clone(), workspace_id.clone()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(mobile, compact);
    assert!(!state.reveal_navigation_workspace);
    assert!(!state.reveal_focused_workspace);
    assert!(!state.reveal_mobile_workspace);
    let heading = state
        .hits
        .mobile_targets
        .iter()
        .find_map(|(rect, target)| {
            matches!(target, ClientMobileTarget::Group(GroupKey::Project(_, _))).then_some(*rect)
        })
        .unwrap();
    // A manual click supersedes any reveal already queued before the click.
    state.reveal_navigation_workspace = true;
    state.reveal_focused_workspace = true;
    state.reveal_mobile_workspace = true;
    let outcome = click(&mut state, heading);
    assert!(outcome.actions.is_empty());
    assert_eq!(state.mode, ClientShellMode::Navigate);
    let project = GroupKey::Project("org".into(), "project".into());
    for _ in 0..2 {
        state.compose(40, 80).unwrap();
        assert!(state.grouped_collapsed.contains(&project));
        assert!(!state
            .hits
            .mobile_targets
            .iter()
            .any(|(_, target)| matches!(target, ClientMobileTarget::Workspace { .. })));
    }
    // A fresh explicit keyboard-preview reveal must still reopen the ancestors.
    state.reveal_navigation_workspace = true;
    state.compose(40, 80).unwrap();
    assert!(!state.grouped_collapsed.contains(&project));
    assert!(state
        .hits
        .mobile_targets
        .iter()
        .any(|(_, target)| matches!(target, ClientMobileTarget::Workspace { .. })));
    state.handle_input_bytes(b"-");
    for _ in 0..2 {
        state.compose(40, 80).unwrap();
        assert!(state.grouped_collapsed.contains(&project));
    }
    // A later authoritative focus reveal also remains effective.
    state.reveal_focused_workspace = true;
    state.compose(40, 80).unwrap();
    assert!(!state.grouped_collapsed.contains(&project));
    let expanded = state.handle_input_bytes(b"=");
    assert!(expanded.actions.is_empty());
    state.mark_endpoint_disconnected(&remote);
    state.compose(40, 80).unwrap();
    assert!(!state
        .hits
        .mobile_targets
        .iter()
        .any(|(_, target)| matches!(target,
        ClientMobileTarget::Workspace { endpoint_id, .. } if endpoint_id == &remote)));
    assert!(state
        .hits
        .mobile_targets
        .iter()
        .any(|(_, target)| matches!(target,
        ClientMobileTarget::Machine(endpoint_id) if endpoint_id == &remote)));
}

#[test]
fn grouped_catalog_retirement_and_unsupported_metadata_method_do_not_hide_connections() {
    let (mut state, remote) = grouped_state();
    state.set_endpoint_methods_for(&remote, Some(vec!["workspace.focus".into()]));
    state.compose(120, 50).unwrap();
    assert_eq!(
        leaf_targets(&mut state).len(),
        2,
        "grouping reads tokens, not metadata-report capability"
    );
    let mut disabled = profile();
    disabled.enabled = false;
    state.set_endpoint_catalog(&[disabled]);
    state.compose(120, 50).unwrap();
    assert_eq!(leaf_targets(&mut state).len(), 1);
    assert!(state
        .hits
        .machines
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    state.set_endpoint_catalog(&[]);
    state.compose(120, 50).unwrap();
    assert!(!state
        .hits
        .machines
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    assert_eq!(state.hits.workspaces.len(), 1);
    let mut machines = Config::default();
    machines.ui.sidebar.grouping.enabled = false;
    state.apply_client_live_config(&machines, &[], &[]);
    let frame = state.compose(120, 50).unwrap();
    assert!(frame_rows(&frame).iter().any(|row| row.contains("spaces")));
    assert!(state.hits.grouped_headings.is_empty());
    assert!(state.grouped_rows.is_empty());
}

#[test]
fn grouped_old_boot_or_generation_cannot_accept_reused_workspace_id() {
    let (mut state, remote) = grouped_state();
    state.mode = ClientShellMode::Navigate;
    state.navigate_workspace_id = state.navigation_target(&remote, "ws_1");
    let mut changed = snapshot();
    changed.boot_id = "new-boot".into();
    state.cache_endpoint_snapshot_for_generation(&remote, 8, Box::new(changed));
    let mut outcome = ClientShellInput::default();
    state.accept_navigate_workspace(&mut outcome);
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.is_empty());
    state.refresh_grouped_projection();
    assert!(
        state.grouped_rows.iter().any(|row| matches!(
            row,
            GroupedRow::Heading {
                key: GroupKey::Ungrouped,
                ..
            }
        )),
        "fresh unstamped boot must not retain previous org/project identity"
    );
}

#[test]
fn grouped_cache_reuses_projection_for_terminal_frames_at_one_and_fifteen_workspaces() {
    for count in [1, 15] {
        let (mut state, remote) = grouped_state();
        let mut local = snapshot();
        let workspace = local.workspaces[0].clone();
        let pane = local.panes[0].clone();
        local.workspaces = (0..count)
            .map(|index| {
                let mut next = workspace.clone();
                next.workspace_id = format!("ws_{}", index + 1);
                next.number = index + 1;
                next.focused = index == 0;
                stamp(&mut next, "org", "project", false);
                next
            })
            .collect();
        local.panes = (0..count)
            .map(|index| ClientShellPane {
                pane_id: format!("pane_{}", index + 1),
                workspace_id: format!("ws_{}", index + 1),
                focused: index == 0,
                ..pane.clone()
            })
            .collect();
        state.set_snapshot(Box::new(local));
        let mut terminal = surface();
        let pane = terminal.panes[0].clone();
        terminal.panes = (0..count)
            .map(|index| PaneSurfacePane {
                pane_id: format!("pane_{}", index + 1),
                focused: index == 0,
                ..pane.clone()
            })
            .collect();
        state.set_pane_surface(terminal.clone());
        state.compose(120, 50).unwrap();
        let builds = state.grouped_projection_rebuilds;
        let rows = state.grouped_rows.clone();
        for revision in 2..5 {
            terminal.surface_revision = revision;
            state.set_pane_surface(terminal.clone());
            state.compose(120, 50).unwrap();
            state.compose(120, 50).unwrap();
            assert_eq!(
                state.grouped_projection_rebuilds, builds,
                "{count} populated workspaces/panes"
            );
            assert_eq!(state.grouped_rows, rows);
        }
        state.toggle_grouped_heading(GroupKey::Project("org".into(), "project".into()));
        state.compose(120, 50).unwrap();
        assert_eq!(
            state.grouped_projection_rebuilds, builds,
            "collapse filters cached projection"
        );
        state.toggle_grouped_heading(GroupKey::Project("org".into(), "project".into()));
        state.mode = ClientShellMode::Navigate;
        state.navigate_workspace_id = state.focused_navigation_target();
        state.reveal_navigation_workspace = true;
        for _ in 0..3 {
            state.compose(40, 80).unwrap();
            assert_eq!(
                state.grouped_projection_rebuilds, builds,
                "mobile composition reuses cache"
            );
            assert!(!state.reveal_navigation_workspace);
            assert!(!state.reveal_focused_workspace);
        }
        state.mode = ClientShellMode::Terminal;
        state.navigate_workspace_id = None;
        let mut metadata = state.endpoints[1].snapshot.clone().unwrap();
        metadata.revision += 1;
        stamp(&mut metadata.workspaces[0], "different", "project", true);
        state.cache_endpoint_snapshot(&remote, metadata);
        assert!(state.grouped_projection_dirty);
        state.compose(120, 50).unwrap();
        assert_eq!(state.grouped_projection_rebuilds, builds + 1);
        assert_ne!(state.grouped_rows, rows);
        state.set_endpoint_status(&remote, ClientEndpointStatus::Reconnecting);
        assert!(state.grouped_projection_dirty);
        state.compose(120, 50).unwrap();
        assert_eq!(state.grouped_projection_rebuilds, builds + 2);
        state.set_endpoint_catalog(&[]);
        assert!(state.grouped_projection_dirty);
        state.compose(120, 50).unwrap();
        assert_eq!(state.grouped_projection_rebuilds, builds + 3);
        let mut config = Config::default();
        config.ui.sidebar.grouping.enabled = true;
        config.ui.sidebar.grouping.org_id = "other_org_key".into();
        state.apply_client_live_config(&config, &[], &[]);
        assert!(state.grouped_projection_dirty);
        state.compose(120, 50).unwrap();
        assert_eq!(state.grouped_projection_rebuilds, builds + 4);
        let before = state.grouped_projection_rebuilds;
        state.apply_client_live_config(&config, &[], &[]);
        state.compose(120, 50).unwrap();
        assert_eq!(
            state.grouped_projection_rebuilds, before,
            "unchanged grouping config reuses cache"
        );
        config.ui.sidebar.grouping.enabled = false;
        state.apply_client_live_config(&config, &[], &["ui".into()]);
        state.compose(120, 50).unwrap();
        assert!(
            state.config.grouping.enabled,
            "invalid UI keeps prior grouping"
        );
        assert_eq!(state.grouped_projection_rebuilds, before);
    }
}
