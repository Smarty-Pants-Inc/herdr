use ratatui::layout::Direction;

use super::super::responses::{encode_error, encode_success};
use crate::api::schema::{
    InstalledPluginInfo, PluginInvocationContext, PluginManifestPane, PluginPaneInfo,
    PluginPaneOpenParams, PluginPanePlacement, PluginPaneScope, ResponseResult,
};
use crate::app::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientPrivatePluginPopupOrigin {
    Pane(crate::layout::PaneId),
    WorkspacePlugin(crate::layout::PaneId),
}
#[derive(Debug, Clone)]
pub(crate) struct ClientPrivatePluginPopupSpec {
    pub(crate) plugin: InstalledPluginInfo,
    pub(crate) entrypoint: String,
    pub(crate) title: String,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) execution_target: crate::execution::ExecutionTarget,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) width: Option<crate::popup_size::PopupSize>,
    pub(crate) height: Option<crate::popup_size::PopupSize>,
    pub(crate) origin: ClientPrivatePluginPopupOrigin,
}

fn private_popup_origin_for_source(
    app: &App,
    source_pane_id: &str,
) -> Option<ClientPrivatePluginPopupOrigin> {
    if let Some((_, pane_id)) = app.parse_pane_id(source_pane_id) {
        return Some(ClientPrivatePluginPopupOrigin::Pane(pane_id));
    }
    app.state
        .workspace_plugin_panes
        .iter()
        .find_map(|(workspace_id, pane)| {
            (crate::app::workspace_plugin_pane::public_workspace_plugin_pane_id(workspace_id)
                == source_pane_id)
                .then_some(ClientPrivatePluginPopupOrigin::WorkspacePlugin(
                    pane.pane_id,
                ))
        })
}
impl App {
    pub(crate) fn plugin_pane_effective_scope(
        &self,
        params: &PluginPaneOpenParams,
    ) -> PluginPaneScope {
        params.scope.unwrap_or_else(|| {
            let plugin_id = super::normalize_plugin_id(&params.plugin_id);
            let entrypoint = super::normalize_action_id(&params.entrypoint);
            plugin_id
                .as_ref()
                .and_then(|plugin_id| self.state.installed_plugins.get(plugin_id))
                .and_then(|plugin| {
                    entrypoint.as_ref().and_then(|entrypoint| {
                        plugin.panes.iter().find(|pane| pane.id == *entrypoint)
                    })
                })
                .map(|pane| pane.scope)
                .unwrap_or_default()
        })
    }

    pub(crate) fn client_private_plugin_popup_spec(
        &self,
        params: &PluginPaneOpenParams,
    ) -> Result<ClientPrivatePluginPopupSpec, (&'static str, String)> {
        if params
            .placement
            .is_some_and(|placement| placement != PluginPanePlacement::Popup)
        {
            return Err((
                "invalid_params",
                "client-private plugin panes only support popup placement".to_string(),
            ));
        }
        if params.workspace_id.is_some() || params.direction.is_some() {
            return Err((
                "invalid_params",
                "client-private plugin popups support target_pane_id but not workspace_id or direction"
                    .to_string(),
            ));
        }
        let Some(plugin_id) = super::normalize_plugin_id(&params.plugin_id) else {
            return Err(("invalid_plugin_id", "invalid plugin id".to_string()));
        };
        let Some(plugin) = self.state.installed_plugins.get(&plugin_id).cloned() else {
            return Err(("plugin_not_found", "plugin not found".to_string()));
        };
        if !super::plugin_manifest_available(&plugin) {
            return Err((
                "plugin_manifest_unavailable",
                format!("plugin {plugin_id} manifest is unavailable"),
            ));
        }
        if !plugin.enabled {
            return Err(("plugin_disabled", format!("plugin {plugin_id} is disabled")));
        }
        let Some(entrypoint) = super::normalize_action_id(&params.entrypoint) else {
            return Err((
                "invalid_plugin_entrypoint",
                "invalid entrypoint id".to_string(),
            ));
        };
        let Some(pane) = plugin
            .panes
            .iter()
            .find(|pane| pane.id == entrypoint)
            .cloned()
        else {
            return Err((
                "plugin_pane_not_found",
                format!("plugin pane entrypoint '{entrypoint}' not found"),
            ));
        };
        if params.scope.unwrap_or(pane.scope) != PluginPaneScope::ClientPrivate {
            return Err((
                "invalid_params",
                "pane scope must be client_private".to_string(),
            ));
        }
        let placement = params.placement.unwrap_or(pane.placement);
        if placement != PluginPanePlacement::Popup {
            return Err((
                "invalid_params",
                "client-private plugin panes only support popup placement".to_string(),
            ));
        }
        let (mut context, execution_target) = self.plugin_pane_source_context(
            params.target_pane_id.as_deref(),
            self.current_plugin_context("plugin-pane"),
        )?;
        let source_pane_id = context.focused_pane_id.clone().ok_or_else(|| {
            (
                "no_active_pane",
                "client-private plugin popup requires a source pane".to_string(),
            )
        })?;
        let origin = private_popup_origin_for_source(self, &source_pane_id).ok_or_else(|| {
            (
                "pane_not_found",
                format!("pane {source_pane_id} is no longer available"),
            )
        })?;
        context.view_id = params.view_id.clone();
        validate_plugin_pane_platform(&plugin, &pane, &execution_target)?;
        let cwd = self.plugin_pane_cwd(&plugin, params.cwd.clone(), &execution_target);
        let env = self
            .plugin_pane_launch_env_without_setup(
                &plugin,
                &pane.id,
                &cwd,
                &execution_target,
                params.env.clone(),
                &context,
            )
            .map_err(|(code, message)| {
                let code = match code.as_str() {
                    "invalid_plugin_context" => "invalid_plugin_context",
                    "invalid_env" => "invalid_env",
                    _ => "invalid_params",
                };
                (code, message)
            })?;
        Ok(ClientPrivatePluginPopupSpec {
            plugin,
            entrypoint: pane.id,
            title: pane.title,
            command: pane.command,
            cwd,
            execution_target,
            env,
            width: params.width.or(pane.width),
            height: params.height.or(pane.height),
            origin,
        })
    }

