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

// #4551: membership repins keep workspace/first-Pi identity unchanged, but can
// still change the reported session's fallback project. Use the real deferred
// headless receiver, not a direct membership setter or a fabricated completion.
struct WorktreeRepinFixture {
    server: HeadlessServer,
    directory: std::path::PathBuf,
    alias: std::path::PathBuf,
    repo: std::path::PathBuf,
    checkout: std::path::PathBuf,
    old_project: std::path::PathBuf,
    workspace: usize,
    pane: crate::layout::PaneId,
    session: String,
}

impl Drop for WorktreeRepinFixture {
    fn drop(&mut self) {
        shutdown_test_runtimes(&mut self.server);
        let _ = std::fs::remove_dir_all(&self.directory);
        if let Some(parent) = self.server.client_socket_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn worktree_repin_fixture(create: bool, target: bool, unchanged: bool) -> WorktreeRepinFixture {
    let directory = std::env::temp_dir().join(format!(
        "repin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(directory.join("real")).unwrap();
    // Windows CI spells temp_dir with an 8.3 alias (RUNNER~1) while canonical
    // paths are verbatim. Give Unix the same alias class with a symlink so the
    // assertions below must compare canonical paths on every platform.
    #[cfg(unix)]
    let alias = {
        let alias = directory.join("alias");
        std::os::unix::fs::symlink(directory.join("real"), &alias).unwrap();
        alias
    };
    #[cfg(not(unix))]
    let alias = directory.join("real");
    let repo = alias.join("repo");
    let checkout = alias.join("checkout");
    std::fs::create_dir_all(&repo).unwrap();
    worktree_repin_git(&repo, &["init", "--quiet"]);
    worktree_repin_git(
        &repo,
        &[
            "-c",
            "user.name=Herdr Test",
            "-c",
            "user.email=herdr@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    if !create {
        worktree_repin_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "repin",
                checkout.to_str().unwrap(),
            ],
        );
    }
    let mut server = test_headless_server();
    let space = crate::workspace::git_space_metadata(&repo).unwrap();
    let mut parent = crate::workspace::Workspace::test_new("parent");
    parent.identity_cwd = repo.clone();
    parent.cached_git_space = Some(space.clone());
    server.app.state.workspaces = vec![parent];
    if target {
        let mut child = crate::workspace::Workspace::test_new("existing-target");
        child.identity_cwd = checkout.clone();
        // A workspace may already track the checkout when create finishes.
        // Cached Git identity lets it be found independently of the Pi cwd.
        child.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            // The create checkout is still absent: canonicalize its parent, as
            // Git discovery would report the checkout once it exists.
            checkout_key: canonical_path(&checkout).display().to_string(),
            repo_root: checkout.clone(),
            is_linked_worktree: true,
            ..space
        });
        server.app.state.workspaces.push(child);
    }
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let workspace = usize::from(target);
    let pane = root(&server, workspace, 0);
    let old_project = if unchanged {
        if target {
            checkout.clone()
        } else {
            repo.clone()
        }
    } else {
        alias.join("old-project")
    };
    // Create the unchanged target directory only for open. A create target must
    // remain absent so Git creates it through the actual worker.
    if old_project != checkout {
        std::fs::create_dir_all(&old_project).unwrap();
    }
    cwd(&mut server, pane, &old_project);
    let session = session(
        &mut server,
        pane,
        directory.file_name().unwrap().to_str().unwrap(),
    );
    // Deliberately stale cached cwd: project authority is the runtime's first
    // Pi foreground cwd, not identity_cwd, cached Git discovery, or shell cwd.
    let terminal = server.app.state.workspaces[workspace]
        .terminal_id(pane)
        .unwrap()
        .clone();
    server.app.state.terminals.get_mut(&terminal).unwrap().cwd = repo.clone();
    assert!(server.app.state.workspaces[workspace]
        .worktree_space()
        .is_none());
    WorktreeRepinFixture {
        server,
        directory,
        alias,
        repo,
        checkout,
        old_project,
        workspace,
        pane,
        session,
    }
}

fn canonical_path(path: &std::path::Path) -> std::path::PathBuf {
    crate::worktree::canonical_or_ancestor(path)
}

/// Messages echo a path as given or as canonicalized; accept either spelling.
fn mentions_path(text: &str, path: &std::path::Path) -> bool {
    text.contains(&path.display().to_string())
        || text.contains(&canonical_path(path).display().to_string())
}

fn worktree_repin_git(repo: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn worktree_repin_start(
    fixture: &mut WorktreeRepinFixture,
    create: bool,
    allow: Option<bool>,
) -> std::sync::mpsc::Receiver<String> {
    let mut params = serde_json::json!({
        "workspace_id": fixture.server.app.public_workspace_id(0),
        "path": fixture.checkout,
        "focus": false,
        "label": "intentional repin"
    });
    if create {
        params["branch"] = serde_json::json!("repin");
    }
    if let Some(allow) = allow {
        params["allow_project_change"] = serde_json::json!(allow);
    }
    // RED on the legacy endpoints is preserved in .local/red-tests.md. Opt-in
    // uses additive methods; legacy refusal calls keep the frozen parameter shape.
    let method = match (create, allow.is_some()) {
        (true, true) => "worktree.create_project_checked",
        (false, true) => "worktree.open_project_checked",
        (true, false) => "worktree.create",
        (false, false) => "worktree.open",
    };
    let request = serde_json::from_value(serde_json::json!({
        "id": "worktree-repin-4551",
        "method": method,
        "params": params
    }))
    .unwrap();
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    fixture
        .server
        .handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            context: api::ApiRequestContext::default(),
            request,
            respond_to,
            response_write_complete: None,
        });
    response_rx
}

async fn worktree_repin_request(
    fixture: &mut WorktreeRepinFixture,
    create: bool,
    allow: Option<bool>,
) -> Result<ResponseResult, ErrorResponse> {
    let response_rx = worktree_repin_start(fixture, create, allow);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(response) = response_rx.try_recv() {
                return decode_response(&response);
            }
            // A refused create answers from its rollback thread, without an event.
            if let Ok(event) = tokio::time::timeout(
                Duration::from_millis(50),
                fixture.server.app.event_rx.recv(),
            )
            .await
            {
                let event = event.expect("deferred worktree event");
                fixture.server.handle_internal_event_with_forwarding(event);
            }
        }
    })
    .await
    .expect("bounded deferred headless response")
}

