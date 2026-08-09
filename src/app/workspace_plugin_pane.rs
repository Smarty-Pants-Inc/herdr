use bytes::Bytes;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::path::PathBuf;
use tracing::warn;

use crate::api::schema::PluginWorkspacePaneInfo;
use crate::app::{App, Mode};
use crate::layout::PaneId;
use crate::pane::{AgentDetection, PaneLaunchEnv};
use crate::popup_size::PopupSize;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

const DEFAULT_WIDTH_PERCENT: u8 = 30;
const MIN_MAIN_WIDTH: u16 = 12;

const PUBLIC_PANE_SUFFIX: &str = ":plugin";

pub(crate) fn public_workspace_plugin_pane_id(workspace_id: &str) -> String {
    format!("{workspace_id}{PUBLIC_PANE_SUFFIX}")
}

fn workspace_id_from_public_pane_id(pane_id: &str) -> Option<&str> {
    pane_id.strip_suffix(PUBLIC_PANE_SUFFIX)
}

pub(crate) fn resolved_workspace_plugin_pane_width(
    width: Option<PopupSize>,
    available: u16,
    minimum_width: u16,
) -> Option<u16> {
    let max_width = available.saturating_sub(MIN_MAIN_WIDTH);
    if max_width < minimum_width {
        return None;
    }
    Some(
        width
            .unwrap_or(PopupSize::Percent(DEFAULT_WIDTH_PERCENT))
            .resolve(available)
            .clamp(minimum_width, max_width),
    )
}

impl crate::app::state::AppState {
    pub(crate) fn active_workspace_plugin_pane(
        &self,
    ) -> Option<(&str, &crate::app::state::WorkspacePluginPaneState)> {
        let workspace_id = self.workspaces.get(self.active?)?.id.as_str();
        Some((workspace_id, self.workspace_plugin_panes.get(workspace_id)?))
    }

    pub(crate) fn effective_workspace_plugin_pane(
        &self,
    ) -> Option<(&str, &crate::app::state::WorkspacePluginPaneState)> {
        if self.view.layout != crate::app::state::ViewLayout::Desktop
            || self.view.workspace_plugin_pane_inner.is_empty()
        {
            return None;
        }
        self.active_workspace_plugin_pane()
    }

    pub(crate) fn focused_workspace_plugin_pane(
        &self,
    ) -> Option<(&str, &crate::app::state::WorkspacePluginPaneState)> {
        self.effective_workspace_plugin_pane()
            .filter(|(_, pane)| pane.focused)
    }

    pub(crate) fn focused_terminal_pane_id(&self, ws_idx: usize) -> Option<PaneId> {
        let workspace = self.workspaces.get(ws_idx)?;
        let workspace_plugin_pane_is_visible = self.active == Some(ws_idx)
            && self.view.layout == crate::app::state::ViewLayout::Desktop
            && !self.view.workspace_plugin_pane_inner.is_empty();
        workspace_plugin_pane_is_visible
            .then(|| self.workspace_plugin_panes.get(&workspace.id))
            .flatten()
            .filter(|pane| pane.focused)
            .map(|pane| pane.pane_id)
            .or_else(|| workspace.focused_pane_id())
    }

    pub(crate) fn unfocus_workspace_plugin_pane(&mut self, workspace_id: &str) -> bool {
        let Some(pane) = self.workspace_plugin_panes.get_mut(workspace_id) else {
            return false;
        };
        std::mem::replace(&mut pane.focused, false)
    }
}