    pub(crate) fn private_popup_source_pane_id(
        &self,
        origin: ClientPrivatePluginPopupOrigin,
    ) -> Option<String> {
        match origin {
            ClientPrivatePluginPopupOrigin::Pane(pane_id) => {
                let (ws_idx, _) = self.find_pane(pane_id)?;
                self.public_pane_id(ws_idx, pane_id)
            }
            ClientPrivatePluginPopupOrigin::WorkspacePlugin(pane_id) => self
                .state
                .workspace_plugin_panes
                .iter()
                .find_map(|(workspace_id, pane)| {
                    (pane.pane_id == pane_id).then(|| {
                        crate::app::workspace_plugin_pane::public_workspace_plugin_pane_id(
                            workspace_id,
                        )
                    })
                }),
        }
    }

    pub(super) fn open_plugin_popup_pane(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin: &InstalledPluginInfo,
        pane: PluginManifestPane,
    ) -> String {
        let (context, execution_target) = match self.plugin_pane_source_context(
            params.target_pane_id.as_deref(),
            self.current_plugin_context("plugin-pane"),
        ) {
            Ok(source) => source,
            Err((code, message)) => return encode_error(id, code, message),
        };
        if let Err((code, message)) =
            validate_plugin_pane_platform(plugin, &pane, &execution_target)
        {
            return encode_error(id, code, message);
        }
        let cwd = self.plugin_pane_cwd(plugin, params.cwd, &execution_target);
        let extra_env = match self.plugin_pane_launch_env(
            plugin,
            &pane.id,
            &cwd,
            &execution_target,
            params.env,
            &context,
        ) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let width = params.width.or(pane.width);
        let height = params.height.or(pane.height);
        if let Err(err) = self.spawn_popup_plugin_command(
            &plugin.plugin_id,
            &pane.id,
            &pane.command,
            Some(cwd),
            &execution_target,
            extra_env,
            crate::app::popup::PopupGeometry { width, height },
        ) {
            return encode_error(id, "plugin_pane_open_failed", err.to_string());
        }
        let Some(popup) = self.state.popup_pane.as_ref() else {
            return encode_error(id, "plugin_pane_open_failed", "plugin popup disappeared");
        };
        if let Some(terminal) = self.state.terminals.get_mut(&popup.terminal_id) {
            terminal.set_manual_label(pane.title);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn open_plugin_workspace_right_pane(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin: &InstalledPluginInfo,
        pane: PluginManifestPane,
    ) -> String {
        let ws_idx = match params.workspace_id.as_deref() {
            Some(workspace_id) => match self.parse_workspace_id(workspace_id) {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "workspace_not_found", "workspace not found"),
            },
            None => match self.state.active {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "no_active_workspace", "no active workspace"),
            },
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        if let Some(existing) = self.workspace_plugin_pane_info(&workspace_id) {
            if existing.plugin_id != plugin.plugin_id || existing.entrypoint != pane.id {
                return encode_error(
                    id,
                    "workspace_plugin_pane_exists",
                    "workspace already has a right plugin pane",
                );
            }
            let plugin_pane = if params.focus {
                self.focus_workspace_plugin_pane(&existing.pane_id)
                    .unwrap_or(existing)
            } else {
                existing
            };
            return encode_success(
                id,
                ResponseResult::PluginWorkspacePaneOpened { plugin_pane },
            );
        }
        let (context, execution_target) = match self.plugin_pane_source_context(
            params.target_pane_id.as_deref(),
            self.plugin_context_for_workspace(ws_idx, "plugin-pane"),
        ) {
            Ok(source) => source,
            Err((code, message)) => return encode_error(id, code, message),
        };
        if let Err((code, message)) =
            validate_plugin_pane_platform(plugin, &pane, &execution_target)
        {
            return encode_error(id, code, message);
        }
        let cwd = self.plugin_pane_cwd(plugin, params.cwd, &execution_target);
        let extra_env = match self.plugin_pane_launch_env(
            plugin,
            &pane.id,
            &cwd,
            &execution_target,
            params.env.clone(),
            &context,
        ) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let width = params.width.or(pane.width);
        if let Err(err) = self.spawn_workspace_plugin_argv_command(
            workspace_id.clone(),
            plugin.plugin_id.clone(),
            pane.id.clone(),
            &execution_target,
            &pane.command,
            cwd,
            extra_env,
            width,
            params.focus,
        ) {
            return encode_error(id, "plugin_pane_open_failed", err.to_string());
        }
        if let Some(terminal_id) = self
            .state
            .workspace_plugin_panes
            .get(&workspace_id)
            .map(|state| state.terminal_id.clone())
        {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.set_manual_label(pane.title);
            }
        }
        let Some(plugin_pane) = self.workspace_plugin_pane_info(&workspace_id) else {
            return encode_error(
                id,
                "plugin_pane_open_failed",
                "workspace plugin pane disappeared",
            );
        };
        encode_success(
            id,
            ResponseResult::PluginWorkspacePaneOpened { plugin_pane },
        )
    }

