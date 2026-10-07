//! Behavioral watch-registry tests. Included by git_watch to inspect the exact
//! registry; native registration/removal can also be audited with strace.
use super::*;

fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "herdr-git-watch-registry-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("init")
        .output()
        .unwrap();
    assert!(output.status.success());
    root
}

// These regressions use the registry's existing repository helper, then drive
// the real App sync -> native hint -> worker -> apply path (no forced refresh
// after initial setup). A quiet boundary excludes leftover removal hints.
fn fixture_git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Watch Test",
            "-c",
            "user.email=watch@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn fixture_commit(root: &Path) {
    fixture_git(root, &["commit", "--allow-empty", "-m", "initial"]);
}

fn watched_app(root: &Path) -> crate::app::App {
    let mut config = crate::config::Config::default();
    config.ui.sidebar.spaces.rows = vec![vec![
        crate::config::SpaceSidebarToken::Branch,
        crate::config::SpaceSidebarToken::GitStatus,
    ]];
    let mut app = crate::app::App::new(
        &config,
        crate::app::AppPolicy::TEST,
        None,
        mpsc::unbounded_channel().1,
        crate::api::EventHub::default(),
    );
    let mut workspace = crate::workspace::Workspace::test_new("restoration");
    workspace.tabs.clear();
    workspace.identity_cwd = root.to_path_buf();
    app.state.workspaces.push(workspace);
    app.mark_git_status_refresh_due(std::time::Instant::now());
    native_app_refresh(&mut app, false);
    quiet_native_app(&mut app);
    app
}