async fn assert_worktree_repin(create: bool, target: bool, outcome: &str) {
    let unchanged = matches!(outcome, "unchanged" | "unknown");
    let mut fixture = worktree_repin_fixture(create, target, unchanged);
    if outcome == "metadata" {
        // Both sides are the same hosting workspace, with complete project
        // authority. A checkout pin must not override its metadata project.
        metadata(
            &mut fixture.server,
            fixture.workspace,
            Some("smarty-pants"),
            "herdr",
        );
    }
    if outcome == "unknown" {
        // An unchanged explicit pin must not turn an unknown first-Pi cwd into
        // a false project change (including a checkout path which is still absent).
        membership(&mut fixture.server, fixture.workspace, &fixture.old_project);
        fixture.server.app.state.workspaces[fixture.workspace]
            .worktree_space
            .as_mut()
            .unwrap()
            .is_linked_worktree = target;
        let terminal = fixture.server.app.state.workspaces[fixture.workspace]
            .terminal_id(fixture.pane)
            .unwrap();
        fixture
            .server
            .app
            .terminal_runtimes
            .get(terminal)
            .unwrap()
            .test_set_foreground_cwd(None);
    }
    let combined_session = if outcome == "combined" {
        assert!(target);
        let source_pane = root(&fixture.server, 0, 0);
        let source_project = fixture.alias.join("old-source-project");
        std::fs::create_dir_all(&source_project).unwrap();
        cwd(&mut fixture.server, source_pane, &source_project);
        Some(session(
            &mut fixture.server,
            source_pane,
            &format!(
                "source-{}",
                fixture.directory.file_name().unwrap().to_str().unwrap()
            ),
        ))
    } else {
        None
    };
    let before = fingerprint(&fixture.server);
    let memberships: Vec<_> = fixture
        .server
        .app
        .state
        .workspaces
        .iter()
        .map(|ws| ws.worktree_space().cloned())
        .collect();
    let counts = (
        fixture.server.app.state.workspaces.len(),
        fixture.server.app.state.terminals.len(),
        fixture.server.app.terminal_runtimes.len(),
    );
    let old_workspace = fixture.server.app.public_workspace_id(fixture.workspace);
    let old_pane = fixture
        .server
        .app
        .public_pane_id(fixture.workspace, fixture.pane)
        .unwrap();
    let repinned = if target {
        fixture.checkout.clone()
    } else {
        fixture.repo.clone()
    };
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let result = if create && outcome == "refuse" {
        // A create refused before any Git work answers at once, not as a
        // completion refusal that must roll a checkout back.
        let response_rx = worktree_repin_start(&mut fixture, create, None);
        decode_response(
            &response_rx
                .try_recv()
                .expect("refusal answers before the Git worker runs"),
        )
    } else {
        worktree_repin_request(
            &mut fixture,
            create,
            (matches!(outcome, "allow" | "combined") || unchanged).then_some(true),
        )
        .await
    };
    if outcome == "refuse" {
        assert!(
            result.is_err(),
            "#4551 must refuse repinning an existing Pi workspace; got {result:?}"
        );
        let error = result.unwrap_err();
        assert_eq!(error.error.code, "project_change_refused");
        for text in [old_pane.as_str(), "Path", fixture.session.as_str()] {
            assert!(
                error.error.message.contains(text),
                "missing {text}: {}",
                error.error.message
            );
        }
        for path in [&fixture.old_project, &repinned] {
            assert!(
                mentions_path(&error.error.message, path),
                "missing {}: {}",
                path.display(),
                error.error.message
            );
        }
        assert!(error.error.message.contains("--allow-project-change"));
        assert!(error.error.message.contains(if create {
            "worktree.create_project_checked"
        } else {
            "worktree.open_project_checked"
        }));
        if create {
            assert!(
                !fixture.checkout.exists(),
                "initial refusal cannot create checkout"
            );
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&fixture.repo)
                .args(["show-ref", "--verify", "--quiet", "refs/heads/repin"])
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "initial refusal cannot create branch"
            );
        }
        assert_eq!(
            fingerprint(&fixture.server),
            before,
            "refusal has no events, rename, or topology effects"
        );
        assert_eq!(
            fixture
                .server
                .app
                .state
                .workspaces
                .iter()
                .map(|ws| ws.worktree_space().cloned())
                .collect::<Vec<_>>(),
            memberships
        );
        assert_eq!(
            (
                fixture.server.app.state.workspaces.len(),
                fixture.server.app.state.terminals.len(),
                fixture.server.app.terminal_runtimes.len()
            ),
            counts,
            "refusal cannot spawn a workspace or runtime"
        );
    } else {
        let response = result.unwrap();
        match response {
            ResponseResult::WorktreeCreated { workspace, .. } if create => {
                if target {
                    assert_eq!(workspace.workspace_id, old_workspace);
                }
            }
            ResponseResult::WorktreeOpened {
                workspace,
                already_open,
                ..
            } if !create => {
                assert_eq!(already_open, target);
                if target {
                    assert_eq!(workspace.workspace_id, old_workspace);
                }
            }
            other => panic!("unexpected deferred result: {other:?}"),
        }
        assert_eq!(
            fixture.server.app.state.workspaces.len(),
            2,
            "existing workspace reused"
        );
        assert_eq!(
            canonical_path(
                &fixture.server.app.state.workspaces[fixture.workspace]
                    .worktree_space()
                    .unwrap()
                    .checkout_path
            ),
            canonical_path(&repinned)
        );
    }
    if outcome == "combined" {
        // The same successful command is now a no-op membership repin. It
        // must neither re-prompt nor append an audit for either session.
        assert!(worktree_repin_request(&mut fixture, false, None)
            .await
            .is_ok());
    }
    drop(guard);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let audits: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("intentional project change allowed"))
        .collect();
    let expected = usize::from(matches!(outcome, "allow" | "combined"));
    assert_eq!(
        audits.len(),
        expected,
        "#4551 one intentional command must emit exactly one audit: {logs}"
    );
    assert_eq!(
        logs.matches("intentional project change allowed").count(),
        expected,
        "{logs}"
    );
    if expected == 1 {
        for text in [old_pane.as_str(), "Path", fixture.session.as_str()] {
            assert!(audits[0].contains(text), "missing {text}: {logs}");
        }
        for path in [&fixture.old_project, &repinned] {
            assert!(
                mentions_path(audits[0], path),
                "missing {}: {logs}",
                path.display()
            );
        }
    }
    if let Some(combined_session) = combined_session {
        assert!(
            audits[0].contains(&combined_session),
            "source session absent: {logs}"
        );
        assert!(
            mentions_path(audits[0], &fixture.repo),
            "source target absent: {logs}"
        );
        assert_eq!(
            audits[0].matches("would change project").count(),
            2,
            "{logs}"
        );
        let source_pane = root(&fixture.server, 0, 0);
        let terminal = fixture.server.app.state.workspaces[0]
            .terminal_id(source_pane)
            .unwrap();
        assert_eq!(
            fixture.server.app.state.terminals[terminal]
                .agent_session_reference()
                .unwrap()
                .value,
            combined_session
        );
    }
    let terminal = fixture.server.app.state.workspaces[fixture.workspace]
        .terminal_id(fixture.pane)
        .unwrap();
    assert_eq!(
        fixture.server.app.state.terminals[terminal]
            .agent_session_reference()
            .unwrap()
            .value,
        fixture.session
    );
    assert!(fixture.server.app.terminal_runtimes.get(terminal).is_some());
    assert_eq!(
        fixture.server.app.public_workspace_id(fixture.workspace),
        old_workspace
    );
    assert_eq!(
        fixture
            .server
            .app
            .public_pane_id(fixture.workspace, fixture.pane)
            .unwrap(),
        old_pane
    );
    assert_eq!(fixture.server.app.state.active, Some(0));
    assert!(fixture.server.app.pending_api_worktree_creates.is_empty());
    fixture.server.app.state.assert_invariants_for_test();
}