    pub(super) fn open_plugin_overlay_pane(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin: &InstalledPluginInfo,
        pane: PluginManifestPane,
    ) -> String {
        let (context, execution_target) = match self.plugin_pane_source_context(
            params.target_pane_id.as_deref(),
            self.current_plugin_context("plugin-pane"),
        ) {
            Ok(source) => source,
            Err((code, message)) => return encode_error(id, code, message),
        };
        if let Err((code, message)) =
            validate_plugin_pane_platform(plugin, &pane, &execution_target)
        {
            return encode_error(id, code, message);
        }
        let cwd = self.plugin_pane_cwd(plugin, params.cwd, &execution_target);
        let extra_env = match self.plugin_pane_launch_env(
            plugin,
            &pane.id,
            &cwd,
            &execution_target,
            params.env,
            &context,
        ) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let (ws_idx, new_pane) = match self.spawn_overlay_plugin_command(
            &plugin.plugin_id,
            &pane.id,
            &pane.command,
            Some(cwd),
            &execution_target,
            extra_env,
            Vec::new(),
        ) {
            Ok(result) => result,
            Err(err) => return encode_error(id, "plugin_pane_open_failed", err.to_string()),
        };
        let layout_tab_idx = self
            .overlay_panes
            .get(&new_pane.pane_id)
            .map(|overlay| overlay.tab_idx);
        self.finish_plugin_pane_open(
            id,
            ws_idx,
            None,
            layout_tab_idx,
            new_pane,
            plugin.plugin_id.clone(),
            pane,
        )
    }

