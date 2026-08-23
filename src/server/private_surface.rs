use bytes::Bytes;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::app::{ClientPrivatePluginPopupOrigin, ClientPrivatePluginPopupSpec};
use crate::layout::PaneId;
use crate::pane::PaneLaunchEnv;
use crate::popup_size::{resolve_popup_geometry, PopupResolvedGeometry, PopupSize};
use crate::protocol::CursorState;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};

pub(crate) struct PrivateLinkClick {
    pub(crate) url: String,
    pub(crate) origin: ClientPrivatePluginPopupOrigin,
}

pub(crate) struct PrivateSurface {
    pane_id: PaneId,
    terminal: TerminalState,
    runtime: Option<TerminalRuntime>,
    width: Option<PopupSize>,
    height: Option<PopupSize>,
    origin: ClientPrivatePluginPopupOrigin,
    render_area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
    pending_url_click: bool,
}

impl PrivateSurface {
    pub(crate) fn spawn(
        spec: ClientPrivatePluginPopupSpec,
        area: Rect,
        cell_size: crate::kitty_graphics::HostCellSize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        app: &crate::app::App,
    ) -> std::io::Result<Self> {
        let geometry = resolve_popup_geometry(spec.width, spec.height, area)
            .ok_or_else(|| std::io::Error::other("terminal area too small for popup"))?;
        if spec.execution_target.is_local() {
            crate::app::ensure_plugin_user_dirs(&spec.plugin)?;
        }

        let pane_id = PaneId::alloc();
        let terminal_id = TerminalId::alloc();
        let launch_env = PaneLaunchEnv::from_extra(spec.env).without_pane_identity();
        let runtime = TerminalRuntime::spawn_plugin_command_on(
            pane_id,
            geometry.inner.height,
            geometry.inner.width,
            spec.cwd.clone(),
            &spec.execution_target,
            &spec.plugin.plugin_id,
            &spec.entrypoint,
            &spec.command,
            &launch_env,
            crate::pane::AgentDetection::Disabled,
            app.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            app.event_tx.clone(),
            app.render_notify.clone(),
            app.render_dirty.clone(),
        )?;
        runtime.resize(
            geometry.inner.height,
            geometry.inner.width,
            cell_size.width_px,
            cell_size.height_px,
        );

        let mut terminal = TerminalState::new(terminal_id.clone(), spec.cwd)
            .with_execution_target(spec.execution_target)
            .with_launch_argv(spec.command);
        terminal.set_manual_label(spec.title);

        Ok(Self {
            pane_id,
            terminal,
            runtime: Some(runtime),
            width: spec.width,
            height: spec.height,
            origin: spec.origin,
            render_area: area,
            cell_size,
            pending_url_click: false,
        })
    }
    #[cfg(test)]
    pub(crate) fn test_with_screen_bytes(
        area: Rect,
        origin: ClientPrivatePluginPopupOrigin,
        bytes: &[u8],
    ) -> Self {
        let geometry = resolve_popup_geometry(None, None, area).expect("test popup geometry");
        let pane_id = PaneId::alloc();
        let terminal = TerminalState::new(TerminalId::alloc(), std::path::PathBuf::from("/tmp"));
        Self {
            pane_id,
            terminal,
            runtime: Some(TerminalRuntime::test_with_screen_bytes(
                geometry.inner.width,
                geometry.inner.height,
                bytes,
            )),
            width: None,
            height: None,
            origin,
            render_area: area,
            cell_size: crate::kitty_graphics::HostCellSize::default(),
            pending_url_click: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn runtime_size_for_test(&self) -> Option<(u16, u16)> {
        self.runtime.as_ref().map(TerminalRuntime::current_size)
    }
    #[cfg(test)]
    pub(crate) fn render_area_for_test(&self) -> Rect {
        self.render_area
    }

    pub(crate) fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    pub(crate) fn keyboard_report_all_requested(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(TerminalRuntime::keyboard_report_all_requested)
    }

    pub(crate) fn resize(&mut self, area: Rect, cell_size: crate::kitty_graphics::HostCellSize) {
        if self.render_area == area && self.cell_size == cell_size {
            return;
        }
        self.render_area = area;
        self.cell_size = cell_size;
        let Some(geometry) = self.geometry() else {
            return;
        };
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.resize(
                geometry.inner.height,
                geometry.inner.width,
                cell_size.width_px,
                cell_size.height_px,
            );
        }
    }

    pub(crate) fn apply_host_theme(
        &self,
        theme: crate::terminal_theme::TerminalTheme,
        appearance: Option<crate::terminal_theme::HostAppearance>,
    ) {
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.apply_host_terminal_theme(theme);
            runtime.apply_host_terminal_appearance(appearance);
        }
    }

