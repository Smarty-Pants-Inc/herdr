use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossterm::event::{KeyEventKind, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::protocol::{
    ClientInputEvent, ClientKeyCode, ClientKeyKind, ClientKeySource, ClientMessage,
    ClientMouseKind, FrameData, OmpRendererCapabilities, OmpRendererPrefix, OmpRendererRoute,
    RenderEncoding,
};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalRuntime;

pub(super) const OMP_RENDERER_LAUNCH_ID_ENV: &str = "HERDR_OMP_RENDERER_LAUNCH_ID";

const BIND_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_DAMAGE_TIMEOUT: Duration = Duration::from_secs(10);

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
        size: (u16, u16, u32, u32),
    ) -> std::io::Result<Self> {
        let (cols, rows, cell_width_px, cell_height_px) = size;
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
            0,
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

    fn resize(&mut self, size: (u16, u16, u32, u32)) {
        self.size = size;
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let (cols, rows, cell_width_px, cell_height_px) = size;
        runtime.resize(rows.max(1), cols.max(1), cell_width_px, cell_height_px);
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
                AppEvent::ClipboardWrite { content } if self.promoted => {
                    effects.push(LocalEffect::ClipboardWrite(content));
                }
                AppEvent::OpenUrl { url, .. } if self.promoted => {
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

    fn frame(&self, size: (u16, u16)) -> Option<FrameData> {
        let runtime = self.runtime.as_ref()?;
        let area = Rect::new(0, 0, size.0.max(1), size.1.max(1));
        let (buffer, cursor) = crate::server::render_stream::render_terminal_virtual(runtime, area);
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
}

impl ClientOmpRenderer {
    pub(super) fn new(omp_executable: Option<crate::update::OmpExecutable>) -> Self {
        Self {
            omp_executable,
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
        size: (u16, u16, u32, u32),
    ) {
        if self.omp_executable.is_none() || launch_id < self.latest_launch_id {
            return;
        }
        if route.is_none() {
            self.latest_launch_id = launch_id;
            self.stop_target();
            self.discard_deferred_messages();
            self.cached_server_frame = None;
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
            self.cached_server_frame = None;
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
                    size,
                )
                .ok()
            });
            self.target = target;
            self.needs_render = true;
        }
        if !bound {
            self.cached_server_frame = None;
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
            self.cached_server_frame = None;
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
        target.resize(size);
        self.needs_render = true;
        if let Some(local_active) = confirm_promotion {
            if local_active {
                self.local_selected = true;
                self.cached_server_frame = None;
                self.needs_render = true;
                self.force_repaint = true;
            }
            self.resolve_promotion(local_active);
        }
    }

    pub(super) fn cache_server_frame(&mut self, frame: FrameData) -> Option<SurfaceFrame> {
        if self.local_selected {
            return None;
        }
        self.cached_server_frame = Some(frame.clone());
        Some(SurfaceFrame {
            frame,
            force_repaint: false,
        })
    }

    pub(super) fn resize(&mut self, size: (u16, u16, u32, u32)) {
        if let Some(target) = self.target.as_mut() {
            target.resize(size);
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
            let protocol_event = client_event_from_raw(&event);
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
                self.cached_server_frame = None;
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
            let sent = self
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .is_some_and(|runtime| forward_local_event(runtime, event));
            if !sent {
                if let Some(target) = self.target.as_mut() {
                    target.fail();
                }
                self.awaiting_fallback = true;
                self.cached_server_frame = None;
                self.needs_render = true;
                self.force_repaint = true;
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
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
        if self.awaiting_fallback || self.awaiting_promotion {
            self.deferred_messages.push(message);
            return None;
        }
        if self.server_owned_input || !self.local_selected {
            return Some(message);
        }
        let sent = self.target.as_ref().and_then(|target| {
            target.runtime.as_ref().and_then(|runtime| {
                forward_local_pixel_event(runtime, &data, geometry, target.size)
            })
        });
        match sent {
            Some(true) => None,
            None => Some(message),
            Some(false) => {
                if let Some(target) = self.target.as_mut() {
                    target.fail();
                }
                self.awaiting_fallback = true;
                self.cached_server_frame = None;
                self.deferred_messages.push(message);
                self.needs_render = true;
                self.force_repaint = true;
                None
            }
        }
    }

    pub(super) fn next_frame(&mut self, now: Instant, size: (u16, u16)) -> Option<SurfaceFrame> {
        let damaged = self
            .target
            .as_mut()
            .is_some_and(|target| target.poll(now, &mut self.effects));
        if let Some(target) = self.target.as_mut() {
            if target.bound
                && target.first_damage
                && target.runtime.is_some()
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
                self.cached_server_frame = None;
            }
        }
        let should_select = self.target.as_ref().is_some_and(|target| {
            !target.failed
                && target.runtime.is_some()
                && target.bound
                && target.surface_active
                && target.first_damage
                && !self.server_owned_input
        });
        if should_select != self.local_selected {
            self.local_selected = should_select;
            if should_select {
                self.cached_server_frame = None;
            }
            self.needs_render = true;
            self.force_repaint = true;
        }
        if self.local_selected && (damaged || self.needs_render) {
            let frame = self.target.as_ref()?.frame(size)?;
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

    pub(super) fn take_outbound_messages(&mut self) -> Vec<ClientMessage> {
        std::mem::take(&mut self.outbound_messages)
    }

    pub(super) fn take_effects(&mut self) -> Vec<LocalEffect> {
        std::mem::take(&mut self.effects)
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

fn encode_local_mouse(
    runtime: &TerminalRuntime,
    kind: MouseEventKind,
    position: crate::input::mouse::Position,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<Vec<u8>> {
    match kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            runtime.encode_mouse_wheel(kind, position, modifiers)
        }
        MouseEventKind::Moved => runtime.encode_mouse_motion(kind, position, modifiers),
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
            runtime.encode_mouse_button(kind, position, modifiers)
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => None,
    }
}

fn forward_local_pixel_event(
    runtime: &TerminalRuntime,
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    size: (u16, u16, u32, u32),
) -> Option<bool> {
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
        Rect::new(0, 0, geometry.cols, geometry.rows),
        child_width_px,
        child_height_px,
    )?;
    let bytes = encode_local_mouse(runtime, mouse.kind, position, mouse.modifiers);
    Some(
        bytes.is_none_or(|bytes| {
            bytes.is_empty() || runtime.try_send_bytes(Bytes::from(bytes)).is_ok()
        }),
    )
}

fn forward_local_focus_event(runtime: &TerminalRuntime, event: crate::ghostty::FocusEvent) -> bool {
    !runtime.focus_reporting_enabled()
        || crate::ghostty::encode_focus(event)
            .is_ok_and(|bytes| runtime.try_send_bytes(Bytes::from(bytes)).is_ok())
}

fn forward_local_event(runtime: &TerminalRuntime, event: crate::raw_input::RawInputEvent) -> bool {
    let bytes = match event {
        crate::raw_input::RawInputEvent::Key(key) => Some(runtime.encode_terminal_key(key)),
        crate::raw_input::RawInputEvent::Text(text) => Some(text.into_string().into_bytes()),
        crate::raw_input::RawInputEvent::Paste(text) => {
            return runtime.try_send_paste(text).is_ok()
        }
        crate::raw_input::RawInputEvent::OuterFocusGained => {
            return forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Gained)
        }
        crate::raw_input::RawInputEvent::OuterFocusLost => {
            return forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Lost)
        }
        crate::raw_input::RawInputEvent::Mouse(mouse) => encode_local_mouse(
            runtime,
            mouse.kind,
            crate::input::mouse::Position::Cell {
                column: mouse.column,
                row: mouse.row,
            },
            mouse.modifiers,
        ),
        crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => return true,
    };
    bytes.is_none_or(|bytes| bytes.is_empty() || runtime.try_send_bytes(Bytes::from(bytes)).is_ok())
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

    fn test_omp_executable() -> crate::update::OmpExecutable {
        crate::update::OmpExecutable::Explicit("/tmp/pre-resolved-omp".into())
    }

    fn active_renderer(
        runtime: TerminalRuntime,
        prefix: OmpRendererPrefix,
    ) -> (ClientOmpRenderer, mpsc::Sender<AppEvent>, PaneId) {
        let pane_id = PaneId::alloc();
        let (events_tx, events) = mpsc::channel(8);
        let mut renderer = ClientOmpRenderer::new(Some(test_omp_executable()));
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
        let mut renderer = ClientOmpRenderer::new(Some(executable));

        renderer.apply_target(1, 2, None, false, false, test_prefix(), (80, 24, 0, 0));

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
        let mut renderer = ClientOmpRenderer::new(None);
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
            (80, 24, 0, 0),
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
        renderer.target.as_mut().unwrap().size = (40, 12, 10, 20);
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer.route_pixel_input(data, geometry).is_none());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<35;161;121M");
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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0));
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

        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0));

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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0));
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
    async fn disabled_focus_reporting_is_a_successful_local_noop() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusGained])
            .is_empty());
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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0));
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

        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20));

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
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        renderer.take_outbound_messages();
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer.route_pixel_input(data, geometry).is_none());

        renderer.apply_target(1, 2, Some(route), true, false, prefix, (80, 24, 10, 20));

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

    #[tokio::test]
    async fn native_authority_discards_server_frames_and_stale_fallback_cache() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();
        let frame = FrameData {
            cells: Vec::new(),
            width: 0,
            height: 0,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: vec![1],
        };

        assert!(renderer.cache_server_frame(frame.clone()).is_none());
        assert!(renderer.cached_server_frame.is_none());
        renderer.cached_server_frame = Some(frame);
        renderer.apply_target(1, 2, Some(route), true, false, prefix, (80, 24, 10, 20));
        assert!(renderer.cached_server_frame.is_none());
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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 10, 20));
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
        renderer.apply_target(2, 2, None, false, false, test_prefix(), (80, 24, 0, 0));
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
