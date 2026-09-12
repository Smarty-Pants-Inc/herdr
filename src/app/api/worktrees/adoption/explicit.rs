use super::*;
use crate::api::schema::{
    ResponseResult, WorktreeAdoptPane, WorktreeAdoptParams, WorktreeVerifyAdoptionParams,
};
use crate::app::api::responses::{encode_error, encode_success};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlainCheckout {
    binding: WorktreeAdoptPane,
    native: crate::platform::WorktreeProcessIdentity,
    job: ForegroundJob,
    cwd: PathBuf,
    reported_cwd: PathBuf,
    foreground_cwd: PathBuf,
    session_processes: Vec<u32>,
    worker: Option<LiveWorker>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveWorker {
    pid: u32,
    session_id: String,
    processes: Vec<(u32, crate::platform::WorktreeProcessIdentity, ForegroundJob)>,
}

impl App {
    pub(in crate::app::api) fn handle_worktree_verify_adoption(
        &mut self,
        id: String,
        params: WorktreeVerifyAdoptionParams,
    ) -> String {
        if !crate::platform::capabilities().foreground_worktree_adoption
            || params
                .agent_session_id
                .as_ref()
                .is_some_and(|id| crate::agent_resume::AgentSessionRef::id(id.clone()).is_none())
        {
            return encode_error(
                id,
                "worktree_adoption_unavailable",
                "native verification is unavailable",
            );
        }
        let target = match self.exact_adoption_target(&params.binding) {
            Ok(index) => index,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        self.verify_adoption_with(id, &params, |app, index| {
            let session = if index == target {
                params.agent_session_id.as_deref()
            } else {
                None
            };
            app.observe_plain_checkout(index, session)
        })
    }

    fn verify_adoption_with(
        &mut self,
        id: String,
        params: &WorktreeVerifyAdoptionParams,
        mut observe: impl FnMut(&Self, usize) -> Result<Vec<PlainCheckout>, ApiFailure>,
    ) -> String {
        let prepared = self.prepare_explicit_adoption(
            &params.binding,
            params.agent_session_id.as_deref(),
            &mut observe,
        );
        let (source, target, entry) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        if self.state.workspaces[target].worktree_space().is_none() {
            return encode_error(
                id,
                "worktree_adoption_unavailable",
                "target has not been adopted",
            );
        }
        let known = match self.exact_adoption_workspace(&params.binding.known_workspace_id) {
            Ok(index) => index,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        let target_info = self.worktree_info_for_entry(&source, entry.clone(), Some(target));
        let known_info = self.worktree_info_for_entry(&source, entry, Some(known));
        // Qualified result, never a path-only lookup or membership mutation.
        encode_success(
            id,
            ResponseResult::WorktreeList {
                source: self.worktree_source_info(&source),
                worktrees: vec![target_info, known_info],
            },
        )
    }

    pub(in crate::app::api) fn handle_worktree_adopt(
        &mut self,
        id: String,
        params: WorktreeAdoptParams,
    ) -> String {
        if !crate::platform::capabilities().foreground_worktree_adoption {
            return encode_error(
                id,
                "worktree_adoption_unavailable",
                "native adoption is unsupported",
            );
        }
        self.handle_worktree_adopt_with(id, params, |app, index| {
            app.observe_plain_checkout(index, None)
        })
    }

    fn handle_worktree_adopt_with(
        &mut self,
        id: String,
        params: WorktreeAdoptParams,
        mut observe: impl FnMut(&Self, usize) -> Result<Vec<PlainCheckout>, ApiFailure>,
    ) -> String {
        let prepared = self.prepare_explicit_adoption(&params, None, &mut observe);
        let (source, target, entry) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return encode_error(id, err.code, err.message),
        };
        let Some(tab) = self.tab_info(target, 0) else {
            return encode_error(
                id,
                "worktree_adoption_unavailable",
                "target tab is unavailable",
            );
        };
        let Some(root_pane) = self.root_pane_info(target, 0) else {
            return encode_error(
                id,
                "worktree_adoption_unavailable",
                "target pane is unavailable",
            );
        };
        // No allocation, focus change, pane move, or modification of the known other view.
        let changed = self.state.workspaces[target].worktree_space().is_none();
        if changed {
            self.mark_worktree_membership(&source, target, entry.path.clone(), true, true);
        }
        let worktree = self.worktree_info_for_entry(&source, entry, Some(target));
        if changed {
            self.emit_worktree_opened_event(target, worktree.clone(), true);
            self.schedule_session_save();
        }
        encode_success(
            id,
            ResponseResult::WorktreeOpened {
                workspace: self.workspace_info(target),
                tab,
                root_pane,
                worktree,
                already_open: true,
            },
        )
    }

    fn prepare_explicit_adoption(
        &mut self,
        params: &WorktreeAdoptParams,
        agent_session: Option<&str>,
        observe: &mut impl FnMut(&Self, usize) -> Result<Vec<PlainCheckout>, ApiFailure>,
    ) -> Result<(WorktreeSource, usize, crate::worktree::ExistingWorktree), ApiFailure> {
        let target = self.exact_adoption_target(params)?;
        let parent = self.exact_adoption_workspace(&params.parent_workspace_id)?;
        let known = self.exact_adoption_workspace(&params.known_workspace_id)?;
        if parent == target || parent == known || target == known || params.known_panes.is_empty() {
            return Err(unavailable());
        }
        let source = self.worktree_source_from_workspace(parent)?;
        let checkout = physical(Path::new(&params.path))?;
        let root = physical(&source.source_repo_root)?;
        let git = crate::workspace::git_space_metadata(&checkout).ok_or_else(unavailable)?;
        let root_git = crate::workspace::git_space_metadata(&root).ok_or_else(unavailable)?;
        if Path::new(&params.path) != checkout
            || Path::new(&params.repo_root) != root
            || physical(Path::new(&params.repo_root))? != root
            || params.repo_key != git.key
            || physical(Path::new(&params.repo_key))? != Path::new(&git.key)
            || checkout == root
            || !git.is_linked_worktree
            || root_git.is_linked_worktree
            || git.key != root_git.key
            || git.key != source.repo_key
            || physical(Path::new(&git.checkout_key))? != checkout
            || physical(&root_git.repo_root)? != root
        {
            return Err(unavailable());
        }
        self.validate_adoption_membership(parent, &root, &root, &git.key, false)?;
        self.validate_adoption_membership(known, &checkout, &root, &git.key, true)?;
        if self.state.workspaces[target].worktree_space().is_some() {
            self.validate_adoption_membership(target, &checkout, &root, &git.key, true)?;
        }
        let entry = self.find_worktree_entry(&source, Some(params.path.clone()), None, false)?;
        if entry.is_bare || entry.is_prunable || entry.path != checkout {
            return Err(unavailable());
        }
        let [tab] = self.state.workspaces[target].tabs.as_slice() else {
            return Err(unavailable());
        };
        if tab.panes.len() != 1
            || !tab.panes.contains_key(&tab.root_pane)
            || self.state.workspaces[target].active_tab != 0
        {
            return Err(unavailable());
        }
        let mut expected = params.known_panes.clone();
        expected.push(params.target.clone());
        expected.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        for (index, pane) in expected.iter().enumerate() {
            if expected[..index].iter().any(|other| {
                other.pane_id == pane.pane_id
                    || other.terminal_id == pane.terminal_id
                    || other.shell_pid == pane.shell_pid
            }) {
                return Err(unavailable());
            }
        }
        let mut before = None;
        for _ in 0..2 {
            // Include all other workspaces in both observations: an unexpected third
            // checkout (including a nested foreground shell) never becomes a second view.
            for (index, workspace) in self.state.workspaces.iter().enumerate() {
                if index == target || index == known || index == parent {
                    continue;
                }
                if workspace.worktree_space().is_some_and(|membership| {
                    !membership.is_linked_worktree && membership.key == git.key
                }) || self.workspace_matches_checkout(workspace, &checkout, &params.path)
                    || self.workspace_matches_checkout(workspace, &root, &params.repo_root)
                    || self
                        .foreground_checkout(&source, index, &checkout)?
                        .is_some()
                {
                    return Err(unavailable());
                }
            }
            self.validate_plain_workspace(target, &checkout, agent_session)?;
            self.validate_plain_workspace(known, &checkout, None)?;
            let mut observed = observe(self, target)?;
            if observed.len() != 1 || observed[0].binding != params.target {
                return Err(unavailable());
            }
            observed.extend(observe(self, known)?);
            observed.sort_by(|a, b| a.binding.pane_id.cmp(&b.binding.pane_id));
            if observed.len() != expected.len()
                || !observed.iter().zip(&expected).all(|(pane, expected)| {
                    let session = if pane.binding == params.target {
                        agent_session
                    } else {
                        None
                    };
                    pane.binding == *expected && valid_plain_checkout(pane, &checkout, session)
                })
                || before.as_ref().is_some_and(|before| before != &observed)
                || crate::workspace::git_space_metadata(&checkout).as_ref() != Some(&git)
                || crate::workspace::git_space_metadata(&root).as_ref() != Some(&root_git)
            {
                return Err(unavailable());
            }
            before = Some(observed);
        }
        if self.find_worktree_entry(&source, Some(params.path.clone()), None, false)? != entry {
            return Err(unavailable());
        }
        Ok((source, target, entry))
    }

    fn exact_adoption_target(&self, params: &WorktreeAdoptParams) -> Result<usize, ApiFailure> {
        let (index, pane) = self.parse_pane_id(&params.target.pane_id).ok_or_else(|| {
            ApiFailure::new(
                "pane_not_found",
                "target pane not found in the selected server",
            )
        })?;
        if index != self.exact_adoption_workspace(&params.workspace_id)?
            || self.public_pane_id(index, pane).as_deref() != Some(params.target.pane_id.as_str())
        {
            return Err(unavailable());
        }
        Ok(index)
    }

    fn exact_adoption_workspace(&self, id: &str) -> Result<usize, ApiFailure> {
        let index = self.parse_workspace_id(id).ok_or_else(unavailable)?;
        if self.public_workspace_id(index) != id
            || self
                .state
                .workspaces
                .iter()
                .filter(|workspace| workspace.id == id)
                .count()
                != 1
        {
            return Err(unavailable());
        }
        Ok(index)
    }

    fn validate_adoption_membership(
        &self,
        index: usize,
        checkout: &Path,
        root: &Path,
        key: &str,
        linked: bool,
    ) -> Result<(), ApiFailure> {
        let membership = self.state.workspaces[index]
            .worktree_space()
            .ok_or_else(unavailable)?;
        if membership.key != key
            || membership.is_linked_worktree != linked
            || physical(&membership.checkout_path)? != checkout
            || physical(&membership.repo_root)? != root
        {
            return Err(unavailable());
        }
        Ok(())
    }

    fn validate_plain_workspace(
        &self,
        index: usize,
        checkout: &Path,
        agent_session: Option<&str>,
    ) -> Result<(), ApiFailure> {
        let workspace = &self.state.workspaces[index];
        if workspace.tabs.is_empty() || physical(&workspace.identity_cwd)? != checkout {
            return Err(unavailable());
        }
        for tab in &workspace.tabs {
            if tab.panes.is_empty() || !tab.panes.contains_key(&tab.root_pane) {
                return Err(unavailable());
            }
            for (pane_id, pane) in &tab.panes {
                let terminal = self
                    .state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .ok_or_else(unavailable)?;
                let expected_worker =
                    agent_session.is_some_and(|id| expected_pi_session(terminal, id));
                if (!expected_worker
                    && (terminal.detected_agent.is_some()
                        || terminal.hook_authority.is_some()
                        || terminal.persisted_agent_session.is_some()
                        || terminal.agent_name.is_some()
                        || terminal.managed_agent_kind().is_some()))
                    || self
                        .state
                        .workspaces
                        .iter()
                        .flat_map(|workspace| &workspace.tabs)
                        .flat_map(|tab| tab.panes.iter())
                        .filter(|(other_id, other)| {
                            **other_id == *pane_id
                                || other.attached_terminal_id == pane.attached_terminal_id
                        })
                        .count()
                        != 1
                {
                    return Err(unavailable());
                }
            }
        }
        Ok(())
    }

    fn observe_plain_checkout(
        &self,
        index: usize,
        agent_session: Option<&str>,
    ) -> Result<Vec<PlainCheckout>, ApiFailure> {
        let mut observed = Vec::new();
        for (tab_index, tab) in self.state.workspaces[index].tabs.iter().enumerate() {
            for (pane_id, pane) in &tab.panes {
                let runtime = self
                    .terminal_runtimes
                    .get(&pane.attached_terminal_id)
                    .ok_or_else(unavailable)?;
                let pid = runtime.child_pid().ok_or_else(unavailable)?;
                if crate::platform::process_agent_hint(pid).is_some() {
                    return Err(unavailable());
                }
                let foreground =
                    crate::platform::foreground_process_group_id(pid).ok_or_else(unavailable)?;
                let mut session_processes = crate::platform::session_processes(pid);
                session_processes.sort_unstable();
                let worker = if foreground == pid {
                    None
                } else {
                    let session_id = agent_session.ok_or_else(unavailable)?;
                    let terminal = self
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .ok_or_else(unavailable)?;
                    if !expected_pi_session(terminal, session_id) {
                        return Err(unavailable());
                    }
                    let mut processes = Vec::new();
                    for child in session_processes
                        .iter()
                        .copied()
                        .filter(|child| *child != pid)
                    {
                        let identity = crate::platform::worktree_process_identity(child)
                            .ok_or_else(unavailable)?;
                        let job = crate::platform::foreground_group_leader_job(child)
                            .ok_or_else(unavailable)?;
                        if crate::platform::process_agent_hint(child)
                            .is_some_and(|agent| agent != crate::detect::Agent::Pi)
                            || crate::detect::identify_agent_in_job(&job)
                                .is_some_and(|(agent, _)| agent != crate::detect::Agent::Pi)
                        {
                            return Err(unavailable());
                        }
                        processes.push((child, identity, job));
                    }
                    Some(LiveWorker {
                        pid: foreground,
                        session_id: session_id.into(),
                        processes,
                    })
                };
                let native =
                    crate::platform::worktree_process_identity(pid).ok_or_else(unavailable)?;
                observed.push(PlainCheckout {
                    binding: WorktreeAdoptPane {
                        pane_id: self
                            .public_pane_id(index, *pane_id)
                            .ok_or_else(unavailable)?,
                        tab_id: self
                            .public_tab_id(index, tab_index)
                            .ok_or_else(unavailable)?,
                        terminal_id: pane.attached_terminal_id.to_string(),
                        shell_pid: pid,
                        shell_birth: [native.birth.0, native.birth.1],
                    },
                    native,
                    job: crate::platform::foreground_group_leader_job(pid)
                        .ok_or_else(unavailable)?,
                    cwd: crate::platform::process_cwd(pid).ok_or_else(unavailable)?,
                    reported_cwd: runtime.cwd().ok_or_else(unavailable)?,
                    foreground_cwd: runtime.foreground_cwd().ok_or_else(unavailable)?,
                    session_processes,
                    worker,
                });
            }
        }
        Ok(observed)
    }
}

fn valid_plain_checkout(
    pane: &PlainCheckout,
    checkout: &Path,
    agent_session: Option<&str>,
) -> bool {
    let pid = pane.binding.shell_pid;
    pid != 0
        && pane.native.parent_pid != 0
        && pane.native.session == pid
        && pane.native.process_group == pid
        && pane.native.terminal != 0
        && [pane.native.birth.0, pane.native.birth.1] == pane.binding.shell_birth
        && pane
            .native
            .executable
            .to_str()
            .is_some_and(crate::platform::is_pane_shell_process_name)
        && pane.job.process_group_id == pid
        && single_shell(&pane.job)
        && match &pane.worker {
            None => pane.session_processes == [pid],
            Some(worker) => {
                agent_session == Some(worker.session_id.as_str()) && valid_worker_tree(pane, worker)
            }
        }
        && pane.cwd == checkout
        && pane.reported_cwd == checkout
        && pane.foreground_cwd == checkout
}

fn expected_pi_session(terminal: &crate::terminal::TerminalState, id: &str) -> bool {
    terminal
        .persisted_agent_session
        .as_ref()
        .is_some_and(|session| {
            session.agent == "pi"
                && session.session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id
                && session.session_ref.value == id
        })
        && terminal
            .detected_agent
            .is_none_or(|agent| agent == crate::detect::Agent::Pi)
        && terminal
            .managed_agent_kind()
            .is_none_or(|agent| agent == crate::detect::Agent::Pi)
        && terminal.hook_authority.as_ref().is_none_or(|authority| {
            crate::detect::parse_agent_label(&authority.agent_label)
                == Some(crate::detect::Agent::Pi)
                && authority.session_ref.as_ref().is_none_or(|session| {
                    session.kind == crate::agent_resume::AgentSessionRefKind::Id
                        && session.value == id
                })
        })
}

fn valid_worker_tree(pane: &PlainCheckout, worker: &LiveWorker) -> bool {
    let shell = pane.binding.shell_pid;
    let Some((_, native, job)) = worker
        .processes
        .iter()
        .find(|(pid, _, _)| *pid == worker.pid)
    else {
        return false;
    };
    if worker.pid == shell
        || native.parent_pid != shell
        || native.process_group != worker.pid
        || crate::detect::identify_agent_in_job(job).map(|(agent, _)| agent)
            != Some(crate::detect::Agent::Pi)
    {
        return false;
    }
    let mut pids: Vec<_> = worker.processes.iter().map(|(pid, _, _)| *pid).collect();
    pids.push(shell);
    pids.sort_unstable();
    if pids != pane.session_processes || pids.windows(2).any(|pair| pair[0] == pair[1]) {
        return false;
    }
    for (pid, identity, job) in &worker.processes {
        if identity.session != shell || identity.terminal != pane.native.terminal {
            return false;
        }
        if *pid == worker.pid {
            continue;
        }
        if crate::detect::identify_agent_in_job(job).is_some() {
            return false;
        }
        let mut parent = identity.parent_pid;
        let mut remaining = worker.processes.len();
        while parent != worker.pid {
            if remaining == 0 || parent == shell {
                return false;
            }
            let Some((_, ancestor, _)) = worker.processes.iter().find(|(pid, _, _)| *pid == parent)
            else {
                return false;
            };
            parent = ancestor.parent_pid;
            remaining -= 1;
        }
    }
    true
}

#[cfg(test)]
mod tests;