macro_rules! worktree_repin_tests {
    ($(($name:ident, $create:expr, $target:expr, $outcome:expr)),+ $(,)?) => {
        $(#[tokio::test]
        async fn $name() { assert_worktree_repin($create, $target, $outcome).await; })+
    };
}

worktree_repin_tests!(
    (
        project_change_worktree_4551_open_metadata_no_audit,
        false,
        true,
        "metadata"
    ),
    (
        project_change_worktree_4551_create_metadata_no_audit,
        true,
        true,
        "metadata"
    ),
    (
        project_change_worktree_4551_open_unknown_noop_no_audit,
        false,
        true,
        "unknown"
    ),
    (
        project_change_worktree_4551_create_unknown_noop_no_audit,
        true,
        true,
        "unknown"
    ),
    (
        project_change_worktree_4551_open_combined_audit,
        false,
        true,
        "combined"
    ),
    (
        project_change_worktree_4551_create_combined_audit,
        true,
        true,
        "combined"
    ),
    (
        project_change_worktree_4551_open_source_refuses,
        false,
        false,
        "refuse"
    ),
    (
        project_change_worktree_4551_open_target_refuses,
        false,
        true,
        "refuse"
    ),
    (
        project_change_worktree_4551_create_source_refuses,
        true,
        false,
        "refuse"
    ),
    (
        project_change_worktree_4551_create_target_refuses,
        true,
        true,
        "refuse"
    ),
    (
        project_change_worktree_4551_open_source_allows_once,
        false,
        false,
        "allow"
    ),
    (
        project_change_worktree_4551_open_target_allows_once,
        false,
        true,
        "allow"
    ),
    (
        project_change_worktree_4551_create_source_allows_once,
        true,
        false,
        "allow"
    ),
    (
        project_change_worktree_4551_create_target_allows_once,
        true,
        true,
        "allow"
    ),
    (
        project_change_worktree_4551_open_source_unchanged_no_audit,
        false,
        false,
        "unchanged"
    ),
    (
        project_change_worktree_4551_open_target_unchanged_no_audit,
        false,
        true,
        "unchanged"
    ),
    (
        project_change_worktree_4551_create_source_unchanged_no_audit,
        true,
        false,
        "unchanged"
    ),
    (
        project_change_worktree_4551_create_target_unchanged_no_audit,
        true,
        true,
        "unchanged"
    ),
);

