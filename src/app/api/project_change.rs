//! Server-owned project checks for topology commands (never render/refresh).
use std::collections::HashMap;
use std::path::PathBuf;

use crate::app::App;
use crate::layout::PaneId;
use crate::terminal::TerminalState;

pub(super) struct ProjectTab {
    pub root: PaneId,
    pub panes: Vec<PaneId>,
}

pub(super) struct ProjectWorkspace {
    pub id: String,
    pub tabs: Vec<ProjectTab>,
    pub checkout_path: Option<PathBuf>,
    pub tokens: HashMap<String, String>,
}

impl ProjectWorkspace {
    pub fn remove_pane(&mut self, pane: PaneId) {
        for tab in &mut self.tabs {
            tab.panes.retain(|id| *id != pane);
            if let (true, Some(root)) = (tab.root == pane, tab.panes.first()) {
                tab.root = *root;
            }
        }
        self.tabs.retain(|tab| !tab.panes.is_empty());
    }

    fn complete_metadata(&self) -> bool {
        ["smarty_org_id", "smarty_project_id"].iter().all(|key| {
            self.tokens
                .get(*key)
                .is_some_and(|value| !value.trim().is_empty())
        })
    }

    fn first_pi(&self, terminals: &HashMap<PaneId, &TerminalState>) -> Option<PaneId> {
        // Exactly collect_agent_infos order: workspace, tab, layout leaf order.
        // AgentInfo.agent uses effective_agent_label; a Pi label also implies
        // is_agent_terminal. Do not scan past a first Pi with an unknown cwd.
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .copied()
            .find(|pane| {
                terminals
                    .get(pane)
                    .is_some_and(|terminal| terminal.effective_agent_label() == Some("pi"))
            })
    }
}

struct SmartyProject {
    org: String,
    project: String,
    name: String,
}

struct ProjectIdentity {
    smarty: Option<SmartyProject>,
    directory: Option<PathBuf>,
}

impl ProjectIdentity {
    /// Metadata only when *both* workspace pairs are complete; otherwise
    /// canonical checkout/first-Pi foreground cwd on both sides. Linked
    /// worktrees are different fallback projects even with a common git dir.
    fn compare(&self, target: &Self) -> (bool, String, String) {
        if let (Some(source), Some(target)) = (&self.smarty, &target.smarty) {
            return (
                source.org == target.org && source.project == target.project,
                source.name.clone(),
                target.name.clone(),
            );
        }
        let name = |path: &Option<PathBuf>| {
            path.as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown project".into())
        };
        (
            self.directory.is_some() && self.directory == target.directory,
            name(&self.directory),
            name(&target.directory),
        )
    }
}

pub(super) struct ProjectChange {
    pane: String,
    session: String,
    kind: crate::agent_resume::AgentSessionRefKind,
    source: String,
    target: String,
}

impl ProjectChange {
    fn description(&self) -> String {
        // Paths can contain newlines; one intentional command is one log line.
        let single_line = |value: &str| value.replace('\r', "\\r").replace('\n', "\\n");
        format!(
            "pane {} agent session {:?} {} would change project from {} to {}",
            single_line(&self.pane),
            self.kind,
            single_line(&self.session),
            single_line(&self.source),
            single_line(&self.target)
        )
    }
}

impl App {
    pub(super) fn project_topology(&self) -> Vec<ProjectWorkspace> {
        self.state
            .workspaces
            .iter()
            .map(|ws| ProjectWorkspace {
                id: ws.id.clone(),
                tabs: ws
                    .tabs
                    .iter()
                    .map(|tab| ProjectTab {
                        root: tab.root_pane,
                        panes: tab.layout.pane_ids(),
                    })
                    .collect(),
                checkout_path: ws.worktree_space().map(|space| space.checkout_path.clone()),
                tokens: ws.metadata_tokens.values(),
            })
            .collect()
    }

    fn topology_project(
        &self,
        ws: &ProjectWorkspace,
        fallback: bool,
        first_pi: Option<PaneId>,
        terminals: &HashMap<PaneId, &TerminalState>,
        foreground_cwds: &mut HashMap<PaneId, Option<PathBuf>>,
    ) -> ProjectIdentity {
        // Hosting workspace tokens only, never pane tokens or client settings.
        let token = |key: &str| ws.tokens.get(key).filter(|value| !value.trim().is_empty());
        let smarty =
            token("smarty_org_id")
                .zip(token("smarty_project_id"))
                .map(|(org, project)| SmartyProject {
                    org: org.clone(),
                    project: project.clone(),
                    name: format!(
                        "{} ({org}/{project})",
                        token("smarty_project_label").unwrap_or(project)
                    ),
                });
        if !fallback {
            return ProjectIdentity {
                smarty,
                directory: None,
            };
        }
        let cwd = ws.checkout_path.clone().or_else(|| {
            let pane = first_pi?;
            foreground_cwds
                .entry(pane)
                .or_insert_with(|| {
                    let terminal = terminals.get(&pane)?;
                    self.terminal_runtimes.get(&terminal.id)?.foreground_cwd()
                })
                .clone()
        });
        let directory = cwd
            .filter(|cwd| cwd.is_absolute())
            .map(|cwd| crate::worktree::canonical_or_original(&cwd));
        ProjectIdentity { smarty, directory }
    }

