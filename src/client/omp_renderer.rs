use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crossterm::event::{KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::protocol::{
    ClientInputEvent, ClientKeyCode, ClientKeyKind, ClientKeySource, ClientMessage,
    ClientMouseKind, FrameData, OmpRendererCapabilities, OmpRendererPrefix, OmpRendererRoute,
    RenderEncoding, MAX_LINK_URL_LENGTH,
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

enum DeferredMessage {
    InputEvents {
        events: Vec<ClientInputEvent>,
        generation: u64,
    },
    InputPixels {
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        generation: u64,
    },
}

impl DeferredMessage {
    fn into_client_message(self, current_generation: u64) -> Option<ClientMessage> {
        match self {
            Self::InputEvents { events, generation }
                if generation == current_generation || !input_events_include_mouse(&events) =>
            {
                Some(ClientMessage::InputEvents { events })
            }
            Self::InputEvents { .. } => None,
            Self::InputPixels {
                data,
                geometry,
                generation,
            } if generation == current_generation => Some(pixel_input_message(data, geometry)),
            Self::InputPixels { .. } => None,
        }
    }
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
    handoff_frame: Option<FrameData>,
    local_selected: bool,
    server_owned_input: bool,
    pending_link_click: bool,
    pointer_cell: Option<(u16, u16)>,
    pointer_pixels: Option<crate::input::mouse::HostPixels>,
    hovered_link_cells: Option<Vec<(u16, u16)>>,
    suppress_link_affordance: bool,
    awaiting_fallback: bool,
    awaiting_promotion: bool,
    deferred_messages: Vec<DeferredMessage>,
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
            handoff_frame: None,
            local_selected: false,
            server_owned_input: false,
            pending_link_click: false,
            pointer_cell: None,
            pointer_pixels: None,
            hovered_link_cells: None,
            awaiting_fallback: false,
            suppress_link_affordance: false,
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
        current_input_generation: u64,
    ) {
        if self.omp_executable.is_none() || launch_id < self.latest_launch_id {
            return;
        }
        if route.is_none() {
            self.latest_launch_id = launch_id;
            self.prepare_surface_handoff();
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
            self.prepare_surface_handoff();
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
            self.release_deferred_messages(current_input_generation);
        }
        if self.local_selected
            && self.target.as_ref().is_some_and(|target| {
                (target.surface_active && !surface_active) || (target.bound && !bound)
            })
        {
            self.prepare_surface_handoff();
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
                self.handoff_frame = None;
                self.suppress_link_affordance = false;
                self.needs_render = true;
                self.force_repaint = true;
            }
            self.resolve_promotion(local_active, current_input_generation);
        }
        self.remap_pointer_pixels();
        self.refresh_hovered_link();
    }

    pub(super) fn cache_server_frame(&mut self, frame: FrameData) -> Option<SurfaceFrame> {
        if self.local_selected {
            return None;
        }
        self.handoff_frame = None;
        self.suppress_link_affordance = false;
        self.cached_server_frame = Some(frame.clone());
        Some(SurfaceFrame {
            frame,
            force_repaint: false,
        })
    }

    pub(super) fn resize(
        &mut self,
        size: (u16, u16, u32, u32),
        host_geometry: Option<crate::input::mouse::HostGeometry>,
        current_input_generation: u64,
    ) {
        self.deferred_messages.retain(|message| match message {
            DeferredMessage::InputEvents { events, generation } => {
                *generation == current_input_generation || !input_events_include_mouse(events)
            }
            DeferredMessage::InputPixels { generation, .. } => {
                *generation == current_input_generation
            }
        });
        self.pointer_pixels = self.pointer_pixels.and_then(|mut pointer| {
            pointer.geometry = host_geometry?;
            Some(pointer)
        });
        if let Some(target) = self.target.as_mut() {
            target.resize(size);
        }
        if self.pointer_pixels.is_some() {
            self.remap_pointer_pixels();
        } else {
            self.set_pointer_cell(None);
        }
        self.refresh_hovered_link();
        self.force_repaint = true;
        self.needs_render = true;
    }

    pub(super) fn owns_input(&self) -> bool {
        self.local_selected
            || self.server_owned_input
            || self.awaiting_fallback
            || self.awaiting_promotion
    }

    fn prepare_surface_handoff(&mut self) {
        if !self.local_selected {
            return;
        }
        if let Some(frame) = self
            .target
            .as_ref()
            .and_then(|target| target.frame((target.size.0, target.size.1)))
        {
            self.store_handoff_frame(&frame);
        }
        self.suppress_link_affordance = true;
        self.needs_render = true;
        self.force_repaint = true;
    }

    fn store_handoff_frame(&mut self, frame: &FrameData) {
        let mut cleanup = frame.clone();
        for cell in &mut cleanup.cells {
            cell.hyperlink = None;
        }
        cleanup.hyperlinks.clear();
        self.handoff_frame = Some(cleanup);
    }

    fn resolved_link_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::app::actions::ResolvedTerminalLink> {
        if !self.local_selected || self.server_owned_input {
            return None;
        }
        let target = self.target.as_ref()?;
        let runtime = target.runtime.as_ref()?;
        let (cols, rows, _, _) = target.size;
        let link =
            crate::app::actions::resolved_terminal_link_at_cell(runtime, row, column, cols, rows)?;
        (link.url.len() <= MAX_LINK_URL_LENGTH).then_some(link)
    }

    pub(super) fn observe_pointer_cell(&mut self, cell: Option<(u16, u16)>) {
        self.pointer_pixels = None;
        self.set_pointer_cell(cell);
    }

    fn set_pointer_cell(&mut self, cell: Option<(u16, u16)>) {
        if self.pointer_cell == cell {
            return;
        }
        self.pointer_cell = cell;
        self.refresh_hovered_link();
    }

    fn remap_pointer_pixels(&mut self) {
        let Some(pointer) = self.pointer_pixels else {
            return;
        };
        self.set_pointer_cell(pointer.geometry.cell(pointer.x, pointer.y));
    }

    fn refresh_hovered_link(&mut self) {
        let cells = self
            .pointer_cell
            .and_then(|(column, row)| self.resolved_link_at(column, row).map(|link| link.cells));
        if self.hovered_link_cells != cells {
            self.hovered_link_cells = cells;
            self.needs_render = true;
        }
    }

    pub(super) fn native_link_active(&self) -> Option<bool> {
        if self.suppress_link_affordance {
            return Some(false);
        }
        if !self.local_selected || self.server_owned_input {
            return None;
        }
        Some(
            self.pointer_cell
                .is_some_and(|(column, row)| self.resolved_link_at(column, row).is_some()),
        )
    }

    fn link_activation_message(&self, column: u16, row: u16) -> Option<ClientMessage> {
        let link = self.resolved_link_at(column, row)?;
        Some(ClientMessage::ActivateOmpLink {
            launch_id: self.target.as_ref()?.launch_id,
            url: link.url,
        })
    }

    #[cfg(test)]
    pub(super) fn route_input(
        &mut self,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> Vec<ClientMessage> {
        self.route_input_at_generation(events, 0)
    }

    pub(super) fn route_input_at_generation(
        &mut self,
        events: Vec<crate::raw_input::RawInputEvent>,
        input_generation: u64,
    ) -> Vec<ClientMessage> {
        self.route_input_inner(events, input_generation, true)
    }

    fn route_input_inner(
        &mut self,
        events: Vec<crate::raw_input::RawInputEvent>,
        input_generation: u64,
        observe_pointer: bool,
    ) -> Vec<ClientMessage> {
        let mut messages = Vec::new();
        let mut server_batch = Vec::new();
        let mut deferred_events = Vec::new();
        for event in events {
            let protocol_event = client_event_from_raw(&event);
            if observe_pointer {
                match &event {
                    crate::raw_input::RawInputEvent::Mouse(mouse) => {
                        self.observe_pointer_cell(Some((mouse.column, mouse.row)));
                    }
                    crate::raw_input::RawInputEvent::OuterFocusLost => {
                        self.observe_pointer_cell(None);
                    }
                    _ => {}
                }
            }
            if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pending_link_click = false;
                    }
                    MouseEventKind::Drag(MouseButton::Left) if self.pending_link_click => {
                        continue;
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.pending_link_click => {
                        self.pending_link_click = false;
                        continue;
                    }
                    _ => {}
                }
            }
            if self.awaiting_fallback || self.awaiting_promotion {
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
                }
                continue;
            }
            if self.local_selected && !self.server_owned_input {
                if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        self.pending_link_click = false;
                        if let Some(message) = self.link_activation_message(mouse.column, mouse.row)
                        {
                            if !server_batch.is_empty() {
                                messages.push(ClientMessage::InputEvents {
                                    events: std::mem::take(&mut server_batch),
                                });
                            }
                            messages.push(message);
                            self.pending_link_click = true;
                            continue;
                        }
                    }
                }
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
                self.prepare_surface_handoff();
                self.server_owned_input = true;
                self.refresh_hovered_link();
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
                self.prepare_surface_handoff();
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
            self.deferred_messages.push(DeferredMessage::InputEvents {
                events: deferred_events,
                generation: input_generation,
            });
        }
        messages
    }

    pub(super) fn route_pixel_input(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
    ) -> Option<ClientMessage> {
        self.route_pixel_input_inner(data, geometry, input_generation, true)
    }

    fn route_pixel_input_inner(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
        observe_pointer: bool,
    ) -> Option<ClientMessage> {
        let decoded_host_mouse = decode_pixel_mouse_cell(&data, geometry);
        if observe_pointer {
            if let Some((x, y)) = crate::input::mouse::parse_report(&data) {
                self.pointer_pixels = Some(crate::input::mouse::HostPixels { x, y, geometry });
                self.remap_pointer_pixels();
            }
        }
        let local_mouse = self
            .target
            .as_ref()
            .and_then(|target| decode_local_pixel_mouse(&data, geometry, target.size));
        let server_surface = !self.local_selected || self.server_owned_input;
        let host_mouse = ((server_surface && !self.awaiting_fallback && !self.awaiting_promotion)
            || (self.pending_link_click && local_mouse.is_none()))
        .then_some(decoded_host_mouse)
        .flatten();
        let mouse_kind = local_mouse
            .map(|mouse| mouse.mouse.kind)
            .or_else(|| host_mouse.map(|(kind, _, _)| kind));
        match mouse_kind {
            Some(MouseEventKind::Down(MouseButton::Left)) => {
                self.pending_link_click = false;
            }
            Some(MouseEventKind::Drag(MouseButton::Left)) if self.pending_link_click => {
                return None;
            }
            Some(MouseEventKind::Up(MouseButton::Left)) if self.pending_link_click => {
                self.pending_link_click = false;
                return None;
            }
            _ => {}
        }
        if self.awaiting_fallback || self.awaiting_promotion {
            self.deferred_messages.push(DeferredMessage::InputPixels {
                data,
                geometry,
                generation: input_generation,
            });
            return None;
        }
        if self.server_owned_input || !self.local_selected {
            return Some(pixel_input_message(data, geometry));
        }
        if let Some((MouseEventKind::Down(MouseButton::Left), column, row)) = decoded_host_mouse {
            if let Some(message) = self.link_activation_message(column, row) {
                self.pending_link_click = true;
                return Some(message);
            }
        }
        let sent = self.target.as_ref().and_then(|target| {
            let runtime = target.runtime.as_ref()?;
            Some(forward_local_pixel_mouse(runtime, local_mouse?))
        });
        match sent {
            Some(true) => None,
            None => Some(pixel_input_message(data, geometry)),
            Some(false) => {
                self.prepare_surface_handoff();
                if let Some(target) = self.target.as_mut() {
                    target.fail();
                }
                self.awaiting_fallback = true;
                self.cached_server_frame = None;
                self.deferred_messages.push(DeferredMessage::InputPixels {
                    data,
                    geometry,
                    generation: input_generation,
                });
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
        let selection_changed = should_select != self.local_selected;
        if selection_changed {
            if !should_select {
                self.prepare_surface_handoff();
            }
            self.local_selected = should_select;
            if should_select {
                self.cached_server_frame = None;
                self.handoff_frame = None;
                self.suppress_link_affordance = false;
            }
            self.needs_render = true;
            self.force_repaint = true;
        }
        if damaged || selection_changed {
            self.refresh_hovered_link();
        }
        if self.local_selected && (damaged || self.needs_render) {
            let mut frame = self.target.as_ref()?.frame(size)?;
            self.store_handoff_frame(&frame);
            if let Some(cells) = self.hovered_link_cells.as_deref() {
                for &(column, row) in cells {
                    if column < frame.width && row < frame.height {
                        let index =
                            usize::from(row) * usize::from(frame.width) + usize::from(column);
                        frame.cells[index].modifier |= ratatui::style::Modifier::UNDERLINED.bits();
                    }
                }
            }
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
            let frame = self
                .cached_server_frame
                .clone()
                .or_else(|| self.handoff_frame.take());
            return frame.map(|frame| SurfaceFrame {
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

    fn resolve_promotion(&mut self, local_active: bool, current_input_generation: u64) {
        self.awaiting_promotion = false;
        if !local_active {
            let deferred = std::mem::take(&mut self.deferred_messages);
            self.outbound_messages.extend(
                deferred
                    .into_iter()
                    .filter_map(|message| message.into_client_message(current_input_generation)),
            );
            return;
        }
        let deferred = std::mem::take(&mut self.deferred_messages);
        for message in deferred {
            if self.awaiting_fallback {
                self.deferred_messages.push(message);
                continue;
            }
            match message {
                DeferredMessage::InputEvents { events, .. } => {
                    let events = events
                        .into_iter()
                        .filter(|event| !matches!(event, ClientInputEvent::Mouse { .. }))
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    let messages = self.route_input_inner(events, current_input_generation, false);
                    self.outbound_messages.extend(messages);
                }
                DeferredMessage::InputPixels { .. } => {}
            }
        }
    }

    fn release_deferred_messages(&mut self, current_input_generation: u64) {
        if self.awaiting_fallback || self.awaiting_promotion {
            self.awaiting_fallback = false;
            self.awaiting_promotion = false;
            let deferred = std::mem::take(&mut self.deferred_messages);
            self.outbound_messages.extend(
                deferred
                    .into_iter()
                    .filter_map(|message| message.into_client_message(current_input_generation)),
            );
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
        self.hovered_link_cells = None;
        self.needs_render = true;
    }
}

fn input_events_include_mouse(events: &[ClientInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, ClientInputEvent::Mouse { .. }))
}

fn pixel_input_message(
    data: Vec<u8>,
    geometry: crate::input::mouse::HostGeometry,
) -> ClientMessage {
    ClientMessage::InputPixels {
        data,
        cols: geometry.cols,
        rows: geometry.rows,
        width_px: geometry.width_px,
        height_px: geometry.height_px,
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

#[derive(Clone, Copy)]
struct LocalPixelMouse {
    mouse: MouseEvent,
    position: crate::input::mouse::Position,
}

fn decode_pixel_mouse_cell(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
) -> Option<(MouseEventKind, u16, u16)> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let (column, row) = geometry.cell(x, y)?;
    let report = crate::input::mouse::report_at_cell(data, column, row)?;
    crate::raw_input::parse_raw_input_bytes_sync(&report)
        .into_iter()
        .find_map(|event| match event {
            crate::raw_input::RawInputEvent::Mouse(mouse) => Some((mouse.kind, column, row)),
            _ => None,
        })
}

fn local_pixel_position(
    pointer: crate::input::mouse::HostPixels,
    size: (u16, u16, u32, u32),
) -> Option<crate::input::mouse::Position> {
    let (cols, rows, cell_width_px, cell_height_px) = size;
    let child_width_px = u32::from(cols).checked_mul(cell_width_px)?;
    let child_height_px = u32::from(rows).checked_mul(cell_height_px)?;
    pointer.pane_position(
        Rect::new(0, 0, pointer.geometry.cols, pointer.geometry.rows),
        child_width_px,
        child_height_px,
    )
}

fn decode_local_pixel_mouse(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    size: (u16, u16, u32, u32),
) -> Option<LocalPixelMouse> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let (host_column, host_row) = geometry.cell(x, y)?;
    let cell_report = crate::input::mouse::report_at_cell(data, host_column, host_row)?;
    let mouse = crate::raw_input::parse_raw_input_bytes_sync(&cell_report)
        .into_iter()
        .find_map(|event| match event {
            crate::raw_input::RawInputEvent::Mouse(mouse) => Some(mouse),
            _ => None,
        })?;
    let position = local_pixel_position(crate::input::mouse::HostPixels { x, y, geometry }, size)?;
    Some(LocalPixelMouse { mouse, position })
}

fn forward_local_pixel_mouse(runtime: &TerminalRuntime, mouse: LocalPixelMouse) -> bool {
    let bytes = encode_local_mouse(
        runtime,
        mouse.mouse.kind,
        mouse.position,
        mouse.mouse.modifiers,
    );
    bytes.is_none_or(|bytes| bytes.is_empty() || runtime.try_send_bytes(Bytes::from(bytes)).is_ok())
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
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent};

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

        renderer.apply_target(1, 2, None, false, false, test_prefix(), (80, 24, 0, 0), 0);

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
            0,
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
        assert!(renderer.route_pixel_input(data, geometry, 0).is_none());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<35;161;121M");
    }

    #[tokio::test]
    async fn native_hover_tracks_pointer_while_server_surface_is_active() {
        let url = "https://example.com/hover";
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(url.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        renderer.observe_pointer_cell(Some((40, 10)));
        renderer.target.as_mut().unwrap().surface_active = false;
        let cleanup = renderer
            .next_frame(Instant::now(), (80, 24))
            .expect("inactive native surface cleanup")
            .frame;
        assert!(cleanup.hyperlinks.is_empty());
        renderer.observe_pointer_cell(Some((8, 0)));
        assert_eq!(renderer.native_link_active(), Some(false));

        renderer.target.as_mut().unwrap().surface_active = true;
        let frame = renderer
            .next_frame(Instant::now(), (80, 24))
            .expect("reactivated native surface")
            .frame;
        assert_eq!(renderer.native_link_active(), Some(true));
        let index = 8;
        assert_ne!(
            frame.cells[index].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
    }

    #[tokio::test]
    async fn native_pixel_pointer_uses_the_same_displayed_cell_as_activation() {
        let url = "https://example.com/pixel";
        let screen = format!(" \x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 801, 20).unwrap();

        let message = renderer
            .route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0)
            .expect("pixel link activation");
        assert!(matches!(
            message,
            ClientMessage::ActivateOmpLink { url: produced, .. } if produced == url
        ));
        assert_eq!(renderer.pointer_cell, Some((1, 0)));
        assert!(renderer.resolved_link_at(0, 0).is_none());
        assert_eq!(renderer.native_link_active(), Some(true));
        let frame = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("pixel hover repaint")
            .frame;
        assert_ne!(
            frame.cells[1].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
    }

    #[tokio::test]
    async fn pixel_pointer_observed_without_target_remaps_when_target_returns() {
        let url = "https://example.com/reinstalled";
        let screen = format!(" \x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        let target = renderer.target.take().unwrap();
        let route = target.route.clone();
        renderer.local_selected = false;
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 801, 20).unwrap();

        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<35;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::InputPixels { .. })
        ));
        assert!(renderer.pointer_pixels.is_some());
        assert_eq!(renderer.pointer_cell, Some((1, 0)));

        renderer.target = Some(target);
        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 1, 10, 20), 0);
        assert_eq!(renderer.pointer_cell, Some((1, 0)));
        renderer.next_frame(Instant::now(), (80, 1));
        assert_eq!(renderer.native_link_active(), Some(true));
    }

    #[tokio::test]
    async fn pixel_coordinates_update_even_when_the_button_code_is_unsupported() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        renderer.observe_pointer_cell(Some((0, 0)));
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        let report = b"\x1b[<128;21;1M".to_vec();

        assert!(decode_pixel_mouse_cell(&report, geometry).is_none());
        assert!(matches!(
            renderer.route_pixel_input(report, geometry, 0),
            Some(ClientMessage::InputPixels { .. })
        ));
        assert_eq!(renderer.pointer_cell, Some((2, 0)));
    }

    #[tokio::test]
    async fn resize_refreshes_retained_pixel_geometry_before_remapping() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(100, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        let old_geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<35;401;1M".to_vec(), old_geometry, 0)
            .is_none());
        assert_eq!(renderer.pointer_cell, Some((40, 0)));

        let new_geometry = crate::input::mouse::HostGeometry::new(100, 1, 1000, 20).unwrap();
        renderer.resize((100, 1, 10, 20), Some(new_geometry), 0);
        assert_eq!(renderer.pointer_cell, Some((40, 0)));

        renderer.observe_pointer_cell(Some((40, 0)));
        let raw_resize_geometry = crate::input::mouse::HostGeometry::new(160, 1, 800, 20).unwrap();
        renderer.resize((160, 1, 5, 20), Some(raw_resize_geometry), 0);
        assert_eq!(renderer.pointer_cell, None);
    }

    #[tokio::test]
    async fn prefix_handoff_immediately_clears_native_link_affordances() {
        let url = "https://example.com/handoff";
        let screen = format!("\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        let hovered = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("hovered native frame")
            .frame;
        assert_ne!(
            hovered.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert!(hovered.cells[0].hyperlink.is_some());

        renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        )]);
        assert!(renderer.server_owned_input);
        assert_eq!(renderer.native_link_active(), Some(false));
        let cleanup = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("native handoff cleanup frame")
            .frame;
        assert_eq!(
            cleanup.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert!(cleanup.cells[0].hyperlink.is_none());
        assert!(cleanup.hyperlinks.is_empty());
    }

    #[tokio::test]
    async fn target_retirement_immediately_clears_native_link_affordances() {
        let url = "https://example.com/retired";
        let screen = format!("\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })]);
        renderer.next_frame(Instant::now(), (80, 1));

        renderer.apply_target(2, 2, None, false, false, prefix, (80, 1, 10, 20), 0);
        assert_eq!(renderer.native_link_active(), Some(false));
        let cleanup = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("target retirement cleanup frame")
            .frame;
        assert_eq!(
            cleanup.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert!(cleanup.cells[0].hyperlink.is_none());
        assert!(cleanup.hyperlinks.is_empty());
    }

    #[tokio::test]
    async fn failed_local_write_preserves_the_pre_failure_cleanup_frame() {
        let url = "https://example.com/failed";
        let screen = format!("\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        drop(input);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        renderer.next_frame(Instant::now(), (80, 1));

        renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
        )]);
        assert!(renderer.awaiting_fallback);
        let cleanup = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("cleanup survives target failure")
            .frame;
        assert_eq!(
            cleanup.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert!(cleanup.cells[0].hyperlink.is_none());
    }

    #[tokio::test]
    async fn pane_death_uses_the_last_clean_local_frame() {
        let url = "https://example.com/died";
        let screen = format!("\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, events, pane_id) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        renderer.next_frame(Instant::now(), (80, 1));
        let ignored_server_frame = renderer.handoff_frame.clone().unwrap();
        assert!(renderer.cache_server_frame(ignored_server_frame).is_none());
        assert!(renderer.handoff_frame.is_some());
        events
            .try_send(AppEvent::PaneDied { pane_id })
            .expect("queue local pane death");

        let cleanup = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("last clean local frame survives pane death")
            .frame;
        assert_eq!(
            cleanup.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
        assert!(cleanup.cells[0].hyperlink.is_none());
        assert_eq!(renderer.native_link_active(), Some(false));
    }

    #[tokio::test]
    async fn direct_promotion_clears_handoff_suppression() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.prepare_surface_handoff();
        assert!(renderer.suppress_link_affordance);
        let target = renderer.target.as_mut().unwrap();
        let route = target.route.clone();
        target.surface_active = false;
        target.ready_reported = true;
        target.promoted = false;
        renderer.local_selected = false;
        renderer.awaiting_promotion = true;

        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20), 0);
        assert!(renderer.local_selected);
        assert!(!renderer.suppress_link_affordance);
        assert!(renderer.handoff_frame.is_none());
    }

    #[tokio::test]
    async fn clearing_pointer_repaints_without_the_synthetic_underline() {
        let url = "https://example.com/capture";
        let screen = format!("\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })]);
        let hovered = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("hovered frame")
            .frame;
        assert_ne!(
            hovered.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );

        renderer.observe_pointer_cell(None);
        assert_eq!(renderer.native_link_active(), Some(false));
        let cleanup = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("pointer-clear repaint")
            .frame;
        assert_eq!(
            cleanup.cells[0].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
    }

    #[tokio::test]
    async fn native_hover_and_click_share_one_resolver_for_plain_and_osc8_links() {
        let plain_url = "https://example.com/plain";
        let osc8_url = "https://example.com/osc8";
        let screen = format!("{plain_url}\r\n\x1b]8;;{osc8_url}\x1b\\label\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        for (column, row, expected_url, explicit) in
            [(8, 0, plain_url, false), (1, 1, osc8_url, true)]
        {
            let mouse = |kind| {
                crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::empty(),
                })
            };

            assert!(renderer
                .route_input(vec![mouse(MouseEventKind::Moved)])
                .is_empty());
            assert_eq!(renderer.native_link_active(), Some(true));
            let resolved = renderer
                .resolved_link_at(column, row)
                .expect("hovered native link");
            assert_eq!(resolved.url, expected_url);
            let frame = renderer
                .next_frame(Instant::now(), (80, 24))
                .expect("hover repaint")
                .frame;
            for &(link_column, link_row) in &resolved.cells {
                let index =
                    usize::from(link_row) * usize::from(frame.width) + usize::from(link_column);
                assert_ne!(
                    frame.cells[index].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
                    0
                );
            }
            let clicked_index = usize::from(row) * usize::from(frame.width) + usize::from(column);
            assert_eq!(frame.cells[clicked_index].hyperlink.is_some(), explicit);

            assert!(matches!(
                renderer
                    .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))])
                    .as_slice(),
                [ClientMessage::ActivateOmpLink { launch_id: 1, url }] if url == expected_url
            ));
            assert!(renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))])
                .is_empty());
        }
    }

    #[tokio::test]
    async fn native_link_activation_enforces_the_protocol_url_bound() {
        let prefix = "https://e/";
        for (length, accepted) in [
            (MAX_LINK_URL_LENGTH, true),
            (MAX_LINK_URL_LENGTH + 1, false),
        ] {
            let url = format!("{prefix}{}", "a".repeat(length - prefix.len()));
            let (runtime, _input) = TerminalRuntime::test_with_channel(length as u16, 1);
            runtime.test_process_pty_bytes(url.as_bytes());
            let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
            renderer.target.as_mut().unwrap().size = (length as u16, 1, 10, 20);

            let messages =
                renderer.route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })]);
            assert_eq!(
                messages.iter().any(|message| matches!(
                    message,
                    ClientMessage::ActivateOmpLink { url: produced, .. } if produced == &url
                )),
                accepted
            );
            assert_eq!(renderer.native_link_active(), Some(accepted));
        }
    }

    #[tokio::test]
    async fn native_link_click_routes_the_full_gesture_to_the_server() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b]8;;file:///tmp/report.md?line=7\x1b\\report\x1b]8;;\x1b\\",
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert!(matches!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))])
                .as_slice(),
            [ClientMessage::ActivateOmpLink { launch_id: 1, url }]
                if url == "file:///tmp/report.md?line=7"
        ));
        renderer.server_owned_input = true;
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Left))])
            .is_empty());
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))])
            .is_empty());
        renderer.server_owned_input = false;
        assert!(
            input.try_recv().is_err(),
            "link click must not reach the local OMP guest"
        );

        renderer.server_owned_input = true;
        assert!(matches!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))])
                .as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));
        renderer.server_owned_input = false;

        renderer.target.as_mut().unwrap().size = (80, 24, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ActivateOmpLink { launch_id: 1, url })
                if url == "file:///tmp/report.md?line=7"
        ));
        renderer.apply_target(2, 2, None, false, false, test_prefix(), (80, 24, 10, 20), 0);
        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;1m".to_vec(), geometry, 0)
            .is_none());
        assert!(
            input.try_recv().is_err(),
            "pixel link click must not reach the local OMP guest after target replacement"
        );
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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0), 0);
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

        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0), 0);

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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0), 0);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));

        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_some());
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
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0), 0);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusGained])
        ));
    }

    #[tokio::test]
    async fn promotion_replays_non_mouse_input_without_cross_surface_gestures() {
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
        assert!(renderer.awaiting_promotion);
        assert!(!renderer.local_selected);
        assert!(renderer.owns_input());
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };
        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Char('x'),
                    KeyModifiers::empty(),
                )),
                mouse(MouseEventKind::Down(MouseButton::Left)),
                mouse(MouseEventKind::Up(MouseButton::Left)),
            ])
            .is_empty());
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<35;321;241M".to_vec(), geometry, 0)
            .is_none());

        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20), 0);

        assert!(!renderer.awaiting_promotion);
        assert!(renderer.local_selected);
        assert!(renderer.owns_input());
        assert!(renderer.target.as_ref().unwrap().promoted);
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn promotion_drops_pixel_click_from_an_older_input_generation() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(24, 80, 10, 20);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b[?1016h\x1b]8;;file:///tmp/old.md\x1b\\link\x1b]8;;\x1b\\",
        );
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let target = renderer.target.as_mut().unwrap();
        let route = target.route.clone();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        renderer.take_outbound_messages();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0)
            .is_none());

        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20), 1);

        assert!(renderer.take_outbound_messages().is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn promotion_drops_raw_click_from_an_older_input_generation() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b]8;;file:///tmp/old.md\x1b\\link\x1b]8;;\x1b\\",
        );
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let target = renderer.target.as_mut().unwrap();
        let route = target.route.clone();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        renderer.take_outbound_messages();
        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })],
                0,
            )
            .is_empty());

        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20), 1);

        assert!(renderer.take_outbound_messages().is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn promotion_replay_does_not_restore_pointer_cleared_after_capture_disable() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
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
        renderer.take_outbound_messages();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 8,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<35;81;1M".to_vec(), geometry, 0)
            .is_none());
        assert!(renderer.pointer_pixels.is_some());

        renderer.observe_pointer_cell(None);
        renderer.apply_target(1, 2, Some(route), true, true, prefix, (80, 24, 10, 20), 0);
        assert_eq!(renderer.pointer_cell, None);
        assert_eq!(renderer.pointer_pixels, None);
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
        assert!(renderer.route_pixel_input(data, geometry, 0).is_none());

        renderer.apply_target(1, 2, Some(route), true, false, prefix, (80, 24, 10, 20), 0);

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
        renderer.apply_target(1, 2, Some(route), true, false, prefix, (80, 24, 10, 20), 0);
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
        assert!(renderer
            .route_pixel_input(data.clone(), geometry, 0)
            .is_none());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 10, 20), 0);
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
    async fn fallback_drops_pixel_input_from_an_older_generation() {
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
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<35;321;241M".to_vec(), geometry, 0)
            .is_none());
        assert!(renderer.awaiting_fallback);

        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 10, 20), 1);

        assert!(!renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn fallback_drops_raw_mouse_input_from_an_older_generation() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();
        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 1,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })],
                0,
            )
            .is_empty());
        assert!(renderer.awaiting_fallback);

        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 10, 20), 1);

        assert!(!renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
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
        renderer.apply_target(2, 2, None, false, false, test_prefix(), (80, 24, 0, 0), 0);
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