    pub(crate) fn update_cwd(&mut self, cwd: std::path::PathBuf) {
        self.terminal.cwd = cwd;
    }

    pub(crate) fn render(
        &self,
        frame: &mut ratatui::Frame,
        palette: &crate::app::state::Palette,
        area: Rect,
    ) {
        let (Some(runtime), Some(geometry)) = (self.runtime.as_ref(), self.geometry_for(area))
        else {
            return;
        };
        let title = self.terminal.manual_label.as_deref().unwrap_or("popup");
        crate::ui::render_popup_runtime(
            frame,
            geometry.outer,
            geometry.inner,
            runtime,
            title,
            palette,
        );
    }

    pub(crate) fn cursor(&self, area: Rect) -> Option<CursorState> {
        let runtime = self.runtime.as_ref()?;
        if runtime.synchronized_output_active() {
            return None;
        }
        let geometry = self.geometry_for(area)?;
        let cursor = runtime.cursor_state(geometry.inner, true)?;
        Some(CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
    }

    pub(crate) fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        let (Some(runtime), Some(geometry)) = (self.runtime.as_ref(), self.geometry_for(area))
        else {
            return Vec::new();
        };
        runtime.visible_hyperlinks(geometry.inner)
    }

    pub(crate) fn outer_rect(&self, area: Rect) -> Option<Rect> {
        self.geometry_for(area).map(|geometry| geometry.outer)
    }

