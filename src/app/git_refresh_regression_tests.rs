// Regression proofs through App -> native hint -> scheduler -> worker -> apply.

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn make_tree_writable(path: &std::path::Path) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    if metadata.file_type().is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            make_tree_writable(&entry.unwrap().path());
        }
    }
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[cfg(not(windows))]
fn make_tree_writable(_path: &std::path::Path) {}

fn git_config_path(path: &std::path::Path) -> String {
    // Git config treats backslashes as escapes. Forward slashes are accepted on
    // every platform and keep this fixture valid on Windows.
    path.to_string_lossy().replace('\\', "/")
}

#[test]
fn git_watch_repair_same_path_directory_replacement_then_real_change() {
    isolated_git_watch_test!(git_watch_repair_same_path_directory_replacement_then_real_change);
    for directory in [".git/refs", ".git"] {
        let repo = GitWatchRepo::new("replacement");
        repo.init();
        let mut app = repo.app();
        let path = repo.0.join(directory);
        let retired = repo.0.join("retired-metadata");
        let safety_deadline =
            app.last_git_repo_discovery_refresh + GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        std::fs::rename(&path, &retired).unwrap();
        copy_tree(&retired, &path);
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.last_git_repo_discovery_refresh + GIT_REPO_DISCOVERY_REFRESH_INTERVAL,
            safety_deadline
        );
        // Retain the retired inode: stale registrations must not track it.
        drain_git_watch_hints(&mut app);
        repo.git(&["commit", "--allow-empty", "-m", "after replacement"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.state.workspaces[0].git_ahead_behind(),
            Some((1, 0)),
            "{directory}"
        );
        repo.git(&["switch", "-c", "after-replacement"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("after-replacement")
        );
    }
}

#[cfg(unix)]
#[test]
fn git_watch_repair_symlink_refs_target_edits_and_retarget() {
    isolated_git_watch_test!(git_watch_repair_symlink_refs_target_edits_and_retarget);
    let repo = GitWatchRepo::new("symlink-refs");
    let external = GitWatchRepo::new("refs-targets");
    repo.init();
    let refs = repo.0.join(".git/refs");
    let first = external.0.join("first");
    let second = external.0.join("second");
    std::fs::rename(&refs, &first).unwrap();
    std::os::unix::fs::symlink(&first, &refs).unwrap();
    let mut app = repo.app();
    repo.git(&["commit", "--allow-empty", "-m", "first target"]);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    copy_tree(&first, &second);
    let replacement = repo.0.join(".git/new-refs");
    std::os::unix::fs::symlink(&second, &replacement).unwrap();
    std::fs::rename(replacement, refs).unwrap();
    drive_git_watch_refresh(&mut app);
    repo.git(&["commit", "--allow-empty", "-m", "second target"]);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((2, 0)));
}

#[test]
fn git_watch_repair_user_config_fetch_refspec_and_missing_parent_creation() {
    isolated_git_watch_test!(
        git_watch_repair_user_config_fetch_refspec_and_missing_parent_creation
    );
    let process_home = std::env::var_os("HOME");
    let process_xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = GitWatchRepo::new("user-home");
    let xdg = home.0.join("xdg");
    let env = crate::environment::test_env();
    env.set("HOME", &home.0);
    env.set("XDG_CONFIG_HOME", &xdg);
    assert_eq!(std::env::var_os("HOME"), process_home);
    assert_eq!(std::env::var_os("XDG_CONFIG_HOME"), process_xdg);
    let repo = GitWatchRepo::new("user-config-fetch");
    repo.init();
    let initial = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let tip = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&["update-ref", "refs/remotes/test/old", &initial]);
    repo.git(&["update-ref", "refs/remotes/test/new", &tip]);
    repo.git(&["config", "branch.main.remote", "test"]);
    repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
    let user_config = home.0.join(".gitconfig");
    let fetch_config = |target| {
        format!("[remote \"test\"]\nfetch = +refs/heads/main:refs/remotes/test/{target}\n")
    };
    std::fs::write(&user_config, fetch_config("old")).unwrap();
    let mut app = repo.app();
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let git_config_before = std::fs::read(repo.0.join(".git/config")).unwrap();
    let temporary = home.0.join("user-config-new");
    std::fs::write(&temporary, fetch_config("new")).unwrap();
    std::fs::rename(temporary, &user_config).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    // A missing user-config hierarchy is observed by exact ancestor creation,
    // then migrated to the new non-recursive parent after the worker read.
    std::fs::create_dir(&xdg).unwrap();
    drive_git_watch_refresh(&mut app);
    std::fs::create_dir(xdg.join("git")).unwrap();
    drive_git_watch_refresh(&mut app);
    let xdg_config = xdg.join("git/config");
    let include = home.0.join("xdg-include");
    std::fs::write(
        &xdg_config,
        format!("[include]\npath = {}\n", git_config_path(&include)),
    )
    .unwrap();
    drive_git_watch_refresh(&mut app);
    std::fs::write(
        &include,
        "[branch \"main\"]\nremote = .\nmerge = refs/heads/upstream\n",
    )
    .unwrap();
    // Local branch definitions win, so merely reading this dependency must not
    // corrupt status. Its native hint still goes through worker/application.
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    assert_eq!(
        std::fs::read(repo.0.join(".git/config")).unwrap(),
        git_config_before
    );
}