    pub(super) fn open_plugin_split_pane(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin: &InstalledPluginInfo,
        pane: PluginManifestPane,
        placement: PluginPanePlacement,
    ) -> String {
        let target_pane_id = params
            .target_pane_id
            .clone()
            .or_else(|| self.current_public_pane_id());
        let Some(target_pane_id) = target_pane_id else {
            return encode_error(id, "no_active_pane", "no active pane");
        };
        let Some((ws_idx, target_pane)) = self.parse_pane_id(&target_pane_id) else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {target_pane_id} not found"),
            );
        };
        let execution_target = self
            .execution_target_for_pane_in_workspace(ws_idx, target_pane)
            .unwrap_or_default();
        if let Err((code, message)) =
            validate_plugin_pane_platform(plugin, &pane, &execution_target)
        {
            return encode_error(id, code, message);
        }
        let context = self.plugin_context_for_pane(ws_idx, target_pane, "plugin-pane");
        let cwd = self.plugin_pane_cwd(plugin, params.cwd, &execution_target);
        let extra_env = match self.plugin_pane_launch_env(
            plugin,
            &pane.id,
            &cwd,
            &execution_target,
            params.env,
            &context,
        ) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let direction = match params
            .direction
            .unwrap_or(crate::api::schema::SplitDirection::Right)
        {
            crate::api::schema::SplitDirection::Right => Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => Direction::Vertical,
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let previous_focus = self.state.current_pane_focus_target();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let result = ws.split_pane_plugin_command(
            target_pane,
            direction,
            rows.max(4),
            cols.max(10),
            Some(cwd),
            &execution_target,
            &plugin.plugin_id,
            &pane.id,
            &pane.command,
            extra_env,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            params.focus || placement == PluginPanePlacement::Zoomed,
        );
        let (tab_idx, new_pane) = match result {
            Some(Ok(result)) => result,
            Some(Err(err)) => return encode_error(id, "plugin_pane_open_failed", err.to_string()),
            None => {
                return encode_error(
                    id,
                    "pane_not_found",
                    format!("pane {target_pane_id} not found"),
                )
            }
        };
        if params.focus || placement == PluginPanePlacement::Zoomed {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.mode = crate::app::Mode::Terminal;
        }
        if placement == PluginPanePlacement::Zoomed {
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs.get_mut(tab_idx))
            {
                tab.zoomed = true;
            }
        }
        self.finish_plugin_pane_open(
            id,
            ws_idx,
            None,
            Some(tab_idx),
            new_pane,
            plugin.plugin_id.clone(),
            pane,
        )
    }

    pub(super) fn open_plugin_tab(
        &mut self,
        id: String,
        params: PluginPaneOpenParams,
        plugin: &InstalledPluginInfo,
        pane: PluginManifestPane,
    ) -> String {
        let ws_idx = match params.workspace_id.as_deref() {
            Some(workspace_id) => match self.parse_workspace_id(workspace_id) {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "workspace_not_found", "workspace not found"),
            },
            None => match self.state.active {
                Some(ws_idx) => ws_idx,
                None => return encode_error(id, "no_active_workspace", "no active workspace"),
            },
        };
        let (context, execution_target) = match self.plugin_pane_source_context(
            params.target_pane_id.as_deref(),
            self.plugin_context_for_workspace(ws_idx, "plugin-pane"),
        ) {
            Ok(source) => source,
            Err((code, message)) => return encode_error(id, code, message),
        };
        if let Err((code, message)) =
            validate_plugin_pane_platform(plugin, &pane, &execution_target)
        {
            return encode_error(id, code, message);
        }
        let cwd = self.plugin_pane_cwd(plugin, params.cwd, &execution_target);
        let extra_env = match self.plugin_pane_launch_env(
            plugin,
            &pane.id,
            &cwd,
            &execution_target,
            params.env,
            &context,
        ) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let (tab_idx, terminal, runtime) = match ws.create_tab_plugin_command(
            rows.max(4),
            cols.max(10),
            cwd,
            &execution_target,
            &plugin.plugin_id,
            &pane.id,
            &pane.command,
            extra_env,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
        ) {
            Ok(result) => result,
            Err(err) => return encode_error(id, "plugin_pane_open_failed", err.to_string()),
        };
        let pane_id = ws.tabs[tab_idx].root_pane;
        if params.focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let new_pane = crate::workspace::NewPane {
            pane_id,
            terminal,
            runtime,
        };
        self.finish_plugin_pane_open(
            id,
            ws_idx,
            Some(tab_idx),
            Some(tab_idx),
            new_pane,
            plugin.plugin_id.clone(),
            pane,
        )
    }
    pub(super) fn plugin_pane_source_context(
        &self,
        source_pane_id: Option<&str>,
        fallback_context: PluginInvocationContext,
    ) -> Result<(PluginInvocationContext, crate::execution::ExecutionTarget), (&'static str, String)>
    {
        let Some(source_pane_id) = source_pane_id else {
            let execution_target = self.plugin_command_execution_target(&fallback_context);
            return Ok((fallback_context, execution_target));
        };
        self.plugin_context_and_execution_target_for_source_pane(source_pane_id, "plugin-pane")
            .ok_or_else(|| ("pane_not_found", format!("pane {source_pane_id} not found")))
    }

    fn plugin_pane_launch_env(
        &self,
        plugin: &InstalledPluginInfo,
        entrypoint: &str,
        cwd: &std::path::Path,
        execution_target: &crate::execution::ExecutionTarget,
        env: std::collections::HashMap<String, String>,
        context: &PluginInvocationContext,
    ) -> Result<Vec<(String, String)>, (String, String)> {
        if execution_target.is_local() {
            super::env::ensure_plugin_user_dirs(plugin)
                .map_err(|err| ("plugin_user_dir_create_failed".to_string(), err.to_string()))?;
        }
        self.plugin_pane_launch_env_without_setup(
            plugin,
            entrypoint,
            cwd,
            execution_target,
            env,
            context,
        )
    }

    fn plugin_pane_launch_env_without_setup(
        &self,
        plugin: &InstalledPluginInfo,
        entrypoint: &str,
        cwd: &std::path::Path,
        execution_target: &crate::execution::ExecutionTarget,
        env: std::collections::HashMap<String, String>,
        context: &PluginInvocationContext,
    ) -> Result<Vec<(String, String)>, (String, String)> {
        let mut env = super::super::env::normalize_launch_env(env)?;
        if !cwd.as_os_str().is_empty() {
            crate::platform::set_default_plugin_pane_pwd(&mut env, cwd);
        }
        let context_json = serde_json::to_string(&context)
            .map_err(|err| ("invalid_plugin_context".to_string(), err.to_string()))?;
        env.retain(|(key, _)| !plugin_pane_protected_env_key(key));
        env.extend(plugin_theme_env(&self.state.palette));
        if execution_target.is_local() {
            env.extend(super::env::plugin_path_env(plugin));
            env.push((
                crate::api::SOCKET_PATH_ENV_VAR.to_string(),
                crate::api::socket_path().display().to_string(),
            ));
            if let Ok(current_exe) = std::env::current_exe() {
                env.push((
                    "HERDR_BIN_PATH".to_string(),
                    current_exe.display().to_string(),
                ));
            }
        }
        env.push(("HERDR_ENV".to_string(), "1".to_string()));
        env.push(("HERDR_PLUGIN_ID".to_string(), plugin.plugin_id.clone()));
        env.push((
            "HERDR_PLUGIN_ENTRYPOINT_ID".to_string(),
            entrypoint.to_string(),
        ));
        env.push(("HERDR_PLUGIN_CONTEXT_JSON".to_string(), context_json));
        if let Some(view_id) = context.view_id.as_ref() {
            env.push(("HERDR_VIEW_ID".to_string(), view_id.to_string()));
        }
        Ok(env)
    }

    fn finish_plugin_pane_open(
        &mut self,
        id: String,
        ws_idx: usize,
        created_tab_idx: Option<usize>,
        layout_tab_idx: Option<usize>,
        new_pane: crate::workspace::NewPane,
        plugin_id: String,
        pane_manifest: PluginManifestPane,
    ) -> String {
        let entrypoint = pane_manifest.id.clone();
        let mut terminal = new_pane.terminal;
        terminal.set_manual_label(pane_manifest.title.clone());
        let terminal_id = terminal.id.clone();
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id, terminal);
        self.state.plugin_panes.insert(
            new_pane.pane_id,
            crate::app::state::PluginPaneRecord {
                plugin_id: plugin_id.clone(),
                entrypoint: entrypoint.clone(),
            },
        );
        if let Some(tab_idx) = created_tab_idx {
            if let Some(tab) = self.tab_info(ws_idx, tab_idx) {
                self.emit_event(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::TabCreated,
                    data: crate::api::schema::EventData::TabCreated { tab },
                });
            }
        }
        self.schedule_session_save();
        let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) else {
            return encode_error(id, "plugin_pane_open_failed", "plugin pane disappeared");
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneCreated,
            data: crate::api::schema::EventData::PaneCreated { pane: pane.clone() },
        });
        if let Some(tab_idx) = layout_tab_idx {
            self.emit_layout_updated_event(ws_idx, tab_idx);
        }
        encode_success(
            id,
            ResponseResult::PluginPaneOpened {
                plugin_pane: PluginPaneInfo {
                    plugin_id,
                    entrypoint,
                    pane,
                },
            },
        )
    }

    fn plugin_pane_cwd(
        &self,
        plugin: &InstalledPluginInfo,
        override_cwd: Option<String>,
        execution_target: &crate::execution::ExecutionTarget,
    ) -> std::path::PathBuf {
        override_cwd
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                if execution_target.is_local() {
                    std::path::PathBuf::from(&plugin.plugin_root)
                } else {
                    std::path::PathBuf::new()
                }
            })
    }

    fn current_public_pane_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
        self.public_pane_id(ws_idx, pane_id)
    }
}

