use std::collections::{HashMap, HashSet};
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
    ClientMouseButton, ClientMouseKind, FrameData, OmpRendererCapabilities, OmpRendererFrame,
    OmpRendererPane, OmpRendererPrefix, OmpRendererRoute, RenderEncoding, MAX_LINK_URL_LENGTH,
};
use crate::render_signal::RenderSignal;
use crate::terminal::TerminalRuntime;

pub(super) const OMP_RENDERER_LAUNCH_ID_ENV: &str = "HERDR_OMP_RENDERER_LAUNCH_ID";

const BIND_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_DAMAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUEUED_LINK_INPUTS: usize = 256;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromotionBarrier {
    AuthorityAfter(u64),
    Frame(u64),
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
}

#[derive(Clone)]
struct TargetOffer {
    launch_id: u64,
    authority_revision: u64,
    target_app_client_id: u64,
    route: OmpRendererRoute,
    bound: bool,
    surface_active: bool,
    prefix: OmpRendererPrefix,
}

struct LocalTarget {
    launch_id: u64,
    prefix: OmpRendererPrefix,
    pane: OmpRendererPane,
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
        pane: OmpRendererPane,
        prefix: OmpRendererPrefix,
        size: (u16, u16, u32, u32),
    ) -> std::io::Result<Self> {
        let (_, _, cell_width_px, cell_height_px) = size;
        let cols = pane.width.max(1);
        let rows = pane.height.max(1);
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
            usize::try_from(pane.scrollback_limit_bytes).unwrap_or(usize::MAX),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            events_tx,
            render_notify,
            render_dirty.clone(),
        )?;
        runtime.resize(rows.max(1), cols.max(1), cell_width_px, cell_height_px);
        Ok(Self {
            launch_id,
            pane,
            prefix,
            runtime: Some(runtime),
            pane_id,
            events,
            render_dirty,
            size: (cols, rows, cell_width_px, cell_height_px),
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
        let (_, _, observed_cell_width_px, observed_cell_height_px) = size;
        let (cell_width_px, cell_height_px) =
            if observed_cell_width_px > 0 && observed_cell_height_px > 0 {
                (observed_cell_width_px, observed_cell_height_px)
            } else {
                (self.size.2, self.size.3)
            };
        self.size = (
            self.pane.width.max(1),
            self.pane.height.max(1),
            cell_width_px,
            cell_height_px,
        );
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        runtime.resize(self.size.1, self.size.0, cell_width_px, cell_height_px);
    }

    fn update_pane(&mut self, pane: OmpRendererPane, size: (u16, u16, u32, u32)) {
        self.pane = pane;
        self.resize(size);
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

    fn frame(&self) -> Option<FrameData> {
        let runtime = self.runtime.as_ref()?;
        let area = Rect::new(0, 0, self.pane.width.max(1), self.pane.height.max(1));
        let (buffer, cursor) = crate::server::render_stream::render_terminal_virtual(runtime, area);
        let hyperlinks = runtime.visible_hyperlinks(area);
        Some(FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            cursor,
            &hyperlinks,
        ))
    }
}

struct PendingDirectGraphics {
    image_id: u32,
    leading: Vec<u8>,
    control: String,
    size: (u16, u16, u32, u32),
}