    /// Compare every surviving session against the complete projected agent
    /// order. Removing/reordering the first Pi can reclassify other sessions;
    /// inserting before the target's first Pi can reclassify target sessions.
    /// Return audit details; log only after the complete mutation succeeds.
    pub(super) fn precheck_project_change(
        &self,
        after: &[ProjectWorkspace],
        allow_project_change: bool,
        checked_method: &str,
    ) -> Result<Vec<ProjectChange>, String> {
        let before = self.project_topology();
        let mut terminals = HashMap::new();
        for ws in &self.state.workspaces {
            for tab in &ws.tabs {
                for pane in tab.layout.pane_ids() {
                    if let Some(terminal) = tab
                        .terminal_id(pane)
                        .and_then(|id| self.state.terminals.get(id))
                    {
                        terminals.insert(pane, terminal);
                    }
                }
            }
        }
        let before_first: Vec<_> = before.iter().map(|ws| ws.first_pi(&terminals)).collect();
        let after_first: Vec<_> = after.iter().map(|ws| ws.first_pi(&terminals)).collect();
        let mut destinations = HashMap::new();
        for (index, ws) in after.iter().enumerate() {
            for tab in &ws.tabs {
                for pane in &tab.panes {
                    destinations.insert(*pane, index);
                }
            }
        }
        let mut before_projects = HashMap::new();
        let mut after_projects = HashMap::new();
        let mut foreground_cwds = HashMap::new();
        let mut changes = Vec::new();
        // Deterministic workspace/tab/leaf traversal, no session-file scan.
        for (ws_idx, ws) in before.iter().enumerate() {
            for tab in &ws.tabs {
                for pane in &tab.panes {
                    let Some(target_idx) = destinations.get(pane).copied() else {
                        continue;
                    };
                    let target_ws = &after[target_idx];
                    if target_ws.id == ws.id && before_first[ws_idx] == after_first[target_idx] {
                        // No association/first-Pi change: unrelated unknown projects
                        // and root-shell cwd changes must not block this command.
                        continue;
                    }
                    let Some(session) = terminals
                        .get(pane)
                        .and_then(|terminal| terminal.agent_session_reference())
                    else {
                        continue;
                    };
                    let fallback = !(ws.complete_metadata() && target_ws.complete_metadata());
                    let source = before_projects
                        .entry((ws_idx, fallback))
                        .or_insert_with(|| {
                            self.topology_project(
                                ws,
                                fallback,
                                before_first[ws_idx],
                                &terminals,
                                &mut foreground_cwds,
                            )
                        });
                    let target =
                        after_projects
                            .entry((target_idx, fallback))
                            .or_insert_with(|| {
                                self.topology_project(
                                    target_ws,
                                    fallback,
                                    after_first[target_idx],
                                    &terminals,
                                    &mut foreground_cwds,
                                )
                            });
                    let (same, source, target) = source.compare(target);
                    if !same {
                        changes.push(ProjectChange {
                            pane: self
                                .public_pane_id(ws_idx, *pane)
                                .unwrap_or_else(|| format!("{}:{pane:?}", ws.id)),
                            session: session.value.clone(),
                            kind: session.kind,
                            source,
                            target,
                        });
                    }
                }
            }
        }
        if !allow_project_change && !changes.is_empty() {
            let hint = if checked_method == "pane.move_project_checked" {
                "--allow-project-change (API: pane.move_project_checked with allow_project_change=true)".to_string()
            } else {
                format!("{checked_method} with allow_project_change=true")
            };
            return Err(format!(
                "project change refused: {}; use {hint} to proceed intentionally",
                changes
                    .iter()
                    .map(ProjectChange::description)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        Ok(changes)
    }

    pub(super) fn log_project_changes(changes: &[ProjectChange]) {
        Self::log_project_changes_with_context(changes, "topology command");
    }

    pub(super) fn log_project_changes_with_context(changes: &[ProjectChange], context: &str) {
        if !changes.is_empty() {
            tracing::info!(context, changes = %changes.iter().map(ProjectChange::description)
                .collect::<Vec<_>>().join("; "), "intentional project change allowed");
        }
    }
}
