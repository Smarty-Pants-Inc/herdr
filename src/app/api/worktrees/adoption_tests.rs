use super::super::tests::{app_with_parent, create_committed_repo, run_git};
use super::*;
use crate::platform::ForegroundProcess;
use crate::workspace::Workspace;

struct Fixture {
    repo: PathBuf,
    checkout: PathBuf,
    app: App,
    source: WorktreeSource,
}

impl Fixture {
    fn new() -> Self {
        let repo = create_committed_repo("foreground-worktree-adoption");
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
        let mut app = app_with_parent(&repo);
        let mut child = Workspace::test_new("nested-shell");
        child.identity_cwd = repo.clone();
        app.state.workspaces.push(child);
        app.state.ensure_test_terminals();
        let source = app
            .resolve_worktree_source(None, Some(repo.display().to_string()))
            .unwrap();
        Self {
            repo,
            checkout,
            app,
            source,
        }
    }

    fn observation(&self, index: usize) -> ForegroundCheckout {
        let workspace = &self.app.state.workspaces[index];
        let tab = &workspace.tabs[0];
        ForegroundCheckout {
            workspace: workspace.id.clone(),
            tab: tab.number,
            pane: tab.root_pane,
            terminal: tab.terminal_id(tab.root_pane).unwrap().clone(),
            shell: 100,
            shell_birth: (10, 0),
            root_job: shell_job(100),
            shell_cwd: self.repo.clone(),
            reported_cwd: self.repo.clone(),
            foreground_cwd: self.checkout.clone(),
            foreground_birth: (20, 0),
            session_processes: vec![100, 200],
            treehouse: None,
            job: shell_job(200),
        }
    }

    fn lookup(&self, observed: ForegroundCheckout) -> Result<Option<usize>, ApiFailure> {
        self.app
            .lookup_worktree_checkout_with(&self.source, &self.checkout, |_, index, _| {
                Ok((index == 1).then(|| observed.clone()))
            })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        crate::app::api::test_support::shutdown_test_runtimes(&mut self.app);
        let _ = std::fs::remove_dir_all(&self.repo);
    }
}

fn shell_job(pid: u32) -> ForegroundJob {
    ForegroundJob {
        process_group_id: pid,
        processes: vec![ForegroundProcess {
            pid,
            name: "sh".into(),
            argv0: Some("/bin/sh".into()),
            argv: Some(vec!["/bin/sh".into(), "-i".into()]),
            cmdline: Some("/bin/sh -i".into()),
        }],
    }
}

#[test]
fn nested_shell_lookup_uses_foreground_checkout_without_changing_endpoint() {
    let fixture = Fixture::new();
    let before = fixture.observation(1);
    let count = fixture.app.state.workspaces.len();
    // Characterize the retained failure: root-shell cwd is still the parent.
    assert_eq!(
        fixture
            .app
            .open_workspace_idx_for_checkout(&fixture.checkout),
        None
    );
    assert_eq!(fixture.lookup(before.clone()).unwrap(), Some(1));
    assert_eq!(fixture.observation(1), before);
    assert_eq!(fixture.app.state.workspaces.len(), count);
    assert!(fixture.app.state.workspaces[1].worktree_space().is_none());
    assert_eq!(fixture.app.terminal_runtimes.len(), 0);
    fixture.app.state.assert_invariants_for_test();
}