#[test]
fn git_watch_repair_new_root_and_reactivated_consumer_get_initial_status() {
    isolated_git_watch_test!(git_watch_repair_new_root_and_reactivated_consumer_get_initial_status);
    let first = GitWatchRepo::new("first-root");
    first.init();
    let mut app = first.app();
    let second = GitWatchRepo::new("new-root");
    second.init();
    second.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let mut ws = Workspace::test_new("second");
    ws.tabs.clear();
    ws.identity_cwd = second.0.clone();
    app.state.workspaces.push(ws);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[1].git_ahead_behind(), Some((1, 0)));
    app.clear_git_watches(); // Same release used for a clientless headless app.
    second.git(&["commit", "--allow-empty", "-m", "while detached"]);
    // Consumer reactivation calls sync; no periodic deadline is due yet.
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[1].git_ahead_behind(), Some((2, 0)));
}

#[test]
fn git_watch_repair_git_directory_recreated_after_removal_was_applied() {
    isolated_git_watch_test!(git_watch_repair_git_directory_recreated_after_removal_was_applied);
    let repo = GitWatchRepo::new("replacement-gap");
    repo.init();
    let mut app = repo.app();
    let retired = repo.0.join("retired-metadata");
    // Git for Windows can mark object files read-only. That is fixture state,
    // not a watcher handle: notify opens directory watches with FILE_SHARE_DELETE.
    make_tree_writable(&repo.0.join(".git"));
    std::fs::rename(repo.0.join(".git"), &retired).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].cached_git_branch, None);
    copy_tree(&retired, &repo.0.join(".git"));
    drive_git_watch_refresh(&mut app);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("main")
    );
    repo.git(&["commit", "--allow-empty", "-m", "after recreation"]);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
}

