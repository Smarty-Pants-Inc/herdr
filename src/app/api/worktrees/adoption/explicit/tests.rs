use super::*;
use crate::app::api::worktrees::tests::{app_with_parent, create_committed_repo, run_git};
use crate::platform::ForegroundProcess;
use crate::workspace::{Workspace, WorktreeSpaceMembership};

struct Fixture {
    app: App,
    repo: PathBuf,
    params: WorktreeAdoptParams,
    observations: Vec<Vec<PlainCheckout>>,
}

impl Fixture {
    fn new() -> Self {
        let repo = create_committed_repo("explicit-worktree-adoption");
        let checkout = repo.join("linked");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                checkout.to_str().unwrap(),
            ],
        );
        let repo = std::fs::canonicalize(repo).unwrap();
        let checkout = std::fs::canonicalize(checkout).unwrap();
        let git = crate::workspace::git_space_metadata(&checkout).unwrap();
        let membership = WorktreeSpaceMembership {
            key: git.key,
            label: "repo".into(),
            repo_root: repo.clone(),
            checkout_path: checkout.clone(),
            is_linked_worktree: true,
        };
        let mut app = app_with_parent(&repo);
        app.state.workspaces[0].worktree_space = Some(WorktreeSpaceMembership {
            checkout_path: repo.clone(),
            is_linked_worktree: false,
            ..membership.clone()
        });
        let mut target = Workspace::test_new("moved owner");
        target.identity_cwd = checkout.clone();
        let mut known = Workspace::test_new("retained lanes");
        known.identity_cwd = checkout.clone();
        known.test_add_tab(Some("second"));
        known.test_add_tab(Some("third"));
        known.worktree_space = Some(membership);
        app.state.workspaces.extend([target, known]);
        app.state.ensure_test_terminals();
        let mut observations = vec![vec![], vec![], vec![]];
        for (index, records) in observations.iter_mut().enumerate().skip(1) {
            for (tab_index, tab) in app.state.workspaces[index].tabs.iter().enumerate() {
                let pid = (100 * index + tab_index) as u32;
                let birth = [u64::from(pid), 0];
                records.push(PlainCheckout {
                    binding: WorktreeAdoptPane {
                        pane_id: app.public_pane_id(index, tab.root_pane).unwrap(),
                        tab_id: app.public_tab_id(index, tab_index).unwrap(),
                        terminal_id: tab.terminal_id(tab.root_pane).unwrap().to_string(),
                        shell_pid: pid,
                        shell_birth: birth,
                    },
                    native: crate::platform::WorktreeProcessIdentity {
                        parent_pid: 1,
                        process_group: pid,
                        session: pid,
                        terminal: u64::from(pid),
                        birth: (birth[0], birth[1]),
                        executable: PathBuf::from("/bin/bash"),
                    },
                    job: ForegroundJob {
                        process_group_id: pid,
                        processes: vec![ForegroundProcess {
                            pid,
                            name: "bash".into(),
                            argv0: Some("/bin/bash".into()),
                            argv: Some(vec!["/bin/bash".into()]),
                            cmdline: Some("/bin/bash".into()),
                        }],
                    },
                    cwd: checkout.clone(),
                    reported_cwd: checkout.clone(),
                    foreground_cwd: checkout.clone(),
                    session_processes: vec![pid],
                    worker: None,
                });
            }
        }
        let params = WorktreeAdoptParams {
            parent_workspace_id: app.public_workspace_id(0),
            workspace_id: app.public_workspace_id(1),
            path: checkout.display().to_string(),
            repo_root: repo.display().to_string(),
            repo_key: app.state.workspaces[0]
                .worktree_space
                .as_ref()
                .unwrap()
                .key
                .clone(),
            target: observations[1][0].binding.clone(),
            known_workspace_id: app.public_workspace_id(2),
            known_panes: observations[2]
                .iter()
                .map(|pane| pane.binding.clone())
                .collect(),
        };
        Self {
            app,
            repo,
            params,
            observations,
        }
    }

    fn adopt(&mut self) -> serde_json::Value {
        let observations = &self.observations;
        serde_json::from_str(&self.app.handle_worktree_adopt_with(
            "test".into(),
            self.params.clone(),
            |_, index| Ok(observations[index].clone()),
        ))
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        crate::app::api::test_support::shutdown_test_runtimes(&mut self.app);
        let _ = std::fs::remove_dir_all(&self.repo);
    }
}