    pub(crate) fn route_event(
        &mut self,
        event: crate::raw_input::RawInputEvent,
        mouse_scroll_lines: usize,
    ) -> Option<PrivateLinkClick> {
        match event {
            crate::raw_input::RawInputEvent::Key(key) => {
                let runtime = self.runtime.as_ref()?;
                runtime.scroll_reset();
                let bytes = runtime.encode_terminal_key(key);
                if !bytes.is_empty() {
                    let _ = runtime.try_send_bytes(Bytes::from(bytes));
                }
            }
            crate::raw_input::RawInputEvent::Text(text) => {
                let runtime = self.runtime.as_ref()?;
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(text.as_str().as_bytes()));
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                let runtime = self.runtime.as_ref()?;
                let _ = runtime.try_send_paste(text);
            }
            crate::raw_input::RawInputEvent::Mouse(mouse) => {
                return self.route_mouse(mouse, mouse_scroll_lines);
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                self.runtime
                    .as_ref()?
                    .try_send_focus_event(crate::ghostty::FocusEvent::Gained);
            }
            crate::raw_input::RawInputEvent::OuterFocusLost => {
                self.runtime
                    .as_ref()?
                    .try_send_focus_event(crate::ghostty::FocusEvent::Lost);
            }
            crate::raw_input::RawInputEvent::HostDefaultColor { .. }
            | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
            | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
            | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
            | crate::raw_input::RawInputEvent::Unsupported => {}
        }
        None
    }

    pub(crate) fn shutdown(mut self) {
        self.shutdown_runtime();
    }

    fn route_mouse(
        &mut self,
        mouse: MouseEvent,
        mouse_scroll_lines: usize,
    ) -> Option<PrivateLinkClick> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.pending_url_click = false,
            MouseEventKind::Drag(MouseButton::Left) if self.pending_url_click => return None,
            MouseEventKind::Up(MouseButton::Left) if self.pending_url_click => {
                self.pending_url_click = false;
                return None;
            }
            _ => {}
        }

        let geometry = self.geometry()?;
        if !geometry
            .inner
            .contains(ratatui::layout::Position::new(mouse.column, mouse.row))
        {
            return None;
        }
        let viewport_row = mouse.row.saturating_sub(geometry.inner.y);
        let col = mouse.column.saturating_sub(geometry.inner.x);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(runtime) = self.runtime.as_ref() {
                if let Some(link) = crate::app::actions::resolved_terminal_link_at_cell(
                    runtime,
                    viewport_row,
                    col,
                    geometry.inner.width,
                    geometry.inner.height,
                ) {
                    self.pending_url_click = true;
                    return Some(PrivateLinkClick {
                        url: link.url,
                        origin: self.origin,
                    });
                }
            }
        }

        let runtime = self.runtime.as_ref()?;
        let position = crate::input::mouse::Position::Cell {
            column: col,
            row: viewport_row,
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
                    match mouse.kind {
                        MouseEventKind::ScrollUp => runtime.scroll_up(mouse_scroll_lines),
                        MouseEventKind::ScrollDown => runtime.scroll_down(mouse_scroll_lines),
                        _ => {}
                    }
                    return None;
                }
            },
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                runtime.encode_mouse_button(mouse.kind, position, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                runtime.encode_mouse_motion(mouse.kind, position, mouse.modifiers)
            }
        };
        if let Some(bytes) = bytes {
            if !matches!(mouse.kind, MouseEventKind::Moved) {
                runtime.scroll_reset();
            }
            let _ = runtime.try_send_bytes(Bytes::from(bytes));
        }
        None
    }

    fn geometry(&self) -> Option<PopupResolvedGeometry> {
        self.geometry_for(self.render_area)
    }

    fn geometry_for(&self, area: Rect) -> Option<PopupResolvedGeometry> {
        resolve_popup_geometry(self.width, self.height, area)
    }

    fn shutdown_runtime(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }
}

impl Drop for PrivateSurface {
    fn drop(&mut self) {
        self.shutdown_runtime();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[tokio::test]
    async fn plain_click_uses_translated_private_coordinates_and_preserves_raw_url() {
        let area = Rect::new(0, 0, 80, 24);
        let origin = ClientPrivatePluginPopupOrigin::Pane(PaneId::from_raw(7));
        let mut surface = PrivateSurface::test_with_screen_bytes(
            area,
            origin,
            b"\x1b]8;;file:///tmp/private.txt\x1b\\open\x1b]8;;\x1b\\",
        );
        let ((column, row), _, uri) = surface
            .visible_hyperlinks(area)
            .into_iter()
            .next()
            .expect("private hyperlink");
        assert_eq!(uri, "file:///tmp/private.txt");

        let click = surface
            .route_event(
                crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                }),
                3,
            )
            .expect("plain private link click");

        assert_eq!(click.url, "file:///tmp/private.txt");
        assert_eq!(click.origin, origin);
    }

    #[tokio::test]
    async fn resize_updates_only_private_runtime_geometry() {
        let mut surface = PrivateSurface::test_with_screen_bytes(
            Rect::new(0, 0, 80, 24),
            ClientPrivatePluginPopupOrigin::Pane(PaneId::from_raw(8)),
            b"private",
        );
        surface.resize(
            Rect::new(0, 0, 100, 30),
            crate::kitty_graphics::HostCellSize::default(),
        );
        let expected = resolve_popup_geometry(None, None, Rect::new(0, 0, 100, 30))
            .expect("resized popup geometry")
            .inner;
        assert_eq!(
            surface.runtime_size_for_test(),
            Some((expected.height, expected.width))
        );
    }
}
