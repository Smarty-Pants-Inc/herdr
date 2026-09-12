use std::path::{Path, PathBuf};

use crate::app::App;
use crate::platform::ForegroundJob;

use super::{ApiFailure, WorktreeSource};

mod explicit;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForegroundCheckout {
    workspace: String,
    tab: usize,
    pane: crate::layout::PaneId,
    terminal: crate::terminal::TerminalId,
    shell: u32,
    shell_birth: (u64, u64),
    root_job: ForegroundJob,
    shell_cwd: PathBuf,
    reported_cwd: PathBuf,
    foreground_cwd: PathBuf,
    foreground_birth: (u64, u64),
    session_processes: Vec<u32>,
    treehouse: Option<TreehouseIntermediary>,
    job: ForegroundJob,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreehouseIntermediary {
    pid: u32,
    root: crate::platform::WorktreeProcessIdentity,
    process: crate::platform::WorktreeProcessIdentity,
    child: crate::platform::WorktreeProcessIdentity,
    job: ForegroundJob,
    invoked_executable: PathBuf,
    cwd: PathBuf,
}

fn unavailable() -> ApiFailure {
    ApiFailure::new(
        "worktree_adoption_unavailable",
        "existing foreground checkout has conflicting or unavailable identity",
    )
}

fn physical(path: &Path) -> Result<PathBuf, ApiFailure> {
    std::fs::canonicalize(path).map_err(|_| unavailable())
}

impl App {
    /// Explicit list/open only: never widen parent discovery or render-time identity scans.
    pub(super) fn lookup_worktree_checkout(
        &self,
        source: &WorktreeSource,
        checkout: &Path,
    ) -> Result<Option<usize>, ApiFailure> {
        self.lookup_worktree_checkout_with(source, checkout, |app, index, checkout| {
            if crate::platform::capabilities().foreground_worktree_adoption {
                app.foreground_checkout(source, index, checkout)
            } else {
                Ok(None)
            }
        })
    }

    fn lookup_worktree_checkout_with(
        &self,
        source: &WorktreeSource,
        checkout: &Path,
        mut observe: impl FnMut(&Self, usize, &Path) -> Result<Option<ForegroundCheckout>, ApiFailure>,
    ) -> Result<Option<usize>, ApiFailure> {
        let checkout = physical(checkout)?;
        let parent = physical(&source.source_repo_root)?;
        if checkout == parent {
            return Ok(self.open_workspace_idx_for_checkout(&checkout));
        }
        let git = crate::workspace::git_space_metadata(&checkout).ok_or_else(unavailable)?;
        if !git.is_linked_worktree
            || git.key != source.repo_key
            || physical(Path::new(&git.checkout_key))? != checkout
        {
            return Err(unavailable());
        }

        let checkout_key = checkout.display().to_string();
        let mut selected = None;
        let mut foreground = None;
        for (index, workspace) in self.state.workspaces.iter().enumerate() {
            let existing = self.workspace_matches_checkout(workspace, &checkout, &checkout_key);
            if existing {
                if let Some(membership) = workspace.worktree_space() {
                    if membership.key != git.key
                        || !membership.is_linked_worktree
                        || physical(&membership.checkout_path)? != checkout
                        || physical(&membership.repo_root)? != parent
                    {
                        return Err(unavailable());
                    }
                } else {
                    let current = workspace
                        .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .as_deref()
                        .and_then(crate::workspace::git_space_metadata)
                        .ok_or_else(unavailable)?;
                    if current.key != git.key
                        || physical(Path::new(&current.checkout_key))? != checkout
                    {
                        return Err(unavailable());
                    }
                }
            }
            // Preserve registered/preallocated workspace behavior, including active agents.
            // Other panes at the same checkout must still make this lookup ambiguous.
            let observed = if existing {
                None
            } else {
                observe(self, index, &checkout)?
            };
            if !existing && observed.is_none() {
                continue;
            }
            if selected.replace(index).is_some() {
                return Err(ApiFailure::new(
                    "worktree_adoption_ambiguous",
                    "more than one workspace refers to this checkout",
                ));
            }
            if let Some(observed) = observed {
                self.validate_foreground_identity(index, &observed)?;
                if workspace.worktree_space().is_some()
                    || source.workspace_idx.is_none()
                    || source.workspace_idx == Some(index)
                    || physical(&observed.shell_cwd)? != parent
                    || physical(&observed.reported_cwd)? != parent
                    || physical(&workspace.identity_cwd)? != parent
                    || observed.foreground_cwd != checkout
                {
                    return Err(unavailable());
                }
                if let Some(cached) = workspace.git_space() {
                    if cached.key != git.key
                        || cached.is_linked_worktree
                        || physical(Path::new(&cached.checkout_key))? != parent
                    {
                        return Err(unavailable());
                    }
                }
                let root_git = crate::workspace::git_space_metadata(&observed.shell_cwd)
                    .ok_or_else(unavailable)?;
                if root_git.is_linked_worktree || root_git.key != git.key {
                    return Err(unavailable());
                }
                foreground = Some(observed);
            }
        }
        if let (Some(index), Some(before)) = (selected, foreground) {
            // App topology is serialized on this thread; the OS process is not.
            // Reobserve birth/argv/cwd and endpoint identity before permitting adoption.
            if observe(self, index, &checkout)?.as_ref() != Some(&before)
                || crate::workspace::git_space_metadata(&checkout).as_ref() != Some(&git)
            {
                return Err(unavailable());
            }
            let current_parent =
                crate::workspace::git_space_metadata(&parent).ok_or_else(unavailable)?;
            if current_parent.key != git.key
                || current_parent.is_linked_worktree
                || physical(&current_parent.repo_root)? != parent
            {
                return Err(unavailable());
            }
        }
        Ok(selected)
    }