#[tokio::test]
async fn project_change_worktree_4551_create_completion_rechecks_topology_drift() {
    for target_drift in [false, true] {
        // A refusal rolls back only what this create made: a branch that
        // existed before the request must survive (P2 on 16224428). A checkout
        // that gained files while Git ran is kept, and the hint says open.
        // An ignored file counts: a clean `git worktree remove` deletes it.
        // A post-checkout hook may still write: the checkout and its branch
        // stay. A commit made on the new branch after the add is not ours:
        // the clean checkout goes, the branch stays.
        for (allow, existing_branch, new_file, hook, advance) in [
            (false, false, None, false, false),
            (false, true, None, false, false),
            (false, false, Some("agent-notes.txt"), false, false),
            (false, false, Some("local.env"), false, false),
            (false, false, None, true, false),
            (false, false, None, false, true),
            (true, false, None, false, false),
        ] {
            let mut fixture = worktree_repin_fixture(true, false, true);
            if hook {
                let hook_path = fixture.repo.join(".git/hooks/post-checkout");
                std::fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
                std::fs::write(
                    &hook_path,
                    "#!/bin/sh\ngit -c user.name=Hook -c user.email=hook@example.invalid \
                     commit --quiet --allow-empty -m hook\n",
                )
                .unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
            }
            let branch_before = existing_branch.then(|| {
                worktree_repin_git(&fixture.repo, &["branch", "repin"]);
                worktree_repin_rev(&fixture.repo, "refs/heads/repin").unwrap()
            });
            // Initial source project equals its proposed pin; no rejection at
            // start. Delay consuming the actual Git completion, not the worker.
            let response_rx = worktree_repin_start(&mut fixture, true, allow.then_some(true));
            let event =
                tokio::time::timeout(Duration::from_secs(30), fixture.server.app.event_rx.recv())
                    .await
                    .unwrap()
                    .expect("real Git completion");
            assert!(matches!(
                &event,
                crate::events::AppEvent::WorktreeAddFinished(_)
            ));
            assert!(
                fixture.checkout.exists(),
                "Git already created the checkout"
            );
            std::fs::write(fixture.repo.join(".git/info/exclude"), "local.env\n").unwrap();
            let kept_file = fixture.checkout.join(new_file.unwrap_or("absent"));
            if new_file.is_some() {
                std::fs::write(&kept_file, "written while Git ran").unwrap();
            }
            if advance {
                worktree_repin_git(
                    &fixture.checkout,
                    &[
                        "-c",
                        "user.name=User",
                        "-c",
                        "user.email=user@example.invalid",
                        "commit",
                        "--quiet",
                        "--allow-empty",
                        "-m",
                        "user",
                    ],
                );
            }
            // A workspace that arrived on the checkout keeps it, too.
            let kept = !allow && (new_file.is_some() || hook || target_drift);
            let drift_workspace = if target_drift {
                let mut workspace = crate::workspace::Workspace::test_new("arrived-during-Git");
                workspace.identity_cwd = fixture.checkout.clone();
                workspace.cached_git_space =
                    crate::workspace::git_space_metadata(&fixture.checkout);
                fixture.server.app.state.workspaces.push(workspace);
                fixture.server.app.state.ensure_test_terminals();
                1
            } else {
                0
            };
            let pane = root(&fixture.server, drift_workspace, 0);
            let drift_project = fixture.alias.join("async-drift-project");
            std::fs::create_dir_all(&drift_project).unwrap();
            cwd(&mut fixture.server, pane, &drift_project);
            let drift_session = session(
                &mut fixture.server,
                pane,
                &format!(
                    "drift-{}",
                    fixture.directory.file_name().unwrap().to_str().unwrap()
                ),
            );
            let before = fingerprint(&fixture.server);
            let count = fixture.server.app.state.workspaces.len();
            let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || LogWriter(writer.clone()))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                fixture.server.handle_internal_event_with_forwarding(event);
            });
            let response =
                decode_response(&response_rx.recv_timeout(Duration::from_secs(30)).unwrap());
            let refusal = response
                .as_ref()
                .err()
                .map(|error| error.error.message.clone())
                .unwrap_or_default();
            if allow {
                assert!(matches!(
                    response.unwrap(),
                    ResponseResult::WorktreeCreated { .. }
                ));
                assert_eq!(fixture.server.app.state.workspaces.len(), 2);
            } else {
                let error = response.unwrap_err();
                assert_eq!(error.error.code, "project_change_refused");
                assert!(error.error.message.contains(&drift_session));
                assert!(
                    mentions_path(&error.error.message, &drift_project),
                    "{}",
                    error.error.message
                );
                assert_eq!(fingerprint(&fixture.server), before);
                assert_eq!(fixture.server.app.state.workspaces.len(), count);
                assert!(fixture
                    .server
                    .app
                    .state
                    .workspaces
                    .iter()
                    .all(|ws| ws.worktree_space().is_none()));
                let list = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&fixture.repo)
                    .args(["worktree", "list", "--porcelain"])
                    .output()
                    .unwrap();
                let list = String::from_utf8_lossy(&list.stdout).replace('\\', "/");
                if kept {
                    // Never forced: the file survives, and the hint must not
                    // send the user to a create that fails on the existing path.
                    assert_eq!(kept_file.exists(), new_file.is_some());
                    assert!(fixture.checkout.exists());
                    assert_eq!(list.matches("worktree ").count(), 2, "{list}");
                    if hook && !target_drift {
                        assert!(
                            error.error.message.contains("post-checkout hook"),
                            "{}",
                            error.error.message
                        );
                    }
                    assert!(worktree_repin_rev(&fixture.repo, "refs/heads/repin").is_some());
                    for text in [
                        "was kept",
                        "herdr worktree open --allow-project-change",
                        "worktree.open_project_checked",
                    ] {
                        assert!(
                            error.error.message.contains(text),
                            "{}",
                            error.error.message
                        );
                    }
                    assert!(
                        !error.error.message.contains("create_project_checked"),
                        "{}",
                        error.error.message
                    );
                } else {
                    assert!(
                        error
                            .error
                            .message
                            .contains("worktree.create_project_checked"),
                        "{}",
                        error.error.message
                    );
                }
            }
            if !allow {
                // No rollback quarantine is left behind either way.
                let leftovers: Vec<_> = std::fs::read_dir(&fixture.alias)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .contains("herdr-rollback")
                    })
                    .collect();
                assert!(leftovers.is_empty(), "{leftovers:?}");
            }
            if !allow && !kept {
                // Git and filesystem state are back to before the request.
                assert!(!fixture.checkout.exists(), "refused checkout removed");
                let list = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&fixture.repo)
                    .args(["worktree", "list", "--porcelain"])
                    .output()
                    .unwrap();
                let list = String::from_utf8_lossy(&list.stdout).replace('\\', "/");
                assert_eq!(
                    list.matches("worktree ").count(),
                    1,
                    "only the main checkout remains: {list}"
                );
                if advance {
                    let hook_tip = worktree_repin_rev(&fixture.repo, "refs/heads/repin");
                    assert!(hook_tip.is_some(), "a later commit is never deleted");
                    assert_ne!(hook_tip, worktree_repin_rev(&fixture.repo, "HEAD"));
                    assert!(
                        refusal.contains("was removed, but branch repin was kept"),
                        "{refusal}"
                    );
                } else {
                    assert_eq!(
                        worktree_repin_rev(&fixture.repo, "refs/heads/repin"),
                        branch_before,
                        "a branch this create made is deleted; a pre-existing one is untouched"
                    );
                }
            }
            let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
            assert_eq!(
                logs.matches("intentional project change allowed").count(),
                usize::from(allow),
                "{logs}"
            );
            if allow {
                assert!(logs.contains(&drift_session), "{logs}");
            }
            let terminal = fixture.server.app.state.workspaces[drift_workspace]
                .terminal_id(pane)
                .unwrap();
            assert_eq!(
                fixture.server.app.state.terminals[terminal]
                    .agent_session_reference()
                    .unwrap()
                    .value,
                drift_session
            );
            assert!(fixture.server.app.terminal_runtimes.get(terminal).is_some());
            assert!(fixture.server.app.pending_api_worktree_creates.is_empty());
            fixture.server.app.state.assert_invariants_for_test();
            if kept {
                // The hinted open succeeds on the kept checkout.
                let retry = worktree_repin_request(&mut fixture, false, Some(true)).await;
                assert!(
                    matches!(retry, Ok(ResponseResult::WorktreeOpened { .. })),
                    "hinted open of the kept checkout: {retry:?}"
                );
                assert_eq!(kept_file.exists(), new_file.is_some());
            } else if !allow {
                // Nothing orphaned blocks a retry of the same create.
                let retry = worktree_repin_request(&mut fixture, true, Some(true)).await;
                assert!(
                    matches!(retry, Ok(ResponseResult::WorktreeCreated { .. })),
                    "retry after rollback: {retry:?}"
                );
                assert!(fixture.checkout.exists());
                if let Some(branch_before) = &branch_before {
                    assert_eq!(
                        worktree_repin_rev(&fixture.repo, "refs/heads/repin").as_ref(),
                        Some(branch_before)
                    );
                }
            }
        }
    }
}