// Waits for the observed native event and its refresh. The bound only stops a
// hung test; it stays far below the 60 s safety refresh, which these tests
// also assert did not run. The 1 s product target is not a CI wall clock.
const NATIVE_EVENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Polls until the watcher enqueues its event. Load may delay delivery, so
/// only the hang watchdog bounds the wait, not a latency target.
#[track_caller]
fn wait_for_native_event(rx: &mpsc::Receiver<AppEvent>) {
    let start = std::time::Instant::now();
    while rx.is_empty() {
        assert!(
            start.elapsed() < NATIVE_EVENT_DEADLINE,
            "native event not observed within {NATIVE_EVENT_DEADLINE:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[track_caller]
fn native_app_refresh(app: &mut crate::app::App, require_discovery: bool) {
    let start = std::time::Instant::now();
    let safety = app.last_git_repo_discovery_refresh;
    let mut discovery_seen = false;
    loop {
        assert!(
            start.elapsed() < NATIVE_EVENT_DEADLINE,
            "native restoration/refresh not observed within {NATIVE_EVENT_DEADLINE:?} \
             (require_discovery={require_discovery})"
        );
        app.sync_git_watches();
        app.start_git_status_refresh_if_due(std::time::Instant::now());
        while let Ok(event) = app.event_rx.try_recv() {
            if matches!(event, AppEvent::GitFilesChanged) {
                discovery_seen |= app
                    .git_watches
                    .as_ref()
                    .unwrap()
                    .discovery_dirty
                    .load(Ordering::Acquire);
            }
            let completed = matches!(event, AppEvent::GitStatusRefreshed { .. });
            app.handle_internal_event(event);
            if completed {
                assert_eq!(
                    app.last_git_repo_discovery_refresh, safety,
                    "safety discovery cannot satisfy native regression"
                );
                assert!(
                    !require_discovery || discovery_seen,
                    "restoration must invalidate discovery, not only wake the app"
                );
                eprintln!(
                    "native App refresh: {:?}, discovery={discovery_seen}",
                    start.elapsed()
                );
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[track_caller]
fn quiet_native_app(app: &mut crate::app::App) {
    let start = std::time::Instant::now();
    let safety = app.last_git_repo_discovery_refresh;
    let mut quiet = start;
    loop {
        assert!(
            start.elapsed() < NATIVE_EVENT_DEADLINE,
            "native hints did not settle"
        );
        app.sync_git_watches();
        app.start_git_status_refresh_if_due(std::time::Instant::now());
        while let Ok(event) = app.event_rx.try_recv() {
            app.handle_internal_event(event);
            quiet = std::time::Instant::now();
        }
        if app.git_refresh_in_flight || app.git_watch_refresh_deadline.is_some() {
            quiet = std::time::Instant::now();
        } else if quiet.elapsed() >= std::time::Duration::from_millis(150) {
            assert_eq!(app.last_git_repo_discovery_refresh, safety);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Reconciles the removal of an external common dir that has no watched parent.
/// Windows reports no event when a watched directory itself is renamed (notify
/// has no `IN_MOVE_SELF` there), so production notices that removal at the
/// safety refresh. An explicit identity refresh stands in for it, without
/// moving the safety clock. The restoration that follows stays native on
/// every platform: the Restore sentinel watches the nearest existing parent.
#[track_caller]
fn external_common_removal_refresh(app: &mut crate::app::App) {
    #[cfg(windows)]
    app.request_git_identity_refresh(std::time::Instant::now());
    native_app_refresh(app, false);
}

fn external_common_fixture(name: &str, common_name: &str) -> (PathBuf, PathBuf) {
    let root = repository(name);
    let common = root.join(common_name);
    fixture_git(
        &root,
        &["init", &format!("--separate-git-dir={}", common.display())],
    );
    fixture_commit(&root);
    (root, common)
}

fn linked_fixture(root: &Path, linked: &Path) -> String {
    fixture_git(root, &["branch", "upstream"]);
    fixture_git(
        root,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    fixture_git(linked, &["branch", "--set-upstream-to=upstream", "linked"]);
    fixture_commit(linked);
    fixture_git(linked, &["rev-parse", "HEAD"])
}

#[test]
fn git_watch_gap_external_common_entry_restoration_and_refs() {
    let (root, common) = external_common_fixture("external-common", "repo.git");
    let linked = root.with_extension("linked");
    let tip = linked_fixture(&root, &linked);
    let mut app = watched_app(&linked); // Only the linked checkout is a consumer.
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    let retired = root.join("retired-metadata");
    std::fs::rename(&common, &retired).unwrap();
    external_common_removal_refresh(&mut app);
    quiet_native_app(&mut app);
    assert_eq!(app.state.workspaces[0].cached_git_branch, None);
    let missing = app
        .git_status_cache
        .get(&app.state.workspaces[0].cached_git_status_key)
        .unwrap();
    assert!(missing.fingerprint.is_none());
    assert!(missing.config_dependency_paths().is_empty());
    std::fs::rename(&retired, &common).unwrap();
    native_app_refresh(&mut app, true);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("linked")
    );
    quiet_native_app(&mut app);
    fixture_git(&root, &["update-ref", "refs/heads/upstream", &tip]);
    native_app_refresh(&mut app, false);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    drop(app);
    fixture_git(
        &root,
        &["worktree", "remove", "--force", linked.to_str().unwrap()],
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_gap_nested_linked_fallback_retains_nearer_restoration() {
    let ancestor = repository("ancestor-fallback");
    fixture_commit(&ancestor);
    fixture_git(&ancestor, &["switch", "-c", "ancestor"]);
    let (root, common) = external_common_fixture("nested-common", ".bare");
    let linked = ancestor.join("nested-linked");
    let tip = linked_fixture(&root, &linked);
    let mut app = watched_app(&linked);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("linked")
    );
    let retired = root.join("retired-metadata");
    std::fs::rename(&common, &retired).unwrap();
    external_common_removal_refresh(&mut app);
    quiet_native_app(&mut app);
    assert_eq!(
        crate::workspace::git_worktree_info(&linked)
            .unwrap()
            .repo_root,
        ancestor
    );
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("ancestor")
    );
    std::fs::rename(&retired, &common).unwrap();
    native_app_refresh(&mut app, true);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("linked")
    );
    quiet_native_app(&mut app);
    fixture_git(&root, &["update-ref", "refs/heads/upstream", &tip]);
    native_app_refresh(&mut app, false);
    assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
    drop(app);
    fixture_git(
        &root,
        &["worktree", "remove", "--force", linked.to_str().unwrap()],
    );
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(ancestor).unwrap();
}

#[test]
fn git_watch_gap_head_only_restoration_invalidates_negative_discovery() {
    let root = repository("head-only");
    fixture_commit(&root);
    fixture_git(&root, &["switch", "-c", "restored-head"]);
    let mut app = watched_app(&root);
    let head = root.join(".git/HEAD");
    let content = std::fs::read(&head).unwrap();
    std::fs::remove_file(&head).unwrap();
    native_app_refresh(&mut app, false);
    quiet_native_app(&mut app);
    assert_eq!(app.state.workspaces[0].cached_git_branch, None);
    assert!(app
        .git_status_cache
        .get(&app.state.workspaces[0].cached_git_status_key)
        .unwrap()
        .fingerprint
        .is_none());
    std::fs::write(&head, content).unwrap();
    native_app_refresh(&mut app, true);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("restored-head")
    );
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_registry_releases_native_watches_over_100_add_remove_cycles() {
    let root = repository("cycles");
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    for _ in 0..100 {
        watches.sync(HashSet::from([root.clone()]));
        assert_eq!(watches.roots.len(), 1);
        let expected_watches = if cfg!(windows) { 2 } else { 3 };
        assert_eq!(watches.watched.len(), expected_watches); // Windows folds refs into the recursive .git watch.
        assert!(!watches.watched.keys().any(|path| path.ends_with("objects")));
        watches.sync(HashSet::new());
        assert!(watches.roots.is_empty());
        assert!(watches.watched.is_empty());
        assert!(watches.targets.read().unwrap().is_empty());
    }
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_identical_checkout_directories_share_watches_and_remain_until_last_root_removed() {
    let root = repository("sharing");
    let nested = root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([root.clone()]));
    let original = watches.watched.clone();
    watches.sync(HashSet::from([root.clone(), nested.clone()]));
    assert_eq!(watches.watched, original);
    watches.sync(HashSet::from([nested]));
    assert_eq!(watches.watched, original);
    watches.sync(HashSet::new());
    assert!(watches.watched.is_empty());
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_linked_missing_roots_keep_only_their_own_shared_native_sentinels() {
    let root = repository("linked-gap-sharing");
    let first = root.with_extension("first-linked");
    let second = root.with_extension("second-linked");
    for args in [
        vec![
            "-c",
            "user.name=Watch Test",
            "-c",
            "user.email=watch@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
        vec!["worktree", "add", "-b", "first", first.to_str().unwrap()],
        vec!["worktree", "add", "-b", "second", second.to_str().unwrap()],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let native_root = root.canonicalize().unwrap();
    let native_first = first.canonicalize().unwrap();
    let native_second = second.canonicalize().unwrap();
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([first.clone(), second.clone()]));
    for path in [&native_root, &native_first, &native_second] {
        assert!(watches.watched.contains_key(path));
        assert!(watches
            .targets
            .read()
            .unwrap()
            .contains(&WatchTarget::Marker(path.clone())));
    }
    let retired = root.join("retired-metadata");
    std::fs::rename(root.join(".git"), &retired).unwrap();
    // Reconcile both before and after a missing worker result would be applied.
    // No config dependency can lend a watch to either consumer here.
    for _ in 0..2 {
        watches.topology_dirty = true;
        watches.sync(HashSet::from([first.clone(), second.clone()]));
        assert_eq!(
            watches.watched.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([
                native_root.clone(),
                native_first.clone(),
                native_second.clone()
            ])
        );
    }
    watches.sync(HashSet::from([second.clone()]));
    assert_eq!(
        watches.watched.keys().cloned().collect::<HashSet<_>>(),
        HashSet::from([native_root.clone(), native_second.clone()])
    );
    // The consumer's own raw pointer, missing with the common .git.
    let second_pointer = WatchTarget::Pointer {
        file: native_root
            .join(".git/worktrees")
            .join(second.file_name().unwrap()),
        directory: native_root.clone(),
    };
    assert_eq!(
        watches
            .targets
            .read()
            .unwrap()
            .iter()
            .cloned()
            .collect::<HashSet<_>>(),
        HashSet::from([
            // Exact restoration dependencies share the root's marker watch.
            WatchTarget::Restore {
                file: native_root
                    .join(".git/worktrees")
                    .join(second.file_name().unwrap())
                    .join("HEAD"),
                directory: native_root.clone(),
            },
            WatchTarget::Restore {
                file: native_root.join(".git/HEAD"),
                directory: native_root.clone(),
            },
            second_pointer.clone(),
            WatchTarget::Marker(native_root.clone()),
            WatchTarget::Marker(native_second)
        ])
    );
    assert!(!watches.watched.contains_key(&native_first));
    watches.sync(HashSet::new());
    assert!(watches.watched.is_empty());
    assert!(watches.targets.read().unwrap().is_empty());
    // A removed root cannot reacquire its own former markers during the gap.
    // Only the exact missing pointer its existing .git file names is current.
    watches.sync(HashSet::from([second.clone()]));
    assert_eq!(
        watches.watched.keys().cloned().collect::<HashSet<_>>(),
        HashSet::from([native_root.clone()])
    );
    assert_eq!(*watches.targets.read().unwrap(), vec![second_pointer]);
    drop(watches);
    std::fs::rename(retired, root.join(".git")).unwrap();
    for path in [first, second] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Git for Windows marks committed object files read-only. Clear only this
    // owned fixture's object tree after native watches and worktrees are gone.
    #[cfg(windows)]
    {
        #[allow(clippy::permissions_set_readonly_false)] // Windows readonly attribute, not Unix mode bits.
        fn make_owned_objects_writable(path: &Path) {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    make_owned_objects_writable(&entry.unwrap().path());
                }
            }
            let mut permissions = metadata.permissions();
            if permissions.readonly() {
                permissions.set_readonly(false);
                std::fs::set_permissions(path, permissions).unwrap();
            }
        }
        make_owned_objects_writable(&root.join(".git/objects"));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_new_missing_root_cannot_borrow_removed_ancestor_sentinel() {
    let root = repository("no-marker-borrow");
    let nested = root.join("new-consumer");
    std::fs::create_dir(&nested).unwrap();
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([root.clone()]));
    assert!(watches.watched.contains_key(&root.canonicalize().unwrap()));
    std::fs::rename(root.join(".git"), root.join("retired-metadata")).unwrap();
    // The new CWD is beneath a previous sentinel, but never owned that sentinel.
    watches.sync(HashSet::from([nested]));
    assert!(watches.watched.is_empty());
    assert!(watches.targets.read().unwrap().is_empty());
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn git_watch_directory_aliases_share_registration_after_partial_removal() {
    let root = repository("directory-alias");
    let canonical = root.canonicalize().unwrap();
    let alias = root.with_extension("alias");
    std::os::unix::fs::symlink(&canonical, &alias).unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([
        root.clone(),
        alias.clone(),
        canonical.clone(),
    ]));
    let expected_watches = if cfg!(windows) { 2 } else { 3 };
    assert_eq!(watches.watched.len(), expected_watches);
    let registrations = watches.watched.clone();
    watches.sync(HashSet::from([canonical.clone()]));
    assert_eq!(watches.watched, registrations);
    std::fs::write(canonical.join(".git/HEAD.new"), "ref: refs/heads/other\n").unwrap();
    std::fs::rename(canonical.join(".git/HEAD.new"), canonical.join(".git/HEAD")).unwrap();
    wait_for_native_event(&rx);
    assert!(matches!(rx.try_recv().unwrap(), AppEvent::GitFilesChanged));
    watches.sync(HashSet::new());
    assert!(watches.watched.is_empty());
    drop(watches);
    std::fs::remove_file(alias).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_structural_filter_accepts_directory_recreation_events() {
    use notify::event::{CreateKind, EventKind, RemoveKind};
    let dir = PathBuf::from("repo/.git");
    let targets = vec![WatchTarget::Metadata(dir.clone())];
    let create_any = Event::new(EventKind::Create(CreateKind::Any)).add_path(dir.clone());
    let create_folder = Event::new(EventKind::Create(CreateKind::Folder)).add_path(dir.clone());
    let remove_folder = Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(dir);
    assert!(structural_event(&create_any, &targets));
    assert!(structural_event(&create_folder, &targets));
    assert!(structural_event(&remove_folder, &targets));
}

#[test]
fn git_watch_armed_restore_ignores_parent_directory_events() {
    use notify::event::{CreateKind, DataChange, ModifyKind};
    let git_dir = PathBuf::from("d/.git");
    let targets = vec![WatchTarget::Restore {
        file: git_dir.join("HEAD"),
        directory: git_dir.clone(),
    }];
    // A directory-level notification for an unrelated child is not restoration.
    let parent =
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(git_dir.clone());
    assert!(relevant_event(&parent, &targets));
    assert!(!structural_event(&parent, &targets));
    let head = Event::new(EventKind::Create(CreateKind::File)).add_path(git_dir.join("HEAD"));
    assert!(structural_event(&head, &targets));
    // A missing component created beneath the watched nearest ancestor counts.
    let ancestor = vec![WatchTarget::Restore {
        file: PathBuf::from("d/.bare/HEAD"),
        directory: PathBuf::from("d"),
    }];
    let component =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(PathBuf::from("d/.bare"));
    assert!(structural_event(&component, &ancestor));
}

#[test]
fn git_watch_retargeted_pointer_keeps_restore_sentinels_bounded() {
    let base = std::env::temp_dir().join(format!(
        "herdr-git-watch-registry-retarget-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let consumer = base.join("consumer");
    std::fs::create_dir_all(&consumer).unwrap();
    let metadata = |i: usize| base.join(format!("meta-{i}")).join("repo.git");
    let init = |i: usize| {
        std::fs::create_dir_all(metadata(i).parent().unwrap()).unwrap();
        // Reinitializing moves the previous metadata and rewrites the pointer,
        // so the old HEAD is missing before the next sync.
        fixture_git(
            &consumer,
            &[
                "init",
                &format!("--separate-git-dir={}", metadata(i).display()),
            ],
        );
    };
    init(0);
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([consumer.clone()]));
    let restores = |watches: &GitWatches| {
        watches.root_markers[&consumer]
            .iter()
            .filter(|target| matches!(target, WatchTarget::Restore { .. }))
            .count()
    };
    let mut baseline = None;
    for i in 1..=3 {
        init(i);
        assert!(!metadata(i - 1).exists());
        watches.topology_dirty = true;
        watches.sync(HashSet::from([consumer.clone()]));
        let counts = (restores(&watches), watches.watched.len());
        assert!(counts.0 <= 2, "iteration {i}: {counts:?}");
        assert_eq!(*baseline.get_or_insert(counts), counts, "iteration {i}");
    }
    drop(watches);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn git_watch_nested_retarget_chain_keeps_restore_sentinels_bounded() {
    let base = std::env::temp_dir().join(format!(
        "herdr-git-watch-registry-nested-retarget-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let consumer = base.join("consumer");
    std::fs::create_dir_all(&consumer).unwrap();
    // Each metadata dir sits inside the previous one: D0, D0/m1.git, ...
    let mut metadata = vec![base.join("m0.git")];
    for i in 1..=3 {
        let next = metadata[i - 1].join(format!("m{i}.git"));
        metadata.push(next);
    }
    let init = |i: usize| {
        let scratch = base.join(format!("scratch-{i}"));
        std::fs::create_dir_all(&scratch).unwrap();
        fixture_git(
            &scratch,
            &[
                "init",
                &format!("--separate-git-dir={}", metadata[i].display()),
            ],
        );
        std::fs::write(
            consumer.join(".git"),
            format!("gitdir: {}\n", metadata[i].display()),
        )
        .unwrap();
    };
    init(0);
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([consumer.clone()]));
    let restores = |watches: &GitWatches| {
        watches.root_markers[&consumer]
            .iter()
            .filter(|target| matches!(target, WatchTarget::Restore { .. }))
            .count()
    };
    let mut baseline = None;
    for i in 1..=3 {
        init(i);
        std::fs::remove_file(metadata[i - 1].join("HEAD")).unwrap();
        watches.topology_dirty = true;
        watches.sync(HashSet::from([consumer.clone()]));
        let counts = (restores(&watches), watches.watched.len());
        assert!(counts.0 <= 2, "iteration {i}: {counts:?}");
        assert_eq!(*baseline.get_or_insert(counts), counts, "iteration {i}");
    }
    drop(watches);
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(unix)]
#[test]
fn git_watch_gap_symlinked_metadata_keeps_restore_sentinel_while_dangling() {
    let ancestor = repository("symlink-ancestor");
    fixture_commit(&ancestor);
    fixture_git(&ancestor, &["switch", "-c", "ancestor"]);
    let consumer = ancestor.join("consumer");
    std::fs::create_dir(&consumer).unwrap();
    let store = ancestor.with_extension("store");
    std::fs::create_dir(&store).unwrap();
    let real = store.join("real.git");
    fixture_git(
        &consumer,
        &["init", &format!("--separate-git-dir={}", real.display())],
    );
    fixture_git(&consumer, &["switch", "-c", "aliased"]);
    fixture_commit(&consumer);
    // The consumer reaches its metadata only through a symlink alias.
    let alias = store.join("alias.git");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    std::fs::write(
        consumer.join(".git"),
        format!("gitdir: {}\n", alias.display()),
    )
    .unwrap();
    let mut app = watched_app(&consumer);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("aliased")
    );
    // Leave the alias dangling; discovery falls back to the ancestor.
    let retired = store.join("retired.git");
    std::fs::rename(&real, &retired).unwrap();
    native_app_refresh(&mut app, false);
    quiet_native_app(&mut app);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("ancestor")
    );
    let native_store = store.canonicalize().unwrap();
    let watches = app.git_watches.as_ref().unwrap();
    assert!(watches
        .targets
        .read()
        .unwrap()
        .contains(&WatchTarget::Restore {
            file: native_store.join("real.git/HEAD"),
            directory: native_store.clone(),
        }));
    assert!(watches.watched.contains_key(&native_store));
    std::fs::rename(&retired, &real).unwrap();
    native_app_refresh(&mut app, true);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("aliased")
    );
    drop(app);
    std::fs::remove_dir_all(store).unwrap();
    std::fs::remove_dir_all(ancestor).unwrap();
}

#[cfg(unix)]
#[test]
fn git_watch_gap_pointer_alias_removal_recreation_and_retarget_are_structural() {
    // The old resolved git dir and its HEAD stay intact throughout, so only an
    // exact sentinel on the raw alias can notice these changes natively.
    let ancestor = repository("pointer-alias-ancestor");
    fixture_commit(&ancestor);
    fixture_git(&ancestor, &["switch", "-c", "ancestor"]);
    let consumer = ancestor.join("consumer");
    std::fs::create_dir(&consumer).unwrap();
    let store = ancestor.with_extension("pointer-store");
    std::fs::create_dir(&store).unwrap();
    let other = store.join("other.git");
    let scratch = store.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    fixture_git(
        &scratch,
        &["init", &format!("--separate-git-dir={}", other.display())],
    );
    fixture_git(&scratch, &["switch", "-c", "retargeted"]);
    fixture_commit(&scratch);
    let real = store.join("real.git");
    fixture_git(
        &consumer,
        &["init", &format!("--separate-git-dir={}", real.display())],
    );
    fixture_git(&consumer, &["switch", "-c", "aliased"]);
    fixture_commit(&consumer);
    let alias = store.join("alias.git");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    std::fs::write(
        consumer.join(".git"),
        format!("gitdir: {}\n", alias.display()),
    )
    .unwrap();
    let mut app = watched_app(&consumer);
    let branch = |app: &crate::app::App| app.state.workspaces[0].cached_git_branch.clone();
    assert_eq!(branch(&app).as_deref(), Some("aliased"));
    let sentinel = WatchTarget::Pointer {
        file: store.canonicalize().unwrap().join("alias.git"),
        directory: store.canonicalize().unwrap(),
    };
    let installed = |app: &crate::app::App| {
        let watches = app.git_watches.as_ref().unwrap();
        watches.targets.read().unwrap().contains(&sentinel)
            && watches.watched.contains_key(sentinel.directory())
    };
    assert!(installed(&app));
    // Remove the alias; real.git/HEAD stays, discovery falls back.
    std::fs::remove_file(&alias).unwrap();
    native_app_refresh(&mut app, true);
    quiet_native_app(&mut app);
    assert_eq!(branch(&app).as_deref(), Some("ancestor"));
    assert!(installed(&app));
    // Recreate it.
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    native_app_refresh(&mut app, true);
    quiet_native_app(&mut app);
    assert_eq!(branch(&app).as_deref(), Some("aliased"));
    // Retarget it by atomic rename-over; the old target stays intact.
    let staged = store.join("alias.tmp");
    std::os::unix::fs::symlink(&other, &staged).unwrap();
    std::fs::rename(&staged, &alias).unwrap();
    native_app_refresh(&mut app, true);
    quiet_native_app(&mut app);
    assert_eq!(branch(&app).as_deref(), Some("retargeted"));
    assert!(real.join("HEAD").exists());
    drop(app);
    std::fs::remove_dir_all(store).unwrap();
    std::fs::remove_dir_all(ancestor).unwrap();
}

#[test]
fn git_watch_pointer_sentinel_is_exact() {
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
    let store = PathBuf::from("store");
    let targets = vec![WatchTarget::Pointer {
        file: store.join("alias.git"),
        directory: store.clone(),
    }];
    for kind in [
        EventKind::Create(CreateKind::File),
        EventKind::Remove(RemoveKind::File),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)),
    ] {
        let event = Event::new(kind).add_path(store.join("alias.git"));
        assert!(relevant_event(&event, &targets), "{kind:?}");
        assert!(structural_event(&event, &targets), "{kind:?}");
    }
    // Sibling entries and the watched directory itself are not the alias.
    for path in [store.join("real.git"), store.clone()] {
        let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path);
        assert!(!structural_event(&event, &targets));
    }
}

#[test]
fn git_watch_carried_marker_is_decided_by_discovery_not_a_later_filesystem_read() {
    // Race: discovery fell back to the ancestor while nested/.git was missing,
    // then .git was restored before reconciliation. The nested Marker must
    // still be carried (and installed), or the restoration event finds no
    // filter and waits for the safety refresh.
    let ancestor = repository("carried-marker-race");
    let nested = ancestor.join("nested");
    std::fs::create_dir_all(nested.join(".git")).unwrap(); // restored already
    let root = nested.join("sub");
    let elsewhere = ancestor.with_extension("elsewhere");
    let previous = [
        WatchTarget::Marker(nested.clone()),
        WatchTarget::Marker(elsewhere), // not on the consumer's chain
        WatchTarget::Marker(ancestor.clone()),
    ];
    let fallback = [WatchTarget::Marker(ancestor.clone())];
    assert_eq!(
        carried_markers(&previous, &fallback, &root, &root),
        vec![WatchTarget::Marker(nested.clone())]
    );
    // Retired once discovery reaches the nested repository again.
    let healthy = [WatchTarget::Marker(nested.clone())];
    assert!(carried_markers(&previous, &healthy, &root, &root).is_empty());
    std::fs::remove_dir_all(ancestor).unwrap();
}

#[test]
fn git_watch_gap_nested_repo_marker_survives_ancestor_fallback() {
    let ancestor = repository("nested-marker-ancestor");
    fixture_commit(&ancestor);
    fixture_git(&ancestor, &["switch", "-c", "ancestor"]);
    let nested = ancestor.join("nested");
    std::fs::create_dir(&nested).unwrap();
    fixture_git(&nested, &["init"]);
    fixture_git(&nested, &["switch", "-c", "nested"]);
    fixture_commit(&nested);
    let consumer = nested.join("sub");
    std::fs::create_dir(&consumer).unwrap();
    let mut app = watched_app(&consumer);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("nested")
    );
    let retired = nested.join("retired-metadata");
    std::fs::rename(nested.join(".git"), &retired).unwrap();
    native_app_refresh(&mut app, false);
    quiet_native_app(&mut app);
    assert_eq!(
        crate::workspace::git_worktree_info(&consumer)
            .unwrap()
            .repo_root,
        ancestor
    );
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("ancestor")
    );
    let native_nested = nested.canonicalize().unwrap();
    let watches = app.git_watches.as_ref().unwrap();
    assert!(watches
        .targets
        .read()
        .unwrap()
        .contains(&WatchTarget::Marker(native_nested.clone())));
    assert!(watches.watched.contains_key(&native_nested));
    std::fs::rename(&retired, nested.join(".git")).unwrap();
    native_app_refresh(&mut app, true);
    assert_eq!(
        app.state.workspaces[0].cached_git_branch.as_deref(),
        Some("nested")
    );
    drop(app);
    std::fs::remove_dir_all(ancestor).unwrap();
}

#[cfg(unix)]
#[test]
fn git_watch_retargeted_symlink_alias_keeps_restore_sentinels_bounded() {
    let base = std::env::temp_dir().join(format!(
        "herdr-git-watch-registry-alias-retarget-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let consumer = base.join("consumer");
    let store = base.join("store");
    std::fs::create_dir_all(&consumer).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    let metadata: Vec<_> = (0..=3).map(|i| store.join(format!("t{i}.git"))).collect();
    for (i, dir) in metadata.iter().enumerate() {
        let scratch = base.join(format!("scratch-{i}"));
        std::fs::create_dir_all(&scratch).unwrap();
        fixture_git(
            &scratch,
            &["init", &format!("--separate-git-dir={}", dir.display())],
        );
    }
    // The raw pointer stays the alias; only the alias target changes.
    let alias = store.join("alias.git");
    std::fs::write(
        consumer.join(".git"),
        format!("gitdir: {}\n", alias.display()),
    )
    .unwrap();
    let retarget = |i: usize| {
        let _ = std::fs::remove_file(&alias);
        std::os::unix::fs::symlink(&metadata[i], &alias).unwrap();
    };
    retarget(0);
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([consumer.clone()]));
    let restores = |watches: &GitWatches| {
        watches.root_markers[&consumer]
            .iter()
            .filter(|target| matches!(target, WatchTarget::Restore { .. }))
            .count()
    };
    let mut baseline = None;
    for i in 1..=3 {
        retarget(i);
        std::fs::remove_file(metadata[i - 1].join("HEAD")).unwrap();
        watches.topology_dirty = true;
        watches.sync(HashSet::from([consumer.clone()]));
        let counts = (restores(&watches), watches.watched.len());
        assert!(counts.0 <= 2, "iteration {i}: {counts:?}");
        assert_eq!(*baseline.get_or_insert(counts), counts, "iteration {i}");
    }
    drop(watches);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn git_watch_healthy_head_and_index_replacements_stay_non_structural() {
    use notify::event::{ModifyKind, RenameMode};
    let root = repository("healthy-head");
    let (tx, _rx) = mpsc::channel(8);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([root.clone()]));
    let targets = watches.targets.read().unwrap().clone();
    // Restoration sentinels are stored, but installed only while missing.
    assert!(!targets
        .iter()
        .any(|target| matches!(target, WatchTarget::Restore { .. })));
    let git_dir = root.canonicalize().unwrap().join(".git");
    for name in ["HEAD", "index"] {
        let replacement = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(git_dir.join(format!("{name}.lock")))
            .add_path(git_dir.join(name));
        assert!(relevant_event(&replacement, &targets), "{name}");
        assert!(!structural_event(&replacement, &targets), "{name}");
    }
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_event_filter_ignores_reads_locks_and_objects_but_accepts_atomic_replacements() {
    use notify::event::{AccessKind, ModifyKind, RenameMode};
    let dir = PathBuf::from("repo/.git");
    let targets = vec![
        WatchTarget::Metadata(dir.clone()),
        WatchTarget::Refs(dir.join("refs")),
    ];
    let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(dir.join("HEAD"));
    assert!(!relevant_event(&access, &targets));
    let replacement = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(dir.join("index.lock"))
        .add_path(dir.join("index"));
    assert!(relevant_event(&replacement, &targets));
    for path in [
        "objects/aa/oid",
        "logs/HEAD",
        "refs/heads/main.lock",
        "index.lock",
    ] {
        let event = Event::new(EventKind::Any).add_path(dir.join(path));
        assert!(!relevant_event(&event, &targets), "{path}");
    }
    for path in [
        "HEAD",
        "index",
        "packed-refs",
        "config",
        "config.worktree",
        "refs/heads/nested/topic",
    ] {
        let event = Event::new(EventKind::Any).add_path(dir.join(path));
        assert!(relevant_event(&event, &targets), "{path}");
    }
}

#[test]
fn git_watch_native_burst_enqueues_one_nonblocking_app_event() {
    let root = repository("coalescing");
    let (tx, mut rx) = mpsc::channel(1);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([root.clone()]));
    for i in 0..100 {
        std::fs::write(root.join(".git/index"), i.to_string()).unwrap();
    }
    wait_for_native_event(&rx);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(rx.len(), 1);
    assert!(matches!(rx.try_recv().unwrap(), AppEvent::GitFilesChanged));
    assert!(rx.try_recv().is_err());
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn git_watch_retargeted_commondir_keeps_restore_sentinels_bounded() {
    let root = repository("commondir-retarget");
    fixture_commit(&root);
    let linked = root.with_extension("linked");
    fixture_git(
        &root,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    let git_dir = PathBuf::from(fixture_git(&linked, &["rev-parse", "--absolute-git-dir"]));
    let commons: Vec<_> = (1..=3)
        .map(|i| {
            let common = repository(&format!("commondir-target-{i}"));
            fixture_commit(&common);
            common
        })
        .collect();
    let (tx, _rx) = mpsc::channel(256);
    let mut watches = GitWatches::new(tx).unwrap();
    watches.sync(HashSet::from([linked.clone()]));
    let restores = |watches: &GitWatches| {
        watches.root_markers[&linked]
            .iter()
            .filter(|target| matches!(target, WatchTarget::Restore { .. }))
            .count()
    };
    let mut previous = root.join(".git");
    let mut baseline = None;
    for (i, common) in commons.iter().enumerate() {
        // The raw .git pointer stays G and G/HEAD stays present.
        let next = common.join(".git");
        std::fs::write(git_dir.join("commondir"), format!("{}\n", next.display())).unwrap();
        std::fs::remove_file(previous.join("HEAD")).unwrap();
        watches.topology_dirty = true;
        watches.sync(HashSet::from([linked.clone()]));
        assert_eq!(
            crate::workspace::git_worktree_info(&linked)
                .unwrap()
                .git_common_dir,
            next.canonicalize().unwrap()
        );
        let counts = (restores(&watches), watches.watched.len());
        assert!(counts.0 <= 2, "iteration {i}: {counts:?}");
        assert_eq!(*baseline.get_or_insert(counts), counts, "iteration {i}");
        previous = next;
    }
    drop(watches);
    for path in commons.into_iter().chain([root, linked]) {
        std::fs::remove_dir_all(path).unwrap();
    }
}