impl App {
    pub(crate) fn resize_workspace_plugin_pane(
        &mut self,
        workspace_id: &str,
        right_edge: u16,
        available_width: u16,
        divider_col: u16,
    ) -> bool {
        let desired_width = right_edge.saturating_sub(divider_col);
        let Some(width) = resolved_workspace_plugin_pane_width(
            Some(PopupSize::Cells(desired_width)),
            available_width,
            self.state.sidebar_min_width,
        ) else {
            return false;
        };
        let Some(pane) = self.state.workspace_plugin_panes.get_mut(workspace_id) else {
            return false;
        };
        let width = Some(PopupSize::Cells(width));
        if pane.width == width {
            return false;
        }
        pane.width = width;
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn focused_workspace_plugin_runtime(&self) -> Option<&TerminalRuntime> {
        let (_, pane) = self.state.focused_workspace_plugin_pane()?;
        self.terminal_runtimes.get(&pane.terminal_id)
    }

    pub(crate) fn workspace_plugin_runtime_for_pane(
        &self,
        pane_id: PaneId,
    ) -> Option<&TerminalRuntime> {
        let pane = self
            .state
            .workspace_plugin_panes
            .values()
            .find(|pane| pane.pane_id == pane_id)?;
        self.terminal_runtimes.get(&pane.terminal_id)
    }

    pub(crate) fn workspace_plugin_pane_info(
        &self,
        workspace_id: &str,
    ) -> Option<PluginWorkspacePaneInfo> {
        let pane = self.state.workspace_plugin_panes.get(workspace_id)?;
        Some(PluginWorkspacePaneInfo {
            plugin_id: pane.plugin_id.clone(),
            entrypoint: pane.entrypoint.clone(),
            pane_id: public_workspace_plugin_pane_id(workspace_id),
            terminal_id: pane.terminal_id.to_string(),
            workspace_id: workspace_id.to_string(),
            focused: pane.focused,
        })
    }

    pub(crate) fn workspace_plugin_pane_info_for_public_id(
        &self,
        pane_id: &str,
    ) -> Option<PluginWorkspacePaneInfo> {
        self.workspace_plugin_pane_info(workspace_id_from_public_pane_id(pane_id)?)
    }

    pub(crate) fn focus_workspace_plugin_pane(
        &mut self,
        pane_id: &str,
    ) -> Option<PluginWorkspacePaneInfo> {
        let workspace_id = workspace_id_from_public_pane_id(pane_id)?.to_string();
        self.state.workspace_plugin_panes.get(&workspace_id)?;
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        self.state.switch_workspace(ws_idx);
        let pane = self.state.workspace_plugin_panes.get_mut(&workspace_id)?;
        pane.collapsed = false;
        pane.focused = true;
        self.state.settle_terminal_mode_after_focus();
        self.workspace_plugin_pane_info(&workspace_id)
    }

    pub(crate) fn toggle_workspace_plugin_pane_collapsed(&mut self, workspace_id: &str) -> bool {
        let Some(pane) = self.state.workspace_plugin_panes.get_mut(workspace_id) else {
            return false;
        };
        pane.collapsed = !pane.collapsed;
        pane.focused = false;
        self.state.mode = Mode::Terminal;
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn close_workspace_plugin_pane(&mut self, pane_id: &str) -> bool {
        let Some(workspace_id) = workspace_id_from_public_pane_id(pane_id) else {
            return false;
        };
        self.close_workspace_plugin_pane_for_workspace(workspace_id)
    }

    pub(crate) fn close_workspace_plugin_pane_by_internal_id(&mut self, pane_id: PaneId) -> bool {
        let Some(workspace_id) =
            self.state
                .workspace_plugin_panes
                .iter()
                .find_map(|(workspace_id, pane)| {
                    (pane.pane_id == pane_id).then(|| workspace_id.clone())
                })
        else {
            return false;
        };
        self.close_workspace_plugin_pane_for_workspace(&workspace_id)
    }

    fn close_workspace_plugin_pane_for_workspace(&mut self, workspace_id: &str) -> bool {
        let was_active_focused = self
            .state
            .active
            .and_then(|idx| self.state.workspaces.get(idx))
            .is_some_and(|workspace| workspace.id == workspace_id)
            && self
                .state
                .workspace_plugin_panes
                .get(workspace_id)
                .is_some_and(|pane| pane.focused);
        let Some(pane) = self.state.workspace_plugin_panes.remove(workspace_id) else {
            return false;
        };
        self.state
            .direct_attach_resize_locks
            .remove(&pane.terminal_id);
        self.state.terminals.remove(&pane.terminal_id);
        self.shutdown_terminal_runtime(pane.terminal_id);
        if was_active_focused {
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    pub(crate) fn spawn_workspace_plugin_argv_command(
        &mut self,
        workspace_id: String,
        plugin_id: String,
        entrypoint: String,
        argv: &[String],
        cwd: PathBuf,
        extra_env: Vec<(String, String)>,
        width: Option<PopupSize>,
        focus: bool,
    ) -> std::io::Result<()> {
        if self
            .state
            .workspace_plugin_panes
            .contains_key(&workspace_id)
        {
            return Err(std::io::Error::other(
                "workspace already has a right plugin pane",
            ));
        }
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
            .ok_or_else(|| std::io::Error::other("workspace not found"))?;
        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let public_pane_id = public_workspace_plugin_pane_id(&workspace_id);
        let (estimated_rows, estimated_cols) = self.state.estimate_pane_size();
        let minimum_width = self.state.sidebar_min_width;
        let available_width = self
            .state
            .view
            .terminal_area
            .width
            .max(estimated_cols)
            .max(minimum_width.saturating_add(MIN_MAIN_WIDTH));
        let resolved_width =
            resolved_workspace_plugin_pane_width(width, available_width, minimum_width)
                .unwrap_or(minimum_width);
        let cols = resolved_width.saturating_sub(1).max(2);
        let rows = self
            .state
            .view
            .terminal_area
            .height
            .max(estimated_rows)
            .max(2);
        let launch_env = PaneLaunchEnv::from_extra(extra_env)
            .with_workspace_identity(workspace_id.clone(), public_pane_id);
        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd.clone(),
            argv,
            &launch_env,
            AgentDetection::Disabled,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )?;
        let terminal = TerminalState::new(terminal_id.clone(), cwd).with_launch_argv(argv.to_vec());
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.state.workspace_plugin_panes.insert(
            workspace_id.clone(),
            crate::app::state::WorkspacePluginPaneState {
                pane_id,
                terminal_id,
                plugin_id,
                entrypoint,
                width,
                focused: focus,
                collapsed: !focus,
            },
        );
        if focus {
            self.state.switch_workspace(ws_idx);
            self.state.mode = Mode::Terminal;
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        Ok(())
    }
    pub(super) fn handle_workspace_plugin_mouse(&mut self, mouse: MouseEvent) -> bool {
        let resize_drag = match self.state.drag.as_ref().map(|drag| &drag.target) {
            Some(crate::app::state::DragTarget::WorkspacePluginDivider {
                workspace_id,
                right_edge,
                available_width,
            }) => Some((workspace_id.clone(), *right_edge, *available_width)),
            _ => None,
        };
        if let Some((workspace_id, right_edge, available_width)) = resize_drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.resize_workspace_plugin_pane(
                        &workspace_id,
                        right_edge,
                        available_width,
                        mouse.column,
                    );
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.resize_workspace_plugin_pane(
                        &workspace_id,
                        right_edge,
                        available_width,
                        mouse.column,
                    );
                    self.state.drag = None;
                    return true;
                }
                _ => {}
            }
        }

        let outer = self.state.view.workspace_plugin_pane_outer;
        let inner = self.state.view.workspace_plugin_pane_inner;
        if outer.is_empty()
            || mouse.column < outer.x
            || mouse.column >= outer.x.saturating_add(outer.width)
            || mouse.row < outer.y
            || mouse.row >= outer.y.saturating_add(outer.height)
        {
            return false;
        }
        let Some(workspace_id) = self
            .state
            .active
            .and_then(|ws_idx| self.state.workspaces.get(ws_idx))
            .map(|workspace| workspace.id.clone())
        else {
            return false;
        };
        let Some((pane_id, terminal_id, collapsed)) = self
            .state
            .workspace_plugin_panes
            .get(&workspace_id)
            .map(|pane| (pane.pane_id, pane.terminal_id.clone(), pane.collapsed))
        else {
            return false;
        };
        let toggle = crate::ui::workspace_plugin_pane_toggle_rect(outer);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && mouse.column == toggle.x
            && mouse.row == toggle.y
        {
            self.toggle_workspace_plugin_pane_collapsed(&workspace_id);
            return true;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && !collapsed
            && mouse.column == outer.x
        {
            let right_edge = outer.x.saturating_add(outer.width);
            let available_width = outer
                .width
                .saturating_add(self.state.view.terminal_area.width);
            self.state.drag = Some(crate::app::state::DragState {
                target: crate::app::state::DragTarget::WorkspacePluginDivider {
                    workspace_id: workspace_id.clone(),
                    right_edge,
                    available_width,
                },
            });
            self.resize_workspace_plugin_pane(
                &workspace_id,
                right_edge,
                available_width,
                mouse.column,
            );
            return true;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left | MouseButton::Middle)
        ) {
            if let Some(pane) = self.state.workspace_plugin_panes.get_mut(&workspace_id) {
                pane.focused = true;
            }
            self.state.mode = Mode::Terminal;
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        if mouse.column < inner.x
            || mouse.column >= inner.x.saturating_add(inner.width)
            || mouse.row < inner.y
            || mouse.row >= inner.y.saturating_add(inner.height)
        {
            return true;
        }
        let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
            self.close_workspace_plugin_pane_by_internal_id(pane_id);
            return true;
        };
        let position = crate::input::mouse::Position::Cell {
            column: mouse.column.saturating_sub(inner.x),
            row: mouse.row.saturating_sub(inner.y),
        };
        let bytes = match mouse.kind {
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => match runtime.wheel_routing() {
                Some(crate::pane::WheelRouting::MouseReport) => {
                    runtime.encode_mouse_wheel(mouse.kind, position, mouse.modifiers)
                }
                Some(crate::pane::WheelRouting::AlternateScroll) => {
                    runtime.encode_alternate_scroll(mouse.kind)
                }
                Some(crate::pane::WheelRouting::HostScroll) | None => {
                    let lines_per_notch = self.state.mouse_scroll_lines;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => runtime.scroll_up(lines_per_notch),
                        MouseEventKind::ScrollDown => runtime.scroll_down(lines_per_notch),
                        _ => {}
                    }
                    return true;
                }
            },
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                runtime.encode_mouse_button(mouse.kind, position, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                runtime.encode_mouse_motion(mouse.kind, position, mouse.modifiers)
            }
        };
        let Some(bytes) = bytes else {
            return true;
        };
        if !matches!(mouse.kind, MouseEventKind::Moved) {
            runtime.scroll_reset();
        }
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, kind = ?mouse.kind, "failed to forward workspace plugin mouse event");
        }
        true
    }
}