/// Reach a quiet native/app boundary, including late removal and re-arm hints.
/// No synthetic event, forced refresh, or advanced safety clock may hide a lost watch.
#[track_caller]
fn drain_git_watch_hints(app: &mut App) {
    let start = Instant::now();
    let safety_refresh = app.last_git_repo_discovery_refresh;
    let mut quiet_since = Instant::now();
    loop {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(3),
            "native git hints did not settle within 3s"
        );
        app.sync_git_watches();
        app.start_git_status_refresh_if_due(Instant::now());
        while let Ok(event) = app.event_rx.try_recv() {
            app.handle_internal_event(event);
            quiet_since = Instant::now();
        }
        if app.git_refresh_in_flight || app.git_watch_refresh_deadline.is_some() {
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= std::time::Duration::from_millis(150) {
            assert!(app.event_rx.is_empty());
            assert_eq!(
                app.last_git_repo_discovery_refresh, safety_refresh,
                "periodic safety discovery must not satisfy a native quiet wait"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn git_watch_repair_linked_only_common_directory_restored_after_missing_apply() {
    isolated_git_watch_test!(
        git_watch_repair_linked_only_common_directory_restored_after_missing_apply
    );
    git_watch_repair_linked_only_common_directory_restored_scenario(false);
}

#[test]
#[ignore = "performance: 1 s native refresh target; run separately without test contention"]
fn perf_git_watch_repair_linked_only_common_directory_restored_after_missing_apply() {
    isolated_git_watch_test!(
        perf_git_watch_repair_linked_only_common_directory_restored_after_missing_apply
    );
    git_watch_repair_linked_only_common_directory_restored_scenario(true);
}

fn git_watch_repair_linked_only_common_directory_restored_scenario(check_latency: bool) {
    let repo = GitWatchRepo::new("linked-common-gap-main");
    repo.init();
    let linked = GitWatchRepo::new("linked-common-gap-consumer");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.0.to_str().unwrap(),
    ]);
    assert!(!linked
        .0
        .canonicalize()
        .unwrap()
        .starts_with(repo.0.canonicalize().unwrap()));
    linked.git(&["branch", "--set-upstream-to=upstream", "linked"]);
    linked.git(&["commit", "--allow-empty", "-m", "linked ahead"]);
    let tip = linked.git(&["rev-parse", "HEAD"]);
    let marker = linked.0.join(".git");
    let marker_before = std::fs::read(&marker).unwrap();
    let marker_modified = std::fs::metadata(&marker).unwrap().modified().unwrap();
    let mut app = linked.app(); // The main checkout is never an App consumer.
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("linked")
    );
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    drain_git_watch_hints(&mut app);
    let safety_deadline = app.last_git_repo_discovery_refresh + GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
    let common = repo.0.join(".git");
    let retired = repo.0.join("retired-metadata");
    make_tree_writable(&common);
    std::fs::rename(&common, &retired).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].cached_git_branch, None);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), None);
    drain_git_watch_hints(&mut app);
    let missing = app
        .git_status_cache
        .get(&app.state.workspaces[0].cached_git_status_key)
        .unwrap();
    assert!(missing.fingerprint.is_none());
    assert!(missing.config_dependency_paths().is_empty());
    assert!(!app.git_refresh_in_flight);
    assert!(app.git_watch_refresh_deadline.is_none());

    let start = Instant::now();
    std::fs::rename(&retired, &common).unwrap();
    let (restoration_latency, changed) = drive_git_watch_refresh(&mut app);
    // Preserve the original total measurement, including the mutation itself,
    // rather than replacing it with the helper's post-command refresh duration.
    if check_latency {
        assert!(start.elapsed() < GIT_WATCH_LATENCY_TARGET);
    }
    assert!(changed);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("linked")
    );
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    drain_git_watch_hints(&mut app);

    let start = Instant::now();
    repo.git(&["update-ref", "refs/heads/upstream", &tip]);
    let (ref_latency, changed) = drive_git_watch_refresh(&mut app);
    if check_latency {
        assert!(start.elapsed() < GIT_WATCH_LATENCY_TARGET);
    }
    assert!(changed);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    drain_git_watch_hints(&mut app);
    let start = Instant::now();
    linked.git(&["switch", "-c", "after-common-restoration"]);
    let (branch_latency, changed) = drive_git_watch_refresh(&mut app);
    if check_latency {
        assert!(start.elapsed() < GIT_WATCH_LATENCY_TARGET);
    }
    assert!(changed);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("after-common-restoration")
    );
    assert_eq!(std::fs::read(&marker).unwrap(), marker_before);
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        marker_modified
    );
    assert_eq!(
        app.last_git_repo_discovery_refresh + GIT_REPO_DISCOVERY_REFRESH_INTERVAL,
        safety_deadline
    );
    assert!(Instant::now() < safety_deadline);
    eprintln!("linked common-gap native App refresh: restore={restoration_latency:?} ref={ref_latency:?} branch={branch_latency:?}");
}

fn upstream_config(reference: &str) -> String {
    format!("[branch \"main\"]\nremote = .\nmerge = refs/heads/{reference}\n")
}

