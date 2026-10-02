use super::{api_helpers::pane_agent_status, App};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTarget {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: crate::layout::PaneId,
    pub terminal_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTargetCandidate {
    pub terminal_id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalTargetError {
    NotFound {
        target: String,
    },
    Ambiguous {
        target: String,
        candidates: Vec<TerminalTargetCandidate>,
    },
}

impl App {
    /// Compare only opaque identities, never pane positions or agent names. The caller
    /// holds server ownership through the effect and uses the checked terminal directly.
    pub(super) fn check_expected_terminal(
        &self,
        expected: Option<&str>,
        actual: Option<&crate::terminal::TerminalId>,
    ) -> Result<(), crate::api::schema::ErrorBody> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let actual = actual.filter(|id| self.state.terminals.contains_key(*id));
        if actual.is_some_and(|id| id.as_str() == expected) {
            return Ok(());
        }
        Err(crate::api::schema::ErrorBody {
            code: "terminal_identity_mismatch".into(),
            message: format!(
                "expected terminal {expected:?}, but the target pane owns {}",
                actual.map_or("no terminal", |id| id.as_str()),
            ),
        })
    }

    pub(crate) fn resolve_terminal_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        let terminal_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| candidate.terminal_id == target)
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, terminal_matches)? {
            return Ok(resolved);
        }

        if let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(target) {
            if let Some(resolved) = self.terminal_target_for_pane(ws_idx, pane_id) {
                return Ok(resolved);
            }
        }

        let agent_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| {
                self.state
                    .terminals
                    .values()
                    .find(|terminal| terminal.id.to_string() == candidate.terminal_id)
                    .is_some_and(|terminal| {
                        terminal.agent_name.as_deref() == Some(target)
                            || terminal.effective_agent_label() == Some(target)
                    })
            })
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, agent_matches)? {
            return Ok(resolved);
        }

        Err(TerminalTargetError::NotFound {
            target: target.to_string(),
        })
    }

    pub(crate) fn resolve_agent_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        if let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(target) {
            if let Some(resolved) = self
                .terminal_target_for_pane(ws_idx, pane_id)
                .filter(|resolved| self.target_is_agent(resolved))
            {
                return Ok(resolved);
            }
        }

        let name_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| {
                self.state
                    .terminals
                    .values()
                    .find(|terminal| terminal.id.to_string() == candidate.terminal_id)
                    .is_some_and(|terminal| terminal.agent_name.as_deref() == Some(target))
            })
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, name_matches)? {
            return Ok(resolved);
        }

        Err(TerminalTargetError::NotFound {
            target: target.to_string(),
        })
    }

    fn target_is_agent(&self, target: &TerminalTarget) -> bool {
        self.state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == target.terminal_id)
            .is_some_and(|terminal| terminal.is_agent_terminal())
    }

    fn single_terminal_match(
        &self,
        target: &str,
        matches: Vec<TerminalTarget>,
    ) -> Result<Option<TerminalTarget>, TerminalTargetError> {
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(TerminalTargetError::Ambiguous {
                target: target.to_string(),
                candidates: matches
                    .into_iter()
                    .filter_map(|candidate| {
                        self.terminal_target_candidate(candidate.ws_idx, candidate.pane_id)
                    })
                    .collect(),
            }),
        }
    }

    pub(crate) fn terminal_targets(&self) -> Vec<TerminalTarget> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().enumerate().flat_map(move |(tab_idx, tab)| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| {
                            tab.terminal_id(pane_id).map(|terminal_id| TerminalTarget {
                                ws_idx,
                                tab_idx,
                                pane_id,
                                terminal_id: terminal_id.to_string(),
                            })
                        })
                })
            })
            .collect()
    }

    pub(crate) fn terminal_target_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<TerminalTarget> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let terminal_id = ws.terminal_id(pane_id)?.to_string();
        Some(TerminalTarget {
            ws_idx,
            tab_idx,
            pane_id,
            terminal_id,
        })
    }

    /// Maps a locally attributed process to the managed agent terminal that owns
    /// its session. Missing runtime state or process inspection intentionally
    /// yields no match so callers retain normal compatibility behavior.
    pub(crate) fn agent_terminal_target_for_peer_pid(
        &self,
        peer_pid: u32,
    ) -> Option<TerminalTarget> {
        let mut matches = self.terminal_targets().into_iter().filter(|target| {
            if !self.target_is_agent(target) {
                return false;
            }

            let Some(child_pid) = self
                .state
                .runtime_for_pane_in_workspace(
                    &self.terminal_runtimes,
                    target.ws_idx,
                    target.pane_id,
                )
                .and_then(crate::terminal::TerminalRuntime::child_pid)
            else {
                return false;
            };

            child_pid == peer_pid
                || crate::platform::session_processes(child_pid)
                    .into_iter()
                    .any(|session_pid| session_pid == peer_pid)
        });
        let target = matches.next()?;
        matches.next().is_none().then_some(target)
    }

    /// Maps a locally attributed process to the one pane whose session it runs in,
    /// agent or not. Used when a process reports under a pane ID it inherited
    /// before its pane was renumbered or moved (smarty-dev#509).
    ///
    /// A process that left its pane's session (a tool runner that calls `setsid`, as Pi's shell
    /// tool does) still descends from the pane's shell, so the lookup walks the peer's
    /// ancestors until one runs in a pane session (smarty-dev#931).
    pub(crate) fn pane_target_for_peer_pid(&self, peer_pid: u32) -> Option<TerminalTarget> {
        let panes: Vec<(TerminalTarget, u32)> = self
            .terminal_targets()
            .into_iter()
            .filter_map(|target| {
                let child_pid = self
                    .state
                    .runtime_for_pane_in_workspace(
                        &self.terminal_runtimes,
                        target.ws_idx,
                        target.pane_id,
                    )
                    .and_then(crate::terminal::TerminalRuntime::child_pid)?;
                Some((target, child_pid))
            })
            .collect();
        find_in_ancestors(peer_pid, crate::platform::parent_process_id, |pid| {
            let mut matches = panes
                .iter()
                .filter(|(_, child_pid)| crate::platform::process_in_pane_session(*child_pid, pid));
            let (target, _) = matches.next()?;
            matches.next().is_none().then(|| target.clone())
        })
    }

    fn terminal_target_candidate(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<TerminalTargetCandidate> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        Some(TerminalTargetCandidate {
            terminal_id: terminal.id.to_string(),
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            cwd: ws.tabs[tab_idx]
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            agent_status: pane_agent_status(terminal.state, pane.seen),
        })
    }
}

