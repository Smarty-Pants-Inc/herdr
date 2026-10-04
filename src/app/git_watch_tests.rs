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
    assert_eq!(
        watches
            .targets
            .read()
            .unwrap()
            .iter()
            .cloned()
            .collect::<HashSet<_>>(),
        HashSet::from([
            WatchTarget::Marker(native_root),
            WatchTarget::Marker(native_second)
        ])
    );
    assert!(!watches.watched.contains_key(&native_first));
    watches.sync(HashSet::new());
    assert!(watches.watched.is_empty());
    assert!(watches.targets.read().unwrap().is_empty());
    // A removed root cannot reacquire its own former markers during the gap.
    watches.sync(HashSet::from([second.clone()]));
    assert!(watches.watched.is_empty());
    assert!(watches.targets.read().unwrap().is_empty());
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
    let start = std::time::Instant::now();
    while rx.is_empty() {
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
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
    let start = std::time::Instant::now();
    while rx.is_empty() {
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(rx.len(), 1);
    assert!(matches!(rx.try_recv().unwrap(), AppEvent::GitFilesChanged));
    assert!(rx.try_recv().is_err());
    drop(watches);
    std::fs::remove_dir_all(root).unwrap();
}