#[test]
fn git_watch_repair_external_include_atomic_replacement_and_graph_change() {
    isolated_git_watch_test!(git_watch_repair_external_include_atomic_replacement_and_graph_change);
    let repo = GitWatchRepo::new("external-config");
    let external = GitWatchRepo::new("include-files");
    repo.init();
    repo.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let include = external.0.join("included-config");
    std::fs::write(&include, upstream_config("upstream")).unwrap();
    let include_config_path = git_config_path(&include);
    repo.git(&["config", "include.path", &include_config_path]);
    let mut app = repo.app();
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let metadata_before = std::fs::read(repo.0.join(".git/config")).unwrap();
    let replacement = external.0.join("replacement");
    std::fs::write(&replacement, upstream_config("main")).unwrap();
    std::fs::rename(&replacement, &include).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));

    let nested = external.0.join("new-config");
    std::fs::write(
        &include,
        format!("[include]\npath = {}\n", git_config_path(&nested)),
    )
    .unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    // Missing dependencies must already be registered, even with unchanged roots.
    std::fs::write(&nested, upstream_config("main")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    std::fs::write(&nested, upstream_config("upstream")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    assert_eq!(
        std::fs::read(repo.0.join(".git/config")).unwrap(),
        metadata_before
    );
}

#[cfg(unix)]
#[test]
fn git_watch_repair_external_symlink_retarget_then_target_edit() {
    isolated_git_watch_test!(git_watch_repair_external_symlink_retarget_then_target_edit);
    let repo = GitWatchRepo::new("symlink-config");
    let external = GitWatchRepo::new("symlink-files");
    repo.init();
    repo.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let first = external.0.join("first");
    let other_dir = external.0.join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let second = other_dir.join("second");
    let include = external.0.join("include");
    std::fs::write(&first, upstream_config("upstream")).unwrap();
    std::fs::write(&second, upstream_config("main")).unwrap();
    std::os::unix::fs::symlink(&first, &include).unwrap();
    repo.git(&["config", "include.path", include.to_str().unwrap()]);
    let mut app = repo.app();
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let replacement = external.0.join("replacement");
    std::os::unix::fs::symlink(&second, &replacement).unwrap();
    std::fs::rename(replacement, include).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    std::fs::write(second, upstream_config("upstream")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
}

#[cfg(unix)]
#[test]
fn git_watch_repair_directory_symlink_retarget_and_missing_parent_migration() {
    isolated_git_watch_test!(
        git_watch_repair_directory_symlink_retarget_and_missing_parent_migration
    );
    let repo = GitWatchRepo::new("directory-symlink-config");
    let external = GitWatchRepo::new("directory-symlink-files");
    repo.init();
    repo.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let first = external.0.join("first");
    let second = external.0.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(first.join("config"), upstream_config("upstream")).unwrap();
    std::fs::write(second.join("config"), upstream_config("main")).unwrap();
    let link = external.0.join("logical-directory");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    repo.git(&[
        "config",
        "include.path",
        link.join("config").to_str().unwrap(),
    ]);
    let mut app = repo.app();
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let replacement = external.0.join("new-link");
    std::os::unix::fs::symlink(&second, &replacement).unwrap();
    std::fs::rename(replacement, &link).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));

    // Atomic target edits must survive retiring the old canonical directory.
    let temporary = second.join("new-config");
    std::fs::write(&temporary, upstream_config("upstream")).unwrap();
    std::fs::rename(&temporary, second.join("config")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let missing = link.join("nested/config");
    std::fs::write(
        second.join("config"),
        format!("[include]\npath = {}\n", git_config_path(&missing)),
    )
    .unwrap();
    drive_git_watch_refresh(&mut app);
    std::fs::create_dir(second.join("nested")).unwrap();
    drive_git_watch_refresh(&mut app);
    std::fs::write(second.join("nested/config"), upstream_config("main")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    std::fs::write(&temporary, upstream_config("upstream")).unwrap();
    std::fs::rename(temporary, second.join("nested/config")).unwrap();
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
}

#[test]
fn git_watch_repair_config_reload_expands_branch_only_demand_on_quiet_repo() {
    isolated_git_watch_test!(
        git_watch_repair_config_reload_expands_branch_only_demand_on_quiet_repo
    );
    for in_flight in [false, true] {
        let repo = GitWatchRepo::new("demand-growth");
        repo.init();
        repo.git(&["commit", "--allow-empty", "-m", "ahead"]);
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Branch]];
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("branch-only");
        ws.tabs.clear();
        ws.identity_cwd = repo.0.clone();
        app.state.workspaces.push(ws);
        app.mark_git_status_refresh_due(Instant::now());
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), None);
        if in_flight {
            app.mark_git_status_refresh_due(Instant::now());
            app.start_git_status_refresh_if_due(Instant::now());
            assert!(app.git_refresh_in_flight);
        }
        let config_path = repo.0.join("herdr-config.toml");
        std::fs::write(
            &config_path,
            "[ui.sidebar.spaces]\nrows = [[\"branch\", \"git_status\"]]\n",
        )
        .unwrap();
        let process_config_path = std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR);
        let env = crate::environment::test_env();
        env.set(crate::config::CONFIG_PATH_ENV_VAR, &config_path);
        let report = app.reload_config();
        assert_eq!(
            std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR),
            process_config_path
        );
        assert_eq!(report.status, crate::config::ConfigReloadStatus::Applied);
        // No repository mutation or native hint: actual reload must request the read.
        drive_git_watch_refresh(&mut app);
        if in_flight {
            // The real branch-only worker finishes first; its output cannot satisfy
            // new demand. Completion must preserve the immediate rerun request.
            assert_eq!(app.state.workspaces[0].git_ahead_behind(), None);
            drive_git_watch_refresh(&mut app);
        }
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    }
}

