//! Native watches for git metadata, not the working tree or object database.
//!
//! The app loop reconciles workspace roots. Identical directories (including
//! linked worktrees' common refs) share one native watch. Callbacks only filter
//! paths and try to enqueue one wakeup; all discovery and refresh stays on the
//! existing app refresh path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::events::AppEvent;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WatchTarget {
    Metadata(PathBuf),
    Refs(PathBuf),
    Marker(PathBuf),
    ConfigFile { file: PathBuf, directory: PathBuf },
    // A vanished HEAD of a previously discovered git dir; installed only while
    // missing, so restoring it invalidates negative discovery.
    Restore { file: PathBuf, directory: PathBuf },
    // A consumer's raw git dir pointer while it is a symlink alias or missing:
    // creating, removing or retargeting it changes discovery even while the
    // old resolved git dir and its HEAD stay intact.
    Pointer { file: PathBuf, directory: PathBuf },
}

impl WatchTarget {
    fn directory(&self) -> &Path {
        match self {
            Self::Metadata(path) | Self::Refs(path) | Self::Marker(path) => path,
            Self::ConfigFile { directory, .. }
            | Self::Restore { directory, .. }
            | Self::Pointer { directory, .. } => directory,
        }
    }

    fn recursive_mode(&self) -> RecursiveMode {
        match self {
            Self::Refs(_) => RecursiveMode::Recursive,
            #[cfg(windows)]
            Self::Metadata(_) => RecursiveMode::Recursive,
            _ => RecursiveMode::NonRecursive,
        }
    }

    fn matches(&self, path: &Path) -> bool {
        let directory = self.directory();
        if let Self::ConfigFile { file, .. }
        | Self::Restore { file, .. }
        | Self::Pointer { file, .. } = self
        {
            // Parents are watched non-recursively so rename-over and creation
            // of a previously missing dependency are observed without HOME scans.
            return path == file || file.starts_with(path);
        }
        if path == directory {
            return true;
        }
        match self {
            Self::Refs(_) => {
                path.starts_with(directory)
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
            }
            Self::Marker(_) => {
                path.parent() == Some(directory)
                    && path.file_name().is_some_and(|name| name == ".git")
            }
            Self::ConfigFile { .. } | Self::Restore { .. } | Self::Pointer { .. } => false, // handled above
            Self::Metadata(_) => {
                path.parent() == Some(directory)
                    && path.file_name().is_some_and(|name| {
                        matches!(
                            name.to_str(),
                            Some(
                                "HEAD"
                                    | "index"
                                    | "packed-refs"
                                    | "commondir"
                                    | "config"
                                    | "config.worktree"
                                    | "refs"
                                    | "reftable"
                            )
                        )
                    })
            }
        }
    }
}

fn relevant_event(event: &Event, targets: &[WatchTarget]) -> bool {
    // Reading HEAD/refs during refresh must not feed back into another refresh.
    event.need_rescan()
        || (!matches!(event.kind, EventKind::Access(_))
            && event
                .paths
                .iter()
                .any(|path| targets.iter().any(|target| target.matches(path))))
}

fn structural_event(event: &Event, targets: &[WatchTarget]) -> bool {
    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    // Narrower than relevance: the missing file itself, or a missing component
    // created beneath the watched ancestor. Directory-level notifications for
    // unrelated children (e.g. `.git/index` while HEAD is absent) do not count.
    let restored = !matches!(event.kind, EventKind::Access(_))
        && event.paths.iter().any(|path| {
            targets.iter().any(|target| match target {
                WatchTarget::Restore { file, directory }
                | WatchTarget::Pointer { file, directory } => {
                    path == file || (file.starts_with(path) && !directory.starts_with(path))
                }
                _ => false,
            })
        });
    event.need_rescan()
        || restored
        || (matches!(
            event.kind,
            EventKind::Create(CreateKind::Any)
                | EventKind::Create(CreateKind::Folder)
                | EventKind::Remove(RemoveKind::Folder)
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Remove(RemoveKind::Any)
        ) && event.paths.iter().any(|path| {
            targets.iter().any(|target| {
                target.directory().starts_with(path)
                    || matches!(target, WatchTarget::Marker(_)) && target.matches(path)
                    || matches!(target, WatchTarget::Metadata(_))
                        && path.parent() == Some(target.directory())
                        && path
                            .file_name()
                            .is_some_and(|name| name == "refs" || name == "reftable")
            })
        }))
}

