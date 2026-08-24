use bytes::Bytes;
use crossterm::event::{KeyEventKind, MouseButton, MouseEventKind};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::protocol::{
    ClientInputEvent, ClientKeyCode, ClientKeyKind, ClientKeySource, ClientMessage,
    ClientMouseKind, FrameData, OmpRendererCapabilities, OmpRendererPrefix, OmpRendererRect,
    OmpRendererRoute, RenderEncoding,
};
use crate::render_signal::RenderSignal;
use crate::selection::Selection;
use crate::terminal::TerminalRuntime;

pub(super) const OMP_RENDERER_LAUNCH_ID_ENV: &str = "HERDR_OMP_RENDERER_LAUNCH_ID";

const BIND_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_DAMAGE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_COPY_FEEDBACK_DURATION: Duration = Duration::from_secs(2);
const LOCAL_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(350);

#[derive(Clone, Copy)]
struct LocalClick {
    at: Instant,
    row: u16,
    column: u16,
}

pub(super) fn capabilities(
    encoding: RenderEncoding,
    app_surface: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    omp_executable: bool,
) -> OmpRendererCapabilities {
    OmpRendererCapabilities {
        client_local_native: cfg!(unix)
            && app_surface
            && encoding == RenderEncoding::SemanticFrame
            && stdin_tty
            && stdout_tty
            && omp_executable,
    }
}

pub(super) struct SurfaceFrame {
    pub(super) frame: FrameData,
    pub(super) force_repaint: bool,
}

pub(super) enum LocalEffect {
    Bell(u16),
    ClipboardWrite(Vec<u8>),
    OpenUrl(String),
}

struct LocalTarget {
    launch_id: u64,
    target_app_client_id: u64,
    route: OmpRendererRoute,
    prefix: OmpRendererPrefix,
    runtime: Option<TerminalRuntime>,
    pane_id: PaneId,
    events: mpsc::Receiver<AppEvent>,
    render_dirty: Arc<RenderSignal>,
    rect: OmpRendererRect,
    size: (u16, u16, u32, u32),
    started_at: Instant,
    bound_at: Option<Instant>,
    bound: bool,
    surface_active: bool,
    first_damage: bool,
    ready_reported: bool,
    promoted: bool,
    failed: bool,
    fallback_confirmed: bool,
}

impl LocalTarget {
    fn spawn(
        omp_executable: &crate::update::OmpExecutable,
        launch_id: u64,
        target_app_client_id: u64,
        route: OmpRendererRoute,
        prefix: OmpRendererPrefix,
        rect: OmpRendererRect,
        cell_size: (u32, u32),
        scrollback_limit_bytes: usize,
    ) -> std::io::Result<Self> {
        let (cell_width_px, cell_height_px) = cell_size;
        let cols = rect.width;
        let rows = rect.height;
        let size = (cols, rows, cell_width_px, cell_height_px);
        let pane_id = PaneId::alloc();
        let (events_tx, events) = mpsc::channel(16);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(RenderSignal::new());
        let executable = std::env::current_exe()?;
        let argv = vec![
            executable.to_string_lossy().into_owned(),
            "__omp-pane".into(),
            route.pane_id.clone(),
            route.omp_session_id.clone(),
            route.route_generation.to_string(),
            "--app-client-id".into(),
            target_app_client_id.to_string(),
        ];
        let mut extra_env = vec![(OMP_RENDERER_LAUNCH_ID_ENV.into(), launch_id.to_string())];
        omp_executable.append_launch_env(&mut extra_env);
        let launch_env = crate::pane::PaneLaunchEnv::from_extra(extra_env).without_pane_identity();
        let runtime = TerminalRuntime::spawn_argv_command(
            pane_id,
            rows.max(1),
            cols.max(1),
            std::env::current_dir().unwrap_or_else(|_| "/".into()),
            &argv,
            &launch_env,
            crate::pane::AgentDetection::Disabled,
            scrollback_limit_bytes,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            events_tx,
            render_notify,
            render_dirty.clone(),
        )?;
        runtime.resize(rows.max(1), cols.max(1), cell_width_px, cell_height_px);
        Ok(Self {
            launch_id,
            target_app_client_id,
            route,
            prefix,
            runtime: Some(runtime),
            pane_id,
            events,
            render_dirty,
            size,
            rect,
            started_at: Instant::now(),
            bound_at: None,
            bound: false,
            surface_active: false,
            first_damage: false,
            ready_reported: false,
            promoted: false,
            failed: false,
            fallback_confirmed: false,
        })
    }

    fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }

    fn fail(&mut self) {
        self.failed = true;
        self.bound = false;
        self.surface_active = false;
        self.stop();
    }

    fn resize(&mut self, rect: OmpRendererRect, cell_size: (u32, u32)) {
        self.rect = rect;
        let (cell_width_px, cell_height_px) = cell_size;
        self.size = (rect.width, rect.height, cell_width_px, cell_height_px);
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        runtime.resize(
            rect.height.max(1),
            rect.width.max(1),
            cell_width_px,
            cell_height_px,
        );
    }

    fn poll(&mut self, now: Instant, effects: &mut Vec<LocalEffect>) -> bool {
        let mut damaged = false;
        if self.render_dirty.is_pending() {
            let request = self.render_dirty.take();
            damaged = request.pty_sources.contains(&self.pane_id);
            self.first_damage |= damaged;
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                AppEvent::PaneDied { pane_id } if pane_id == self.pane_id => {
                    self.fail();
                    return true;
                }
                AppEvent::TerminalBell { pane_id, count }
                    if self.promoted && pane_id == self.pane_id =>
                {
                    effects.push(LocalEffect::Bell(count));
                }
                AppEvent::ClipboardWrite { content } if self.promoted && self.surface_active => {
                    effects.push(LocalEffect::ClipboardWrite(content));
                }
                AppEvent::OpenUrl { url, .. } if self.promoted && self.surface_active => {
                    effects.push(LocalEffect::OpenUrl(url));
                }
                _ => {}
            }
        }
        let timed_out = !self.bound && now.duration_since(self.started_at) >= BIND_TIMEOUT
            || self.bound
                && !self.first_damage
                && self
                    .bound_at
                    .is_some_and(|bound_at| now.duration_since(bound_at) >= FIRST_DAMAGE_TIMEOUT);
        if timed_out {
            self.fail();
            return true;
        }
        damaged
    }
    fn frame(&self, selection: Option<&Selection>) -> Option<FrameData> {
        if self.rect.is_empty() {
            return None;
        }
        let runtime = self.runtime.as_ref()?;
        let area = Rect::new(0, 0, self.rect.width, self.rect.height);
        let (mut buffer, cursor) =
            crate::server::render_stream::render_terminal_virtual(runtime, area);
        if let Some(selection) = selection.filter(|selection| selection.pane_id == self.pane_id) {
            let metrics = runtime.scroll_metrics();
            for y in 0..area.height {
                for x in 0..area.width {
                    if selection.contains(y, x, metrics) {
                        let cell = &mut buffer[(x, y)];
                        cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
        let hyperlinks = runtime.visible_hyperlinks(area);
        Some(FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            cursor,
            &hyperlinks,
        ))
    }
}

#[derive(Default)]
pub(super) struct ClientOmpRenderer {
    omp_executable: Option<crate::update::OmpExecutable>,
    scrollback_limit_bytes: usize,
    mouse_scroll_lines: usize,
    copy_on_select: bool,
    copy_feedback_enabled: bool,
    copy_feedback_position: crate::config::ToastClipboardPosition,
    copy_feedback_deadline: Option<Instant>,
    latest_launch_id: u64,
    attempted_launches: HashSet<u64>,
    target: Option<LocalTarget>,
    cached_server_frame: Option<FrameData>,
    local_selected: bool,
    server_owned_input: bool,
    awaiting_fallback: bool,
    awaiting_promotion: bool,
    deferred_messages: Vec<ClientMessage>,
    outbound_messages: Vec<ClientMessage>,
    effects: Vec<LocalEffect>,
    needs_render: bool,
    force_repaint: bool,
    selection: Option<Selection>,
    last_click: Option<LocalClick>,
    suppress_copy_key: bool,
}

impl ClientOmpRenderer {
    pub(super) fn new(
        omp_executable: Option<crate::update::OmpExecutable>,
        scrollback_limit_bytes: usize,
        mouse_scroll_lines: usize,
        copy_on_select: bool,
        copy_feedback_enabled: bool,
        copy_feedback_position: crate::config::ToastClipboardPosition,
    ) -> Self {
        Self {
            omp_executable,
            scrollback_limit_bytes,
            mouse_scroll_lines,
            copy_on_select,
            copy_feedback_enabled,
            copy_feedback_position,
            copy_feedback_deadline: None,
            latest_launch_id: 0,
            attempted_launches: HashSet::new(),
            target: None,
            cached_server_frame: None,
            local_selected: false,
            server_owned_input: false,
            awaiting_fallback: false,
            awaiting_promotion: false,
            deferred_messages: Vec::new(),
            outbound_messages: Vec::new(),
            effects: Vec::new(),
            needs_render: false,
            force_repaint: false,
            selection: None,
            last_click: None,
            suppress_copy_key: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_target(
        &mut self,
        launch_id: u64,
        target_app_client_id: u64,
        route: Option<OmpRendererRoute>,
        bound: bool,
        surface_active: bool,
        prefix: OmpRendererPrefix,
        rect: OmpRendererRect,
        cell_size: (u32, u32),
    ) {
        if self.omp_executable.is_none() || launch_id < self.latest_launch_id {
            return;
        }
        if route.is_none() {
            self.latest_launch_id = launch_id;
            self.stop_target();
            self.discard_deferred_messages();
            return;
        }
        let route = route.expect("route checked above");
        let replace = launch_id > self.latest_launch_id
            || self.target.as_ref().is_some_and(|target| {
                target.launch_id != launch_id
                    || target.target_app_client_id != target_app_client_id
                    || target.route != route
            });
        if replace {
            self.stop_target();
            self.discard_deferred_messages();
            self.latest_launch_id = launch_id;
            if !self.attempted_launches.insert(launch_id) {
                return;
            }
            let target = self.omp_executable.as_ref().and_then(|omp_executable| {
                LocalTarget::spawn(
                    omp_executable,
                    launch_id,
                    target_app_client_id,
                    route,
                    prefix.clone(),
                    rect,
                    cell_size,
                    self.scrollback_limit_bytes,
                )
                .ok()
            });
            self.target = target;
            self.needs_render = true;
        }
        if !bound {
            self.release_deferred_messages();
        }
        let mut confirm_promotion = None;
        let Some(target) = self
            .target
            .as_mut()
            .filter(|target| target.launch_id == launch_id)
        else {
            return;
        };
        let was_surface_active = target.surface_active;
        if !bound && target.ready_reported {
            target.fallback_confirmed = true;
        }
        if target.bound && !bound {
            target.fail();
        }
        if bound && !target.bound {
            target.bound_at = Some(Instant::now());
        }
        target.bound = bound && !target.failed;
        target.surface_active = surface_active && !target.failed;
        target.promoted = target.ready_reported && target.bound;
        target.prefix = prefix;
        if was_surface_active && !target.surface_active {
            self.selection = None;
            self.last_click = None;
            self.suppress_copy_key = false;
            self.effects.clear();
            self.needs_render = true;
            self.force_repaint = true;
        }
        if target.bound && target.surface_active {
            self.server_owned_input = false;
        }
        if self.awaiting_promotion && target.ready_reported && target.bound {
            confirm_promotion = Some(
                target.surface_active
                    && target.first_damage
                    && target.runtime.is_some()
                    && !target.failed
                    && !self.server_owned_input,
            );
        }
        target.resize(rect, cell_size);
        self.needs_render = true;
        if let Some(local_active) = confirm_promotion {
            if local_active {
                self.local_selected = true;
                self.needs_render = true;
                self.force_repaint = true;
            }
            self.resolve_promotion(local_active);
        }
    }

    pub(super) fn cache_server_frame(&mut self, frame: FrameData) -> Option<SurfaceFrame> {
        self.cached_server_frame = Some(frame.clone());
        let frame = if self.local_selected {
            self.composed_frame()?
        } else {
            frame
        };
        Some(SurfaceFrame {
            frame,
            force_repaint: false,
        })
    }

    fn route_local_selection_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        position: Option<crate::input::mouse::Position>,
    ) -> bool {
        let Self {
            target,
            selection,
            effects,
            copy_on_select,
            last_click,
            ..
        } = self;
        let Some(target) = target.as_ref() else {
            return false;
        };
        let Some(runtime) = target.runtime.as_ref() else {
            return false;
        };
        let area = Rect::new(0, 0, target.rect.width, target.rect.height);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if position.is_some_and(|position| {
                    runtime
                        .encode_mouse_button(mouse.kind, position, mouse.modifiers)
                        .is_some()
                }) {
                    *selection = None;
                    *last_click = None;
                    return false;
                }
                let now = Instant::now();
                let is_double_click = mouse.modifiers.is_empty()
                    && last_click.is_some_and(|last| {
                        now.duration_since(last.at) <= LOCAL_DOUBLE_CLICK_WINDOW
                            && last.row.abs_diff(mouse.row) <= 1
                            && last.column.abs_diff(mouse.column) <= 1
                    });
                *last_click = None;
                if is_double_click {
                    if let Some((selected, content)) = Self::local_word_selection(
                        target.pane_id,
                        runtime,
                        mouse.row,
                        mouse.column,
                        target.rect.width,
                        *copy_on_select,
                    ) {
                        *selection = Some(selected);
                        if let Some(content) = content {
                            effects.push(LocalEffect::ClipboardWrite(content));
                        }
                        return true;
                    }
                }
                if mouse.modifiers.is_empty() {
                    *last_click = Some(LocalClick {
                        at: now,
                        row: mouse.row,
                        column: mouse.column,
                    });
                }
                *selection = Some(Selection::anchor(
                    target.pane_id,
                    mouse.row,
                    mouse.column,
                    runtime.scroll_metrics(),
                ));
                true
            }
            MouseEventKind::Drag(MouseButton::Left)
                if selection.as_ref().is_some_and(Selection::is_in_progress) =>
            {
                *last_click = None;
                if let Some(selection) = selection.as_mut() {
                    selection.drag(mouse.column, mouse.row, area, runtime.scroll_metrics());
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) if selection.is_some() => {
                let was_click = selection.as_ref().is_some_and(Selection::was_just_click);
                let was_finalized = selection.as_ref().is_some_and(Selection::is_finalized);
                if was_click {
                    *selection = None;
                } else {
                    *last_click = None;
                    if !was_finalized && *copy_on_select {
                        let mut selected = selection.take().expect("selection checked above");
                        if selected.finish() {
                            if let Some(text) = runtime
                                .extract_selection(&selected)
                                .filter(|text| !text.is_empty())
                            {
                                effects.push(LocalEffect::ClipboardWrite(text.into_bytes()));
                            }
                        }
                    } else if !was_finalized {
                        if let Some(selection) = selection.as_mut() {
                            selection.finish();
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn local_word_selection(
        pane_id: PaneId,
        runtime: &TerminalRuntime,
        row: u16,
        column: u16,
        width: u16,
        copy_on_select: bool,
    ) -> Option<(Selection, Option<Vec<u8>>)> {
        let metrics = runtime.scroll_metrics();
        let row_selection = Selection::range(pane_id, row, 0, width.saturating_sub(1), metrics);
        let row_text = runtime.extract_selection(&row_selection)?;
        let (start_col, end_col) = crate::app::actions::word_bounds_at_column(&row_text, column)?;
        let mut selection = Selection::range(pane_id, row, start_col, end_col, metrics);
        if !selection.finish() {
            return None;
        }
        let content = copy_on_select
            .then(|| runtime.extract_selection(&selection))
            .flatten()
            .filter(|text| !text.is_empty())
            .map(String::into_bytes);
        Some((selection, content))
    }

    fn try_copy_retained_selection(&mut self, event: &crate::raw_input::RawInputEvent) -> bool {
        let crate::raw_input::RawInputEvent::Key(key) = event else {
            return false;
        };
        if self.suppress_copy_key && matches!(key.code, crossterm::event::KeyCode::Char('c' | 'C'))
        {
            if key.kind == KeyEventKind::Press {
                self.suppress_copy_key = false;
            } else {
                if key.kind == KeyEventKind::Release {
                    self.suppress_copy_key = false;
                }
                return true;
            }
        }
        let is_copy_key = matches!(key.code, crossterm::event::KeyCode::Char('c' | 'C'))
            && matches!(
                key.modifiers,
                crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SUPER
            );
        if !is_copy_key
            || self.copy_on_select
            || key.kind == KeyEventKind::Release
            || !self.selection.as_ref().is_some_and(Selection::is_finalized)
        {
            return false;
        }
        let selected = self.selection.take().expect("selection checked above");
        let content = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .and_then(|runtime| runtime.extract_selection(&selected))
            .filter(|text| !text.is_empty());
        self.needs_render = true;
        self.force_repaint = true;
        if let Some(content) = content {
            self.suppress_copy_key = true;
            self.effects
                .push(LocalEffect::ClipboardWrite(content.into_bytes()));
            true
        } else {
            false
        }
    }

    fn clear_selection_for_input(&mut self, event: &crate::raw_input::RawInputEvent) {
        let clears = matches!(
            event,
            crate::raw_input::RawInputEvent::Key(_)
                | crate::raw_input::RawInputEvent::Text(_)
                | crate::raw_input::RawInputEvent::Paste(_)
                | crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(_),
                    ..
                })
        );
        if clears && self.selection.take().is_some() {
            self.needs_render = true;
            self.force_repaint = true;
        }
    }

    pub(super) fn update_config(
        &mut self,
        mouse_scroll_lines: usize,
        copy_on_select: bool,
        copy_feedback_enabled: bool,
        copy_feedback_position: crate::config::ToastClipboardPosition,
    ) {
        self.mouse_scroll_lines = mouse_scroll_lines;
        self.copy_on_select = copy_on_select;
        self.copy_feedback_position = copy_feedback_position;
        if self.copy_feedback_enabled && !copy_feedback_enabled {
            self.copy_feedback_deadline = None;
            self.needs_render = true;
            self.force_repaint = true;
        }
        self.copy_feedback_enabled = copy_feedback_enabled;
    }

    pub(super) fn resize(&mut self, cell_size: (u32, u32)) {
        if let Some(target) = self.target.as_mut() {
            target.resize(target.rect, cell_size);
        }
        self.force_repaint = true;
        self.needs_render = true;
    }

    pub(super) fn owns_input(&self) -> bool {
        self.local_selected
            || self.server_owned_input
            || self.awaiting_fallback
            || self.awaiting_promotion
    }

    pub(super) fn route_input(
        &mut self,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> Vec<ClientMessage> {
        let mut messages = Vec::new();
        let mut server_batch = Vec::new();
        let mut deferred_events = Vec::new();
        for event in events {
            if self.suppress_copy_key && self.try_copy_retained_selection(&event) {
                continue;
            }
            let protocol_event = client_event_from_raw(&event);
            let continuing_selection = self
                .selection
                .as_ref()
                .is_some_and(Selection::is_in_progress)
                && matches!(
                    &event,
                    crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                        kind: MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left),
                        ..
                    })
                );
            let outside_local_mouse = !continuing_selection
                && matches!(
                    &event,
                    crate::raw_input::RawInputEvent::Mouse(mouse)
                        if self.target.as_ref().is_some_and(|target| {
                            !target.rect.is_empty()
                                && !Rect::new(
                                    target.rect.x,
                                    target.rect.y,
                                    target.rect.width,
                                    target.rect.height,
                                )
                                .contains((mouse.column, mouse.row).into())
                        })
                );
            if outside_local_mouse {
                self.clear_selection_for_input(&event);
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                }
                continue;
            }
            if self.awaiting_fallback || self.awaiting_promotion {
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
                }
                continue;
            }
            let prefix = self.local_selected
                && matches!(&event, crate::raw_input::RawInputEvent::Key(key)
                if key.kind != KeyEventKind::Release
                    && self.target.as_ref().is_some_and(|target| {
                        crate::config::terminal_key_matches_combo(key, target.prefix.key_combo())
                    }));
            if prefix {
                self.clear_selection_for_input(&event);
                if !server_batch.is_empty() {
                    messages.push(ClientMessage::InputEvents {
                        events: std::mem::take(&mut server_batch),
                    });
                }
                if let Some(event) = protocol_event {
                    messages.push(ClientMessage::InputEvents {
                        events: vec![event],
                    });
                }
                self.server_owned_input = true;
                self.needs_render = true;
                self.force_repaint = true;
                continue;
            }
            if self.server_owned_input || !self.local_selected {
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                }
                continue;
            }
            let rect = self
                .target
                .as_ref()
                .map(|target| target.rect)
                .unwrap_or_default();
            let forward_focus_to_server = matches!(
                &event,
                crate::raw_input::RawInputEvent::OuterFocusGained
                    | crate::raw_input::RawInputEvent::OuterFocusLost
            );
            let local_event = translate_local_event(event, rect);
            let outcome = if self.try_copy_retained_selection(&local_event) {
                Some(LocalInputOutcome::Redraw)
            } else {
                let selection_handled = match &local_event {
                    crate::raw_input::RawInputEvent::Mouse(mouse) => self
                        .route_local_selection_mouse(
                            *mouse,
                            Some(crate::input::mouse::Position::Cell {
                                column: mouse.column,
                                row: mouse.row,
                            }),
                        ),
                    _ => false,
                };
                if selection_handled {
                    Some(LocalInputOutcome::Redraw)
                } else {
                    self.clear_selection_for_input(&local_event);
                    self.target
                        .as_ref()
                        .and_then(|target| target.runtime.as_ref())
                        .map(|runtime| {
                            forward_local_event(
                                runtime,
                                local_event,
                                self.mouse_scroll_lines,
                                rect.height,
                            )
                        })
                }
            };
            match outcome {
                Some(LocalInputOutcome::Handled) => {
                    if forward_focus_to_server {
                        if let Some(event) = protocol_event {
                            server_batch.push(event);
                        }
                    }
                }
                Some(LocalInputOutcome::Redraw) => {
                    if forward_focus_to_server {
                        if let Some(event) = protocol_event {
                            server_batch.push(event);
                        }
                    }
                    self.needs_render = true;
                    self.force_repaint = true;
                }
                Some(LocalInputOutcome::Failed) | None => {
                    if let Some(target) = self.target.as_mut() {
                        target.fail();
                    }
                    self.awaiting_fallback = true;
                    self.needs_render = true;
                    self.force_repaint = true;
                    if let Some(event) = protocol_event {
                        deferred_events.push(event);
                    }
                }
            }
        }
        if !server_batch.is_empty() {
            messages.push(ClientMessage::InputEvents {
                events: server_batch,
            });
        }
        if !deferred_events.is_empty() {
            self.deferred_messages.push(ClientMessage::InputEvents {
                events: deferred_events,
            });
        }
        messages
    }

    pub(super) fn route_pixel_input(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
    ) -> Option<ClientMessage> {
        let message = ClientMessage::InputPixels {
            data: data.clone(),
            cols: geometry.cols,
            rows: geometry.rows,
            width_px: geometry.width_px,
            height_px: geometry.height_px,
        };
        let local_pixel = self
            .target
            .as_ref()
            .and_then(|target| parse_local_pixel_event(&data, geometry, target.rect, target.size));
        let continuing_selection = self
            .selection
            .as_ref()
            .is_some_and(Selection::is_in_progress)
            && local_pixel.as_ref().is_some_and(|(mouse, _)| {
                matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                )
            });
        let outside_local_pane = !continuing_selection
            && self.target.as_ref().is_some_and(|target| {
                !target.rect.is_empty()
                    && crate::input::mouse::parse_report(&data)
                        .and_then(|(x, y)| geometry.cell(x, y))
                        .is_some_and(|position| {
                            !Rect::new(
                                target.rect.x,
                                target.rect.y,
                                target.rect.width,
                                target.rect.height,
                            )
                            .contains(position.into())
                        })
            });
        if outside_local_pane {
            return Some(message);
        }
        if self.awaiting_fallback || self.awaiting_promotion {
            self.deferred_messages.push(message);
            return None;
        }
        if self.server_owned_input || !self.local_selected {
            return Some(message);
        }
        if let Some((mouse, position)) = local_pixel {
            if self.route_local_selection_mouse(mouse, position) {
                self.needs_render = true;
                self.force_repaint = true;
                return None;
            }
        }
        let outcome = self.target.as_ref().and_then(|target| {
            target.runtime.as_ref().and_then(|runtime| {
                forward_local_pixel_event(
                    runtime,
                    &data,
                    geometry,
                    target.rect,
                    target.size,
                    self.mouse_scroll_lines,
                )
            })
        });
        match outcome {
            Some(LocalInputOutcome::Handled) => None,
            Some(LocalInputOutcome::Redraw) => {
                self.needs_render = true;
                self.force_repaint = true;
                None
            }
            None => Some(message),
            Some(LocalInputOutcome::Failed) => {
                if let Some(target) = self.target.as_mut() {
                    target.fail();
                }
                self.awaiting_fallback = true;
                self.deferred_messages.push(message);
                self.needs_render = true;
                self.force_repaint = true;
                None
            }
        }
    }

    pub(super) fn next_frame(&mut self, now: Instant, _size: (u16, u16)) -> Option<SurfaceFrame> {
        if self
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.copy_feedback_deadline = None;
            self.needs_render = true;
            self.force_repaint = true;
        }
        let damaged = self
            .target
            .as_mut()
            .is_some_and(|target| target.poll(now, &mut self.effects));
        if let Some(target) = self.target.as_mut() {
            if target.bound
                && target.first_damage
                && target.runtime.is_some()
                && !target.rect.is_empty()
                && self.cached_server_frame.is_some()
                && !target.failed
                && !target.ready_reported
            {
                target.ready_reported = true;
                self.outbound_messages
                    .push(ClientMessage::OmpRendererReady {
                        launch_id: target.launch_id,
                    });
                self.awaiting_promotion = true;
            }
            if target.failed && target.ready_reported && !target.fallback_confirmed {
                self.awaiting_promotion = false;
                self.awaiting_fallback = true;
            }
        }
        let should_select = self.target.as_ref().is_some_and(|target| {
            !target.failed
                && target.runtime.is_some()
                && target.bound
                && target.surface_active
                && !target.rect.is_empty()
                && target.first_damage
                && !self.server_owned_input
        });
        if should_select != self.local_selected {
            self.local_selected = should_select;
            if !should_select {
                self.selection = None;
                self.last_click = None;
                self.suppress_copy_key = false;
            }
            self.needs_render = true;
            self.force_repaint = true;
        }
        if self.local_selected && (damaged || self.needs_render) {
            let frame = self.composed_frame()?;
            let force_repaint = std::mem::take(&mut self.force_repaint);
            self.needs_render = false;
            return Some(SurfaceFrame {
                frame,
                force_repaint,
            });
        }
        if !self.local_selected && self.needs_render {
            self.needs_render = false;
            let force_repaint = std::mem::take(&mut self.force_repaint);
            return self.cached_server_frame.clone().map(|frame| SurfaceFrame {
                frame,
                force_repaint,
            });
        }
        None
    }
    fn composed_frame(&self) -> Option<FrameData> {
        let base = self.cached_server_frame.as_ref()?;
        let target = self.target.as_ref()?;
        let mut frame = compose_frame(base, target.frame(self.selection.as_ref())?, target.rect)?;
        if self.copy_feedback_active(Instant::now()) {
            overlay_copy_feedback(&mut frame, self.copy_feedback_position);
        }
        Some(frame)
    }

    pub(super) fn take_outbound_messages(&mut self) -> Vec<ClientMessage> {
        std::mem::take(&mut self.outbound_messages)
    }

    pub(super) fn take_effects(&mut self) -> Vec<LocalEffect> {
        std::mem::take(&mut self.effects)
    }

    pub(super) fn show_copy_feedback(&mut self, now: Instant) -> bool {
        if !self.copy_feedback_enabled {
            return false;
        }
        self.copy_feedback_deadline = Some(now + LOCAL_COPY_FEEDBACK_DURATION);
        self.needs_render = true;
        self.force_repaint = true;
        true
    }

    pub(super) fn copy_feedback_active(&self, now: Instant) -> bool {
        self.copy_feedback_enabled
            && self
                .copy_feedback_deadline
                .is_some_and(|deadline| now < deadline)
    }

    fn resolve_promotion(&mut self, local_active: bool) {
        self.awaiting_promotion = false;
        if !local_active {
            self.outbound_messages.append(&mut self.deferred_messages);
            return;
        }
        let deferred = std::mem::take(&mut self.deferred_messages);
        for message in deferred {
            if self.awaiting_fallback {
                self.deferred_messages.push(message);
                continue;
            }
            match message {
                ClientMessage::InputEvents { events } => {
                    let events = events
                        .iter()
                        .map(ClientInputEvent::to_raw_input_event)
                        .collect();
                    let messages = self.route_input(events);
                    self.outbound_messages.extend(messages);
                }
                ClientMessage::InputPixels {
                    data,
                    cols,
                    rows,
                    width_px,
                    height_px,
                } => {
                    let Some(geometry) =
                        crate::input::mouse::HostGeometry::new(cols, rows, width_px, height_px)
                    else {
                        self.outbound_messages.push(ClientMessage::InputPixels {
                            data,
                            cols,
                            rows,
                            width_px,
                            height_px,
                        });
                        continue;
                    };
                    if let Some(message) = self.route_pixel_input(data, geometry) {
                        self.outbound_messages.push(message);
                    }
                }
                message => self.outbound_messages.push(message),
            }
        }
    }

    fn release_deferred_messages(&mut self) {
        if self.awaiting_fallback || self.awaiting_promotion {
            self.awaiting_fallback = false;
            self.awaiting_promotion = false;
            self.outbound_messages.append(&mut self.deferred_messages);
        }
    }

    fn discard_deferred_messages(&mut self) {
        self.awaiting_fallback = false;
        self.awaiting_promotion = false;
        self.deferred_messages.clear();
        self.outbound_messages.clear();
        self.effects.clear();
    }

    fn stop_target(&mut self) {
        self.selection = None;
        self.last_click = None;
        self.suppress_copy_key = false;
        if let Some(mut target) = self.target.take() {
            target.stop();
        }
        if self.local_selected || self.server_owned_input {
            self.force_repaint = true;
        }
        self.local_selected = false;
        self.server_owned_input = false;
        self.needs_render = true;
    }
}