fn validate_plugin_pane_platform(
    plugin: &InstalledPluginInfo,
    pane: &PluginManifestPane,
    execution_target: &crate::execution::ExecutionTarget,
) -> Result<(), (&'static str, String)> {
    if execution_target.is_local() {
        super::manifest::ensure_platform_supported(
            super::manifest::effective_platforms(&pane.platforms, &plugin.platforms),
            "plugin pane",
        )
    } else {
        Ok(())
    }
}

fn plugin_pane_protected_env_key(key: &str) -> bool {
    key.starts_with("HERDR_THEME_")
        || matches!(
            key,
            crate::api::SOCKET_PATH_ENV_VAR
                | crate::integration::HERDR_WORKSPACE_ID_ENV_VAR
                | crate::integration::HERDR_TAB_ID_ENV_VAR
                | crate::integration::HERDR_PANE_ID_ENV_VAR
                | "HERDR_ENV"
                | "HERDR_PLUGIN_ID"
                | "HERDR_PLUGIN_ROOT"
                | "HERDR_PLUGIN_CONFIG_DIR"
                | "HERDR_PLUGIN_STATE_DIR"
                | "HERDR_PLUGIN_ENTRYPOINT_ID"
                | "HERDR_PLUGIN_CONTEXT_JSON"
                | "HERDR_BIN_PATH"
                | "HERDR_VIEW_ID"
        )
}