pub(super) struct GitWatches {
    watcher: RecommendedWatcher,
    roots: HashSet<PathBuf>,
    // Sentinels belong to consumers, not path ancestry: a linked checkout's
    // common .git may live beside (or entirely outside) its workspace CWD.
    // Holds Marker sentinels plus Restore HEAD files of past discoveries.
    root_markers: HashMap<PathBuf, Vec<WatchTarget>>,
    // Raw .git pointer, and that git dir's resolved identity, read when each
    // root's sentinels were last stored.
    root_pointers: HashMap<PathBuf, (PathBuf, PointerIdentity)>,
    watched: HashMap<PathBuf, RecursiveMode>,
    targets: Arc<RwLock<Vec<WatchTarget>>>,
    wakeup_pending: Arc<AtomicBool>,
    structural_dirty: Arc<AtomicBool>,
    discovery_dirty: Arc<AtomicBool>,
    config_dependencies: HashSet<PathBuf>,
    pub(super) topology_dirty: bool,
    pub(super) rearm_requested: bool,
}

impl GitWatches {
    pub(super) fn new(event_tx: mpsc::Sender<AppEvent>) -> notify::Result<Self> {
        let targets = Arc::new(RwLock::new(Vec::<WatchTarget>::new()));
        let callback_targets = targets.clone();
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let callback_pending = wakeup_pending.clone();
        let structural_dirty = Arc::new(AtomicBool::new(false));
        let callback_structural = structural_dirty.clone();
        let discovery_dirty = Arc::new(AtomicBool::new(false));
        let callback_discovery = discovery_dirty.clone();
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let relevant = match result {
                    Ok(event) => match callback_targets.try_read() {
                        Ok(targets) => {
                            if structural_event(&event, &targets) {
                                callback_structural.store(true, Ordering::Release);
                                callback_discovery.store(true, Ordering::Release);
                            }
                            relevant_event(&event, &targets)
                        }
                        // Never block the native callback on app reconciliation.
                        Err(_) => {
                            callback_structural.store(true, Ordering::Release);
                            callback_discovery.store(true, Ordering::Release);
                            !matches!(event.kind, EventKind::Access(_))
                        }
                    },
                    Err(_) => {
                        callback_structural.store(true, Ordering::Release);
                        callback_discovery.store(true, Ordering::Release);
                        true // Overflow/backend errors request re-arming too.
                    }
                };
                if relevant
                    && !callback_pending.swap(true, Ordering::AcqRel)
                    && event_tx.try_send(AppEvent::GitFilesChanged).is_err()
                {
                    // A full app channel may lose this hint; the 60s safety net
                    // still refreshes, and a subsequent event may retry it.
                    callback_pending.store(false, Ordering::Release);
                }
            },
            notify::Config::default().with_follow_symlinks(false),
        )?;
        Ok(Self {
            watcher,
            roots: HashSet::new(),
            root_markers: HashMap::new(),
            root_pointers: HashMap::new(),
            watched: HashMap::new(),
            targets,
            wakeup_pending,
            structural_dirty,
            discovery_dirty,
            config_dependencies: HashSet::new(),
            topology_dirty: false,
            rearm_requested: false,
        })
    }

    pub(super) fn take_discovery_hint(&self) -> bool {
        self.discovery_dirty.swap(false, Ordering::AcqRel)
    }

    pub(super) fn acknowledge(&self) {
        self.wakeup_pending.store(false, Ordering::Release);
    }

    pub(super) fn roots_changed(&self, roots: &HashSet<PathBuf>) -> bool {
        self.roots != *roots
    }

    pub(super) fn has_new_roots(&self, roots: &HashSet<PathBuf>) -> bool {
        roots.iter().any(|root| !self.roots.contains(root))
    }

    pub(super) fn set_config_dependencies(&mut self, paths: HashSet<PathBuf>) {
        self.config_dependencies = paths;
        // Reconcile after each actual worker read, even for equal roots/deps:
        // a missing dependency's parent may now exist, or an include moved.
        self.topology_dirty = true;
    }

    pub(super) fn sync(&mut self, roots: HashSet<PathBuf>) {
        // Native directory registrations are inode-bound. Only structural
        // invalidations re-arm them; ordinary atomic HEAD/index writes do not.
        self.rearm_requested |= self.structural_dirty.swap(false, Ordering::AcqRel);
        if self.roots == roots && !self.topology_dirty && !self.rearm_requested {
            return;
        }
        let rearm = std::mem::take(&mut self.rearm_requested);
        self.roots = roots;
        self.topology_dirty = false;
        self.root_markers
            .retain(|root, _| self.roots.contains(root));
        self.root_pointers
            .retain(|root, _| self.roots.contains(root));
        let mut targets = HashSet::new();
        for root in &self.roots {
            let discovered = targets_for_root(root);
            if !discovered.is_empty() {
                // Successful discovery replaces this consumer's sentinels.
                // Store native paths while they exist, before a removal gap.
                // Still-missing Restore files survive an ancestor fallback only
                // while the raw .git pointer is unchanged since they were
                // stored; one pointer yields at most two HEAD sentinels.
                let previous = self.root_markers.remove(root).unwrap_or_default();
                // The key also includes G/commondir and the resolved git and
                // common dirs: a retargeted common dir or symlink alias with an
                // unchanged raw pointer must not carry old HEADs. While a
                // component is unresolvable (G vanished or dangles), the stored
                // value is kept so a later resolved value is compared to it.
                let previous_key = self.root_pointers.remove(root);
                let pointer = current_git_dir_pointer(root);
                let identity = pointer.as_deref().map(PointerIdentity::read);
                let same_pointer = match (&previous_key, &pointer, &identity) {
                    (Some((previous, stored)), Some(current), Some(identity)) => {
                        previous == current && identity.compatible_with(stored)
                    }
                    _ => false,
                };
                let native_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                let discovered_sentinels: Vec<_> = discovered
                    .iter()
                    .filter(|target| {
                        matches!(target, WatchTarget::Marker(_) | WatchTarget::Restore { .. })
                    })
                    .cloned()
                    .map(native_target)
                    .collect();
                // Decided once, from this discovery, and used for both storage
                // and installation (no second filesystem read can split them).
                let carried = carried_markers(&previous, &discovered_sentinels, root, &native_root);
                let mut sentinels = Vec::new();
                let candidates = discovered_sentinels
                    .into_iter()
                    .chain(carried.iter().cloned())
                    .chain(previous.into_iter().filter(|target| {
                        same_pointer
                            && matches!(target, WatchTarget::Restore { file, .. }
                                if std::fs::symlink_metadata(file).is_err())
                    }));
                for target in candidates {
                    if !sentinels.contains(&target) {
                        sentinels.push(target);
                    }
                }
                targets.extend(sentinels.iter().filter_map(missing_restore));
                targets.extend(carried);
                self.root_markers.insert(root.clone(), sentinels);
                if let (Some(pointer), Some(identity)) = (pointer, identity) {
                    let stored = previous_key
                        .filter(|(previous, _)| *previous == pointer)
                        .map(|(_, stored)| stored);
                    self.root_pointers
                        .insert(root.clone(), (pointer, identity.merged(stored)));
                }
            } else if let Some(markers) = self.root_markers.get(root) {
                // Keep only this current consumer's own exact .git sentinels,
                // including its non-ancestor common directory. New/missing
                // roots cannot borrow markers from another or a removed root.
                targets.extend(
                    markers
                        .iter()
                        .filter(|target| matches!(target, WatchTarget::Marker(_)))
                        .cloned(),
                );
                targets.extend(markers.iter().filter_map(missing_restore));
            }
            // Restore sentinels are never installed while their file exists,
            // so ordinary atomic HEAD writes stay non-structural.
            targets.extend(
                discovered
                    .into_iter()
                    .filter(|target| !matches!(target, WatchTarget::Restore { .. })),
            );
            // Stateless: read every sync, whatever discovery found, so an
            // ancestor fallback or a negative discovery keeps it installed.
            targets.extend(pointer_sentinel(root));
        }
        for file in &self.config_dependencies {
            // Preserve logical symlink invalidation, including directory links:
            // follow_symlinks=false cannot observe retargeting via the target
            // directory alone. Watch each link's parent with an exact filter.
            for ancestor in file.ancestors() {
                if std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    if let Some(parent) = ancestor.parent() {
                        targets.insert(WatchTarget::ConfigFile {
                            file: ancestor.to_path_buf(),
                            directory: parent.to_path_buf(),
                        });
                    }
                }
            }
            // Observe missing ancestor creation one level at a time, never
            // recursively watching a home directory or working tree.
            if let Some(directory) = file
                .parent()
                .and_then(|parent| parent.ancestors().find(|path| path.is_dir()))
            {
                targets.insert(WatchTarget::ConfigFile {
                    file: file.clone(),
                    directory: directory.to_path_buf(),
                });
            }
        }
        // Backends may share one inode-bound registration across path aliases.
        // Normalize before diffing/unwatching, otherwise retiring one spelling
        // can silently remove another spelling's still-needed native watch.
        // Config leaves are NOT canonicalized: retain symlink/atomic rename
        // filters, mapping only the existing directory prefix to native paths.
        let targets: HashSet<_> = targets.into_iter().map(native_target).collect();
        // Publish filtering before watch installation so events during setup
        // are not discarded. This lock never includes filesystem/native work.
        if let Ok(mut current) = self.targets.write() {
            *current = targets.iter().cloned().collect();
        }
        let mut desired = HashMap::new();
        for target in &targets {
            let mode = target.recursive_mode();
            desired
                .entry(target.directory().to_path_buf())
                .and_modify(|current| {
                    if mode == RecursiveMode::Recursive {
                        *current = mode;
                    }
                })
                .or_insert(mode);
        }
        #[cfg(windows)]
        {
            // ReadDirectoryChangesW keeps a handle for every registration.
            // Do not hold a descendant handle while Git replaces its parent
            // `.git` directory: the recursive parent watch covers the same
            // notifications and avoids blocking a user rename/delete.
            let recursive_ancestors: Vec<_> = desired
                .iter()
                .filter(|(_, mode)| **mode == RecursiveMode::Recursive)
                .map(|(path, _)| path.clone())
                .collect();
            desired.retain(|path, _| {
                !recursive_ancestors
                    .iter()
                    .any(|ancestor| path != ancestor && path.starts_with(ancestor))
            });
        }
        self.watched.retain(|path, mode| {
            // The safety reconciliation also re-arms watches after missed
            // directory removal/replacement or an unavailable native backend.
            if !rearm && desired.get(path) == Some(mode) {
                true
            } else {
                if let Err(err) = self.watcher.unwatch(path) {
                    tracing::debug!(path = %path.display(), %err, "git directory unwatch failed");
                }
                false
            }
        });
        for (path, mode) in desired {
            if self.watched.contains_key(&path) {
                continue;
            }
            match self.watcher.watch(&path, mode) {
                Ok(()) => {
                    self.watched.insert(path, mode);
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), %err, "git directory watch unavailable; using safety refresh");
                }
            }
        }
    }
}