// Exercise the real producer, not only a supplied cwd observation. These are
// the platforms with the native foreground-shell contract; policy tests below
// remain platform independent.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn nested_shell_native_list_and_open_preserve_process_and_terminal() {
    let mut fixture = Fixture::new();
    let old_terminal = fixture.observation(1).terminal;
    fixture.app.state.workspaces.pop();
    fixture.app.state.terminals.remove(&old_terminal);
    fixture.app.state.default_shell = "/bin/sh".into();
    let index = fixture
        .app
        .create_workspace_with_options(fixture.repo.clone(), false)
        .unwrap();
    assert_eq!(index, 1);
    // Real workspaces discover Git identity at creation; test_new does not.
    // A cold parent fixture otherwise makes --cwd select the nested workspace
    // as its source parent, which adoption must refuse.
    let parent_space = crate::workspace::git_space_metadata(&fixture.repo).unwrap();
    assert_eq!(
        fixture.app.find_parent_workspace_for_space(&parent_space),
        Some(index)
    );
    fixture.app.state.workspaces[0].cached_git_space = Some(parent_space.clone());
    assert_eq!(
        fixture.app.find_parent_workspace_for_space(&parent_space),
        Some(0)
    );
    let tab = &fixture.app.state.workspaces[index].tabs[0];
    let terminal = tab.terminal_id(tab.root_pane).unwrap().clone();
    // Quote the nested shell body as one POSIX argument; paths may contain apostrophes.
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
    let body = format!(
        "cd -- {} && exec /bin/sh -i",
        quote(&fixture.checkout.display().to_string())
    );
    let command = format!("/bin/sh -c {}\n", quote(&body));
    fixture
        .app
        .terminal_runtimes
        .get(&terminal)
        .unwrap()
        .try_send_bytes(bytes::Bytes::from(command))
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let before = loop {
        if let Ok(Some(observed)) =
            fixture
                .app
                .foreground_checkout(&fixture.source, index, &fixture.checkout)
        {
            if observed.foreground_cwd == fixture.checkout {
                break observed;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "nested foreground shell did not settle"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(before.shell_cwd, fixture.repo);
    assert_eq!(before.reported_cwd, fixture.repo);
    assert_ne!(before.shell, before.job.process_group_id);
    assert_eq!(
        fixture
            .app
            .open_workspace_idx_for_checkout(&fixture.checkout),
        None
    );
    let workspace_id = fixture.app.public_workspace_id(index);
    let pane_id = fixture.app.public_pane_id(index, before.pane).unwrap();
    let tab_id = fixture.app.public_tab_id(index, 0).unwrap();
    let listed = fixture.app.handle_worktree_list(
        "list".into(),
        crate::api::schema::WorktreeListParams {
            workspace_id: None,
            cwd: Some(fixture.repo.display().to_string()),
            trust_repository: false,
        },
    );
    let listed: crate::api::schema::SuccessResponse = serde_json::from_str(&listed)
        .unwrap_or_else(|error| panic!("worktree.list: {error}; response: {listed}"));
    let crate::api::schema::ResponseResult::WorktreeList { worktrees, .. } = listed.result else {
        panic!("expected list");
    };
    assert_eq!(
        worktrees
            .iter()
            .find(|wt| Path::new(&wt.path) == fixture.checkout)
            .unwrap()
            .open_workspace_id
            .as_ref(),
        Some(&workspace_id)
    );
    assert!(fixture.app.state.workspaces[index]
        .worktree_space()
        .is_none());
    let opened = fixture.app.handle_worktree_open(
        "open".into(),
        crate::api::schema::WorktreeOpenParams {
            workspace_id: None,
            cwd: Some(fixture.repo.display().to_string()),
            path: Some(fixture.checkout.display().to_string()),
            branch: None,
            label: None,
            focus: false,
            trust_repository: false,
        },
    );
    let opened: crate::api::schema::SuccessResponse = serde_json::from_str(&opened)
        .unwrap_or_else(|error| panic!("worktree.open: {error}; response: {opened}"));
    let crate::api::schema::ResponseResult::WorktreeOpened {
        workspace,
        tab,
        root_pane,
        worktree,
        already_open,
    } = opened.result
    else {
        panic!("expected open");
    };
    assert!(already_open);
    assert_eq!(workspace.workspace_id, workspace_id);
    assert_eq!(tab.tab_id, tab_id);
    assert_eq!(root_pane.pane_id, pane_id);
    assert_eq!(root_pane.terminal_id, terminal.as_str());
    assert_eq!(worktree.open_workspace_id, Some(workspace_id));
    assert_eq!(fixture.app.state.workspaces.len(), 2);
    assert_eq!(fixture.app.terminal_runtimes.len(), 1);
    assert_eq!(
        fixture
            .app
            .foreground_checkout(&fixture.source, index, &fixture.checkout)
            .unwrap(),
        Some(before)
    );
    fixture.app.state.assert_invariants_for_test();
}

fn treehouse_observation(fixture: &Fixture) -> ForegroundCheckout {
    let mut observed = fixture.observation(1);
    let root = crate::platform::WorktreeProcessIdentity {
        parent_pid: 1,
        process_group: 100,
        session: 100,
        terminal: 42,
        birth: observed.shell_birth,
        executable: PathBuf::from("/bin/bash"),
    };
    let mut process = root.clone();
    process.parent_pid = 100;
    process.process_group = 150;
    process.birth = (15, 0);
    process.executable = PathBuf::from("/installed/treehouse");
    let mut child = root.clone();
    child.parent_pid = 150;
    child.process_group = 200;
    child.birth = observed.foreground_birth;
    observed.session_processes = vec![100, 150, 200];
    observed.treehouse = Some(TreehouseIntermediary {
        pid: 150,
        root,
        process,
        child,
        job: ForegroundJob {
            process_group_id: 150,
            processes: vec![ForegroundProcess {
                pid: 150,
                name: "treehouse".into(),
                argv0: Some("/installed/treehouse".into()),
                argv: Some(vec!["/installed/treehouse".into(), "get".into()]),
                cmdline: Some("/installed/treehouse get".into()),
            }],
        },
        invoked_executable: PathBuf::from("/installed/treehouse"),
        cwd: fixture.repo.clone(),
    });
    observed
}

#[test]
fn treehouse_get_ancestry_preserves_existing_checkout_identity() {
    let fixture = Fixture::new();
    let before = treehouse_observation(&fixture);
    assert!(supported_session_shape(&before));
    assert_eq!(fixture.lookup(before).unwrap(), Some(1));
    assert_eq!(fixture.app.state.workspaces.len(), 2);
    assert!(fixture.app.state.workspaces[1].worktree_space().is_none());
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn treehouse_get_rejects_unrelated_ancestry_helpers_agents_and_extra_processes() {
    let fixture = Fixture::new();
    let before = treehouse_observation(&fixture);
    let mut invalid = Vec::new();
    for change in 0..14 {
        let mut item = before.clone();
        let helper = item.treehouse.as_mut().unwrap();
        match change {
            0 => helper.process.parent_pid = 999,
            1 => helper.child.parent_pid = item.shell,
            2 => helper.process.session = 999,
            3 => helper.child.session = 999,
            4 => helper.process.terminal += 1,
            5 => helper.child.terminal += 1,
            6 => helper.process.process_group = item.shell,
            7 => helper.job.processes[0].name = "pi".into(),
            8 => helper.job.processes[0].argv.as_mut().unwrap()[1] = "run".into(),
            9 => helper.job.processes[0].argv.as_mut().unwrap().push("pi".into()),
            10 => helper.invoked_executable = PathBuf::from("/other/treehouse"),
            11 => helper.cwd = fixture.checkout.clone(),
            12 => item.session_processes.push(300),
            _ => item.treehouse = None,
        }
        invalid.push(item);
    }
    for item in invalid {
        assert_eq!(
            fixture.lookup(item).unwrap_err().code,
            "worktree_adoption_unavailable"
        );
    }
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn treehouse_intermediary_must_remain_identical_on_second_observation() {
    let fixture = Fixture::new();
    let before = treehouse_observation(&fixture);
    for change in 0..5 {
        let mut after = before.clone();
        let helper = after.treehouse.as_mut().unwrap();
        match change {
            0 => helper.process.birth.0 += 1,
            1 => helper.child.parent_pid = 999,
            2 => helper.process.executable = PathBuf::from("/replaced/treehouse"),
            3 => helper.job.processes[0].argv = None,
            _ => after.treehouse = None,
        }
        let mut calls = 0;
        let result = fixture.app.lookup_worktree_checkout_with(
            &fixture.source,
            &fixture.checkout,
            |_, index, _| {
                if index != 1 {
                    return Ok(None);
                }
                calls += 1;
                Ok(Some(if calls == 1 {
                    before.clone()
                } else {
                    after.clone()
                }))
            },
        );
        assert_eq!(result.unwrap_err().code, "worktree_adoption_unavailable");
    }
}

#[cfg(unix)]
#[test]
fn treehouse_executable_requires_exact_installed_path_not_a_name_or_wrapper() {
    let fixture = Fixture::new();
    let installed = fixture.repo.join("installed");
    let wrappers = fixture.repo.join("wrappers");
    let unrelated = fixture.repo.join("unrelated");
    for directory in [&installed, &wrappers, &unrelated] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("treehouse"), "fixture, never executed").unwrap();
    }
    let executable = installed.join("treehouse");
    let path = std::env::join_paths([&wrappers, &installed]).unwrap();
    assert!(treehouse_executable_on_path(&executable, &path));
    assert!(!treehouse_executable_on_path(
        &unrelated.join("treehouse"),
        &path
    ));
    assert!(!treehouse_executable_on_path(Path::new("treehouse"), &path));
    assert!(!treehouse_executable_on_path(
        &executable,
        std::ffi::OsStr::new(".")
    ));
    let links = fixture.repo.join("links");
    std::fs::create_dir(&links).unwrap();
    std::os::unix::fs::symlink(&executable, links.join("treehouse")).unwrap();
    let path = std::env::join_paths([&wrappers, &links]).unwrap();
    assert!(treehouse_executable_on_path(&executable, &path));
    std::fs::remove_file(links.join("treehouse")).unwrap();
    std::os::unix::fs::symlink(unrelated.join("treehouse"), links.join("treehouse")).unwrap();
    assert!(!treehouse_executable_on_path(&executable, &path));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn native_worktree_process_identity_matches_current_executable_and_birth() {
    let pid = std::process::id();
    let before = crate::platform::worktree_process_identity(pid).unwrap();
    assert!(before.parent_pid > 0);
    assert!(before.session > 0);
    assert!(before.process_group > 0);
    assert_eq!(
        Some(before.birth),
        crate::platform::process_birth_identity(pid)
    );
    assert_eq!(
        before.executable,
        std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    );
    assert_eq!(Some(before), crate::platform::worktree_process_identity(pid));
    assert!(crate::platform::worktree_process_identity(u32::MAX).is_none());
}

#[test]
fn preallocated_checkout_keeps_existing_lookup_without_foreground_evidence() {
    let mut fixture = Fixture::new();
    let terminal = fixture.observation(1).terminal;
    fixture.app.state.terminals.get_mut(&terminal).unwrap().cwd = fixture.checkout.clone();
    assert_eq!(
        fixture
            .app
            .open_workspace_idx_for_checkout(&fixture.checkout),
        Some(1)
    );
    let result = fixture.app.lookup_worktree_checkout_with(
        &fixture.source,
        &fixture.checkout,
        |_, index, _| {
            assert_ne!(index, 1);
            Ok(None)
        },
    );
    assert_eq!(result.unwrap(), Some(1));
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn foreground_adoption_rejects_changed_or_missing_second_observation() {
    let fixture = Fixture::new();
    let before = fixture.observation(1);
    let mut changed = Vec::new();
    let mut item = before.clone();
    item.shell_birth.0 += 1;
    changed.push(Some(item));
    let mut item = before.clone();
    item.foreground_birth.0 += 1;
    changed.push(Some(item));
    let mut item = before.clone();
    item.terminal = crate::terminal::TerminalId::alloc();
    changed.push(Some(item));
    let mut item = before.clone();
    item.workspace.push_str("-reused");
    changed.push(Some(item));
    let mut item = before.clone();
    item.tab += 1;
    changed.push(Some(item));
    let mut item = before.clone();
    item.pane = crate::layout::PaneId::alloc();
    changed.push(Some(item));
    let mut item = before.clone();
    item.foreground_cwd = fixture.repo.clone();
    changed.push(Some(item));
    let mut item = before.clone();
    item.job.processes[0].argv = None;
    changed.push(Some(item));
    changed.push(None);
    for after in changed {
        let mut calls = 0;
        let result = fixture.app.lookup_worktree_checkout_with(
            &fixture.source,
            &fixture.checkout,
            |_, index, _| {
                if index != 1 {
                    return Ok(None);
                }
                calls += 1;
                Ok(if calls == 1 {
                    Some(before.clone())
                } else {
                    after.clone()
                })
            },
        );
        assert_eq!(result.unwrap_err().code, "worktree_adoption_unavailable");
    }
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn foreground_adoption_rejects_wrong_git_paths_and_endpoint_claims() {
    let fixture = Fixture::new();
    let before = fixture.observation(1);
    let other = create_committed_repo("foreground-adoption-other-repo");
    let mut invalid = Vec::new();
    let mut item = before.clone();
    item.shell_cwd = other.clone();
    invalid.push(item);
    let mut item = before.clone();
    item.reported_cwd = fixture.checkout.clone();
    invalid.push(item);
    let mut item = before.clone();
    item.foreground_cwd = fixture.repo.clone();
    invalid.push(item);
    let mut item = before.clone();
    item.workspace.push_str("-wrong");
    invalid.push(item);
    let mut item = before.clone();
    item.terminal = crate::terminal::TerminalId::alloc();
    invalid.push(item);
    let mut item = before.clone();
    item.shell = 0;
    invalid.push(item);
    let mut item = before.clone();
    item.session_processes.push(300);
    invalid.push(item);
    let mut item = before.clone();
    item.session_processes.clear();
    invalid.push(item);
    let mut item = before.clone();
    item.job.processes.push(item.job.processes[0].clone());
    invalid.push(item);
    let mut item = before.clone();
    item.job.processes[0].name = "pi".into();
    invalid.push(item);
    for item in invalid {
        assert_eq!(
            fixture.lookup(item).unwrap_err().code,
            "worktree_adoption_unavailable"
        );
    }
    let _ = std::fs::remove_dir_all(other);
}

#[cfg(unix)]
#[test]
fn foreground_observation_must_use_the_physical_checkout_not_a_symlink_alias() {
    let fixture = Fixture::new();
    let alias = fixture.repo.join("alias");
    std::os::unix::fs::symlink(&fixture.checkout, &alias).unwrap();
    assert_eq!(std::fs::canonicalize(&alias).unwrap(), fixture.checkout);
    let mut observation = fixture.observation(1);
    observation.foreground_cwd = alias;
    assert_eq!(
        fixture.lookup(observation).unwrap_err().code,
        "worktree_adoption_unavailable"
    );
}

#[test]
fn foreground_adoption_rejects_membership_and_agent_conflicts() {
    let mut fixture = Fixture::new();
    let observation = fixture.observation(1);
    fixture.app.state.workspaces[1].worktree_space =
        Some(crate::workspace::WorktreeSpaceMembership {
            key: fixture.source.repo_key.clone(),
            label: "parent".into(),
            repo_root: fixture.repo.clone(),
            checkout_path: fixture.repo.clone(),
            is_linked_worktree: false,
        });
    assert!(fixture.lookup(observation.clone()).is_err());
    fixture.app.state.workspaces[1].worktree_space = None;
    fixture.app.state.workspaces[1].identity_cwd = fixture.checkout.clone();
    assert!(fixture.lookup(observation.clone()).is_err());
    fixture.app.state.workspaces[1].identity_cwd = fixture.repo.clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&observation.terminal)
        .unwrap()
        .agent_name = Some("existing-agent".into());
    assert!(fixture.lookup(observation).is_err());
}

#[test]
fn foreground_adoption_rejects_two_workspace_candidates() {
    let mut fixture = Fixture::new();
    let mut duplicate = Workspace::test_new("duplicate-shell");
    duplicate.identity_cwd = fixture.repo.clone();
    fixture.app.state.workspaces.push(duplicate);
    fixture.app.state.ensure_test_terminals();
    let result = fixture.app.lookup_worktree_checkout_with(
        &fixture.source,
        &fixture.checkout,
        |_, index, _| Ok((index > 0).then(|| fixture.observation(index))),
    );
    assert_eq!(result.unwrap_err().code, "worktree_adoption_ambiguous");
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn foreground_adoption_rejects_duplicate_registered_owner() {
    let mut fixture = Fixture::new();
    let mut duplicate = Workspace::test_new("preallocated-owner");
    duplicate.identity_cwd = fixture.checkout.clone();
    fixture.app.state.workspaces.push(duplicate);
    fixture.app.state.ensure_test_terminals();
    assert_eq!(
        fixture.lookup(fixture.observation(1)).unwrap_err().code,
        "worktree_adoption_ambiguous"
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_foreground_adoption_preserves_lookup_and_open_with_an_extra_parent() {
    let mut fixture = Fixture::new();
    // Production lookup must allow ordinary opening despite the extra parent-cwd
    // workspace. No synthetic observer or foreground evidence is supplied here.
    assert_eq!(
        fixture
            .app
            .lookup_worktree_checkout(&fixture.source, &fixture.checkout)
            .unwrap(),
        None
    );
    let mut target = Workspace::test_new("preallocated-target");
    target.identity_cwd = fixture.checkout.clone();
    let target_id = target.id.clone();
    fixture.app.state.workspaces.push(target);
    fixture.app.state.ensure_test_terminals();

    // First open is directly preallocated; the second is registered. Place the
    // extra parent both before and after the target so order cannot hide R1.
    for index in [2, 1] {
        if index == 1 {
            fixture.app.state.workspaces.swap(1, 2);
        }
        assert_eq!(
            fixture
                .app
                .lookup_worktree_checkout(&fixture.source, &fixture.checkout)
                .unwrap(),
            Some(index)
        );
        let response = fixture.app.handle_worktree_open(
            "unsupported-foreground".into(),
            crate::api::schema::WorktreeOpenParams {
                workspace_id: None,
                cwd: Some(fixture.repo.display().to_string()),
                path: Some(fixture.checkout.display().to_string()),
                branch: None,
                label: None,
                focus: false,
                trust_repository: false,
            },
        );
        let response: crate::api::schema::SuccessResponse =
            serde_json::from_str(&response).unwrap();
        let crate::api::schema::ResponseResult::WorktreeOpened {
            workspace,
            worktree,
            already_open,
            ..
        } = response.result
        else {
            panic!("expected existing workspace");
        };
        assert!(already_open);
        assert_eq!(workspace.workspace_id, target_id);
        assert_eq!(worktree.open_workspace_id.as_ref(), Some(&target_id));
        assert_eq!(fixture.app.state.workspaces.len(), 3);
        assert_eq!(fixture.app.terminal_runtimes.len(), 0);
        fixture.app.state.assert_invariants_for_test();
    }

    let mut duplicate = Workspace::test_new("duplicate-target");
    duplicate.identity_cwd = fixture.checkout.clone();
    fixture.app.state.workspaces.push(duplicate);
    fixture.app.state.ensure_test_terminals();
    assert_eq!(
        fixture
            .app
            .lookup_worktree_checkout(&fixture.source, &fixture.checkout)
            .unwrap_err()
            .code,
        "worktree_adoption_ambiguous"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn unavailable_parent_shell_refuses_open_before_membership_or_allocation() {
    let mut fixture = Fixture::new();
    // An unregistered parent-cwd workspace without a live foreground observation
    // could be the retained shell. Missing evidence must not authorize a second workspace.
    let response = fixture.app.handle_worktree_open(
        "missing-foreground".into(),
        crate::api::schema::WorktreeOpenParams {
            workspace_id: None,
            cwd: Some(fixture.repo.display().to_string()),
            path: Some(fixture.checkout.display().to_string()),
            branch: None,
            label: None,
            focus: false,
            trust_repository: false,
        },
    );
    let response: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(response.error.code, "worktree_adoption_unavailable");
    assert_eq!(fixture.app.state.workspaces.len(), 2);
    assert!(fixture
        .app
        .state
        .workspaces
        .iter()
        .all(|ws| ws.worktree_space().is_none()));
    assert_eq!(fixture.app.terminal_runtimes.len(), 0);
    fixture.app.state.assert_invariants_for_test();
}

#[test]
fn mismatched_repository_source_and_reused_terminal_are_rejected() {
    let mut fixture = Fixture::new();
    let before = fixture.observation(1);
    fixture.source.repo_key.push_str("-different-repository");
    assert!(fixture.lookup(before.clone()).is_err());
    fixture.source.repo_key = crate::workspace::git_space_metadata(&fixture.repo)
        .unwrap()
        .key;
    let mut other = Workspace::test_new("other-attachment");
    let root = other.tabs[0].root_pane;
    other.tabs[0]
        .panes
        .get_mut(&root)
        .unwrap()
        .attached_terminal_id = before.terminal.clone();
    fixture.app.state.workspaces.push(other);
    assert!(fixture.lookup(before).is_err());
}

#[test]
fn shell_contract_rejects_commands_helpers_and_agents() {
    let job = shell_job(200);
    assert!(single_shell(&job));
    let mut command = job.clone();
    command.processes[0].argv = Some(vec!["sh".into(), "-c".into(), "pi".into()]);
    assert!(!single_shell(&command));
    let mut helper = job.clone();
    helper.processes.push(job.processes[0].clone());
    assert!(!single_shell(&helper));
    let mut agent = job.clone();
    agent.processes[0].argv = Some(vec!["pi".into()]);
    assert!(!single_shell(&agent));
    let mut absent = job;
    absent.processes[0].argv = None;
    assert!(!single_shell(&absent));
}
