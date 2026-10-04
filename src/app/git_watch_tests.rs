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
        assert_eq!(watches.watched.len(), 2); // .git direct + refs recursive
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