fn native_target(target: WatchTarget) -> WatchTarget {
    let directory = target.directory();
    let native = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    let leaf = |file: PathBuf, directory: &Path| {
        file.strip_prefix(directory)
            .map(|suffix| native.join(suffix))
            .unwrap_or(file.clone())
    };
    match target {
        WatchTarget::Metadata(_) => WatchTarget::Metadata(native),
        WatchTarget::Refs(_) => WatchTarget::Refs(native),
        WatchTarget::Marker(_) => WatchTarget::Marker(native),
        WatchTarget::ConfigFile { file, directory } => WatchTarget::ConfigFile {
            file: leaf(file, &directory),
            directory: native.clone(),
        },
        WatchTarget::Restore { file, directory } => WatchTarget::Restore {
            file: leaf(file, &directory),
            directory: native.clone(),
        },
        WatchTarget::Pointer { file, directory } => WatchTarget::Pointer {
            file: leaf(file, &directory),
            directory: native.clone(),
        },
    }
}

/// Exact sentinel for the consumer's raw git dir pointer while that path is a
/// symlink alias or missing, watching the nearest existing ancestor of its
/// parent non-recursively. A real directory needs none: its Metadata watch
/// sees removal, and a Restore sentinel sees recreation.
// ponytail: at most one per root, filtered to the exact path, as missing
// config dependencies already do.
fn pointer_sentinel(root: &Path) -> Option<WatchTarget> {
    let file = current_git_dir_pointer(root)?;
    if std::fs::symlink_metadata(&file).is_ok_and(|metadata| !metadata.file_type().is_symlink()) {
        return None;
    }
    let directory = file
        .parent()
        .and_then(|parent| parent.ancestors().find(|path| path.is_dir()))?
        .to_path_buf();
    Some(WatchTarget::Pointer { file, directory })
}

