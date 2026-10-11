use crate::api::schema::{EventData, PluginInvocationContext};
use crate::app::App;

impl App {
    /// Builds a public API invocation context from the caller's trusted
    /// invoking pane (its live peer attribution), never from UI focus or a
    /// request claim. With an invoker, workspace, tab, cwd and agent are
    /// rebuilt from that real pane, and a provided `focused_pane_id` is only
    /// validated: it must resolve (aliases included) to the invoker, else
    /// `invoking_pane_mismatch` before anything else is built. Without an
    /// invoker the invocation is global: any pane claim is ignored and all
    /// pane, workspace and tab fields are absent, including caller-supplied
    /// ones. Caller-own fields (selection, source, correlation, URL, handler)
    /// are kept as the caller's input.
    pub(super) fn merge_plugin_context(
        &self,
        provided: Option<PluginInvocationContext>,
        correlation_id: &str,
        invoker: Option<(usize, crate::layout::PaneId)>,
    ) -> Result<PluginInvocationContext, (&'static str, String)> {
        if let (Some(claim), Some(invoker)) = (
            provided
                .as_ref()
                .and_then(|provided| provided.focused_pane_id.as_deref()),
            invoker,
        ) {
            if self.parse_pane_id(claim) != Some(invoker) {
                return Err((
                    super::super::INVOKING_PANE_MISMATCH,
                    super::super::INVOKING_PANE_MISMATCH_MESSAGE.to_owned(),
                ));
            }
        }
        let mut context = self.invoking_plugin_context(invoker, correlation_id);
        let Some(provided) = provided else {
            return Ok(context);
        };
        context.selected_text = provided.selected_text;
        context.invocation_source = provided.invocation_source.or(context.invocation_source);
        context.correlation_id = provided.correlation_id.or(context.correlation_id);
        context.clicked_url = provided.clicked_url;
        context.link_handler_id = provided.link_handler_id;
        Ok(context)
    }

    /// Context for an invocation that may have no invoking pane (a global
    /// keybinding, CLI call or startup hook). It never falls back to UI focus.
    pub(super) fn invoking_plugin_context(
        &self,
        invoking_pane: Option<(usize, crate::layout::PaneId)>,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        match invoking_pane {
            Some((ws_idx, pane_id))
                if self
                    .state
                    .workspaces
                    .get(ws_idx)
                    .and_then(|ws| ws.find_tab_index_for_pane(pane_id))
                    .is_some() =>
            {
                self.plugin_context_for_pane(ws_idx, pane_id, correlation_id)
            }
            _ => empty_plugin_context(correlation_id),
        }
    }

    pub(super) fn current_plugin_context(&self, correlation_id: &str) -> PluginInvocationContext {
        let Some(ws_idx) = self.state.active else {
            return empty_plugin_context(correlation_id);
        };
        self.plugin_context_for_workspace(ws_idx, correlation_id)
    }