impl Drop for ClientOmpRenderer {
    fn drop(&mut self) {
        self.stop_target();
    }
}

fn client_event_from_raw(event: &crate::raw_input::RawInputEvent) -> Option<ClientInputEvent> {
    match event {
        crate::raw_input::RawInputEvent::Key(key) => Some(ClientInputEvent::Key {
            code: ClientKeyCode::from_crossterm(key.code)?,
            modifiers: key.modifiers.bits(),
            kind: ClientKeyKind::from_crossterm(key.kind),
            repeat_count: key.repeat_count,
            generated_text: key.generated_text.clone(),
            source: key
                .vt_bytes()
                .map_or(ClientKeySource::Synthesized, |bytes| ClientKeySource::Vt {
                    bytes: bytes.to_vec(),
                }),
        }),
        crate::raw_input::RawInputEvent::Text(text) => {
            Some(ClientInputEvent::TextCommit(text.as_str().to_owned()))
        }
        crate::raw_input::RawInputEvent::Paste(text) => {
            Some(ClientInputEvent::Paste { text: text.clone() })
        }
        crate::raw_input::RawInputEvent::Mouse(mouse) => Some(ClientInputEvent::Mouse {
            kind: ClientMouseKind::from_crossterm(mouse.kind)?,
            column: mouse.column,
            row: mouse.row,
            modifiers: mouse.modifiers.bits(),
        }),
        crate::raw_input::RawInputEvent::OuterFocusGained => Some(ClientInputEvent::FocusGained),
        crate::raw_input::RawInputEvent::OuterFocusLost => Some(ClientInputEvent::FocusLost),
        crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => None,
    }
}