/// The consumer's raw git dir pointer, neither normalized nor canonicalized:
/// the nearest ancestor with a `.git` entry, resolved like discovery does.
fn current_git_dir_pointer(root: &Path) -> Option<PathBuf> {
    let repo_root = root
        .ancestors()
        .find(|path| std::fs::symlink_metadata(path.join(".git")).is_ok())?;
    crate::workspace::git_dir_for_repo_root(repo_root)
}

/// Previous `.git` Markers on the consumer's ancestor chain that are nearer than
/// every chain Marker of the current discovery: a nested repository's vanished
/// `.git` survives an ancestor fallback, so recreating it wakes natively. Pure
/// (no filesystem read), bounded to the chain, and retired once discovery
/// reaches that depth again.
fn carried_markers(
    previous: &[WatchTarget],
    discovered: &[WatchTarget],
    root: &Path,
    native_root: &Path,
) -> Vec<WatchTarget> {
    let on_chain = |dir: &Path| root.starts_with(dir) || native_root.starts_with(dir);
    let discovered_depth = discovered
        .iter()
        .filter_map(|target| match target {
            WatchTarget::Marker(dir) if on_chain(dir) => Some(dir.components().count()),
            _ => None,
        })
        .max();
    previous
        .iter()
        .filter(|target| {
            matches!(target, WatchTarget::Marker(dir)
                if on_chain(dir)
                    && !matches!(discovered_depth, Some(depth)
                        if dir.components().count() <= depth))
        })
        .cloned()
        .collect()
}