fn plugin_theme_env(palette: &crate::app::state::Palette) -> Vec<(String, String)> {
    [
        ("HERDR_THEME_ACCENT", palette.accent),
        ("HERDR_THEME_PANEL_BG", palette.panel_bg),
        ("HERDR_THEME_SIDEBAR_BG", palette.sidebar_bg),
        ("HERDR_THEME_SURFACE0", palette.surface0),
        ("HERDR_THEME_SURFACE1", palette.surface1),
        ("HERDR_THEME_SURFACE_DIM", palette.surface_dim),
        ("HERDR_THEME_OVERLAY0", palette.overlay0),
        ("HERDR_THEME_OVERLAY1", palette.overlay1),
        ("HERDR_THEME_TEXT", palette.text),
        ("HERDR_THEME_SUBTEXT0", palette.subtext0),
        ("HERDR_THEME_MAUVE", palette.mauve),
        ("HERDR_THEME_GREEN", palette.green),
        ("HERDR_THEME_YELLOW", palette.yellow),
        ("HERDR_THEME_RED", palette.red),
        ("HERDR_THEME_BLUE", palette.blue),
        ("HERDR_THEME_TEAL", palette.teal),
        ("HERDR_THEME_PEACH", palette.peach),
    ]
    .into_iter()
    .map(|(key, color)| (key.to_string(), plugin_theme_color(color)))
    .collect()
}