// ponytail: APFS cannot create a non-UTF-8 path, so keep this native-path
// fixture on Linux instead of weakening it into a lossy Unicode approximation.
#[cfg(target_os = "linux")]
#[test]
fn git_watch_repair_non_utf8_ancestor_relative_markers_common_ref_update() {
    isolated_git_watch_test!(git_watch_repair_non_utf8_ancestor_relative_markers_common_ref_update);
    use std::os::unix::ffi::OsStringExt;
    let container = GitWatchRepo::new("native-paths");
    let ancestor = container
        .0
        .join(std::ffi::OsString::from_vec(b"ancestor-\xff".to_vec()));
    let repo = GitWatchRepo(ancestor.join("repo"));
    std::fs::create_dir_all(&repo.0).unwrap();
    repo.init();
    let linked = GitWatchRepo(ancestor.join("linked"));
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.0)
        .args(["worktree", "add", "-b", "linked"])
        .arg(&linked.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(
        linked.0.join(".git"),
        "gitdir: ../repo/.git/worktrees/linked\n",
    )
    .unwrap();
    std::fs::write(repo.0.join(".git/worktrees/linked/commondir"), "../..\n").unwrap();
    linked.git(&["branch", "--set-upstream-to=upstream", "linked"]);
    linked.git(&["commit", "--allow-empty", "-m", "ahead"]);
    let mut app = linked.app();
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let tip = linked.git(&["rev-parse", "HEAD"]);
    repo.git(&["update-ref", "refs/heads/upstream", &tip]);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
}

#[test]
fn git_watch_repair_removed_workspace_drops_its_cached_config_dependency() {
    isolated_git_watch_test!(git_watch_repair_removed_workspace_drops_its_cached_config_dependency);
    let first = GitWatchRepo::new("retained-config-root");
    first.init();
    let mut app = first.app();
    let second = GitWatchRepo::new("removed-config-root");
    second.init();
    let mut ws = Workspace::test_new("removed");
    ws.tabs.clear();
    ws.identity_cwd = second.0.clone();
    app.state.workspaces.push(ws);
    drive_git_watch_refresh(&mut app);
    app.state.workspaces.pop();
    app.sync_git_watches();
    // Wait for setup workers and late native hints, not just a fixed sleep.
    drain_git_watch_hints(&mut app);
    second.git(&["config", "unused.removed", "changed"]);
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        app.event_rx.try_recv().is_err(),
        "retired config path still wakes App"
    );
    first.git(&["commit", "--allow-empty", "-m", "retained root"]);
    drive_git_watch_refresh(&mut app);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
}