/// Raw `commondir` plus resolved git/common dir identities of a raw pointer.
/// `None` means unreadable or unresolvable (e.g. a dangling alias).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PointerIdentity {
    commondir: Option<String>,
    canonical_git_dir: Option<PathBuf>,
    canonical_common: Option<PathBuf>,
}

impl PointerIdentity {
    fn read(git_dir: &Path) -> Self {
        let commondir = read_commondir(git_dir);
        let common = commondir
            .as_deref()
            .map_or_else(|| git_dir.to_path_buf(), |raw| git_dir.join(raw));
        Self {
            commondir,
            canonical_git_dir: git_dir.canonicalize().ok(),
            canonical_common: common.canonicalize().ok(),
        }
    }

    /// Each currently resolvable component must equal the stored one.
    fn compatible_with(&self, stored: &Self) -> bool {
        fn same<T: PartialEq>(current: &Option<T>, stored: &Option<T>) -> bool {
            current.is_none() || current == stored
        }
        same(&self.commondir, &stored.commondir)
            && same(&self.canonical_git_dir, &stored.canonical_git_dir)
            && same(&self.canonical_common, &stored.canonical_common)
    }

    /// Keeps stored components that are currently unresolvable.
    fn merged(self, stored: Option<Self>) -> Self {
        let stored = stored.unwrap_or_default();
        Self {
            commondir: self.commondir.or(stored.commondir),
            canonical_git_dir: self.canonical_git_dir.or(stored.canonical_git_dir),
            canonical_common: self.canonical_common.or(stored.canonical_common),
        }
    }
}

/// Trimmed raw `commondir` content of a git dir, if readable.
fn read_commondir(git_dir: &Path) -> Option<String> {
    std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|content| content.trim().to_string())
}

/// Installable form of a stored Restore sentinel whose file is missing,
/// watching the nearest existing ancestor of its parent non-recursively.
// ponytail: a permanently missing repo keeps one non-recursive parent watch,
// filtered to the exact path, as missing config dependencies already do.
fn missing_restore(target: &WatchTarget) -> Option<WatchTarget> {
    let WatchTarget::Restore { file, .. } = target else {
        return None;
    };
    if std::fs::symlink_metadata(file).is_ok() {
        return None;
    }
    let directory = file
        .parent()
        .and_then(|parent| parent.ancestors().find(|path| path.is_dir()))?;
    Some(WatchTarget::Restore {
        file: file.clone(),
        directory: directory.to_path_buf(),
    })
}

fn targets_for_root(root: &Path) -> Vec<WatchTarget> {
    let Some(info) = crate::workspace::git_worktree_info(root) else {
        return Vec::new();
    };
    let marker = info.repo_root.join(".git");
    let git_dir = info.git_dir;
    let common = info.git_common_dir;
    let mut targets = vec![
        WatchTarget::Metadata(git_dir.clone()),
        WatchTarget::Metadata(common.clone()),
    ];
    if marker.exists() {
        targets.push(WatchTarget::Marker(info.repo_root));
    }
    // Restoration sentinels, captured while each HEAD exists; sync never
    // installs them directly.
    for dir in [&git_dir, &common] {
        let file = dir.join("HEAD");
        if std::fs::symlink_metadata(&file).is_ok() {
            let target = WatchTarget::Restore {
                file,
                directory: dir.clone(),
            };
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    if common.file_name().is_some_and(|name| name == ".git") {
        if let Some(parent) = common.parent() {
            targets.push(WatchTarget::Marker(parent.to_path_buf()));
        }
    }
    for dir in [git_dir, common] {
        for name in ["refs", "reftable"] {
            let path = dir.join(name);
            if path.is_dir() {
                targets.push(WatchTarget::Refs(path));
            }
        }
    }
    targets
}

#[cfg(test)]
#[path = "git_watch_tests.rs"]
mod tests;