fn plugin_theme_color(color: ratatui::style::Color) -> String {
    use ratatui::style::Color;

    match color {
        Color::Reset => "reset".to_string(),
        Color::Black => "black".to_string(),
        Color::Red => "red".to_string(),
        Color::Green => "green".to_string(),
        Color::Yellow => "yellow".to_string(),
        Color::Blue => "blue".to_string(),
        Color::Magenta => "magenta".to_string(),
        Color::Cyan => "cyan".to_string(),
        Color::Gray => "gray".to_string(),
        Color::DarkGray => "darkgray".to_string(),
        Color::LightRed => "lightred".to_string(),
        Color::LightGreen => "lightgreen".to_string(),
        Color::LightYellow => "lightyellow".to_string(),
        Color::LightBlue => "lightblue".to_string(),
        Color::LightMagenta => "lightmagenta".to_string(),
        Color::LightCyan => "lightcyan".to_string(),
        Color::White => "white".to_string(),
        Color::Indexed(index) => format!("indexed:{index}"),
        Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_pane_env_rejects_spoofed_identity_but_keeps_context_and_explicit_view() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let plugin = InstalledPluginInfo {
            plugin_id: "example.identity-boundary".into(),
            name: "Identity Boundary".into(),
            version: "0.1.0".into(),
            min_herdr_version: crate::build_info::BASE_VERSION.into(),
            description: None,
            manifest_path: "/tmp/example.identity-boundary/herdr-plugin.toml".into(),
            plugin_root: "/tmp/example.identity-boundary".into(),
            enabled: true,
            platforms: None,
            build: Vec::new(),
            startup: Vec::new(),
            actions: Vec::new(),
            events: Vec::new(),
            panes: Vec::new(),
            link_handlers: Vec::new(),
            source: Default::default(),
            warnings: Vec::new(),
        };
        let context = PluginInvocationContext {
            workspace_id: Some("source-workspace".into()),
            workspace_label: None,
            workspace_cwd: None,
            worktree: None,
            tab_id: Some("source-tab".into()),
            tab_label: None,
            focused_pane_id: Some("source-pane".into()),
            focused_pane_cwd: None,
            focused_pane_agent: None,
            focused_pane_status: None,
            selected_text: None,
            invocation_source: Some("plugin-pane".into()),
            correlation_id: None,
            clicked_url: None,
            link_handler_id: None,
            view_id: crate::api::schema::ViewId::from_opaque("view-private"),
        };
        let env = app
            .plugin_pane_launch_env_without_setup(
                &plugin,
                "board",
                std::path::Path::new(""),
                &crate::execution::ExecutionTarget::ssh("plugin-host").unwrap(),
                std::collections::HashMap::from([
                    (
                        crate::integration::HERDR_WORKSPACE_ID_ENV_VAR.into(),
                        "spoofed-workspace".into(),
                    ),
                    (
                        crate::integration::HERDR_TAB_ID_ENV_VAR.into(),
                        "spoofed-tab".into(),
                    ),
                    (
                        crate::integration::HERDR_PANE_ID_ENV_VAR.into(),
                        "spoofed-pane".into(),
                    ),
                    ("HERDR_VIEW_ID".into(), "spoofed-view".into()),
                ]),
                &context,
            )
            .unwrap()
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();

        for key in [
            crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
            crate::integration::HERDR_TAB_ID_ENV_VAR,
            crate::integration::HERDR_PANE_ID_ENV_VAR,
        ] {
            assert!(!env.contains_key(key), "{key} should be protected");
        }
        assert_eq!(env.get("HERDR_VIEW_ID"), Some(&"view-private".to_string()));
        let serialized_context: PluginInvocationContext =
            serde_json::from_str(&env["HERDR_PLUGIN_CONTEXT_JSON"]).unwrap();
        assert_eq!(serialized_context, context);
    }

    #[test]
    fn private_popup_origin_does_not_follow_reused_workspace_plugin_public_id() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let workspace = crate::workspace::Workspace::test_new("source");
        let workspace_id = workspace.id.clone();
        app.state.workspaces = vec![workspace];
        let terminal_id = crate::terminal::TerminalId::alloc();
        let original_pane_id = crate::layout::PaneId::alloc();
        app.state.workspace_plugin_panes.insert(
            workspace_id.clone(),
            crate::app::state::WorkspacePluginPaneState {
                pane_id: original_pane_id,
                terminal_id: terminal_id.clone(),
                plugin_id: "example.explorer".into(),
                entrypoint: "explorer".into(),
                width: None,
                focused: false,
                collapsed: false,
            },
        );
        let source =
            crate::app::workspace_plugin_pane::public_workspace_plugin_pane_id(&workspace_id);
        let origin = private_popup_origin_for_source(&app, &source).unwrap();
        assert_eq!(app.private_popup_source_pane_id(origin), Some(source));

        app.state
            .workspace_plugin_panes
            .get_mut(&workspace_id)
            .unwrap()
            .pane_id = crate::layout::PaneId::alloc();

        assert_eq!(app.private_popup_source_pane_id(origin), None);
    }
}