// Review P3 on 17c786af: an allowed create repins the source workspace before
// it opens the target. If opening the target fails, the repin stays, so its
// audit must be written, once.
#[tokio::test]
async fn project_change_worktree_4551_create_target_open_failure_still_audits_repin() {
    let mut fixture = worktree_repin_fixture(true, false, false);
    fixture.server.app.state.default_shell = fixture
        .directory
        .join("missing-shell")
        .display()
        .to_string();
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let result = worktree_repin_request(&mut fixture, true, Some(true)).await;
    drop(guard);
    let error = result.expect_err("the target workspace cannot open");
    assert_eq!(error.error.code, "worktree_open_failed", "{error:?}");
    assert_eq!(
        canonical_path(
            &fixture.server.app.state.workspaces[0]
                .worktree_space()
                .expect("the source repin persisted")
                .checkout_path
        ),
        canonical_path(&fixture.repo)
    );
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("intentional project change allowed").count(),
        1,
        "{logs}"
    );
    assert!(logs.contains(&fixture.session), "{logs}");
}

fn worktree_repin_rev(repo: &std::path::Path, reference: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
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

// The reported fallback is first Pi in tab/layout-leaf order, NOT root_pane
// or TerminalState.cwd. Real processes are exercised by .local/r1-proof.sh;
// these tests supply the same detected Pi/session and foreground-cwd evidence.
fn two_pi_fallback_server(
    split: Option<ratatui::layout::Direction>,
    same_project: bool,
) -> (
    HeadlessServer,
    crate::layout::PaneId,
    crate::layout::PaneId,
    String,
    String,
) {
    let mut server = project_server();
    server.app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let first = root(&server, 0, 0);
    let next = match split {
        Some(direction) => server.app.state.workspaces[0].test_split(direction),
        None => root(&server, 0, 1),
    };
    server.app.state.ensure_test_terminals();
    let a = std::env::temp_dir().join("r1-first-pi-project-a");
    let b = if same_project {
        a.clone()
    } else {
        std::env::temp_dir().join("r1-next-pi-project-b")
    };
    cwd(&mut server, first, &a);
    cwd(&mut server, next, &b);
    let first_session = session(&mut server, first, "r1-first-pi");
    let next_session = session(&mut server, next, "r1-next-pi");
    // Deliberately stale/equal cached cwd: policy must read foreground cwd.
    for pane in [first, next] {
        let id = server.app.state.workspaces[0]
            .terminal_id(pane)
            .unwrap()
            .clone();
        server.app.state.terminals.get_mut(&id).unwrap().cwd =
            std::env::temp_dir().join("r1-unrelated-cached-shell-cwd");
    }
    assert!(server.app.state.workspaces[0].worktree_space().is_none());
    (server, first, next, first_session, next_session)
}

fn capture_project_logs(action: impl FnOnce()) -> String {
    let logs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, action);
    let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    output
}

fn assert_project_audit(logs: &str, count: usize, sessions: &[&str]) {
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("intentional project change allowed"))
        .collect();
    assert_eq!(lines.len(), count, "{logs}");
    assert_eq!(
        logs.matches("intentional project change allowed").count(),
        count,
        "{logs}"
    );
    if count > 0 {
        for expected in sessions.iter().copied().chain([
            "Path",
            "r1-first-pi-project-a",
            "r1-next-pi-project-b",
        ]) {
            assert!(lines[0].contains(expected), "missing {expected}: {logs}");
        }
    }
}

fn assert_attached_session(server: &HeadlessServer, pane: crate::layout::PaneId, path: &str) {
    let terminal = server.app.state.workspaces[0].terminal_id(pane).unwrap();
    assert_eq!(
        server.app.state.terminals[terminal]
            .agent_session_reference()
            .unwrap()
            .value,
        path
    );
    assert!(server.app.terminal_runtimes.get(terminal).is_some());
}