/// The deepest ancestor walk: a pane's shell sits a few levels above any tool it runs.
const MAX_ANCESTOR_DEPTH: usize = 32;

/// Returns the first hit of `found` for `pid` or one of its ancestors, nearest first. The walk
/// stops at pid 1 (init adopts orphans, so it and above belong to no pane), at an unknown parent,
/// and after `MAX_ANCESTOR_DEPTH` steps, which also ends a parent cycle.
fn find_in_ancestors<T>(
    pid: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    mut found: impl FnMut(u32) -> Option<T>,
) -> Option<T> {
    let mut current = pid;
    for _ in 0..=MAX_ANCESTOR_DEPTH {
        if current <= 1 {
            return None;
        }
        if let Some(hit) = found(current) {
            return Some(hit);
        }
        current = parent_of(current)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Fake process table: pid -> parent. Pane shells are pid 100 (pane "a") and 200 (pane "b").
    fn walk(table: &HashMap<u32, u32>, pid: u32) -> Option<&'static str> {
        find_in_ancestors(
            pid,
            |pid| table.get(&pid).copied(),
            |pid| match pid {
                100 => Some("a"),
                200 => Some("b"),
                _ => None,
            },
        )
    }

    #[test]
    fn ancestor_walk_maps_grandchild_to_its_pane_shell() {
        // 100 (pane a shell) -> 101 (pi) -> 102 (setsid tool shell) -> 103 (herdr cli)
        let table = HashMap::from([(100, 50), (101, 100), (102, 101), (103, 102), (50, 1)]);
        assert_eq!(walk(&table, 103), Some("a"));
        assert_eq!(walk(&table, 100), Some("a"));
    }

    #[test]
    fn ancestor_walk_ignores_unrelated_process() {
        let table = HashMap::from([(300, 301), (301, 1), (400, 999)]);
        assert_eq!(walk(&table, 300), None);
        // The parent is unknown (exited), so the walk stops.
        assert_eq!(walk(&table, 400), None);
        assert_eq!(walk(&table, 1), None);
        assert_eq!(walk(&table, 0), None);
    }

    #[test]
    fn ancestor_walk_stops_on_cycle_and_depth_cap() {
        let cycle = HashMap::from([(10, 11), (11, 12), (12, 10)]);
        assert_eq!(walk(&cycle, 10), None);

        // A chain whose pane shell sits one step past the cap is not reached; at the cap it is.
        let chain = |len: u32| -> HashMap<u32, u32> {
            let mut table: HashMap<u32, u32> = (0..len).map(|i| (1000 + i, 1000 + i + 1)).collect();
            table.insert(1000 + len, 100);
            table
        };
        let depth = MAX_ANCESTOR_DEPTH as u32;
        assert_eq!(walk(&chain(depth - 1), 1000), Some("a"));
        assert_eq!(walk(&chain(depth), 1000), None);
    }
}