fn translate_local_event(
    event: crate::raw_input::RawInputEvent,
    rect: OmpRendererRect,
) -> crate::raw_input::RawInputEvent {
    match event {
        crate::raw_input::RawInputEvent::Mouse(mut mouse) => {
            mouse.column = mouse.column.saturating_sub(rect.x);

            mouse.row = mouse.row.saturating_sub(rect.y);
            crate::raw_input::RawInputEvent::Mouse(mouse)
        }
        event => event,
    }
}

fn overlay_copy_feedback(frame: &mut FrameData, position: crate::config::ToastClipboardPosition) {
    const MESSAGE: &str = "copied to clipboard";
    let width = (MESSAGE.len() as u16 + 4).min(frame.width);
    let height = 3u16.min(frame.height);
    if width < 3 || height < 3 {
        return;
    }
    let x = match position {
        crate::config::ToastClipboardPosition::TopLeft
        | crate::config::ToastClipboardPosition::BottomLeft => 0,
        crate::config::ToastClipboardPosition::TopCenter
        | crate::config::ToastClipboardPosition::BottomCenter => {
            frame.width.saturating_sub(width) / 2
        }
        crate::config::ToastClipboardPosition::TopRight
        | crate::config::ToastClipboardPosition::BottomRight => frame.width.saturating_sub(width),
    };
    let y = match position {
        crate::config::ToastClipboardPosition::TopLeft
        | crate::config::ToastClipboardPosition::TopCenter
        | crate::config::ToastClipboardPosition::TopRight => 0,
        crate::config::ToastClipboardPosition::BottomLeft
        | crate::config::ToastClipboardPosition::BottomCenter
        | crate::config::ToastClipboardPosition::BottomRight => frame.height - height,
    };
    let fallback = frame
        .cells
        .first()
        .cloned()
        .unwrap_or(crate::protocol::CellData {
            symbol: " ".to_owned(),
            fg: crate::protocol::color_to_u32(Color::White),
            bg: crate::protocol::color_to_u32(Color::Rgb(24, 24, 37)),
            modifier: 0,
            skip: false,
            hyperlink: None,
        });
    let panel_bg = if fallback.bg == crate::protocol::color_to_u32(Color::Reset) {
        crate::protocol::color_to_u32(Color::Rgb(24, 24, 37))
    } else {
        fallback.bg
    };
    let text_fg = if fallback.fg == crate::protocol::color_to_u32(Color::Reset) {
        crate::protocol::color_to_u32(Color::Rgb(205, 214, 244))
    } else {
        fallback.fg
    };
    let green = crate::protocol::color_to_u32(Color::Green);
    for row in 0..height {
        for column in 0..width {
            let border = row == 0 || row == height - 1 || column == 0 || column == width - 1;
            let mut cell = fallback.clone();
            cell.symbol = " ".to_owned();
            cell.bg = panel_bg;
            cell.fg = if border { green } else { text_fg };
            cell.modifier = 0;
            cell.skip = false;
            cell.hyperlink = None;
            if border {
                cell.symbol = match (row, column) {
                    (0, 0) => "┌",
                    (0, c) if c == width - 1 => "┐",
                    (r, 0) if r == height - 1 => "└",
                    (r, c) if r == height - 1 && c == width - 1 => "┘",
                    (0, _) => "─",
                    (r, _) if r == height - 1 => "─",
                    (_, _) => "│",
                }
                .to_owned();
            } else if row == 1 && column == 1 {
                cell.symbol = "●".to_owned();
                cell.fg = green;
            } else if row == 1 && column >= 3 {
                let index = usize::from(column - 3);
                if let Some(character) = MESSAGE.chars().nth(index) {
                    cell.symbol = character.to_string();
                    cell.modifier = Modifier::BOLD.bits();
                }
            }
            let index = usize::from(y + row) * usize::from(frame.width) + usize::from(x + column);
            if let Some(destination) = frame.cells.get_mut(index) {
                *destination = cell;
            }
        }
    }
}
fn compose_frame(base: &FrameData, local: FrameData, rect: OmpRendererRect) -> Option<FrameData> {
    let base_cells = usize::from(base.width).checked_mul(usize::from(base.height))?;
    let local_cells = usize::from(local.width).checked_mul(usize::from(local.height))?;
    if base.cells.len() != base_cells || local.cells.len() != local_cells {
        return None;
    }
    let mut composed = base.clone();
    let hyperlink_offset = u32::try_from(composed.hyperlinks.len()).ok()?;
    composed.hyperlinks.extend(local.hyperlinks);
    let width = rect
        .width
        .min(local.width)
        .min(base.width.saturating_sub(rect.x));
    let height = rect
        .height
        .min(local.height)
        .min(base.height.saturating_sub(rect.y));
    for y in 0..height {
        for x in 0..width {
            let local_index = usize::from(y) * usize::from(local.width) + usize::from(x);
            let base_index =
                usize::from(rect.y + y) * usize::from(base.width) + usize::from(rect.x + x);
            let mut cell = local.cells[local_index].clone();
            if let Some(index) = cell.hyperlink.as_mut() {
                *index = index.checked_add(hyperlink_offset)?;
            }
            composed.cells[base_index] = cell;
        }
    }
    composed.cursor = local.cursor.map(|cursor| crate::protocol::CursorState {
        x: rect.x.saturating_add(cursor.x),
        y: rect.y.saturating_add(cursor.y),
        visible: cursor.visible,
        shape: cursor.shape,
    });
    Some(composed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalInputOutcome {
    Handled,
    Redraw,
    Failed,
}

fn reset_local_scroll(runtime: &TerminalRuntime) -> bool {
    let changed = runtime
        .scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0);
    runtime.scroll_reset();
    changed
}

fn forward_local_bytes(
    runtime: &TerminalRuntime,
    bytes: Option<Vec<u8>>,
    reset_scroll: bool,
) -> LocalInputOutcome {
    let Some(bytes) = bytes else {
        return LocalInputOutcome::Handled;
    };
    let redraw = reset_scroll && reset_local_scroll(runtime);
    if bytes.is_empty() || runtime.try_send_bytes(Bytes::from(bytes)).is_ok() {
        if redraw {
            LocalInputOutcome::Redraw
        } else {
            LocalInputOutcome::Handled
        }
    } else {
        LocalInputOutcome::Failed
    }
}

fn forward_local_mouse_event(
    runtime: &TerminalRuntime,
    kind: MouseEventKind,
    position: crate::input::mouse::Position,
    modifiers: crossterm::event::KeyModifiers,
    mouse_scroll_lines: usize,
) -> LocalInputOutcome {
    match kind {
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => match runtime.wheel_routing() {
            Some(crate::pane::WheelRouting::MouseReport) => forward_local_bytes(
                runtime,
                runtime.encode_mouse_wheel(kind, position, modifiers),
                true,
            ),
            Some(crate::pane::WheelRouting::AlternateScroll) => {
                forward_local_bytes(runtime, runtime.encode_alternate_scroll(kind), true)
            }
            Some(crate::pane::WheelRouting::HostScroll) | None => {
                match kind {
                    MouseEventKind::ScrollUp => runtime.scroll_up(mouse_scroll_lines.max(1)),
                    MouseEventKind::ScrollDown => runtime.scroll_down(mouse_scroll_lines.max(1)),
                    MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                        return LocalInputOutcome::Handled;
                    }
                    _ => unreachable!("wheel event matched above"),
                }
                LocalInputOutcome::Redraw
            }
        },
        MouseEventKind::Moved => forward_local_bytes(
            runtime,
            runtime.encode_mouse_motion(kind, position, modifiers),
            false,
        ),
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
            forward_local_bytes(
                runtime,
                runtime.encode_mouse_button(kind, position, modifiers),
                true,
            )
        }
    }
}