    fn validate_foreground_identity(
        &self,
        index: usize,
        observed: &ForegroundCheckout,
    ) -> Result<(), ApiFailure> {
        let workspace = self.state.workspaces.get(index).ok_or_else(unavailable)?;
        let [tab] = workspace.tabs.as_slice() else {
            return Err(unavailable());
        };
        let terminal_id = tab.terminal_id(tab.root_pane).ok_or_else(unavailable)?;
        let terminal = self
            .state
            .terminals
            .get(terminal_id)
            .ok_or_else(unavailable)?;
        if workspace.active_tab != 0
            || tab.panes.len() != 1
            || observed.workspace != workspace.id
            || self
                .state
                .workspaces
                .iter()
                .filter(|ws| ws.id == observed.workspace)
                .count()
                != 1
            || observed.tab != tab.number
            || observed.pane != tab.root_pane
            || observed.terminal != *terminal_id
            || observed.shell == 0
            || observed.root_job.process_group_id != observed.shell
            || observed.job.process_group_id == observed.shell
            || !single_shell(&observed.root_job)
            || !single_shell(&observed.job)
            || !supported_session_shape(observed)
            || !observed.session_processes.contains(&observed.shell)
            || !observed
                .session_processes
                .contains(&observed.job.process_group_id)
            || terminal.detected_agent.is_some()
            || terminal.hook_authority.is_some()
            || terminal.persisted_agent_session.is_some()
            || terminal.agent_name.is_some()
            || terminal.managed_agent_kind().is_some()
            || self
                .state
                .workspaces
                .iter()
                .flat_map(|ws| &ws.tabs)
                .flat_map(|tab| tab.panes.iter())
                .filter(|(pane_id, pane)| {
                    **pane_id == observed.pane || pane.attached_terminal_id == *terminal_id
                })
                .count()
                != 1
        {
            return Err(unavailable());
        }
        Ok(())
    }