#[tokio::test]
async fn project_change_swap_directional_and_explicit_refuse_atomically_then_allow_once() {
    use api::schema::{PaneDirection, PaneSwapParams, PaneSwapProjectCheckedParams};
    // All four directional routes plus the explicit source/target API form.
    for (direction, reverse) in [
        (Some(PaneDirection::Right), false),
        (Some(PaneDirection::Left), true),
        (Some(PaneDirection::Down), false),
        (Some(PaneDirection::Up), true),
        (None, false),
    ] {
        let axis = if matches!(direction, Some(PaneDirection::Down | PaneDirection::Up)) {
            ratatui::layout::Direction::Vertical
        } else {
            ratatui::layout::Direction::Horizontal
        };
        let (mut server, first, next, first_session, next_session) =
            two_pi_fallback_server(Some(axis), false);
        let source = server
            .app
            .public_pane_id(0, if reverse { next } else { first })
            .unwrap();
        let target = server
            .app
            .public_pane_id(0, if reverse { first } else { next })
            .unwrap();
        let params = if let Some(direction) = direction {
            PaneSwapParams {
                pane_id: Some(source),
                direction: Some(direction),
                ..PaneSwapParams::default()
            }
        } else {
            PaneSwapParams {
                source_pane_id: Some(source),
                target_pane_id: Some(target),
                ..PaneSwapParams::default()
            }
        };
        let (control_rx, _render_rx) = connect_test_shell(&mut server, 144, 80, 23);
        let _initial = client_shell_snapshot(&control_rx);
        let location = server.clients[&144].shell_location.clone();
        let revision = server.clients[&144].shell_projection_revision;
        let before = fingerprint(&server);
        for method in [
            Method::PaneSwap(params.clone()),
            Method::PaneSwapProjectChecked(PaneSwapProjectCheckedParams {
                params: params.clone(),
                allow_project_change: false,
            }),
        ] {
            let logs = capture_project_logs(|| {
                let error = public_method(&mut server, method).unwrap_err();
                assert_eq!(error.error.code, "project_change_refused");
                for text in [&first_session, &next_session] {
                    assert!(
                        error.error.message.contains(text),
                        "{}",
                        error.error.message
                    );
                }
                assert!(error.error.message.contains("r1-first-pi-project-a"));
                assert!(error.error.message.contains("r1-next-pi-project-b"));
                assert_eq!(fingerprint(&server), before, "swap refusal is atomic");
                assert_eq!(server.clients[&144].shell_location, location);
                assert_eq!(server.clients[&144].shell_projection_revision, revision);
            });
            assert_project_audit(&logs, 0, &[]);
        }
        let logs = capture_project_logs(|| {
            let ResponseResult::PaneSwap { swap } = public_method(
                &mut server,
                Method::PaneSwapProjectChecked(PaneSwapProjectCheckedParams {
                    params,
                    allow_project_change: true,
                }),
            )
            .unwrap() else {
                panic!("pane swap response")
            };
            assert!(swap.changed);
            assert_eq!(
                server.app.state.workspaces[0].tabs[0].layout.pane_ids(),
                vec![next, first]
            );
            // Swapping leaves does not alter root identity or terminal/session attachment.
            assert_eq!(root(&server, 0, 0), first);
            assert_attached_session(&server, first, &first_session);
            assert_attached_session(&server, next, &next_session);
        });
        assert_project_audit(&logs, 1, &[&first_session, &next_session]);
        server.app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn project_change_swap_same_foreground_project_succeeds_without_audit() {
    use api::schema::{PaneSwapParams, PaneSwapProjectCheckedParams};
    let (mut server, first, next, first_session, next_session) =
        two_pi_fallback_server(Some(ratatui::layout::Direction::Horizontal), true);
    let params = PaneSwapParams {
        source_pane_id: server.app.public_pane_id(0, first),
        target_pane_id: server.app.public_pane_id(0, next),
        ..PaneSwapParams::default()
    };
    for method in [
        Method::PaneSwap(params.clone()),
        Method::PaneSwapProjectChecked(PaneSwapProjectCheckedParams {
            params,
            allow_project_change: true,
        }),
    ] {
        let logs = capture_project_logs(|| {
            assert!(matches!(public_method(&mut server, method).unwrap(),
                ResponseResult::PaneSwap { swap } if swap.changed));
        });
        assert_project_audit(&logs, 0, &[]);
        assert_attached_session(&server, first, &first_session);
        assert_attached_session(&server, next, &next_session);
    }
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_explicit_pane_and_tab_close_allow_once_only_for_changed_survivors() {
    // pane.close of a split root, pane.close of a whole tab, and tab.close.
    // Explicit close is an intentional opt-in: there is NO public refusal path.
    for close_kind in 0..3 {
        for same_project in [false, true] {
            for close_first in [true, false] {
                let split = (close_kind == 0).then_some(ratatui::layout::Direction::Horizontal);
                let (mut server, first, next, first_session, next_session) =
                    two_pi_fallback_server(split, same_project);
                let (removed, survivor, removed_session, survivor_session) = if close_first {
                    (first, next, &first_session, &next_session)
                } else {
                    (next, first, &next_session, &first_session)
                };
                let removed_terminal = server.app.state.workspaces[0]
                    .terminal_id(removed)
                    .unwrap()
                    .clone();
                let method = if close_kind == 2 {
                    Method::TabClose(api::schema::TabTarget {
                        tab_id: server
                            .app
                            .public_tab_id(0, if close_first { 0 } else { 1 })
                            .unwrap(),
                    })
                } else {
                    Method::PaneClose(api::schema::PaneTarget {
                        pane_id: server.app.public_pane_id(0, removed).unwrap(),
                    })
                };
                let logs = capture_project_logs(|| {
                    assert!(public_method(&mut server, method).is_ok());
                    assert_attached_session(&server, survivor, survivor_session);
                    assert!(server.app.state.workspaces[0]
                        .find_tab_index_for_pane(removed)
                        .is_none());
                    assert!(!server.app.state.terminals.contains_key(&removed_terminal));
                    assert!(server
                        .app
                        .terminal_runtimes
                        .get(&removed_terminal)
                        .is_none());
                });
                let changed = close_first && !same_project;
                assert_project_audit(&logs, usize::from(changed), &[survivor_session]);
                if changed {
                    let audit = logs
                        .lines()
                        .find(|line| line.contains("intentional project change allowed"))
                        .unwrap();
                    assert!(
                        !audit.contains(removed_session),
                        "closed session is not a survivor: {audit}"
                    );
                    assert!(audit.contains("intentional close"), "{audit}");
                    assert!(
                        audit.contains(if close_kind == 2 {
                            "tab.close"
                        } else {
                            "pane.close"
                        }),
                        "{audit}"
                    );
                }
                server.app.state.assert_invariants_for_test();
                shutdown_test_runtimes(&mut server);
            }
        }
    }
}

fn replacement_params(server: &HeadlessServer, tab: usize) -> api::schema::LayoutApplyParams {
    // Deserialization keeps this fixture aligned with the published JSON shape.
    // Omit command to use test_headless_server's portable exiting default shell.
    serde_json::from_value(serde_json::json!({
        "tab_id": server.app.public_tab_id(0, tab).unwrap(),
        "focus": false,
        "root": { "type": "pane" }
    }))
    .unwrap()
}

#[tokio::test]
async fn project_change_layout_replacement_refuses_before_spawn_then_allows_one_survivor_audit() {
    use api::schema::LayoutApplyProjectCheckedParams;
    let (mut server, first, next, first_session, next_session) =
        two_pi_fallback_server(None, false);
    let params = replacement_params(&server, 0);
    let terminals_before = server.app.state.terminals.len();
    let runtimes_before = server.app.terminal_runtimes.len();
    let (control_rx, _render_rx) = connect_test_shell(&mut server, 145, 80, 23);
    let _initial = client_shell_snapshot(&control_rx);
    let before = fingerprint(&server);
    let location = server.clients[&145].shell_location.clone();
    let revision = server.clients[&145].shell_projection_revision;
    for method in [
        Method::LayoutApply(params.clone()),
        Method::LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams {
            params: params.clone(),
            allow_project_change: false,
        }),
    ] {
        let logs = capture_project_logs(|| {
            let error = public_method(&mut server, method).unwrap_err();
            assert_eq!(error.error.code, "project_change_refused");
            assert!(error.error.message.contains(&next_session));
            assert!(!error.error.message.contains(&first_session));
            assert!(error.error.message.contains("r1-first-pi-project-a"));
            assert!(error.error.message.contains("r1-next-pi-project-b"));
            assert_eq!(fingerprint(&server), before, "layout refusal is atomic");
            assert_eq!(server.app.state.terminals.len(), terminals_before);
            assert_eq!(server.app.terminal_runtimes.len(), runtimes_before);
            assert_eq!(server.clients[&145].shell_location, location);
            assert_eq!(server.clients[&145].shell_projection_revision, revision);
        });
        assert_project_audit(&logs, 0, &[]);
    }
    let logs = capture_project_logs(|| {
        assert!(matches!(
            public_method(
                &mut server,
                Method::LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams {
                    params,
                    allow_project_change: true,
                })
            )
            .unwrap(),
            ResponseResult::LayoutApply { .. }
        ));
        assert!(server.app.state.workspaces[0]
            .find_tab_index_for_pane(first)
            .is_none());
        assert_attached_session(&server, next, &next_session);
        assert_eq!(
            root(&server, 0, 0),
            next,
            "plain-shell replacement is appended"
        );
    });
    assert_project_audit(&logs, 1, &[&next_session]);
    let audit = logs
        .lines()
        .find(|line| line.contains("intentional project change allowed"))
        .unwrap();
    assert!(!audit.contains(&first_session));
    server.app.state.assert_invariants_for_test();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn project_change_layout_unchanged_first_pi_or_same_foreground_project_never_audits() {
    use api::schema::LayoutApplyProjectCheckedParams;
    for same_project in [false, true] {
        for checked in [false, true] {
            let (mut server, first, next, first_session, next_session) =
                two_pi_fallback_server(None, same_project);
            // Distinct cwd counterexample: replacing the SECOND Pi leaves first
            // Pi unchanged. Same-cwd counterexample: replace the FIRST Pi.
            let tab = if same_project { 0 } else { 1 };
            let (survivor, survivor_session) = if same_project {
                (next, &next_session)
            } else {
                (first, &first_session)
            };
            let params = replacement_params(&server, tab);
            let method = if checked {
                Method::LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams {
                    params,
                    allow_project_change: true,
                })
            } else {
                Method::LayoutApply(params)
            };
            let logs = capture_project_logs(|| {
                assert!(matches!(
                    public_method(&mut server, method).unwrap(),
                    ResponseResult::LayoutApply { .. }
                ));
                assert_attached_session(&server, survivor, survivor_session);
            });
            assert_project_audit(&logs, 0, &[]);
            server.app.state.assert_invariants_for_test();
            shutdown_test_runtimes(&mut server);
        }
    }
}

// Exercise the receiver, not App::handle_api_request: shell requests attribute
// geometry to their client, while public requests reapply existing controllers.
fn project_receiver_method(
    server: &mut HeadlessServer,
    client_id: Option<u64>,
    method: Method,
) -> Result<ResponseResult, ErrorResponse> {
    let Some(client_id) = client_id else {
        return public_method(server, method);
    };
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        client_id,
        api::ApiRequestMessage {
            context: api::ApiRequestContext::default(),
            request: api::schema::Request {
                id: "project-receiver".into(),
                method,
            },
            respond_to,
            response_write_complete: None,
        },
    );
    decode_response(
        &response_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("bounded shell receiver response"),
    )
}

