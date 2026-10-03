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
}

impl WatchTarget {
    fn directory(&self) -> &Path {
        match self {
            Self::Metadata(path) | Self::Refs(path) | Self::Marker(path) => path,
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
            Self::Marker(_) => path == directory.join(".git"),
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

pub(super) struct GitWatches {
    watcher: RecommendedWatcher,
    roots: HashSet<PathBuf>,
    watched: HashMap<PathBuf, RecursiveMode>,
    targets: Arc<RwLock<Vec<WatchTarget>>>,
    wakeup_pending: Arc<AtomicBool>,
    pub(super) topology_dirty: bool,
    pub(super) rearm_requested: bool,
}

impl GitWatches {
    pub(super) fn new(event_tx: mpsc::Sender<AppEvent>) -> notify::Result<Self> {
        let targets = Arc::new(RwLock::new(Vec::<WatchTarget>::new()));
        let callback_targets = targets.clone();
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let callback_pending = wakeup_pending.clone();
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let relevant = match result {
                    Ok(event) => match callback_targets.try_read() {
                        Ok(targets) => relevant_event(&event, &targets),
                        // Never block the native callback on app reconciliation.
                        Err(_) => !matches!(event.kind, EventKind::Access(_)),
                    },
                    Err(_) => true, // Overflow/backend errors request a reconciliation too.
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
            topology_dirty: false,
            rearm_requested: false,
        })
    }

    pub(super) fn acknowledge(&self) {
        self.wakeup_pending.store(false, Ordering::Release);
    }

    pub(super) fn sync(&mut self, roots: HashSet<PathBuf>) {
        if self.roots == roots && !self.topology_dirty && !self.rearm_requested {
            return;
        }
        let rearm = std::mem::take(&mut self.rearm_requested);
        self.roots = roots;
        self.topology_dirty = false;
        let mut targets = HashSet::new();
        for root in &self.roots {
            targets.extend(targets_for_root(root));
        }
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

fn targets_for_root(root: &Path) -> Vec<WatchTarget> {
    let Some(space) = crate::workspace::git_space_metadata(root) else {
        return Vec::new();
    };
    let marker = space.repo_root.join(".git");
    let git_dir = if marker.is_dir() {
        marker.clone()
    } else if marker.is_file() {
        let Ok(contents) = std::fs::read_to_string(&marker) else {
            return Vec::new();
        };
        let Some(path) = contents.trim().strip_prefix("gitdir:").map(str::trim) else {
            return Vec::new();
        };
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            space.repo_root.join(path)
        }
    } else {
        // git_space_metadata also supports bare repository roots.
        space.repo_root.clone()
    };
    let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let common = PathBuf::from(space.key);
    let mut targets = vec![
        WatchTarget::Metadata(git_dir.clone()),
        WatchTarget::Metadata(common.clone()),
    ];
    if marker.is_file() {
        targets.push(WatchTarget::Marker(space.repo_root));
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
