use std::collections::VecDeque;
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
use crate::terminal::{
    OmpPhysicalKeyPresses, OmpPhysicalKeyRoute, OmpReplyNavigationPresses, OmpReplyNavigationRoute,
    TerminalRuntime,
};

pub(super) const OMP_RENDERER_LAUNCH_ID_ENV: &str = "HERDR_OMP_RENDERER_LAUNCH_ID";

const BIND_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_DAMAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUEUED_LINK_INPUTS: usize = 256;
const MAX_TRACKED_SERVER_PRESSES: usize = 256;
const MAX_PENDING_INPUT_EVENTS: usize = 4096;
const MAX_PENDING_INPUT_PAYLOAD_BYTES: usize = 1024 * 1024;
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

enum LinkInput {
    Events {
        events: Vec<ClientInputEvent>,
        generation: u64,
    },
    Pixels {
        inputs: Vec<(Vec<u8>, crate::input::mouse::HostGeometry)>,
        generation: u64,
    },
}

impl LinkInput {
    fn generation(&self) -> u64 {
        match self {
            Self::Events { generation, .. } | Self::Pixels { generation, .. } => *generation,
        }
    }

    fn input_usage(&self) -> (usize, usize) {
        match self {
            Self::Events { events, .. } => client_input_events_usage(events),
            Self::Pixels { inputs, .. } => inputs.iter().fold((0, 0), |mut total, (data, _)| {
                add_input_usage(&mut total, (1, data.len()));
                total
            }),
        }
    }
}

fn client_input_event_usage(event: &ClientInputEvent) -> (usize, usize) {
    let payload = match event {
        ClientInputEvent::Key {
            generated_text,
            source,
            ..
        } => {
            generated_text.as_ref().map_or(0, String::len)
                + match source {
                    ClientKeySource::Vt { bytes } => bytes.len(),
                    ClientKeySource::Synthesized | ClientKeySource::WindowsConsole { .. } => 0,
                }
        }
        ClientInputEvent::TextCommit(text) | ClientInputEvent::Paste { text } => text.len(),
        ClientInputEvent::Mouse { .. }
        | ClientInputEvent::FocusGained
        | ClientInputEvent::FocusLost => 0,
    };
    (1, payload)
}

fn add_input_usage(total: &mut (usize, usize), usage: (usize, usize)) {
    total.0 = total.0.saturating_add(usage.0);
    total.1 = total.1.saturating_add(usage.1);
}

fn client_input_events_usage(events: &[ClientInputEvent]) -> (usize, usize) {
    let mut total = (0, 0);
    for event in events {
        add_input_usage(&mut total, client_input_event_usage(event));
    }
    total
}