fn project_pane_size(server: &HeadlessServer, pane: crate::layout::PaneId) -> (u16, u16) {
    let terminal = server.app.state.workspaces[0].terminal_id(pane).unwrap();
    let runtime = server.app.terminal_runtimes.get(terminal).unwrap();
    let size = runtime.current_size();
    assert_eq!(runtime.terminal_dimensions(), Some((size.1, size.0)));
    size
}

#[tokio::test]
async fn project_change_checked_swap_receivers_reapply_unequal_controlled_leaf_geometry() {
    use api::schema::{LayoutSetSplitRatioParams, PaneSwapParams, PaneSwapProjectCheckedParams};
    for shell_route in [false, true] {
        for same_project in [false, true] {
            let (mut server, first, next, first_session, next_session) =
                two_pi_fallback_server(Some(ratatui::layout::Direction::Horizontal), same_project);
            let (control, _render) = connect_test_shell(&mut server, 144, 137, 41);
            let _initial = client_shell_snapshot(&control);
            let tab_id = server.app.public_tab_id(0, 0).unwrap();
            assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&144));
            // A 50/50 split can conceal a skipped resize after swapping leaves.
            public_method(
                &mut server,
                Method::LayoutSetSplitRatio(LayoutSetSplitRatioParams {
                    tab_id: Some(tab_id.clone()),
                    pane_id: None,
                    path: Vec::new(),
                    ratio: 0.3,
                }),
            )
            .unwrap();
            let before_sizes = [
                project_pane_size(&server, first),
                project_pane_size(&server, next),
            ];
            assert_ne!(before_sizes[0], before_sizes[1]);
            let location = server.clients[&144].shell_location.clone();
            let before = fingerprint(&server);
            let route = shell_route.then_some(144);
            let params = PaneSwapParams {
                source_pane_id: server.app.public_pane_id(0, first),
                target_pane_id: server.app.public_pane_id(0, next),
                ..PaneSwapParams::default()
            };
            if !same_project {
                let logs = capture_project_logs(|| {
                    let error = project_receiver_method(
                        &mut server,
                        route,
                        Method::PaneSwapProjectChecked(PaneSwapProjectCheckedParams {
                            params: params.clone(),
                            allow_project_change: false,
                        }),
                    )
                    .unwrap_err();
                    assert_eq!(error.error.code, "project_change_refused");
                    assert_eq!(fingerprint(&server), before);
                    assert_eq!(server.clients[&144].shell_location, location);
                    assert_eq!(
                        [
                            project_pane_size(&server, first),
                            project_pane_size(&server, next)
                        ],
                        before_sizes
                    );
                });
                assert_project_audit(&logs, 0, &[]);
            }
            let logs = capture_project_logs(|| {
                assert!(matches!(
                    project_receiver_method(
                        &mut server,
                        route,
                        Method::PaneSwapProjectChecked(PaneSwapProjectCheckedParams {
                            params,
                            allow_project_change: !same_project,
                        }),
                    ).unwrap(),
                    ResponseResult::PaneSwap { swap } if swap.changed
                ));
                assert_eq!(
                    server.app.state.workspaces[0].tabs[0].layout.pane_ids(),
                    vec![next, first]
                );
                assert_eq!(server.clients[&144].shell_location, location);
                assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&144));
                // Exact cross-over proves a receiver resize occurred. The frozen
                // checked-method lists leave each terminal at its old leaf size.
                assert_eq!(project_pane_size(&server, first), before_sizes[1]);
                assert_eq!(project_pane_size(&server, next), before_sizes[0]);
                assert_attached_session(&server, first, &first_session);
                assert_attached_session(&server, next, &next_session);
            });
            assert_project_audit(
                &logs,
                usize::from(!same_project),
                &[&first_session, &next_session],
            );
            server.app.state.assert_invariants_for_test();
            shutdown_test_runtimes(&mut server);
        }
    }
}