    pub(super) fn plugin_context_for_event(
        &self,
        event: &crate::api::schema::EventEnvelope,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        match &event.data {
            EventData::WorkspaceCreated { workspace }
            | EventData::WorkspaceUpdated { workspace }
            | EventData::WorkspaceMetadataUpdated { workspace }
            | EventData::WorktreeCreated { workspace, .. }
            | EventData::WorktreeOpened { workspace, .. } => {
                self.plugin_context_for_workspace_info(workspace, correlation_id)
            }
            EventData::WorkspaceClosed {
                workspace_id,
                workspace,
            } => workspace
                .as_ref()
                .map(|workspace| self.plugin_context_for_workspace_info(workspace, correlation_id))
                .unwrap_or_else(|| {
                    self.plugin_context_for_workspace_id(workspace_id, correlation_id)
                        .unwrap_or_else(|| {
                            let mut context = empty_plugin_context(correlation_id);
                            context.workspace_id = Some(workspace_id.clone());
                            context
                        })
                }),
            EventData::WorkspaceReordered { workspace_ids, .. } => workspace_ids
                .first()
                .and_then(|workspace_id| {
                    self.plugin_context_for_workspace_id(workspace_id, correlation_id)
                })
                .unwrap_or_else(|| empty_plugin_context(correlation_id)),
            EventData::WorkspaceRenamed { workspace_id, .. }
            | EventData::WorkspaceMoved { workspace_id, .. }
            | EventData::WorkspaceFocused { workspace_id } => self
                .plugin_context_for_workspace_id(workspace_id, correlation_id)
                .unwrap_or_else(|| {
                    let mut context = empty_plugin_context(correlation_id);
                    context.workspace_id = Some(workspace_id.clone());
                    context
                }),
            EventData::WorktreeRemoved {
                workspace_id,
                workspace,
                worktree,
                ..
            } => workspace
                .as_ref()
                .map(|workspace| {
                    self.plugin_context_for_workspace_snapshot(workspace, correlation_id)
                })
                .or_else(|| self.plugin_context_for_workspace_id(workspace_id, correlation_id))
                .unwrap_or_else(|| {
                    let mut context = empty_plugin_context(correlation_id);
                    context.workspace_id = Some(workspace_id.clone());
                    context.workspace_label = Some(worktree.label.clone());
                    context.workspace_cwd = Some(worktree.path.clone());
                    context
                }),
            EventData::TabCreated { tab } => self.plugin_context_for_tab_info(tab, correlation_id),
            EventData::TabClosed {
                tab_id,
                workspace_id,
            } => {
                let mut context = empty_plugin_context(correlation_id);
                context.workspace_id = Some(workspace_id.clone());
                context.tab_id = Some(tab_id.clone());
                context
            }
            EventData::TabRenamed {
                tab_id,
                workspace_id,
                ..
            }
            | EventData::TabMoved {
                tab_id,
                workspace_id,
                ..
            }
            | EventData::TabFocused {
                tab_id,
                workspace_id,
            } => self
                .plugin_context_for_tab_id(tab_id, correlation_id)
                .or_else(|| self.plugin_context_for_workspace_id(workspace_id, correlation_id))
                .unwrap_or_else(|| {
                    let mut context = empty_plugin_context(correlation_id);
                    context.workspace_id = Some(workspace_id.clone());
                    context.tab_id = Some(tab_id.clone());
                    context
                }),
            EventData::LayoutUpdated { layout } => self
                .plugin_context_for_tab_id(&layout.tab_id, correlation_id)
                .or_else(|| {
                    self.plugin_context_for_workspace_id(&layout.workspace_id, correlation_id)
                })
                .unwrap_or_else(|| {
                    let mut context = empty_plugin_context(correlation_id);
                    context.workspace_id = Some(layout.workspace_id.clone());
                    context.tab_id = Some(layout.tab_id.clone());
                    context
                }),
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                self.plugin_context_for_pane_info(pane, correlation_id)
            }
            EventData::PaneMoved { pane, .. } => {
                self.plugin_context_for_pane_info(pane.as_ref(), correlation_id)
            }
            EventData::PaneClosed {
                pane_id,
                workspace_id,
            } => {
                let mut context = empty_plugin_context(correlation_id);
                context.workspace_id = Some(workspace_id.clone());
                context.focused_pane_id = Some(pane_id.clone());
                context
            }
            EventData::PaneFocused {
                pane_id,
                workspace_id,
            }
            | EventData::PaneOutputChanged {
                pane_id,
                workspace_id,
                ..
            }
            | EventData::PaneExited {
                pane_id,
                workspace_id,
            }
            | EventData::PaneAgentDetected {
                pane_id,
                workspace_id,
                ..
            }
            | EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                ..
            } => self
                .plugin_context_for_public_pane_id(pane_id, correlation_id)
                .or_else(|| self.plugin_context_for_workspace_id(workspace_id, correlation_id))
                .unwrap_or_else(|| {
                    let mut context = empty_plugin_context(correlation_id);
                    context.workspace_id = Some(workspace_id.clone());
                    context.focused_pane_id = Some(pane_id.clone());
                    context
                }),
        }
    }

    fn plugin_context_for_workspace_id(
        &self,
        workspace_id: &str,
        correlation_id: &str,
    ) -> Option<PluginInvocationContext> {
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(idx, _)| (self.public_workspace_id(idx) == workspace_id).then_some(idx))?;
        Some(self.plugin_context_for_workspace(ws_idx, correlation_id))
    }

    fn plugin_context_for_workspace_info(
        &self,
        workspace: &crate::api::schema::WorkspaceInfo,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        self.plugin_context_for_workspace_id(&workspace.workspace_id, correlation_id)
            .unwrap_or_else(|| {
                self.plugin_context_for_workspace_snapshot(workspace, correlation_id)
            })
    }

    fn plugin_context_for_workspace_snapshot(
        &self,
        workspace: &crate::api::schema::WorkspaceInfo,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        let mut context = empty_plugin_context(correlation_id);
        context.workspace_id = Some(workspace.workspace_id.clone());
        context.workspace_label = Some(workspace.label.clone());
        context.workspace_cwd = workspace
            .worktree
            .as_ref()
            .map(|worktree| worktree.checkout_path.clone());
        context.worktree = workspace.worktree.clone();
        context.tab_id = Some(workspace.active_tab_id.clone());
        context
    }

    fn plugin_context_for_tab_id(
        &self,
        tab_id: &str,
        correlation_id: &str,
    ) -> Option<PluginInvocationContext> {
        let (ws_idx, tab_idx) = self.parse_tab_id(tab_id)?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let workspace = self.workspace_info(ws_idx);
        let tab = ws.tabs.get(tab_idx)?;
        let pane_id = tab.layout.focused();
        let focused_pane = self.pane_info(ws_idx, pane_id);
        Some(self.plugin_context_from_parts(
            ws_idx,
            workspace,
            self.public_tab_id(ws_idx, tab_idx),
            ws.tab_display_name(tab_idx),
            focused_pane,
            correlation_id,
        ))
    }

    fn plugin_context_for_tab_info(
        &self,
        tab: &crate::api::schema::TabInfo,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        self.plugin_context_for_tab_id(&tab.tab_id, correlation_id)
            .or_else(|| self.plugin_context_for_workspace_id(&tab.workspace_id, correlation_id))
            .unwrap_or_else(|| {
                let mut context = empty_plugin_context(correlation_id);
                context.workspace_id = Some(tab.workspace_id.clone());
                context.tab_id = Some(tab.tab_id.clone());
                context.tab_label = Some(tab.label.clone());
                context
            })
    }

    pub(super) fn plugin_context_for_public_pane_id(
        &self,
        pane_id: &str,
        correlation_id: &str,
    ) -> Option<PluginInvocationContext> {
        let (ws_idx, pane_id) = self.parse_pane_id(pane_id)?;
        Some(self.plugin_context_for_pane(ws_idx, pane_id, correlation_id))
    }

    fn plugin_context_for_pane_info(
        &self,
        pane: &crate::api::schema::PaneInfo,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        self.plugin_context_for_public_pane_id(&pane.pane_id, correlation_id)
            .or_else(|| self.plugin_context_for_workspace_id(&pane.workspace_id, correlation_id))
            .unwrap_or_else(|| {
                let mut context = empty_plugin_context(correlation_id);
                context.workspace_id = Some(pane.workspace_id.clone());
                context.tab_id = Some(pane.tab_id.clone());
                context.focused_pane_id = Some(pane.pane_id.clone());
                context.focused_pane_cwd = pane.cwd.clone();
                context.focused_pane_agent = pane.agent.clone();
                context.focused_pane_status = Some(pane.agent_status);
                context
            })
    }

    pub(super) fn plugin_context_for_workspace(
        &self,
        ws_idx: usize,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return empty_plugin_context(correlation_id);
        };
        let workspace = self.workspace_info(ws_idx);
        let tab_idx = ws.active_tab_index();
        let tab_id = self.public_tab_id(ws_idx, tab_idx);
        let tab_label = ws.tab_display_name(tab_idx);
        let focused_pane = ws
            .focused_pane_id()
            .and_then(|pane_id| self.pane_info(ws_idx, pane_id));
        self.plugin_context_from_parts(
            ws_idx,
            workspace,
            tab_id,
            tab_label,
            focused_pane,
            correlation_id,
        )
    }

    pub(super) fn plugin_context_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        let ws = &self.state.workspaces[ws_idx];
        let workspace = self.workspace_info(ws_idx);
        let tab_idx = ws
            .find_tab_index_for_pane(pane_id)
            .unwrap_or_else(|| ws.active_tab_index());
        let tab_id = self.public_tab_id(ws_idx, tab_idx);
        let tab_label = ws.tab_display_name(tab_idx);
        let focused_pane = self.pane_info(ws_idx, pane_id);
        self.plugin_context_from_parts(
            ws_idx,
            workspace,
            tab_id,
            tab_label,
            focused_pane,
            correlation_id,
        )
    }

    fn plugin_context_from_parts(
        &self,
        ws_idx: usize,
        workspace: crate::api::schema::WorkspaceInfo,
        tab_id: Option<String>,
        tab_label: Option<String>,
        focused_pane: Option<crate::api::schema::PaneInfo>,
        correlation_id: &str,
    ) -> PluginInvocationContext {
        let workspace_cwd = focused_pane
            .as_ref()
            .and_then(|pane| pane.cwd.clone())
            .or_else(|| Some(self.default_cwd_for_workspace(ws_idx).display().to_string()));
        PluginInvocationContext {
            workspace_id: Some(workspace.workspace_id),
            workspace_label: Some(workspace.label),
            workspace_cwd,
            worktree: workspace.worktree,
            tab_id,
            tab_label,
            focused_pane_id: focused_pane.as_ref().map(|pane| pane.pane_id.clone()),
            focused_pane_cwd: focused_pane.as_ref().and_then(|pane| pane.cwd.clone()),
            focused_pane_agent: focused_pane.as_ref().and_then(|pane| pane.agent.clone()),
            focused_pane_status: focused_pane.as_ref().map(|pane| pane.agent_status),
            // Selection is client presentation state. Client keybindings provide
            // revision-validated coordinates; API callers can provide explicit context.
            selected_text: None,
            invocation_source: Some("api".to_string()),
            correlation_id: Some(correlation_id.to_string()),
            clicked_url: None,
            link_handler_id: None,
        }
    }

    fn default_cwd_for_workspace(&self, ws_idx: usize) -> std::path::PathBuf {
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| {
                ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()))
    }
}

fn empty_plugin_context(correlation_id: &str) -> PluginInvocationContext {
    PluginInvocationContext {
        workspace_id: None,
        workspace_label: None,
        workspace_cwd: None,
        worktree: None,
        tab_id: None,
        tab_label: None,
        focused_pane_id: None,
        focused_pane_cwd: None,
        focused_pane_agent: None,
        focused_pane_status: None,
        selected_text: None,
        invocation_source: Some("api".to_string()),
        correlation_id: Some(correlation_id.to_string()),
        clicked_url: None,
        link_handler_id: None,
    }
}