fn parse_local_pixel_event(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    rect: OmpRendererRect,
    size: (u16, u16, u32, u32),
) -> Option<(
    crossterm::event::MouseEvent,
    Option<crate::input::mouse::Position>,
)> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let (column, row) = geometry.cell(x, y)?;
    let cell_report = crate::input::mouse::report_at_cell(data, column, row)?;
    let mouse = crate::raw_input::parse_raw_input_bytes_sync(&cell_report)
        .into_iter()
        .find_map(|event| match event {
            crate::raw_input::RawInputEvent::Mouse(mouse) => Some(mouse),
            _ => None,
        })?;
    let (cols, rows, cell_width_px, cell_height_px) = size;
    let child_width_px = u32::from(cols).checked_mul(cell_width_px)?;
    let child_height_px = u32::from(rows).checked_mul(cell_height_px)?;
    let position = crate::input::mouse::HostPixels { x, y, geometry }.pane_position(
        Rect::new(rect.x, rect.y, rect.width, rect.height),
        child_width_px,
        child_height_px,
    );
    Some((
        crossterm::event::MouseEvent {
            column: column.saturating_sub(rect.x),
            row: row.saturating_sub(rect.y),
            ..mouse
        },
        position,
    ))
}

fn forward_local_pixel_event(
    runtime: &TerminalRuntime,
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    rect: OmpRendererRect,
    size: (u16, u16, u32, u32),
    mouse_scroll_lines: usize,
) -> Option<LocalInputOutcome> {
    let (mouse, position) = parse_local_pixel_event(data, geometry, rect, size)?;
    Some(forward_local_mouse_event(
        runtime,
        mouse.kind,
        position?,
        mouse.modifiers,
        mouse_scroll_lines,
    ))
}