#[tokio::test]
async fn project_change_checked_layout_receivers_reconcile_viewers_and_reapply_dimensions() {
    use api::schema::LayoutApplyProjectCheckedParams;
    for shell_route in [false, true] {
        // Replacing the first Pi changes project; replacing the second leaves
        // the first Pi/project unchanged and must not audit or disturb its tab.
        for replace_first in [true, false] {
            let (mut server, first, next, first_session, next_session) =
                two_pi_fallback_server(None, false);
            let tab = usize::from(!replace_first);
            let removed = if replace_first { first } else { next };
            let survivor = if replace_first { next } else { first };
            let survivor_session = if replace_first {
                &next_session
            } else {
                &first_session
            };
            let removed_terminal = server.app.state.workspaces[0]
                .terminal_id(removed)
                .unwrap()
                .clone();
            let removed_tab_id = server.app.public_tab_id(0, tab).unwrap();
            let survivor_tab_id = server
                .app
                .public_tab_id(0, usize::from(replace_first))
                .unwrap();
            let (control, _render) = connect_test_shell(&mut server, 144, 137, 41);
            let _initial = client_shell_snapshot(&control);
            // Singleton bootstrap resizes all existing tabs, not just active.
            let full_size = project_pane_size(&server, survivor);
            assert_eq!(project_pane_size(&server, removed), full_size);
            let (observer_control, _observer_render) = connect_test_shell(&mut server, 145, 71, 19);
            let _observer_initial = client_shell_snapshot(&observer_control);
            assert!(server.focus_shell_client_on_tab(145, &removed_tab_id));
            if shell_route && !replace_first {
                assert!(server.focus_shell_client_on_tab(144, &removed_tab_id));
                assert!(server.claim_shell_tab_geometry(144, false));
            }
            // The second viewer is necessary: the shell receiver directly
            // focuses its requesting client even without reconciliation.
            let locations = [
                server.clients[&144].shell_location.clone(),
                server.clients[&145].shell_location.clone(),
            ];
            let before = fingerprint(&server);
            let sizes = [
                project_pane_size(&server, first),
                project_pane_size(&server, next),
            ];
            let counts = (
                server.app.state.terminals.len(),
                server.app.terminal_runtimes.len(),
            );
            let params = replacement_params(&server, tab);
            let route = shell_route.then_some(144);
            if replace_first {
                let logs = capture_project_logs(|| {
                    let error = project_receiver_method(
                        &mut server,
                        route,
                        Method::LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams {
                            params: params.clone(),
                            allow_project_change: false,
                        }),
                    )
                    .unwrap_err();
                    assert_eq!(error.error.code, "project_change_refused");
                    assert_eq!(fingerprint(&server), before);
                    assert_eq!(
                        [
                            server.clients[&144].shell_location.clone(),
                            server.clients[&145].shell_location.clone()
                        ],
                        locations
                    );
                    assert_eq!(
                        [
                            project_pane_size(&server, first),
                            project_pane_size(&server, next)
                        ],
                        sizes
                    );
                    assert_eq!(
                        (
                            server.app.state.terminals.len(),
                            server.app.terminal_runtimes.len()
                        ),
                        counts
                    );
                });
                assert_project_audit(&logs, 0, &[]);
            }
            let logs = capture_project_logs(|| {
                let ResponseResult::LayoutApply { layout } = project_receiver_method(
                    &mut server,
                    route,
                    Method::LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams {
                        params,
                        allow_project_change: replace_first,
                    }),
                )
                .unwrap() else {
                    panic!("checked layout receiver response")
                };
                assert!(server.app.parse_tab_id(&removed_tab_id).is_none());
                assert!(!server.app.state.terminals.contains_key(&removed_terminal));
                assert!(server
                    .app
                    .terminal_runtimes
                    .get(&removed_terminal)
                    .is_none());
                assert_attached_session(&server, survivor, survivor_session);
                assert_eq!(project_pane_size(&server, survivor), full_size);
                assert!(server.app.parse_tab_id(&survivor_tab_id).is_some());
                for client_id in [144, 145] {
                    let location = server.clients[&client_id].shell_location.as_ref().unwrap();
                    let viewed = location.focused_tab_id().unwrap();
                    assert_ne!(viewed, removed_tab_id);
                    let (ws, tab) = server
                        .app
                        .parse_tab_id(viewed)
                        .expect("reconciled live tab");
                    let focused = server.app.state.workspaces[ws].tabs[tab].layout.focused();
                    assert!(server.app.state.workspaces[ws].tabs[tab]
                        .terminal_id(focused)
                        .is_some());
                    assert_eq!(project_pane_size(&server, focused), full_size);
                    assert_eq!(server.tab_geometry_controllers.get(viewed), Some(&144));
                }
                assert!(!server
                    .tab_geometry_controllers
                    .contains_key(&removed_tab_id));
                // Public replacement of an inactive tab preserves the primary
                // client's location. Shell/active replacement selects the new tab.
                if !shell_route && !replace_first {
                    assert_eq!(server.clients[&144].shell_location, locations[0]);
                    assert_eq!(
                        server.shell_tab_id_for_client(145).as_deref(),
                        Some(survivor_tab_id.as_str())
                    );
                } else {
                    assert_eq!(
                        server.shell_tab_id_for_client(144).as_deref(),
                        Some(layout.tab_id.as_str())
                    );
                    let (_, new_tab) = server.app.parse_tab_id(&layout.tab_id).unwrap();
                    assert_eq!(
                        project_pane_size(&server, root(&server, 0, new_tab)),
                        full_size
                    );
                }
            });
            assert_project_audit(&logs, usize::from(replace_first), &[survivor_session]);
            server.app.state.assert_invariants_for_test();
            shutdown_test_runtimes(&mut server);
        }
    }
}

#[test]
fn project_change_swap_and_layout_checked_schemas_default_deny_and_preserve_legacy_shapes() {
    let swap_json = serde_json::json!({ "source_pane_id": "w1:p1", "target_pane_id": "w1:p2" });
    let swap: api::schema::PaneSwapProjectCheckedParams =
        serde_json::from_value(swap_json.clone()).unwrap();
    assert!(!swap.allow_project_change);
    assert_eq!(serde_json::to_value(swap.params).unwrap(), swap_json);
    let layout_json =
        serde_json::json!({ "tab_id": "w1:t1", "focus": false, "root": { "type": "pane" } });
    let layout: api::schema::LayoutApplyProjectCheckedParams =
        serde_json::from_value(layout_json.clone()).unwrap();
    assert!(!layout.allow_project_change);
    assert_eq!(serde_json::to_value(layout.params).unwrap(), layout_json);
    for (method, params) in [
        ("pane.swap_project_checked", swap_json),
        ("layout.apply_project_checked", layout_json),
    ] {
        assert!(crate::server::client_commands::supports_client_shell_method_name(method));
        let request: api::schema::Request = serde_json::from_value(serde_json::json!({
            "id": "r1-schema", "method": method, "params": params
        }))
        .unwrap();
        match request.method {
            Method::PaneSwapProjectChecked(params) => assert!(!params.allow_project_change),
            Method::LayoutApplyProjectChecked(params) => assert!(!params.allow_project_change),
            other => panic!("wrong checked method: {other:?}"),
        }
    }
}