#[derive(Default)]
pub(super) struct ClientOmpRenderer {
    latest_authority_revision: u64,
    omp_executable: Option<crate::update::OmpExecutable>,
    latest_launch_id: u64,
    attempted_launches: HashSet<u64>,
    offer: Option<TargetOffer>,
    projection: Option<OmpRendererFrame>,
    target: Option<LocalTarget>,
    cached_server_frame: Option<FrameData>,
    kitty_placement_replay: Option<crate::kitty_graphics::KittyPlacementReplay>,
    handoff_frame: Option<FrameData>,
    local_selected: bool,
    server_owned_input: bool,
    pending_direct_graphics: HashMap<u64, PendingDirectGraphics>,
    server_owned_frame: bool,
    server_overlay_keys: HashMap<crate::input::KeyIdentity, crate::input::TerminalKey>,
    local_keys: HashMap<crate::input::KeyIdentity, crate::input::TerminalKey>,
    server_overlay_forced_input: bool,
    next_link_request_id: u64,
    pending_link_click: bool,
    pending_link_request_id: Option<u64>,
    pending_link_input: Option<LinkInput>,
    queued_link_inputs: Vec<LinkInput>,
    pointer_cell: Option<(u16, u16)>,
    pointer_host_cell: Option<(u16, u16)>,
    pointer_in_pane: bool,
    pointer_pixels: Option<crate::input::mouse::HostPixels>,
    mouse_gesture_local: Option<bool>,
    local_mouse_buttons: Vec<(MouseButton, u8)>,
    server_mouse_buttons: HashSet<MouseButton>,
    server_mouse_position: Option<crate::input::mouse::Position>,
    server_mouse_geometry: Option<crate::input::mouse::HostGeometry>,
    server_mouse_modifiers: u8,
    local_mouse_position: Option<crate::input::mouse::Position>,
    server_gesture_mouse_mode: Option<(bool, bool)>,
    server_sgr_pixels_active: bool,
    outer_focused: bool,
    effective_focus: Option<bool>,
    hovered_link_cells: Option<Vec<(u16, u16)>>,
    suppress_link_affordance: bool,
    awaiting_fallback: bool,
    awaiting_promotion: Option<PromotionBarrier>,
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
            latest_authority_revision: 0,
            attempted_launches: HashSet::new(),
            offer: None,
            projection: None,
            target: None,
            cached_server_frame: None,
            kitty_placement_replay: None,
            handoff_frame: None,
            local_selected: false,
            server_owned_input: false,
            pending_direct_graphics: HashMap::new(),
            server_owned_frame: false,
            server_overlay_keys: HashMap::new(),
            local_keys: HashMap::new(),
            server_overlay_forced_input: false,
            next_link_request_id: 1,
            pending_link_click: false,
            pending_link_request_id: None,
            pending_link_input: None,
            queued_link_inputs: Vec::new(),
            pointer_cell: None,
            pointer_host_cell: None,
            pointer_in_pane: false,
            pointer_pixels: None,
            mouse_gesture_local: None,
            local_mouse_buttons: Vec::new(),
            server_mouse_buttons: HashSet::new(),
            server_mouse_position: None,
            server_mouse_geometry: None,
            server_mouse_modifiers: 0,
            local_mouse_position: None,
            server_gesture_mouse_mode: None,
            server_sgr_pixels_active: false,
            outer_focused: true,
            effective_focus: None,
            hovered_link_cells: None,
            suppress_link_affordance: false,
            awaiting_fallback: false,
            awaiting_promotion: None,
            deferred_messages: Vec::new(),
            outbound_messages: Vec::new(),
            effects: Vec::new(),
            needs_render: false,
            force_repaint: false,
        }
    }

    pub(super) fn apply_target(
        &mut self,
        launch_id: u64,
        authority_revision: u64,
        target_app_client_id: u64,
        route: Option<OmpRendererRoute>,
        bound: bool,
        surface_active: bool,
        prefix: OmpRendererPrefix,
        size: (u16, u16, u32, u32),
        current_input_generation: u64,
    ) {
        if self.omp_executable.is_none()
            || launch_id < self.latest_launch_id
            || authority_revision < self.latest_authority_revision
        {
            return;
        }
        let previous_launch_id = self.latest_launch_id;
        let previous_offer = self.offer.clone();
        let barrier_accepts_authority =
            self.awaiting_promotion
                .is_some_and(|barrier| match barrier {
                    PromotionBarrier::AuthorityAfter(revision) => authority_revision > revision,
                    PromotionBarrier::Frame(revision) => authority_revision >= revision,
                });
        self.latest_launch_id = launch_id;
        self.latest_authority_revision = authority_revision;
        let Some(route) = route else {
            self.finish_local_mouse_gesture();
            self.prepare_surface_handoff();
            self.stop_target(true);
            self.offer = None;
            self.projection = None;
            if launch_id == previous_launch_id && barrier_accepts_authority {
                self.resolve_promotion(false, current_input_generation);
            } else {
                self.discard_deferred_messages();
            }
            return;
        };
        let replace = previous_offer.as_ref().is_some_and(|offer| {
            offer.launch_id != launch_id
                || offer.target_app_client_id != target_app_client_id
                || offer.route != route
        });
        let authority_changed = previous_offer
            .as_ref()
            .is_none_or(|offer| offer.authority_revision != authority_revision);
        let new_active_authority = surface_active && (replace || authority_changed);
        if replace || !surface_active {
            self.finish_local_mouse_gesture();
        }
        if replace {
            self.prepare_surface_handoff();
            self.stop_target(true);
            self.discard_deferred_messages();
        } else if !surface_active {
            self.prepare_surface_handoff();
        }
        if authority_changed && self.mouse_gesture_local != Some(false) {
            self.server_owned_input = false;
        }
        if replace || authority_changed || !surface_active {
            self.projection = None;
        }
        if new_active_authority {
            self.awaiting_promotion = Some(PromotionBarrier::Frame(authority_revision));
        } else if !surface_active {
            self.server_owned_input = true;
        }
        self.offer = Some(TargetOffer {
            launch_id,
            authority_revision,
            target_app_client_id,
            route,
            bound,
            surface_active,
            prefix,
        });
        if !surface_active && !replace && barrier_accepts_authority {
            self.resolve_promotion(false, current_input_generation);
        }
        self.sync_target_to_projection(size, current_input_generation);
        self.needs_render = true;
    }

    fn sync_target_to_projection(
        &mut self,
        size: (u16, u16, u32, u32),
        current_input_generation: u64,
    ) {
        let Some(offer) = self.offer.clone() else {
            return;
        };
        let matching_projection = self.projection.filter(|projection| {
            projection.launch_id == offer.launch_id
                && projection.authority_revision == offer.authority_revision
        });
        let projection =
            matching_projection.filter(|projection| projection.pane.is_some_and(valid_pane));
        if self.target.is_none() {
            let Some(pane) = projection.and_then(|projection| projection.pane) else {
                if matching_projection.is_some()
                    && self.awaiting_promotion
                        == Some(PromotionBarrier::Frame(offer.authority_revision))
                    && !self.server_owned_frame
                {
                    self.resolve_promotion(false, current_input_generation);
                }
                return;
            };
            if !self.attempted_launches.insert(offer.launch_id) {
                return;
            }
            self.target = self.omp_executable.as_ref().and_then(|omp_executable| {
                LocalTarget::spawn(
                    omp_executable,
                    offer.launch_id,
                    offer.target_app_client_id,
                    offer.route.clone(),
                    pane,
                    offer.prefix.clone(),
                    size,
                )
                .ok()
            });
        }
        if !offer.bound {
            self.release_deferred_messages(current_input_generation);
        }
        if self.awaiting_promotion.is_some() {
            self.settle_local_gesture_before_stale_deferred_prune(current_input_generation);
        }
        let mut confirm_promotion = None;
        let Some(target) = self
            .target
            .as_mut()
            .filter(|target| target.launch_id == offer.launch_id)
        else {
            return;
        };
        if !offer.bound && target.ready_reported {
            target.fallback_confirmed = true;
        }
        if target.bound && !offer.bound {
            target.fail();
        }
        if offer.bound && !target.bound {
            target.bound_at = Some(Instant::now());
        }
        target.bound = offer.bound && !target.failed;
        target.surface_active = offer.surface_active
            && projection.is_some_and(|projection| projection.surface_active)
            && !self.server_owned_frame
            && !target.failed;
        target.promoted = target.ready_reported && target.bound;
        target.prefix = offer.prefix;
        if let Some(pane) = projection.and_then(|projection| projection.pane) {
            target.update_pane(pane, size);
        }
        if let Some(matching_projection) = matching_projection.filter(|_| {
            self.awaiting_promotion == Some(PromotionBarrier::Frame(offer.authority_revision))
                && target.ready_reported
                && target.bound
                && self.mouse_gesture_local != Some(false)
                && !self.server_owned_frame
                && !self.server_owned_input
        }) {
            confirm_promotion = Some((
                target.surface_active
                    && target.first_damage
                    && target.runtime.is_some()
                    && !target.failed
                    && !self.server_owned_input,
                matching_projection.frame_nonce,
            ));
        }
        if let Some((local_active, frame_nonce)) = confirm_promotion {
            if local_active {
                self.outbound_messages
                    .push(ClientMessage::OmpRendererAuthorityAck {
                        launch_id: offer.launch_id,
                        authority_revision: offer.authority_revision,
                        frame_nonce,
                    });
                self.local_selected = true;
                self.handoff_frame = None;
                self.suppress_link_affordance = false;
                self.needs_render = true;
                self.force_repaint = true;
            }
            self.resolve_promotion(local_active, current_input_generation);
        }
        self.remap_pointer_cell();
        self.remap_pointer_pixels();
        self.refresh_selection(false);
        self.sync_effective_focus();
        self.refresh_hovered_link();
    }

    fn sync_effective_focus(&mut self) {
        let focused = self.outer_focused && self.local_selected && self.projection_focused();
        if self.effective_focus == Some(focused) {
            return;
        }
        self.effective_focus = Some(focused);
        if let Some(runtime) = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
        {
            let event = if focused {
                crate::ghostty::FocusEvent::Gained
            } else {
                crate::ghostty::FocusEvent::Lost
            };
            if !forward_local_focus_event(runtime, event) {
                self.begin_local_forward_fallback();
            }
        }
    }

    pub(super) fn cache_server_frame(
        &mut self,
        frame: FrameData,
        cell_size_px: (u32, u32),
        current_input_generation: u64,
    ) -> Option<SurfaceFrame> {
        let raw_projection = frame.omp_renderer;
        let authority_projection = raw_projection.filter(|projection| {
            self.offer.as_ref().is_some_and(|offer| {
                projection.launch_id == offer.launch_id
                    && projection.authority_revision == offer.authority_revision
            })
        });
        let rejected_projection = raw_projection.is_some() && authority_projection.is_none();
        if rejected_projection {
            return None;
        }
        let server_owned_frame = authority_projection.is_some_and(|projection| {
            projection.server_owned_overlay
                || !projection.surface_active
                || projection.pane.is_none_or(|pane| !valid_pane(pane))
        });
        let projection =
            authority_projection.filter(|projection| projection.pane.is_none_or(valid_pane));
        let entering_server_owned_frame = server_owned_frame && !self.server_owned_frame;
        self.server_owned_frame = server_owned_frame;
        if entering_server_owned_frame {
            self.finish_local_mouse_gesture();
            self.release_pending_link_inputs_to_server(current_input_generation);
            self.release_deferred_to_server(current_input_generation);
        }
        if !self.server_owned_frame {
            self.release_overlay_input_if_idle(current_input_generation);
        }
        self.projection = projection;
        let replay_size = (frame.width, frame.height, cell_size_px.0, cell_size_px.1);
        let replay_graphics = self.update_graphics_replay(&frame.graphics, replay_size);
        let mut cached = frame.clone();
        cached.graphics = replay_graphics;
        self.cached_server_frame = Some(cached);
        self.sync_target_to_projection(replay_size, current_input_generation);
        self.refresh_selection(false);
        self.sync_effective_focus();
        if projection.is_some_and(|projection| projection.pane.is_some_and(valid_pane)) {
            self.handoff_frame = None;
            self.suppress_link_affordance = false;
        }
        self.needs_render = false;
        Some(SurfaceFrame {
            frame: self.composed_or_base_frame(&frame),
            force_repaint: std::mem::take(&mut self.force_repaint),
        })
    }

    pub(super) fn cache_server_graphics(&mut self, bytes: &[u8], size: (u16, u16, u32, u32)) {
        let replay_graphics = self.update_graphics_replay(bytes, size);
        if let Some(frame) = self.cached_server_frame.as_mut() {
            frame.graphics = replay_graphics;
        }
    }

    pub(super) fn stage_direct_graphics(
        &mut self,
        transfer_id: u64,
        image_id: u32,
        leading: Vec<u8>,
        control: String,
        size: (u16, u16, u32, u32),
    ) {
        self.pending_direct_graphics.insert(
            transfer_id,
            PendingDirectGraphics {
                image_id,
                leading,
                control,
                size,
            },
        );
    }

    pub(super) fn complete_direct_graphics(
        &mut self,
        transfer_id: u64,
        image_id: u32,
        success: bool,
    ) {
        let Some(pending) = self.pending_direct_graphics.remove(&transfer_id) else {
            return;
        };
        if !success || pending.image_id != image_id {
            return;
        }
        if self.kitty_placement_replay.is_none() {
            self.kitty_placement_replay =
                crate::kitty_graphics::KittyPlacementReplay::new(pending.size);
        }
        let replay = self.kitty_placement_replay.as_mut().and_then(|replay| {
            replay.register_external_file(
                image_id,
                &pending.leading,
                &pending.control,
                pending.size,
            )
        });
        self.apply_graphics_replay(replay);
    }

    pub(super) fn retire_direct_graphics(&mut self, transfer_id: u64, image_id: u32) {
        self.pending_direct_graphics.remove(&transfer_id);
        let replay = self
            .kitty_placement_replay
            .as_mut()
            .and_then(|replay| replay.retire_external_file(image_id));
        self.apply_graphics_replay(replay);
    }

    fn apply_graphics_replay(&mut self, replay: Option<Vec<u8>>) {
        if replay.is_none() {
            self.kitty_placement_replay = None;
        }
        if let Some(frame) = self.cached_server_frame.as_mut() {
            frame.graphics = replay.unwrap_or_default();
        }
    }

    fn update_graphics_replay(&mut self, bytes: &[u8], size: (u16, u16, u32, u32)) -> Vec<u8> {
        if self.kitty_placement_replay.is_none() {
            if bytes.is_empty() {
                return Vec::new();
            }
            self.kitty_placement_replay = crate::kitty_graphics::KittyPlacementReplay::new(size);
        }
        let replay = self
            .kitty_placement_replay
            .as_mut()
            .and_then(|replay| replay.update(bytes, size));
        if replay.is_none() {
            self.kitty_placement_replay = None;
        }
        replay.unwrap_or_default()
    }

    fn refresh_selection(&mut self, force: bool) {
        let should_select = self.target.as_ref().is_some_and(|target| {
            !target.failed
                && target.runtime.is_some()
                && target.bound
                && target.first_damage
                && target.surface_active
                && self.projection.is_some_and(|projection| {
                    projection.surface_active
                        && !projection.server_owned_overlay
                        && projection.pane.is_some_and(valid_pane)
                })
                && !self.server_owned_input
                && self.awaiting_promotion.is_none()
        });
        if should_select == self.local_selected {
            return;
        }
        if !should_select {
            self.prepare_surface_handoff();
        }
        self.local_selected = should_select;
        if should_select {
            self.handoff_frame = None;
            self.suppress_link_affordance = false;
        }
        self.needs_render = true;
        self.force_repaint |= force;
    }

    fn composed_or_base_frame(&self, base: &FrameData) -> FrameData {
        let base = strip_renderer_metadata(base.clone());
        if self
            .projection
            .is_some_and(|projection| projection.server_owned_overlay)
        {
            return base;
        }
        if !self.local_selected {
            return base;
        }
        let Some(projection) = self.projection else {
            return base;
        };
        let Some(pane) = projection.pane else {
            return base;
        };
        let Some(mut local) = self.target.as_ref().and_then(LocalTarget::frame) else {
            return base;
        };
        if let Some(cells) = self.hovered_link_cells.as_deref() {
            for &(column, row) in cells {
                if column < local.width && row < local.height {
                    let index = usize::from(row) * usize::from(local.width) + usize::from(column);
                    local.cells[index].modifier |= ratatui::style::Modifier::UNDERLINED.bits();
                }
            }
        }
        let fallback = base.clone();
        compose_local_pane(base, local, pane, projection.focused).unwrap_or(fallback)
    }
    fn current_projection_pane(&self) -> Option<OmpRendererPane> {
        self.projection
            .filter(|projection| projection.surface_active)
            .and_then(|projection| projection.pane)
            .filter(|pane| valid_pane(*pane))
    }

    fn projection_focused(&self) -> bool {
        self.projection.is_some_and(|projection| {
            projection.focused
                && projection.surface_active
                && !projection.server_owned_overlay
                && projection.pane.is_some_and(valid_pane)
        })
    }

    pub(super) fn resize(
        &mut self,
        size: (u16, u16, u32, u32),
        host_geometry: Option<crate::input::mouse::HostGeometry>,
        current_input_generation: u64,
    ) {
        self.finish_local_mouse_gesture();
        self.finish_server_mouse_gesture();
        self.cancel_stale_link_inputs(current_input_generation);
        self.deferred_messages
            .retain_mut(|message| message.retain_for_generation(current_input_generation));
        let authority_geometry_changed = self
            .offer
            .as_ref()
            .filter(|offer| offer.surface_active)
            .is_some_and(|_| {
                self.cached_server_frame
                    .as_ref()
                    .is_some_and(|frame| (frame.width, frame.height) != (size.0, size.1))
                    || (size.2 > 0
                        && size.3 > 0
                        && self.target.as_ref().is_some_and(|target| {
                            (target.size.2, target.size.3) != (size.2, size.3)
                        }))
            });
        if authority_geometry_changed {
            self.prepare_surface_handoff();
            self.local_selected = false;
            if self.mouse_gesture_local != Some(false) {
                self.server_owned_input = false;
            }
            self.projection = None;
            self.awaiting_promotion = self
                .offer
                .as_ref()
                .map(|offer| PromotionBarrier::AuthorityAfter(offer.authority_revision));
        }
        let lost_pixel_geometry = self.pointer_pixels.is_some() && host_geometry.is_none();
        self.pointer_pixels = self.pointer_pixels.and_then(|mut pointer| {
            pointer.geometry = host_geometry?;
            Some(pointer)
        });
        if lost_pixel_geometry {
            self.pointer_host_cell = None;
        }
        if let Some(target) = self.target.as_mut() {
            target.resize(size);
        }
        if self.pointer_pixels.is_some() {
            self.remap_pointer_pixels();
        } else {
            self.remap_pointer_cell();
        }
        self.refresh_hovered_link();
        self.force_repaint = true;
        self.needs_render = true;
    }

    pub(super) fn observe_outer_focus(&mut self, events: &[crate::raw_input::RawInputEvent]) {
        for event in events {
            match event {
                crate::raw_input::RawInputEvent::OuterFocusGained => self.outer_focused = true,
                crate::raw_input::RawInputEvent::OuterFocusLost => {
                    let releases = self.finish_input_leases_on_focus_lost();
                    if !releases.is_empty() {
                        self.outbound_messages
                            .push(ClientMessage::ServerOwnedInputEvents { events: releases });
                    }
                    self.outer_focused = false;
                }
                _ => continue,
            }
            self.sync_effective_focus();
        }
    }

    pub(super) fn owns_input(&self) -> bool {
        self.server_owned_frame
            || self.local_selected
            || self.server_owned_input
            || self.awaiting_fallback
            || self.awaiting_promotion.is_some()
    }

    pub(super) fn set_server_sgr_pixels_active(&mut self, active: bool) {
        self.server_sgr_pixels_active = active;
    }

    fn target_mouse_mode(&self) -> (bool, bool) {
        self.target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .map_or((false, false), |runtime| {
                let capture = runtime.mouse_reporting_enabled();
                (capture, capture && runtime.sgr_pixel_mouse_enabled())
            })
    }

    pub(super) fn local_mouse_mode(&self) -> (bool, bool) {
        if self.mouse_gesture_local == Some(false) {
            return self
                .server_gesture_mouse_mode
                .unwrap_or_else(|| self.target_mouse_mode());
        }
        if self.awaiting_promotion.is_some() {
            return self.target_mouse_mode();
        }
        if !self.local_selected || self.server_owned_input {
            return (false, false);
        }
        self.target_mouse_mode()
    }

    pub(super) fn finish_local_mouse_gesture(&mut self) {
        let buttons = std::mem::take(&mut self.local_mouse_buttons);
        if buttons.is_empty() {
            return;
        }
        let position = self.local_mouse_position.take();
        let release_failed = match (
            position,
            self.target
                .as_ref()
                .and_then(|target| target.runtime.as_ref()),
        ) {
            (Some(position), Some(runtime)) => buttons.into_iter().any(|(button, modifiers)| {
                !forward_local_mouse_release(
                    runtime,
                    button,
                    position,
                    crossterm::event::KeyModifiers::from_bits_truncate(modifiers),
                )
            }),
            _ => true,
        };
        self.mouse_gesture_local = None;
        self.server_mouse_buttons.clear();
        self.clear_server_mouse_position();
        self.server_gesture_mouse_mode = None;
        if release_failed {
            self.begin_local_forward_fallback();
        }
    }

    fn begin_mouse_gesture(&mut self, mouse: &MouseEvent, local: bool) {
        let MouseEventKind::Down(button) = mouse.kind else {
            return;
        };
        if local {
            if self.mouse_gesture_local != Some(true) {
                self.local_mouse_buttons.clear();
                self.local_mouse_position = None;
            }
            self.mouse_gesture_local = Some(true);
            self.server_mouse_buttons.clear();
            self.clear_server_mouse_position();
            self.server_gesture_mouse_mode = None;
            if let Some((_, modifiers)) = self
                .local_mouse_buttons
                .iter_mut()
                .find(|(pressed, _)| *pressed == button)
            {
                *modifiers = mouse.modifiers.bits();
            } else {
                self.local_mouse_buttons
                    .push((button, mouse.modifiers.bits()));
            }
        } else {
            if self.mouse_gesture_local != Some(false) {
                self.local_mouse_buttons.clear();
                self.local_mouse_position = None;
                self.server_mouse_buttons.clear();
                self.clear_server_mouse_position();
                self.server_gesture_mouse_mode = Some(self.target_mouse_mode());
            }
            self.mouse_gesture_local = Some(false);
            self.server_mouse_buttons.insert(button);
        }
    }

    fn record_server_mouse_cell(&mut self, mouse: &MouseEvent) {
        self.server_mouse_position = Some(crate::input::mouse::Position::Cell {
            column: mouse.column,
            row: mouse.row,
        });
        self.server_mouse_geometry = None;
        self.server_mouse_modifiers = mouse.modifiers.bits();
    }

    fn record_server_mouse_pixels(
        &mut self,
        data: &[u8],
        geometry: crate::input::mouse::HostGeometry,
    ) {
        let Some((x, y)) = crate::input::mouse::parse_report(data) else {
            return;
        };
        self.server_mouse_position = Some(crate::input::mouse::Position::Pixels { x, y });
        self.server_mouse_geometry = Some(geometry);
        self.server_mouse_modifiers =
            decode_pixel_mouse(data).map_or(0, |mouse| mouse.modifiers.bits());
    }

    fn clear_server_mouse_position(&mut self) {
        self.server_mouse_position = None;
        self.server_mouse_geometry = None;
        self.server_mouse_modifiers = 0;
    }

    fn take_server_mouse_gesture_releases(&mut self) -> Vec<ClientInputEvent> {
        if self.mouse_gesture_local != Some(false) {
            return Vec::new();
        }
        let buttons = std::mem::take(&mut self.server_mouse_buttons);
        let position = self
            .server_mouse_position
            .take()
            .and_then(|position| match position {
                crate::input::mouse::Position::Cell { column, row } => Some((column, row)),
                crate::input::mouse::Position::Pixels { x, y } => self
                    .server_mouse_geometry
                    .and_then(|geometry| geometry.cell(x, y)),
            });
        let modifiers = self.server_mouse_modifiers;
        self.clear_server_mouse_position();
        self.end_mouse_gesture();
        let Some((column, row)) = position else {
            return Vec::new();
        };
        buttons
            .into_iter()
            .map(|button| ClientInputEvent::Mouse {
                kind: ClientMouseKind::Up(ClientMouseButton::from_crossterm(button)),
                column,
                row,
                modifiers,
            })
            .collect()
    }

    fn finish_server_mouse_gesture(&mut self) {
        let events = self.take_server_mouse_gesture_releases();
        if !events.is_empty() {
            self.outbound_messages
                .push(ClientMessage::ServerOwnedInputEvents { events });
        }
    }

    fn release_mouse_button(&mut self, button: MouseButton) {
        match self.mouse_gesture_local {
            Some(true) => {
                let Some(index) = self
                    .local_mouse_buttons
                    .iter()
                    .position(|(pressed, _)| *pressed == button)
                else {
                    return;
                };
                self.local_mouse_buttons.remove(index);
                if self.local_mouse_buttons.is_empty() {
                    self.end_mouse_gesture();
                }
            }
            Some(false)
                if self.server_mouse_buttons.remove(&button)
                    && self.server_mouse_buttons.is_empty() =>
            {
                self.end_mouse_gesture();
            }
            _ => {}
        }
    }

    fn end_mouse_gesture(&mut self) {
        self.mouse_gesture_local = None;
        self.local_mouse_buttons.clear();
        self.local_mouse_position = None;
        self.server_mouse_buttons.clear();
        self.clear_server_mouse_position();
        self.server_gesture_mouse_mode = None;
    }

    fn settle_server_gesture(&mut self, current_input_generation: u64) {
        if self.awaiting_promotion.is_some()
            || self
                .offer
                .as_ref()
                .is_none_or(|offer| !offer.surface_active)
        {
            self.server_owned_input = false;
        }
        if let Some(size) = self.target.as_ref().map(|target| target.size) {
            self.sync_target_to_projection(size, current_input_generation);
        }
        self.refresh_selection(false);
        self.sync_effective_focus();
    }

    fn prepare_surface_handoff(&mut self) {
        if !self.local_selected {
            return;
        }
        let composed = self.cached_server_frame.as_ref().and_then(|base| {
            let projection = self.projection?;
            let pane = projection.pane?;

            let local = self.target.as_ref()?.frame()?;
            compose_local_pane(
                strip_renderer_metadata(base.clone()),
                local,
                pane,
                projection.focused,
            )
        });
        if let Some(frame) = composed {
            self.store_handoff_frame(&frame);
        } else if self.handoff_frame.is_none() {
            if let Some(frame) = self.cached_server_frame.clone() {
                self.store_handoff_frame(&frame);
            }
        }
        self.suppress_link_affordance = true;
        self.needs_render = true;
        self.force_repaint = true;
    }

    fn store_handoff_frame(&mut self, frame: &FrameData) {
        let mut frame = strip_renderer_metadata(frame.clone());
        if let Some(pane) = self.projection.and_then(|projection| projection.pane) {
            let x_end = pane.x.checked_add(pane.width);
            let y_end = pane.y.checked_add(pane.height);
            if x_end.is_some_and(|x| x <= frame.width) && y_end.is_some_and(|y| y <= frame.height) {
                for local_y in 0..pane.height {
                    for local_x in 0..pane.width {
                        let index = usize::from(pane.y + local_y) * usize::from(frame.width)
                            + usize::from(pane.x + local_x);
                        let cell = &mut frame.cells[index];
                        cell.hyperlink = None;
                        if self
                            .hovered_link_cells
                            .as_deref()
                            .is_some_and(|cells| cells.contains(&(local_x, local_y)))
                        {
                            cell.modifier &= !ratatui::style::Modifier::UNDERLINED.bits();
                        }
                    }
                }
                let server_hyperlinks = self
                    .cached_server_frame
                    .as_ref()
                    .map_or(0, |base| base.hyperlinks.len());
                frame.hyperlinks.truncate(server_hyperlinks);
            }
        }
        self.handoff_frame = Some(frame);
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
        self.pointer_host_cell = cell;
        self.remap_pointer_cell();
    }

    fn remap_pointer_cell(&mut self) {
        let Some((column, row)) = self.pointer_host_cell else {
            self.pointer_in_pane = false;
            self.set_pointer_cell(None);
            return;
        };
        let cell = self
            .current_projection_pane()
            .and_then(|pane| pane_local_cell(pane, column, row));
        self.pointer_in_pane = cell.is_some();
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
        let Some(pane) = self.current_projection_pane() else {
            self.pointer_in_pane = false;
            self.set_pointer_cell(None);
            return;
        };
        let cell = self
            .target
            .as_ref()
            .and_then(|target| local_pixel_cell(pointer, pane, target.size));
        self.pointer_in_pane = cell.is_some();
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

    pub(super) fn native_link_active(&self) -> Option<bool> {
        if !self.pointer_in_pane {
            return None;
        }
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
        self.cancel_stale_link_inputs(input_generation);
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
        let mut serverward_gesture_suffix = false;
        let mut server_gesture_ended = false;
        for event in events {
            let protocol_event = client_event_from_raw(&event);
            let released_mouse_button = match &event {
                crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(button),
                    ..
                }) => Some(*button),
                _ => None,
            };
            let is_outer_focus = matches!(
                &event,
                crate::raw_input::RawInputEvent::OuterFocusGained
                    | crate::raw_input::RawInputEvent::OuterFocusLost
            );
            let focus_routes_server =
                matches!(&event, crate::raw_input::RawInputEvent::OuterFocusLost).then(|| {
                    self.server_owned_frame
                        || self.server_owned_input
                        || !self.local_selected
                        || !self.projection_focused()
                });
            let host_mouse_cell = match &event {
                crate::raw_input::RawInputEvent::Mouse(mouse) => Some((mouse.column, mouse.row)),
                _ => None,
            };
            let local_mouse_cell = host_mouse_cell.and_then(|(column, row)| {
                self.current_projection_pane()
                    .and_then(|pane| pane_local_cell(pane, column, row))
            });
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
            if matches!(event, crate::raw_input::RawInputEvent::OuterFocusGained) {
                self.outer_focused = true;
                self.sync_effective_focus();
            } else if matches!(event, crate::raw_input::RawInputEvent::OuterFocusLost) {
                server_batch.extend(self.finish_input_leases_on_focus_lost());
                self.outer_focused = false;
                self.sync_effective_focus();
            }
            if let crate::raw_input::RawInputEvent::Key(key) = &event {
                let identity = key.identity();
                if self.local_keys.contains_key(&identity) {
                    let released = key.kind == KeyEventKind::Release;
                    let sent = self
                        .target
                        .as_ref()
                        .and_then(|target| target.runtime.as_ref())
                        .is_some_and(|runtime| {
                            forward_local_event(
                                runtime,
                                crate::raw_input::RawInputEvent::Key(key.clone()),
                            )
                        });
                    if released || !sent {
                        self.local_keys.remove(&identity);
                    }
                    if !sent {
                        self.begin_local_forward_fallback();
                        if let Some(event) = protocol_event {
                            deferred_events.push(event);
                        }
                    }
                    continue;
                }
            }
            if self.server_owned_frame {
                match &event {
                    crate::raw_input::RawInputEvent::Key(key) => {
                        self.track_overlay_server_key(key);
                    }
                    crate::raw_input::RawInputEvent::Mouse(mouse) => {
                        if matches!(mouse.kind, MouseEventKind::Down(_)) {
                            self.begin_overlay_server_input();
                            self.begin_mouse_gesture(mouse, false);
                        }
                        if self.mouse_gesture_local == Some(false) {
                            self.record_server_mouse_cell(mouse);
                        }
                        if let MouseEventKind::Up(button) = mouse.kind {
                            self.release_mouse_button(button);
                        }
                    }
                    _ => {}
                }
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                }
                continue;
            }
            if let crate::raw_input::RawInputEvent::Key(key) = &event {
                if self.server_overlay_keys.contains_key(&key.identity()) {
                    self.track_overlay_server_key(key);
                    if let Some(event) = protocol_event {
                        server_batch.push(event);
                    }
                    self.release_overlay_input_if_idle(input_generation);
                    continue;
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
            if self.pending_link_request_id.is_some() {
                if let Some(event) = protocol_event {
                    self.queue_link_input(LinkInput::Events {
                        events: vec![event],
                        generation: input_generation,
                    });
                }
                continue;
            }
            let mut targets_local = if host_mouse_cell.is_some() {
                local_mouse_cell.is_some()
            } else {
                self.projection_focused()
            };
            if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
                ) {
                    if let Some(local) = self.mouse_gesture_local {
                        targets_local = local;
                    }
                }
            }
            if (self.awaiting_promotion.is_some() || self.awaiting_fallback)
                && self.mouse_gesture_local != Some(false)
                && !serverward_gesture_suffix
            {
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
                }
                continue;
            }
            if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                if matches!(mouse.kind, MouseEventKind::Down(_))
                    && self.local_selected
                    && !self.server_owned_input
                    && !targets_local
                {
                    self.prepare_surface_handoff();
                    self.server_owned_input = true;
                    self.begin_mouse_gesture(mouse, false);
                    self.record_server_mouse_cell(mouse);
                    self.refresh_selection(false);
                    self.sync_effective_focus();
                    if !server_batch.is_empty() {
                        messages.push(ClientMessage::ServerOwnedInputEvents {
                            events: std::mem::take(&mut server_batch),
                        });
                    }
                    if let Some(event) = protocol_event {
                        messages.push(ClientMessage::ServerOwnedInputEvents {
                            events: vec![event],
                        });
                    }
                    continue;
                }
            }
            if self.local_selected && !self.server_owned_input {
                if let (crate::raw_input::RawInputEvent::Mouse(mouse), Some((column, row))) =
                    (&event, local_mouse_cell)
                {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        let request_id = self.allocate_link_request_id();
                        if let Some(message) = self.link_activation_message(column, row, request_id)
                        {
                            if !server_batch.is_empty() {
                                messages.push(ClientMessage::ServerOwnedInputEvents {
                                    events: std::mem::take(&mut server_batch),
                                });
                            }
                            messages.push(message);
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
            if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.begin_mouse_gesture(
                        mouse,
                        targets_local && self.local_selected && !self.server_owned_input,
                    );
                }
            }
            let prefix = self.local_selected
                && self.projection_focused()
                && matches!(&event, crate::raw_input::RawInputEvent::Key(key)
                if key.kind != KeyEventKind::Release
                    && self.target.as_ref().is_some_and(|target| {
                        crate::config::terminal_key_matches_combo(key, target.prefix.key_combo())
                    }));
            if prefix {
                if !server_batch.is_empty() {
                    messages.push(ClientMessage::ServerOwnedInputEvents {
                        events: std::mem::take(&mut server_batch),
                    });
                }
                if let Some(event) = protocol_event {
                    messages.push(ClientMessage::ServerOwnedInputEvents {
                        events: vec![event],
                    });
                }
                self.prepare_surface_handoff();
                self.server_owned_input = true;
                self.refresh_hovered_link();
                self.needs_render = true;
                self.force_repaint = true;
                continue;
            }
            if focus_routes_server
                .unwrap_or(self.server_owned_input || !self.local_selected || !targets_local)
            {
                if let Some(event) = protocol_event {
                    server_batch.push(event);
                }
                if let Some(button) = released_mouse_button {
                    let releases_server_button = self.mouse_gesture_local == Some(false)
                        && self.server_mouse_buttons.contains(&button);
                    let releases_local_button = self.mouse_gesture_local == Some(true)
                        && self
                            .local_mouse_buttons
                            .iter()
                            .any(|(pressed, _)| *pressed == button);
                    if releases_server_button || releases_local_button {
                        self.release_mouse_button(button);
                        if releases_server_button && self.mouse_gesture_local != Some(false) {
                            serverward_gesture_suffix = true;
                            server_gesture_ended = true;
                        }
                    }
                }
                if self.mouse_gesture_local == Some(false) {
                    if let crate::raw_input::RawInputEvent::Mouse(mouse) = &event {
                        self.record_server_mouse_cell(mouse);
                    }
                }
                continue;
            }
            let local_mouse_cell = local_mouse_cell.or_else(|| {
                self.mouse_gesture_local
                    .filter(|local| *local)
                    .and_then(|_| self.current_projection_pane())
                    .and_then(|pane| {
                        host_mouse_cell
                            .map(|(column, row)| clamp_pane_local_cell(pane, column, row))
                    })
            });
            let local_mouse_position = local_mouse_cell
                .map(|(column, row)| crate::input::mouse::Position::Cell { column, row });
            let local_key_lifecycle = match &event {
                crate::raw_input::RawInputEvent::Key(key) if key.reports_event_types() => {
                    Some((key.identity(), key.kind, key.clone()))
                }
                _ => None,
            };
            let local_event = if is_outer_focus {
                None
            } else {
                localize_raw_event(event, local_mouse_cell, self.projection_focused())
            };
            let sent = if is_outer_focus {
                true
            } else {
                self.target
                    .as_ref()
                    .and_then(|target| target.runtime.as_ref())
                    .is_some_and(|runtime| {
                        local_event.is_some_and(|event| forward_local_event(runtime, event))
                    })
            };
            if sent && self.mouse_gesture_local == Some(true) {
                if let Some(position) = local_mouse_position {
                    self.local_mouse_position = Some(position);
                }
            }
            if sent {
                if let Some((identity, kind, key)) = local_key_lifecycle {
                    match kind {
                        KeyEventKind::Press | KeyEventKind::Repeat => {
                            self.local_keys.insert(identity, key);
                        }
                        KeyEventKind::Release => {
                            self.local_keys.remove(&identity);
                        }
                    }
                }
            }
            if !sent {
                self.begin_local_forward_fallback();
                if let Some(event) = protocol_event {
                    deferred_events.push(event);
                }
            }
            if let Some(button) = released_mouse_button {
                self.release_mouse_button(button);
            }
        }
        if !server_batch.is_empty() {
            messages.push(ClientMessage::ServerOwnedInputEvents {
                events: server_batch,
            });
        }
        if !deferred_events.is_empty() {
            self.deferred_messages.push(DeferredMessage::InputEvents {
                events: deferred_events,
                generation: input_generation,
            });
        }
        if server_gesture_ended {
            self.settle_server_gesture(input_generation);
            self.release_overlay_input_if_idle(input_generation);
        }
        messages
    }

    pub(super) fn route_pixel_input(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
    ) -> Option<ClientMessage> {
        self.cancel_stale_link_inputs(input_generation);
        self.route_pixel_input_inner(data, geometry, input_generation, true)
    }

    fn server_pixel_input_message(
        &self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
    ) -> ClientMessage {
        if self.server_sgr_pixels_active {
            return pixel_input_message(data, geometry);
        }
        let events = crate::input::mouse::parse_report(&data)
            .and_then(|(x, y)| {
                geometry.cell(x, y).and_then(|(column, row)| {
                    crate::input::mouse::report_at_cell(&data, column, row)
                })
            })
            .map(|report| {
                crate::raw_input::parse_raw_input_bytes_sync(&report)
                    .into_iter()
                    .filter_map(|event| client_event_from_raw(&event))
                    .collect()
            })
            .unwrap_or_default();
        ClientMessage::ServerOwnedInputEvents { events }
    }

    fn route_pixel_input_inner(
        &mut self,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
        input_generation: u64,
        observe_pointer: bool,
    ) -> Option<ClientMessage> {
        let mouse_kind = decode_pixel_mouse(&data).map(|mouse| mouse.kind);
        let released_mouse_button = match mouse_kind {
            Some(MouseEventKind::Up(button)) => Some(button),
            _ => None,
        };
        if observe_pointer {
            if let Some((x, y)) = crate::input::mouse::parse_report(&data) {
                self.pointer_pixels = Some(crate::input::mouse::HostPixels { x, y, geometry });
                self.remap_pointer_pixels();
            }
        }
        if self.server_owned_frame {
            if matches!(mouse_kind, Some(MouseEventKind::Down(_))) {
                if let Some(mouse) = decode_pixel_mouse(&data) {
                    self.begin_overlay_server_input();
                    self.begin_mouse_gesture(&mouse, false);
                }
            }
            if self.mouse_gesture_local == Some(false) {
                self.record_server_mouse_pixels(&data, geometry);
            }
            let message = self.server_pixel_input_message(data, geometry);
            if let Some(button) = released_mouse_button {
                self.release_mouse_button(button);
            }
            return Some(message);
        }
        let mut local_mouse = self.current_projection_pane().and_then(|pane| {
            self.target
                .as_ref()
                .and_then(|target| decode_local_pixel_mouse(&data, geometry, pane, target.size))
        });
        if matches!(
            mouse_kind,
            Some(MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_))
        ) && self.mouse_gesture_local == Some(true)
            && local_mouse.is_none()
        {
            local_mouse = self.current_projection_pane().and_then(|pane| {
                self.target.as_ref().and_then(|target| {
                    decode_clamped_local_pixel_mouse(&data, geometry, pane, target.size)
                })
            });
        }
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
        if (self.awaiting_promotion.is_some() || self.awaiting_fallback)
            && self.mouse_gesture_local != Some(false)
        {
            self.deferred_messages.push(DeferredMessage::InputPixels {
                data,
                geometry,
                generation: input_generation,
            });
            return None;
        }
        if matches!(mouse_kind, Some(MouseEventKind::Down(_)))
            && self.local_selected
            && !self.server_owned_input
            && local_mouse.is_none()
        {
            self.prepare_surface_handoff();
            self.server_owned_input = true;
            self.refresh_selection(false);
            self.sync_effective_focus();
        }
        if (self.server_owned_input || self.mouse_gesture_local == Some(false))
            && matches!(mouse_kind, Some(MouseEventKind::Down(_)))
        {
            if let Some(mouse) = decode_pixel_mouse(&data) {
                self.begin_mouse_gesture(&mouse, false);
                self.record_server_mouse_pixels(&data, geometry);
            }
        }
        if self.server_owned_input
            || self.mouse_gesture_local == Some(false)
            || !self.local_selected
            || local_mouse.is_none()
        {
            if self.mouse_gesture_local == Some(false) {
                self.record_server_mouse_pixels(&data, geometry);
            }
            let message = self.server_pixel_input_message(data, geometry);
            if let Some(button) = released_mouse_button {
                let releases_server_button = self.mouse_gesture_local == Some(false)
                    && self.server_mouse_buttons.contains(&button);
                let releases_local_button = self.mouse_gesture_local == Some(true)
                    && self
                        .local_mouse_buttons
                        .iter()
                        .any(|(pressed, _)| *pressed == button);
                if releases_server_button || releases_local_button {
                    self.release_mouse_button(button);
                    if releases_server_button && self.mouse_gesture_local != Some(false) {
                        self.settle_server_gesture(input_generation);
                        self.release_overlay_input_if_idle(input_generation);
                    }
                }
            }
            return Some(message);
        }
        if local_mouse.as_ref().is_some_and(|mouse| {
            matches!(mouse.mouse.kind, MouseEventKind::Down(MouseButton::Left))
        }) {
            let Some(local_mouse) = local_mouse else {
                return Some(self.server_pixel_input_message(data, geometry));
            };
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
        if let Some(local_mouse) = local_mouse
            .as_ref()
            .filter(|mouse| matches!(mouse.mouse.kind, MouseEventKind::Down(_)))
        {
            self.begin_mouse_gesture(&local_mouse.mouse, true);
        }
        let local_mouse_position = local_mouse.map(|mouse| mouse.position);
        let sent = self.target.as_ref().and_then(|target| {
            let runtime = target.runtime.as_ref()?;
            Some(forward_local_pixel_mouse(runtime, local_mouse?))
        });
        match sent {
            Some(true) => {
                if self.mouse_gesture_local == Some(true) {
                    self.local_mouse_position = local_mouse_position;
                }
                if let Some(button) = released_mouse_button {
                    self.release_mouse_button(button);
                }
                None
            }
            None => {
                if let Some(button) = released_mouse_button {
                    self.release_mouse_button(button);
                }
                Some(self.server_pixel_input_message(data, geometry))
            }
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

    pub(super) fn next_frame(&mut self, now: Instant, _size: (u16, u16)) -> Option<SurfaceFrame> {
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
                self.awaiting_promotion = self
                    .offer
                    .as_ref()
                    .filter(|offer| offer.launch_id == target.launch_id)
                    .map(|offer| PromotionBarrier::AuthorityAfter(offer.authority_revision));
            }
            if target.failed && target.ready_reported && !target.fallback_confirmed {
                self.awaiting_promotion = None;
                self.awaiting_fallback = true;
            }
        }
        let was_selected = self.local_selected;
        self.refresh_selection(true);
        self.sync_effective_focus();
        let selection_changed = was_selected != self.local_selected;
        if damaged || selection_changed {
            self.refresh_hovered_link();
        }
        if self.local_selected && (damaged || self.needs_render) {
            let base = self.cached_server_frame.as_ref()?.clone();
            let frame = self.composed_or_base_frame(&base);
            self.store_handoff_frame(&frame);
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
            let frame = self.handoff_frame.take().or_else(|| {
                self.cached_server_frame
                    .clone()
                    .map(strip_renderer_metadata)
            });
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

        let replay_locally = self.local_selected && !self.server_owned_input;
        let runtime_missing = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
            .is_none();
        let pane = self.current_projection_pane();
        let focused = self.projection_focused();
        if replay_locally {
            match &input {
                LinkInput::Events { events, .. } => {
                    for event in events {
                        if let crate::raw_input::RawInputEvent::Mouse(mouse) =
                            event.to_raw_input_event()
                        {
                            match mouse.kind {
                                MouseEventKind::Down(_) => self.begin_mouse_gesture(&mouse, true),
                                MouseEventKind::Up(button) => self.release_mouse_button(button),
                                _ => {}
                            }
                        }
                    }
                }
                LinkInput::Pixels { inputs, .. } => {
                    for (data, _) in inputs {
                        if let Some(mouse) = decode_pixel_mouse(data) {
                            match mouse.kind {
                                MouseEventKind::Down(_) => self.begin_mouse_gesture(&mouse, true),
                                MouseEventKind::Up(button) => self.release_mouse_button(button),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        match input {
            LinkInput::Events {
                events,
                generation: _,
            } if !replay_locally => {
                self.outbound_messages
                    .push(ClientMessage::ServerOwnedInputEvents { events });
            }
            LinkInput::Events { events, generation } => {
                let mut last_local_mouse_position = None;
                let failed_at = self
                    .target
                    .as_ref()
                    .and_then(|target| target.runtime.as_ref())
                    .and_then(|runtime| {
                        events.iter().enumerate().find_map(|(index, event)| {
                            let raw = event.to_raw_input_event();
                            let local_mouse_cell = match &raw {
                                crate::raw_input::RawInputEvent::Mouse(mouse) => pane.map(|pane| {
                                    pane_local_cell(pane, mouse.column, mouse.row).unwrap_or_else(
                                        || clamp_pane_local_cell(pane, mouse.column, mouse.row),
                                    )
                                }),
                                _ => None,
                            };
                            let sent = localize_raw_event(raw, local_mouse_cell, focused)
                                .is_some_and(|event| forward_local_event(runtime, event));
                            if sent {
                                last_local_mouse_position =
                                    local_mouse_cell.map(|(column, row)| {
                                        crate::input::mouse::Position::Cell { column, row }
                                    });
                            }
                            (!sent).then_some(index)
                        })
                    })
                    .or_else(|| runtime_missing.then_some(0));
                if self.mouse_gesture_local == Some(true) {
                    if let Some(position) = last_local_mouse_position {
                        self.local_mouse_position = Some(position);
                    }
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
                for (data, geometry) in inputs {
                    let message = self.server_pixel_input_message(data, geometry);
                    self.outbound_messages.push(message);
                }
            }
            LinkInput::Pixels { inputs, generation } => {
                let mut last_local_mouse_position = None;
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
                        let pane = pane?;
                        inputs
                            .iter()
                            .enumerate()
                            .find_map(|(index, (data, geometry))| {
                                let mouse = decode_local_pixel_mouse(data, *geometry, pane, size)
                                    .or_else(|| {
                                        decode_clamped_local_pixel_mouse(
                                            data, *geometry, pane, size,
                                        )
                                    });
                                let sent = mouse
                                    .is_some_and(|mouse| forward_local_pixel_mouse(runtime, mouse));
                                if sent {
                                    last_local_mouse_position = mouse.map(|mouse| mouse.position);
                                }
                                (!sent).then_some(index)
                            })
                    })
                    .or_else(|| runtime_missing.then_some(0));
                if self.mouse_gesture_local == Some(true) {
                    if let Some(position) = last_local_mouse_position {
                        self.local_mouse_position = Some(position);
                    }
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

    fn defer_link_input(&mut self, input: LinkInput) {
        match input {
            LinkInput::Events { events, generation } => {
                self.deferred_messages
                    .push(DeferredMessage::InputEvents { events, generation });
            }
            LinkInput::Pixels { inputs, generation } => {
                for (data, geometry) in inputs {
                    self.deferred_messages.push(DeferredMessage::InputPixels {
                        data,
                        geometry,
                        generation,
                    });
                }
            }
        }
    }

    fn drain_queued_link_inputs(&mut self) {
        let queued = std::mem::take(&mut self.queued_link_inputs);
        for input in queued {
            match input {
                LinkInput::Events { events, generation } => {
                    let raw_events = events
                        .into_iter()
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    let messages = self.route_input_inner(raw_events, generation, false);
                    self.outbound_messages.extend(messages);
                }
                LinkInput::Pixels { inputs, generation } => {
                    for (data, geometry) in inputs {
                        if let Some(message) =
                            self.route_pixel_input_inner(data, geometry, generation, false)
                        {
                            self.outbound_messages.push(message);
                        }
                    }
                }
            }
        }
    }
    fn drain_link_inputs_for_generation(&mut self, current_generation: u64) {
        self.retain_link_inputs_for_generation(current_generation);
        self.drain_queued_link_inputs();
    }

    fn begin_local_forward_fallback(&mut self) {
        self.prepare_surface_handoff();
        if let Some(target) = self.target.as_mut() {
            target.fail();
        }
        self.awaiting_fallback = true;
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

    fn release_pending_link_inputs_to_server(&mut self, current_input_generation: u64) {
        let mut inputs = Vec::with_capacity(self.queued_link_inputs.len() + 1);
        if let Some(input) = self.pending_link_input.take() {
            inputs.push(input);
        }
        inputs.extend(std::mem::take(&mut self.queued_link_inputs));
        self.clear_pending_link_activation();
        for input in inputs {
            self.defer_link_input(input);
        }
        self.release_deferred_to_server(current_input_generation);
    }

    fn begin_overlay_server_input(&mut self) {
        if !self.server_owned_input {
            self.server_overlay_forced_input = true;
            self.server_owned_input = true;
        }
    }

    fn track_overlay_server_key(&mut self, key: &crate::input::TerminalKey) {
        if !key.reports_event_types() {
            return;
        }
        match key.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                self.begin_overlay_server_input();
                self.server_overlay_keys.insert(key.identity(), key.clone());
            }
            KeyEventKind::Release => {
                self.server_overlay_keys.remove(&key.identity());
            }
        }
    }

    fn finish_local_key_leases(&mut self) {
        let keys = std::mem::take(&mut self.local_keys);
        let Some(runtime) = self
            .target
            .as_ref()
            .and_then(|target| target.runtime.as_ref())
        else {
            return;
        };
        let mut failed = false;
        for key in keys.into_values() {
            failed |= !forward_local_event(
                runtime,
                crate::raw_input::RawInputEvent::Key(key.with_kind(KeyEventKind::Release)),
            );
        }
        if failed {
            self.begin_local_forward_fallback();
        }
    }

    fn finish_overlay_server_key_leases(&mut self) -> Vec<ClientInputEvent> {
        std::mem::take(&mut self.server_overlay_keys)
            .into_values()
            .filter_map(|key| {
                client_event_from_raw(&crate::raw_input::RawInputEvent::Key(
                    key.with_kind(KeyEventKind::Release),
                ))
            })
            .collect()
    }

    fn finish_input_leases_on_focus_lost(&mut self) -> Vec<ClientInputEvent> {
        self.finish_local_key_leases();
        self.finish_local_mouse_gesture();
        let mut releases = self.finish_overlay_server_key_leases();
        releases.extend(self.take_server_mouse_gesture_releases());
        self.server_overlay_forced_input = false;
        self.server_owned_input = false;
        releases
    }

    fn release_overlay_input_if_idle(&mut self, current_input_generation: u64) {
        if self.server_owned_frame
            || self.mouse_gesture_local == Some(false)
            || !self.server_overlay_keys.is_empty()
            || !self.server_overlay_forced_input
        {
            return;
        }
        self.server_overlay_forced_input = false;
        self.server_owned_input = false;
        if let Some(size) = self.target.as_ref().map(|target| target.size) {
            self.sync_target_to_projection(size, current_input_generation);
        }
        self.refresh_selection(false);
        self.sync_effective_focus();
    }

    fn settle_local_gesture_before_stale_deferred_prune(&mut self, current_generation: u64) {
        let stale_mouse = self.deferred_messages.iter().any(|message| match message {
            DeferredMessage::InputEvents { events, generation } => {
                *generation != current_generation
                    && events
                        .iter()
                        .any(|event| matches!(event, ClientInputEvent::Mouse { .. }))
            }
            DeferredMessage::InputPixels { generation, .. } => *generation != current_generation,
        });
        if stale_mouse {
            self.finish_local_mouse_gesture();
        }
    }

    pub(super) fn take_effects(&mut self) -> Vec<LocalEffect> {
        std::mem::take(&mut self.effects)
    }

    fn release_deferred_to_server(&mut self, current_input_generation: u64) {
        self.settle_local_gesture_before_stale_deferred_prune(current_input_generation);
        let deferred = std::mem::take(&mut self.deferred_messages);
        for mut message in deferred {
            if !message.retain_for_generation(current_input_generation) {
                continue;
            }
            let message = match message {
                DeferredMessage::InputEvents { events, .. } => {
                    ClientMessage::ServerOwnedInputEvents { events }
                }
                DeferredMessage::InputPixels { data, geometry, .. } => {
                    self.server_pixel_input_message(data, geometry)
                }
            };
            self.outbound_messages.push(message);
        }
    }

    fn resolve_promotion(&mut self, local_active: bool, current_input_generation: u64) {
        self.awaiting_promotion = None;
        if !local_active {
            self.release_deferred_to_server(current_input_generation);
            return;
        }
        let deferred = std::mem::take(&mut self.deferred_messages);
        for mut message in deferred {
            if !message.retain_for_generation(current_input_generation) {
                continue;
            }
            if self.awaiting_fallback {
                self.deferred_messages.push(message);
                continue;
            }
            match message {
                DeferredMessage::InputEvents { events, .. } => {
                    let events = events
                        .into_iter()
                        .map(|event| event.to_raw_input_event())
                        .collect();
                    let messages = self.route_input_inner(events, current_input_generation, false);
                    self.outbound_messages.extend(messages);
                }
                DeferredMessage::InputPixels { data, geometry, .. } => {
                    if let Some(message) = self.route_pixel_input_inner(
                        data,
                        geometry,
                        current_input_generation,
                        false,
                    ) {
                        self.outbound_messages.push(message);
                    }
                }
            }
        }
    }

    fn release_deferred_messages(&mut self, current_input_generation: u64) {
        if self.awaiting_fallback || self.awaiting_promotion.is_some() {
            self.awaiting_fallback = false;
            self.awaiting_promotion = None;
            self.release_deferred_to_server(current_input_generation);
        }
    }

    fn discard_deferred_messages(&mut self) {
        self.awaiting_fallback = false;
        self.awaiting_promotion = None;
        self.deferred_messages.clear();
        self.outbound_messages.clear();
        self.effects.clear();
    }

    fn stop_target(&mut self, preserve_server_gesture: bool) {
        self.finish_local_key_leases();
        self.finish_local_mouse_gesture();
        let server_gesture = preserve_server_gesture && self.mouse_gesture_local == Some(false);
        if let Some(mut target) = self.target.take() {
            target.stop();
        }
        if self.local_selected || self.server_owned_input {
            self.force_repaint = true;
        }
        self.local_selected = false;
        self.server_owned_input = server_gesture;
        if !server_gesture {
            self.end_mouse_gesture();
        }
        self.clear_pending_link_click();
        self.hovered_link_cells = None;
        self.needs_render = true;
    }
}

fn strip_renderer_metadata(mut frame: FrameData) -> FrameData {
    frame.omp_renderer = None;
    frame
}

fn compose_local_pane(
    mut base: FrameData,
    local: FrameData,
    pane: OmpRendererPane,
    focused: bool,
) -> Option<FrameData> {
    let base_len = usize::from(base.width).checked_mul(usize::from(base.height))?;
    let local_len = usize::from(local.width).checked_mul(usize::from(local.height))?;
    if base.cells.len() != base_len
        || local.cells.len() != local_len
        || local.width != pane.width
        || local.height != pane.height
        || pane.width == 0
        || pane.height == 0
        || pane.x.checked_add(pane.width)? > base.width
        || pane.y.checked_add(pane.height)? > base.height
    {
        return None;
    }
    let hyperlink_offset = u32::try_from(base.hyperlinks.len()).ok()?;
    for cell in &local.cells {
        if let Some(index) = cell.hyperlink {
            let index = usize::try_from(index).ok()?;
            if index >= local.hyperlinks.len() {
                return None;
            }
            hyperlink_offset.checked_add(u32::try_from(index).ok()?)?;
        }
    }
    base.hyperlinks.extend(local.hyperlinks);
    for local_y in 0..pane.height {
        for local_x in 0..pane.width {
            let local_index = usize::from(local_y) * usize::from(pane.width) + usize::from(local_x);
            let base_index = usize::from(pane.y + local_y) * usize::from(base.width)
                + usize::from(pane.x + local_x);
            let mut cell = local.cells[local_index].clone();
            if let Some(index) = cell.hyperlink {
                cell.hyperlink = Some(hyperlink_offset.checked_add(index)?);
            }
            base.cells[base_index] = cell;
        }
    }
    if focused {
        base.cursor = match local.cursor {
            Some(cursor) => Some(crate::protocol::CursorState {
                x: pane.x.checked_add(cursor.x)?,
                y: pane.y.checked_add(cursor.y)?,
                visible: cursor.visible,
                shape: cursor.shape,
            }),
            None => None,
        };
    }
    base.omp_renderer = None;
    Some(base)
}

fn pixel_input_message(
    data: Vec<u8>,
    geometry: crate::input::mouse::HostGeometry,
) -> ClientMessage {
    ClientMessage::ServerOwnedInputPixels {
        data,
        cols: geometry.cols,
        rows: geometry.rows,
        width_px: geometry.width_px,
        height_px: geometry.height_px,
    }
}

impl Drop for ClientOmpRenderer {
    fn drop(&mut self) {
        self.stop_target(false);
    }
}

fn valid_pane(pane: OmpRendererPane) -> bool {
    pane.width > 0 && pane.height > 0
}

fn pane_local_cell(pane: OmpRendererPane, column: u16, row: u16) -> Option<(u16, u16)> {
    if column < pane.x
        || row < pane.y
        || column >= pane.x.checked_add(pane.width)?
        || row >= pane.y.checked_add(pane.height)?
    {
        return None;
    }
    Some((column - pane.x, row - pane.y))
}

fn clamp_pane_local_cell(pane: OmpRendererPane, column: u16, row: u16) -> (u16, u16) {
    (
        column
            .saturating_sub(pane.x)
            .min(pane.width.saturating_sub(1)),
        row.saturating_sub(pane.y)
            .min(pane.height.saturating_sub(1)),
    )
}

fn localize_raw_event(
    event: crate::raw_input::RawInputEvent,
    local_mouse_cell: Option<(u16, u16)>,
    focused: bool,
) -> Option<crate::raw_input::RawInputEvent> {
    match event {
        crate::raw_input::RawInputEvent::Mouse(mut mouse) => {
            let (column, row) = local_mouse_cell?;
            mouse.column = column;
            mouse.row = row;
            Some(crate::raw_input::RawInputEvent::Mouse(mouse))
        }
        event if focused => Some(event),
        _ => None,
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
    pane: OmpRendererPane,
    size: (u16, u16, u32, u32),
) -> Option<crate::input::mouse::Position> {
    let (_, _, cell_width_px, cell_height_px) = size;
    let child_width_px = u32::from(pane.width).checked_mul(cell_width_px)?;
    let child_height_px = u32::from(pane.height).checked_mul(cell_height_px)?;
    let crate::input::mouse::Position::Pixels { x, y } = pointer.pane_position(
        Rect::new(pane.x, pane.y, pane.width, pane.height),
        child_width_px,
        child_height_px,
    )?
    else {
        return None;
    };
    let (column, row) = local_pixel_cell(pointer, pane, size)?;
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
    pane: OmpRendererPane,
    _size: (u16, u16, u32, u32),
) -> Option<(u16, u16)> {
    let (column, row) = pointer.geometry.cell(pointer.x, pointer.y)?;
    pane_local_cell(pane, column, row)
}

fn decode_local_pixel_mouse(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    pane: OmpRendererPane,
    size: (u16, u16, u32, u32),
) -> Option<LocalPixelMouse> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let mouse = decode_pixel_mouse(data)?;
    let pointer = crate::input::mouse::HostPixels { x, y, geometry };
    let position = local_pixel_position(pointer, pane, size)?;
    let cell = local_pixel_cell(pointer, pane, size)?;
    Some(LocalPixelMouse {
        mouse,
        position,
        cell,
    })
}

fn decode_clamped_local_pixel_mouse(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
    pane: OmpRendererPane,
    size: (u16, u16, u32, u32),
) -> Option<LocalPixelMouse> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let mouse = decode_pixel_mouse(data)?;
    let (column, row) = geometry.cell(x, y)?;
    let cell = clamp_pane_local_cell(pane, column, row);
    let (_, _, cell_width_px, cell_height_px) = size;
    Some(LocalPixelMouse {
        mouse,
        position: crate::input::mouse::Position::Pixels {
            x: u32::from(cell.0)
                .checked_mul(cell_width_px)?
                .checked_add(1)?,
            y: u32::from(cell.1)
                .checked_mul(cell_height_px)?
                .checked_add(1)?,
        },
        cell,
    })
}

fn forward_local_mouse_release(
    runtime: &TerminalRuntime,
    button: MouseButton,
    position: crate::input::mouse::Position,
    modifiers: crossterm::event::KeyModifiers,
) -> bool {
    encode_local_mouse(runtime, MouseEventKind::Up(button), position, modifiers)
        .is_none_or(|bytes| bytes.is_empty() || runtime.try_send_bytes(Bytes::from(bytes)).is_ok())
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
        let (rows, cols) = runtime.current_size();
        let pane = OmpRendererPane {
            x: 0,
            y: 0,
            width: cols,
            height: rows,
            scrollback_limit_bytes: 8 * 1024 * 1024,
        };
        let route = OmpRendererRoute {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let mut renderer = ClientOmpRenderer::new(Some(test_omp_executable()));
        renderer.set_server_sgr_pixels_active(true);
        renderer.latest_launch_id = 1;
        renderer.attempted_launches.insert(1);
        renderer.local_selected = true;
        renderer.offer = Some(TargetOffer {
            launch_id: 1,
            authority_revision: 1,
            target_app_client_id: 2,
            route: route.clone(),
            bound: true,
            surface_active: true,
            prefix: prefix.clone(),
        });
        renderer.projection = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, cols, rows));
        renderer.cached_server_frame = Some(FrameData::from_ratatui_buffer(&buffer, None));
        renderer.target = Some(LocalTarget {
            launch_id: 1,
            prefix,
            pane,
            runtime: Some(runtime),
            pane_id,
            events,
            render_dirty: Arc::new(RenderSignal::new()),
            size: (cols, rows, 10, 20),
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

    fn accept_projection(
        renderer: &mut ClientOmpRenderer,
        authority_revision: u64,
        size: (u16, u16, u32, u32),
    ) {
        let pane = renderer.target.as_ref().expect("native target").pane;
        renderer.projection = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision,
            frame_nonce: [authority_revision as u8; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        renderer.sync_target_to_projection(size, 0);
    }

    fn roundtrip_server_message(
        message: crate::protocol::ServerMessage,
    ) -> crate::protocol::ServerMessage {
        let bytes = bincode::serde::encode_to_vec(&message, bincode::config::standard()).unwrap();
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .unwrap()
            .0
    }

    fn roundtrip_client_message(message: ClientMessage) -> ClientMessage {
        let bytes = bincode::serde::encode_to_vec(&message, bincode::config::standard()).unwrap();
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .unwrap()
            .0
    }
    fn test_cell(symbol: &str, hyperlink: Option<u32>) -> crate::protocol::CellData {
        crate::protocol::CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink,
        }
    }

    #[test]
    fn pane_composition_preserves_chrome_siblings_links_graphics_and_cursor() {
        let base = FrameData {
            cells: vec![test_cell("B", None); 15],
            width: 5,
            height: 3,
            cursor: Some(crate::protocol::CursorState {
                x: 4,
                y: 2,
                visible: true,
                shape: 2,
            }),
            hyperlinks: vec!["https://sibling".into()],
            graphics: vec![7, 8, 9],
            omp_renderer: Some(OmpRendererFrame {
                launch_id: 1,
                authority_revision: 1,
                frame_nonce: [1; 16],
                pane: None,
                focused: true,
                server_owned_overlay: false,
                surface_active: true,
            }),
        };
        let mut base = base;
        base.cells[4].hyperlink = Some(0);
        let local = FrameData {
            cells: vec![test_cell("L", Some(0)), test_cell("R", None)],
            width: 2,
            height: 1,
            cursor: Some(crate::protocol::CursorState {
                x: 1,
                y: 0,
                visible: true,
                shape: 6,
            }),
            hyperlinks: vec!["artifact://local".into()],
            graphics: Vec::new(),
            omp_renderer: None,
        };
        let pane = OmpRendererPane {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
            scrollback_limit_bytes: 1024,
        };

        let composed = compose_local_pane(base.clone(), local.clone(), pane, true).unwrap();

        assert_eq!(composed.cells[0].symbol, "B");
        assert_eq!(composed.cells[4].hyperlink, Some(0));
        assert_eq!(composed.cells[6].symbol, "L");
        assert_eq!(composed.cells[6].hyperlink, Some(1));
        assert_eq!(composed.cells[7].symbol, "R");
        assert_eq!(
            composed.hyperlinks,
            vec!["https://sibling", "artifact://local"]
        );
        assert_eq!(composed.graphics, vec![7, 8, 9]);
        assert_eq!(
            composed.cursor,
            Some(crate::protocol::CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 6,
            })
        );
        assert!(composed.omp_renderer.is_none());

        let unfocused = compose_local_pane(base.clone(), local, pane, false).unwrap();
        assert_eq!(unfocused.cursor, base.cursor);
    }

    #[tokio::test]
    async fn server_owned_overlay_suspends_local_composition_and_input_until_clear() {
        let (runtime, mut local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006hLR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        assert!(local_input.try_recv().is_ok());
        assert_eq!(renderer.mouse_gesture_local, Some(true));
        let pane = OmpRendererPane {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
            scrollback_limit_bytes: 1024,
        };
        let mut frame = FrameData {
            cells: vec![test_cell("S", None); 8],
            width: 4,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            omp_renderer: Some(OmpRendererFrame {
                launch_id: 1,
                authority_revision: 1,
                frame_nonce: [1; 16],
                pane: Some(pane),
                focused: true,
                server_owned_overlay: true,
                surface_active: true,
            }),
        };

        let overlay = renderer
            .cache_server_frame(frame.clone(), (10, 20), 0)
            .expect("server overlay frame");
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1m");
        assert_eq!(renderer.mouse_gesture_local, None);
        assert_eq!(overlay.frame.cells[5].symbol, "S");
        assert_eq!(overlay.frame.cells[6].symbol, "S");
        assert!(!renderer.local_selected);
        assert!(renderer.owns_input());
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
                )])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { .. }]
        ));
        let geometry = crate::input::mouse::HostGeometry::new(4, 2, 40, 40).unwrap();
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;16;31M".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        assert!(local_input.try_recv().is_err());

        frame.omp_renderer.as_mut().unwrap().server_owned_overlay = false;
        let resumed = renderer
            .cache_server_frame(frame.clone(), (10, 20), 0)
            .expect("resumed server frame while pixel gesture is held");
        assert!(!renderer.local_selected);
        assert_eq!(resumed.frame.cells[5].symbol, "S");
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;16;31m".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        assert!(renderer.local_selected);
        let composed = renderer
            .cache_server_frame(frame.clone(), (10, 20), 0)
            .expect("resumed local frame");
        assert_eq!(composed.frame.cells[5].symbol, "L");
        assert_eq!(composed.frame.cells[6].symbol, "R");

        renderer.server_owned_input = true;
        let server_mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        renderer.begin_mouse_gesture(&server_mouse, false);
        renderer.record_server_mouse_cell(&server_mouse);
        frame.omp_renderer.as_mut().unwrap().server_owned_overlay = true;
        renderer.cache_server_frame(frame, (10, 20), 0);
        assert!(renderer.server_owned_input);
        assert_eq!(renderer.mouse_gesture_local, Some(false));
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn overlay_key_release_stays_server_owned_after_overlay_clears() {
        let (runtime, mut local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let pane = renderer.projection.unwrap().pane.unwrap();
        let mut frame = FrameData {
            cells: vec![test_cell("S", None); 2],
            width: 2,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            omp_renderer: Some(OmpRendererFrame {
                launch_id: 1,
                authority_revision: 1,
                frame_nonce: [1; 16],
                pane: Some(pane),
                focused: true,
                server_owned_overlay: true,
                surface_active: true,
            }),
        };
        renderer.cache_server_frame(frame.clone(), (10, 20), 0);
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(press.clone())])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { .. }]
        ));
        assert!(renderer.server_overlay_keys.contains_key(&press.identity()));

        frame.omp_renderer.as_mut().unwrap().server_owned_overlay = false;
        renderer.cache_server_frame(frame.clone(), (10, 20), 0);
        assert!(!renderer.local_selected);
        let release = press.with_kind(KeyEventKind::Release);
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(release)])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { .. }]
        ));
        assert!(renderer.server_overlay_keys.is_empty());
        assert!(renderer.local_selected);
        assert!(local_input.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_release_aware_key_stays_local_until_release_across_overlay() {
        let (runtime, mut local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let pane = renderer.projection.unwrap().pane.unwrap();
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(press.clone())])
            .is_empty());
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"x");
        assert!(renderer.local_keys.contains_key(&press.identity()));

        let overlay = FrameData {
            cells: vec![test_cell("S", None); 2],
            width: 2,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            omp_renderer: Some(OmpRendererFrame {
                launch_id: 1,
                authority_revision: 1,
                frame_nonce: [1; 16],
                pane: Some(pane),
                focused: true,
                server_owned_overlay: true,
                surface_active: true,
            }),
        };
        renderer.cache_server_frame(overlay, (10, 20), 0);

        let repeat = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_kind(KeyEventKind::Repeat)
            .with_vt_bytes(b"\x1b[120;1:2u".to_vec());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(repeat)])
            .is_empty());
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"x");

        let release = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_kind(KeyEventKind::Release)
            .with_vt_bytes(b"\x1b[120;1:3u".to_vec());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(release)])
            .is_empty());
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[120;1:3u");
        assert!(renderer.local_keys.is_empty());

        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty())
                )])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { .. }]
        ));
    }

    #[tokio::test]
    async fn focus_loss_releases_local_key_lease_before_target_blur() {
        let (runtime, mut local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        runtime.test_process_pty_bytes(b"\x1b[>3u");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(press.clone())])
            .is_empty());
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"x");
        assert!(renderer.local_keys.contains_key(&press.identity()));

        renderer.observe_outer_focus(&[crate::raw_input::RawInputEvent::OuterFocusLost]);

        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[120;1:3u");
        assert!(renderer.local_keys.is_empty());
    }

    #[tokio::test]
    async fn focus_loss_retires_server_overlay_key_lease() {
        let (runtime, _local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let pane = renderer.projection.unwrap().pane.unwrap();
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.omp_renderer = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: true,
            surface_active: true,
        });
        renderer.cache_server_frame(frame, (10, 20), 0);
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(press.clone())])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { .. }]
        ));

        renderer.observe_outer_focus(&[crate::raw_input::RawInputEvent::OuterFocusLost]);

        assert!(renderer.server_overlay_keys.is_empty());
        assert!(!renderer.server_overlay_forced_input);
        assert!(!renderer.server_owned_input);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Key { kind: ClientKeyKind::Release, .. }])
        ));
    }

    #[tokio::test]
    async fn focus_loss_finishes_local_leases_in_batch_order() {
        let (runtime, mut local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h\x1b[>3uLR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());

        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(press),
                crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                }),
                crate::raw_input::RawInputEvent::OuterFocusLost,
            ])
            .is_empty());

        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"x");
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1M");
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[120;1:3u");
        assert_eq!(local_input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1m");
        assert!(local_input.try_recv().is_err());
        assert!(renderer.local_keys.is_empty());
        assert!(renderer.local_mouse_buttons.is_empty());
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(!renderer.outer_focused);
    }

    #[tokio::test]
    async fn focus_loss_finishes_server_overlay_leases_in_batch_order() {
        let (runtime, _local_input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let pane = renderer.projection.unwrap().pane.unwrap();
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.omp_renderer = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: true,
            surface_active: true,
        });
        renderer.cache_server_frame(frame, (10, 20), 0);
        let _ = renderer.take_outbound_messages();
        let press = crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_vt_bytes(b"\x1b[120;1:1u".to_vec());

        let messages = renderer.route_input(vec![
            crate::raw_input::RawInputEvent::Key(press),
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::empty(),
            }),
            crate::raw_input::RawInputEvent::OuterFocusLost,
        ]);

        assert!(matches!(
            messages.as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(
                    events.as_slice(),
                    [
                        ClientInputEvent::Key { kind: ClientKeyKind::Press, .. },
                        ClientInputEvent::Mouse { kind: ClientMouseKind::Down(_), .. },
                        ClientInputEvent::Key { kind: ClientKeyKind::Release, .. },
                        ClientInputEvent::Mouse { kind: ClientMouseKind::Up(_), .. },
                        ClientInputEvent::FocusLost,
                    ]
                )
        ));
        assert!(renderer.server_overlay_keys.is_empty());
        assert!(renderer.server_mouse_buttons.is_empty());
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(!renderer.server_overlay_forced_input);
        assert!(!renderer.server_owned_input);
        assert!(!renderer.outer_focused);
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn projectionless_frame_does_not_claim_client_local_input_bridges() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.omp_renderer = None;

        assert!(renderer.cache_server_frame(frame, (10, 20), 0).is_some());
        assert!(!renderer.server_owned_frame);
        assert!(!renderer.local_selected);
        assert!(!renderer.owns_input());
    }

    #[test]
    fn pane_composition_rejects_out_of_bounds_projection() {
        let base = FrameData {
            cells: vec![test_cell("B", None); 4],
            width: 2,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            omp_renderer: None,
        };
        let local = FrameData {
            cells: vec![test_cell("L", None); 2],
            width: 2,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            omp_renderer: None,
        };
        let pane = OmpRendererPane {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
            scrollback_limit_bytes: 1024,
        };

        assert!(compose_local_pane(base, local, pane, true).is_none());
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

        renderer.apply_target(1, 2, 2, None, false, true, test_prefix(), (80, 24, 0, 0), 0);

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
            2,
            Some(OmpRendererRoute {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 1,
            }),
            false,
            true,
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
                ClientMessage::ServerOwnedInputEvents { events: prefix },
                ClientMessage::ServerOwnedInputEvents { events: command }
            ] if prefix.len() == 1 && command.len() == 1
        ));
        assert!(renderer.server_owned_input);
        assert!(renderer.local_selected);
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
    async fn early_rejected_native_link_latches_the_remaining_gesture_locally() {
        let url = "artifact://native-early-reject";
        let screen = format!("\x1b[?1003h\x1b[?1006h\x1b]8;;{url}\x1b\\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert!(matches!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 0)])
                .as_slice(),
            [ClientMessage::ActivateOmpLink { .. }]
        ));
        renderer.resolve_link_activation(1, 1, false, 0);
        assert_eq!(renderer.mouse_gesture_local, Some(true));
        assert!(renderer
            .route_input(vec![
                mouse(MouseEventKind::Drag(MouseButton::Left), 79),
                mouse(MouseEventKind::Up(MouseButton::Left), 79),
            ])
            .is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1M");
        assert!(
            input.try_recv().is_ok(),
            "drag remains with the local renderer"
        );
        assert!(
            input.try_recv().is_ok(),
            "release remains with the local renderer"
        );
        assert!(renderer.take_outbound_messages().is_empty());
        assert_eq!(renderer.mouse_gesture_local, None);
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
    async fn pixel_mouse_maps_host_pixels_to_the_local_pty() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(40, 12);
        runtime.resize(12, 40, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (40, 12, 10, 20);
        let data = b"\x1b[<35;321;121M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();

        assert!(renderer.route_pixel_input(data, geometry, 0).is_none());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<35;321;121M");
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
        assert_eq!(geometry.cell(11, 1), Some((1, 0)));

        let message = renderer
            .route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0)
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
        let pane = OmpRendererPane {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
            scrollback_limit_bytes: 8 * 1024 * 1024,
        };

        assert_eq!(local_pixel_cell(pointer, pane, size), Some((1, 0)));
        assert_eq!(
            local_pixel_position(pointer, pane, size),
            Some(crate::input::mouse::Position::Pixels { x: 11, y: 1 })
        );
    }

    #[tokio::test]
    async fn resize_without_pixel_geometry_never_reuses_a_stale_cell_pointer() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        renderer.observe_pointer_cell(Some((1, 0)));
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        let _ = renderer.route_pixel_input(b"\x1b[<35;401;1M".to_vec(), geometry, 0);
        assert!(renderer.pointer_pixels.is_some());

        renderer.resize((80, 1, 0, 0), None, 0);

        assert_eq!(renderer.pointer_pixels, None);
        assert_eq!(renderer.pointer_host_cell, None);
        assert_eq!(renderer.pointer_cell, None);
        assert!(!renderer.pointer_in_pane);
        assert!(renderer.projection.is_some());
        assert_eq!(renderer.awaiting_promotion, None);
        assert_eq!(renderer.target.as_ref().unwrap().size, (80, 1, 10, 20));

        renderer.resize((80, 1, 10, 20), Some(geometry), 0);
        assert!(renderer.projection.is_some());
        assert_eq!(renderer.awaiting_promotion, None);
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
        let route = renderer.offer.as_ref().unwrap().route.clone();
        renderer.local_selected = false;
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 801, 20).unwrap();

        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<35;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        assert!(renderer.pointer_pixels.is_some());
        assert_eq!(renderer.pointer_cell, None);

        renderer.target = Some(target);
        renderer.apply_target(1, 2, 2, Some(route), true, true, prefix, (80, 1, 10, 20), 0);
        accept_projection(&mut renderer, 2, (80, 1, 10, 20));
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
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        assert_eq!(renderer.pointer_cell, Some((2, 0)));
    }

    #[tokio::test]
    async fn resize_waits_for_fresh_projection_before_remapping_retained_pointer() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 1, 10, 20);
        let old_geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<35;401;1M".to_vec(), old_geometry, 0)
            .is_none());
        assert_eq!(renderer.pointer_cell, Some((40, 0)));

        let new_geometry = crate::input::mouse::HostGeometry::new(100, 1, 1000, 20).unwrap();
        renderer.resize((100, 1, 10, 20), Some(new_geometry), 0);
        assert_eq!(renderer.pointer_cell, None);
        assert_eq!(renderer.projection, None);

        let offer = renderer.offer.as_ref().unwrap().clone();
        renderer.apply_target(
            offer.launch_id,
            2,
            offer.target_app_client_id,
            Some(offer.route),
            true,
            true,
            offer.prefix,
            (100, 1, 10, 20),
            0,
        );
        accept_projection(&mut renderer, 2, (100, 1, 10, 20));
        assert_eq!(renderer.pointer_cell, Some((40, 0)));

        renderer.observe_pointer_cell(Some((40, 0)));
        let raw_resize_geometry = crate::input::mouse::HostGeometry::new(160, 1, 800, 20).unwrap();
        renderer.resize((160, 1, 5, 20), Some(raw_resize_geometry), 0);
        assert_eq!(renderer.pointer_cell, None);
        assert_eq!(renderer.projection, None);

        let offer = renderer.offer.as_ref().unwrap().clone();
        renderer.apply_target(
            offer.launch_id,
            3,
            offer.target_app_client_id,
            Some(offer.route),
            true,
            true,
            offer.prefix,
            (160, 1, 5, 20),
            0,
        );
        accept_projection(&mut renderer, 3, (160, 1, 5, 20));
        assert_eq!(renderer.pointer_cell, Some((40, 0)));
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

        renderer.apply_target(2, 2, 2, None, false, true, prefix, (80, 1, 10, 20), 0);
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
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = true;
        target.promoted = false;
        renderer.local_selected = false;
        renderer.awaiting_promotion = Some(PromotionBarrier::AuthorityAfter(1));

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
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
        assert_eq!(renderer.native_link_active(), None);
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
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
        assert!(matches!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
        renderer.server_owned_input = false;

        let (runtime, _pixel_server_input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b]8;;file:///tmp/report.md?line=7\x1b\\report\x1b]8;;\x1b\\",
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 24, 10, 20);
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        renderer.server_owned_input = true;
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1m".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
        ));
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(
            b"\x1b[?1000h\x1b[?1006h\x1b]8;;file:///tmp/report.md?line=7\x1b\\report\x1b]8;;\x1b\\",
        );
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().size = (80, 24, 10, 20);
        renderer.server_owned_input = false;
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ActivateOmpLink {
                launch_id: 1,
                url,
                ..
            }) if url == "file:///tmp/report.md?line=7"
        ));
        renderer.apply_target(
            2,
            2,
            2,
            None,
            false,
            true,
            test_prefix(),
            (80, 24, 10, 20),
            0,
        );
        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;11;1m".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputPixels { .. })
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
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(1, 2, 2, Some(route), false, true, prefix, (80, 24, 0, 0), 0);
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
    }

    #[tokio::test]
    async fn native_child_death_releases_surface_after_server_fallback_confirmation() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, events, pane_id) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();

        events
            .try_send(AppEvent::PaneDied {
                pane_id,
                child_pid: None,
            })
            .unwrap();
        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_some());
        assert!(renderer.target.as_ref().unwrap().failed);
        assert!(!renderer.local_selected);
        assert!(renderer.awaiting_fallback);

        renderer.apply_target(1, 2, 2, Some(route), false, true, prefix, (80, 24, 0, 0), 0);

        assert!(!renderer.awaiting_fallback);
        assert!(renderer.target.as_ref().unwrap().fallback_confirmed);
    }

    #[tokio::test]
    async fn bound_false_confirmation_does_not_rearm_fallback_for_failed_ready_target() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(1, 2, 2, Some(route), false, true, prefix, (80, 24, 0, 0), 0);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));

        assert!(renderer.next_frame(Instant::now(), (80, 24)).is_some());
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
                )])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
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
        let route = renderer.offer.as_ref().unwrap().route.clone();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::OuterFocusGained])
            .is_empty());
        assert!(renderer.target.as_ref().unwrap().failed);
        assert!(renderer.awaiting_fallback);
        renderer.apply_target(1, 2, 2, Some(route), false, true, prefix, (80, 24, 0, 0), 0);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::FocusGained])
        ));
    }

    #[tokio::test]
    async fn promotion_replays_all_activation_window_input_on_exact_projection() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(24, 80, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererReady { launch_id: 1 }]
        ));
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::AuthorityAfter(1))
        );
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

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
        assert_eq!(renderer.awaiting_promotion, None);
        assert!(renderer.local_selected);
        assert!(renderer.owns_input());
        assert!(renderer.target.as_ref().unwrap().promoted);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererAuthorityAck {
                launch_id: 1,
                authority_revision: 2,
                frame_nonce: _,
            }]
        ));
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert!(
            input.try_recv().is_ok(),
            "deferred mouse down reaches the local renderer"
        );
        assert!(
            input.try_recv().is_ok(),
            "deferred mouse release reaches the local renderer"
        );
        assert!(
            input.try_recv().is_ok(),
            "deferred pixel input reaches the local renderer"
        );
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn ready_ignores_stale_frame_until_newer_authority_projection_arrives() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::AuthorityAfter(1))
        );
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererReady { launch_id: 1 }]
        ));
        let mut same_revision = renderer.cached_server_frame.clone().unwrap();
        same_revision.omp_renderer = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: renderer.target.as_ref().map(|target| target.pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        renderer.cache_server_frame(same_revision, (10, 20), 0);
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::AuthorityAfter(1))
        );
        assert!(!renderer.local_selected);
        assert!(renderer.take_outbound_messages().is_empty());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        let mut stale = renderer.cached_server_frame.clone().unwrap();
        stale.omp_renderer = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: renderer.target.as_ref().map(|target| target.pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: false,
        });
        renderer.cache_server_frame(stale, (10, 20), 0);
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::Frame(2))
        );
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
            )])
            .is_empty());

        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
        assert_eq!(renderer.awaiting_promotion, None);
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"y");
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererAuthorityAck {
                launch_id: 1,
                authority_revision: 2,
                frame_nonce: _,
            }]
        ));
    }

    #[tokio::test]
    async fn wire_activation_acknowledges_exact_frame_before_replayed_input() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 24, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let pane = renderer.target.as_ref().unwrap().pane;
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;

        renderer.next_frame(Instant::now(), (80, 24));
        let mut ready = renderer.take_outbound_messages();
        assert_eq!(ready.len(), 1);
        assert!(matches!(
            roundtrip_client_message(ready.remove(0)),
            ClientMessage::OmpRendererReady { launch_id: 1 }
        ));
        assert!(renderer
            .route_input(vec![
                crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Char('x'),
                    KeyModifiers::empty(),
                )),
                crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 60,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                }),
            ])
            .is_empty());

        let target = roundtrip_server_message(crate::protocol::ServerMessage::OmpRendererTarget {
            launch_id: 1,
            authority_revision: 2,
            target_app_client_id: 2,
            route: Some(route),
            bound: true,
            surface_active: true,
            prefix,
        });
        let crate::protocol::ServerMessage::OmpRendererTarget {
            launch_id,
            authority_revision,
            target_app_client_id,
            route,
            bound,
            surface_active,
            prefix,
        } = target
        else {
            panic!("expected renderer target");
        };
        renderer.apply_target(
            launch_id,
            authority_revision,
            target_app_client_id,
            route,
            bound,
            surface_active,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::Frame(2))
        );

        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.omp_renderer = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 2,
            frame_nonce: [2; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        let frame = roundtrip_server_message(crate::protocol::ServerMessage::Frame(frame));
        let crate::protocol::ServerMessage::Frame(frame) = frame else {
            panic!("expected renderer frame");
        };
        renderer.cache_server_frame(frame, (10, 20), 0);

        let mut outbound = renderer.take_outbound_messages();
        assert_eq!(outbound.len(), 2);
        assert!(matches!(
            roundtrip_client_message(outbound.remove(0)),
            ClientMessage::OmpRendererAuthorityAck {
                launch_id: 1,
                authority_revision: 2,
                frame_nonce,
            }
                if frame_nonce == [2; 16]
        ));
        assert!(matches!(
            roundtrip_client_message(outbound.remove(0)),
            ClientMessage::ServerOwnedInputEvents { events }
                if matches!(events.as_slice(), [ClientInputEvent::Mouse {
                    column: 60,
                    row: 0,
                    ..
                }])
        ));
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert!(input.try_recv().is_err());
    }

    #[tokio::test]
    async fn active_authority_gap_defers_input_until_its_exact_frame() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::Frame(2))
        );
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert!(input.try_recv().is_err());

        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
    }

    #[tokio::test]
    async fn newer_inactive_authority_drains_deferred_input_serverward_in_order() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.resize(24, 80, 10, 20);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h\x1b[?1016h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.set_server_sgr_pixels_active(false);
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
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
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0)
            .is_none());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
            )])
            .is_empty());

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            false,
            prefix,
            (80, 24, 10, 20),
            0,
        );

        assert_eq!(renderer.awaiting_promotion, None);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [
                ClientMessage::ServerOwnedInputEvents { events: first },
                ClientMessage::ServerOwnedInputEvents { events: second },
                ClientMessage::ServerOwnedInputEvents { events: third },
            ] if matches!(first.as_slice(), [
                ClientInputEvent::Key { code: ClientKeyCode::Char('x'), .. }
            ]) && matches!(second.as_slice(), [
                ClientInputEvent::Mouse { column: 1, row: 0, .. }
            ]) && matches!(third.as_slice(), [
                ClientInputEvent::Key { code: ClientKeyCode::Char('y'), .. }
            ])
        ));
    }

    #[tokio::test]
    async fn local_only_pixel_report_outside_the_pane_becomes_cell_mouse_input() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        renderer.set_server_sgr_pixels_active(false);
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();

        assert!(matches!(
            renderer.route_pixel_input(b"\x1b[<0;601;1M".to_vec(), geometry, 0),
            Some(ClientMessage::ServerOwnedInputEvents { events })
                if matches!(events.as_slice(), [ClientInputEvent::Mouse { column: 60, row: 0, .. }])
        ));
    }

    #[tokio::test]
    async fn synthesized_release_uses_last_forwarded_local_position_when_pointer_is_unmapped() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert!(renderer
            .route_input(vec![
                mouse(MouseEventKind::Down(MouseButton::Left), 1),
                mouse(MouseEventKind::Drag(MouseButton::Left), 60),
                crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Char('x'),
                    KeyModifiers::empty(),
                )),
            ])
            .is_empty());
        renderer.observe_pointer_cell(None);
        renderer.finish_local_mouse_gesture();

        assert!(input.try_recv().is_ok());
        assert!(input.try_recv().is_ok());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;40;1m");
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
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
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

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            1,
        );

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
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
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

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            1,
        );

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
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
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
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        assert_eq!(renderer.pointer_cell, None);
        assert_eq!(renderer.pointer_pixels, None);
    }

    #[tokio::test]
    async fn inactive_promotion_routes_buffered_input_and_forwards_effects() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, events, pane_id) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let target = renderer.target.as_mut().unwrap();
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

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
        assert!(renderer.target.as_ref().unwrap().promoted);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererAuthorityAck {
                launch_id: 1,
                authority_revision: 2,
                frame_nonce: _,
            }]
        ));
        assert_eq!(input.try_recv().unwrap().as_ref(), b"x");
        assert!(input.try_recv().is_err());
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
    async fn native_composition_retains_full_server_base_and_same_route_process() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(2, 1);
        runtime.test_process_pty_bytes(b"LR");
        let prefix = test_prefix();
        let (mut renderer, _events, pane_id) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let pane = OmpRendererPane {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
            scrollback_limit_bytes: 1024,
        };
        let frame = FrameData {
            cells: vec![test_cell("B", None); 8],
            width: 4,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: vec![1],
            omp_renderer: Some(OmpRendererFrame {
                launch_id: 1,
                authority_revision: 1,
                frame_nonce: [1; 16],
                pane: Some(pane),
                focused: true,
                server_owned_overlay: false,
                surface_active: true,
            }),
        };

        let composed = renderer
            .cache_server_frame(frame.clone(), (10, 20), 0)
            .expect("composed pane-local frame")
            .frame;
        assert_eq!((composed.width, composed.height), (4, 2));
        assert_eq!(composed.cells[0].symbol, "B");
        assert_eq!(composed.cells[5].symbol, "L");
        assert_eq!(composed.cells[6].symbol, "R");
        assert_eq!(composed.cells[7].symbol, "B");
        assert_eq!(composed.graphics, vec![1]);
        let mut expected_cached = frame;
        expected_cached.graphics.clear();
        assert_eq!(renderer.cached_server_frame, Some(expected_cached));

        renderer.apply_target(1, 2, 2, Some(route), true, true, prefix, (4, 2, 10, 20), 0);
        assert_eq!(
            renderer.target.as_ref().map(|target| target.pane_id),
            Some(pane_id)
        );
        assert!(renderer.cached_server_frame.is_some());
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
        renderer.set_server_sgr_pixels_active(false);
        let data = b"\x1b[<35;321;241M".to_vec();
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert!(renderer
            .route_pixel_input(data.clone(), geometry, 0)
            .is_none());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            false,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Mouse {
                    column: 32,
                    row: 12,
                    ..
                }])
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

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            false,
            true,
            prefix,
            (80, 24, 10, 20),
            1,
        );

        assert!(!renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());
    }

    #[tokio::test]
    async fn fallback_defers_the_complete_chronological_suffix() {
        let (runtime, input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
        drop(input);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let route = renderer.offer.as_ref().unwrap().route.clone();
        assert!(renderer
            .route_input_at_generation(
                vec![
                    crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                        KeyCode::Char('x'),
                        KeyModifiers::empty(),
                    )),
                    crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: 60,
                        row: 0,
                        modifiers: KeyModifiers::empty(),
                    }),
                    crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                        KeyCode::Char('y'),
                        KeyModifiers::empty(),
                    )),
                ],
                0,
            )
            .is_empty());
        assert!(renderer.awaiting_fallback);
        assert!(renderer.take_outbound_messages().is_empty());

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            false,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );

        assert!(!renderer.awaiting_fallback);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [
                    ClientInputEvent::Key { code: ClientKeyCode::Char('x'), .. },
                    ClientInputEvent::Mouse { column: 60, row: 0, .. },
                    ClientInputEvent::Key { code: ClientKeyCode::Char('y'), .. },
                ])
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
        renderer.apply_target(2, 2, 2, None, false, true, test_prefix(), (80, 24, 0, 0), 0);
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
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::AuthorityAfter(1))
        );
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
    #[tokio::test]
    async fn stale_projection_cannot_override_newer_target_authority() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();

        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            prefix,
            (80, 24, 10, 20),
            0,
        );
        renderer.projection = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 1,
            frame_nonce: [1; 16],
            pane: renderer.target.as_ref().map(|target| target.pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        renderer.sync_target_to_projection((80, 24, 10, 20), 0);
        assert!(!renderer.local_selected);

        accept_projection(&mut renderer, 2, (80, 24, 10, 20));
        assert!(renderer.local_selected);
        renderer.observe_pointer_cell(Some((1, 0)));
        assert_eq!(renderer.pointer_cell, Some((1, 0)));
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .x = 2;
        renderer.remap_pointer_cell();
        assert_eq!(renderer.pointer_cell, None);
        let mut zero_sized = renderer.cached_server_frame.clone().unwrap();
        let mut projection = renderer.projection.unwrap();
        projection.pane.as_mut().unwrap().width = 0;
        zero_sized.omp_renderer = Some(projection);
        renderer.cache_server_frame(zero_sized, (10, 20), 0);
        assert!(renderer.projection.is_none());
        assert!(!renderer.local_selected);
    }

    #[tokio::test]
    async fn outside_click_hands_following_input_to_the_server_and_latches_local_gestures() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let server = renderer.route_input(vec![
            mouse(MouseEventKind::Down(MouseButton::Left), 60),
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('x'),
                KeyModifiers::empty(),
            )),
        ]);
        assert!(matches!(
            server.as_slice(),
            [
                ClientMessage::ServerOwnedInputEvents { events: click },
                ClientMessage::ServerOwnedInputEvents { events: key }
            ] if click.len() == 1 && key.len() == 1
        ));
        assert!(input.try_recv().is_err());
        assert!(matches!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left), 60)])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
        assert_eq!(renderer.mouse_gesture_local, None);

        renderer.server_owned_input = false;
        renderer.local_selected = true;
        assert!(renderer
            .route_input(vec![
                mouse(MouseEventKind::Down(MouseButton::Left), 1),
                mouse(MouseEventKind::Drag(MouseButton::Left), 60),
                mouse(MouseEventKind::Up(MouseButton::Left), 60),
            ])
            .is_empty());
        assert!(input.try_recv().is_ok());
        assert_eq!(renderer.mouse_gesture_local, None);
    }

    #[tokio::test]
    async fn host_mouse_mode_survives_promotion_and_server_gesture_handoff() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let target = renderer.target.as_mut().unwrap();
        target.surface_active = false;
        target.ready_reported = false;
        target.promoted = false;
        renderer.local_selected = false;
        renderer.next_frame(Instant::now(), (80, 24));
        assert_eq!(renderer.local_mouse_mode(), (true, true));

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        renderer.begin_mouse_gesture(&mouse, false);
        assert_eq!(renderer.local_mouse_mode(), (true, true));
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Right),
                    column: 60,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
        assert_eq!(renderer.mouse_gesture_local, Some(false));
        assert_eq!(renderer.local_mouse_mode(), (true, true));
        renderer.stop_target(true);
        assert_eq!(renderer.local_mouse_mode(), (true, true));
        renderer.end_mouse_gesture();
        assert_eq!(renderer.local_mouse_mode(), (false, false));
    }

    #[tokio::test]
    async fn active_reoffer_after_outside_release_restores_local_ownership() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 60,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        let messages = renderer.route_input(vec![
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
            crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Char('x'),
                KeyModifiers::empty(),
            )),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [
                ClientMessage::ServerOwnedInputEvents { events: down },
                ClientMessage::ServerOwnedInputEvents { events: tail },
            ] if down.len() == 1 && matches!(tail.as_slice(), [
                ClientInputEvent::Mouse { kind: ClientMouseKind::Up(_), .. },
                ClientInputEvent::Key { code: ClientKeyCode::Char('x'), .. },
            ])
        ));
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.server_owned_input);

        renderer.apply_target(1, 2, 2, Some(route), true, true, prefix, (80, 1, 10, 20), 0);
        accept_projection(&mut renderer, 2, (80, 1, 10, 20));
        assert!(renderer.local_selected);
        assert!(!renderer.server_owned_input);
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererAuthorityAck {
                launch_id: 1,
                authority_revision: 2,
                frame_nonce: _,
            }]
        ));
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
            )])
            .is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"y");
    }

    #[tokio::test]
    async fn resize_defers_click_until_new_geometry_frame() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();

        renderer.resize((80, 1, 10, 20), Some(geometry), 1);
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::AuthorityAfter(1))
        );
        assert!(!renderer.local_selected);
        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 60,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })],
                1,
            )
            .is_empty());
        assert!(input.try_recv().is_err());

        renderer.apply_target(1, 2, 2, Some(route), true, true, prefix, (80, 1, 10, 20), 1);
        let mut pane = renderer.target.as_ref().unwrap().pane;
        pane.width = 80;
        renderer.projection = Some(OmpRendererFrame {
            launch_id: 1,
            authority_revision: 2,
            frame_nonce: [2; 16],
            pane: Some(pane),
            focused: true,
            server_owned_overlay: false,
            surface_active: true,
        });
        renderer.sync_target_to_projection((80, 1, 10, 20), 1);

        assert!(input.try_recv().is_ok());
        assert!(input.try_recv().is_err());
        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::OmpRendererAuthorityAck {
                authority_revision: 2,
                frame_nonce: _,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn revision_only_prefix_reload_preserves_physical_local_gesture() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 1)])
            .is_empty());
        assert!(input.try_recv().is_ok());
        let new_prefix = OmpRendererPrefix {
            code: ClientKeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL.bits(),
        };
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            new_prefix,
            (80, 1, 10, 20),
            0,
        );
        assert_eq!(renderer.mouse_gesture_local, Some(true));
        assert!(
            input.try_recv().is_err(),
            "revision update must not synthesize Up"
        );
        assert!(renderer
            .route_input(vec![
                mouse(MouseEventKind::Drag(MouseButton::Left), 60),
                mouse(MouseEventKind::Up(MouseButton::Left), 60),
            ])
            .is_empty());
        assert!(input.try_recv().is_err());

        accept_projection(&mut renderer, 2, (80, 1, 10, 20));
        assert!(input.try_recv().is_ok());
        assert!(input.try_recv().is_ok());
        assert!(input.try_recv().is_err());
        assert_eq!(renderer.mouse_gesture_local, None);
    }

    #[tokio::test]
    async fn full_pty_release_queue_retires_local_target() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel_capacity(80, 1, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            false,
            prefix,
            (80, 1, 10, 20),
            0,
        );

        let target = renderer.target.as_ref().unwrap();
        assert!(target.failed);
        assert!(target.runtime.is_none());
        assert!(renderer.awaiting_fallback);
        assert!(input.try_recv().is_ok(), "queued Down remains observable");
        assert!(
            input.try_recv().is_err(),
            "failed synthetic Up was not queued"
        );
    }

    #[tokio::test]
    async fn rejected_link_deactivation_synthesizes_release_at_replayed_position() {
        let url = "artifact://native-deactivate";
        let screen = format!("\x1b[?1000h\x1b[?1006h\x1b]8;;{url}\x1b\\x\x1b]8;;\x1b\\");
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(screen.as_bytes());
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let down = crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });

        assert!(matches!(
            renderer.route_input(vec![down]).as_slice(),
            [ClientMessage::ActivateOmpLink { .. }]
        ));
        renderer.resolve_link_activation(1, 1, false, 0);
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            false,
            prefix,
            (80, 1, 10, 20),
            0,
        );

        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1M");
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[<0;1;1m");
        assert!(matches!(
            renderer
                .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })])
                .as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }] if events.len() == 1
        ));
    }

    #[tokio::test]
    async fn multi_button_local_gesture_keeps_outside_down_with_existing_owner() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mouse = |kind, column| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left), 1)])
            .is_empty());
        assert_eq!(renderer.local_mouse_buttons.len(), 1);
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Right), 60)])
            .is_empty());
        assert_eq!(renderer.local_mouse_buttons.len(), 2);
        assert!(!renderer.server_owned_input);

        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left), 1)])
            .is_empty());
        assert_eq!(renderer.mouse_gesture_local, Some(true));
        assert_eq!(
            renderer.local_mouse_buttons.as_slice(),
            &[(MouseButton::Right, 0)]
        );

        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Drag(MouseButton::Right), 60)])
            .is_empty());
        assert_eq!(
            renderer.local_mouse_position,
            Some(crate::input::mouse::Position::Cell { column: 39, row: 0 })
        );
        assert!(renderer
            .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Right), 60)])
            .is_empty());
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.local_mouse_buttons.is_empty());
    }

    #[tokio::test]
    async fn multi_button_pixel_gesture_keeps_outside_down_with_existing_owner() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h\x1b[?1016h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();

        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;1M".to_vec(), geometry, 0)
            .is_none());
        assert!(renderer
            .route_pixel_input(b"\x1b[<2;601;1M".to_vec(), geometry, 0)
            .is_none());
        assert_eq!(renderer.local_mouse_buttons.len(), 2);
        assert_eq!(renderer.mouse_gesture_local, Some(true));
        assert!(!renderer.server_owned_input);

        assert!(renderer
            .route_pixel_input(b"\x1b[<0;11;1m".to_vec(), geometry, 0)
            .is_none());
        assert_eq!(
            renderer.local_mouse_buttons.as_slice(),
            &[(MouseButton::Right, 0)]
        );
        assert!(renderer
            .route_pixel_input(b"\x1b[<2;601;1m".to_vec(), geometry, 0)
            .is_none());
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.local_mouse_buttons.is_empty());
    }

    #[tokio::test]
    async fn raw_server_owned_gesture_waits_for_every_button_up() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let mouse = |kind| {
            crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind,
                column: 60,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })
        };

        assert_eq!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Left))])
                .len(),
            1
        );
        assert_eq!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Down(MouseButton::Right))])
                .len(),
            1
        );
        assert_eq!(renderer.server_mouse_buttons.len(), 2);

        assert_eq!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Left))])
                .len(),
            1
        );
        assert_eq!(renderer.mouse_gesture_local, Some(false));
        assert_eq!(
            renderer.server_mouse_buttons,
            HashSet::from([MouseButton::Right])
        );
        assert!(renderer.server_owned_input);

        assert_eq!(
            renderer
                .route_input(vec![mouse(MouseEventKind::Up(MouseButton::Right))])
                .len(),
            1
        );
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.server_mouse_buttons.is_empty());
        assert!(renderer.server_owned_input);
    }

    #[tokio::test]
    async fn pixel_server_owned_gesture_waits_for_every_button_up() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(40, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1002h\x1b[?1006h\x1b[?1016h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();

        assert!(renderer
            .route_pixel_input(b"\x1b[<0;601;1M".to_vec(), geometry, 0)
            .is_some());
        assert!(renderer
            .route_pixel_input(b"\x1b[<2;601;1M".to_vec(), geometry, 0)
            .is_some());
        assert_eq!(renderer.server_mouse_buttons.len(), 2);

        assert!(renderer
            .route_pixel_input(b"\x1b[<0;601;1m".to_vec(), geometry, 0)
            .is_some());
        assert_eq!(renderer.mouse_gesture_local, Some(false));
        assert_eq!(
            renderer.server_mouse_buttons,
            HashSet::from([MouseButton::Right])
        );
        assert!(renderer.server_owned_input);

        assert!(renderer
            .route_pixel_input(b"\x1b[<2;601;1m".to_vec(), geometry, 0)
            .is_some());
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.server_mouse_buttons.is_empty());
        assert!(renderer.server_owned_input);
    }

    #[tokio::test]
    async fn resize_synthesizes_active_local_button_release_before_generation_pruning() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());

        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());
        assert!(input.try_recv().is_ok(), "local Down reaches the child");
        let route = renderer.offer.as_ref().unwrap().route.clone();
        renderer.apply_target(
            1,
            2,
            2,
            Some(route),
            true,
            true,
            test_prefix(),
            (80, 1, 10, 20),
            0,
        );
        assert_eq!(
            renderer.awaiting_promotion,
            Some(PromotionBarrier::Frame(2))
        );
        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: 1,
                    row: 0,
                    modifiers: KeyModifiers::empty(),
                })],
                0,
            )
            .is_empty());
        assert!(input.try_recv().is_err(), "local Up is deferred");

        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        renderer.resize((80, 1, 10, 20), Some(geometry), 1);

        assert!(
            input.try_recv().is_ok(),
            "resize synthesizes the matching Up"
        );
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.local_mouse_buttons.is_empty());
        assert!(renderer.deferred_messages.is_empty());

        accept_projection(&mut renderer, 2, (80, 1, 10, 20));
        assert!(renderer.local_selected);
        assert!(input.try_recv().is_err(), "stale Up is not replayed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resize_settles_server_owned_gesture_before_stale_up_is_pruned() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let prefix = test_prefix();
        let (mut renderer, _events, _) = active_renderer(runtime, prefix.clone());
        renderer.target.as_mut().unwrap().pane.width = 40;
        renderer.target.as_mut().unwrap().size = (40, 1, 10, 20);
        renderer
            .projection
            .as_mut()
            .unwrap()
            .pane
            .as_mut()
            .unwrap()
            .width = 40;
        let route = renderer.offer.as_ref().unwrap().route.clone();
        let down = crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });

        assert!(matches!(
            renderer.route_input_at_generation(vec![down], 0).as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Mouse {
                    kind: ClientMouseKind::Down(ClientMouseButton::Left),
                    column: 60,
                    row: 0,
                    modifiers: 0,
                }])
        ));
        assert_eq!(renderer.mouse_gesture_local, Some(false));
        assert!(renderer.server_owned_input);

        let geometry = crate::input::mouse::HostGeometry::new(80, 1, 800, 20).unwrap();
        renderer.resize((80, 1, 10, 20), Some(geometry), 1);

        assert!(matches!(
            renderer.take_outbound_messages().as_slice(),
            [ClientMessage::ServerOwnedInputEvents { events }]
                if matches!(events.as_slice(), [ClientInputEvent::Mouse {
                    kind: ClientMouseKind::Up(ClientMouseButton::Left),
                    column: 60,
                    row: 0,
                    modifiers: 0,
                }])
        ));
        let old_generation = super::super::HostInputSnapshot::from_parts(0, true, false);
        let current_generation = super::super::HostInputSnapshot::from_parts(1, true, false);
        assert!(
            !super::super::mouse_input_is_current(old_generation, current_generation, 1, true,),
            "old-generation Up must be discarded before renderer routing",
        );
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.server_mouse_buttons.is_empty());

        // The matching old-generation Up is rejected before it reaches the
        // renderer; the synthetic release lets the server reoffer ownership.
        renderer.apply_target(1, 2, 2, Some(route), true, true, prefix, (80, 1, 10, 20), 1);
        accept_projection(&mut renderer, 2, (80, 1, 10, 20));
        assert!(renderer.local_selected);
        assert!(!renderer.server_owned_input);
        assert!(renderer
            .route_input_at_generation(
                vec![crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('y'), KeyModifiers::empty()),
                )],
                1,
            )
            .is_empty());
        assert_eq!(input.try_recv().unwrap().as_ref(), b"y");
    }

    #[tokio::test]
    async fn resize_release_failure_retires_pressed_local_target() {
        let (runtime, _input) = TerminalRuntime::test_with_channel_capacity(80, 1, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        assert!(renderer
            .route_input(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 0,
                modifiers: KeyModifiers::empty(),
            })])
            .is_empty());

        let geometry = crate::input::mouse::HostGeometry::new(100, 1, 1000, 20).unwrap();
        renderer.resize((100, 1, 10, 20), Some(geometry), 1);

        assert!(renderer.target.as_ref().unwrap().failed);
        assert!(!renderer.local_selected);
        assert_eq!(renderer.mouse_gesture_local, None);
        assert!(renderer.local_mouse_buttons.is_empty());
        assert!(renderer
            .take_outbound_messages()
            .iter()
            .all(|message| !matches!(message, ClientMessage::OmpRendererAuthorityAck { .. })));
    }

    #[tokio::test]
    async fn client_only_repaint_replays_all_siblings_from_deferred_frames() {
        let (runtime, mut input) = TerminalRuntime::test_with_channel(80, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        renderer.sync_effective_focus();
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[I");

        let first = b"\x1b_Ga=t,t=d,f=32,s=1,v=1,i=10001,q=2,m=0;/wAA/w==\x1b\\\x1b[1;2H\x1b_Ga=p,i=10001,p=7,c=1,r=1,z=0,C=1,q=2;\x1b\\".to_vec();
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.graphics = first.clone();
        frame.omp_renderer = renderer.projection;
        let immediate = renderer
            .cache_server_frame(frame, (10, 20), 0)
            .expect("first sibling frame");
        assert_eq!(immediate.frame.graphics, first);

        let second = b"\x1b[1;10H\x1b_Ga=T,t=d,f=32,s=1,v=1,i=10002,p=8,c=1,r=1,z=1,C=1,q=2,m=0;AAAAAA==\x1b\\".to_vec();
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.graphics = second.clone();
        frame.omp_renderer = renderer.projection;
        let immediate = renderer
            .cache_server_frame(frame, (10, 20), 0)
            .expect("second sibling frame");
        assert_eq!(immediate.frame.graphics, second);

        renderer.needs_render = true;
        renderer.force_repaint = true;
        let repaint = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("client-only full repaint");
        assert!(repaint.force_repaint);
        let replay = String::from_utf8_lossy(&repaint.frame.graphics);
        assert!(replay.contains("a=p,i=10001,p=7"), "{replay:?}");
        assert!(replay.contains("a=p,i=10002,p=8"), "{replay:?}");
        assert!(!replay.contains("a=t"), "{replay:?}");
        assert!(!replay.contains("a=T"), "{replay:?}");

        renderer.projection.as_mut().unwrap().focused = false;
        renderer.refresh_selection(false);
        renderer.sync_effective_focus();
        assert_eq!(input.try_recv().unwrap().as_ref(), b"\x1b[O");
    }

    #[tokio::test]
    async fn graphics_only_delta_updates_replay_without_a_later_frame() {
        let (runtime, _input) = TerminalRuntime::test_with_channel(80, 1);
        let (mut renderer, _events, _) = active_renderer(runtime, test_prefix());
        let initial = b"\x1b_Ga=t,t=d,f=32,s=1,v=1,i=10001,q=2,m=0;/wAA/w==\x1b\\\x1b[1;2H\x1b_Ga=p,i=10001,p=7,c=1,r=1,z=0,C=1,q=2;\x1b\\".to_vec();
        let mut frame = renderer.cached_server_frame.clone().unwrap();
        frame.graphics = initial;
        frame.omp_renderer = renderer.projection;
        renderer.cache_server_frame(frame, (10, 20), 0).unwrap();

        renderer.cache_server_graphics(
            b"\x1b7\x1b_Ga=d,d=i,i=10001,p=7,q=2;\x1b\\\x1b[1;6H\x1b_Ga=p,i=10001,p=7,c=1,r=1,z=0,C=1,q=2;\x1b\\\x1b[1;10H\x1b_Ga=T,t=d,f=32,s=1,v=1,i=10002,p=8,c=1,r=1,z=1,C=1,q=2,m=0;AAAAAA==\x1b\\\x1b8",
            (80, 1, 10, 20),
        );
        renderer.needs_render = true;
        renderer.force_repaint = true;
        let repaint = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("graphics-only replay");
        let replay = String::from_utf8_lossy(&repaint.frame.graphics);
        assert!(replay.contains("\x1b[1;6H"), "{replay:?}");
        assert!(!replay.contains("\x1b[1;2H"), "{replay:?}");
        assert!(replay.contains("a=p,i=10001,p=7"), "{replay:?}");
        assert!(replay.contains("a=p,i=10002,p=8"), "{replay:?}");

        renderer.cache_server_graphics(
            b"\x1b7\x1b_Ga=d,d=i,i=10001,p=7,q=2;\x1b\\\x1b8",
            (80, 1, 10, 20),
        );
        renderer.needs_render = true;
        renderer.force_repaint = true;
        let repaint = renderer
            .next_frame(Instant::now(), (80, 1))
            .expect("graphics-only delete replay");
        let replay = String::from_utf8_lossy(&repaint.frame.graphics);
        assert!(!replay.contains("i=10001"), "{replay:?}");
        assert!(replay.contains("a=p,i=10002,p=8"), "{replay:?}");
    }
}