fn forward_local_focus_event(
    runtime: &TerminalRuntime,
    event: crate::ghostty::FocusEvent,
) -> LocalInputOutcome {
    if !runtime.focus_reporting_enabled() {
        return LocalInputOutcome::Handled;
    }
    match crate::ghostty::encode_focus(event) {
        Ok(bytes) => forward_local_bytes(runtime, Some(bytes), false),
        Err(_) => LocalInputOutcome::Failed,
    }
}

fn forward_local_key(
    runtime: &TerminalRuntime,
    key: crate::input::TerminalKey,
    viewport_rows: u16,
) -> LocalInputOutcome {
    if matches!(
        key.code,
        crossterm::event::KeyCode::PageUp | crossterm::event::KeyCode::PageDown
    ) && key.modifiers.is_empty()
        && runtime.plain_page_keys_use_host_scrollback() == Some(true)
    {
        if key.kind == KeyEventKind::Release {
            return LocalInputOutcome::Handled;
        }
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            let lines = usize::from(viewport_rows.max(1));
            if key.code == crossterm::event::KeyCode::PageUp {
                runtime.scroll_up(lines);
            } else {
                runtime.scroll_down(lines);
            }
            return LocalInputOutcome::Redraw;
        }
    }
    forward_local_bytes(runtime, Some(runtime.encode_terminal_key(key)), true)
}