impl DeferredMessage {
    fn retain_for_generation(&mut self, current_generation: u64) -> bool {
        match self {
            Self::InputEvents { events, generation } => {
                if *generation != current_generation {
                    events.retain(|event| !matches!(event, ClientInputEvent::Mouse { .. }));
                }
                !events.is_empty()
            }
            Self::InputPixels { generation, .. } => *generation == current_generation,
        }
    }
    fn input_usage(&self) -> (usize, usize) {
        match self {
            Self::InputEvents { events, .. } => client_input_events_usage(events),
            Self::InputPixels { data, .. } => (1, data.len()),
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
        scrollback_limit_bytes: usize,
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
            scrollback_limit_bytes,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            events_tx,
            render_notify,
            render_dirty.clone(),
        )?;
        runtime.set_preserve_primary_scrollback(true);
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
                AppEvent::PaneDied { pane_id, .. } if pane_id == self.pane_id => {
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
                AppEvent::PaneClipboardWrite { pane_id, content }
                    if self.promoted && pane_id == self.pane_id =>
                {
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
    scrollback_limit_bytes: usize,
    mouse_scroll_lines: usize,
    latest_launch_id: u64,
    target: Option<LocalTarget>,
    cached_server_frame: Option<FrameData>,
    handoff_frame: Option<FrameData>,
    local_selected: bool,
    server_owned_input: bool,
    omp_reply_navigation_presses: OmpReplyNavigationPresses,
    local_physical_presses: OmpPhysicalKeyPresses,
    // Host-key lifecycles already sent to the server, including semantic Unix Kitty keys.
    server_forwarded_presses: Vec<crate::input::KeyIdentity>,
    server_forwarded_overflow: bool,
    next_link_request_id: u64,
    pending_link_click: bool,
    pending_link_request_id: Option<u64>,
    pending_link_input: Option<LinkInput>,
    queued_link_inputs: Vec<LinkInput>,
    pointer_cell: Option<(u16, u16)>,
    pointer_pixels: Option<crate::input::mouse::HostPixels>,
    hovered_link_cells: Option<Vec<(u16, u16)>>,
    suppress_link_affordance: bool,
    awaiting_fallback: bool,
    awaiting_promotion: bool,
    deferred_messages: Vec<DeferredMessage>,
    outbound_messages: Vec<ClientMessage>,
    post_send_input: VecDeque<DeferredMessage>,
    effects: Vec<LocalEffect>,
    needs_render: bool,
    force_repaint: bool,
}

impl ClientOmpRenderer {
    pub(super) fn new(
        omp_executable: Option<crate::update::OmpExecutable>,
        scrollback_limit_bytes: usize,
        mouse_scroll_lines: usize,
    ) -> Self {
        Self {
            omp_executable,
            scrollback_limit_bytes,
            mouse_scroll_lines: mouse_scroll_lines.max(1),
            latest_launch_id: 0,
            target: None,
            cached_server_frame: None,
            handoff_frame: None,
            local_selected: false,
            server_owned_input: false,
            omp_reply_navigation_presses: OmpReplyNavigationPresses::default(),
            local_physical_presses: OmpPhysicalKeyPresses::default(),
            server_forwarded_presses: Vec::new(),
            server_forwarded_overflow: false,
            next_link_request_id: 1,
            pending_link_click: false,
            pending_link_request_id: None,
            pending_link_input: None,
            queued_link_inputs: Vec::new(),
            pointer_cell: None,
            pointer_pixels: None,
            hovered_link_cells: None,
            awaiting_fallback: false,
            suppress_link_affordance: false,
            awaiting_promotion: false,
            deferred_messages: Vec::new(),
            outbound_messages: Vec::new(),
            post_send_input: VecDeque::new(),
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
            self.release_pending_input_to_server();
            self.stop_target();
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
            self.release_pending_input_to_server();
            self.stop_target();
            self.cached_server_frame = None;
            self.latest_launch_id = launch_id;
            let scrollback_limit_bytes = self.scrollback_limit_bytes;
            let target = self.omp_executable.as_ref().and_then(|omp_executable| {
                LocalTarget::spawn(
                    omp_executable,
                    launch_id,
                    target_app_client_id,
                    route,
                    prefix.clone(),
                    size,
                    scrollback_limit_bytes,
                )
                .ok()
            });
            self.target = target;
            self.needs_render = true;
        }
        let loses_binding = self
            .target
            .as_ref()
            .is_some_and(|target| target.bound && !bound);
        if loses_binding {
            self.release_pending_input_to_server();
        } else if !bound {
            self.cached_server_frame = None;
            self.release_deferred_messages();
        }
        if self.local_selected
            && self
                .target
                .as_ref()
                .is_some_and(|target| (target.surface_active && !surface_active) || loses_binding)
        {
            self.prepare_surface_handoff();
        }
        if loses_binding {
            self.retire_local_input_owner();
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
                    && !self.server_owned_input
                    && !self.server_forwarded_overflow,
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
        self.cancel_stale_link_inputs(current_input_generation);
        self.deferred_messages
            .retain_mut(|message| message.retain_for_generation(current_input_generation));
        self.post_send_input
            .retain_mut(|message| message.retain_for_generation(current_input_generation));
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

    pub(super) fn observe_server_input(&mut self, events: &[crate::raw_input::RawInputEvent]) {
        for event in events {
            if matches!(event, crate::raw_input::RawInputEvent::OuterFocusLost) {
                self.server_forwarded_presses.clear();
                self.server_forwarded_overflow = false;
                continue;
            }
            let crate::raw_input::RawInputEvent::Key(key) = event else {
                continue;
            };
            let identity = key.identity();
            match key.kind {
                KeyEventKind::Press => {
                    if self.server_forwarded_presses.contains(&identity) {
                        continue;
                    }
                    if self.server_forwarded_presses.len() < MAX_TRACKED_SERVER_PRESSES {
                        self.server_forwarded_presses.push(identity);
                    } else {
                        // A malicious key stream cannot grow state without bound. Keep all input
                        // server-routed until focus loss safely resets it.
                        self.server_forwarded_overflow = true;
                    }
                }
                KeyEventKind::Release => {
                    self.server_forwarded_presses
                        .retain(|pressed| *pressed != identity);
                }
                KeyEventKind::Repeat => {}
            }
        }
    }

    fn route_existing_server_input(&mut self, key: &crate::input::TerminalKey) -> bool {
        if self.server_forwarded_overflow {
            return true;
        }
        let identity = key.identity();
        let Some(index) = self
            .server_forwarded_presses
            .iter()
            .position(|pressed| *pressed == identity)
        else {
            return false;
        };
        if key.kind == KeyEventKind::Press && !key.has_physical_identity() {
            self.server_forwarded_presses.swap_remove(index);
            return false;
        }
        if key.kind == KeyEventKind::Release {
            self.server_forwarded_presses.swap_remove(index);
        }
        true
    }

    fn queue_server_message(&mut self, message: ClientMessage) {
        if let ClientMessage::InputEvents { events } = &message {
            let events = events
                .iter()
                .map(ClientInputEvent::to_raw_input_event)
                .collect::<Vec<_>>();
            self.observe_server_input(&events);
        }
        self.outbound_messages.push(message);
    }

    pub(super) fn owns_surface_input(&self) -> bool {
        self.local_selected
            || self.server_owned_input
            || self.server_forwarded_overflow
            || self.awaiting_fallback
            || self.awaiting_promotion
    }

    pub(super) fn owns_input(&self) -> bool {
        self.owns_surface_input()
            || !self.omp_reply_navigation_presses.is_empty()
            || self.local_physical_presses.owns_input()
            || !self.post_send_input.is_empty()
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
        if !self.local_selected || self.server_owned_input || self.server_forwarded_overflow {
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
        let cell = match self.target.as_ref() {
            Some(target) => local_pixel_cell(pointer, target.size),
            None => pointer.geometry.cell(pointer.x, pointer.y),
        };
        self.set_pointer_cell(cell);
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

    #[cfg(test)]
    pub(super) fn native_link_active(&self) -> Option<bool> {
        if self.suppress_link_affordance {
            return Some(false);
        }
        if !self.local_selected || self.server_owned_input || self.server_forwarded_overflow {
            return None;
        }
        Some(
            self.pointer_cell
                .is_some_and(|(column, row)| self.resolved_link_at(column, row).is_some()),
        )
    }

    fn allocate_link_request_id(&mut self) -> u64 {
        let request_id = self.next_link_request_id;
        self.next_link_request_id = self.next_link_request_id.wrapping_add(1).max(1);
        request_id
    }

    fn queue_link_input(&mut self, input: LinkInput) {
        if self.queued_link_inputs.len() < MAX_QUEUED_LINK_INPUTS {
            self.queued_link_inputs.push(input);
        } else {
            self.fail_link_input_overflow(input);
        }
    }

    fn retain_link_inputs_for_generation(&mut self, current_generation: u64) -> bool {
        let queued = std::mem::take(&mut self.queued_link_inputs);
        let mut retained = Vec::with_capacity(queued.len());
        let mut changed = false;
        for input in queued {
            match input {
                LinkInput::Events {
                    mut events,
                    generation,
                } if generation != current_generation => {
                    changed = true;
                    events.retain(|event| !matches!(event, ClientInputEvent::Mouse { .. }));
                    if !events.is_empty() {
                        retained.push(LinkInput::Events { events, generation });
                    }
                }
                LinkInput::Pixels { generation, .. } if generation != current_generation => {
                    changed = true;
                }
                input => retained.push(input),
            }
        }
        self.queued_link_inputs = retained;
        changed
    }

    fn cancel_stale_link_inputs(&mut self, current_generation: u64) {
        let stale_pending = self
            .pending_link_input
            .as_ref()
            .is_some_and(|input| input.generation() != current_generation);
        let changed = self.retain_link_inputs_for_generation(current_generation);
        if stale_pending {
            self.clear_pending_link_activation();
        }
        if stale_pending || changed {
            self.drain_queued_link_inputs();
        }
    }

    fn link_activation_message(
        &self,
        column: u16,
        row: u16,
        request_id: u64,
    ) -> Option<ClientMessage> {
        let link = self.resolved_link_at(column, row)?;
        Some(ClientMessage::ActivateOmpLink {
            launch_id: self.target.as_ref()?.launch_id,
            request_id,
            url: link.url,
        })
    }

    fn can_extend_server_batch(&self, event: &crate::raw_input::RawInputEvent) -> bool {
        self.pending_link_request_id.is_none()
            && self.server_owned_input
            && !matches!(event, crate::raw_input::RawInputEvent::Key(key)
                if self.local_physical_presses.owns_key(key)
                    || self.omp_reply_navigation_presses.owns_key(key))
    }

    fn pending_input_usage(&self) -> (usize, usize) {
        let mut total = (0, 0);
        for message in &self.deferred_messages {
            add_input_usage(&mut total, message.input_usage());
        }
        for message in &self.post_send_input {
            add_input_usage(&mut total, message.input_usage());
        }
        if let Some(input) = &self.pending_link_input {
            add_input_usage(&mut total, input.input_usage());
        }
        for input in &self.queued_link_inputs {
            add_input_usage(&mut total, input.input_usage());
        }
        for message in &self.outbound_messages {
            match message {
                ClientMessage::InputEvents { events } => {
                    add_input_usage(&mut total, client_input_events_usage(events));
                }
                ClientMessage::InputPixels { data, .. } => {
                    add_input_usage(&mut total, (1, data.len()));
                }
                _ => {}
            }
        }
        total
    }

    fn pending_input_would_overflow(&self, incoming: (usize, usize)) -> bool {
        let pending = self.pending_input_usage();
        pending.0.saturating_add(incoming.0) > MAX_PENDING_INPUT_EVENTS
            || pending.1.saturating_add(incoming.1) > MAX_PENDING_INPUT_PAYLOAD_BYTES
    }

    fn route_admission_overflow_to_server(
        &mut self,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> Vec<ClientMessage> {
        self.begin_local_forward_fallback();
        self.release_pending_input_to_server();
        let events = events
            .into_iter()
            .filter_map(|event| client_event_from_raw(&event))
            .filter(|event| !matches!(event, ClientInputEvent::Mouse { .. }))
            .collect::<Vec<_>>();
        if !events.is_empty() {
            self.queue_server_message(ClientMessage::InputEvents { events });
        }
        self.take_outbound_messages()
    }

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
        self.cancel_stale_link_inputs(input_generation);
        let incoming = events
            .iter()
            .filter_map(client_event_from_raw)
            .collect::<Vec<_>>();
        if self.pending_input_would_overflow(client_input_events_usage(&incoming)) {
            return self.route_admission_overflow_to_server(events);
        }
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
        let mut post_send_events = Vec::new();
        let mut defer_ordered_tail = false;
        for event in events {
            let protocol_event = client_event_from_raw(&event);
            if self.pending_link_request_id.is_some() {
                match &event {
                    crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        ..
                    }) => self.pending_link_click = false,
                    crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Drag(MouseButton::Left),
                        ..
                    }) if self.pending_link_click => {
                        self.append_pending_link_event(protocol_event);
                        continue;
                    }
                    crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        ..
                    }) if self.pending_link_click => {
                        self.append_pending_link_event(protocol_event);
                        self.pending_link_click = false;
                        continue;
                    }
                    _ => {}
                }
                if let Some(event) = protocol_event {
                    self.queue_link_input(LinkInput::Events {
                        events: vec![event],
                        generation: input_generation,
                    });
                }
                continue;
            }
            if (self.awaiting_fallback || self.awaiting_promotion)
                && (!self.deferred_messages.is_empty() || !deferred_events.is_empty())
            {
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
                }
                continue;
            }
            if defer_ordered_tail
                && (!post_send_events.is_empty() || !self.can_extend_server_batch(&event))
            {
                if let Some(event) = protocol_event {
                    post_send_events.push(event);
                }
                continue;
            }
            if matches!(&event, crate::raw_input::RawInputEvent::Key(key)
                if self.route_existing_server_input(key))
            {
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                }
                defer_ordered_tail = true;
                continue;
            }
            let tracked_key = match &event {
                crate::raw_input::RawInputEvent::Key(key) => Some(key.clone()),
                _ => None,
            };
            let local_physical_route = tracked_key
                .as_ref()
                .and_then(|key| self.local_physical_presses.route_existing(key));
            if let Some(route) = local_physical_route {
                match route {
                    OmpPhysicalKeyRoute::Forwarded => {
                        let sent = self
                            .target
                            .as_ref()
                            .and_then(|target| target.runtime.as_ref())
                            .is_some_and(|runtime| {
                                forward_local_event(runtime, event, self.mouse_scroll_lines)
                            });
                        if let Some(key) = tracked_key.as_ref() {
                            if key.kind == KeyEventKind::Release {
                                self.local_physical_presses.forget(key);
                                if !sent {
                                    self.begin_local_forward_fallback();
                                }
                            } else if !sent {
                                self.local_physical_presses.suppress_existing(key);
                                self.begin_local_forward_fallback();
                            }
                        }
                    }
                    OmpPhysicalKeyRoute::ReplyNavigation => {
                        if matches!(tracked_key.as_ref(), Some(key) if key.kind != KeyEventKind::Release)
                        {
                            let navigated = self
                                .target
                                .as_ref()
                                .and_then(|target| target.runtime.as_ref())
                                .is_some_and(|runtime| {
                                    try_navigate_local_omp_reply(runtime, &event)
                                });
                            if navigated {
                                self.needs_render = true;
                                self.refresh_hovered_link();
                            }
                        }
                    }
                    OmpPhysicalKeyRoute::Suppressed => {}
                }
                continue;
            }

            let target = &self.target;
            let local_semantic_route = match &event {
                crate::raw_input::RawInputEvent::Key(key) => self
                    .omp_reply_navigation_presses
                    .route_existing_with(key, || {
                        target
                            .as_ref()
                            .and_then(|target| target.runtime.as_ref())
                            .is_some_and(|runtime| try_navigate_local_omp_reply(runtime, &event))
                    }),
                _ => None,
            };
            if let Some(outcome) = local_semantic_route {
                match outcome {
                    OmpReplyNavigationRoute::Forwarded => {
                        let sent = self
                            .target
                            .as_ref()
                            .and_then(|target| target.runtime.as_ref())
                            .is_some_and(|runtime| {
                                forward_local_event(runtime, event, self.mouse_scroll_lines)
                            });
                        if let Some(key) = tracked_key.as_ref() {
                            if key.kind == KeyEventKind::Release {
                                self.omp_reply_navigation_presses.forget(key);
                                if !sent {
                                    self.begin_local_forward_fallback();
                                }
                            } else if !sent {
                                self.omp_reply_navigation_presses.suppress_existing(key);
                                self.begin_local_forward_fallback();
                            }
                        }
                    }
                    OmpReplyNavigationRoute::Consumed { navigated: true } => {
                        self.needs_render = true;
                        self.refresh_hovered_link();
                    }
                    OmpReplyNavigationRoute::Consumed { navigated: false } => {}
                }
                continue;
            }
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
                        if self.pending_link_request_id.is_none() {
                            self.clear_pending_link_click();
                        } else {
                            self.pending_link_click = false;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) if self.pending_link_click => {
                        self.append_pending_link_event(protocol_event);
                        continue;
                    }
                    MouseEventKind::Up(MouseButton::Left) if self.pending_link_click => {
                        self.append_pending_link_event(protocol_event);
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
            if matches!(&event, crate::raw_input::RawInputEvent::OuterFocusLost) {
                let mut forward_server_focus_loss = !self.server_forwarded_presses.is_empty()
                    || self.server_forwarded_overflow
                    || self.server_owned_input
                    || !self.local_selected;
                forward_server_focus_loss |= !self.release_local_input_for_focus_loss();
                self.server_forwarded_presses.clear();
                self.server_forwarded_overflow = false;
                if forward_server_focus_loss {
                    if let Some(event) = protocol_event {
                        server_batch.push(event);
                    }
                    defer_ordered_tail = true;
                }
                continue;
            }
            if self.local_selected && !self.server_owned_input && !self.server_forwarded_overflow {
                if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        let request_id = self.allocate_link_request_id();
                        if let Some(message) =
                            self.link_activation_message(mouse.column, mouse.row, request_id)
                        {
                            if !server_batch.is_empty() {
                                messages.push(ClientMessage::InputEvents {
                                    events: std::mem::take(&mut server_batch),
                                });
                            }
                            messages.push(message);
                            defer_ordered_tail = true;
                            self.pending_link_click = true;
                            self.pending_link_request_id = Some(request_id);
                            self.pending_link_input =
                                protocol_event.map(|event| LinkInput::Events {
                                    events: vec![event],
                                    generation: input_generation,
                                });
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
                self.observe_server_input(std::slice::from_ref(&event));
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
                defer_ordered_tail = true;
                self.prepare_surface_handoff();
                self.server_owned_input = true;
                self.refresh_hovered_link();
                self.cached_server_frame = None;
                self.needs_render = true;
                self.force_repaint = true;
                continue;
            }
            if self.server_owned_input || self.server_forwarded_overflow || !self.local_selected {
                self.observe_server_input(std::slice::from_ref(&event));
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                    defer_ordered_tail = true;
                }
                continue;
            }
            let (needs_render, sent) = self.route_local_event(event);
            if needs_render {
                self.needs_render = true;
                self.refresh_hovered_link();
                continue;
            }
            if !sent {
                self.begin_local_forward_fallback();
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
        if !post_send_events.is_empty() {
            self.post_send_input
                .push_back(DeferredMessage::InputEvents {
                    events: post_send_events,
                    generation: input_generation,
                });
        }
        messages
    }

    /// Returns `(needs_render, sent)` for local input handling.
    fn route_local_event(&mut self, event: crate::raw_input::RawInputEvent) -> (bool, bool) {
        let key = match &event {
            crate::raw_input::RawInputEvent::Key(key) => Some(key),
            _ => None,
        };
        if let Some(key) = key {
            if key.has_physical_identity() {
                if key.kind != KeyEventKind::Press {
                    return (false, true);
                }
                let Some(releases) = self.local_physical_presses.reserve_press(key) else {
                    return (false, true);
                };
                if !self.forward_local_releases(releases) {
                    self.local_physical_presses.forget(key);
                    self.begin_local_forward_fallback();
                    return (false, false);
                }
                let navigated = self
                    .target
                    .as_ref()
                    .and_then(|target| target.runtime.as_ref())
                    .is_some_and(|runtime| try_navigate_local_omp_reply(runtime, &event));
                let route = if navigated {
                    OmpPhysicalKeyRoute::ReplyNavigation
                } else {
                    OmpPhysicalKeyRoute::Forwarded
                };
                let committed = self.local_physical_presses.commit_press(key, route);
                debug_assert!(committed);
                if navigated {
                    return (true, true);
                }
            } else {
                let target = &self.target;
                let outcome = self.omp_reply_navigation_presses.route(key, || {
                    target
                        .as_ref()
                        .and_then(|target| target.runtime.as_ref())
                        .is_some_and(|runtime| try_navigate_local_omp_reply(runtime, &event))
                });
                if let OmpReplyNavigationRoute::Consumed { navigated } = outcome {
                    return (navigated, true);
                }
                if !self.omp_reply_navigation_presses.owns_key(key) {
                    if key.kind != KeyEventKind::Press {
                        return (false, true);
                    }
                    let Some(releases) = self.local_physical_presses.reserve_press(key) else {
                        return (false, true);
                    };
                    if !self.forward_local_releases(releases) {
                        self.local_physical_presses.forget(key);
                        self.begin_local_forward_fallback();
                        return (false, false);
                    }
                    let committed = self
                        .local_physical_presses
                        .commit_press(key, OmpPhysicalKeyRoute::Forwarded);
                    debug_assert!(committed);
                }
            }
        }

        let wheel = matches!(
            &event,
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp | MouseEventKind::ScrollDown,
                ..
            })
        );
        let resets_viewport = match &event {
            crate::raw_input::RawInputEvent::Key(key) => {
                key.kind != crossterm::event::KeyEventKind::Release
            }
            crate::raw_input::RawInputEvent::Text(_)
            | crate::raw_input::RawInputEvent::Paste(_) => true,
            _ => false,
        };
        let tracked_key = key.cloned();
        let sent = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .is_some_and(|runtime| forward_local_event(runtime, event, self.mouse_scroll_lines));
        if !sent {
            if let Some(key) = tracked_key.as_ref() {
                self.local_physical_presses.forget(key);
                self.omp_reply_navigation_presses.forget(key);
            }
        }
        ((wheel || resets_viewport) && sent, sent)
    }

