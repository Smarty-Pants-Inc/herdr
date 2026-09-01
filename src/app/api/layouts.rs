use std::path::PathBuf;

use ratatui::layout::Direction;

use crate::api::schema::{
    ErrorBody, EventData, EventEnvelope, EventKind, LayoutApplyParams, LayoutDescription,
    LayoutExportParams, LayoutIdempotentParams, LayoutNode, LayoutPane, LayoutSetSplitRatioParams,
    ResponseResult, SplitDirection,
};
use crate::app::{App, Mode};
use crate::layout::{Node, PaneId};
use crate::workspace::NewPane;

use super::responses::{encode_error, encode_error_body, encode_success};

const MAX_LAYOUT_PANES: usize = 24;
const MAX_LAYOUT_DEPTH: usize = 16;
const IDEMPOTENCY_NO_EFFECT_MESSAGE: &str = "no layout.apply effect exists for idempotency_key";
const IDEMPOTENCY_PENDING_MESSAGE: &str =
    "layout.apply outcome is ambiguous and remains pending for reconciliation";

#[derive(Clone, Copy)]
struct LayoutApplyTarget {
    ws_idx: usize,
    replace_target: Option<(usize, usize)>,
}

fn layout_apply_error(code: impl Into<String>, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code.into(),
        message: message.into(),
    }
}

fn encode_layout_idempotency_store_error(id: String, error: String) -> String {
    let code = if error.contains("capacity") || error.contains("size limit") {
        "idempotency_capacity"
    } else {
        "idempotency_persist_failed"
    };
    encode_error(
        id,
        code,
        format!("failed to persist layout idempotency receipt: {error}"),
    )
}

impl App {
    pub(super) fn handle_layout_export(
        &mut self,
        id: String,
        params: LayoutExportParams,
    ) -> String {
        let Some((ws_idx, tab_idx)) = self.resolve_layout_export_target(&params) else {
            return encode_error(id, "layout_not_found", "layout target not found");
        };
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return encode_error(id, "layout_not_found", "layout unavailable");
        };