fn forward_local_event(
    runtime: &TerminalRuntime,
    event: crate::raw_input::RawInputEvent,
    mouse_scroll_lines: usize,
    viewport_rows: u16,
) -> LocalInputOutcome {
    match event {
        crate::raw_input::RawInputEvent::Key(key) => forward_local_key(runtime, key, viewport_rows),
        crate::raw_input::RawInputEvent::Text(text) => {
            forward_local_bytes(runtime, Some(text.into_string().into_bytes()), true)
        }
        crate::raw_input::RawInputEvent::Paste(text) => {
            let redraw = reset_local_scroll(runtime);
            if runtime.try_send_paste(text).is_err() {
                LocalInputOutcome::Failed
            } else if redraw {
                LocalInputOutcome::Redraw
            } else {
                LocalInputOutcome::Handled
            }
        }
        crate::raw_input::RawInputEvent::OuterFocusGained => {
            forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Gained)
        }
        crate::raw_input::RawInputEvent::OuterFocusLost => {
            forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Lost)
        }
        crate::raw_input::RawInputEvent::Mouse(mouse) => forward_local_mouse_event(
            runtime,
            mouse.kind,
            crate::input::mouse::Position::Cell {
                column: mouse.column,
                row: mouse.row,
            },
            mouse.modifiers,
            mouse_scroll_lines,
        ),
        crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => LocalInputOutcome::Handled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn test_prefix() -> OmpRendererPrefix {
        OmpRendererPrefix {
            code: ClientKeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL.bits(),
        }
    }

    fn test_rect() -> OmpRendererRect {
        OmpRendererRect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    fn test_server_frame() -> FrameData {
        FrameData {
            cells: vec![
                crate::protocol::CellData {
                    symbol: " ".into(),
                    fg: 0,
                    bg: 0,
                    modifier: 0,
                    skip: false,
                    hyperlink: None,
                };
                80 * 24
            ],
            width: 80,
            height: 24,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    fn test_omp_executable() -> crate::update::OmpExecutable {
        crate::update::OmpExecutable::Explicit("/tmp/pre-resolved-omp".into())
    }

    fn active_renderer(
        runtime: TerminalRuntime,
        prefix: OmpRendererPrefix,
    ) -> (ClientOmpRenderer, mpsc::Sender<AppEvent>, PaneId) {
        let pane_id = PaneId::alloc();
        let (events_tx, events) = mpsc::channel(8);
        let mut renderer = ClientOmpRenderer::new(
            Some(test_omp_executable()),
            crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES,
            crate::config::DEFAULT_MOUSE_SCROLL_LINES,
            true,
            true,
            crate::config::ToastClipboardPosition::BottomCenter,
        );
        renderer.latest_launch_id = 1;
        renderer.attempted_launches.insert(1);
        renderer.local_selected = true;
        renderer.target = Some(LocalTarget {
            launch_id: 1,
            target_app_client_id: 2,
            route: OmpRendererRoute {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 1,
            },
            prefix,
            runtime: Some(runtime),
            pane_id,
            events,
            render_dirty: Arc::new(RenderSignal::new()),
            rect: test_rect(),
            size: (80, 24, 10, 20),
            started_at: Instant::now(),
            bound_at: Some(Instant::now()),
            bound: true,
            surface_active: true,
            first_damage: true,
            ready_reported: true,
            promoted: true,
            failed: false,
            fallback_confirmed: false,
        });
        (renderer, events_tx, pane_id)
    }

    #[test]
    fn capability_requires_semantic_app_tty() {
        assert!(
            capabilities(RenderEncoding::SemanticFrame, true, true, true, true).client_local_native
        );
        assert!(
            !capabilities(RenderEncoding::TerminalAnsi, true, true, true, true).client_local_native
        );
        assert!(
            !capabilities(RenderEncoding::SemanticFrame, true, true, true, false)
                .client_local_native
        );
        assert!(
            !capabilities(RenderEncoding::SemanticFrame, false, true, true, true)
                .client_local_native
        );
        assert!(
            !capabilities(RenderEncoding::SemanticFrame, true, false, true, true)
                .client_local_native
        );
    }

    #[test]
    fn target_handling_reuses_pre_resolved_omp_without_resolving() {
        let calls = std::cell::Cell::new(0);
        let expected = std::env::current_exe().expect("current test executable");
        let executable = super::super::resolve_client_omp_executable_with(
            true,
            || {
                calls.set(calls.get() + 1);
                Ok(crate::update::OmpExecutable::Explicit(expected.clone()))
            },
            |_| {},
        )
        .expect("resolved test executable");
        let mut renderer = ClientOmpRenderer::new(
            Some(executable),
            crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES,
            crate::config::DEFAULT_MOUSE_SCROLL_LINES,
            true,
            true,
            crate::config::ToastClipboardPosition::BottomCenter,
        );

        renderer.apply_target(1, 2, None, false, false, test_prefix(), test_rect(), (0, 0));

        assert_eq!(calls.get(), 1);
        assert_eq!(
            renderer
                .omp_executable
                .as_ref()
                .map(|executable| executable.executable()),
            Some(expected.as_path())
        );
    }

    #[test]
    fn target_handling_ignores_native_target_without_pre_resolved_executable() {
        let mut renderer = ClientOmpRenderer::new(
            None,
            crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES,
            crate::config::DEFAULT_MOUSE_SCROLL_LINES,
            true,
            true,
            crate::config::ToastClipboardPosition::BottomCenter,
        );
        renderer.apply_target(
            1,
            2,
            Some(OmpRendererRoute {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 1,
            }),
            false,
            false,
            test_prefix(),
            test_rect(),
            (0, 0),
        );

        assert!(renderer.target.is_none());
        assert_eq!(renderer.latest_launch_id, 0);
    }

    #[test]
    fn surface_selection_requires_bound_active_damage_and_client_ownership() {
        let selected = |bound: bool, active: bool, damage: bool, server_owned: bool| {
            bound && active && damage && !server_owned
        };
        assert!(!selected(false, true, true, false));
        assert!(!selected(true, false, true, false));
        assert!(!selected(true, true, false, false));
        assert!(!selected(true, true, true, true));
        assert!(selected(true, true, true, false));
    }
    #[tokio::test]
    async fn bound_renderer_allows_cold_start_before_first_damage() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let now = Instant::now();
        let target = renderer.target.as_mut().unwrap();
        target.bound_at = Some(now - Duration::from_secs(3));
        target.first_damage = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        assert!(renderer.next_frame(now, (80, 24)).is_none());
        assert!(!renderer.target.as_ref().unwrap().failed);
    }

    #[test]
    fn configured_prefix_matches_the_actual_terminal_key() {
        let prefix = test_prefix();
        assert!(crate::config::terminal_key_matches_combo(
            &crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            prefix.key_combo(),
        ));
        assert!(!crate::config::terminal_key_matches_combo(
            &crate::input::TerminalKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            prefix.key_combo(),
        ));
    }

    #[tokio::test]
    async fn local_surface_resize_updates_runtime() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(30, 100, 8, 16);
        assert_eq!(runtime.current_size(), (30, 100));
        runtime.shutdown();
    }

    #[tokio::test]
    async fn local_pixel_wheel_scrolls_without_server_input() {
        let output = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            output.as_bytes(),
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.mouse_scroll_lines = 7;
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();

        assert!(renderer
            .route_pixel_input(b"\x1b[<64;101;101M".to_vec(), geometry)
            .is_none());

        let metrics = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .and_then(TerminalRuntime::scroll_metrics)
            .unwrap();
        assert_eq!(metrics.offset_from_bottom, 7);
        assert!(renderer.needs_render);
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_wheel_scrolls_without_server_input() {
        let output = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            output.as_bytes(),
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.mouse_scroll_lines = 7;

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::empty(),
                },
            )])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .unwrap()
                .offset_from_bottom,
            7
        );
        assert!(renderer.needs_render);
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_page_key_scrolls_and_ordinary_input_returns_to_live_output() {
        let output = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            output.as_bytes(),
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::PageUp, KeyModifiers::empty()),
            )])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .unwrap()
                .offset_from_bottom,
            24
        );
        assert!(input.try_recv().is_err());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .unwrap()
                .offset_from_bottom,
            0
        );
    }

    #[tokio::test]
    async fn local_focus_events_reach_server_and_local_pty() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        let messages =
            renderer.route_input(vec![crate::raw_input::RawInputEvent::OuterFocusGained]);

        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusGained])
        ));
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[I");
    }

    #[tokio::test]
    async fn local_alternate_screen_wheel_sends_arrow_input() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1049h\x1b[?1007h",
            8,
        );
        let expected = runtime
            .encode_alternate_scroll(MouseEventKind::ScrollUp)
            .unwrap();
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::empty(),
                },
            )])
            .is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn local_drag_selection_preserves_scroll_and_copies_local_runtime() {
        let output = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            output.as_bytes(),
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .scroll_up(7);

        let mouse = |kind, column, row| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::empty(),
            })
        };
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 1, 1)])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .unwrap()
                .offset_from_bottom,
            7
        );
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 4, 2)])
            .is_empty());
        assert!(renderer
            .selection
            .as_ref()
            .is_some_and(Selection::is_visible));
        renderer.cached_server_frame = Some(test_server_frame());
        let composed = renderer.composed_frame().expect("composed local frame");
        assert_ne!(composed.cells[80 + 1].modifier, 0);
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left), 4, 2)])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .unwrap()
                .offset_from_bottom,
            7
        );
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [LocalEffect::ClipboardWrite(content)] if !content.is_empty()
        ));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_retained_selection_copy_shortcut_uses_local_runtime() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            b"alpha beta\r\n",
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.copy_on_select = false;
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };
        renderer.route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 0)]);
        renderer.route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 4)]);
        renderer.route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left), 4)]);
        assert!(renderer
            .selection
            .as_ref()
            .is_some_and(Selection::is_finalized));
        assert!(renderer.take_effects().is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(
                    KeyCode::Char('c'),
                    if cfg!(target_os = "macos") {
                        KeyModifiers::SUPER
                    } else {
                        KeyModifiers::CONTROL
                    },
                ),
            )])
            .is_empty());
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [LocalEffect::ClipboardWrite(content)] if !content.is_empty()
        ));
        assert!(renderer.selection.is_none());
        let copy_modifiers = if cfg!(target_os = "macos") {
            KeyModifiers::SUPER
        } else {
            KeyModifiers::CONTROL
        };
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('c'), copy_modifiers)
                    .with_kind(KeyEventKind::Repeat),
            )])
            .is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('c'), KeyModifiers::empty())
                    .with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_pixel_drag_outside_pane_finishes_selection() {
        let output = (0..80)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            output.as_bytes(),
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let geometry = crate::input::mouse::HostGeometry::new(100, 24, 1000, 480).unwrap();

        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;21M".to_vec(), geometry)
            .is_none());
        assert!(renderer
            .route_pixel_input(b"\x1b[<32;901;21M".to_vec(), geometry)
            .is_none());
        assert!(renderer
            .route_pixel_input(b"\x1b[<0;901;21m".to_vec(), geometry)
            .is_none());
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [LocalEffect::ClipboardWrite(content)] if !content.is_empty()
        ));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_double_click_selects_and_copies_word() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            b"alpha beta\r\n",
            8,
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };
        renderer.route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))]);
        renderer.route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))]);
        renderer.route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))]);
        renderer.route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))]);
        assert!(renderer
            .selection
            .as_ref()
            .is_some_and(Selection::is_finalized));
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [LocalEffect::ClipboardWrite(content)] if content == b"alpha"
        ));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn inactive_native_surface_clears_selection_and_side_effects() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            64 * 1024,
            b"alpha beta\r\n",
            8,
        );
        let prefix = test_prefix();
        let (mut renderer, events, _pane_id) = active_renderer(runtime, prefix.clone());
        renderer.copy_on_select = false;
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };
        renderer.route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 0)]);
        renderer.route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Left), 4)]);
        renderer.route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left), 4)]);
        assert!(renderer.selection.is_some());
        let route = renderer.target.as_ref().unwrap().route.clone();
        renderer.apply_target(1, 2, Some(route), true, false, prefix, test_rect(), (0, 0));
        assert!(renderer.selection.is_none());
        events
            .try_send(AppEvent::ClipboardWrite {
                content: b"inactive".to_vec(),
            })
            .unwrap();
        renderer.next_frame(Instant::now(), (80, 24));
        assert!(renderer.take_effects().is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn prefix_command_batch_waits_for_authoritative_local_restore() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            )),
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('n'),
                KeyModifiers::empty(),
            )),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [
                ClientMessage::InputEvents { events: prefix },
                ClientMessage::InputEvents { events: command }
            ] if prefix.len() == 1 && command.len() == 1
        ));
        assert!(renderer.server_owned_input);
        assert!(renderer.local_selected);
    }

    #[tokio::test]
    async fn pixel_mouse_maps_host_pixels_to_the_local_pty() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(40, 12);
        runtime.resize(12, 40, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let target = renderer.target.as_mut().unwrap();
        target.rect = OmpRendererRect {
            x: 20,
            y: 6,
            width: 40,
            height: 12,
        };
        target.size = (40, 12, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let outside = b"\x1b[<35;101;21M".to_vec();
        assert!(matches!(
            renderer.route_pixel_input(outside, geometry),
            Some(ClientMessage::InputPixels { .. })
        ));
        let inside = b"\x1b[<35;321;241M".to_vec();
        assert!(renderer.route_pixel_input(inside, geometry).is_none());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<35;121;121M");
    }

    #[tokio::test]
    async fn failed_local_write_waits_for_server_fallback_before_replay() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        let route = OmpRendererRoute {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(1, 2, Some(route), false, false, prefix, test_rect(), (0, 0));
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));
    }

    #[tokio::test]
    async fn native_child_death_releases_surface_after_server_fallback_confirmation() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, events, pane_id) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();

        events.try_send(AppEvent::PaneDied { pane_id }).unwrap();
        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_none());
        assert!(renderer.target.as_ref().unwrap().failed);
        assert!(!renderer.local_selected);
        assert!(renderer.awaiting_fallback);

        renderer.apply_target(1, 2, Some(route), false, false, prefix, test_rect(), (0, 0));

        assert!(!renderer.awaiting_fallback);
        assert!(renderer.target.as_ref().unwrap().fallback_confirmed);
    }

    #[tokio::test]
    async fn bound_false_confirmation_does_not_rearm_fallback_for_failed_ready_target() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(1, 2, Some(route), false, false, prefix, test_rect(), (0, 0));
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));

        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_none());
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
                )])
                .as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));
    }

    #[tokio::test]
    async fn disabled_focus_reporting_still_reaches_server() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusGained])
                .as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusGained])
        ));
        assert!(!renderer.awaiting_fallback);
        assert!(!renderer.target.as_ref().unwrap().failed);
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn failed_focus_write_replays_after_bound_false_confirmation() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusGained])
            .is_empty());
        assert!(renderer.target.as_ref().unwrap().failed);
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(1, 2, Some(route), false, false, prefix, test_rect(), (0, 0));
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusGained])
        ));
    }

    #[tokio::test]
    async fn promotion_confirmation_replays_buffered_key_and_pixel_locally() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(24, 80, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let target = renderer.target.as_mut().unwrap();
        let route = target.route.clone();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;
        renderer.cached_server_frame = Some(test_server_frame());

        renderer.next_frame(Instant::now(), (80, 24));
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererReady { launch_id: 1 }]
        ));
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer.route_pixel_input(data, geometry).is_none());

        renderer.apply_target(1, 2, Some(route), true, true, prefix, test_rect(), (10, 20));

        assert!(!renderer.awaiting_promotion);
        assert!(renderer.target.as_ref().unwrap().promoted);
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<35;321;241M");
    }

    #[tokio::test]
    async fn inactive_promotion_routes_buffered_input_and_forwards_effects() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, events, pane_id) = active_renderer(runtime, prefix.clone());
        let target = renderer.target.as_mut().unwrap();
        let route = target.route.clone();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        target.rect = OmpRendererRect {
            x: 20,
            y: 6,
            width: 40,
            height: 12,
        };
        target.size = (40, 12, 10, 20);
        renderer.local_selected = false;
        renderer.cached_server_frame = Some(test_server_frame());

        renderer.next_frame(Instant::now(), (80, 24));
        renderer.take_outbound_messages();
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let outside = b"\x1b[<35;11;21M".to_vec();
        assert!(matches!(
            renderer.route_pixel_input(outside.clone(), geometry),
            Some(ClientMessage::InputPixels { data, .. }) if data == outside
        ));
        assert!(renderer.route_pixel_input(data, geometry).is_none());

        renderer.apply_target(
            1,
            2,
            Some(route),
            true,
            false,
            prefix,
            test_rect(),
            (10, 20),
        );

        assert!(renderer.target.as_ref().unwrap().promoted);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [
                ClientMessage::InputEvents { events: input },
                ClientMessage::InputPixels { .. }
            ] if input.len() == 1
        ));
        events
            .try_send(AppEvent::TerminalBell { pane_id, count: 1 })
            .unwrap();
        renderer.next_frame(Instant::now(), (80, 24));
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [LocalEffect::Bell(1)]
        ));
    }

    #[test]
    fn native_surface_composes_inside_server_frame_and_preserves_outer_state() {
        let cell = |symbol: &str, hyperlink| crate::protocol::CellData {
            symbol: symbol.into(),
            fg: 1,
            bg: 2,
            modifier: 0,
            skip: false,
            hyperlink,
        };
        let base = FrameData {
            cells: vec![cell("S", None); 12],
            width: 4,
            height: 3,
            cursor: Some(crate::protocol::CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: vec!["server".into()],
            graphics: vec![7, 8],
        };
        let local = FrameData {
            cells: vec![cell("L", Some(0)); 4],
            width: 2,
            height: 2,
            cursor: Some(crate::protocol::CursorState {
                x: 1,
                y: 0,
                visible: true,
                shape: 2,
            }),
            hyperlinks: vec!["local".into()],
            graphics: Vec::new(),
        };

        let composed = compose_frame(
            &base,
            local,
            OmpRendererRect {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
        )
        .unwrap();

        assert_eq!(composed.cells[0].symbol, "S");
        assert_eq!(composed.cells[5].symbol, "L");
        assert_eq!(composed.cells[5].hyperlink, Some(1));
        assert_eq!(composed.hyperlinks, ["server", "local"]);
        assert_eq!(composed.graphics, [7, 8]);
        assert_eq!(
            composed.cursor,
            Some(crate::protocol::CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 2,
            })
        );
    }

    #[test]
    fn local_copy_feedback_overlay_matches_toast_shape_and_expires() {
        let mut frame = test_server_frame();
        overlay_copy_feedback(
            &mut frame,
            crate::config::ToastClipboardPosition::BottomCenter,
        );
        let width = 23u16;
        let x = (frame.width - width) / 2;
        let y = frame.height - 3;
        let at = |column: u16, row: u16| {
            &frame.cells[usize::from(row) * usize::from(frame.width) + usize::from(column)]
        };
        assert_eq!(at(x, y).symbol, "┌");
        assert_eq!(at(x + 1, y + 1).symbol, "●");
        assert_eq!(at(x + 3, y + 1).symbol, "c");
        assert_eq!(at(x + width - 1, y + 2).symbol, "┘");
        assert_eq!(at(x + 3, y + 1).modifier, Modifier::BOLD.bits());

        let now = Instant::now();
        let mut renderer = ClientOmpRenderer::new(
            None,
            crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES,
            crate::config::DEFAULT_MOUSE_SCROLL_LINES,
            true,
            true,
            crate::config::ToastClipboardPosition::BottomCenter,
        );
        assert!(renderer.show_copy_feedback(now));
        assert!(renderer.copy_feedback_active(now + Duration::from_secs(1)));
        renderer.next_frame(now + LOCAL_COPY_FEEDBACK_DURATION, (80, 24));
        assert!(!renderer.copy_feedback_active(now + LOCAL_COPY_FEEDBACK_DURATION));
    }

    #[tokio::test]
    async fn failed_pixel_write_waits_for_server_fallback_before_replay() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(24, 80, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        drop(input);
        let route = OmpRendererRoute {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer.route_pixel_input(data.clone(), geometry).is_none());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(
            1,
            2,
            Some(route),
            false,
            false,
            prefix,
            test_rect(),
            (10, 20),
        );
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputPixels {
                data: replayed,
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }] if replayed == &data
        ));
    }

    #[tokio::test]
    async fn replacement_launch_discards_deferred_input() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(2, 2, None, false, false, test_prefix(), test_rect(), (0, 0));
        assert!(!renderer.awaiting_fallback);
        assert!(renderer.deferred_messages.is_empty());
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn readiness_is_reported_once_before_surface_activation() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;
        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_none());
        assert!(renderer.take_outbound_messages().is_empty());
        assert!(!renderer.awaiting_promotion);
        renderer.cached_server_frame = Some(test_server_frame());

        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_none());
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererReady { launch_id: 1 }]
        ));
        assert!(renderer.awaiting_promotion);
        assert!(renderer.owns_input());
        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_none());
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn warming_renderer_suppresses_terminal_effects() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, events, pane_id) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().promoted = false;
        events
            .try_send(AppEvent::TerminalBell { pane_id, count: 1 })
            .unwrap();
        events
            .try_send(AppEvent::ClipboardWrite {
                content: b"private-authoritative".to_vec(),
            })
            .unwrap();
        events
            .try_send(AppEvent::OpenUrl {
                url: "https://example.com/warming".into(),
                source_id: 0,
            })
            .unwrap();
        renderer.next_frame(Instant::now(), (80, 24));
        assert!(renderer.take_effects().is_empty());
    }

    #[tokio::test]
    async fn local_terminal_effects_are_forwarded() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, events, pane_id) = active_renderer(runtime, test_prefix());
        events
            .try_send(AppEvent::TerminalBell { pane_id, count: 2 })
            .unwrap();
        events
            .try_send(AppEvent::ClipboardWrite {
                content: b"copied".to_vec(),
            })
            .unwrap();
        events
            .try_send(AppEvent::OpenUrl {
                url: "https://example.com".into(),
                source_id: 0,
            })
            .unwrap();
        renderer.next_frame(Instant::now(), (80, 24));
        assert!(matches!(
            renderer.take_effects().as_slice(),
            [
                LocalEffect::Bell(2),
                LocalEffect::ClipboardWrite(content),
                LocalEffect::OpenUrl(url)
            ] if content == b"copied" && url == "https://example.com"
        ));
    }
}
