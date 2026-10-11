use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tracing::warn;

use super::{
    git_watch::GitWatches, App, GIT_REMOTE_STATUS_REFRESH_INTERVAL,
    GIT_REPO_DISCOVERY_REFRESH_INTERVAL, GIT_WATCH_DEBOUNCE,
};
use crate::events::AppEvent;
use crate::workspace::{GitStatusCacheEntry, GitStatusRefreshDemand, WorkspaceGitStatus};

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshItem {
    workspace_id: String,
    resolved_identity_cwd: PathBuf,
    cache_key_hint: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshTarget {
    workspace_id: String,
    resolved_identity_cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshJob {
    cache_key: PathBuf,
    cached: Option<GitStatusCacheEntry>,
    targets: Vec<WorkspaceGitRefreshTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshOutput {
    results: Vec<WorkspaceGitStatus>,
    cache_updates: Vec<(PathBuf, GitStatusCacheEntry)>,
}

impl App {
    /// Reconcile native watches outside rendering. Main's headless loop calls
    /// this after workspace/client changes; unchanged roots do no filesystem I/O.
    pub(crate) fn sync_git_watches(&mut self) {
        let demand = self.git_refresh_demand();
        let roots: HashSet<_> = if demand.is_empty() {
            HashSet::new()
        } else {
            self.workspace_git_refresh_items(false)
                .into_iter()
                // Consumer identity must not change when discovery supplies a
                // canonical cache key for the same logical workspace CWD.
                .map(|item| item.resolved_identity_cwd)
                .collect()
        };
        if roots.is_empty() {
            self.clear_git_watches();
            return;
        }
        self.git_watch_demand = self.request_git_demand_growth(self.git_watch_demand);
        if self
            .git_watches
            .as_ref()
            .is_some_and(|watches| watches.roots_changed(&roots))
        {
            // Config dependencies belong to current consumers, not retired cache entries.
            self.reconcile_git_config_watches();
        }
        if self.git_watches.is_none() {
            match GitWatches::new(self.event_tx.clone()) {
                Ok(mut watches) => {
                    let cache_roots = self
                        .workspace_git_refresh_items(false)
                        .into_iter()
                        .map(|item| item.cache_key_hint.unwrap_or(item.resolved_identity_cwd))
                        .collect();
                    watches.set_config_dependencies(self.git_config_dependency_paths(&cache_roots));
                    self.git_watches = Some(watches);
                }
                Err(err) => {
                    tracing::warn!(%err, "native git watches unavailable; using safety refresh");
                    return;
                }
            }
        }
        if self
            .git_watches
            .as_ref()
            .is_some_and(|watches| watches.has_new_roots(&roots))
        {
            self.mark_git_status_refresh_due(Instant::now());
        }
        if let Some(watches) = &mut self.git_watches {
            watches.sync(roots);
        }
    }

    /// A clientless headless server can release all native watcher resources.
    pub(crate) fn clear_git_watches(&mut self) {
        self.git_watches = None;
        self.git_watch_demand = GitStatusRefreshDemand::default();
        self.git_watch_refresh_deadline = None;
    }

    pub(super) fn handle_git_files_changed(&mut self, now: Instant) {
        if self.git_refresh_demand().is_empty() {
            self.clear_git_watches();
            return;
        }
        if self
            .git_watches
            .as_ref()
            .is_some_and(GitWatches::take_discovery_hint)
        {
            // Directory/marker changes can invalidate a negative cache or a
            // cached checkout identity. Ordinary HEAD/index writes do neither.
            self.request_git_identity_refresh(now);
        }
        // Bound bursts to one wakeup and one refresh per debounce window, even
        // if a writer never goes quiet. In-flight refreshes keep this deadline.
        self.git_watch_refresh_deadline
            .get_or_insert(now + GIT_WATCH_DEBOUNCE);
    }

    pub(crate) fn refresh_restored_workspace_git_metadata(&mut self) {
        if self.state.workspaces.is_empty() {
            return;
        }
        self.pending_restored_worktree_spaces = self
            .state
            .workspaces
            .iter()
            .filter_map(|workspace| {
                workspace
                    .worktree_space
                    .clone()
                    .map(|space| (workspace.id.clone(), space))
            })
            .collect();
        let now = Instant::now();
        self.start_restored_worktree_validation(now);
        self.request_git_identity_refresh(now);
        // Start once even without an attached TUI or sidebar Git tokens. Reuse
        // the single detached worker so stalled I/O cannot block server shutdown.
        self.start_git_status_refresh_if_due(now);
    }

    pub(crate) fn start_restored_worktree_validation(&mut self, now: Instant) {
        self.restored_worktree_validation_retry_at = None;
        let jobs = Arc::new(self.pending_restored_worktree_spaces.clone());
        let next = Arc::new(AtomicUsize::new(0));
        let worker_count = jobs.len().min(4);
        let mut started = 0;
        // These one-shot checks are independent of the optional Git cache. A
        // stalled checkout keeps its slot; never retry it on each refresh tick.
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs);
            let next = Arc::clone(&next);
            let event_tx = self.event_tx.clone();
            let spawned = crate::thread_spawn::spawn_named("herdr-worktree-check", move || {
                while let Some((workspace_id, expected)) =
                    jobs.get(next.fetch_add(1, Ordering::Relaxed))
                {
                    let valid =
                        crate::persist::restored_worktree_space_membership(Some(expected.clone()))
                            .is_some();
                    if event_tx
                        .blocking_send(AppEvent::RestoredWorktreeSpaceChecked {
                            workspace_id: workspace_id.clone(),
                            expected: expected.clone(),
                            valid,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            match spawned {
                Ok(_) => started += 1,
                Err(err) => {
                    warn!(err = %err, "failed to spawn restored worktree check thread");
                }
            }
        }
        // Any started worker drains the shared queue. With none, memberships
        // stay unvalidated (worktree actions keep waiting) and we retry later.
        if worker_count > 0 && started == 0 {
            self.restored_worktree_validation_retry_at =
                Some(now + GIT_REMOTE_STATUS_REFRESH_INTERVAL);
        }
    }

    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let safety_discovery_due = now
            .saturating_duration_since(self.last_git_repo_discovery_refresh)
            >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        let refresh_repo_discovery = self.git_identity_refresh_requested || safety_discovery_due;
        let workspaces = self.workspace_git_refresh_items(refresh_repo_discovery);
        self.git_watch_refresh_deadline = None;
        if let Some(watches) = &mut self.git_watches {
            watches.acknowledge();
            if safety_discovery_due {
                watches.rearm_requested = true;
            }
        }
        if workspaces.is_empty() {
            self.last_git_remote_status_refresh = now;
            if safety_discovery_due {
                self.last_git_repo_discovery_refresh = now;
            }
            self.git_identity_refresh_requested = false;
            self.git_refresh_spawn_retry_pending = false;
            return;
        }

        let event_tx = self.event_tx.clone();
        let cache = self.git_status_cache.clone();
        let mut demand = self.git_refresh_demand();
        if self.git_identity_refresh_requested {
            demand.branch = true;
        }
        #[cfg(test)]
        let git_test_homes =
            ["HOME", "XDG_CONFIG_HOME"].map(|key| (key, crate::environment::var_os(key)));
        let generation = self.git_refresh_generation + 1;
        let spawned = crate::thread_spawn::spawn_named("herdr-git-refresh", move || {
            // Test scopes do not implicitly cross threads. Pass only this
            // worker's Git fixture homes explicitly; production is unchanged.
            #[cfg(test)]
            let _git_env = {
                let env = crate::environment::test_env();
                for (key, value) in git_test_homes {
                    if let Some(value) = value {
                        env.set(key, value);
                    } else {
                        env.remove(key);
                    }
                }
                env
            };
            let output =
                refresh_workspace_git_statuses_with_cache_and_demand(workspaces, &cache, demand);
            let _ = event_tx.blocking_send(AppEvent::GitStatusRefreshed {
                generation,
                results: output.results,
                cache_updates: output.cache_updates,
            });
        });
        if let Err(err) = spawned {
            // Keep the pending requests and retry after the normal interval.
            warn!(err = %err, "failed to spawn git refresh thread; retrying later");
            self.last_git_remote_status_refresh = now;
            self.git_refresh_spawn_retry_pending = true;
            return;
        }
        self.git_refresh_spawn_retry_pending = false;
        self.git_refresh_generation = generation;
        self.git_refresh_in_flight = true;
        self.git_identity_refresh_requested = false;
        // Explicit/native identity hints must not move the independent safety
        // net; continuous metadata replacement cannot postpone reconciliation.
        // A failed spawn must not count as a completed safety discovery either.
        if safety_discovery_due {
            self.last_git_repo_discovery_refresh = now;
        }
    }

    pub(crate) fn request_git_identity_refresh(&mut self, now: Instant) {
        self.git_identity_refresh_requested = true;
        // Only a worker started after this request observes the new identity;
        // an in-flight worker's result (e.g. a missing-HEAD negative) is stale.
        self.git_identity_refresh_floor = self.git_refresh_generation + 1;
        self.mark_git_status_refresh_due(now);
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        self.git_status_cache
            .retain(|_, entry| entry.fingerprint.is_some());
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        // New identity/demand hints remain pending but cannot turn a failed
        // worker's bounded retry into a tight loop under continuing events.
        if self.git_refresh_spawn_retry_pending {
            return;
        }
        self.last_git_remote_status_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        (!self.git_refresh_in_flight
            && !self.state.workspaces.is_empty()
            && (self.git_identity_refresh_requested || !self.git_refresh_demand().is_empty()))
        .then(|| {
            // A failed worker has a bounded retry even if discovery is overdue
            // or a native watcher has supplied another debounce deadline.
            if self.git_refresh_spawn_retry_pending {
                return self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL;
            }
            let safety = (self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
                .min(self.last_git_repo_discovery_refresh + GIT_REPO_DISCOVERY_REFRESH_INTERVAL);
            self.git_watch_refresh_deadline
                .map_or(safety, |deadline| safety.min(deadline))
        })
    }

    pub(super) fn request_git_demand_growth(
        &mut self,
        previous: GitStatusRefreshDemand,
    ) -> GitStatusRefreshDemand {
        let demand = self.git_refresh_demand();
        if (demand.branch && !previous.branch) || (demand.ahead_behind && !previous.ahead_behind) {
            self.mark_git_status_refresh_due(Instant::now());
        }
        demand
    }

    fn git_config_dependency_paths(&self, roots: &HashSet<PathBuf>) -> HashSet<PathBuf> {
        roots
            .iter()
            .filter_map(|root| self.git_status_cache.get(root))
            .flat_map(GitStatusCacheEntry::config_dependency_paths)
            .collect()
    }

    pub(super) fn reconcile_git_config_watches(&mut self) {
        // Only worker completion reads cached config dependencies. The regular
        // unchanged-root app-loop sync never discovers files or clones deps.
        let roots = self
            .workspace_git_refresh_items(false)
            .into_iter()
            .map(|item| item.cache_key_hint.unwrap_or(item.resolved_identity_cwd))
            .collect();
        let paths = self.git_config_dependency_paths(&roots);
        if let Some(watches) = &mut self.git_watches {
            watches.set_config_dependencies(paths);
        }
    }

    pub(super) fn git_refresh_demand(&self) -> GitStatusRefreshDemand {
        let mut demand = GitStatusRefreshDemand::default();
        for token in self.state.sidebar_spaces.rows.iter().flatten() {
            match token.parts().0 {
                crate::config::SpaceSidebarToken::Branch => demand.branch = true,
                crate::config::SpaceSidebarToken::GitStatus => demand.ahead_behind = true,
                _ => {}
            }
        }
        demand
    }

    fn workspace_git_refresh_items(
        &self,
        refresh_repo_discovery: bool,
    ) -> Vec<WorkspaceGitRefreshItem> {
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let cache_key_hint = (!refresh_repo_discovery && ws.cached_identity_cwd == cwd)
                    .then(|| ws.cached_git_status_key.clone());
                Some(WorkspaceGitRefreshItem {
                    workspace_id: ws.id.clone(),
                    resolved_identity_cwd: cwd,
                    cache_key_hint,
                })
            })
            .collect()
    }
}

fn deduplicate_git_refresh_items(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> Vec<WorkspaceGitRefreshJob> {
    let mut indexes = HashMap::<PathBuf, usize>::new();
    let mut jobs = Vec::<WorkspaceGitRefreshJob>::new();

    for item in items {
        let reconcile = item.cache_key_hint.is_none();
        let cache_key = item.cache_key_hint.unwrap_or_else(|| {
            crate::workspace::git_status_cache_key(&item.resolved_identity_cwd)
                .unwrap_or_else(|| item.resolved_identity_cwd.clone())
        });
        let target = WorkspaceGitRefreshTarget {
            workspace_id: item.workspace_id,
            resolved_identity_cwd: item.resolved_identity_cwd,
        };
        if let Some(&index) = indexes.get(&cache_key) {
            jobs[index].cached = jobs[index].cached.take().filter(|_| !reconcile);
            jobs[index].targets.push(target);
            continue;
        }

        let cached = cache.get(&cache_key).filter(|_| !reconcile).cloned();
        indexes.insert(cache_key.clone(), jobs.len());
        jobs.push(WorkspaceGitRefreshJob {
            cache_key,
            cached,
            targets: vec![target],
        });
    }

    jobs
}

fn refresh_workspace_git_statuses_with_cache_and_demand(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
    demand: GitStatusRefreshDemand,
) -> WorkspaceGitRefreshOutput {
    let mut results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &job.cache_key,
            job.cached.as_ref(),
            demand,
        );
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        results.extend(job.targets.into_iter().map(move |target| {
            snapshot.clone().into_workspace_status(
                target.workspace_id,
                target.resolved_identity_cwd,
                job.cache_key.clone(),
                demand,
            )
        }));
    }

    WorkspaceGitRefreshOutput {
        results,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-dedupe-{}", std::process::id()));
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");

        let output = refresh_workspace_git_statuses_with_cache_and_demand(
            vec![
                WorkspaceGitRefreshItem {
                    workspace_id: "one".into(),
                    resolved_identity_cwd: nested.clone(),
                    cache_key_hint: None,
                },
                WorkspaceGitRefreshItem {
                    workspace_id: "two".into(),
                    resolved_identity_cwd: other.clone(),
                    cache_key_hint: None,
                },
            ],
            &HashMap::new(),
            GitStatusRefreshDemand::ALL,
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(
            output.cache_updates[0].0,
            std::fs::canonicalize(&repo).expect("canonical repo path")
        );
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].workspace_id, "one");
        assert_eq!(output.results[0].resolved_identity_cwd, nested);
        assert_eq!(output.results[1].workspace_id, "two");
        assert_eq!(output.results[1].resolved_identity_cwd, other);

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn shared_root_repo_refresh_keeps_workspace_specific_fallback_labels() {
        let cache_key = PathBuf::from("/");
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: Some(Instant::now() + std::time::Duration::from_secs(30)),
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "/".into(),
                branch: Some("main".into()),
                ahead_behind: None,
                space: Some(crate::workspace::GitSpaceMetadata {
                    key: "/.git".into(),
                    checkout_key: "/".into(),
                    repo_name: "repo".into(),
                    repo_root: cache_key.clone(),
                    is_linked_worktree: false,
                }),
            },
        };
        let items = ["alpha", "beta"]
            .into_iter()
            .map(|name| WorkspaceGitRefreshItem {
                workspace_id: name.into(),
                resolved_identity_cwd: cache_key.join(name),
                cache_key_hint: Some(cache_key.clone()),
            })
            .collect();

        let output = refresh_workspace_git_statuses_with_cache_and_demand(
            items,
            &HashMap::from([(cache_key, cached)]),
            GitStatusRefreshDemand::ALL,
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].auto_label, "alpha");
        assert_eq!(output.results[1].auto_label, "beta");
        assert_eq!(output.results[0].branch.as_deref(), Some("main"));
        assert_eq!(output.results[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_refresh_item_collection_does_not_discover_uncached_cwd() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-uncached-cwd-{}", std::process::id()));
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].resolved_identity_cwd, cwd);
        assert_eq!(items[0].cache_key_hint, None);
    }

    #[test]
    fn git_refresh_item_collection_reuses_matching_cached_key() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let cache_key = PathBuf::from("/repo");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = cache_key.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, Some(cache_key));
    }

    #[test]
    fn periodic_repo_discovery_ignores_cached_key_hints() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = PathBuf::from("/repo");
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(true);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, None);
        let cache_key = items[0].resolved_identity_cwd.clone();
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: crate::workspace::WorkspaceGitStatusSnapshot {
                auto_label: "stale".into(),
                branch: None,
                ahead_behind: None,
                space: None,
            },
        };
        let jobs = deduplicate_git_refresh_items(items, &HashMap::from([(cache_key, cached)]));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cached, None);
    }

    #[test]
    fn failed_git_refresh_spawn_retries_after_interval() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.request_git_identity_refresh(now);

        crate::thread_spawn::test_hook::fail_next_spawns(1);
        app.start_git_status_refresh_if_due(now);

        assert!(!app.git_refresh_in_flight);
        assert!(app.git_identity_refresh_requested);
        assert_eq!(
            app.git_refresh_deadline(),
            Some(now + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
        );

        let retry_at = now + GIT_REMOTE_STATUS_REFRESH_INTERVAL;
        app.start_git_status_refresh_if_due(retry_at);
        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
    }

    #[test]
    fn identity_refresh_keeps_independent_safety_discovery_deadline() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut workspace = Workspace::test_new("identity-refresh");
        workspace.tabs.clear();
        workspace.identity_cwd = std::env::temp_dir();
        app.state.workspaces.push(workspace);
        let safety_start = Instant::now();
        app.last_git_repo_discovery_refresh = safety_start;
        let now = safety_start + std::time::Duration::from_secs(30);

        app.request_git_identity_refresh(now);
        app.start_git_status_refresh_if_due(now);

        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
        assert_eq!(app.last_git_repo_discovery_refresh, safety_start);
    }

    #[test]
    fn failed_safety_refresh_preserves_discovery_and_bounds_native_retry() {
        let mut app = test_app(&crate::config::Config::default());
        let mut workspace = Workspace::test_new("failed-safety-refresh");
        workspace.tabs.clear();
        workspace.identity_cwd = std::env::temp_dir();
        app.state.workspaces.push(workspace);
        let now = Instant::now();
        let safety_start = now - GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        app.last_git_repo_discovery_refresh = safety_start;
        app.request_git_identity_refresh(now);

        crate::thread_spawn::test_hook::fail_next_spawns(1);
        app.start_git_status_refresh_if_due(now);

        assert!(!app.git_refresh_in_flight);
        assert!(app.git_identity_refresh_requested);
        assert!(app.git_refresh_spawn_retry_pending);
        assert_eq!(app.last_git_repo_discovery_refresh, safety_start);
        let retry_at = now + GIT_REMOTE_STATUS_REFRESH_INTERVAL;
        app.handle_git_files_changed(now);
        assert_eq!(
            app.git_watch_refresh_deadline,
            Some(now + GIT_WATCH_DEBOUNCE)
        );
        app.request_git_identity_refresh(now + GIT_WATCH_DEBOUNCE);
        assert_eq!(app.git_refresh_deadline(), Some(retry_at));
        app.start_git_status_refresh_if_due(now + GIT_WATCH_DEBOUNCE);
        assert!(!app.git_refresh_in_flight);

        app.start_git_status_refresh_if_due(retry_at);
        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
        assert!(!app.git_refresh_spawn_retry_pending);
        assert_eq!(app.last_git_repo_discovery_refresh, retry_at);
    }

    #[test]
    fn failed_restored_worktree_check_spawns_keep_spaces_pending_and_retry() {
        let mut app = test_app(&crate::config::Config::default());
        let mut workspace = Workspace::test_new("restored");
        let membership = crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/nonexistent/repo".into(),
            checkout_path: "/nonexistent/repo".into(),
            is_linked_worktree: false,
        };
        workspace.worktree_space = Some(membership.clone());
        app.pending_restored_worktree_spaces
            .push((workspace.id.clone(), membership.clone()));
        app.state.workspaces.push(workspace);

        let now = Instant::now();
        crate::thread_spawn::test_hook::fail_next_spawns(1);
        app.start_restored_worktree_validation(now);

        assert_eq!(app.pending_restored_worktree_spaces.len(), 1);
        let retry_at = now + GIT_REMOTE_STATUS_REFRESH_INTERVAL;
        assert_eq!(app.restored_worktree_validation_retry_at, Some(retry_at));
        assert!(app
            .next_headless_loop_deadline_with_git_refresh(now, false, false)
            .is_some_and(|deadline| deadline <= retry_at));

        app.start_restored_worktree_validation(retry_at);
        assert_eq!(app.restored_worktree_validation_retry_at, None);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let event = loop {
            match app.event_rx.try_recv() {
                Ok(event @ AppEvent::RestoredWorktreeSpaceChecked { .. }) => break event,
                Ok(_) => {}
                Err(_) => {
                    assert!(Instant::now() < deadline, "validation retry never reported");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        };
        app.handle_internal_event(event);
        assert!(app.pending_restored_worktree_spaces.is_empty());
        assert_eq!(app.state.workspaces[0].worktree_space, Some(membership));
    }

    #[test]
    fn cwd_identity_refresh_runs_once_without_sidebar_git_tokens() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();

        app.request_git_identity_refresh(now);

        assert!(app.git_refresh_deadline().is_some());
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh_in_flight);
        assert!(!app.git_identity_refresh_requested);
    }

    #[tokio::test]
    async fn restored_workspace_metadata_refresh_runs_once_without_sidebar_git_tokens() {
        let repo =
            std::env::temp_dir().join(format!("herdr-restored-git-refresh-{}", std::process::id()));
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut workspace = Workspace::test_new("restored");
        workspace.identity_cwd = nested.clone();
        workspace.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "replaced-repo".into(),
            label: "old".into(),
            repo_root: repo.clone(),
            checkout_path: repo.clone(),
            is_linked_worktree: false,
        });
        app.state.workspaces.push(workspace);
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        for terminal in app.state.terminals.values_mut() {
            terminal.cwd = nested.clone();
        }
        app.state.assert_invariants_for_test();

        app.refresh_restored_workspace_git_metadata();
        assert!(app.git_refresh_in_flight);
        assert_eq!(app.pending_restored_worktree_spaces.len(), 1);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while app.git_refresh_in_flight || !app.pending_restored_worktree_spaces.is_empty() {
                let event = app.event_rx.recv().await.expect("Git worker completion");
                app.handle_internal_event(event);
            }
        })
        .await
        .expect("restored Git metadata refresh completes");

        let workspace = &app.state.workspaces[0];
        assert_eq!(workspace.cached_git_branch.as_deref(), Some("main"));
        assert_eq!(
            workspace.cached_auto_label,
            repo.file_name().unwrap().to_string_lossy()
        );
        assert!(workspace.cached_git_space.is_some());
        assert!(workspace.worktree_space.is_none());
        assert!(app.pending_restored_worktree_spaces.is_empty());
        assert!(app.state.session_dirty);
        assert_eq!(app.git_refresh_deadline(), None);
        app.start_git_status_refresh_if_due(Instant::now());
        assert!(!app.git_refresh_in_flight);
        app.state.assert_invariants_for_test();
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn due_git_refresh_does_not_start_without_sidebar_consumer() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        app.start_git_status_refresh_if_due(now);

        assert!(!app.git_refresh_in_flight);
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn restored_worktree_validation_notifies_only_matching_rejections() {
        let mut app = test_app(&crate::config::Config::default());
        app.state = crate::app::AppState::test_with_adversarial_identity_state();
        let workspace_id = app.state.workspaces[0].id.clone();
        let expected = crate::workspace::WorktreeSpaceMembership {
            key: "saved".into(),
            label: "saved".into(),
            repo_root: "/repo".into(),
            checkout_path: "/checkout".into(),
            is_linked_worktree: true,
        };
        app.state.workspaces[0].worktree_space = Some(expected.clone());
        app.pending_restored_worktree_spaces
            .push((workspace_id.clone(), expected.clone()));
        app.handle_internal_event(AppEvent::RestoredWorktreeSpaceChecked {
            workspace_id: workspace_id.clone(),
            expected: expected.clone(),
            valid: true,
        });
        assert!(app.pending_restored_worktree_spaces.is_empty());
        assert!(app.event_hub.events_after(0).is_empty());
        assert_eq!(
            app.state.workspaces[0].worktree_space,
            Some(expected.clone())
        );

        let stale = crate::workspace::WorktreeSpaceMembership {
            key: "stale".into(),
            ..expected.clone()
        };
        app.handle_internal_event(AppEvent::RestoredWorktreeSpaceChecked {
            workspace_id: workspace_id.clone(),
            expected: stale,
            valid: false,
        });
        assert!(app.event_hub.events_after(0).is_empty());
        app.handle_internal_event(AppEvent::RestoredWorktreeSpaceChecked {
            workspace_id,
            expected,
            valid: false,
        });
        let events = app.event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].1.event,
            crate::api::schema::EventKind::WorkspaceUpdated
        );
        assert!(app.state.workspaces[0].worktree_space.is_none());
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn git_refresh_demand_matches_sidebar_rows() {
        let cases = [
            (
                crate::config::SpaceSidebarToken::Workspace,
                GitStatusRefreshDemand::default(),
            ),
            (
                crate::config::SpaceSidebarToken::Branch,
                GitStatusRefreshDemand {
                    branch: true,
                    ahead_behind: false,
                },
            ),
            (
                crate::config::SpaceSidebarToken::GitStatus,
                GitStatusRefreshDemand {
                    branch: false,
                    ahead_behind: true,
                },
            ),
        ];

        for (token, expected) in cases {
            let mut config = crate::config::Config::default();
            config.ui.sidebar.spaces.rows = vec![vec![token.clone()]];
            let mut app = test_app(&config);
            app.state.workspaces.push(Workspace::test_new("test"));

            assert_eq!(app.git_refresh_demand(), expected, "token: {token:?}");
            assert_eq!(
                app.git_refresh_deadline().is_some(),
                !expected.is_empty(),
                "token: {token:?}"
            );
        }
    }

    #[test]
    fn unnamed_linked_worktree_does_not_force_periodic_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("test");
        child.custom_name = None;
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn custom_named_linked_worktree_does_not_require_branch_refresh() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut child = Workspace::test_new("custom");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: "/repo-worktree".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces.push(child);

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = test_app(&crate::config::Config::default());
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - GIT_REMOTE_STATUS_REFRESH_INTERVAL;

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            None
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn explicit_git_refresh_invalidates_cached_non_git_results() {
        let mut app = test_app(&crate::config::Config::default());
        let cwd = std::env::temp_dir().join(format!("herdr-git-miss-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let (_, entry) = crate::workspace::git_status_snapshot_for_cwd_with_demand(
            &cwd,
            None,
            GitStatusRefreshDemand::ALL,
        );
        app.git_status_cache
            .insert(cwd.clone(), entry.expect("non-Git cache entry"));

        app.mark_git_status_refresh_due(Instant::now());

        assert!(app.git_status_cache.is_empty());
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = test_app(&crate::config::Config::default());
        let now = Instant::now();
        app.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh_due_after_in_flight);

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            generation: 0,
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh_in_flight);
        assert!(!app.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    struct GitWatchRepo(PathBuf);

    impl GitWatchRepo {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-git-watch-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn git(&self, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args([
                    "-c",
                    "user.name=Git Watch Test",
                    "-c",
                    "user.email=git-watch@example.invalid",
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

        fn init(&self) {
            self.git(&["init", "-b", "main"]);
            self.git(&["commit", "--allow-empty", "-m", "initial"]);
            self.git(&["branch", "upstream"]);
            self.git(&["branch", "--set-upstream-to=upstream", "main"]);
        }

        fn app(&self) -> App {
            let mut config = crate::config::Config::default();
            config.ui.sidebar.spaces.rows = vec![vec![
                crate::config::SpaceSidebarToken::Branch,
                crate::config::SpaceSidebarToken::GitStatus,
            ]];
            let mut app = test_app(&config);
            let mut ws = Workspace::test_new("watched");
            ws.tabs.clear();
            ws.identity_cwd = self.0.clone();
            app.state.workspaces.push(ws);
            app.mark_git_status_refresh_due(Instant::now());
            // Fixture setup is a forced refresh, not a native-event refresh.
            drive_git_watch_refresh_within(&mut app, GIT_WATCH_HANG_WATCHDOG);
            app
        }
    }

    impl Drop for GitWatchRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Functional waits (including fixture setup) have a hang watchdog, not a
    // product latency assertion. Ignored perf tests measure the separate target.
    const GIT_WATCH_HANG_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);
    const GIT_WATCH_LATENCY_TARGET: std::time::Duration = std::time::Duration::from_secs(1);

    fn run_git_watch_test_in_child(name: &str) -> bool {
        const CHILD_ENV: &str = "HERDR_TEST_GIT_WATCH_CHILD";
        if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
            return false;
        }
        // Serializing just these fixtures avoids overlapping native watchers;
        // re-exec also excludes unrelated PTY forks and process-wide fixtures.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            // Exact selection still runs one test when the parent is an ignored
            // perf test; without this flag its child would execute zero tests.
            .args([
                "--exact",
                name,
                "--include-ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD_ENV, name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run isolated git watch test");
        let start = Instant::now();
        let timed_out = loop {
            if child
                .try_wait()
                .expect("observe isolated git watch test")
                .is_some()
            {
                break false;
            }
            if start.elapsed() >= std::time::Duration::from_secs(30) {
                child.kill().expect("stop isolated git watch test");
                break true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let output = child
            .wait_with_output()
            .expect("join isolated git watch test");
        assert!(
            !timed_out
                && output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "isolated {name}: timed_out={timed_out}, status={}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    macro_rules! isolated_git_watch_test {
        ($name:ident) => {
            if run_git_watch_test_in_child(concat!("app::git_refresh::tests::", stringify!($name)))
            {
                return;
            }
        };
    }

    /// Exercise the same sync -> scheduler -> worker -> App event application
    /// path used by a headless server with an attached app consumer.
    /// Wait up to the 10s hang watchdog for an applied GitStatusRefreshed event.
    /// The returned duration starts after the fixture's git command returns;
    /// ignored perf tests assert the separate 1s native-refresh latency target.
    #[track_caller]
    fn drive_git_watch_refresh(app: &mut App) -> (std::time::Duration, bool) {
        drive_git_watch_refresh_within(app, GIT_WATCH_HANG_WATCHDOG)
    }

    /// Use one absolute watchdog deadline, independent of performance targets.
    #[track_caller]
    fn drive_git_watch_refresh_within(
        app: &mut App,
        budget: std::time::Duration,
    ) -> (std::time::Duration, bool) {
        let start = Instant::now();
        let deadline = start + budget;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // A safety refresh must never satisfy a native-event regression, even
        // when a long-running test is close to its original safety deadline.
        let safety_refresh = app.last_git_repo_discovery_refresh;
        loop {
            let now = Instant::now();
            assert!(
                now < deadline,
                "native git refresh exceeded {budget:?}: in_flight={}, watch_deadline={:?}",
                app.git_refresh_in_flight,
                app.git_watch_refresh_deadline
            );
            app.sync_git_watches();
            app.start_git_status_refresh_if_due(now);
            // Block on the next App event, waking for the debounce deadline
            // or the hang watchdog instead of polling on a fixed sleep.
            let wake = app
                .git_refresh_deadline()
                .map_or(deadline, |due| due.min(deadline));
            let wait = wake.saturating_duration_since(Instant::now());
            let Ok(Some(event)) =
                runtime.block_on(async { tokio::time::timeout(wait, app.event_rx.recv()).await })
            else {
                continue;
            };
            let completed = matches!(event, AppEvent::GitStatusRefreshed { .. });
            let changed = app.handle_internal_event_with_render_impact(event);
            if completed {
                assert!(
                    start.elapsed() < budget,
                    "native git refresh application exceeded {budget:?}"
                );
                assert_eq!(
                    app.last_git_repo_discovery_refresh, safety_refresh,
                    "periodic safety discovery must not satisfy a native refresh wait"
                );
                return (start.elapsed(), changed);
            }
        }
    }

    #[test]
    fn git_watch_refresh_wait_accepts_completion_after_one_second() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        assert!(app.git_refresh_demand().is_empty());
        assert!(app.state.workspaces.is_empty());
        // No consumers or workspaces: only this synthetic event can complete
        // the wait. This probes helper policy, not native refresh performance.
        let sender = app.event_tx.clone();
        let delay = std::time::Duration::from_millis(1200);
        let start = Instant::now();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(delay);
            sender
                .blocking_send(AppEvent::GitStatusRefreshed {
                    generation: 0,
                    results: Vec::new(),
                    cache_updates: Vec::new(),
                })
                .expect("send delayed git completion");
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drive_git_watch_refresh(&mut app)
        }));
        // Join even if the helper regresses to the 1s wait and panics.
        thread.join().expect("join delayed git completion sender");
        let (_, changed) = result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert!(start.elapsed() >= delay);
        assert!(!changed);
    }

    #[test]
    #[should_panic(expected = "native git refresh exceeded 50ms")]
    fn git_watch_refresh_wait_missing_completion_exceeds_explicit_budget() {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        assert!(app.git_refresh_demand().is_empty());
        assert!(app.state.workspaces.is_empty());
        // Keep the channel open, but supply no event and start no workers.
        drive_git_watch_refresh_within(&mut app, std::time::Duration::from_millis(50));
    }

    #[test]
    fn git_watch_commit_branch_and_atomic_index_refresh_through_app() {
        isolated_git_watch_test!(git_watch_commit_branch_and_atomic_index_refresh_through_app);
        git_watch_commit_branch_and_atomic_index_refresh_scenario(false);
    }

    #[test]
    #[ignore = "performance: 1 s native refresh target; run separately without test contention"]
    fn perf_git_watch_commit_branch_and_atomic_index_refresh_through_app_under_one_second() {
        isolated_git_watch_test!(
            perf_git_watch_commit_branch_and_atomic_index_refresh_through_app_under_one_second
        );
        git_watch_commit_branch_and_atomic_index_refresh_scenario(true);
    }

    fn git_watch_commit_branch_and_atomic_index_refresh_scenario(check_latency: bool) {
        let repo = GitWatchRepo::new("freshness");
        repo.init();
        let mut app = repo.app();
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));

        repo.git(&["commit", "--allow-empty", "-m", "watched"]);
        let (commit_latency, changed) = drive_git_watch_refresh(&mut app);
        assert!(changed);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));

        repo.git(&["switch", "-c", "feature/nested"]);
        let (branch_latency, changed) = drive_git_watch_refresh(&mut app);
        assert!(changed);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("feature/nested")
        );

        // git add atomically replaces index. An index change requests an App
        // refresh, but cannot invent dirty/staged fields or a visual change.
        std::fs::write(repo.0.join("staged.txt"), "staged\n").unwrap();
        let branch_before = app.state.workspaces[0].cached_git_branch.clone();
        let status_before = app.state.workspaces[0].git_ahead_behind();
        repo.git(&["add", "staged.txt"]);
        let (index_latency, changed) = drive_git_watch_refresh(&mut app);
        assert!(!changed);
        assert_eq!(app.state.workspaces[0].cached_git_branch, branch_before);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), status_before);
        if check_latency {
            assert!(
                commit_latency < GIT_WATCH_LATENCY_TARGET,
                "commit refresh: {commit_latency:?}"
            );
            assert!(
                branch_latency < GIT_WATCH_LATENCY_TARGET,
                "branch refresh: {branch_latency:?}"
            );
            assert!(
                index_latency < GIT_WATCH_LATENCY_TARGET,
                "index refresh: {index_latency:?}"
            );
        }
        eprintln!("native App refresh: commit={commit_latency:?} branch={branch_latency:?} index={index_latency:?}");

        // Status reads must not create a feedback loop of native access events.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn git_watch_linked_worktree_common_refs_packed_refs_and_config_refresh_through_app() {
        isolated_git_watch_test!(
            git_watch_linked_worktree_common_refs_packed_refs_and_config_refresh_through_app
        );
        let repo = GitWatchRepo::new("common-refs");
        repo.init();
        let linked = GitWatchRepo::new("linked");
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.0.to_str().unwrap(),
        ]);
        linked.git(&["branch", "--set-upstream-to=upstream", "linked"]);
        let mut app = linked.app();
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("linked")
        );
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));

        linked.git(&["commit", "--allow-empty", "-m", "linked commit"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
        let tip = linked.git(&["rev-parse", "HEAD"]);

        // Common refs changed by another checkout must wake the linked one.
        repo.git(&["update-ref", "refs/heads/upstream", &tip]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));

        repo.git(&["pack-refs", "--all", "--prune"]);
        drive_git_watch_refresh(&mut app);
        let initial = linked.git(&["rev-parse", "HEAD~1"]);
        repo.git(&["update-ref", "refs/heads/upstream", &initial]);
        repo.git(&["pack-refs", "--all", "--prune"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));

        linked.git(&["config", "--unset", "branch.linked.remote"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), None);

        linked.git(&["switch", "-c", "topic/switched"]);
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("topic/switched")
        );
    }

    #[test]
    fn git_watch_missed_event_safety_refresh_discovers_non_git_at_sixty_seconds() {
        isolated_git_watch_test!(
            git_watch_missed_event_safety_refresh_discovers_non_git_at_sixty_seconds
        );
        let repo = GitWatchRepo::new("missed-non-git");
        let mut app = repo.app();
        assert_eq!(app.state.workspaces[0].cached_git_branch, None);
        let safety_start = app.last_git_repo_discovery_refresh;
        // No .git existed when watches were installed, so init has no native hint.
        repo.init();
        assert!(app.event_rx.try_recv().is_err());
        let before = safety_start + std::time::Duration::from_secs(59);
        app.start_git_status_refresh_if_due(before);
        assert!(!app.git_refresh_in_flight);
        let due = safety_start + std::time::Duration::from_secs(60);
        app.start_git_status_refresh_if_due(due);
        assert!(app.git_refresh_in_flight);
        drive_git_watch_refresh(&mut app);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert_eq!(app.last_git_repo_discovery_refresh, due);
    }

    #[test]
    fn git_watch_missed_event_safety_refresh_updates_existing_git_at_sixty_seconds() {
        isolated_git_watch_test!(
            git_watch_missed_event_safety_refresh_updates_existing_git_at_sixty_seconds
        );
        let repo = GitWatchRepo::new("missed-existing-git");
        repo.init();
        let mut app = repo.app();
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
        let safety_start = app.last_git_repo_discovery_refresh;
        let cached_before = app.git_status_cache.clone();

        // Intentionally miss real git changes, not just repository discovery.
        app.clear_git_watches();
        repo.git(&["switch", "-c", "feature/missed"]);
        repo.git(&["branch", "--set-upstream-to=upstream", "feature/missed"]);
        repo.git(&["commit", "--allow-empty", "-m", "missed commit"]);
        while let Ok(event) = app.event_rx.try_recv() {
            // Drop any late native hint from the retired watcher too.
            assert!(matches!(event, AppEvent::GitFilesChanged));
        }

        app.start_git_status_refresh_if_due(safety_start + std::time::Duration::from_secs(59));
        assert!(!app.git_refresh_in_flight);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((0, 0)));
        assert_eq!(app.git_status_cache, cached_before);

        let due = safety_start + std::time::Duration::from_secs(60);
        app.start_git_status_refresh_if_due(due);
        assert!(app.git_refresh_in_flight);
        let (_, changed) = drive_git_watch_refresh(&mut app);
        assert!(changed);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("feature/missed")
        );
        assert_eq!(app.state.workspaces[0].git_ahead_behind(), Some((1, 0)));
        assert_ne!(app.git_status_cache, cached_before);
        assert_eq!(app.last_git_repo_discovery_refresh, due);
    }

    #[test]
    fn git_watch_events_do_not_postpone_full_discovery_and_debounce_survives_in_flight() {
        isolated_git_watch_test!(
            git_watch_events_do_not_postpone_full_discovery_and_debounce_survives_in_flight
        );
        let repo = GitWatchRepo::new("deadlines");
        repo.init();
        let mut app = repo.app();
        let now = Instant::now();
        app.last_git_repo_discovery_refresh = now;
        app.last_git_remote_status_refresh = now + std::time::Duration::from_secs(59);
        assert_eq!(
            app.git_refresh_deadline(),
            Some(now + std::time::Duration::from_secs(60))
        );
        app.git_refresh_in_flight = true;
        assert!(!app.handle_internal_event_with_render_impact(AppEvent::GitFilesChanged));
        assert_eq!(app.git_refresh_deadline(), None);
        let deadline = app.git_watch_refresh_deadline.unwrap();
        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            generation: 0,
            results: Vec::new(),
            cache_updates: Vec::new(),
        });
        assert_eq!(app.git_refresh_deadline(), Some(deadline));
    }

    #[test]
    fn git_watch_workspace_removal_and_demand_removal_release_watcher() {
        isolated_git_watch_test!(git_watch_workspace_removal_and_demand_removal_release_watcher);
        let repo = GitWatchRepo::new("app-removal");
        repo.init();
        let mut app = repo.app();
        assert!(app.git_watches.is_some());
        app.state.sidebar_spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        app.sync_git_watches();
        assert!(app.git_watches.is_none());
        app.state.sidebar_spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Branch]];
        app.sync_git_watches();
        assert!(app.git_watches.is_some());
        app.state.workspaces.clear();
        app.sync_git_watches();
        assert!(app.git_watches.is_none());
    }

    /// Workspace-only sidebar: no native watches or branch demand, so the only
    /// refreshes are the explicit identity/ordinary requests made by the test.
    fn stale_identity_app(repo: &GitWatchRepo) -> App {
        let mut config = crate::config::Config::default();
        config.ui.sidebar.spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]];
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("stale-identity");
        ws.tabs.clear();
        ws.identity_cwd = repo.0.clone();
        app.state.workspaces.push(ws);
        app
    }

    /// Start the real worker now and wait (10 s hang watchdog, event-driven)
    /// for its completion without applying it.
    #[track_caller]
    fn start_and_capture_git_completion(app: &mut App) -> AppEvent {
        let now = Instant::now();
        app.start_git_status_refresh_if_due(now);
        assert!(
            app.git_refresh_in_flight,
            "refresh worker must be in flight"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(GIT_WATCH_HANG_WATCHDOG, async {
                loop {
                    let event = app.event_rx.recv().await.expect("Git worker completion");
                    if matches!(event, AppEvent::GitStatusRefreshed { .. }) {
                        return event;
                    }
                }
            })
            .await
            .expect("git refresh worker hang watchdog")
        })
    }

    #[test]
    fn stale_missing_head_identity_completion_is_dropped_after_restore() {
        let repo = GitWatchRepo::new("stale-missing-head");
        repo.init();
        let mut app = stale_identity_app(&repo);
        app.request_git_identity_refresh(Instant::now());
        let initial = start_and_capture_git_completion(&mut app);
        assert!(app.handle_internal_event_with_render_impact(initial));
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        let space_before = app.state.workspaces[0].cached_git_space.clone();
        assert!(space_before.is_some());

        // Worker observes a checkout whose HEAD is absent (not discoverable).
        let head = repo.0.join(".git/HEAD");
        let parked = repo.0.join(".git/HEAD.parked");
        std::fs::rename(&head, &parked).unwrap();
        app.request_git_identity_refresh(Instant::now());
        let stale = start_and_capture_git_completion(&mut app);

        // HEAD is restored and a newer identity refresh is requested while the
        // old worker is still logically in flight.
        std::fs::rename(&parked, &head).unwrap();
        app.request_git_identity_refresh(Instant::now());

        let changed = app.handle_internal_event_with_render_impact(stale);
        assert!(
            !changed,
            "stale negative identity completion must not be published"
        );
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main"),
            "stale negative identity completion must not be applied"
        );
        assert_eq!(app.state.workspaces[0].cached_git_space, space_before);
        assert!(
            app.git_status_cache
                .values()
                .all(|entry| entry.fingerprint.is_some()),
            "stale negative cache entry must not be stored"
        );
        // Completion still releases the worker and runs the newer request.
        assert!(!app.git_refresh_in_flight);
        assert!(app.git_identity_refresh_requested);
        assert!(app
            .git_refresh_deadline()
            .is_some_and(|due| due <= Instant::now()));
        let newer = start_and_capture_git_completion(&mut app);
        app.handle_internal_event_with_render_impact(newer);
        assert!(!app.git_refresh_in_flight);
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert_eq!(app.state.workspaces[0].cached_git_space, space_before);
        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn ordinary_refresh_request_during_flight_keeps_useful_completion() {
        let repo = GitWatchRepo::new("ordinary-in-flight");
        repo.init();
        let mut app = stale_identity_app(&repo);
        app.request_git_identity_refresh(Instant::now());
        let useful = start_and_capture_git_completion(&mut app);

        // An ordinary HEAD write plus a non-identity refresh request.
        repo.git(&["switch", "-c", "feature/ordinary"]);
        app.mark_git_status_refresh_due(Instant::now());
        assert!(app.git_refresh_due_after_in_flight);

        assert!(app.handle_internal_event_with_render_impact(useful));
        assert_eq!(
            app.state.workspaces[0].cached_git_branch.as_deref(),
            Some("main")
        );
        assert!(app.state.workspaces[0].cached_git_space.is_some());
        assert!(!app.git_refresh_in_flight);
    }

    include!("git_refresh_regression_tests.rs");

    fn test_app(config: &crate::config::Config) -> super::super::App {
        super::super::App::new(
            config,
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }
}