#[test]
fn explicit_adoption_preserves_four_panes_and_replay_keeps_path_lookup_ambiguous() {
    let mut fixture = Fixture::new();
    let before = fixture.app.state.workspaces[2].worktree_space.clone();
    let terminals = fixture.app.state.terminals.len();
    let active = fixture.app.state.active;
    for _ in 0..2 {
        let reply = fixture.adopt();
        assert_eq!(reply["result"]["type"], "worktree_opened", "{reply}");
        let path = reply["result"]["worktree"]["path"].as_str().unwrap();
        assert_eq!(
            std::fs::canonicalize(path).unwrap(),
            PathBuf::from(&fixture.params.path)
        );
        assert_eq!(reply["result"]["already_open"], true);
        assert_eq!(
            reply["result"]["workspace"]["workspace_id"],
            fixture.params.workspace_id
        );
        assert_eq!(
            reply["result"]["root_pane"]["terminal_id"],
            fixture.params.target.terminal_id
        );
        assert_eq!(fixture.app.state.workspaces[2].worktree_space, before);
        assert_eq!(fixture.app.state.workspaces[2].tabs.len(), 3);
        assert_eq!(fixture.app.state.workspaces.len(), 3);
        assert_eq!(fixture.app.state.terminals.len(), terminals);
        assert_eq!(fixture.app.state.active, active);
        fixture.app.state.assert_invariants_for_test();
        let source = fixture.app.worktree_source_from_workspace(0).unwrap();
        let result = fixture.app.lookup_worktree_checkout_with(
            &source,
            Path::new(&fixture.params.path),
            |_, _, _| Ok(None),
        );
        assert_eq!(result.unwrap_err().code, "worktree_adoption_ambiguous");
    }
}

#[test]
fn explicit_adoption_rejects_wrong_expected_identity_and_extra_known_panes() {
    for change in 0..12 {
        let mut fixture = Fixture::new();
        match change {
            0 => fixture.params.target.shell_birth[0] += 1,
            1 => fixture.params.target.terminal_id.push('x'),
            2 => fixture.params.target.pane_id = fixture.params.known_panes[0].pane_id.clone(),
            3 => {
                fixture.params.known_panes.pop();
            }
            4 => fixture
                .params
                .known_panes
                .push(fixture.params.known_panes[0].clone()),
            5 => fixture.params.parent_workspace_id = fixture.params.workspace_id.clone(),
            6 => fixture.params.known_workspace_id = fixture.params.workspace_id.clone(),
            7 => fixture.params.target.tab_id.push('x'),
            8 => fixture.params.path = fixture.repo.display().to_string(),
            9 => fixture.params.repo_root = fixture.params.path.clone(),
            10 => fixture.params.repo_key.push('x'),
            _ => fixture.app.state.workspaces[2]
                .worktree_space
                .as_mut()
                .unwrap()
                .key
                .push('x'),
        }
        assert_eq!(
            fixture.adopt()["error"]["code"],
            "worktree_adoption_unavailable"
        );
        assert!(fixture.app.state.workspaces[1].worktree_space.is_none());
        fixture.app.state.assert_invariants_for_test();
    }
}

#[test]
fn explicit_adoption_rejects_agents_third_matches_and_extra_destination_tabs() {
    for change in 0..5 {
        let mut fixture = Fixture::new();
        match change {
            0 | 1 => {
                let index = if change == 0 { 1 } else { 2 };
                let tab = &fixture.app.state.workspaces[index].tabs[0];
                let terminal = tab.terminal_id(tab.root_pane).unwrap().clone();
                fixture
                    .app
                    .state
                    .terminals
                    .get_mut(&terminal)
                    .unwrap()
                    .agent_name = Some("pi".into());
            }
            2 => {
                let mut third = Workspace::test_new("unexpected third view");
                third.identity_cwd = PathBuf::from(&fixture.params.path);
                fixture.app.state.workspaces.push(third);
            }
            3 => {
                let mut other_parent = Workspace::test_new("unregistered duplicate parent");
                other_parent.identity_cwd = fixture.repo.clone();
                fixture.app.state.workspaces.push(other_parent);
            }
            _ => {
                fixture.app.state.workspaces[1].test_add_tab(None);
            }
        }
        fixture.app.state.ensure_test_terminals();
        assert_eq!(
            fixture.adopt()["error"]["code"],
            "worktree_adoption_unavailable"
        );
        assert!(fixture.app.state.workspaces[1].worktree_space.is_none());
        fixture.app.state.assert_invariants_for_test();
    }
}