        encode_success(id, ResponseResult::LayoutExport { layout })
    }

    pub(super) fn handle_layout_apply(&mut self, id: String, params: LayoutApplyParams) -> String {
        if self.layout_apply_quarantined {
            return encode_error(id, "server_unavailable", "server is shutting down");
        }
        let target = match self.prepare_layout_apply(&params) {
            Ok(target) => target,
            Err(error) => return encode_error_body(id, error),
        };
        match self.apply_layout_once(&params, target, None) {
            Ok(layout) => {
                self.schedule_session_save();
                encode_success(id, ResponseResult::LayoutApply { layout })
            }
            Err(error) => encode_error_body(id, error),
        }
    }

    pub(super) fn handle_layout_apply_idempotent(
        &mut self,
        id: String,
        params: LayoutIdempotentParams,
        reconcile_only: bool,
    ) -> String {
        if self.layout_apply_quarantined {
            return encode_error(id, "server_unavailable", "server is shutting down");
        }
        if self.session_persistence_blocked {
            return encode_error(
                id,
                "session_snapshot_unsupported",
                "session persistence is blocked by an unsupported snapshot",
            );
        }
        if self.no_session {
            return encode_error(
                id,
                "unsupported_in_no_session",
                "idempotent layout methods require session persistence",
            );
        }

        let LayoutIdempotentParams {
            idempotency_key,
            layout,
        } = params;
        if let Err(message) = self.validate_layout_idempotency_key(&idempotency_key) {
            return encode_error(id, "invalid_request", message);
        }
        let request_digest = match self.layout_apply_request_digest(&layout) {
            Ok(digest) => digest,
            Err(message) => return encode_error(id, "invalid_request", message),
        };

        let receipt = match self.layout_apply_receipt(&idempotency_key) {
            Ok(receipt) => receipt,
            Err(err) => {
                return encode_error(
                    id,
                    "idempotency_unavailable",
                    format!("layout idempotency ledger is unavailable: {err}"),
                )
            }
        };
        if let Some(receipt) = receipt {
            if receipt.request_digest != request_digest {
                return encode_error(
                    id,
                    "idempotency_conflict",
                    "idempotency_key was already used with a different layout request",
                );
            }
            return self.replay_layout_apply_receipt(id, idempotency_key, receipt);
        }

        let effect_nonce = match self.new_layout_effect_nonce() {
            Ok(nonce) => nonce,
            Err(err) => return encode_error(id, "idempotency_unavailable", err),
        };
        if let Err(err) = self.save_layout_apply_session_snapshot_now() {
            return encode_error(
                id,
                "session_persist_failed",
                format!("failed to bind the idempotency epoch to the current session: {err}"),
            );
        }
        if reconcile_only {
            let receipt = crate::persist::LayoutApplyReceipt {
                session_epoch: self.layout_apply_epoch.clone(),
                request_digest,
                effect_nonce,
                outcome: crate::persist::LayoutApplyOutcome::Cancelled,
            };
            if let Err(err) = self.store_layout_apply_receipt(idempotency_key, receipt) {
                return encode_layout_idempotency_store_error(id, err);
            }
            return encode_error(id, "idempotency_no_effect", IDEMPOTENCY_NO_EFFECT_MESSAGE);
        }

        let target = match self.prepare_layout_apply(&layout) {
            Ok(target) => target,
            Err(error) => {
                let receipt = crate::persist::LayoutApplyReceipt {
                    session_epoch: self.layout_apply_epoch.clone(),
                    request_digest,
                    effect_nonce,
                    outcome: crate::persist::LayoutApplyOutcome::NoEffect,
                };
                if let Err(err) = self.store_layout_apply_receipt(idempotency_key, receipt) {
                    return encode_layout_idempotency_store_error(id, err);
                }
                return encode_error_body(id, error);
            }
        };
        let expected_tab_id = self.expected_layout_apply_tab_id(target);
        let pending = crate::persist::LayoutApplyReceipt {
            session_epoch: self.layout_apply_epoch.clone(),
            request_digest,
            effect_nonce: effect_nonce.clone(),
            outcome: crate::persist::LayoutApplyOutcome::pending(expected_tab_id),
        };
        if let Err(err) = self.store_layout_apply_receipt(idempotency_key.clone(), pending.clone())
        {
            return encode_layout_idempotency_store_error(id, err);
        }

        let layout_result = match self.apply_layout_once(&layout, target, Some(&effect_nonce)) {
            Ok(layout_result) => layout_result,
            Err(error) => {
                return encode_error(
                    id,
                    "idempotency_pending",
                    format!(
                    "{IDEMPOTENCY_PENDING_MESSAGE}: layout effect may have started before {}: {}",
                    error.code, error.message
                ),
                )
            }
        };

        if let Err(err) = self.save_layout_apply_session_snapshot_now() {
            let message = self.quarantine_layout_apply_after_effect(format!(
                "failed to persist idempotent layout session snapshot: {err}"
            ));
            return encode_error(id, "session_persist_failed", message);
        }
        let committed = crate::persist::LayoutApplyReceipt {
            outcome: crate::persist::LayoutApplyOutcome::Committed {
                tab_id: layout_result.tab_id.clone(),
            },
            ..pending
        };
        if let Err(err) = self.store_layout_apply_receipt(idempotency_key, committed) {
            let message = self.quarantine_layout_apply_after_effect(format!(
                "failed to commit idempotent layout receipt: {err}"
            ));
            return encode_error(id, "idempotency_persist_failed", message);
        }

        encode_success(
            id,
            ResponseResult::LayoutApply {
                layout: layout_result,
            },
        )
    }

    fn replay_layout_apply_receipt(
        &mut self,
        id: String,
        idempotency_key: String,
        receipt: crate::persist::LayoutApplyReceipt,
    ) -> String {
        match &receipt.outcome {
            crate::persist::LayoutApplyOutcome::Committed { .. } => {
                match self.replay_committed_layout_apply_receipt(&receipt) {
                    super::layout_idempotency::PendingResolution::Committed(layout) => {
                        let layout = *layout;
                        encode_success(id, ResponseResult::LayoutApply { layout })
                    }
                    super::layout_idempotency::PendingResolution::Ambiguous(reason) => {
                        encode_error(
                            id,
                            "idempotency_pending",
                            format!("{IDEMPOTENCY_PENDING_MESSAGE}: {reason}"),
                        )
                    }
                }
            }
            crate::persist::LayoutApplyOutcome::Cancelled
            | crate::persist::LayoutApplyOutcome::NoEffect => {
                encode_error(id, "idempotency_no_effect", IDEMPOTENCY_NO_EFFECT_MESSAGE)
            }
            crate::persist::LayoutApplyOutcome::Pending { .. } => {
                match self.reconcile_layout_apply_receipt(&receipt) {
                    super::layout_idempotency::PendingResolution::Committed(layout) => {
                        let layout = *layout;
                        if let Err(err) = self.save_layout_apply_session_snapshot_now() {
                            let message = self.quarantine_layout_apply_after_effect(format!(
                                "failed to persist a reconciled idempotent layout session: {err}"
                            ));
                            return encode_error(id, "session_persist_failed", message);
                        }
                        let committed = crate::persist::LayoutApplyReceipt {
                            outcome: crate::persist::LayoutApplyOutcome::Committed {
                                tab_id: layout.tab_id.clone(),
                            },
                            ..receipt
                        };
                        if let Err(err) =
                            self.store_layout_apply_receipt(idempotency_key, committed)
                        {
                            let message = self.quarantine_layout_apply_after_effect(format!(
                                "failed to commit reconciled layout receipt: {err}"
                            ));
                            return encode_error(id, "idempotency_persist_failed", message);
                        }
                        encode_success(id, ResponseResult::LayoutApply { layout })
                    }
                    super::layout_idempotency::PendingResolution::Ambiguous(reason) => {
                        encode_error(
                            id,
                            "idempotency_pending",
                            format!("{IDEMPOTENCY_PENDING_MESSAGE}: {reason}"),
                        )
                    }
                }
            }
        }
    }

    fn prepare_layout_apply(
        &self,
        params: &LayoutApplyParams,
    ) -> Result<LayoutApplyTarget, ErrorBody> {
        let replace_target = match params.tab_id.as_deref() {
            Some(tab_id) => match self.parse_tab_id(tab_id) {
                Some(target) => Some(target),
                None => {
                    return Err(layout_apply_error(
                        "tab_not_found",
                        format!("tab {tab_id} not found"),
                    ))
                }
            },
            None => None,
        };
        if replace_target.is_some() && params.workspace_id.is_some() {
            return Err(layout_apply_error(
                "invalid_target",
                "use either tab_id or workspace_id, not both",
            ));
        }
        let ws_idx = if let Some((ws_idx, _)) = replace_target {
            ws_idx
        } else if let Some(workspace_id) = params.workspace_id.as_deref() {
            self.parse_workspace_id(workspace_id).ok_or_else(|| {
                layout_apply_error(
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                )
            })?
        } else if let Some(active) = self.state.active {
            active
        } else {
            return Err(layout_apply_error(
                "workspace_not_found",
                "no active workspace",
            ));
        };
        validate_layout_tree(&params.root)
            .map_err(|message| layout_apply_error("invalid_layout", message))?;
        Ok(LayoutApplyTarget {
            ws_idx,
            replace_target,
        })
    }

    fn expected_layout_apply_tab_id(&self, target: LayoutApplyTarget) -> String {
        let workspace = &self.state.workspaces[target.ws_idx];
        crate::workspace::public_tab_id_for_number(&workspace.id, workspace.next_public_tab_number)
    }

    fn apply_layout_once(
        &mut self,
        params: &LayoutApplyParams,
        target: LayoutApplyTarget,
        effect_nonce: Option<&str>,
    ) -> Result<LayoutDescription, ErrorBody> {
        let ws_idx = target.ws_idx;
        let replace_target = target.replace_target;

        let replacement_label = params.tab_label.clone().or_else(|| {
            let (_, tab_idx) = replace_target?;
            self.state
                .workspaces
                .get(ws_idx)?
                .tabs
                .get(tab_idx)?
                .custom_name
                .clone()
        });
        let replace_was_active = replace_target.is_some_and(|(target_ws, target_tab)| {
            self.state.active == Some(target_ws)
                && self
                    .state
                    .workspaces
                    .get(target_ws)
                    .is_some_and(|ws| ws.active_tab_index() == target_tab)
        });
        let root_leaf = first_layout_leaf(&params.root);
        let first_cwd = self.layout_root_cwd(ws_idx, replace_target, root_leaf);
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let extra_env = super::env::normalize_launch_env(root_leaf.env.clone())
            .map_err(|(code, message)| layout_apply_error(code, message))?;
        let command = layout_command(root_leaf)
            .map_err(|message| layout_apply_error("invalid_layout", message))?;

        let created = {
            let ws =
                self.state.workspaces.get_mut(ws_idx).ok_or_else(|| {
                    layout_apply_error("workspace_not_found", "workspace not found")
                })?;
            if let Some(argv) = command.as_deref() {
                ws.create_tab_argv_command_on(
                    rows,
                    cols,
                    first_cwd,
                    &root_leaf.execution_target,
                    argv,
                    extra_env,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                )
            } else {
                ws.create_tab_on(
                    rows,
                    cols,
                    first_cwd,
                    &root_leaf.execution_target,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                )
            }
        };

        let (new_tab_idx, terminal, runtime) =
            created.map_err(|err| layout_apply_error("layout_apply_failed", err.to_string()))?;
        self.state.workspaces[ws_idx].tabs[new_tab_idx].layout_effect_nonce =
            effect_nonce.map(str::to_owned);
        let new_root_pane = self.state.workspaces[ws_idx].tabs[new_tab_idx].root_pane;
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.remove_alias_shadowed_by_new_pane(new_root_pane);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        if let Some(label) = replacement_label {
            self.state.workspaces[ws_idx].tabs[new_tab_idx].set_custom_name(label);
        }
        self.apply_layout_pane_label(ws_idx, new_root_pane, root_leaf);

        if let Err(message) = self.apply_layout_node_to_pane(ws_idx, new_root_pane, &params.root) {
            self.rollback_layout_tab(ws_idx, new_root_pane);
            return Err(layout_apply_error("layout_apply_failed", message));
        }

        if let Some((target_ws_idx, target_tab_idx)) = replace_target {
            let closed_tab_id = self
                .public_tab_id(target_ws_idx, target_tab_idx)
                .unwrap_or_else(|| {
                    crate::workspace::public_tab_id_for_number(
                        &self.public_workspace_id(target_ws_idx),
                        target_tab_idx + 1,
                    )
                });
            let terminal_ids = self
                .state
                .terminal_ids_for_tab(target_ws_idx, target_tab_idx);
            let plugin_pane_ids = self.state.pane_ids_for_tab(target_ws_idx, target_tab_idx);
            let ws = self
                .state
                .workspaces
                .get_mut(target_ws_idx)
                .ok_or_else(|| layout_apply_error("tab_not_found", "tab not found"))?;
            if ws.close_tab(target_tab_idx) {
                self.state.remove_plugin_pane_records(plugin_pane_ids);
                self.state.remove_unattached_terminal_ids(terminal_ids);
                self.shutdown_detached_terminal_runtimes();
                self.emit_event(EventEnvelope {
                    event: EventKind::TabClosed,
                    data: EventData::TabClosed {
                        tab_id: closed_tab_id,
                        workspace_id: self.public_workspace_id(target_ws_idx),
                    },
                });
            }
        }

        let new_tab_idx = self.state.workspaces[ws_idx]
            .tabs
            .iter()
            .position(|tab| tab.root_pane == new_root_pane)
            .ok_or_else(|| {
                layout_apply_error("layout_apply_failed", "new layout tab disappeared")
            })?;

        if params.focus || replace_was_active {
            self.state.switch_workspace_tab(ws_idx, new_tab_idx);
            self.state.mode = Mode::Terminal;
        }
        if let Some(tab) = self.tab_info(ws_idx, new_tab_idx) {
            self.emit_event(EventEnvelope {
                event: EventKind::TabCreated,
                data: EventData::TabCreated { tab },
            });
        }
        for pane_id in self.state.workspaces[ws_idx].tabs[new_tab_idx]
            .layout
            .pane_ids()
        {
            if let Some(pane) = self.pane_info(ws_idx, pane_id) {
                self.emit_event(EventEnvelope {
                    event: EventKind::PaneCreated,
                    data: EventData::PaneCreated { pane },
                });
            }
        }
        self.emit_layout_updated_event(ws_idx, new_tab_idx);

        self.layout_description(ws_idx, new_tab_idx)
            .ok_or_else(|| layout_apply_error("layout_apply_failed", "new layout unavailable"))
    }

    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        id: String,
        params: LayoutSetSplitRatioParams,
    ) -> String {
        if !params.ratio.is_finite() {
            return encode_error(id, "invalid_ratio", "ratio must be finite");
        }
        let Some((ws_idx, tab_idx)) = self.resolve_layout_export_target(&LayoutExportParams {
            tab_id: params.tab_id,
            pane_id: params.pane_id,
        }) else {
            return encode_error(id, "layout_not_found", "layout target not found");
        };

        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
            .is_some_and(|tab| tab.layout.set_ratio_at(&params.path, params.ratio));
        if !changed {
            return encode_error(id, "split_not_found", "split path not found");
        }

        self.schedule_session_save();
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return encode_error(id, "layout_not_found", "layout unavailable");
        };
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::LayoutSplitRatioSet { layout })
    }

    fn resolve_layout_export_target(&self, params: &LayoutExportParams) -> Option<(usize, usize)> {
        match (params.tab_id.as_deref(), params.pane_id.as_deref()) {
            (Some(_), Some(_)) => None,
            (Some(tab_id), None) => self.parse_tab_id(tab_id),
            (None, Some(pane_id)) => {
                let (ws_idx, pane_id) = self.parse_pane_id(pane_id)?;
                let tab_idx = self
                    .state
                    .workspaces
                    .get(ws_idx)?
                    .find_tab_index_for_pane(pane_id)?;
                Some((ws_idx, tab_idx))
            }
            (None, None) => {
                let ws_idx = self.state.active?;
                let tab_idx = self.state.workspaces.get(ws_idx)?.active_tab_index();
                Some((ws_idx, tab_idx))
            }
        }
    }

    pub(super) fn layout_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<LayoutDescription> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        Some(LayoutDescription {
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            zoomed: tab.zoomed,
            focused_pane_id: self.public_pane_id(ws_idx, tab.layout.focused())?,
            root: self.layout_node_description(ws_idx, tab_idx, tab.layout.root())?,
        })
    }

    fn layout_node_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        node: &Node,
    ) -> Option<LayoutNode> {
        match node {
            Node::Pane(pane_id) => Some(LayoutNode::Pane {
                pane: self.layout_pane_description(ws_idx, tab_idx, *pane_id)?,
            }),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => Some(LayoutNode::Split {
                direction: match direction {
                    Direction::Horizontal => SplitDirection::Right,
                    Direction::Vertical => SplitDirection::Down,
                },
                ratio: *ratio,
                first: Box::new(self.layout_node_description(ws_idx, tab_idx, first)?),
                second: Box::new(self.layout_node_description(ws_idx, tab_idx, second)?),
            }),
        }
    }

    fn layout_pane_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        pane_id: PaneId,
    ) -> Option<LayoutPane> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        let terminal_id = tab.terminal_id(pane_id)?;
        let terminal = self.state.terminals.get(terminal_id);
        Some(LayoutPane {
            pane_id: Some(self.public_pane_id(ws_idx, pane_id)?),
            label: terminal.and_then(|terminal| terminal.manual_label.clone()),
            cwd: tab
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            command: terminal.and_then(|terminal| terminal.launch_argv.clone()),
            execution_target: terminal
                .map(|terminal| terminal.execution_target.clone())
                .unwrap_or_default(),
            env: Default::default(),
        })
    }

    fn layout_root_cwd(
        &self,
        ws_idx: usize,
        replace_target: Option<(usize, usize)>,
        pane: &LayoutPane,
    ) -> PathBuf {
        if let Some(cwd) = pane.cwd.as_ref() {
            return PathBuf::from(cwd);
        }
        let follow_cwd = replace_target
            .and_then(|(_, tab_idx)| {
                let pane_id = self
                    .state
                    .workspaces
                    .get(ws_idx)?
                    .tabs
                    .get(tab_idx)?
                    .layout
                    .focused();
                self.launch_cwd_for_pane_in_workspace_on(ws_idx, pane_id, &pane.execution_target)
            })
            .or_else(|| {
                let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
                self.launch_cwd_for_pane_in_workspace_on(ws_idx, pane_id, &pane.execution_target)
            });
        if pane.execution_target.is_local() {
            self.resolve_new_terminal_cwd(follow_cwd)
        } else {
            follow_cwd.unwrap_or_default()
        }
    }

    fn apply_layout_node_to_pane(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
        node: &LayoutNode,
    ) -> Result<(), String> {
        match node {
            LayoutNode::Pane { pane } => {
                self.apply_layout_pane_label(ws_idx, pane_id, pane);
                Ok(())
            }
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let second_leaf = first_layout_leaf(second);
                let new_pane = self.layout_split_pane(
                    ws_idx,
                    pane_id,
                    direction.clone(),
                    *ratio,
                    second_leaf,
                )?;
                self.apply_layout_node_to_pane(ws_idx, pane_id, first)?;
                self.apply_layout_node_to_pane(ws_idx, new_pane, second)
            }
        }
    }

    fn layout_split_pane(
        &mut self,
        ws_idx: usize,
        target_pane_id: PaneId,
        direction: SplitDirection,
        ratio: f32,
        pane: &LayoutPane,
    ) -> Result<PaneId, String> {
        let (rows, cols) = self.state.estimate_pane_size();
        let default_shell = self.state.default_shell.clone();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let source_target = self
            .execution_target_for_pane_in_workspace(ws_idx, target_pane_id)
            .unwrap_or_default();
        let source_cwd = self.launch_cwd_for_pane_in_workspace(ws_idx, target_pane_id);
        let cwd = Some(super::panes::split_cwd_for_target(
            pane.cwd.clone(),
            &pane.execution_target,
            &source_target,
            &self.state.new_terminal_cwd,
            source_cwd,
        ));
        let extra_env = super::env::normalize_launch_env(pane.env.clone())
            .map_err(|(_, message)| message.to_string())?;
        let direction = match direction {
            SplitDirection::Right => Direction::Horizontal,
            SplitDirection::Down => Direction::Vertical,
        };
        let command = layout_command(pane)?;
        let result = {
            let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
                return Err("workspace not found".into());
            };
            if let Some(argv) = command.as_deref() {
                ws.split_pane_argv_command_with_ratio_on(
                    target_pane_id,
                    direction,
                    ratio,
                    rows,
                    cols,
                    cwd,
                    &pane.execution_target,
                    argv,
                    extra_env,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    false,
                )
            } else {
                ws.split_pane_with_ratio_on(
                    target_pane_id,
                    direction,
                    ratio,
                    rows,
                    cols,
                    cwd,
                    &pane.execution_target,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    host_terminal_appearance,
                    crate::pane::PaneShellConfig::new(&default_shell, self.state.shell_mode),
                    extra_env,
                    false,
                )
            }
        };
        let (_, new_pane) = result
            .ok_or_else(|| "pane not found".to_string())?
            .map_err(|err| err.to_string())?;
        let new_pane_id = new_pane.pane_id;
        self.attach_new_layout_pane(new_pane);
        self.apply_layout_pane_label(ws_idx, new_pane_id, pane);
        Ok(new_pane_id)
    }

    fn attach_new_layout_pane(&mut self, new_pane: NewPane) {
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
    }

    fn apply_layout_pane_label(&mut self, ws_idx: usize, pane_id: PaneId, pane: &LayoutPane) {
        let Some(label) = pane
            .label
            .as_ref()
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
        else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return;
        };
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_manual_label(label.to_string());
        }
    }

    fn rollback_layout_tab(&mut self, ws_idx: usize, root_pane: PaneId) {
        let Some(tab_idx) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.iter().position(|tab| tab.root_pane == root_pane))
        else {
            return;
        };
        let terminal_ids = self.state.terminal_ids_for_tab(ws_idx, tab_idx);
        let plugin_pane_ids = self.state.pane_ids_for_tab(ws_idx, tab_idx);
        if self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.close_tab(tab_idx))
        {
            self.state.remove_plugin_pane_records(plugin_pane_ids);
            self.state.remove_unattached_terminal_ids(terminal_ids);
            self.shutdown_detached_terminal_runtimes();
        }
    }
}