    pub(super) fn route_pixel_input(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
    ) -> Option<ClientMessage> {
        self.cancel_stale_link_inputs(input_generation);
        if self.pending_input_would_overflow((1, data.len())) {
            self.begin_local_forward_fallback();
            self.release_pending_input_to_server();
            return None;
        }
        self.route_pixel_input_inner(data, geometry, input_generation, true)
    }

    fn route_pixel_input_inner(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
        observe_pointer: bool,
    ) -> Option<ClientMessage> {
        let mouse_kind = decode_pixel_mouse(&data).map(|mouse| mouse.kind);
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
        match mouse_kind {
            Some(MouseEventKind::Down(MouseButton::Left)) => {
                if self.pending_link_request_id.is_none() {
                    self.clear_pending_link_click();
                } else {
                    self.pending_link_click = false;
                }
            }
            Some(MouseEventKind::Drag(MouseButton::Left)) if self.pending_link_click => {
                self.append_pending_link_pixels(data, geometry);
                return None;
            }
            Some(MouseEventKind::Up(MouseButton::Left)) if self.pending_link_click => {
                self.append_pending_link_pixels(data, geometry);
                self.pending_link_click = false;
                return None;
            }
            _ => {}
        }
        if self.pending_link_request_id.is_some() {
            self.queue_link_input(LinkInput::Pixels {
                inputs: vec![(data, geometry)],
                generation: input_generation,
            });
            return None;
        }
        if self.awaiting_fallback || self.awaiting_promotion {
            self.deferred_messages.push(DeferredMessage::InputPixels {
                data,
                geometry,
                generation: input_generation,
            });
            return None;
        }
        if self.server_owned_input || self.server_forwarded_overflow || !self.local_selected {
            return Some(pixel_input_message(data, geometry));
        }
        if let Some(local_mouse) = self
            .target
            .as_ref()
            .and_then(|target| decode_local_pixel_mouse(&data, geometry, target.size))
            .filter(|mouse| matches!(mouse.mouse.kind, MouseEventKind::Down(MouseButton::Left)))
        {
            let request_id = self.allocate_link_request_id();
            if let Some(message) =
                self.link_activation_message(local_mouse.cell.0, local_mouse.cell.1, request_id)
            {
                self.pending_link_click = true;
                self.pending_link_request_id = Some(request_id);
                self.pending_link_input = Some(LinkInput::Pixels {
                    inputs: vec![(data, geometry)],
                    generation: input_generation,
                });
                return Some(message);
            }
        }
        let wheel = local_mouse.as_ref().is_some_and(|mouse| {
            matches!(
                mouse.mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        });
        let sent = self.target.as_ref().and_then(|target| {
            let runtime = target.runtime.as_ref()?;
            Some(forward_local_pixel_mouse(
                runtime,
                local_mouse?,
                self.mouse_scroll_lines,
            ))
        });
        match sent {
            Some(true) => {
                if wheel {
                    self.needs_render = true;
                    self.refresh_hovered_link();
                }
                None
            }
            None => Some(pixel_input_message(data, geometry)),
            Some(false) => {
                self.begin_local_forward_fallback();
                self.deferred_messages.push(DeferredMessage::InputPixels {
                    data,
                    geometry,
                    generation: input_generation,
                });
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
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.failed || target.runtime.is_none())
        {
            self.retire_local_input_owner();
        }
        let should_select = self.target.as_ref().is_some_and(|target| {
            !target.failed
                && target.runtime.is_some()
                && target.bound
                && target.surface_active
                && target.first_damage
                && !self.server_owned_input
                && !self.server_forwarded_overflow
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

    pub(super) fn flush_post_send_input(&mut self) -> Vec<ClientMessage> {
        loop {
            let mut queued = std::mem::take(&mut self.post_send_input);
            let Some(message) = queued.pop_front() else {
                return Vec::new();
            };
            let messages = match message {
                DeferredMessage::InputEvents { events, generation } => {
                    let events = events
                        .into_iter()
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    self.route_input_inner(events, generation, false)
                }
                DeferredMessage::InputPixels {
                    data,
                    geometry,
                    generation,
                } => self
                    .route_pixel_input_inner(data, geometry, generation, false)
                    .into_iter()
                    .collect(),
            };
            self.post_send_input.append(&mut queued);
            if !messages.is_empty() {
                return messages;
            }
        }
    }

    pub(super) fn resolve_link_activation(
        &mut self,
        launch_id: u64,
        request_id: u64,
        activated: bool,
        input_generation: u64,
    ) {
        if self.pending_link_request_id != Some(request_id) {
            return;
        }
        if self
            .target
            .as_ref()
            .is_none_or(|target| target.launch_id != launch_id)
        {
            self.clear_pending_link_activation();
            self.drain_link_inputs_for_generation(input_generation);
            return;
        }
        self.pending_link_request_id = None;
        if activated {
            // Keep suppressing the remainder of the current mouse gesture, but
            // discard the buffered events because the server handled the link.
            self.pending_link_input = None;
            self.drain_link_inputs_for_generation(input_generation);
            return;
        }
        let Some(input) = self.pending_link_input.take() else {
            self.pending_link_click = false;
            self.drain_link_inputs_for_generation(input_generation);
            return;
        };
        self.pending_link_click = false;
        if input.generation() != input_generation {
            self.clear_pending_link_activation();
            self.drain_link_inputs_for_generation(input_generation);
            return;
        }

        let replay_locally =
            self.local_selected && !self.server_owned_input && !self.server_forwarded_overflow;
        let runtime_missing = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .is_none();
        match input {
            LinkInput::Events {
                events,
                generation: _,
            } if !replay_locally => {
                self.queue_server_message(ClientMessage::InputEvents { events });
            }
            LinkInput::Events { events, generation } => {
                let mut needs_render = false;
                let mut failed_at = None;
                for (index, event) in events.iter().enumerate() {
                    let (did_render, sent) = self.route_local_event(event.to_raw_input_event());
                    needs_render |= did_render;
                    if !sent {
                        failed_at = Some(index);
                        break;
                    }
                }
                if needs_render {
                    self.needs_render = true;
                    self.refresh_hovered_link();
                }
                if let Some(index) = failed_at {
                    self.begin_local_forward_fallback();
                    self.deferred_messages.push(DeferredMessage::InputEvents {
                        events: events.into_iter().skip(index).collect(),
                        generation,
                    });
                }
            }
            LinkInput::Pixels {
                inputs,
                generation: _,
            } if !replay_locally => {
                self.outbound_messages.extend(
                    inputs
                        .into_iter()
                        .map(|(data, geometry)| pixel_input_message(data, geometry)),
                );
            }
            LinkInput::Pixels { inputs, generation } => {
                let mouse_scroll_lines = self.mouse_scroll_lines;
                let mut needs_render = false;
                let failed_at = self
                    .target
                    .as_ref()
                    .and_then(|target| {
                        target
                            .runtime
                            .as_ref()
                            .map(|runtime| (runtime, target.size))
                    })
                    .and_then(|(runtime, size)| {
                        inputs
                            .iter()
                            .enumerate()
                            .find_map(|(index, (data, geometry))| {
                                let Some(mouse) = decode_local_pixel_mouse(data, *geometry, size)
                                else {
                                    return Some(index);
                                };
                                let wheel = matches!(
                                    mouse.mouse.kind,
                                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                                );
                                let sent =
                                    forward_local_pixel_mouse(runtime, mouse, mouse_scroll_lines);
                                needs_render |= wheel && sent;
                                (!sent).then_some(index)
                            })
                    })
                    .or_else(|| runtime_missing.then_some(0));
                if needs_render {
                    self.needs_render = true;
                    self.refresh_hovered_link();
                }
                if let Some(index) = failed_at {
                    self.begin_local_forward_fallback();
                    for (data, geometry) in inputs.into_iter().skip(index) {
                        self.deferred_messages.push(DeferredMessage::InputPixels {
                            data,
                            geometry,
                            generation,
                        });
                    }
                }
            }
        }
        self.drain_queued_link_inputs();
    }

    fn append_pending_link_event(&mut self, event: Option<ClientInputEvent>) -> bool {
        let Some(event) = event else {
            return true;
        };
        let overflow_generation = self.pending_link_input.as_ref().and_then(|input| {
            let LinkInput::Events { events, generation } = input else {
                return None;
            };
            (events.len() >= MAX_QUEUED_LINK_INPUTS).then_some(*generation)
        });
        if let Some(generation) = overflow_generation {
            self.fail_link_input_overflow(LinkInput::Events {
                events: vec![event],
                generation,
            });
            return false;
        }
        if let Some(LinkInput::Events { events, .. }) = self.pending_link_input.as_mut() {
            events.push(event);
        }
        true
    }

    fn append_pending_link_pixels(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
    ) -> bool {
        let overflow_generation = self.pending_link_input.as_ref().and_then(|input| {
            let LinkInput::Pixels { inputs, generation } = input else {
                return None;
            };
            (inputs.len() >= MAX_QUEUED_LINK_INPUTS).then_some(*generation)
        });
        if let Some(generation) = overflow_generation {
            self.fail_link_input_overflow(LinkInput::Pixels {
                inputs: vec![(data, geometry)],
                generation,
            });
            return false;
        }
        if let Some(LinkInput::Pixels { inputs, .. }) = self.pending_link_input.as_mut() {
            inputs.push((data, geometry));
        }
        true
    }

    fn fail_link_input_overflow(&mut self, input: LinkInput) {
        let mut inputs = Vec::with_capacity(self.queued_link_inputs.len() + 2);
        if let Some(pending) = self.pending_link_input.take() {
            inputs.push(pending);
        }
        inputs.extend(std::mem::take(&mut self.queued_link_inputs));
        inputs.push(input);
        self.clear_pending_link_activation();
        self.begin_local_forward_fallback();
        for input in inputs {
            self.defer_link_input(input);
        }
    }

    fn deferred_link_input(input: LinkInput) -> Vec<DeferredMessage> {
        match input {
            LinkInput::Events { events, generation } => {
                vec![DeferredMessage::InputEvents { events, generation }]
            }
            LinkInput::Pixels { inputs, generation } => inputs
                .into_iter()
                .map(|(data, geometry)| DeferredMessage::InputPixels {
                    data,
                    geometry,
                    generation,
                })
                .collect(),
        }
    }

    fn defer_link_input(&mut self, input: LinkInput) {
        self.deferred_messages
            .extend(Self::deferred_link_input(input));
    }

    fn drain_queued_link_inputs(&mut self) {
        let mut queued = std::mem::take(&mut self.queued_link_inputs)
            .into_iter()
            .flat_map(Self::deferred_link_input)
            .collect::<VecDeque<_>>();
        while let Some(input) = queued.pop_front() {
            let messages = match input {
                DeferredMessage::InputEvents { events, generation } => {
                    let raw_events = events
                        .into_iter()
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    self.route_input_inner(raw_events, generation, false)
                }
                DeferredMessage::InputPixels {
                    data,
                    geometry,
                    generation,
                } => self
                    .route_pixel_input_inner(data, geometry, generation, false)
                    .into_iter()
                    .collect(),
            };
            if !messages.is_empty() {
                self.outbound_messages.extend(messages);
                self.post_send_input.extend(queued);
                return;
            }
        }
    }
    fn drain_link_inputs_for_generation(&mut self, current_generation: u64) {
        self.retain_link_inputs_for_generation(current_generation);
        self.drain_queued_link_inputs();
    }

    fn begin_local_forward_fallback(&mut self) {
        self.prepare_surface_handoff();
        self.retire_local_input_owner();
        if let Some(target) = self.target.as_mut() {
            target.fail();
        }
        self.awaiting_fallback = true;
        self.cached_server_frame = None;
        self.needs_render = true;
        self.force_repaint = true;
    }

    fn clear_pending_link_activation(&mut self) {
        self.pending_link_click = false;
        self.pending_link_request_id = None;
        self.pending_link_input = None;
    }

    fn clear_pending_link_click(&mut self) {
        self.clear_pending_link_activation();
        self.queued_link_inputs.clear();
    }

    pub(super) fn take_effects(&mut self) -> Vec<LocalEffect> {
        std::mem::take(&mut self.effects)
    }

    fn resolve_promotion(&mut self, local_active: bool, current_input_generation: u64) {
        self.awaiting_promotion = false;
        let mut deferred = VecDeque::from(std::mem::take(&mut self.deferred_messages));
        if !local_active {
            while let Some(message) = deferred.pop_front() {
                if self.queue_deferred_message_to_server(message) {
                    self.post_send_input.extend(deferred);
                    return;
                }
            }
            return;
        }
        while let Some(message) = deferred.pop_front() {
            if self.awaiting_fallback {
                self.deferred_messages.push(message);
                self.deferred_messages.extend(deferred);
                return;
            }
            match message {
                DeferredMessage::InputEvents { events, .. } => {
                    let events = events
                        .into_iter()
                        .filter(|event| !matches!(event, ClientInputEvent::Mouse { .. }))
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    let messages = self.route_input_inner(events, current_input_generation, false);
                    if !messages.is_empty() {
                        self.outbound_messages.extend(messages);
                        self.post_send_input.extend(deferred);
                        return;
                    }
                }
                DeferredMessage::InputPixels { .. } => {}
            }
        }
    }

    fn release_deferred_messages(&mut self) {
        if self.awaiting_fallback || self.awaiting_promotion {
            self.awaiting_fallback = false;
            self.awaiting_promotion = false;
            let mut deferred = VecDeque::from(std::mem::take(&mut self.deferred_messages));
            while let Some(message) = deferred.pop_front() {
                if self.queue_deferred_message_to_server(message) {
                    self.post_send_input.extend(deferred);
                    return;
                }
            }
        }
    }

    fn release_pending_input_to_server(&mut self) {
        let outbound = std::mem::take(&mut self.outbound_messages);
        let post_send = std::mem::take(&mut self.post_send_input);
        let mut pending = Vec::new();
        if let Some(input) = self.pending_link_input.take() {
            pending.push(input);
        }
        pending.extend(std::mem::take(&mut self.queued_link_inputs));
        let deferred = std::mem::take(&mut self.deferred_messages);

        self.clear_pending_link_activation();
        self.awaiting_fallback = false;
        self.awaiting_promotion = false;
        self.server_owned_input = true;

        let mut events = Vec::new();
        for message in outbound {
            if let ClientMessage::InputEvents { events: input } = message {
                events.extend(input);
            }
        }
        for message in post_send {
            if let DeferredMessage::InputEvents { events: input, .. } = message {
                events.extend(input);
            }
        }
        for input in pending {
            if let LinkInput::Events { events: queued, .. } = input {
                events.extend(queued);
            }
        }
        for message in deferred {
            if let DeferredMessage::InputEvents { events: input, .. } = message {
                events.extend(input);
            }
        }
        self.queue_input_events_to_server(events);
        self.effects.clear();
    }

    fn queue_deferred_message_to_server(&mut self, message: DeferredMessage) -> bool {
        match message {
            DeferredMessage::InputEvents { events, .. } => {
                self.queue_input_events_to_server(events)
            }
            DeferredMessage::InputPixels { data, geometry, .. } => {
                drop((data, geometry));
                false
            }
        }
    }

    fn queue_input_events_to_server(&mut self, events: Vec<ClientInputEvent>) -> bool {
        let events = events
            .into_iter()
            .filter(|event| !matches!(event, ClientInputEvent::Mouse { .. }))
            .map(|event| event.to_raw_input_event())
            .collect();
        self.awaiting_fallback = false;
        self.awaiting_promotion = false;
        self.server_owned_input = true;
        let messages = self.route_input_inner(events, 0, false);
        let emitted = !messages.is_empty();
        self.outbound_messages.extend(messages);
        emitted
    }
    fn forward_local_releases(&self, releases: Vec<crate::input::TerminalKey>) -> bool {
        if releases.is_empty() {
            return true;
        }
        let Some(runtime) = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
        else {
            return false;
        };
        let mut delivered = true;
        for release in releases {
            delivered &= forward_local_event(
                runtime,
                crate::raw_input::RawInputEvent::Key(release),
                self.mouse_scroll_lines,
            );
        }
        delivered
    }

    fn release_local_input_for_focus_loss(&mut self) -> bool {
        let forward_focus = (self.local_selected && !self.server_owned_input)
            || self.omp_reply_navigation_presses.has_forwarded()
            || self.local_physical_presses.has_forwarded();
        let mut releases = self.local_physical_presses.release_for_focus_loss();
        releases.extend(self.omp_reply_navigation_presses.release_for_focus_loss());
        self.observe_pointer_cell(None);
        let releases_delivered = self.forward_local_releases(releases);
        let focus_delivered = !forward_focus
            || self
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .is_some_and(|runtime| {
                    forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Lost)
                });
        let delivered = releases_delivered && focus_delivered;
        if !delivered {
            self.begin_local_forward_fallback();
        }
        delivered
    }

    fn retire_local_input_owner(&mut self) -> bool {
        let mut releases = self.local_physical_presses.retire_owner();
        releases.extend(self.omp_reply_navigation_presses.retire_owner());
        self.forward_local_releases(releases)
    }

    fn stop_target(&mut self) {
        self.retire_local_input_owner();
        if let Some(mut target) = self.target.take() {
            target.stop();
        }
        if self.local_selected || self.server_owned_input || self.server_forwarded_overflow {
            self.force_repaint = true;
        }
        self.local_selected = false;
        self.server_owned_input = false;
        self.clear_pending_link_click();
        self.hovered_link_cells = None;
        self.needs_render = true;
    }
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

fn client_key_source(key: &crate::input::TerminalKey) -> ClientKeySource {
    if let Some(bytes) = key.vt_bytes() {
        return ClientKeySource::Vt {
            bytes: bytes.to_vec(),
        };
    }
    #[cfg(any(windows, test))]
    if let Some(record) = key.windows_record() {
        return ClientKeySource::WindowsConsole { record };
    }
    ClientKeySource::Synthesized
}

fn client_event_from_raw(event: &crate::raw_input::RawInputEvent) -> Option<ClientInputEvent> {
    match event {
        crate::raw_input::RawInputEvent::Key(key) => Some(ClientInputEvent::Key {
            code: ClientKeyCode::from_crossterm(key.code)?,
            modifiers: key.modifiers.bits(),
            kind: ClientKeyKind::from_crossterm(key.kind),
            repeat_count: key.repeat_count,
            generated_text: key.generated_text.clone(),
            source: client_key_source(key),
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
        MouseEventKind::Moved => runtime.encode_mouse_motion(kind, position, modifiers),
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
            runtime.encode_mouse_button(kind, position, modifiers)
        }
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => None,
    }
}

fn forward_local_wheel(
    runtime: &TerminalRuntime,
    kind: MouseEventKind,
    position: crate::input::mouse::Position,
    modifiers: crossterm::event::KeyModifiers,
    mouse_scroll_lines: usize,
) -> bool {
    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
            let Some(bytes) = runtime.encode_mouse_wheel(kind, position, modifiers) else {
                return scroll_local_wheel(runtime, kind, mouse_scroll_lines);
            };
            if runtime.try_send_bytes(Bytes::from(bytes)).is_ok() {
                runtime.scroll_reset();
                true
            } else {
                scroll_local_wheel(runtime, kind, mouse_scroll_lines)
            }
        }
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            runtime
                .encode_alternate_scroll(kind)
                .is_none_or(|bytes| runtime.try_send_bytes(Bytes::from(bytes)).is_ok())
        }
        Some(crate::pane::WheelRouting::HostScroll) | None => {
            scroll_local_wheel(runtime, kind, mouse_scroll_lines)
        }
    }
}

fn scroll_local_wheel(
    runtime: &TerminalRuntime,
    kind: MouseEventKind,
    mouse_scroll_lines: usize,
) -> bool {
    match kind {
        MouseEventKind::ScrollUp => runtime.scroll_up(mouse_scroll_lines.max(1)),
        MouseEventKind::ScrollDown => runtime.scroll_down(mouse_scroll_lines.max(1)),
        _ => return false,
    }
    true
}

#[derive(Clone, Copy)]
struct LocalPixelMouse {
    mouse: MouseEvent,
    position: crate::input::mouse::Position,
    cell: (u16, u16),
}

fn decode_pixel_mouse(data: &[u8]) -> Option<MouseEvent> {
    let report = crate::input::mouse::report_at_cell(data, 0, 0)?;
    crate::raw_input::parse_raw_input_bytes_sync(&report)
        .into_iter()
        .find_map(|event| match event {
            crate::raw_input::RawInputEvent::Mouse(mouse) => Some(mouse),
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
    let crate::input::mouse::Position::Pixels { x, y } = pointer.pane_position(
        Rect::new(0, 0, pointer.geometry.cols, pointer.geometry.rows),
        child_width_px,
        child_height_px,
    )?
    else {
        return None;
    };
    let (column, row) = local_pixel_cell(pointer, size)?;
    Some(crate::input::mouse::Position::Pixels {
        x: clamp_pixel_to_cell(x, column, cell_width_px)?,
        y: clamp_pixel_to_cell(y, row, cell_height_px)?,
    })
}

fn clamp_pixel_to_cell(pixel: u32, cell: u16, cell_extent: u32) -> Option<u32> {
    let start = u32::from(cell).checked_mul(cell_extent)?.checked_add(1)?;
    let end = u32::from(cell).checked_add(1)?.checked_mul(cell_extent)?;
    Some(pixel.clamp(start, end))
}

fn local_pixel_cell(
    pointer: crate::input::mouse::HostPixels,
    size: (u16, u16, u32, u32),
) -> Option<(u16, u16)> {
    let (cols, rows, _, _) = size;
    crate::input::mouse::HostGeometry::new(
        cols,
        rows,
        pointer.geometry.width_px,
        pointer.geometry.height_px,
    )?
    .cell(pointer.x, pointer.y)
}

fn decode_local_pixel_mouse(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    size: (u16, u16, u32, u32),
) -> Option<LocalPixelMouse> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let mouse = decode_pixel_mouse(data)?;
    let pointer = crate::input::mouse::HostPixels { x, y, geometry };
    let position = local_pixel_position(pointer, size)?;
    let cell = local_pixel_cell(pointer, size)?;
    Some(LocalPixelMouse {
        mouse,
        position,
        cell,
    })
}

fn forward_local_pixel_mouse(
    runtime: &TerminalRuntime,
    mouse: LocalPixelMouse,
    mouse_scroll_lines: usize,
) -> bool {
    if matches!(
        mouse.mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        return forward_local_wheel(
            runtime,
            mouse.mouse.kind,
            mouse.position,
            mouse.mouse.modifiers,
            mouse_scroll_lines,
        );
    }
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

fn try_navigate_local_omp_reply(
    runtime: &TerminalRuntime,
    event: &crate::raw_input::RawInputEvent,
) -> bool {
    let crate::raw_input::RawInputEvent::Key(key) = event else {
        return false;
    };
    runtime.try_navigate_omp_reply_repeated(true, key)
}

fn forward_local_event(
    runtime: &TerminalRuntime,
    event: crate::raw_input::RawInputEvent,
    mouse_scroll_lines: usize,
) -> bool {
    let bytes = match event {
        crate::raw_input::RawInputEvent::Key(key) => {
            if key.kind != crossterm::event::KeyEventKind::Release {
                runtime.scroll_reset();
            }
            Some(runtime.encode_terminal_key(key))
        }
        crate::raw_input::RawInputEvent::Text(text) => {
            runtime.scroll_reset();
            Some(text.into_string().into_bytes())
        }
        crate::raw_input::RawInputEvent::Paste(text) => {
            return runtime.try_send_paste(text).is_ok()
        }
        crate::raw_input::RawInputEvent::OuterFocusGained => {
            return forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Gained)
        }
        crate::raw_input::RawInputEvent::OuterFocusLost => {
            return forward_local_focus_event(runtime, crate::ghostty::FocusEvent::Lost)
        }
        crate::raw_input::RawInputEvent::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            return forward_local_wheel(
                runtime,
                mouse.kind,
                crate::input::mouse::Position::Cell {
                    column: mouse.column,
                    row: mouse.row,
                },
                mouse.modifiers,
                mouse_scroll_lines,
            );
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
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent};

    fn test_prefix() -> OmpRendererPrefix {
        OmpRendererPrefix {
            code: ClientKeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL.bits(),
        }
    }

    fn physical_j_with_scan(
        scan_code: u16,
        kind: KeyEventKind,
        key_down: bool,
    ) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty())
            .with_kind(kind)
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down,
                repeat_count: 1,
                virtual_key_code: 0x4a,
                virtual_scan_code: scan_code,
                unicode: if key_down { u16::from(b'j') } else { 0 },
                control_key_state: 0,
            })
    }