    fn foreground_checkout(
        &self,
        source: &WorktreeSource,
        index: usize,
        checkout: &Path,
    ) -> Result<Option<ForegroundCheckout>, ApiFailure> {
        let workspace = self.state.workspaces.get(index).ok_or_else(unavailable)?;
        let unresolved_parent_shell = source.workspace_idx != Some(index)
            && workspace.worktree_space().is_none()
            && workspace
                .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .and_then(|cwd| std::fs::canonicalize(cwd).ok())
                == Some(physical(&source.source_repo_root)?);
        let mut selected = None;
        for tab in &workspace.tabs {
            for (pane_id, pane) in &tab.panes {
                let Some(runtime) = self.terminal_runtimes.get(&pane.attached_terminal_id) else {
                    if unresolved_parent_shell {
                        return Err(unavailable());
                    }
                    continue;
                };
                let Some(cwd) = runtime.foreground_cwd() else {
                    if unresolved_parent_shell {
                        return Err(unavailable());
                    }
                    continue;
                };
                let physical_cwd = match physical(&cwd) {
                    Ok(cwd) => cwd,
                    Err(err) if unresolved_parent_shell => return Err(err),
                    Err(_) => continue,
                };
                if physical_cwd != checkout {
                    continue;
                }
                if selected.is_some()
                    || workspace.tabs.len() != 1
                    || tab.panes.len() != 1
                    || tab.root_pane != *pane_id
                    || cwd != checkout
                {
                    return Err(unavailable());
                }
                let shell = runtime.child_pid().ok_or_else(unavailable)?;
                let shell_birth =
                    crate::platform::process_birth_identity(shell).ok_or_else(unavailable)?;
                let root_job =
                    crate::platform::foreground_group_leader_job(shell).ok_or_else(unavailable)?;
                if !single_shell(&root_job) {
                    return Err(unavailable());
                }
                let job = crate::platform::foreground_job(shell).ok_or_else(unavailable)?;
                if !single_shell(&job)
                    || job.process_group_id == shell
                    || crate::platform::foreground_process_group_id(shell)
                        != Some(job.process_group_id)
                    || crate::platform::process_agent_hint(job.process_group_id).is_some()
                {
                    return Err(unavailable());
                }
                let foreground_birth =
                    crate::platform::process_birth_identity(job.process_group_id)
                        .ok_or_else(unavailable)?;
                let actual_cwd =
                    crate::platform::process_cwd(job.process_group_id).ok_or_else(unavailable)?;
                if actual_cwd != cwd {
                    return Err(unavailable());
                }
                let mut session_processes = crate::platform::session_processes(shell);
                session_processes.sort_unstable();
                let treehouse = observe_treehouse_intermediary(
                    shell,
                    job.process_group_id,
                    &session_processes,
                )?;
                selected = Some(ForegroundCheckout {
                    workspace: workspace.id.clone(),
                    tab: tab.number,
                    pane: *pane_id,
                    terminal: pane.attached_terminal_id.clone(),
                    shell,
                    shell_birth,
                    root_job,
                    shell_cwd: crate::platform::process_cwd(shell).ok_or_else(unavailable)?,
                    reported_cwd: runtime.cwd().ok_or_else(unavailable)?,
                    foreground_cwd: cwd,
                    foreground_birth,
                    session_processes,
                    treehouse,
                    job,
                });
            }
        }
        Ok(selected)
    }
}

fn observe_treehouse_intermediary(
    shell: u32,
    foreground: u32,
    session: &[u32],
) -> Result<Option<TreehouseIntermediary>, ApiFailure> {
    if session.len() != 3 {
        return Ok(None);
    }
    let extra: Vec<_> = session
        .iter()
        .copied()
        .filter(|pid| *pid != shell && *pid != foreground)
        .collect();
    let [pid] = extra.as_slice() else {
        return Err(unavailable());
    };
    let process = crate::platform::worktree_process_identity(*pid).ok_or_else(unavailable)?;
    // ponytail: trust the installed executable selected through the server's PATH,
    // not comm/argv alone. Exec wrappers may precede the real binary on PATH.
    // This admits the observed Treehouse 2.3.0 get shape, not arbitrary helpers.
    if !installed_treehouse_executable(&process.executable)
        || crate::platform::process_agent_hint(*pid).is_some()
    {
        return Err(unavailable());
    }
    let job = crate::platform::foreground_group_leader_job(*pid).ok_or_else(unavailable)?;
    let invoked = job
        .processes
        .first()
        .and_then(|process| process.argv.as_ref())
        .and_then(|argv| argv.first())
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(unavailable)?;
    let invoked_executable = physical(invoked)?;
    Ok(Some(TreehouseIntermediary {
        pid: *pid,
        root: crate::platform::worktree_process_identity(shell).ok_or_else(unavailable)?,
        process,
        child: crate::platform::worktree_process_identity(foreground).ok_or_else(unavailable)?,
        job,
        invoked_executable,
        cwd: crate::platform::process_cwd(*pid).ok_or_else(unavailable)?,
    }))
}

fn installed_treehouse_executable(executable: &Path) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| treehouse_executable_on_path(executable, &path))
}

fn treehouse_executable_on_path(executable: &Path, path: &std::ffi::OsStr) -> bool {
    executable.is_absolute()
        && executable.file_name().is_some_and(|name| name == "treehouse")
        && std::env::split_paths(path)
            .filter(|directory| directory.is_absolute())
            .any(|directory| {
                std::fs::canonicalize(directory.join("treehouse"))
                    .ok()
                    .as_deref()
                    == Some(executable)
            })
}

fn supported_session_shape(observed: &ForegroundCheckout) -> bool {
    let Some(helper) = &observed.treehouse else {
        return observed.session_processes.len() == 2;
    };
    let [process] = helper.job.processes.as_slice() else {
        return false;
    };
    let Some(argv) = process.argv.as_deref() else {
        return false;
    };
    observed.session_processes.len() == 3
        && observed.session_processes.contains(&helper.pid)
        && helper.pid != observed.shell
        && helper.pid != observed.job.process_group_id
        && helper.job.process_group_id == helper.pid
        && process.pid == helper.pid
        && process.name == "treehouse"
        && argv.len() == 2
        && helper.invoked_executable == helper.process.executable
        && argv[1] == "get"
        && helper.root.session == observed.shell
        && helper.root.process_group == observed.shell
        && helper.root.birth == observed.shell_birth
        && helper.process.parent_pid == observed.shell
        && helper.process.session == observed.shell
        && helper.process.process_group == helper.pid
        && helper.child.parent_pid == helper.pid
        && helper.child.session == observed.shell
        && helper.child.process_group == observed.job.process_group_id
        && helper.child.birth == observed.foreground_birth
        && helper.root.terminal != 0
        && helper.process.terminal == helper.root.terminal
        && helper.child.terminal == helper.root.terminal
        && helper.cwd == observed.shell_cwd
}

#[cfg(test)]
#[path = "adoption_tests.rs"]
mod tests;

fn single_shell(job: &ForegroundJob) -> bool {
    let [process] = job.processes.as_slice() else {
        return false;
    };
    process.pid == job.process_group_id
        && crate::platform::is_pane_shell_process_name(&process.name)
        && process.argv.as_ref().is_some_and(|argv| {
            argv.first()
                .is_some_and(|name| crate::platform::is_pane_shell_process_name(name))
                && argv.iter().skip(1).all(|arg| {
                    matches!(
                        arg.as_str(),
                        "-i" | "-l" | "-il" | "-li" | "-f" | "--login" | "--noprofile" | "--norc"
                    )
                })
        })
}