#[test]
fn explicit_methods_do_not_search_another_server_for_a_missing_target() {
    let target_server = Fixture::new();
    let mut other_server = Fixture::new();
    let response = other_server.app.handle_worktree_adopt_with(
        "wrong-server".into(),
        target_server.params.clone(),
        |_, _| panic!("missing target must refuse before native observation"),
    );
    let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(reply["error"]["code"], "pane_not_found");
    let params = WorktreeVerifyAdoptionParams {
        binding: target_server.params.clone(),
        agent_session_id: None,
    };
    let response = other_server
        .app
        .verify_adoption_with("wrong-server".into(), &params, |_, _| {
            panic!("missing target must refuse before native observation")
        });
    let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(reply["error"]["code"], "pane_not_found");
    assert!(other_server.app.state.workspaces[1]
        .worktree_space
        .is_none());
    target_server.app.state.assert_invariants_for_test();
    other_server.app.state.assert_invariants_for_test();
}

#[test]
fn explicit_verification_is_read_only_and_requires_existing_membership() {
    let mut fixture = Fixture::new();
    let params = WorktreeVerifyAdoptionParams {
        binding: fixture.params.clone(),
        agent_session_id: None,
    };
    let observations = fixture.observations.clone();
    let response = fixture
        .app
        .verify_adoption_with("test".into(), &params, |_, index| {
            Ok(observations[index].clone())
        });
    let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(reply["error"]["code"], "worktree_adoption_unavailable");
    assert!(fixture.app.state.workspaces[1].worktree_space.is_none());
    assert_eq!(fixture.adopt()["result"]["already_open"], true);
    fixture.app.state.session_dirty = false;
    let events = fixture.app.event_hub.events_after(0).len();
    let response = fixture
        .app
        .verify_adoption_with("test".into(), &params, |_, index| {
            Ok(observations[index].clone())
        });
    let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(reply["result"]["type"], "worktree_list");
    assert_eq!(reply["result"]["worktrees"].as_array().unwrap().len(), 2);
    assert_eq!(
        reply["result"]["worktrees"][0]["open_workspace_id"],
        params.binding.workspace_id
    );
    assert_eq!(
        reply["result"]["worktrees"][1]["open_workspace_id"],
        params.binding.known_workspace_id
    );
    assert!(!fixture.app.state.session_dirty);
    assert_eq!(fixture.app.event_hub.events_after(0).len(), events);
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn explicit_verification_allows_only_the_expected_live_pi_and_its_tool_descendants() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.adopt()["result"]["already_open"], true);
    let tab = &fixture.app.state.workspaces[1].tabs[0];
    let terminal = tab.terminal_id(tab.root_pane).unwrap().clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
        source: "pi".into(),
        agent: "pi".into(),
        session_ref: crate::agent_resume::AgentSessionRef::id("retained-pi").unwrap(),
    });
    let pane = &mut fixture.observations[1][0];
    let mut worker_identity = pane.native.clone();
    worker_identity.parent_pid = pane.binding.shell_pid;
    worker_identity.process_group = 150;
    worker_identity.birth = (150, 0);
    worker_identity.executable = PathBuf::from("/installed/pi");
    let worker_job = ForegroundJob {
        process_group_id: 150,
        processes: vec![ForegroundProcess {
            pid: 150,
            name: "pi".into(),
            argv0: Some("pi".into()),
            argv: Some(vec!["pi".into()]),
            cmdline: Some("pi".into()),
        }],
    };
    let mut tool_identity = pane.native.clone();
    tool_identity.parent_pid = 150;
    tool_identity.birth = (151, 0);
    let mut tool_job = pane.job.clone();
    tool_job.process_group_id = 151;
    tool_job.processes[0].pid = 151;
    pane.session_processes = vec![pane.binding.shell_pid, 150, 151];
    pane.worker = Some(LiveWorker {
        pid: 150,
        session_id: "retained-pi".into(),
        processes: vec![
            (150, worker_identity, worker_job),
            (151, tool_identity, tool_job),
        ],
    });
    let params = WorktreeVerifyAdoptionParams {
        binding: fixture.params.clone(),
        agent_session_id: Some("retained-pi".into()),
    };
    for change in 0..5 {
        let mut params = params.clone();
        let mut observations = fixture.observations.clone();
        match change {
            0 => {}
            1 => params.agent_session_id = Some("unrelated-pi".into()),
            2 => {
                observations[1][0].worker.as_mut().unwrap().processes[1]
                    .1
                    .parent_pid = 100
            }
            3 => {
                observations[1][0].worker.as_mut().unwrap().processes[1]
                    .1
                    .parent_pid = 151
            }
            _ => {
                let worker = observations[1][0].worker.as_mut().unwrap();
                let mut second_agent = worker.processes[0].2.clone();
                second_agent.process_group_id = 151;
                second_agent.processes[0].pid = 151;
                assert_eq!(
                    crate::detect::identify_agent_in_job(&second_agent).map(|(agent, _)| agent),
                    Some(crate::detect::Agent::Pi)
                );
                worker.processes[1].1.executable = PathBuf::from("/installed/pi");
                worker.processes[1].2 = second_agent;
            }
        }
        fixture.app.state.session_dirty = false;
        let events = fixture.app.event_hub.events_after(0).len();
        let response = fixture
            .app
            .verify_adoption_with("test".into(), &params, |_, index| {
                Ok(observations[index].clone())
            });
        let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            reply.get("result").is_some(),
            change == 0,
            "case {change}: {reply}"
        );
        assert!(!fixture.app.state.session_dirty);
        assert_eq!(fixture.app.event_hub.events_after(0).len(), events);
    }
    assert_eq!(
        fixture.adopt()["error"]["code"],
        "worktree_adoption_unavailable"
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_explicit_methods_refuse_before_native_observation() {
    let mut fixture = Fixture::new();
    let adopt = fixture
        .app
        .handle_worktree_adopt("unsupported".into(), fixture.params.clone());
    let verify = fixture.app.handle_worktree_verify_adoption(
        "unsupported".into(),
        WorktreeVerifyAdoptionParams {
            binding: fixture.params.clone(),
            agent_session_id: None,
        },
    );
    for response in [adopt, verify] {
        let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(reply["error"]["code"], "worktree_adoption_unavailable");
        assert!(fixture.app.state.workspaces[1].worktree_space.is_none());
    }
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn expected_pi_session_rejects_competing_raw_claims() {
    let mut fixture = Fixture::new();
    let tab = &fixture.app.state.workspaces[1].tabs[0];
    let id = tab.terminal_id(tab.root_pane).unwrap().clone();
    let terminal = fixture.app.state.terminals.get_mut(&id).unwrap();
    terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
        source: "pi".into(),
        agent: "pi".into(),
        session_ref: crate::agent_resume::AgentSessionRef::id("retained-pi").unwrap(),
    });
    terminal.hook_authority = Some(crate::terminal::state::HookAuthority {
        source: "pi".into(),
        agent_label: "pi".into(),
        state: crate::detect::AgentState::Idle,
        message: None,
        reported_at: std::time::Instant::now(),
        session_ref: Some(crate::agent_resume::AgentSessionRef::id("retained-pi").unwrap()),
    });
    assert!(expected_pi_session(terminal, "retained-pi"));
    terminal.detected_agent = Some(crate::detect::Agent::Claude);
    assert!(!expected_pi_session(terminal, "retained-pi"));
    terminal.detected_agent = None;
    terminal.hook_authority.as_mut().unwrap().session_ref =
        Some(crate::agent_resume::AgentSessionRef::id("other-pi").unwrap());
    assert!(!expected_pi_session(terminal, "retained-pi"));
    terminal.hook_authority = None;
    terminal.restore_managed_agent("competing".into(), crate::detect::Agent::Claude);
    assert!(!expected_pi_session(terminal, "retained-pi"));
}

#[test]
fn explicit_adoption_rejects_native_process_cwd_and_second_observation_changes() {
    for change in 0..7 {
        let mut fixture = Fixture::new();
        let mut calls = 0;
        let observations = &fixture.observations;
        let response = fixture.app.handle_worktree_adopt_with(
            "test".into(),
            fixture.params.clone(),
            |_, index| {
                calls += 1;
                let mut records = observations[index].clone();
                if calls > 2 {
                    match change {
                        0 => records[0].native.parent_pid += 1,
                        1 => records[0].native.session += 1,
                        2 => records[0].native.birth.0 += 1,
                        3 => records[0].session_processes.push(999),
                        4 => records[0].job.processes[0].name = "pi".into(),
                        5 => records[0].cwd = fixture.repo.clone(),
                        _ => records[0].native.terminal += 1,
                    }
                }
                Ok(records)
            },
        );
        let reply: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(reply["error"]["code"], "worktree_adoption_unavailable");
        assert!(fixture.app.state.workspaces[1].worktree_space.is_none());
    }
}