fn first_layout_leaf(node: &LayoutNode) -> &LayoutPane {
    match node {
        LayoutNode::Pane { pane } => pane,
        LayoutNode::Split { first, .. } => first_layout_leaf(first),
    }
}

fn layout_command(pane: &LayoutPane) -> Result<Option<Vec<String>>, String> {
    match pane.command.as_ref() {
        Some(command) if command.is_empty() => Err("pane command must not be empty".into()),
        Some(command) => Ok(Some(command.clone())),
        None => Ok(None),
    }
}

fn validate_layout_tree(root: &LayoutNode) -> Result<(), String> {
    let mut stats = LayoutTreeStats {
        panes: 0,
        max_depth: 0,
    };
    validate_layout_node(root, 1, &mut stats)?;
    if stats.panes > MAX_LAYOUT_PANES {
        return Err(format!(
            "layout has {} panes; maximum is {}",
            stats.panes, MAX_LAYOUT_PANES
        ));
    }
    if stats.max_depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "layout depth is {}; maximum is {}",
            stats.max_depth, MAX_LAYOUT_DEPTH
        ));
    }
    Ok(())
}

struct LayoutTreeStats {
    panes: usize,
    max_depth: usize,
}

fn validate_layout_node(
    node: &LayoutNode,
    depth: usize,
    stats: &mut LayoutTreeStats,
) -> Result<(), String> {
    stats.max_depth = stats.max_depth.max(depth);
    if depth > MAX_LAYOUT_DEPTH {
        return Err(format!(
            "layout depth is {}; maximum is {}",
            depth, MAX_LAYOUT_DEPTH
        ));
    }
    match node {
        LayoutNode::Pane { pane } => {
            stats.panes += 1;
            if stats.panes > MAX_LAYOUT_PANES {
                return Err(format!("layout has more than {} panes", MAX_LAYOUT_PANES));
            }
            layout_command(pane)?;
            super::env::normalize_launch_env(pane.env.clone())
                .map_err(|(_, message)| message.to_string())?;
            Ok(())
        }
        LayoutNode::Split {
            first,
            second,
            ratio,
            ..
        } => {
            if !ratio.is_finite() {
                return Err("split ratio must be finite".into());
            }
            validate_layout_node(first, depth + 1, stats)?;
            validate_layout_node(second, depth + 1, stats)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
    use super::*;
    use crate::{
        api::schema::{ErrorResponse, ResponseResult, SuccessResponse},
        config::{Config, ShellModeConfig},
        workspace::Workspace,
    };

    fn empty_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app
    }

    fn app_with_workspace() -> App {
        let mut app = empty_app();
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    fn persistent_empty_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            false,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app
    }

    fn persistent_app_with_workspace() -> App {
        let mut app = persistent_empty_app();
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    fn with_test_config_home<T>(name: &str, run: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = crate::config::test_config_env_lock().lock();
        let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let previous_session = std::env::var_os(crate::session::SESSION_ENV_VAR);
        let base = std::env::temp_dir().join(format!(
            "herdr-layout-idempotency-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_CONFIG_HOME", &base);
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();

        let result = run(&base);

        match previous_config_home {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match previous_session {
            Some(value) => std::env::set_var(crate::session::SESSION_ENV_VAR, value),
            None => std::env::remove_var(crate::session::SESSION_ENV_VAR),
        }
        let _ = std::fs::remove_dir_all(base);
        result
    }

    fn idempotent_layout_params(
        workspace_id: Option<String>,
        idempotency_key: &str,
        tab_label: &str,
    ) -> LayoutIdempotentParams {
        LayoutIdempotentParams {
            idempotency_key: idempotency_key.into(),
            layout: LayoutApplyParams {
                workspace_id,
                tab_id: None,
                tab_label: Some(tab_label.into()),
                focus: false,
                root: LayoutNode::Pane {
                    pane: LayoutPane {
                        command: Some(vec![exiting_test_command().into()]),
                        ..Default::default()
                    },
                },
            },
        }
    }

    #[test]
    fn layout_export_returns_portable_tree() {
        let mut app = app_with_workspace();
        let root = app.state.workspaces[0].tabs[0].root_pane;
        let right = app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.workspaces[0].tabs[0].layout.focus_pane(root);
        app.state.workspaces[0].tabs[0]
            .layout
            .set_ratio_at(&[], 0.65);
        let right_terminal_id = app.state.workspaces[0].tabs[0]
            .terminal_id(right)
            .cloned()
            .unwrap();
        let remote_target = crate::execution::ExecutionTarget::ssh("build.example").unwrap();
        let right_terminal = app.state.terminals.get_mut(&right_terminal_id).unwrap();
        right_terminal.set_manual_label("tests".into());
        right_terminal.execution_target = remote_target.clone();

        let response = app.handle_layout_export(
            "req".into(),
            LayoutExportParams {
                tab_id: None,
                pane_id: None,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::LayoutExport { layout } = success.result else {
            panic!("expected layout export response");
        };
        assert_eq!(layout.workspace_id, app.public_workspace_id(0));
        assert_eq!(layout.focused_pane_id, app.public_pane_id(0, root).unwrap());
        let LayoutNode::Split {
            direction,
            ratio,
            second,
            ..
        } = layout.root
        else {
            panic!("expected split layout root");
        };
        assert_eq!(direction, SplitDirection::Right);
        assert!((ratio - 0.65).abs() < f32::EPSILON);
        let LayoutNode::Pane { pane } = *second else {
            panic!("expected second pane");
        };
        assert_eq!(pane.label.as_deref(), Some("tests"));
        assert_eq!(pane.pane_id, Some(app.public_pane_id(0, right).unwrap()));
        assert_eq!(pane.execution_target, remote_target);
    }

    #[test]
    fn layout_set_split_ratio_updates_existing_split() {
        let mut app = app_with_workspace();
        app.state.workspaces[0].test_split(Direction::Horizontal);

        let response = app.handle_layout_set_split_ratio(
            "req".into(),
            LayoutSetSplitRatioParams {
                tab_id: None,
                pane_id: None,
                path: vec![],
                ratio: 0.72,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::LayoutSplitRatioSet { layout } = success.result else {
            panic!("expected layout split ratio set response");
        };
        let LayoutNode::Split { ratio, .. } = layout.root else {
            panic!("expected split layout root");
        };
        assert!((ratio - 0.72).abs() < f32::EPSILON);
        assert!(matches!(
            &app.event_hub.events_after(0).last().expect("layout event").1.data,
            EventData::LayoutUpdated { layout }
                if layout.tab_id == app.public_tab_id(0, 0).unwrap()
                    && (layout.splits[0].ratio - 0.72).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split() {
        let mut app = app_with_workspace();

        let response = app.handle_layout_set_split_ratio(
            "req".into(),
            LayoutSetSplitRatioParams {
                tab_id: None,
                pane_id: None,
                path: vec![],
                ratio: 0.72,
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "split_not_found");
    }

    #[tokio::test]
    async fn layout_apply_replays_same_key_and_rejects_divergent_payload() {
        with_test_config_home("replay-conflict", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "layout-operation",
                "idempotent",
            );

            let first: SuccessResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "first".into(),
                params.clone(),
                false,
            ))
            .unwrap();
            let ResponseResult::LayoutApply {
                layout: first_layout,
            } = first.result
            else {
                panic!("expected layout apply response");
            };
            let tab_count = app.state.workspaces[0].tabs.len();

            let replay: SuccessResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("second".into(), params.clone(), false),
            )
            .unwrap();
            assert_eq!(
                replay.result,
                ResponseResult::LayoutApply {
                    layout: first_layout
                }
            );
            assert_eq!(app.state.workspaces[0].tabs.len(), tab_count);

            let mut divergent = params;
            divergent.layout.tab_label = Some("different".into());
            let conflict: ErrorResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("conflict".into(), divergent, false),
            )
            .unwrap();
            assert_eq!(conflict.error.code, "idempotency_conflict");
            assert_eq!(app.state.workspaces[0].tabs.len(), tab_count);
            shutdown_test_runtimes(&mut app);
        });
    }

    #[test]
    fn reconcile_without_receipt_fences_later_apply() {
        with_test_config_home("reconcile-fence", |_| {
            let mut app = persistent_empty_app();
            let params = idempotent_layout_params(None, "cleanup-first", "cleanup");

            let absent: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "cleanup".into(),
                params.clone(),
                true,
            ))
            .unwrap();
            assert_eq!(absent.error.code, "idempotency_no_effect");

            app.state.workspaces = vec![Workspace::test_new("layout")];
            app.state.active = Some(0);
            app.state.selected = 0;
            app.state.ensure_test_terminals();
            let replayed_absent: ErrorResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("late".into(), params, false),
            )
            .unwrap();

            assert_eq!(replayed_absent.error.code, "idempotency_no_effect");
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            assert!(matches!(
                app.layout_apply_receipts["cleanup-first"].outcome,
                crate::persist::LayoutApplyOutcome::Cancelled
            ));
        });
    }
    #[tokio::test]
    async fn session_clear_rotates_epoch_and_resets_idempotency_keys() {
        with_test_config_home("session-clear-reset", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(None, "reusable-after-clear", "replacement");
            let old_epoch = app.layout_apply_epoch.clone();

            let fenced: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "fence".into(),
                params.clone(),
                true,
            ))
            .unwrap();
            assert_eq!(fenced.error.code, "idempotency_no_effect");

            app.state.workspaces.clear();
            app.state.active = None;
            app.save_session_now();
            assert_ne!(app.layout_apply_epoch, old_epoch);
            assert!(app.layout_apply_receipts.is_empty());

            app.state.workspaces = vec![Workspace::test_new("replacement")];
            app.state.active = Some(0);
            app.state.selected = 0;
            app.state.ensure_test_terminals();
            let applied: SuccessResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("replacement".into(), params, false),
            )
            .unwrap();
            assert!(matches!(applied.result, ResponseResult::LayoutApply { .. }));
            shutdown_test_runtimes(&mut app);
        });
    }

    #[test]
    fn future_session_snapshot_blocks_mutation_and_preserves_bytes() {
        with_test_config_home("future-session", |_| {
            let session_path = crate::session::data_dir().join("session.json");
            std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
            let content = br#"{"version":4294967295,"workspaces":[],"active":null,"selected":0}"#;
            std::fs::write(&session_path, content).unwrap();

            let mut app = persistent_empty_app();
            assert!(app.session_persistence_blocked);
            assert!(app.state.should_quit);
            assert!(app.state.workspaces.is_empty());

            let request = crate::api::schema::Request {
                id: "blocked".into(),
                method: crate::api::schema::Method::LayoutApply(
                    idempotent_layout_params(None, "unused", "blocked").layout,
                ),
            };
            let error: ErrorResponse =
                serde_json::from_str(&app.handle_api_request(request)).unwrap();
            assert_eq!(error.error.code, "session_snapshot_unsupported");
            app.save_session_now();
            assert_eq!(std::fs::read(&session_path).unwrap(), content);
        });
    }

    #[test]
    fn no_session_rejects_idempotent_layout_methods() {
        let mut app = app_with_workspace();
        let params =
            idempotent_layout_params(Some(app.public_workspace_id(0)), "unsupported", "keyed");

        for reconcile_only in [false, true] {
            let error: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                format!("request-{reconcile_only}"),
                params.clone(),
                reconcile_only,
            ))
            .unwrap();
            assert_eq!(error.error.code, "unsupported_in_no_session");
        }
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert!(app.layout_apply_receipts.is_empty());
    }

    #[test]
    fn failed_layout_apply_no_effect_is_payload_bound() {
        with_test_config_home("failed-apply-payload-binding", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(
                Some("missing-workspace".into()),
                "failed-apply",
                "failed",
            );

            let first: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "first".into(),
                params.clone(),
                false,
            ))
            .unwrap();
            assert_eq!(first.error.code, "workspace_not_found");
            assert!(matches!(
                app.layout_apply_receipts["failed-apply"].outcome,
                crate::persist::LayoutApplyOutcome::NoEffect
            ));

            let replay: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "replay".into(),
                params.clone(),
                false,
            ))
            .unwrap();
            assert_eq!(replay.error.code, "idempotency_no_effect");

            let mut divergent = params;
            divergent.layout.tab_label = Some("different".into());
            let conflict: ErrorResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("conflict".into(), divergent, false),
            )
            .unwrap();
            assert_eq!(conflict.error.code, "idempotency_conflict");
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            shutdown_test_runtimes(&mut app);
        });
    }

    #[test]
    fn keyed_layout_apply_fails_closed_after_malformed_ledger_load() {
        with_test_config_home("malformed-load", |_| {
            let data_dir = crate::session::data_dir();
            std::fs::create_dir_all(&data_dir).unwrap();
            std::fs::write(data_dir.join("api-idempotency.json"), "{not-json").unwrap();

            let mut app = persistent_empty_app();
            assert!(app.layout_apply_receipts_error.is_some());
            app.state.workspaces = vec![Workspace::test_new("layout")];
            app.state.active = Some(0);
            app.state.selected = 0;
            app.state.ensure_test_terminals();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "unavailable-ledger",
                "must-not-apply",
            );

            let error: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "req".into(),
                params,
                false,
            ))
            .unwrap();

            assert_eq!(error.error.code, "idempotency_unavailable");
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        });
    }

    #[tokio::test]
    async fn baseline_session_save_failure_has_no_effect_or_receipt() {
        with_test_config_home("session-save-failure", |_| {
            let mut app = persistent_app_with_workspace();
            let session_temp_path = crate::session::data_dir().join("session.json.tmp");
            std::fs::create_dir_all(&session_temp_path).unwrap();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "session-save-failure",
                "durable",
            );

            let error: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "req".into(),
                params,
                false,
            ))
            .unwrap();

            assert_eq!(error.error.code, "session_persist_failed");
            assert!(!app.layout_apply_quarantined);
            assert!(!app.state.should_quit);
            assert!(!app.no_session);
            assert!(!app
                .layout_apply_receipts
                .contains_key("session-save-failure"));
            assert!(crate::persist::load_layout_apply_ledger()
                .unwrap()
                .receipts
                .is_empty());
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            assert!(!crate::session::data_dir().join("session.json").exists());
            shutdown_test_runtimes(&mut app);
        });
    }

    #[test]
    fn pending_receipt_without_matching_live_nonce_stays_pending() {
        with_test_config_home("pending-without-nonce", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "pending-without-nonce",
                "pending",
            );
            let digest = app.layout_apply_request_digest(&params.layout).unwrap();
            app.layout_apply_receipts.insert(
                params.idempotency_key.clone(),
                crate::persist::LayoutApplyReceipt {
                    session_epoch: app.layout_apply_epoch.clone(),
                    request_digest: digest,
                    effect_nonce: "ab".repeat(16),
                    outcome: crate::persist::LayoutApplyOutcome::pending(
                        app.public_tab_id(0, 0).unwrap(),
                    ),
                },
            );

            let error: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "reconcile".into(),
                params,
                true,
            ))
            .unwrap();
            assert_eq!(error.error.code, "idempotency_pending");
            assert!(matches!(
                app.layout_apply_receipts["pending-without-nonce"].outcome,
                crate::persist::LayoutApplyOutcome::Pending { .. }
            ));
        });
    }

    #[test]
    fn pending_reconciliation_snapshot_failure_quarantines() {
        with_test_config_home("pending-snapshot-failure", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "pending-snapshot-failure",
                "pending",
            );
            let nonce = "ef".repeat(16);
            app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
            let digest = app.layout_apply_request_digest(&params.layout).unwrap();
            app.layout_apply_receipts.insert(
                params.idempotency_key.clone(),
                crate::persist::LayoutApplyReceipt {
                    session_epoch: app.layout_apply_epoch.clone(),
                    request_digest: digest,
                    effect_nonce: nonce,
                    outcome: crate::persist::LayoutApplyOutcome::pending(
                        app.public_tab_id(0, 0).unwrap(),
                    ),
                },
            );
            std::fs::create_dir_all(crate::session::data_dir().join("session.json.tmp")).unwrap();

            let error: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
                "reconcile".into(),
                params,
                true,
            ))
            .unwrap();
            assert_eq!(error.error.code, "session_persist_failed");
            assert!(app.layout_apply_quarantined);
            assert!(app.state.should_quit);
            assert!(app.no_session);
            assert!(matches!(
                app.layout_apply_receipts["pending-snapshot-failure"].outcome,
                crate::persist::LayoutApplyOutcome::Pending { .. }
            ));
            shutdown_test_runtimes(&mut app);
        });
    }

    #[test]
    fn pending_receipt_commits_only_for_matching_live_nonce() {
        with_test_config_home("pending-matching-nonce", |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "pending-matching-nonce",
                "pending",
            );
            let nonce = "cd".repeat(16);
            app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
            let tab_id = app.public_tab_id(0, 0).unwrap();
            let digest = app.layout_apply_request_digest(&params.layout).unwrap();
            app.layout_apply_receipts.insert(
                params.idempotency_key.clone(),
                crate::persist::LayoutApplyReceipt {
                    session_epoch: app.layout_apply_epoch.clone(),
                    request_digest: digest,
                    effect_nonce: nonce,
                    outcome: crate::persist::LayoutApplyOutcome::pending(tab_id.clone()),
                },
            );

            let success: SuccessResponse = serde_json::from_str(
                &app.handle_layout_apply_idempotent("reconcile".into(), params, true),
            )
            .unwrap();
            let ResponseResult::LayoutApply { layout } = success.result else {
                panic!("expected reconciled layout");
            };
            assert_eq!(layout.tab_id, tab_id);
            assert!(matches!(
                app.layout_apply_receipts["pending-matching-nonce"].outcome,
                crate::persist::LayoutApplyOutcome::Committed { .. }
            ));
        });
    }

    #[tokio::test]
    async fn layout_apply_replaces_tab_with_requested_tree() {
        let mut app = app_with_workspace();
        let original_tab_id = app.public_tab_id(0, 0).unwrap();

        let response = app.handle_layout_apply(
            "req".into(),
            LayoutApplyParams {
                workspace_id: None,
                tab_id: Some(original_tab_id),
                tab_label: Some("dev".into()),
                focus: true,
                root: LayoutNode::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.7,
                    first: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("editor".into()),
                            ..Default::default()
                        },
                    }),
                    second: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("tests".into()),
                            command: Some(vec!["sh".into(), "-c".into(), "true".into()]),
                            env: std::collections::HashMap::from([(
                                "HERDR_ROLE".into(),
                                "tests".into(),
                            )]),
                            ..Default::default()
                        },
                    }),
                },
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::LayoutApply { layout } = success.result else {
            panic!("expected layout apply response");
        };
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(
            app.state.workspaces[0].tab_display_name(0).as_deref(),
            Some("dev")
        );
        let LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } = layout.root
        else {
            panic!("expected split layout root");
        };
        assert_eq!(direction, SplitDirection::Right);
        assert!((ratio - 0.7).abs() < f32::EPSILON);
        let LayoutNode::Pane { pane: first_pane } = *first else {
            panic!("expected first pane");
        };
        let LayoutNode::Pane { pane: second_pane } = *second else {
            panic!("expected second pane");
        };
        assert_eq!(first_pane.label.as_deref(), Some("editor"));
        assert_eq!(second_pane.label.as_deref(), Some("tests"));
        assert_eq!(
            second_pane.command,
            Some(vec!["sh".into(), "-c".into(), "true".into()])
        );
        assert!(matches!(
            &app.event_hub.events_after(0).last().expect("layout event").1.data,
            EventData::LayoutUpdated { layout }
                if layout.tab_id == app.public_tab_id(0, 0).unwrap()
                    && layout.panes.len() == 2
        ));
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn layout_apply_new_tab_follows_cached_focused_pane_cwd_without_runtime() {
        let mut app = app_with_workspace();
        let focused_pane = app.state.workspaces[0].tabs[0].root_pane;
        let cached_cwd = std::env::temp_dir();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = cached_cwd.clone();

        let response = app.handle_layout_apply(
            "req".into(),
            LayoutApplyParams {
                workspace_id: None,
                tab_id: None,
                tab_label: Some("cached".into()),
                focus: false,
                root: LayoutNode::Pane {
                    pane: LayoutPane::default(),
                },
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::LayoutApply { .. }));
        let created = &app.state.workspaces[0].tabs[1];
        let created_terminal_id = created.terminal_id(created.root_pane).unwrap();
        let created_cwd = &app.state.terminals.get(created_terminal_id).unwrap().cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&cached_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn layout_root_cwd_only_follows_matching_execution_target() {
        let mut app = app_with_workspace();
        let focused_pane = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0]
            .terminal_id(focused_pane)
            .cloned()
            .unwrap();
        let remote_target = crate::execution::ExecutionTarget::ssh("build.example").unwrap();
        let remote_cwd = PathBuf::from("/remote/worktree");
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.execution_target = remote_target.clone();
        terminal.cwd = remote_cwd.clone();
        let local_default = std::env::temp_dir();
        app.state.new_terminal_cwd =
            crate::config::NewTerminalCwdConfig::Path(local_default.display().to_string());

        assert_eq!(
            app.layout_root_cwd(0, None, &LayoutPane::default()),
            local_default
        );
        assert_eq!(
            app.layout_root_cwd(
                0,
                None,
                &LayoutPane {
                    execution_target: remote_target,
                    ..Default::default()
                },
            ),
            remote_cwd
        );
        assert_eq!(
            app.layout_root_cwd(
                0,
                None,
                &LayoutPane {
                    execution_target: crate::execution::ExecutionTarget::ssh("other.example")
                        .unwrap(),
                    ..Default::default()
                },
            ),
            PathBuf::new()
        );
    }

    #[tokio::test]
    async fn layout_apply_replace_drops_plugin_pane_records_of_replaced_tab() {
        let mut app = app_with_workspace();
        let original_tab_id = app.public_tab_id(0, 0).unwrap();
        let replaced_pane = app.state.workspaces[0].tabs[0].root_pane;
        app.state.plugin_panes.insert(
            replaced_pane,
            crate::app::state::PluginPaneRecord {
                plugin_id: "example.layout".into(),
                entrypoint: "board".into(),
            },
        );

        let response = app.handle_layout_apply(
            "req".into(),
            LayoutApplyParams {
                workspace_id: None,
                tab_id: Some(original_tab_id),
                tab_label: Some("dev".into()),
                focus: true,
                root: LayoutNode::Pane {
                    pane: LayoutPane {
                        label: Some("editor".into()),
                        ..Default::default()
                    },
                },
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(success.result, ResponseResult::LayoutApply { .. }));
        assert!(!app.state.plugin_panes.contains_key(&replaced_pane));
        app.state.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn layout_apply_rejects_invalid_deep_leaf_without_creating_tab() {
        let mut app = app_with_workspace();
        let original_tab_count = app.state.workspaces[0].tabs.len();

        let response = app.handle_layout_apply(
            "req".into(),
            LayoutApplyParams {
                workspace_id: Some(app.public_workspace_id(0)),
                tab_id: None,
                tab_label: Some("bad".into()),
                focus: false,
                root: LayoutNode::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            label: Some("editor".into()),
                            ..Default::default()
                        },
                    }),
                    second: Box::new(LayoutNode::Pane {
                        pane: LayoutPane {
                            command: Some(Vec::new()),
                            ..Default::default()
                        },
                    }),
                },
            },
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "invalid_layout");
        assert_eq!(app.state.workspaces[0].tabs.len(), original_tab_count);
    }

    #[test]
    fn layout_validation_rejects_too_many_panes() {
        let mut root = LayoutNode::Pane {
            pane: LayoutPane::default(),
        };
        for _ in 0..MAX_LAYOUT_PANES {
            root = LayoutNode::Split {
                direction: SplitDirection::Right,
                ratio: 0.5,
                first: Box::new(root),
                second: Box::new(LayoutNode::Pane {
                    pane: LayoutPane::default(),
                }),
            };
        }

        let err = validate_layout_tree(&root).unwrap_err();
        assert!(err.contains("maximum"));
    }
}
