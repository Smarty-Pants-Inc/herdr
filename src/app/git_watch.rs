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
}

impl WatchTarget {
    fn directory(&self) -> &Path {
        match self {
            Self::Metadata(path) | Self::Refs(path) | Self::Marker(path) => path,
            Self::ConfigFile { directory, .. } => directory,
        }
    }

    fn recursive_mode(&self) -> RecursiveMode {
        match self {
            Self::Refs(_) => RecursiveMode::Recursive,
            _ => RecursiveMode::NonRecursive,
        }
    }

    fn matches(&self, path: &Path) -> bool {
        let directory = self.directory();
        if let Self::ConfigFile { file, .. } = self {
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
            Self::ConfigFile { .. } => false, // handled above
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
    event.need_rescan()
        || (matches!(
            event.kind,
            EventKind::Create(CreateKind::Folder)
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
        let previous_markers: Vec<_> = self
            .targets
            .read()
            .map(|targets| {
                targets
                    .iter()
                    .filter(|target| matches!(target, WatchTarget::Marker(_)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let mut targets = HashSet::new();
        for root in &self.roots {
            let discovered = targets_for_root(root);
            if discovered.is_empty() {
                let native_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                // Keep an already-discovered checkout's exact .git sentinel
                // across a removal gap. Initially non-Git roots still rely on
                // the independent discovery safety net.
                targets.extend(
                    previous_markers
                        .iter()
                        .filter(|target| native_root.starts_with(target.directory()))
                        .cloned(),
                );
            }
            targets.extend(discovered);
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
    match target {
        WatchTarget::Metadata(_) => WatchTarget::Metadata(native),
        WatchTarget::Refs(_) => WatchTarget::Refs(native),
        WatchTarget::Marker(_) => WatchTarget::Marker(native),
        WatchTarget::ConfigFile { file, directory } => {
            let file = file
                .strip_prefix(&directory)
                .map(|suffix| native.join(suffix))
                .unwrap_or(file.clone());
            WatchTarget::ConfigFile {
                file,
                directory: native,
            }
        }
    }
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