    fn physical_j(kind: KeyEventKind, key_down: bool) -> crate::input::TerminalKey {
        physical_j_with_scan(0x24, kind, key_down)
    }

    fn semantic_j(kind: KeyEventKind) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty()).with_kind(kind)
    }

    const OMP_REPLY_SCROLLBACK: &[u8] = b"\x1b]133;A;aid=omp-response-client-run:reply-1\x07reply one\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\r\n\
between one\r\n\
\x1b]133;A;aid=omp-response-client-run:reply-2\x07reply two\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\r\n\
between two\r\n\
\x1b]133;A;aid=omp-response-client-run:reply-3\x07reply three\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\r\n\
tail one\r\ntail two\r\ntail three\r\ntail four\r\n";
    const LOCAL_OMP_SCROLLBACK_LIMIT_BYTES: usize = 128 * 1024;
    const LOCAL_OMP_MOUSE_SCROLL_LINES: usize = 3;

    fn long_omp_reply_scrollback() -> Vec<u8> {
        let mut bytes = Vec::new();
        for reply in 1..=3 {
            bytes.extend_from_slice(
                format!(
                    "\x1b]133;A;aid=omp-response-client-long:reply-{reply}\x07reply {reply} finalized\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\r\n"
                )
                .as_bytes(),
            );
            for line in 0..160 {
                bytes.extend_from_slice(
                    format!("reply {reply} transcript line {line:03} retained output\r\n")
                        .as_bytes(),
                );
            }
        }
        for line in 0..160 {
            bytes.extend_from_slice(format!("tail transcript line {line:03}\r\n").as_bytes());
        }
        bytes
    }
    fn physical_option_up() -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT).with_windows_record(
            crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 0x26,
                virtual_scan_code: 0x48,
                unicode: 0,
                control_key_state: 0x0102,
            },
        )
    }

    fn physical_bare_up_release() -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::empty())
            .with_kind(KeyEventKind::Release)
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down: false,
                repeat_count: 1,
                virtual_key_code: 0x26,
                virtual_scan_code: 0x48,
                unicode: 0,
                control_key_state: 0x0100,
            })
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
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES,
            LOCAL_OMP_MOUSE_SCROLL_LINES,
        );
        renderer.latest_launch_id = 1;
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

    fn collect_sent_input_events(
        renderer: &mut ClientOmpRenderer,
        mut messages: Vec<ClientMessage>,
    ) -> Vec<ClientInputEvent> {
        let mut events = Vec::new();
        loop {
            for message in messages {
                if let ClientMessage::InputEvents { events: sent } = message {
                    events.extend(sent);
                }
            }
            messages = renderer.flush_post_send_input();
            if messages.is_empty() {
                return events;
            }
        }
    }

    fn deactivate_renderer_surface(renderer: &mut ClientOmpRenderer) {
        let target = renderer.target.as_ref().expect("local target");
        let target_app_client_id = target.target_app_client_id;
        let route = target.route.clone();
        let prefix = target.prefix.clone();
        let size = target.size;
        renderer.apply_target(
            1,
            target_app_client_id,
            Some(route),
            true,
            false,
            prefix,
            size,
            0,
        );
        let _ = renderer.next_frame(Instant::now(), (size.0, size.1));
        assert!(!renderer.local_selected);
        assert!(renderer.owns_input());
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
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES,
            LOCAL_OMP_MOUSE_SCROLL_LINES,
        );

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
        let mut renderer = ClientOmpRenderer::new(
            None,
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES,
            LOCAL_OMP_MOUSE_SCROLL_LINES,
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
    async fn local_omp_navigates_to_oldest_finalized_reply_through_long_scrollback() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            4,
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES,
            &[],
            8,
        );
        runtime.set_preserve_primary_scrollback(true);
        runtime.test_process_pty_bytes(&long_omp_reply_scrollback());
        runtime.test_process_pty_bytes(b"\x1b[3J\x1b[2J");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert_eq!(
            renderer.scrollback_limit_bytes,
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES
        );
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        for _ in 0..3 {
            assert!(renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    option_up.clone()
                )])
                .is_empty());
        }

        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply 1 finalized")));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_grouped_option_up_navigates_each_reply_without_forwarding() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let now = Instant::now();
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT).with_repeat_count(3),
            )])
            .is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply one")));
        assert!(renderer
            .next_frame(now + Duration::from_millis(1), (40, 4))
            .is_some());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_xterm_presses_recompute_navigation_and_forwarding() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let now = Instant::now();

        assert!(renderer
            .route_input(crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[A"))
            .is_empty());
        assert_eq!(
            input.try_recv().expect("plain Up forwards"),
            Bytes::from_static(b"\x1b[A")
        );
        assert!(renderer
            .route_input(crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[1;3A"))
            .is_empty());
        assert!(renderer
            .route_input(crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[1;4A"))
            .is_empty());
        assert_eq!(
            input.try_recv().expect("Shift-Option-Up forwards"),
            Bytes::from_static(b"\x1b[1;4A")
        );
        assert!(renderer
            .route_input(crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[1;3A"))
            .is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply three")));
        assert!(renderer
            .next_frame(now + Duration::from_millis(1), (40, 4))
            .is_some());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_later_grouped_option_up_repeat_clamps_without_forwarding() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up
                    .clone()
                    .with_repeat_count(u16::MAX)
                    .with_kind(KeyEventKind::Repeat),
            )])
            .is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply one")));
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_bare_windows_up_release_clears_consumed_reply_navigation_ownership() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = physical_option_up();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone(),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_bare_up_release(),
            )])
            .is_empty());
        assert!(!renderer.local_physical_presses.owns_input());
        assert!(input.try_recv().is_err());

        renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .test_process_pty_bytes(b"\x1b[?1049h\x1b[>3u");

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone(),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("fallthrough Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("fallthrough Option-Up release"),
            Bytes::from_static(b"\x1b[1;3:3A")
        );
    }

    #[tokio::test]
    async fn local_omp_consumed_repeat_stays_consumed_when_navigation_becomes_unavailable() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = physical_option_up();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone(),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);
        renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .test_process_pty_bytes(b"\x1b[?1049h\x1b[>3u");
        deactivate_renderer_surface(&mut renderer);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone().with_kind(KeyEventKind::Repeat),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);
        assert!(input.try_recv().is_err());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(!renderer.local_physical_presses.owns_input());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_activation_keeps_existing_server_key_lifecycle_on_server() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = physical_option_up();
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(option_up.clone())]);
        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(option_up.clone().with_kind(KeyEventKind::Repeat)),
            crate::raw_input::RawInputEvent::Key(physical_bare_up_release()),
        ]);

        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Repeat,
                    ..
                },
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_activation_keeps_existing_semantic_server_key_lifecycle_on_server() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(semantic_j(
            KeyEventKind::Press,
        ))]);

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(semantic_j(KeyEventKind::Repeat)),
            crate::raw_input::RawInputEvent::Key(semantic_j(KeyEventKind::Release)),
        ]);

        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Repeat,
                    ..
                },
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_promotion_routes_a_fresh_semantic_press_to_the_local_owner() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(semantic_j(
            KeyEventKind::Press,
        ))]);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(semantic_j(
                KeyEventKind::Press,
            ))])
            .is_empty());
        assert!(!input
            .try_recv()
            .expect("fresh semantic Press reaches local OMP")
            .is_empty());
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(renderer
            .local_physical_presses
            .owns_key(&semantic_j(KeyEventKind::Press)));
    }

    #[tokio::test]
    async fn local_omp_inactive_forwarded_press_stays_server_owned_after_reactivation() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_option_up(),
            )])
            .is_empty());
        deactivate_renderer_surface(&mut renderer);

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            physical_j(KeyEventKind::Press, true),
        )]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Press,
                    ..
                }])
        ));
        assert_eq!(renderer.server_forwarded_presses.len(), 1);

        renderer.local_selected = true;
        renderer
            .target
            .as_mut()
            .expect("local target")
            .surface_active = true;
        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Repeat, true)),
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
        ]);
        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Repeat,
                    ..
                },
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_omp_focus_loss_reaches_local_surface_and_server_key_owner() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(physical_j(
            KeyEventKind::Press,
            true,
        ))]);

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost]);

        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert_eq!(
            input.try_recv().expect("local focus loss"),
            Bytes::from_static(b"\x1b[O")
        );
        assert!(renderer.server_forwarded_presses.is_empty());
    }

    #[tokio::test]
    async fn local_omp_defers_focus_loss_behind_older_promotion_input() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 4);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.local_selected = false;
        renderer.awaiting_promotion = true;

        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Press, true)),
                crate::raw_input::RawInputEvent::OuterFocusLost,
            ])
            .is_empty());
        assert!(renderer.server_forwarded_presses.is_empty());

        renderer.resolve_promotion(false, 0);
        let messages = renderer.take_outbound_messages();
        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Press,
                    ..
                },
                ClientInputEvent::FocusLost,
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(!renderer.server_forwarded_overflow);
    }

    #[tokio::test]
    async fn deferred_focus_loss_releases_local_physical_owner_before_server_handoff() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert!(!input.try_recv().expect("ordinary key press").is_empty());
        renderer.awaiting_promotion = true;
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost])
            .is_empty());
        assert!(input.try_recv().is_err());

        renderer.resolve_promotion(false, 0);

        assert!(!input
            .try_recv()
            .expect("synthetic local key release")
            .is_empty());
        assert_eq!(
            input.try_recv().expect("local focus loss"),
            Bytes::from_static(b"\x1b[O")
        );
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert!(!renderer.local_physical_presses.owns_input());
    }

    #[tokio::test]
    async fn local_omp_inactive_forwarded_press_gets_focus_loss_teardown() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_option_up(),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("forwarded Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        deactivate_renderer_surface(&mut renderer);

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost]);

        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert_eq!(
            input.try_recv().expect("synthetic owner release"),
            Bytes::from_static(b"\x1b[1;3:3A")
        );
        assert_eq!(
            input.try_recv().expect("local owner focus loss"),
            Bytes::from_static(b"\x1b[O")
        );
        assert!(!renderer.local_physical_presses.owns_input());
    }

    #[test]
    fn server_forwarded_physical_key_state_is_bounded() {
        let mut renderer = ClientOmpRenderer::new(
            Some(test_omp_executable()),
            LOCAL_OMP_SCROLLBACK_LIMIT_BYTES,
            LOCAL_OMP_MOUSE_SCROLL_LINES,
        );
        let events = (1..=MAX_TRACKED_SERVER_PRESSES + 1)
            .map(|index| {
                crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty())
                        .with_windows_record(crate::input::WindowsKeyRecord {
                            key_down: true,
                            repeat_count: 1,
                            virtual_key_code: index as u16,
                            virtual_scan_code: index as u16,
                            unicode: u16::from(b'j'),
                            control_key_state: 0,
                        }),
                )
            })
            .collect::<Vec<_>>();

        renderer.observe_server_input(&events);

        assert_eq!(
            renderer.server_forwarded_presses.len(),
            MAX_TRACKED_SERVER_PRESSES
        );
        assert!(renderer.server_forwarded_overflow);
        assert!(!renderer.server_owned_input);
        assert!(renderer.owns_surface_input());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::OuterFocusLost]);
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(!renderer.server_forwarded_overflow);
        assert!(!renderer.owns_surface_input());
    }

    #[tokio::test]
    async fn local_omp_binding_loss_releases_owned_keys_and_tombstones_late_release() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert!(!input.try_recv().expect("ordinary key press").is_empty());
        deactivate_renderer_surface(&mut renderer);

        let target = renderer.target.as_ref().expect("local target");
        let target_app_client_id = target.target_app_client_id;
        let route = target.route.clone();
        let prefix = target.prefix.clone();
        let size = target.size;
        renderer.apply_target(
            1,
            target_app_client_id,
            Some(route),
            false,
            false,
            prefix,
            size,
            0,
        );

        assert!(!input
            .try_recv()
            .expect("synthetic owner release")
            .is_empty());
        assert!(renderer.owns_input());
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            assert!(renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                    kind, false,
                ))])
                .is_empty());
        }
        assert!(input.try_recv().is_err());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Release,
                false,
            ))])
            .is_empty());
        assert!(!renderer.local_physical_presses.owns_input());
    }

    #[tokio::test]
    async fn local_omp_forwarded_repeat_stays_forwarded_when_navigation_becomes_available() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = physical_option_up();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone(),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);
        assert_eq!(
            input.try_recv().expect("fallthrough Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        deactivate_renderer_surface(&mut renderer);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone().with_kind(KeyEventKind::Repeat),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);
        assert_eq!(
            input.try_recv().expect("fallthrough Option-Up repeat"),
            Bytes::from_static(b"\x1b[1;3:2A")
        );
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(!renderer.local_physical_presses.owns_input());
        assert_eq!(
            input.try_recv().expect("fallthrough Option-Up release"),
            Bytes::from_static(b"\x1b[1;3:3A")
        );
    }

    #[tokio::test]
    async fn local_omp_prefix_handoff_retains_consumed_reply_navigation_ownership() {
        let (runtime, _input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().expect("local target").size = (40, 4, 10, 20);
        let option_up = physical_option_up();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone(),
            )])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 1);

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            )),
            crate::raw_input::RawInputEvent::Key(option_up.with_kind(KeyEventKind::Repeat)),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { .. }]
        ));
        assert_eq!(renderer.local_physical_presses.len(), 1);
        assert!(!renderer.awaiting_promotion);
        assert!(renderer.flush_post_send_input().is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply two")));
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_bare_up_release(),
            )])
            .is_empty());
        assert!(!renderer.local_physical_presses.owns_input());
    }

    #[tokio::test]
    async fn local_omp_prefix_handoff_returns_forwarded_release_to_local_pty() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_option_up(),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("forwarded Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        )]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { .. }]
        ));

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                physical_bare_up_release(),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("forwarded Option-Up release"),
            Bytes::from_static(b"\x1b[1;1:3A")
        );
        assert!(renderer.omp_reply_navigation_presses.is_empty());
    }

    #[tokio::test]
    async fn local_omp_prefix_handoff_keeps_ordinary_physical_key_lifecycle_local() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert!(!input.try_recv().expect("ordinary key press").is_empty());

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        )]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { .. }]
        ));

        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Repeat, true)),
                crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
            ])
            .is_empty());
        assert!(!input.try_recv().expect("ordinary key repeat").is_empty());
        assert!(!input.try_recv().expect("ordinary key release").is_empty());
        assert!(renderer.local_physical_presses.len() == 0);
    }

    #[tokio::test]
    async fn local_omp_prefix_handoff_keeps_ordinary_semantic_key_lifecycle_local() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(semantic_j(
                KeyEventKind::Press,
            ))])
            .is_empty());
        assert!(!input.try_recv().expect("semantic key press").is_empty());

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        )]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { .. }]
        ));

        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(semantic_j(KeyEventKind::Repeat)),
                crate::raw_input::RawInputEvent::Key(semantic_j(KeyEventKind::Release)),
            ])
            .is_empty());
        assert!(!input.try_recv().expect("semantic key repeat").is_empty());
        assert!(!input.try_recv().expect("semantic key release").is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 0);
    }

    #[tokio::test]
    async fn local_omp_prefix_handoff_routes_a_fresh_semantic_press_to_server() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(semantic_j(
                KeyEventKind::Press,
            ))])
            .is_empty());
        assert!(!input.try_recv().expect("first semantic Press").is_empty());
        let prefix_messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        )]);
        assert!(matches!(
            prefix_messages.as_slice(),
            [ClientMessage::InputEvents { .. }]
        ));

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Key(
            semantic_j(KeyEventKind::Press),
        )]);
        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [ClientInputEvent::Key {
                kind: crate::protocol::ClientKeyKind::Press,
                ..
            }]
        ));
        assert!(input.try_recv().is_err());
        assert!(!renderer
            .local_physical_presses
            .owns_key(&semantic_j(KeyEventKind::Press)));
        assert!(renderer
            .server_forwarded_presses
            .contains(&semantic_j(KeyEventKind::Press).identity()));
    }

    #[tokio::test]
    async fn local_omp_direct_input_resets_reply_selection_but_option_release_does_not() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        assert!(local_runtime
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply three")));

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone().with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        assert!(local_runtime
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply three")));
        assert!(input.try_recv().is_err());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.needs_render);
        assert!(renderer.next_frame(Instant::now(), (40, 4)).is_some());
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        assert_eq!(
            local_runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input.try_recv().expect("ordinary input forwarded"),
            Bytes::from("x")
        );

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(option_up)])
            .is_empty());
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        assert!(local_runtime
            .visible_text()
            .lines()
            .next()
            .is_some_and(|line| line.trim_end().starts_with("reply three")));
    }

    #[tokio::test]
    async fn local_omp_text_and_bracketed_paste_reset_reply_scrollback() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .and_then(TerminalRuntime::scroll_metrics)
            .is_some_and(|metrics| metrics.offset_from_bottom > 0));

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Text(
                crate::input::TextCommit::new("入力"),
            )])
            .is_empty());
        assert!(renderer.needs_render);
        assert!(renderer.next_frame(Instant::now(), (40, 4)).is_some());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input.try_recv().expect("forwarded IME text").as_ref(),
            "入力".as_bytes()
        );

        renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime")
            .test_process_pty_bytes(b"\x1b[?2004h");
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(option_up)])
            .is_empty());
        assert!(renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .and_then(TerminalRuntime::scroll_metrics)
            .is_some_and(|metrics| metrics.offset_from_bottom > 0));

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Paste(
                "pasted".into()
            )])
            .is_empty());
        assert!(renderer.needs_render);
        assert!(renderer.next_frame(Instant::now(), (40, 4)).is_some());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input.try_recv().expect("forwarded bracketed paste"),
            Bytes::from_static(b"\x1b[200~pasted\x1b[201~")
        );
    }

    #[tokio::test]
    async fn local_omp_forwards_unconsumed_option_release_with_kitty_event_type() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3uone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("unconsumed Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );

        let offset = {
            let local_runtime = renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .expect("local runtime");
            local_runtime.scroll_up(1);
            let offset = local_runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom;
            assert!(offset > 0);
            offset
        };

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone().with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert_eq!(
            input
                .try_recv()
                .expect("unconsumed Option-Up release with Kitty event type"),
            Bytes::from_static(b"\x1b[1;3:3A")
        );
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        assert_eq!(
            local_runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            offset
        );
        local_runtime.test_process_pty_bytes(b"\x1b[?1004h");
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(option_up)])
            .is_empty());
        assert_eq!(
            input
                .try_recv()
                .expect("pre-focus semantic Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost])
            .is_empty());
        assert_eq!(
            input
                .try_recv()
                .expect("synthetic semantic Option-Up release"),
            Bytes::from_static(b"\x1b[1;3:3A")
        );
        assert_eq!(
            input.try_recv().expect("native semantic focus loss"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn local_semantic_owner_release_write_failure_closes_the_lifecycle() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1049h\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("semantic Option-Up press"),
            Bytes::from_static(b"\x1b[1;3:1A")
        );
        drop(input);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(renderer.omp_reply_navigation_presses.len(), 0);
    }

    #[tokio::test]
    async fn local_physical_owner_release_write_failure_closes_the_lifecycle() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("physical j press"),
            Bytes::from_static(b"j")
        );
        drop(input);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Release,
                false,
            ))])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 0);
    }

    #[tokio::test]
    async fn local_focus_release_write_failure_fails_target_and_forwards_focus_to_server() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("pre-focus physical press"),
            Bytes::from_static(b"j")
        );
        drop(input);

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert!(renderer.awaiting_fallback);
        assert!(renderer.target.as_ref().is_some_and(|target| target.failed));
        assert_eq!(renderer.local_physical_presses.len(), 0);
    }

    #[tokio::test]
    async fn local_focus_write_failure_without_held_keys_falls_back_to_server() {
        let (runtime, input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        drop(input);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert!(renderer.awaiting_fallback);
        assert!(renderer.target.as_ref().is_some_and(|target| target.failed));
    }

    #[tokio::test]
    async fn local_physical_capacity_releases_oldest_press_and_keeps_input_live() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 1024);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        for scan_code in 1..=256 {
            assert!(renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    physical_j_with_scan(scan_code, KeyEventKind::Press, true),
                )])
                .is_empty());
        }
        for _ in 0..256 {
            assert_eq!(
                input.try_recv().expect("forwarded physical key press"),
                Bytes::from_static(b"j")
            );
        }
        let before = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .and_then(TerminalRuntime::scroll_metrics)
            .expect("local scroll metrics")
            .offset_from_bottom;
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT)
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 0x26,
                virtual_scan_code: 257,
                unicode: 0,
                control_key_state: 0x0102,
            });

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(option_up)])
            .is_empty());
        assert_ne!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("local scroll metrics")
                .offset_from_bottom,
            before
        );
        assert!(!input
            .try_recv()
            .expect("synthetic release for the oldest press")
            .is_empty());
        assert!(input.try_recv().is_err());
        assert_eq!(renderer.local_physical_presses.len(), 256);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Text(
                crate::input::TextCommit::new("still-live"),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("input after capacity eviction"),
            Bytes::from_static(b"still-live")
        );

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost])
            .is_empty());
        assert_eq!(renderer.local_physical_presses.len(), 0);
    }

    #[tokio::test]
    async fn local_omp_focus_loss_suppresses_late_consumed_option_release() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        runtime.test_process_pty_bytes(OMP_REPLY_SCROLLBACK);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let option_up = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.clone()
            )])
            .is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusLost])
            .is_empty());
        let local_runtime = renderer
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .expect("local runtime");
        local_runtime.test_process_pty_bytes(b"\x1b[?1049h\x1b[>3u");

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                option_up.with_kind(KeyEventKind::Release),
            )])
            .is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn prefix_command_batch_sends_prefix_before_the_command() {
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
        let sent = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            sent.as_slice(),
            [
                ClientInputEvent::Key {
                    code: ClientKeyCode::Char('b'),
                    ..
                },
                ClientInputEvent::Key {
                    code: ClientKeyCode::Char('n'),
                    ..
                }
            ]
        ));
        assert!(renderer.server_owned_input);
        assert!(renderer.local_selected);
    }

    #[tokio::test]
    async fn deferred_replay_preserves_two_batch_fifo_across_a_send_barrier() {
        for local_active in [Some(true), Some(false), None] {
            let (runtime, mut input) =
                TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, &[], 8);
            runtime.test_process_pty_bytes(b"\x1b[>3u");
            let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
            let local_key = physical_j_with_scan(0x25, KeyEventKind::Press, true);
            assert!(renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    local_key.clone(),
                )])
                .is_empty());
            assert_eq!(
                input.try_recv().expect("initial local press"),
                Bytes::from_static(b"j")
            );
            let prefix = client_event_from_raw(&crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            ))
            .expect("prefix protocol event");
            let repeat = client_event_from_raw(&crate::raw_input::RawInputEvent::Key(
                local_key.with_kind(KeyEventKind::Repeat),
            ))
            .expect("repeat protocol event");
            let later = ClientInputEvent::TextCommit("later".to_owned());
            renderer.awaiting_promotion = local_active.is_some();
            renderer.awaiting_fallback = local_active.is_none();
            renderer.deferred_messages = vec![
                DeferredMessage::InputEvents {
                    events: vec![prefix.clone(), repeat],
                    generation: 0,
                },
                DeferredMessage::InputEvents {
                    events: vec![later.clone()],
                    generation: 0,
                },
            ];

            match local_active {
                Some(local_active) => renderer.resolve_promotion(local_active, 0),
                None => renderer.release_deferred_messages(),
            }
            let prefix_messages = renderer.take_outbound_messages();
            assert!(matches!(
                prefix_messages.as_slice(),
                [ClientMessage::InputEvents { events }] if events == &[prefix]
            ));
            assert!(input.try_recv().is_err());

            let later_messages = renderer.flush_post_send_input();
            assert!(matches!(
                later_messages.as_slice(),
                [ClientMessage::InputEvents { events }] if events == &[later]
            ));
            assert!(!input
                .try_recv()
                .expect("local repeat before later server batch")
                .is_empty());
            assert!(renderer.flush_post_send_input().is_empty());
        }
    }

    #[tokio::test]
    async fn deferred_ordinary_input_precedes_a_later_server_owned_key_lifecycle() {
        for local_active in [Some(true), Some(false), None] {
            let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
            let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
            renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))]);
            renderer.awaiting_promotion = local_active.is_some();
            renderer.awaiting_fallback = local_active.is_none();

            assert!(renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Text(
                    crate::input::TextCommit::new("older"),
                )])
                .is_empty());
            assert!(renderer
                .route_input(vec![
                    crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Repeat, true,)),
                    crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false,)),
                ])
                .is_empty());
            assert!(input.try_recv().is_err());

            match local_active {
                Some(local_active) => renderer.resolve_promotion(local_active, 0),
                None => renderer.release_deferred_messages(),
            }
            let messages = renderer.take_outbound_messages();
            let sent = collect_sent_input_events(&mut renderer, messages);
            if local_active == Some(true) {
                assert_eq!(
                    input.try_recv().expect("older local text"),
                    Bytes::from_static(b"older")
                );
                assert!(matches!(
                    sent.as_slice(),
                    [
                        ClientInputEvent::Key {
                            kind: crate::protocol::ClientKeyKind::Repeat,
                            ..
                        },
                        ClientInputEvent::Key {
                            kind: crate::protocol::ClientKeyKind::Release,
                            ..
                        }
                    ]
                ));
            } else {
                assert!(matches!(
                    sent.as_slice(),
                    [
                        ClientInputEvent::TextCommit(text),
                        ClientInputEvent::Key {
                            kind: crate::protocol::ClientKeyKind::Repeat,
                            ..
                        },
                        ClientInputEvent::Key {
                            kind: crate::protocol::ClientKeyKind::Release,
                            ..
                        }
                    ] if text == "older"
                ));
                assert!(input.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn server_release_is_sent_before_a_later_new_local_press() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(physical_j(
            KeyEventKind::Press,
            true,
        ))]);

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
            crate::raw_input::RawInputEvent::Key(physical_j_with_scan(
                0x25,
                KeyEventKind::Press,
                true,
            )),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }])
        ));
        assert!(input.try_recv().is_err());
        assert!(renderer.flush_post_send_input().is_empty());
        assert_eq!(
            input.try_recv().expect("post-send local press"),
            Bytes::from_static(b"j")
        );
    }

    #[tokio::test]
    async fn prefix_send_precedes_local_repeat_release_and_server_focus() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let local_key = physical_j_with_scan(0x25, KeyEventKind::Press, true);
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                local_key.clone(),
            )])
            .is_empty());
        assert_eq!(
            input.try_recv().expect("initial local press"),
            Bytes::from_static(b"j")
        );

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            )),
            crate::raw_input::RawInputEvent::Key(local_key.clone().with_kind(KeyEventKind::Repeat)),
            crate::raw_input::RawInputEvent::Key(local_key.with_kind(KeyEventKind::Release)),
            crate::raw_input::RawInputEvent::OuterFocusLost,
        ]);

        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }] if events.len() == 1
        ));
        assert!(input.try_recv().is_err());

        let focus = renderer.flush_post_send_input();
        assert!(matches!(
            focus.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusLost])
        ));
        assert!(!input.try_recv().expect("post-send local repeat").is_empty());
        assert!(!input
            .try_recv()
            .expect("post-send local release")
            .is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn server_release_is_sent_before_a_later_local_focus_loss() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, &[], 8);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_server_input(&[crate::raw_input::RawInputEvent::Key(physical_j(
            KeyEventKind::Press,
            true,
        ))]);

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
            crate::raw_input::RawInputEvent::OuterFocusLost,
        ]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }])
        ));
        assert!(input.try_recv().is_err());

        assert!(renderer.flush_post_send_input().is_empty());
        assert_eq!(
            input.try_recv().expect("post-send local focus loss"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn rejected_native_link_replays_the_buffered_mouse_gesture_locally() {
        let url = "artifact://native";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let messages = renderer.route_input(vec![
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Drag(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::ActivateOmpLink { url: produced, .. }] if produced == url
        ));
        assert!(!renderer.pending_link_click);

        renderer.resolve_link_activation(1, 1, false, 0);
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1M");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<32;1;1M");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1m");
    }
    #[tokio::test]
    async fn queued_native_links_resolve_in_gesture_order() {
        let url = "artifact://native-queued";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let messages = renderer.route_input(vec![
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::ActivateOmpLink {
                request_id: 1,
                url: produced,
                ..
            }] if produced == url
        ));

        renderer.resolve_link_activation(1, 1, false, 0);
        let queued = renderer.take_outbound_messages();
        assert!(matches!(
            queued.as_slice(),
            [ClientMessage::ActivateOmpLink {
                request_id: 2,
                url: produced,
                ..
            }] if produced == url
        ));
        assert!(collect_sent_input_events(&mut renderer, queued).is_empty());

        renderer.resolve_link_activation(1, 2, false, 0);
        assert!(renderer.take_outbound_messages().is_empty());
        for expected in [
            b"\x1b[<0;1;1M".as_slice(),
            b"\x1b[<0;1;1m".as_slice(),
            b"\x1b[<0;1;1M".as_slice(),
            b"\x1b[<0;1;1m".as_slice(),
        ] {
            assert_eq!(input.try_recv().unwrap().as_ref(), expected);
        }
    }

    #[tokio::test]
    async fn input_generation_change_cancels_stale_native_link_queue() {
        let url = "artifact://native-stale";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let messages = renderer.route_input_at_generation(
            vec![
                mouse(MouseEventKind::Down(MouseButton::Left)),
                mouse(MouseEventKind::Up(MouseButton::Left)),
                crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Char('x'),
                    KeyModifiers::empty(),
                )),
            ],
            0,
        );
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::ActivateOmpLink { request_id: 1, .. }]
        ));

        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        renderer.resize((80, 1, 10, 20), Some(geometry), 1);
        renderer.resolve_link_activation(1, 1, false, 1);

        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert!(input.try_recv().is_err());
    }
    #[tokio::test]
    async fn accepted_native_link_drops_stale_generation_mouse_queue() {
        let url = "artifact://native-accepted-stale";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let messages = renderer.route_input_at_generation(
            vec![
                mouse(MouseEventKind::Down(MouseButton::Left)),
                mouse(MouseEventKind::Up(MouseButton::Left)),
                mouse(MouseEventKind::Down(MouseButton::Left)),
            ],
            0,
        );
        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::ActivateOmpLink { request_id: 1, .. }]
        ));

        renderer.resolve_link_activation(1, 1, true, 1);

        assert!(renderer.take_outbound_messages().is_empty());
        assert!(renderer.queued_link_inputs.is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn target_withdrawal_flushes_current_non_pointer_input_in_order() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let pointer = ClientInputEvent::Mouse {
            kind: ClientMouseKind::Moved,
            column: 1,
            row: 1,
            modifiers: 0,
        };
        let key = client_event_from_raw(&crate::raw_input::RawInputEvent::Key(physical_j(
            KeyEventKind::Press,
            true,
        )))
        .expect("physical key protocol event");
        let paste = ClientInputEvent::Paste {
            text: "queued".to_owned(),
        };
        let text = ClientInputEvent::TextCommit("deferred".to_owned());
        let already_queued = ClientInputEvent::TextCommit("outbound".to_owned());
        renderer.outbound_messages.extend([
            ClientMessage::InputEvents {
                events: vec![already_queued.clone()],
            },
            ClientMessage::OmpRendererReady { launch_id: 1 },
        ]);

        renderer.pending_link_request_id = Some(1);
        renderer.pending_link_click = true;
        renderer.pending_link_input = Some(LinkInput::Events {
            events: vec![pointer.clone(), key.clone()],
            generation: 7,
        });
        renderer.queued_link_inputs.push(LinkInput::Events {
            events: vec![paste.clone()],
            generation: 7,
        });
        renderer
            .deferred_messages
            .push(DeferredMessage::InputEvents {
                events: vec![pointer, text.clone()],
                generation: 7,
            });
        renderer
            .deferred_messages
            .push(DeferredMessage::InputPixels {
                data: vec![1, 2, 3],
                geometry: crate::input::mouse::HostGeometry::new(80, 24, 800, 480)
                    .expect("valid geometry"),
                generation: 7,
            });

        renderer.apply_target(1, 2, None, false, false, test_prefix(), (80, 24, 0, 0), 7);

        let flushed_messages = renderer.take_outbound_messages();
        assert!(flushed_messages
            .iter()
            .all(|message| matches!(message, ClientMessage::InputEvents { .. })));
        let flushed = collect_sent_input_events(&mut renderer, flushed_messages);
        assert_eq!(flushed, vec![already_queued, key, paste, text]);
        assert!(renderer.pending_link_input.is_none());
        assert!(renderer.queued_link_inputs.is_empty());
        assert!(renderer.deferred_messages.is_empty());
    }

    #[tokio::test]
    async fn same_route_unbind_flushes_pending_input_and_tracks_server_key_lifecycle() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let press = physical_j(KeyEventKind::Press, true);
        let protocol_press =
            client_event_from_raw(&crate::raw_input::RawInputEvent::Key(press.clone()))
                .expect("physical key protocol event");
        let paste = ClientInputEvent::Paste {
            text: "queued".to_owned(),
        };
        let text = ClientInputEvent::TextCommit("deferred".to_owned());
        renderer.pending_link_request_id = Some(1);
        renderer.pending_link_input = Some(LinkInput::Events {
            events: vec![protocol_press.clone()],
            generation: 4,
        });
        renderer.queued_link_inputs.push(LinkInput::Events {
            events: vec![paste.clone()],
            generation: 4,
        });
        renderer
            .deferred_messages
            .push(DeferredMessage::InputEvents {
                events: vec![text.clone()],
                generation: 4,
            });

        let target = renderer.target.as_ref().expect("local target");
        let target_app_client_id = target.target_app_client_id;
        let route = target.route.clone();
        let prefix = target.prefix.clone();
        let size = target.size;
        renderer.apply_target(
            1,
            target_app_client_id,
            Some(route),
            false,
            false,
            prefix,
            size,
            4,
        );

        let messages = renderer.take_outbound_messages();
        let flushed = collect_sent_input_events(&mut renderer, messages);
        assert_eq!(flushed, vec![protocol_press, paste, text]);
        assert_eq!(renderer.server_forwarded_presses.len(), 1);
        assert!(renderer.pending_link_input.is_none());
        assert!(renderer.queued_link_inputs.is_empty());
        assert!(renderer.deferred_messages.is_empty());

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Repeat, true)),
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
        ]);
        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Repeat,
                    ..
                },
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                },
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn native_link_queue_overflow_enters_fallback_without_dropping_input() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.pending_link_request_id = Some(1);
        renderer.pending_link_click = true;
        renderer.pending_link_input = Some(LinkInput::Events {
            events: vec![ClientInputEvent::FocusGained],
            generation: 0,
        });
        renderer.queued_link_inputs = (0..MAX_QUEUED_LINK_INPUTS)
            .map(|_| LinkInput::Events {
                events: vec![ClientInputEvent::FocusLost],
                generation: 0,
            })
            .collect();

        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
                )],
                0,
            )
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.pending_link_request_id.is_none());
        assert!(renderer.pending_link_input.is_none());
        assert!(renderer.queued_link_inputs.is_empty());
        assert_eq!(renderer.deferred_messages.len(), MAX_QUEUED_LINK_INPUTS + 2);
    }

    #[tokio::test]
    async fn native_link_gesture_overflow_enters_fallback_without_dropping_release() {
        let url = "artifact://native-gesture-overflow";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.observe_pointer_cell(Some((0, 0)));
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };
        assert!(matches!(
            renderer.route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))]).as_slice(),
            [ClientMessage::ActivateOmpLink { url: produced, .. }] if produced == url
        ));

        for _ in 0..=MAX_QUEUED_LINK_INPUTS {
            renderer.route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Left))]);
        }
        renderer.route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))]);

        assert!(renderer.awaiting_fallback);
        assert!(renderer.pending_link_input.is_none());
        assert!(renderer.deferred_messages.len() >= 2);
    }

    #[tokio::test]
    async fn pending_input_item_budget_falls_back_without_dropping_ordinary_input() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let queued = ClientInputEvent::TextCommit("queued".to_owned());
        renderer.awaiting_promotion = true;
        renderer
            .deferred_messages
            .push(DeferredMessage::InputEvents {
                events: vec![queued.clone(); MAX_PENDING_INPUT_EVENTS],
                generation: 0,
            });

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Text(
            crate::input::TextCommit::new("current"),
        )]);
        let sent = collect_sent_input_events(&mut renderer, messages);
        assert_eq!(sent.len(), MAX_PENDING_INPUT_EVENTS + 1);
        assert_eq!(sent.first(), Some(&queued));
        assert_eq!(
            sent.last(),
            Some(&ClientInputEvent::TextCommit("current".to_owned()))
        );
        assert_eq!(renderer.pending_input_usage(), (0, 0));
        assert!(renderer.target.as_ref().is_some_and(|target| target.failed));
    }

    #[tokio::test]
    async fn pending_input_payload_budget_falls_back_without_dropping_ordinary_input() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let queued = ClientInputEvent::Paste {
            text: "x".repeat(MAX_PENDING_INPUT_PAYLOAD_BYTES),
        };
        renderer.pending_link_request_id = Some(1);
        renderer.pending_link_input = Some(LinkInput::Events {
            events: vec![queued.clone()],
            generation: 0,
        });

        let messages = renderer.route_input(vec![crate::raw_input::RawInputEvent::Text(
            crate::input::TextCommit::new("current"),
        )]);
        let sent = collect_sent_input_events(&mut renderer, messages);
        assert_eq!(
            sent,
            vec![queued, ClientInputEvent::TextCommit("current".to_owned())]
        );
        assert_eq!(renderer.pending_input_usage(), (0, 0));
        assert!(renderer.target.as_ref().is_some_and(|target| target.failed));
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
    async fn local_omp_cell_wheel_host_scrolls_and_requests_frame() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        let transcript = (0..20)
            .map(|line| format!("line {line:02}\r\n"))
            .collect::<String>();
        runtime.test_process_pty_bytes(transcript.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let now = Instant::now();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("scroll metrics")
                .offset_from_bottom,
            LOCAL_OMP_MOUSE_SCROLL_LINES
        );
        assert!(renderer
            .next_frame(now + Duration::from_millis(1), (40, 4))
            .is_some());
        assert!(input.try_recv().is_err());
    }
    #[tokio::test]
    async fn local_omp_mouse_report_wheel_send_failure_host_scrolls_and_requests_frame() {
        let (runtime, input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 4, 16 * 1024, &[], 8);
        let transcript = (0..20)
            .map(|line| format!("line {line:02}\r\n"))
            .collect::<String>();
        runtime.test_process_pty_bytes(transcript.as_bytes());
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            runtime.wheel_routing(),
            Some(crate::pane::WheelRouting::MouseReport)
        );
        drop(input);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let now = Instant::now();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("scroll metrics")
                .offset_from_bottom,
            LOCAL_OMP_MOUSE_SCROLL_LINES
        );
        assert!(renderer
            .next_frame(now + Duration::from_millis(1), (40, 4))
            .is_some());
    }

    #[tokio::test]
    async fn local_omp_pixel_wheel_host_scrolls_and_requests_frame() {
        let (runtime, mut input) =
            TerminalRuntime::test_with_channel_and_scrollback_bytes(40, 12, 16 * 1024, &[], 8);
        runtime.resize(12, 40, 10, 20);
        let transcript = (0..40)
            .map(|line| format!("line {line:02}\r\n"))
            .collect::<String>();
        runtime.test_process_pty_bytes(transcript.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().expect("local target").size = (40, 12, 10, 20);
        let now = Instant::now();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();

        assert!(renderer
            .route_pixel_input(b"\x1b[<64;321;241M".to_vec(), geometry, 0)
            .is_none());
        assert_eq!(
            renderer
                .target
                .as_ref()
                .and_then(|target| target.runtime.as_ref())
                .and_then(TerminalRuntime::scroll_metrics)
                .expect("scroll metrics")
                .offset_from_bottom,
            LOCAL_OMP_MOUSE_SCROLL_LINES
        );
        assert!(renderer
            .next_frame(now + Duration::from_millis(1), (40, 12))
            .is_some());
        assert!(input.try_recv().is_err());
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
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        assert_eq!(geometry.cell(21, 1), Some((2, 0)));

        let message = renderer
            .route_pixel_input(b"\x1b[<0;21;1M".to_vec(), geometry, 0)
            .expect("pixel link activation");
        assert!(matches!(
            message,
            ClientMessage::ActivateOmpLink { url: produced, .. } if produced == url
        ));
        assert_eq!(renderer.pointer_cell, Some((1, 0)));
        assert!(renderer.resolved_link_at(2, 0).is_none());
        assert_eq!(renderer.native_link_active(), Some(true));
        let frame = renderer
            .next_frame(Instant::now(), (40, 1))
            .expect("pixel hover repaint")
            .frame;
        assert_ne!(
            frame.cells[1].modifier & ratatui::style::Modifier::UNDERLINED.bits(),
            0
        );
    }

    #[test]
    fn fractional_pixel_mapping_keeps_forwarded_position_in_the_hit_cell() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 801, 20).unwrap();
        let pointer = crate::input::mouse::HostPixels {
            x: 11,
            y: 1,
            geometry,
        };
        let size = (80, 1, 10, 20);

        assert_eq!(local_pixel_cell(pointer, size), Some((1, 0)));
        assert_eq!(
            local_pixel_position(pointer, size),
            Some(crate::input::mouse::Position::Pixels { x: 11, y: 1 })
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

        assert!(decode_pixel_mouse(&report).is_none());
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
            .try_send(AppEvent::PaneDied {
                pane_id,
                child_pid: None,
            })
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

            let request_id = match renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))])
                .as_slice()
            {
                [ClientMessage::ActivateOmpLink {
                    launch_id: 1,
                    request_id,
                    url,
                    ..
                }] if url == expected_url => *request_id,
                _ => panic!("expected native link activation request"),
            };
            assert!(renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))])
                .is_empty());
            renderer.resolve_link_activation(1, request_id, false, 0);
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
            [ClientMessage::ActivateOmpLink {
                launch_id: 1,
                url,
                ..
            }] if url == "file:///tmp/report.md?line=7"
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
        renderer.resolve_link_activation(1, 1, true, 0);

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
        renderer.server_owned_input = true;
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::InputPixels { .. })
        ));
        renderer.server_owned_input = false;
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ActivateOmpLink {
                launch_id: 1,
                url,
                ..
            }) if url == "file:///tmp/report.md?line=7"
        ));
        renderer.apply_target(2, 2, None, false, false, test_prefix(), (80, 24, 10, 20), 0);
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1m".to_vec(), geometry, 0),
            Some(ClientMessage::InputPixels { .. })
        ));
        assert!(
            input.try_recv().is_err(),
            "pixel link click must not reach the local OMP guest after target replacement"
        );
    }

    #[tokio::test]
    async fn pixel_link_gesture_absorbs_out_of_bounds_drag_and_release() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b]8;;file:///tmp/report.md\x1b\\report\x1b]8;;\x1b\\",
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 24, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();

        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ActivateOmpLink { .. })
        ));
        assert!(renderer.pending_link_click);
        assert!(renderer
            .route_pixel_input(b"\x1b[<32;901;1M".to_vec(), geometry, 0)
            .is_none());
        assert!(renderer.pending_link_click);
        assert!(renderer
            .route_pixel_input(b"\x1b[<0;901;1m".to_vec(), geometry, 0)
            .is_none());
        assert!(!renderer.pending_link_click);
        assert!(
            input.try_recv().is_err(),
            "link gesture must not reach the local OMP guest"
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
            .route_input(vec![crate::raw_input::RawInputEvent::Key(physical_j(
                KeyEventKind::Press,
                true,
            ))])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 0, 0), 0);
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Press,
                    ..
                }])
        ));
        assert_eq!(renderer.server_forwarded_presses.len(), 1);
        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Repeat, true)),
            crate::raw_input::RawInputEvent::Key(physical_j(KeyEventKind::Release, false)),
        ]);
        let events = collect_sent_input_events(&mut renderer, messages);
        assert!(matches!(
            events.as_slice(),
            [
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Repeat,
                    ..
                },
                ClientInputEvent::Key {
                    kind: crate::protocol::ClientKeyKind::Release,
                    ..
                }
            ]
        ));
        assert!(renderer.server_forwarded_presses.is_empty());
    }

    #[tokio::test]
    async fn native_child_death_releases_surface_after_server_fallback_confirmation() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, events, pane_id) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();

        events
            .try_send(AppEvent::PaneDied {
                pane_id,
                child_pid: None,
            })
            .unwrap();
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
            [ClientMessage::InputEvents { events: input }] if input.len() == 1
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
    async fn failed_pixel_write_drops_pointer_input_during_server_fallback() {
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
        assert!(renderer.take_outbound_messages().is_empty());
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
    async fn fallback_preserves_keys_co_batched_with_stale_mouse_input() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.target.as_ref().unwrap().route.clone();
        assert!(renderer
            .route_input_at_generation(
                vec![
                    crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                        KeyCode::Char('x'),
                        KeyModifiers::empty(),
                    )),
                    crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Moved,
                        column: 1,
                        row: 0,
                        modifiers: KeyModifiers::empty(),
                    }),
                ],
                0,
            )
            .is_empty());
        assert!(renderer.awaiting_fallback);

        renderer.apply_target(1, 2, Some(route), false, false, prefix, (80, 24, 10, 20), 1);

        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key { .. }])
        ));
    }

    #[tokio::test]
    async fn replacement_launch_forwards_deferred_non_pointer_input() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(2, 2, None, false, false, test_prefix(), (80, 24, 0, 0), 1);
        assert!(!renderer.awaiting_fallback);
        assert!(renderer.deferred_messages.is_empty());
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::InputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key { .. }])
        ));
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
