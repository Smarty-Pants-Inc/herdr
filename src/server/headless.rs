//! Headless server mode — runs the herdr event loop without a real terminal.
//!
//! The server:
//! - Does not enter raw mode or read stdin
//! - Creates and listens on both `herdr.sock` (existing JSON API) and
//!   `herdr-client.sock` (new binary protocol)
//! - Initializes AppState and all PTYs from session restore or fresh state
//! - Runs the main event loop (drain events, drain API requests, scheduled tasks)
//! - Renders to a virtual ratatui Buffer in memory
//! - Accepts client connections on the client socket
//! - Streams frames to connected clients after each render
//! - Routes client input events through the existing input pipeline
//! - Continues running after client disconnect
//! - Handles stale socket cleanup, explicit server stop, minimum terminal size,
//!   and pane spawn failure during restore

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use interprocess::local_socket::traits::Listener as _;
#[cfg(windows)]
use interprocess::local_socket::traits::Stream as _;
#[cfg(unix)]
use interprocess::local_socket::ListenerNonblockingMode;
use ratatui::{layout::Rect, widgets::Widget};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use base64::Engine;
use bytes::Bytes;

use crate::api;
use crate::app;
use crate::config;
use crate::events::AppEvent;
#[cfg(test)]
use crate::ipc::bind_local_listener;
use crate::ipc::{
    bind_private_local_listener, remove_socket_file_if_owned, socket_file_identity, LocalListener,
    SocketFileIdentity,
};
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, CursorState, FrameData, OmpControlAction,
    OmpFrameDirection, OmpPaneState, ServerMessage, MAX_FRAME_SIZE,
};
#[cfg(unix)]
use crate::server::client_accept::{
    accept_pending_client_connections, reject_pending_client_connections,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::{
    events_include_interaction, latest_app_client, render_targets, terminal_stream_client_ids,
    ClientConnection, ClientConnectionMode, ClientNavigationState, DeferredRender,
    OmpRendererTargetState,
};
use crate::server::keybindings::{app_keybindings, apply_keybindings};
use crate::server::notifications::{
    should_forward_toast_to_clients, toast_message_from_state_change, toast_notify_kind,
};
use crate::server::omp_bridge;
use crate::server::omp_private_renderer::{
    PrivateOmpGuest, PrivateOmpGuestConfig, PrivateOmpGuestControl, PrivateOmpGuestRecord,
};
use crate::server::omp_route::OmpRouteKey;
use crate::server::omp_service::OmpService;
use crate::server::socket_paths::{
    client_socket_path, prepare_socket_path, restrict_socket_permissions,
};
use crate::server::terminal_attach::paste_payload_for_runtime;

mod pane_graphics;

use crate::protocol::MAX_GRAPHICS_FRAME_SIZE;
use pane_graphics::RetainedGraphicsOutcome;

#[cfg(test)]
use crate::protocol::RenderEncoding;
#[cfg(test)]
use crate::server::client_transport::ClientWriter;
#[cfg(test)]
use std::fs;

const LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(6);
const PRIVATE_OMP_COMPANION_RETRY_DELAY: Duration = Duration::from_secs(1);
const RETIRED_PRIVATE_PANE_ID_LIMIT: usize = 256;

const LIVE_HANDOFF_CLIENT_REASON: &str =
    "live update in progress; reconnect after handoff completes";

fn live_handoff_client_message() -> ServerMessage {
    ServerMessage::ServerHandoff {
        reason: LIVE_HANDOFF_CLIENT_REASON.to_owned(),
    }
}

fn wait_for_live_handoff_response_write(
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) {
    let Some(response_write_complete) = response_write_complete else {
        return;
    };

    match response_write_complete.recv_timeout(LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!("timed out waiting for live handoff response write; old server exiting");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!("live handoff response writer disconnected; old server exiting");
        }
    }
}

fn sound_notify_message(sound: crate::sound::Sound) -> &'static str {
    match sound {
        crate::sound::Sound::Done => "agent done",
        crate::sound::Sound::Request => "agent attention",
    }
}

fn notification_show_response_shown(response: &str) -> bool {
    let Ok(response) = serde_json::from_str::<api::schema::SuccessResponse>(response) else {
        return false;
    };
    matches!(
        response.result,
        api::schema::ResponseResult::NotificationShow { shown: true, .. }
    )
}

fn non_empty_body(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

// ---------------------------------------------------------------------------
// Loop event enum for the headless server event loop
// ---------------------------------------------------------------------------

/// Events that the headless server event loop can process.
enum LoopEvent {
    Timer,
    Internal(AppEvent),
    Api(Box<api::ApiRequestMessage>),
    ServerEvent(ServerEvent),
    RenderRequested,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum RenderImpact {
    #[default]
    None,
    Graphics,
    Full,
}

impl RenderImpact {
    fn merge(&mut self, other: Self) {
        *self = (*self).max(other);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PtyRenderState {
    Clean,
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RetainedRenderInput {
    needs_full_render: bool,
    needs_graphics_render: bool,
    pty: PtyRenderState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetainedRenderPlan {
    Full,
    Graphics,
    Pty,
    HiddenPty,
}

fn retained_render_plan(input: RetainedRenderInput) -> RetainedRenderPlan {
    if input.needs_full_render {
        RetainedRenderPlan::Full
    } else if input.needs_graphics_render && input.pty != PtyRenderState::Visible {
        RetainedRenderPlan::Graphics
    } else {
        match input.pty {
            PtyRenderState::Visible => RetainedRenderPlan::Pty,
            PtyRenderState::Hidden => RetainedRenderPlan::HiddenPty,
            PtyRenderState::Clean => RetainedRenderPlan::Full,
        }
    }
}

fn record_render_impact(source: &'static str, impact: RenderImpact) {
    let event = match (source, impact) {
        ("api_requests", RenderImpact::Graphics) => "graphics_render_cause.api_requests",
        ("api_requests", RenderImpact::Full) => "full_render_cause.api_requests",
        ("server_events", RenderImpact::Graphics) => "graphics_render_cause.server_events",
        ("server_events", RenderImpact::Full) => "full_render_cause.server_events",
        _ => return,
    };
    crate::render_prof::event(event);
}

fn rect_fits_frame(rect: Rect, frame: &FrameData) -> bool {
    rect.x.saturating_add(rect.width) <= frame.width
        && rect.y.saturating_add(rect.height) <= frame.height
}

fn apply_terminal_dirty_patch(
    frame: &mut FrameData,
    area: Rect,
    patch: crate::pane::TerminalDirtyPatch,
) -> bool {
    if !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width);
    for (local_y, row_cells) in patch.rows {
        if local_y >= area.height || row_cells.len() != usize::from(area.width) {
            return false;
        }
        let frame_y = area.y + local_y;
        let start = usize::from(frame_y) * width + usize::from(area.x);
        let end = start + usize::from(area.width);
        if end > frame.cells.len() {
            return false;
        }
        frame.cells[start..end].clone_from_slice(&row_cells);
    }
    true
}

fn dirty_patch_intersects_hyperlinks(
    frame: &FrameData,
    area: Rect,
    patch: &crate::pane::TerminalDirtyPatch,
) -> bool {
    if frame.hyperlinks.is_empty() || !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width);
    for (local_y, _) in &patch.rows {
        if *local_y >= area.height {
            return true;
        }
        let frame_y = area.y + *local_y;
        let start = usize::from(frame_y) * width + usize::from(area.x);
        let end = start + usize::from(area.width);
        if end > frame.cells.len() {
            return true;
        }
        if frame.cells[start..end]
            .iter()
            .any(|cell| cell.hyperlink.is_some())
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for in-flight API requests during shutdown.
#[allow(dead_code)]
const SHUTDOWN_API_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the idle headless loop wakes to poll the local listener for new
/// client connections.
///
/// The listener is non-blocking and not integrated into `tokio::select!`, so
/// a low-frequency wake is required to notice new thin-client attaches while
/// otherwise idle. Keep this much slower than the old resize-poll cadence to
/// avoid reintroducing the idle CPU spin.
const CLIENT_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PRIVATE_SURFACE_READY_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Headless server
// ---------------------------------------------------------------------------

struct AltScreenReadSpec {
    terminal_id: crate::terminal::TerminalId,
    lines: usize,
    unwrap: bool,
    initial: crate::terminal::ScreenSnapshot,
    content_seq: u64,
}

enum AltScreenReadConflict {
    None,
    Frozen(crate::pane::TerminalReadSnapshot),
    Defer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateOmpRetryState {
    Pending(u64),
    Consumed,
}
struct PendingPrivateSurfaceResponse {
    id: String,
    respond_to: std::sync::mpsc::Sender<String>,
}

struct PrivateSurfaceCandidate {
    surface: crate::server::private_surface::PrivateSurface,
    pending_response: Option<PendingPrivateSurfaceResponse>,
    deadline: Instant,
}

/// The headless server — runs the herdr event loop without a real terminal.
pub struct HeadlessServer {
    app: app::App,
    #[cfg(unix)]
    api_tx: Option<api::ApiRequestSender>,
    // Kept on every platform so dropping HeadlessServer owns API server shutdown.
    #[cfg_attr(windows, allow(dead_code))]
    api_server: Option<api::ServerHandle>,
    #[cfg(unix)]
    client_listener: LocalListener,
    client_socket_path: PathBuf,
    client_socket_identity: SocketFileIdentity,
    clients: HashMap<u64, ClientConnection>,
    /// Remote client-private popup launches staged until their helper confirms execution.
    private_surface_candidates: HashMap<u64, PrivateSurfaceCandidate>,
    #[cfg(test)]
    independent_omp_renderers_enabled: bool,
    /// Routes whose server-private guest failed; retain the normal pane fallback
    /// instead of immediately respawning a blank replacement for the same route.
    private_omp_failed_routes: HashMap<u64, OmpRouteKey>,
    /// Exact route and state of its single transient-failure retry.
    private_omp_retry_attempted_routes: HashMap<u64, (OmpRouteKey, PrivateOmpRetryState)>,
    next_private_omp_retry_id: u64,
    /// Routes waiting on the single private companion resolver; keep their host pane masked.
    private_omp_pending_routes: HashMap<u64, OmpRouteKey>,
    /// Last companion resolved and verified off the server event loop. Reverify before every reuse.
    private_omp_executable: Option<crate::update::OmpExecutable>,
    /// The route whose executable is being resolved off the server event loop.
    private_omp_resolving: Option<(u64, OmpRouteKey)>,
    /// Test harness override for synchronous private guest assertions.
    #[cfg(test)]
    private_omp_test_executable: Option<PathBuf>,
    /// Fresh server-owned launch identity for each App-local native renderer offer.
    next_omp_renderer_launch_id: u64,
    omp_service: OmpService,
    /// Recent private pane ids retained to consume late or duplicate actor events.
    retired_private_pane_ids: VecDeque<crate::layout::PaneId>,
    #[cfg(unix)]
    next_client_id: u64,
    /// The client currently driving the shared pane runtime size, theme, and input keybindings.
    foreground_client_id: Option<u64>,
    /// Outer window title last pushed, paired with the client that received it.
    /// Keying on the client means a newly attached terminal is written to even
    /// when the title itself has not changed, without every code path that
    /// changes the foreground client having to remember to invalidate this.
    sent_window_title: Option<(u64, Option<String>)>,
    /// Window title set through `client.window_title.set`. While present it wins
    /// over the configured `ui.window_title` until the API clears it again.
    api_window_title: Option<String>,
    /// Server-owned keybindings, restored when foreground clients use server mode.
    server_keybindings: crate::config::LiveKeybindConfig,
    /// Full server config warning shown to clients that use server keybindings.
    server_config_diagnostic: Option<String>,
    /// Server config warning with keybinding diagnostics removed for local-keybinding clients.
    server_config_diagnostic_without_keybindings: Option<String>,
    /// Writable direct attach owner per terminal id string.
    terminal_attach_owners: HashMap<String, u64>,
    /// Deferred application-history reads currently driving alternate-screen viewports.
    pending_alt_screen_reads: Vec<crate::server::alt_screen_read::PendingAltScreenRead>,
    /// Reads waiting for an alternate-screen traversal of the same terminal to finish.
    deferred_alt_screen_reads: Vec<api::ApiRequestMessage>,
    /// Monotonic activity counter used to pick the most recently active client.
    next_activity_stamp: u64,
    /// Configured virtual terminal size used when no clients are connected.
    headless_size: (u16, u16),
    /// Shared pane runtime size derived from the foreground client, or the
    /// configured headless size when no clients are connected.
    effective_size: (u16, u16),
    /// Flag set when shutdown is initiated.
    shutting_down: bool,
    /// Flag set while exporting live PTYs to a replacement server.
    handoff_in_progress: bool,
    /// Imported panes get one app-safe resize nudge after the first client attaches.
    #[cfg(unix)]
    pending_handoff_repaint_nudge: bool,
    /// Flag set by Ctrl+C or `server stop` signal.
    should_quit: Arc<AtomicBool>,
    /// Channel for receiving server events from client connection threads.
    server_event_rx: mpsc::Receiver<ServerEvent>,
    /// Sender for server events (cloned for each client thread).
    server_event_tx: mpsc::Sender<ServerEvent>,
}

fn apply_terminal_attach_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    column: Option<u16>,
    row: Option<u16>,
    modifiers: u8,
) -> Result<(), String> {
    let wheel_kind = match direction {
        AttachScrollDirection::Up => MouseEventKind::ScrollUp,
        AttachScrollDirection::Down => MouseEventKind::ScrollDown,
    };
    if let AttachScrollSource::PageKey { input } = source {
        let host_scroll = runtime
            .plain_page_keys_use_host_scrollback()
            .unwrap_or(false);
        if host_scroll {
            match direction {
                AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
                AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
            }
            return Ok(());
        }
        return apply_terminal_attach_input(runtime, input);
    }

    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
            runtime.scroll_reset();
            let position = crate::input::mouse::Position::Cell {
                column: column.unwrap_or(0),
                row: row.unwrap_or(0),
            };
            let Some(bytes) = runtime.encode_mouse_wheel(
                wheel_kind,
                position,
                KeyModifiers::from_bits_truncate(modifiers),
            ) else {
                return Err(format!(
                    "failed to encode terminal attach mouse wheel event: {wheel_kind:?}"
                ));
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach mouse wheel input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(());
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach alternate scroll input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::HostScroll) | None => match direction {
            AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(())
}

fn apply_terminal_attach_input(
    runtime: &crate::terminal::TerminalRuntime,
    data: Vec<u8>,
) -> Result<(), String> {
    runtime.scroll_reset();
    if let Some(text) = crate::raw_input::complete_text_bracketed_paste(&data) {
        runtime
            .try_send_paste(text.to_owned())
            .map_err(|err| format!("terminal attach paste failed: {err}"))
    } else {
        runtime
            .try_send_bytes(Bytes::from(data))
            .map_err(|err| format!("terminal attach input failed: {err}"))
    }
}

#[cfg(windows)]
fn spawn_windows_client_accept_thread(
    listener: LocalListener,
    should_quit: Arc<AtomicBool>,
    server_event_tx: mpsc::Sender<ServerEvent>,
) {
    std::thread::spawn(move || {
        let mut next_client_id = 1_u64;
        while !should_quit.load(Ordering::Acquire) {
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(err) => {
                    if should_quit.load(Ordering::Acquire) {
                        break;
                    }
                    error!(err = %err, "client listener accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };

            let client_id = next_client_id;
            next_client_id = next_client_id.saturating_add(1);

            if let Err(err) = stream.set_nonblocking(true) {
                warn!(err = %err, "failed to set client stream nonblocking");
                continue;
            }

            let should_quit = should_quit.clone();
            let server_event_tx = server_event_tx.clone();
            std::thread::spawn(move || {
                if let Err(err) = crate::server::client_transport::handle_client_handshake(
                    stream,
                    client_id,
                    &server_event_tx,
                    &should_quit,
                ) {
                    debug!(client_id, err = %err, "client handshake failed");
                }
            });
        }
    });
}

impl HeadlessServer {
    /// Creates and starts the headless server.
    ///
    /// This:
    /// 1. Prepares the client socket path (cleans up stale sockets)
    /// 2. Binds the client socket listener
    pub fn new(
        mut app: app::App,
        config_diagnostics: &[String],
        api_tx: Option<api::ApiRequestSender>,
        api_server: Option<api::ServerHandle>,
        should_quit: Arc<AtomicBool>,
        prepared_omp_bridge: Option<(TcpListener, crate::pane::OmpBridgeEnv)>,
    ) -> io::Result<Self> {
        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;

        let omp_service = OmpService::new(prepared_omp_bridge)?;
        app.omp_bridge = Some(omp_service.bridge().clone());
        for workspace in &mut app.state.workspaces {
            workspace.omp_bridge = Some(omp_service.bridge().clone());
        }

        let listener = bind_private_local_listener(&client_path)?;
        restrict_socket_permissions(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        info!(path = %client_path.display(), "client protocol socket listening");

        // Set non-blocking on Unix so we can poll it from the event loop.
        #[cfg(unix)]
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        // Channel for server events from client threads.
        let (server_event_tx, server_event_rx) = mpsc::channel(64);
        #[cfg(windows)]
        spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());

        let server_keybindings = app_keybindings(&app);
        let headless_size = app.state.headless_size;
        let (server_config_diagnostic, server_config_diagnostic_without_keybindings) =
            server_config_diagnostic_summaries(config_diagnostics);
        #[cfg(not(unix))]
        let _ = api_tx;
        Ok(Self {
            app,
            #[cfg(unix)]
            api_tx,
            api_server,
            #[cfg(unix)]
            client_listener: listener,
            client_socket_path: client_path,
            client_socket_identity,
            clients: HashMap::new(),
            private_surface_candidates: HashMap::new(),
            #[cfg(test)]
            independent_omp_renderers_enabled: false,
            private_omp_failed_routes: HashMap::new(),
            private_omp_retry_attempted_routes: HashMap::new(),
            next_private_omp_retry_id: 1,
            private_omp_pending_routes: HashMap::new(),
            private_omp_executable: None,
            private_omp_resolving: None,
            #[cfg(test)]
            private_omp_test_executable: None,
            next_omp_renderer_launch_id: 1,
            omp_service,
            retired_private_pane_ids: VecDeque::new(),
            #[cfg(unix)]
            next_client_id: 1,
            foreground_client_id: None,
            sent_window_title: None,
            api_window_title: None,
            server_keybindings,
            server_config_diagnostic,
            server_config_diagnostic_without_keybindings,
            terminal_attach_owners: HashMap::new(),
            pending_alt_screen_reads: Vec::new(),
            deferred_alt_screen_reads: Vec::new(),
            next_activity_stamp: 1,
            headless_size,
            effective_size: headless_size,
            shutting_down: false,
            handoff_in_progress: false,
            #[cfg(unix)]
            pending_handoff_repaint_nudge: false,
            should_quit,
            server_event_rx,
            server_event_tx,
        })
    }

    // Server-private OMP guests are enabled in production for focus-scoped client
    // surfaces. Full-surface native OMP remains capability-gated by the client
    // handshake; tests can disable this path to exercise pane-local fallback.
    fn independent_omp_renderers_enabled(&self) -> bool {
        #[cfg(test)]
        {
            self.independent_omp_renderers_enabled
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    /// Runs the headless server event loop until shutdown.
    ///
    /// This is the server's main loop — analogous to `App::run()` but without
    /// a real terminal. It:
    /// - Drains internal events (pane death, state changes)
    /// - Drains API requests (from the JSON socket)
    /// - Accepts new client connections
    /// - Reads client messages and routes input
    /// - Handles scheduled tasks (session save, metadata expiry, etc.)
    /// - Renders virtually and streams frames to clients
    pub async fn run(&mut self) -> io::Result<()> {
        crate::logging::startup("server");

        // Register SIGINT handler for graceful shutdown.
        let should_quit = self.should_quit.clone();
        let quit_notify = self.server_event_tx.clone();
        ctrlc_handler(should_quit, quit_notify);

        // No input_rx needed — server doesn't read stdin.
        // We use None for input_rx so the event loop doesn't try to read from stdin.
        self.app.input_rx = None;

        let mut needs_render = true;
        let mut needs_full_render = true;
        let mut needs_graphics_render = false;
        let mut next_omp_maintenance_check = Instant::now();

        loop {
            crate::render_prof::event("loop.tick");
            crate::render_prof::flush_if_due();
            self.app.reap_finished_detached_processes();

            // If shutdown has been initiated, complete it and exit.
            if self.shutting_down {
                self.complete_shutdown().await?;
                break;
            }

            // Check if we should start shutting down.
            if self.app.state.should_quit || self.should_quit.load(Ordering::Acquire) {
                self.drain_internal_events_with_forwarding_up_to(
                    crate::app::APP_EVENT_CHANNEL_CAPACITY,
                );
                self.initiate_shutdown();
                continue;
            }

            // 1. Check the coalesced render signal from PTY readers and generic runtime work.
            if self.app.render_dirty.is_pending() {
                needs_render = true;
                crate::render_prof::event("render.request.signal");
            }

            // 2. Drain a bounded internal-event batch. API handlers perform an
            // exhaustive forwarding-aware drain before reading pane/runtime state.
            if self.drain_internal_events_with_forwarding() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.internal_events");
            }
            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }
            if self.app.expire_due_metadata(Instant::now()) {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.metadata_expiry");
            }

            // 3. Drain API requests.
            if self.pane_graphics_runtime_active() {
                let api_impact = self.drain_api_requests_with_render_impact();
                record_render_impact("api_requests", api_impact);
                match api_impact {
                    RenderImpact::None => {}
                    RenderImpact::Graphics => {
                        needs_render = true;
                        needs_graphics_render = true;
                    }
                    RenderImpact::Full => {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
            } else if self.drain_api_requests_with_shutdown_check() {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.api_requests");
            }
            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }

            self.app.sync_focus_events();
            self.app.sync_session_save_schedule();
            let maintenance_now = Instant::now();
            if maintenance_now >= next_omp_maintenance_check {
                next_omp_maintenance_check = maintenance_now + CLIENT_ACCEPT_POLL_INTERVAL;
                if self.enforce_omp_maintenance() {
                    needs_render = true;
                    needs_full_render = true;
                    needs_graphics_render = false;
                    crate::render_prof::event("full_render_cause.omp_maintenance");
                }
            }

            // 4. Accept new client connections.
            self.accept_client_connections()?;
            self.omp_service
                .accept_pending(self.server_event_tx.clone());

            // 5. Drain server events from client threads.
            if self.pane_graphics_runtime_active() {
                let server_impact = self.drain_server_events_with_render_impact();
                record_render_impact("server_events", server_impact);
                match server_impact {
                    RenderImpact::None => {}
                    RenderImpact::Graphics => {
                        needs_render = true;
                        needs_graphics_render = true;
                    }
                    RenderImpact::Full => {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
            } else if self.drain_server_events() {
                needs_render = true;
                needs_full_render = true;
                crate::render_prof::event("full_render_cause.server_events");
            }
            if self.drain_private_omp_guest_records() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.private_omp_guest");
            }

            if self.should_quit.load(Ordering::Acquire) {
                continue;
            }

            // 6. Handle scheduled tasks.
            let now = Instant::now();
            if self.handle_scheduled_tasks_headless(now, needs_render) {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.scheduled_tasks");
            }

            if self.handle_deferred_requests_headless() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
            }

            self.poll_pending_alt_screen_reads(now);
            if self.process_deferred_alt_screen_reads() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
            }

            if latest_app_client(&self.clients).is_some() && self.app.ensure_default_workspace() {
                needs_render = true;
                needs_full_render = true;
                needs_graphics_render = false;
                crate::render_prof::event("full_render_cause.default_workspace");
            }

            if self.app.pane_graphics.retain_live_panes(&self.app.state) {
                needs_render = true;
                needs_graphics_render = true;
            }
            if self.expire_direct_graphics(now) {
                needs_render = true;
                needs_graphics_render = true;
            }

            self.drain_client_config_reload_request();
            self.sync_immediate_pty_sources();
            self.stream_host_mouse_capture_mode();
            self.stream_host_keyboard_enhancement_flags();

            // 7. Render virtually and stream frames. Hidden-only PTY work keeps a
            // bounded classification cadence without delaying presentation work
            // that joins the same coalesced request.
            let render_cadence_due = self.app.can_render_now(now);
            if needs_render
                && (render_cadence_due
                    || (self.app.can_present_now(now)
                        && self.has_pending_presentation_work(
                            needs_full_render,
                            needs_graphics_render,
                        )))
            {
                crate::render_prof::event("render.attempt");
                self.sync_canonical_navigation_to_foreground();
                let render_request = self.app.render_dirty.take();
                if self
                    .app
                    .refresh_hovered_link_for_panes(&render_request.pty_sources)
                {
                    needs_full_render = true;
                }
                let pty_dirty = !render_request.pty_sources.is_empty();
                if pty_dirty {
                    crate::render_prof::event("render.attempt.pty_dirty");
                    crate::render_prof::counter(
                        "render.attempt.pty_sources",
                        render_request.pty_sources.len() as u64,
                    );
                }
                if self
                    .app
                    .refresh_findr_visible_if_needed(&render_request.pty_sources)
                {
                    needs_full_render = true;
                }
                if render_request.generic {
                    needs_full_render = true;
                    crate::render_prof::event("full_render_cause.generic_dirty");
                }
                let (sidebar_title_changed, outer_title_synced) =
                    self.sync_terminal_title_sources(&render_request.terminal_title_sources);
                if sidebar_title_changed {
                    needs_full_render = true;
                    crate::render_prof::event("full_render_cause.terminal_title_sidebar");
                }
                if needs_full_render && !outer_title_synced {
                    self.sync_window_title();
                }
                if !needs_full_render && !needs_graphics_render && !pty_dirty {
                    // A synchronized-output OSC title can be the only pending work.
                    // Its deferred PTY repaint has its own signal; do not manufacture
                    // a full UI render for this client-local side effect.
                    needs_render = false;
                    continue;
                }
                if needs_full_render {
                    crate::render_prof::event("retained_gate.needs_full_render");
                } else if !pty_dirty {
                    crate::render_prof::event("retained_gate.not_pty_dirty");
                }
                let pty = if !pty_dirty {
                    PtyRenderState::Clean
                } else if self.pty_sources_visible_to_any_render_target(&render_request.pty_sources)
                {
                    PtyRenderState::Visible
                } else {
                    PtyRenderState::Hidden
                };
                let mut deferred_graphics = false;
                let render_plan = retained_render_plan(RetainedRenderInput {
                    needs_full_render,
                    needs_graphics_render,
                    pty,
                });
                let rendered_retained = match render_plan {
                    RetainedRenderPlan::Full => false,
                    RetainedRenderPlan::Graphics if self.app_client_count() > 1 => false,
                    RetainedRenderPlan::Graphics => {
                        match self.render_retained_graphics_update_and_stream() {
                            RetainedGraphicsOutcome::Sent => true,
                            RetainedGraphicsOutcome::Deferred => {
                                deferred_graphics = true;
                                false
                            }
                            RetainedGraphicsOutcome::Fallback => false,
                        }
                    }
                    RetainedRenderPlan::Pty => self.render_retained_pty_update_and_stream(),
                    RetainedRenderPlan::HiddenPty if self.app_client_count() > 1 => false,
                    RetainedRenderPlan::HiddenPty => {
                        crate::render_prof::event("render.skipped.hidden_sources");
                        true
                    }
                };
                if deferred_graphics {
                    needs_render = false;
                    continue;
                }
                if !rendered_retained {
                    crate::render_prof::event("full_render.invoke");
                    self.render_and_stream();
                }
                self.app
                    .record_render_attempt(now, render_plan != RetainedRenderPlan::HiddenPty);
                needs_render = false;
                needs_full_render = false;
                needs_graphics_render = false;
                continue;
            }

            // 8. Wait for next event.
            let next_deadline = self
                .app
                .next_headless_loop_deadline_with_git_refresh(
                    now,
                    needs_render,
                    self.has_app_client(),
                )
                .map(|deadline| deadline.min(now + CLIENT_ACCEPT_POLL_INTERVAL))
                .or(Some(now + CLIENT_ACCEPT_POLL_INTERVAL));
            let next_deadline = self
                .pending_alt_screen_reads
                .iter()
                .map(|pending| pending.next_deadline())
                .fold(next_deadline, |deadline, pending| {
                    Some(deadline.map_or(pending, |current| current.min(pending)))
                });
            let next_deadline = self
                .private_surface_candidates
                .values()
                .map(|candidate| candidate.deadline)
                .fold(next_deadline, |deadline, candidate| {
                    Some(deadline.map_or(candidate, |current| current.min(candidate)))
                });
            let event = {
                tokio::select! {
                    maybe_api = self.app.api_rx.recv() => match maybe_api {
                        Some(msg) => LoopEvent::Api(Box::new(msg)),
                        None => LoopEvent::Timer,
                    },
                    maybe_ev = self.app.event_rx.recv() => match maybe_ev {
                        Some(ev) => LoopEvent::Internal(ev),
                        None => LoopEvent::Timer,
                    },
                    maybe_server_ev = self.server_event_rx.recv() => match maybe_server_ev {
                        Some(ev) => LoopEvent::ServerEvent(ev),
                        None => LoopEvent::Timer,
                    },
                    _ = sleep_until_or_pending(next_deadline) => LoopEvent::Timer,
                    _ = self.app.render_notify.notified() => LoopEvent::RenderRequested,
                }
            };

            if self.should_quit.load(Ordering::Acquire) {
                match event {
                    LoopEvent::Internal(ev) => {
                        self.handle_internal_event_with_forwarding(ev);
                    }
                    LoopEvent::ServerEvent(ServerEvent::ClientConnected { writer, .. }) => {
                        if let Ok(message) =
                            Self::frame_server_message(&ServerMessage::ServerShutdown {
                                reason: Some("server is shutting down".to_owned()),
                            })
                        {
                            let _ = writer.control.send(message);
                        }
                    }
                    _ => {}
                }
                continue;
            }

            match event {
                LoopEvent::Timer => {}
                LoopEvent::Internal(ev) => {
                    if self.handle_internal_event_with_forwarding(ev) {
                        needs_render = true;
                        needs_full_render = true;
                        needs_graphics_render = false;
                    }
                }
                LoopEvent::Api(msg) => {
                    if self.pane_graphics_runtime_active() {
                        let impact = self.handle_api_request_with_render_impact(*msg);
                        record_render_impact("api_requests", impact);
                        match impact {
                            RenderImpact::None => {}
                            RenderImpact::Graphics => {
                                needs_render = true;
                                needs_graphics_render = true;
                            }
                            RenderImpact::Full => {
                                needs_render = true;
                                needs_full_render = true;
                                needs_graphics_render = false;
                            }
                        }
                    } else if self.handle_api_request_with_shutdown_check(*msg) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::ServerEvent(ev) => {
                    if self.pane_graphics_runtime_active() {
                        let impact = self.handle_server_event_with_render_impact(ev);
                        record_render_impact("server_events", impact);
                        match impact {
                            RenderImpact::None => {}
                            RenderImpact::Graphics => {
                                needs_render = true;
                                needs_graphics_render = true;
                            }
                            RenderImpact::Full => {
                                needs_render = true;
                                needs_full_render = true;
                                needs_graphics_render = false;
                            }
                        }
                    } else if self.handle_server_event(ev) {
                        needs_render = true;
                        needs_full_render = true;
                    }
                }
                LoopEvent::RenderRequested => {
                    if self.app.render_dirty.is_pending() {
                        needs_render = true;
                    }
                }
            }
        }

        // Save session on exit.
        if !self.app.no_session {
            self.app.save_session_now();
        }

        info!("headless server exiting");
        Ok(())
    }

    fn handle_deferred_requests_headless(&mut self) -> bool {
        let mut needs_render = false;

        if self.app.state.request_complete_onboarding {
            self.app.state.request_complete_onboarding = false;
            self.app.open_settings_from_onboarding();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_onboarding");
        }

        if self.app.state.request_new_workspace {
            self.app.state.request_new_workspace = false;
            let response = self.headless_workspace_create("headless.workspace.create", None, None);
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create workspace"
                );
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_new_workspace");
        }

        if self.app.state.request_new_tab {
            self.app.state.request_new_tab = false;
            let label = self.app.state.requested_new_tab_name.take();
            let response = self.headless_tab_create("headless.tab.create", label);
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create tab"
                );
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_new_tab");
        }

        if let Some(ws_idx) = self.app.state.request_new_linked_worktree.take() {
            self.app.open_new_linked_worktree_dialog(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if let Some(ws_idx) = self.app.state.request_open_existing_worktree.take() {
            self.app.open_existing_worktree_dialog(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if let Some(cwd) = self.app.state.request_new_workspace_cwd.take() {
            let response = self.headless_workspace_create(
                "headless.workspace.create_cwd",
                Some(cwd.display().to_string()),
                None,
            );
            if let Err(error) = response {
                error!(
                    code = %error.code,
                    message = %error.message,
                    "failed to create workspace at requested cwd"
                );
                self.app.state.mode = app::Mode::Navigate;
            }
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_workspace_cwd");
        }

        if let Some(ws_idx) = self.app.state.request_remove_linked_worktree.take() {
            self.app.open_remove_linked_worktree_confirmation(ws_idx);
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_dialog");
        }

        if self.app.state.request_submit_worktree_create {
            self.app.state.request_submit_worktree_create = false;
            self.app.submit_worktree_create_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_submit_worktree_open {
            self.app.state.request_submit_worktree_open = false;
            self.app.submit_worktree_open_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_submit_worktree_remove {
            self.app.state.request_submit_worktree_remove = false;
            self.app.submit_worktree_remove_via_api();
            needs_render = true;
            crate::render_prof::event("full_render_cause.deferred_worktree_submit");
        }

        if self.app.state.request_reload_config {
            self.app.state.request_reload_config = false;
            self.reload_server_config(true);
            needs_render = true;
            crate::render_prof::event("full_render_cause.config_reload");
        }

        needs_render
    }

    fn headless_workspace_create(
        &mut self,
        id: &'static str,
        cwd: Option<String>,
        label: Option<String>,
    ) -> Result<(), api::schema::ErrorBody> {
        self.dispatch_headless_runtime_mutation(
            id,
            api::schema::Method::WorkspaceCreate(api::schema::WorkspaceCreateParams {
                cwd,
                focus: true,
                label,
                env: Default::default(),
            }),
        )
    }

    fn headless_tab_create(
        &mut self,
        id: &'static str,
        label: Option<String>,
    ) -> Result<(), api::schema::ErrorBody> {
        self.dispatch_headless_runtime_mutation(
            id,
            api::schema::Method::TabCreate(api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label,
                env: Default::default(),
            }),
        )
    }

    fn dispatch_headless_runtime_mutation(
        &mut self,
        id: &'static str,
        method: api::schema::Method,
    ) -> Result<(), api::schema::ErrorBody> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        self.handle_api_request_with_shutdown_check_inner(
            api::ApiRequestMessage {
                request: api::schema::Request {
                    id: id.to_string(),
                    method,
                },
                context: api::ApiRequestContext::default(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            },
            true,
        );
        match response_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(response) => serde_json::from_str::<api::schema::ErrorResponse>(&response)
                .map(|response| Err(response.error))
                .unwrap_or(Ok(())),
            Err(err) => Err(api::schema::ErrorBody {
                code: "internal_error".into(),
                message: format!("headless runtime mutation response failed: {err}"),
            }),
        }
    }

    fn allocate_activity_stamp(&mut self) -> u64 {
        let stamp = self.next_activity_stamp;
        self.next_activity_stamp = self.next_activity_stamp.saturating_add(1);
        stamp
    }

    fn resize_shared_runtime_to_effective_size(&mut self) {
        self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(true);
    }

    fn resize_shared_runtime_to_effective_size_before_input(&mut self) {
        self.resize_shared_runtime_to_effective_size_with_pending_agent_resumes(false);
    }

    fn resize_shared_runtime_to_effective_size_with_pending_agent_resumes(
        &mut self,
        start_pending_agent_resumes: bool,
    ) {
        if self.foreground_client_id.is_none() {
            return;
        }
        let Some(client_id) = self.foreground_client_id else {
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        let (cols, rows) = self.effective_size;
        let area = Rect::new(0, 0, cols, rows);
        if self.app.state.kitty_graphics_enabled && client.cell_size.is_known() {
            crate::ui::compute_view_with_cell_size(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
                client.cell_size,
            );
        } else {
            crate::ui::compute_view_with_runtime_registry(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
            );
        }

        // Shared runtime size changes affect pane wrapping and foreground-driven
        // rendering semantics. Force one fresh frame to every remaining client
        // even if the next rendered buffer compares equal to its cached frame.
        for client in self.clients.values_mut() {
            client.request_repaint();
        }
        if !start_pending_agent_resumes {
            self.app.pending_agent_resume_retry_at = None;
            return;
        }
        let now = Instant::now();
        self.app.sync_pending_agent_resume_retry_at(now);
        if self
            .app
            .start_pending_agent_resumes(now, self.app.pending_agent_resume_retry_due(now))
        {
            for client in self.clients.values_mut() {
                client.request_repaint();
            }
        }
    }

    fn sync_headless_view_geometry(&mut self) {
        crate::ui::compute_view_without_resizing_panes(
            &mut self.app.state,
            &self.app.terminal_runtimes,
            Rect::new(0, 0, self.headless_size.0, self.headless_size.1),
        );
    }

    fn sync_foreground_client_state(&mut self) {
        self.app.direct_graphics_available = self.direct_graphics_available();
        self.app.pixel_mouse_available = self.foreground_client_id.is_some_and(|id| {
            self.clients
                .get(&id)
                .is_some_and(|client| client.pixel_mouse)
        });
        if !self.app.direct_graphics_available {
            self.retire_all_direct_graphics();
        }
        let Some(client_id) = self.foreground_client_id else {
            self.effective_size = self.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size = crate::kitty_graphics::HostCellSize::default();
            self.sync_headless_view_geometry();
            let server_keybindings = self.server_keybindings.clone();
            apply_keybindings(&mut self.app, &server_keybindings);
            self.sync_visible_server_config_diagnostic(false);
            return;
        };
        let Some(client) = self.clients.get(&client_id) else {
            self.foreground_client_id = None;
            self.effective_size = self.headless_size;
            self.app.state.outer_terminal_focus = None;
            self.app.state.host_cell_size = crate::kitty_graphics::HostCellSize::default();
            self.sync_headless_view_geometry();
            let server_keybindings = self.server_keybindings.clone();
            apply_keybindings(&mut self.app, &server_keybindings);
            self.sync_visible_server_config_diagnostic(false);
            return;
        };

        let terminal_size = client.terminal_size;
        let outer_terminal_focus = client.outer_terminal_focus;
        let host_cell_size = if self.app.state.kitty_graphics_enabled && client.cell_size.is_known()
        {
            client.cell_size
        } else {
            crate::kitty_graphics::HostCellSize::default()
        };
        let host_terminal_theme = client.host_terminal_theme;
        let host_terminal_appearance = client.host_terminal_appearance;
        let host_terminal_appearance_explicit = client.host_terminal_appearance_explicit;
        let uses_local_keybindings = client.keybindings.is_some();
        let keybindings = client
            .keybindings
            .as_deref()
            .unwrap_or(&self.server_keybindings)
            .clone();

        self.effective_size = terminal_size;
        self.app.state.outer_terminal_focus = outer_terminal_focus;
        self.app.state.host_cell_size = host_cell_size;
        apply_keybindings(&mut self.app, &keybindings);
        self.sync_visible_server_config_diagnostic(uses_local_keybindings);
        if outer_terminal_focus == Some(true) {
            self.app.state.mark_active_tab_seen();
        }
        self.app.set_host_terminal_appearance_state(
            host_terminal_appearance,
            host_terminal_appearance_explicit,
        );
        self.app.set_host_terminal_theme(host_terminal_theme);
    }

    #[cfg(unix)]
    fn authorize_live_handoff(
        &self,
    ) -> io::Result<Option<crate::server::omp_maintenance::OmpMaintenanceHandoffState>> {
        if !self.omp_service.live_route_keys().is_empty() {
            return Err(io::Error::other(
                "live handoff is unavailable while OMP host routes are live; restart Herdr normally",
            ));
        }
        for workspace in &self.app.state.workspaces {
            for tab in &workspace.tabs {
                for (pane_id, pane) in &tab.panes {
                    let terminal_id = &pane.attached_terminal_id;
                    let Some(terminal) = self.app.state.terminals.get(terminal_id) else {
                        continue;
                    };
                    if terminal.execution_target.is_local() {
                        continue;
                    }
                    if self
                        .app
                        .terminal_runtimes
                        .get(terminal_id)
                        .is_some_and(|runtime| !runtime.remote_execution_ready())
                    {
                        return Err(io::Error::other(format!(
                            "live handoff is unavailable while SSH pane {} ({}) is still starting; wait for it to become ready and retry",
                            pane_id.raw(), terminal.execution_target,
                        )));
                    }
                }
            }
        }
        self.omp_service
            .maintenance_handoff_state()
            .map_err(|error| io::Error::other(error.message()))
    }

    #[cfg(unix)]
    fn perform_live_handoff(
        &mut self,
        params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        let omp_maintenance = self.authorize_live_handoff()?;

        info!("starting live handoff");
        let import_exe = params.import_exe.as_deref().map(std::path::PathBuf::from);
        let socket_path = crate::server::handoff::handoff_socket_path();
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let listener = match crate::server::handoff::bind_listener(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                self.handoff_in_progress = false;
                return Err(err);
            }
        };

        let mut pane_by_terminal = HashMap::new();
        for ws in &self.app.state.workspaces {
            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    pane_by_terminal.insert(
                        pane.attached_terminal_id.clone(),
                        (pane_id.raw(), pane.seen),
                    );
                }
            }
        }
        if pane_by_terminal.len() > crate::server::handoff::MAX_FDS_PER_HANDOFF {
            let _ = std::fs::remove_file(&socket_path);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "live handoff supports at most {} panes in one update; close panes or restart herdr normally",
                    crate::server::handoff::MAX_FDS_PER_HANDOFF
                ),
            ));
        }

        self.handoff_in_progress = true;
        let _ = reject_pending_client_connections(&self.client_listener);

        let mut paused_terminal_ids = Vec::new();
        for terminal_id in pane_by_terminal.keys() {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                if let Err(err) = runtime.pause_handoff_reader(Duration::from_secs(2)) {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(err);
                }
                paused_terminal_ids.push(terminal_id.clone());
            }
        }
        // Pausing a reader can synchronously parse final bytes and enqueue ready/cwd
        // events. Reconcile only the queued snapshot before capturing terminal state;
        // private runtimes remain live and must not keep handoff draining forever.
        self.drain_internal_event_snapshot_with_forwarding();

        let snapshot = crate::persist::capture(
            &self.app.state.workspaces,
            &self.app.state.terminals,
            &self.app.terminal_runtimes,
            self.app.state.active,
            self.app.state.selected,
            self.app.state.sidebar_width,
            self.app.state.sidebar_section_split,
            self.app.state.collapsed_space_keys.clone(),
        );

        let mut handoff_entries = Vec::new();
        for (terminal_id, runtime) in self.app.terminal_runtimes.iter() {
            let Some((pane_id, seen)) = pane_by_terminal.get(terminal_id).copied() else {
                continue;
            };
            let mut handoff_runtime = runtime.handoff_runtime_state(pane_id);
            let terminal = self.app.state.terminals.get(terminal_id);
            handoff_runtime.agent_state = terminal
                .map(|terminal| crate::handoff_runtime::HandoffAgentState::capture(terminal, seen));
            if let Some(terminal) = terminal {
                handoff_runtime.pending_agent_resume_plan =
                    terminal.pending_agent_resume_plan.clone();
                handoff_runtime.pending_agent_resume_attempt_pid =
                    terminal.pending_agent_resume_attempt_pid();
                handoff_runtime.pending_agent_resume_retired_pids =
                    terminal.pending_agent_resume_retired_pids().to_vec();
                if !terminal.execution_target.is_local() && terminal.launch_argv.is_none() {
                    handoff_runtime.respawn_shell_on_exit = Some(terminal.respawn_shell_on_exit);
                }
            }
            let has_agent_session =
                terminal.is_some_and(|terminal| terminal.persisted_agent_session.is_some());
            if !has_agent_session {
                handoff_runtime.initial_history_ansi = runtime.handoff_history_ansi();
            }
            handoff_entries.push((terminal_id.clone(), handoff_runtime));
        }

        let panes = handoff_entries
            .iter()
            .map(|(_, runtime)| runtime.clone())
            .collect();
        let manifest = crate::server::handoff::manifest_for(
            snapshot,
            panes,
            params.expected_protocol,
            params.expected_version,
            self.api_window_title.clone(),
            omp_maintenance,
        );
        let mut import_child = match crate::server::handoff::spawn_handoff_import(
            import_exe.as_deref(),
            &socket_path,
            &token,
        ) {
            Ok(child) => child,
            Err(err) => {
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };
        let child_pid = import_child.id();
        info!(pid = child_pid, socket = %socket_path.display(), "spawned handoff import server");

        let mut fds = Vec::new();
        let duplicate_result = (|| {
            for (terminal_id, _) in &handoff_entries {
                let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
                    continue;
                };
                fds.push(runtime.duplicate_handoff_fd()?);
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(err) = duplicate_result {
            for fd in fds {
                let _ = unsafe { libc::close(fd) };
            }
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        let mut stream = match crate::server::handoff::accept_and_validate_on(
            listener,
            &socket_path,
            &token,
            &manifest,
        ) {
            Ok(stream) => stream,
            Err(err) => {
                for fd in fds {
                    let _ = unsafe { libc::close(fd) };
                }
                crate::server::handoff::cleanup_failed_import_child(&mut import_child);
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };

        let send_result = crate::server::handoff::send_fds_and_wait_restored(&mut stream, &fds);
        for fd in fds {
            let _ = unsafe { libc::close(fd) };
        }
        if let Err(err) = send_result {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        if let Some(api_server) = &self.api_server {
            let _ = api_server.remove_socket_file_if_owned();
        } else {
            let _ = std::fs::remove_file(crate::api::socket_path());
        }
        let _ = remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity);
        if let Err(err) = crate::server::handoff::wait_ready(&mut stream) {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            match self.wait_then_restore_public_sockets_after_failed_handoff() {
                Ok(()) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                }
                Err(restore_err) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(io::Error::other(format!(
                        "handoff replacement server did not become ready: {err}; old server could not restore public sockets: {restore_err}"
                    )));
                }
            }
            return Err(io::Error::other(format!(
                "handoff replacement server did not become ready: {err}"
            )));
        }
        if let Err(err) = crate::server::handoff::report_committed(&mut stream) {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            match self.wait_then_restore_public_sockets_after_failed_handoff() {
                Ok(()) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                }
                Err(restore_err) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(io::Error::other(format!(
                        "handoff replacement server was ready, but commit failed: {err}; old server could not restore public sockets: {restore_err}"
                    )));
                }
            }
            return Err(err);
        }
        self.disconnect_all_clients_for_handoff();

        for (terminal_id, runtime) in self.app.terminal_runtimes.drain_for_handoff() {
            if !pane_by_terminal.contains_key(&terminal_id) {
                continue;
            }
            debug!(terminal = %terminal_id, "preserving pane runtime for handoff");
            runtime.preserve_for_handoff();
        }
        crate::server::handoff::wait_owned_ack(&mut stream);

        Ok(())
    }

    fn finish_live_handoff_shutdown(&mut self) {
        self.shutting_down = true;
        self.app.state.should_quit = true;
        self.app.no_session = true;
        info!("live handoff completed; old server exiting");
    }

    #[cfg(not(unix))]
    fn perform_live_handoff(
        &mut self,
        _params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        Err(io::Error::other("live handoff is only supported on Unix"))
    }

    fn sync_visible_server_config_diagnostic(&mut self, uses_local_keybindings: bool) {
        let visible = if uses_local_keybindings {
            &self.server_config_diagnostic_without_keybindings
        } else {
            &self.server_config_diagnostic
        };
        if self.app.state.config_diagnostic == self.server_config_diagnostic
            || self.app.state.config_diagnostic == self.server_config_diagnostic_without_keybindings
        {
            self.app.state.config_diagnostic = visible.clone();
        }
    }

    #[cfg(unix)]
    fn restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let api_tx = self
            .api_tx
            .clone()
            .ok_or_else(|| io::Error::other("cannot restore api socket without api sender"))?;
        let api_server = api::start_server_with_stop_control(
            api_tx,
            self.app.event_hub.clone(),
            self.should_quit.clone(),
        )?;

        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;
        let listener = bind_private_local_listener(&client_path)?;
        restrict_socket_permissions(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        self.api_server = Some(api_server);
        self.client_listener = listener;
        self.client_socket_path = client_path;
        self.client_socket_identity = client_socket_identity;
        Ok(())
    }

    #[cfg(unix)]
    fn wait_then_restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let timeout = crate::server::handoff::COMMIT_TIMEOUT + Duration::from_secs(2);
        wait_for_old_public_sockets_to_close(timeout)?;
        self.restore_public_sockets_after_failed_handoff()
    }

    #[cfg(unix)]
    fn rollback_handoff_before_commit(
        &mut self,
        socket_path: &Path,
        paused_terminal_ids: &[crate::terminal::TerminalId],
    ) {
        for terminal_id in paused_terminal_ids {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                runtime.set_handoff_reader_paused(false);
            }
        }
        self.handoff_in_progress = false;
        let _ = std::fs::remove_file(socket_path);
    }

    #[cfg(unix)]
    fn nudge_handoff_panes_on_first_client_attach(&mut self) {
        if !self.pending_handoff_repaint_nudge {
            return;
        }
        self.pending_handoff_repaint_nudge = false;
        self.app
            .terminal_runtimes
            .nudge_child_redraw_after_handoff();
    }

    #[cfg(not(unix))]
    fn nudge_handoff_panes_on_first_client_attach(&mut self) {}

    fn reload_server_config(&mut self, notify_success: bool) -> crate::config::ConfigReloadReport {
        let server_keybindings = self.server_keybindings.clone();
        apply_keybindings(&mut self.app, &server_keybindings);
        let report = self.app.apply_config_from_disk(notify_success);
        self.app.take_config_reloaded_from_disk();
        self.server_keybindings = app_keybindings(&self.app);
        self.headless_size = self.app.state.headless_size;
        let (server_config_diagnostic, server_config_diagnostic_without_keybindings) =
            server_config_diagnostic_summaries(&report.diagnostics);
        self.server_config_diagnostic = server_config_diagnostic;
        self.server_config_diagnostic_without_keybindings =
            server_config_diagnostic_without_keybindings;
        self.sync_foreground_client_state();
        report
    }

    fn foreground_client_outer_focus(&self) -> Option<bool> {
        let client_id = self.foreground_client_id?;
        self.clients.get(&client_id)?.outer_terminal_focus
    }

    fn active_tab_suppresses_notifications(&self, is_active_tab: bool) -> bool {
        crate::app::actions::active_tab_suppresses_notifications(
            is_active_tab,
            self.foreground_client_outer_focus(),
        )
    }

    fn promote_client_to_foreground(&mut self, client_id: u64) -> bool {
        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.last_activity = stamp;

        let changed = self.foreground_client_id != Some(client_id);
        if changed {
            self.app.clear_hovered_pane_link();
        }
        self.foreground_client_id = Some(client_id);
        self.sync_foreground_client_state();
        changed
    }

    fn promote_latest_remaining_client(&mut self) -> bool {
        let next_foreground = latest_app_client(&self.clients);
        let changed = next_foreground != self.foreground_client_id;
        self.foreground_client_id = next_foreground;
        if changed {
            self.app.clear_hovered_pane_link();
            let canonical = ClientNavigationState::capture(&self.app.state);
            self.restore_foreground_navigation(&canonical);
        }
        self.sync_foreground_client_state();
        changed
    }

    fn app_client_count(&self) -> usize {
        self.clients
            .values()
            .filter(|client| client.is_full_app_client() && client.writer.is_some())
            .count()
    }

    fn direct_graphics_available(&self) -> bool {
        self.app_client_count() == 1
            && self.foreground_client_id.is_some_and(|id| {
                self.clients.get(&id).is_some_and(|client| {
                    client.is_full_app_client() && client.writer.is_some() && client.direct_graphics
                })
            })
    }

    fn has_app_client(&self) -> bool {
        self.app_client_count() > 0
    }
    fn reconcile_client_navigation_states(&mut self, canonical: &ClientNavigationState) {
        let state = &self.app.state;
        for client in self.clients.values_mut() {
            if !client.is_full_app_client() {
                continue;
            }
            let navigation = client.navigation.as_ref().unwrap_or(canonical);
            client.navigation = Some(navigation.reconciled(state, canonical));
        }
    }

    /// Canonical AppState is always the foreground projection between client operations.
    fn sync_canonical_navigation_to_foreground(&mut self) -> ClientNavigationState {
        let canonical = ClientNavigationState::capture(&self.app.state);
        if let Some(client) = self
            .foreground_client_id
            .and_then(|client_id| self.clients.get_mut(&client_id))
            .filter(|client| client.is_full_app_client())
        {
            client.navigation = Some(canonical.clone());
        }
        self.reconcile_client_navigation_states(&canonical);
        canonical
    }

    fn sync_findr_scan_deadline_after_projection(&mut self) {
        let incomplete = self
            .app
            .state
            .findr
            .as_ref()
            .is_some_and(|findr| !findr.complete);
        if incomplete {
            if self.app.findr_scan_deadline.is_none() {
                self.app.findr_scan_deadline = Some(Instant::now());
            }
        } else {
            self.app.findr_scan_deadline = None;
        }
    }

    fn apply_client_navigation(
        &mut self,
        client_id: u64,
        canonical: &ClientNavigationState,
    ) -> bool {
        let Some(client) = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_full_app_client())
        else {
            return false;
        };
        let navigation = client
            .navigation
            .as_ref()
            .unwrap_or(canonical)
            .reconciled(&self.app.state, canonical);
        let applied = navigation.apply_to(&mut self.app.state);
        self.sync_findr_scan_deadline_after_projection();
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.navigation = Some(applied);
        }
        true
    }

    fn restore_foreground_navigation(&mut self, canonical: &ClientNavigationState) {
        let current = ClientNavigationState::capture(&self.app.state);
        let canonical = canonical.reconciled(&self.app.state, &current);
        self.reconcile_client_navigation_states(&canonical);
        let navigation = self
            .foreground_client_id
            .and_then(|client_id| self.clients.get(&client_id))
            .filter(|client| client.is_full_app_client())
            .and_then(|client| client.navigation.clone())
            .unwrap_or_else(|| canonical.clone());
        let applied = navigation.apply_to(&mut self.app.state);
        self.sync_findr_scan_deadline_after_projection();
        if let Some(client) = self
            .foreground_client_id
            .and_then(|client_id| self.clients.get_mut(&client_id))
            .filter(|client| client.is_full_app_client())
        {
            client.navigation = Some(applied);
        }
    }

    fn begin_client_navigation_scope(&mut self, client_id: u64) -> Option<ClientNavigationState> {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_full_app_client)
        {
            return None;
        }
        let canonical = self.sync_canonical_navigation_to_foreground();
        self.apply_client_navigation(client_id, &canonical);
        Some(canonical)
    }

    fn finish_client_navigation_scope(&mut self, client_id: u64, canonical: ClientNavigationState) {
        if self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_full_app_client)
        {
            let navigation = ClientNavigationState::capture(&self.app.state);
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.navigation = Some(navigation);
            }
        }
        self.restore_foreground_navigation(&canonical);
        self.compute_foreground_navigation_view();
    }

    fn compute_client_navigation_view(&mut self, client_id: u64) {
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        let (cols, rows) = client.terminal_size;
        crate::ui::compute_view_without_resizing_panes(
            &mut self.app.state,
            &self.app.terminal_runtimes,
            Rect::new(0, 0, cols, rows),
        );
    }

    fn compute_foreground_navigation_view(&mut self) {
        if let Some(client_id) = self.foreground_client_id {
            self.compute_client_navigation_view(client_id);
            return;
        }
        let (cols, rows) = self.effective_size;
        crate::ui::compute_view_without_resizing_panes(
            &mut self.app.state,
            &self.app.terminal_runtimes,
            Rect::new(0, 0, cols, rows),
        );
    }

    fn retire_private_pane_id(&mut self, pane_id: crate::layout::PaneId) {
        if self.retired_private_pane_ids.contains(&pane_id) {
            return;
        }
        if self.retired_private_pane_ids.len() == RETIRED_PRIVATE_PANE_ID_LIMIT {
            self.retired_private_pane_ids.pop_front();
        }
        self.retired_private_pane_ids.push_back(pane_id);
    }
    fn shared_pane_exists(&self, pane_id: crate::layout::PaneId) -> bool {
        self.app.find_pane(pane_id).is_some()
            || self
                .app
                .state
                .workspace_plugin_panes
                .values()
                .any(|pane| pane.pane_id == pane_id)
            || self
                .app
                .state
                .popup_pane
                .as_ref()
                .is_some_and(|pane| pane.pane_id == pane_id)
            || self.app.overlay_panes.contains_key(&pane_id)
    }

    fn retire_private_surface(&mut self, surface: crate::server::private_surface::PrivateSurface) {
        self.retire_private_pane_id(surface.pane_id());
        surface.shutdown();
    }
    fn private_surface_error_response(
        id: impl Into<String>,
        code: &str,
        message: impl Into<String>,
    ) -> String {
        serde_json::to_string(&api::schema::ErrorResponse {
            id: id.into(),
            error: api::schema::ErrorBody {
                code: code.to_string(),
                message: message.into(),
            },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    fn private_surface_ok_response(id: impl Into<String>) -> String {
        serde_json::to_string(&api::schema::SuccessResponse {
            id: id.into(),
            result: api::schema::ResponseResult::Ok {},
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    fn respond_pending_private_surface(pending: PendingPrivateSurfaceResponse, response: String) {
        let _ = pending.respond_to.send(response);
    }
    fn retire_private_surface_candidate(
        &mut self,
        candidate: PrivateSurfaceCandidate,
        code: &str,
        message: &str,
    ) {
        self.retire_private_surface(candidate.surface);
        if let Some(pending) = candidate.pending_response {
            let response = Self::private_surface_error_response(pending.id.clone(), code, message);
            Self::respond_pending_private_surface(pending, response);
        }
    }

    fn activate_private_surface(
        &mut self,
        client_id: u64,
        surface: crate::server::private_surface::PrivateSurface,
    ) -> bool {
        if !self.clients.contains_key(&client_id) {
            self.retire_private_surface(surface);
            return false;
        }
        self.app.release_input_source_headless(client_id);
        let previous = {
            let client = self
                .clients
                .get_mut(&client_id)
                .expect("owner checked above");
            let previous = client.private_surface.replace(surface);
            client.graphics_surface_reset_pending = true;
            client.request_repaint();
            client.defer_full_render();
            previous
        };
        if let Some(previous) = previous {
            self.retire_private_surface(previous);
        }
        let _ = self.reconcile_omp_renderers();
        true
    }

    #[cfg(test)]
    fn install_private_surface(
        &mut self,
        client_id: u64,
        surface: crate::server::private_surface::PrivateSurface,
        wait_for_remote_ready: bool,
    ) -> bool {
        self.install_private_surface_with_response(client_id, surface, wait_for_remote_ready, None)
    }

    fn install_private_surface_with_response(
        &mut self,
        client_id: u64,
        surface: crate::server::private_surface::PrivateSurface,
        wait_for_remote_ready: bool,
        pending_response: Option<PendingPrivateSurfaceResponse>,
    ) -> bool {
        if !self.clients.contains_key(&client_id) {
            self.retire_private_surface(surface);
            if let Some(pending) = pending_response {
                let response = Self::private_surface_error_response(
                    pending.id.clone(),
                    "view_not_found",
                    "view disconnected during request",
                );
                Self::respond_pending_private_surface(pending, response);
            }
            return false;
        }
        if wait_for_remote_ready {
            if let Some(previous) = self.private_surface_candidates.insert(
                client_id,
                PrivateSurfaceCandidate {
                    surface,
                    pending_response,
                    deadline: Instant::now() + PRIVATE_SURFACE_READY_TIMEOUT,
                },
            ) {
                self.retire_private_surface_candidate(
                    previous,
                    "plugin_pane_open_failed",
                    "private popup launch replaced before execution became ready",
                );
            }
            return true;
        }
        if let Some(previous) = self.private_surface_candidates.remove(&client_id) {
            self.retire_private_surface_candidate(
                previous,
                "plugin_pane_open_failed",
                "private popup launch replaced before execution became ready",
            );
        }
        self.activate_private_surface(client_id, surface)
    }

    fn close_private_surface(&mut self, client_id: u64) {
        let surface = self.clients.get_mut(&client_id).and_then(|client| {
            let surface = client.private_surface.take();
            client.request_repaint();
            client.defer_full_render();
            surface
        });
        let Some(surface) = surface else {
            return;
        };
        self.retire_private_surface(surface);
        if self.foreground_client_id == Some(client_id) {
            self.sync_foreground_client_state();
            self.resize_shared_runtime_to_effective_size();
        }
        let _ = self.reconcile_omp_renderers();
    }

    fn remove_client(&mut self, client_id: u64) -> bool {
        let was_foreground = self.foreground_client_id == Some(client_id);
        self.app.clear_input_source(client_id);
        self.send_client_graphics_cleanup(client_id);
        self.retire_direct_graphics_for_client(client_id);
        let removed = self.clients.remove(&client_id);
        if let Some(candidate) = self.private_surface_candidates.remove(&client_id) {
            self.retire_private_surface_candidate(
                candidate,
                "view_not_found",
                "view disconnected during request",
            );
        }
        self.private_omp_failed_routes.remove(&client_id);
        self.private_omp_retry_attempted_routes.remove(&client_id);
        self.private_omp_pending_routes.remove(&client_id);
        if let Some(mut removed) = removed {
            if let Some(surface) = removed.private_surface.take() {
                self.retire_private_surface(surface);
            }
            crate::server::clipboard_image::remove_files(removed.staged_clipboard_files);
            if let ClientConnectionMode::TerminalAttach { terminal_id } = removed.mode {
                self.terminal_attach_owners.remove(&terminal_id);
                if let Some(terminal_id) = self.terminal_id_by_string(&terminal_id) {
                    self.app
                        .state
                        .direct_attach_resize_locks
                        .remove(&terminal_id);
                }
            }
        }
        if was_foreground {
            self.promote_latest_remaining_client()
        } else {
            false
        }
    }

    fn client_is_omp_pane(&self, client_id: u64) -> bool {
        self.clients
            .get(&client_id)
            .is_some_and(|client| matches!(client.mode, ClientConnectionMode::OmpPane))
    }

    fn client_removal_needs_shared_resize(&self, client_id: u64) -> bool {
        if self.foreground_client_id == Some(client_id) {
            return true;
        }
        matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(
                ClientConnectionMode::TerminalAttach { .. }
                    | ClientConnectionMode::TerminalObserve { .. }
            )
        ) && self.foreground_client_id.is_some()
    }

    fn remove_client_and_resize_if_needed(&mut self, client_id: u64) {
        let needs_shared_resize = self.client_removal_needs_shared_resize(client_id);
        let foreground_changed = self.remove_client(client_id);
        if needs_shared_resize || foreground_changed {
            self.resize_shared_runtime_to_effective_size();
        }
    }

    fn remove_failed_client_and_resize_if_needed(&mut self, client_id: u64) {
        let messages = self.omp_service.disconnect(client_id, &self.clients);
        self.remove_client_and_resize_if_needed(client_id);
        for (target, message) in messages {
            if target != client_id {
                self.send_to_client(target, message);
            }
        }
        self.reconcile_omp_renderers();
    }

    fn prepare_client_graphics_cleanup(
        &self,
        client_id: u64,
        pane_id: Option<crate::layout::PaneId>,
        additional_cleanup: &[u8],
    ) -> Option<(crate::kitty_graphics::HostGraphicsCache, Vec<u8>)> {
        let client = self.clients.get(&client_id)?;
        let mut next_graphics_cache = client.graphics_cache.clone();
        let mut bytes = match pane_id {
            Some(pane_id) => next_graphics_cache.clear_pane_bytes(pane_id),
            None => next_graphics_cache.clear_bytes(),
        };
        bytes.extend_from_slice(additional_cleanup);
        Some((next_graphics_cache, bytes))
    }

    fn queue_client_graphics_cleanup(
        &self,
        client_id: u64,
        pane_id: Option<crate::layout::PaneId>,
        additional_cleanup: &[u8],
        trailing_messages: &[ServerMessage],
    ) -> Option<crate::kitty_graphics::HostGraphicsCache> {
        let (next_graphics_cache, bytes) =
            self.prepare_client_graphics_cleanup(client_id, pane_id, additional_cleanup)?;
        let has_cleanup = !bytes.is_empty();
        let mut serialized = if has_cleanup {
            Self::frame_server_message(&ServerMessage::Graphics { bytes }).ok()?
        } else {
            Vec::new()
        };
        for message in trailing_messages {
            serialized.extend(Self::frame_server_message(message).ok()?);
        }
        if serialized.is_empty() {
            return Some(next_graphics_cache);
        }
        let writer = self.clients.get(&client_id)?.writer.as_ref()?;
        let queued = if let Some(pane_id) = pane_id {
            writer.replace_with_pane_cleanup(pane_id, serialized)
        } else {
            writer.replace_with_cleanup(serialized)
        };
        queued.then_some(next_graphics_cache)
    }

    fn commit_client_graphics_cleanup(
        &mut self,
        client_id: u64,
        next_graphics_cache: crate::kitty_graphics::HostGraphicsCache,
    ) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.graphics_cache = next_graphics_cache;
        }
    }

    fn send_client_graphics_cleanup(&mut self, client_id: u64) -> bool {
        let direct_cleanup = self.direct_graphics_cleanup_for_client(client_id);
        let Some(next_graphics_cache) =
            self.queue_client_graphics_cleanup(client_id, None, &direct_cleanup, &[])
        else {
            return false;
        };
        self.retire_direct_graphics_for_client(client_id);
        self.commit_client_graphics_cleanup(client_id, next_graphics_cache);
        true
    }

    fn queue_native_omp_activation(
        &self,
        client_id: u64,
        target_message: &ServerMessage,
    ) -> Option<crate::kitty_graphics::HostGraphicsCache> {
        let direct_cleanup = self.direct_graphics_cleanup_for_client(client_id);
        let (next_graphics_cache, cleanup_bytes) =
            self.prepare_client_graphics_cleanup(client_id, None, &direct_cleanup)?;
        let retirement_messages = self.direct_graphics_retirement_messages_for_client(client_id);
        let Some(writer) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.writer.as_ref())
        else {
            return cleanup_bytes.is_empty().then_some(next_graphics_cache);
        };
        let has_cleanup = !cleanup_bytes.is_empty();
        let mut serialized = if has_cleanup {
            Self::frame_server_message(&ServerMessage::Graphics {
                bytes: cleanup_bytes,
            })
            .ok()?
        } else {
            Vec::new()
        };
        for retirement_message in retirement_messages {
            serialized.extend(Self::frame_server_message(&retirement_message).ok()?);
        }
        serialized.extend(Self::frame_server_message(target_message).ok()?);
        let queued = if has_cleanup {
            writer.replace_with_cleanup(serialized)
        } else {
            writer.control.send(serialized).is_ok()
        };
        queued.then_some(next_graphics_cache)
    }

    fn begin_omp_graphics_replacement(
        &mut self,
        client_id: u64,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        let direct_cleanup = self.direct_graphics_cleanup_for_client_pane(client_id, pane_id);
        let retirement_messages =
            self.direct_graphics_retirement_messages_for_client_pane(client_id, pane_id);
        let Some(next_graphics_cache) = self.queue_client_graphics_cleanup(
            client_id,
            Some(pane_id),
            &direct_cleanup,
            &retirement_messages,
        ) else {
            return false;
        };
        self.retire_direct_graphics_for_client_pane_without_notifications(client_id, pane_id);
        self.commit_client_graphics_cleanup(client_id, next_graphics_cache);
        true
    }

    fn send_all_clients_graphics_cleanup(&mut self) {
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.send_client_graphics_cleanup(client_id);
        }
    }

    fn update_client_host_theme_from_events(
        &mut self,
        client_id: u64,
        events: &[crate::raw_input::RawInputEvent],
    ) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };

        if !client.update_host_theme_from_events(events) {
            return false;
        }

        if self.foreground_client_id == Some(client_id) {
            let mut changed = self.app.set_host_terminal_appearance_state(
                client.host_terminal_appearance,
                client.host_terminal_appearance_explicit,
            );
            changed |= self.app.set_host_terminal_theme(client.host_terminal_theme);
            if changed {
                self.resize_shared_runtime_to_effective_size_before_input();
            }
            changed
        } else {
            false
        }
    }

    fn update_client_outer_focus_from_events(
        &mut self,
        client_id: u64,
        events: &[crate::raw_input::RawInputEvent],
    ) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let Some(next_focus) = client.update_outer_focus_from_events(events) else {
            return;
        };
        if self.foreground_client_id == Some(client_id) {
            self.app.state.outer_terminal_focus = Some(next_focus);
        }
    }

    fn intercept_identity_input(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> (Vec<crate::raw_input::RawInputEvent>, bool) {
        let mut remaining = Vec::with_capacity(events.len());
        let mut request = false;
        let mut consumed = false;
        for event in events {
            let Some(client) = self.clients.get(&client_id) else {
                remaining.push(event);
                continue;
            };
            let Some(identity) = client.identity.as_ref() else {
                remaining.push(event);
                continue;
            };
            let area = Rect::new(0, 0, client.terminal_size.0, client.terminal_size.1);
            let identity_ui = crate::server::render_stream::identity_ui_state(Some(identity));
            let header = crate::ui::identity_name_hit_rect(&self.app.state, area, &identity_ui);
            let (save, cancel) = crate::ui::identity_modal_inner_rect(area)
                .map_or((Rect::default(), Rect::default()), |inner| {
                    crate::ui::identity_modal_button_rects(inner, identity.committed.is_some())
                });
            let identity = self
                .clients
                .get_mut(&client_id)
                .and_then(|client| client.identity.as_mut())
                .expect("identity was present for this connected App client");

            if !identity.editor.open {
                let opens_editor = matches!(
                    event,
                    crate::raw_input::RawInputEvent::Mouse(mouse)
                        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                            && Self::rect_contains(header, mouse.column, mouse.row)
                );
                if opens_editor {
                    identity.open_editor();
                    consumed = true;
                } else {
                    remaining.push(event);
                }
                continue;
            }

            match event {
                crate::raw_input::RawInputEvent::Text(text) => {
                    if identity.pending.is_none() {
                        identity.insert_editor_text(text.as_str());
                    }
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::Paste(text) => {
                    if identity.pending.is_none() {
                        identity.insert_editor_text(&text);
                    }
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::Key(key) => {
                    if key.kind != KeyEventKind::Release && identity.pending.is_none() {
                        if let Some(text) = key.generated_text.as_deref() {
                            for _ in 0..key.repeat_count {
                                identity.insert_editor_text(text);
                            }
                        } else {
                            match key.code {
                                KeyCode::Backspace => identity.backspace_editor(),
                                KeyCode::Delete => identity.delete_editor(),
                                KeyCode::Left => identity.move_editor_left(),
                                KeyCode::Right => identity.move_editor_right(),
                                KeyCode::Home => identity.move_editor_home(),
                                KeyCode::End => identity.move_editor_end(),
                                KeyCode::Enter => request = true,
                                KeyCode::Esc if identity.committed.is_some() => {
                                    identity.cancel_editor()
                                }
                                KeyCode::Char(ch) if key.modifiers.is_empty() => {
                                    identity.insert_editor_text(&ch.to_string())
                                }
                                _ => {}
                            }
                        }
                    }
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::Mouse(mouse) => {
                    if identity.pending.is_none()
                        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    {
                        if Self::rect_contains(save, mouse.column, mouse.row) {
                            request = true;
                        } else if identity.committed.is_some()
                            && Self::rect_contains(cancel, mouse.column, mouse.row)
                        {
                            identity.cancel_editor();
                        }
                    }
                    consumed = true;
                }
                _ => remaining.push(event),
            }
        }
        if request {
            let request_id = self.allocate_activity_stamp();
            if let Some(request) = self
                .clients
                .get_mut(&client_id)
                .and_then(|client| client.identity.as_mut())
                .and_then(|identity| identity.begin_save(request_id))
            {
                self.send_to_client(
                    client_id,
                    ServerMessage::PersistIdentity {
                        request_id: request.request_id,
                        display_name: request.display_name,
                    },
                );
            }
        }
        (remaining, consumed)
    }

    fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
        column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
    }

    /// Accepts pending client connections from the non-blocking listener.
    #[cfg(unix)]
    fn accept_client_connections(&mut self) -> io::Result<()> {
        if self.handoff_in_progress {
            return reject_pending_client_connections(&self.client_listener);
        }
        accept_pending_client_connections(
            &self.client_listener,
            &mut self.next_client_id,
            &self.should_quit,
            &self.server_event_tx,
        )
    }

    /// Windows named-pipe clients can block in connect unless the server has a
    /// pending blocking accept. The dedicated accept thread handles that path.
    #[cfg(windows)]
    fn accept_client_connections(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Drains a bounded server-event batch so scheduled deadlines remain serviceable under load.
    ///
    /// Uses the original full-render semantics when pane graphics are dormant.
    fn drain_server_events(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..crate::app::APP_EVENT_DRAIN_LIMIT {
            if self.should_quit.load(Ordering::Acquire) || self.app.scroll_render_pending {
                break;
            }
            let Ok(ev) = self.server_event_rx.try_recv() else {
                break;
            };
            changed |= self.handle_server_event(ev);
        }
        changed
    }

    /// Returns the strongest render impact from one bounded server-event batch.
    fn drain_server_events_with_render_impact(&mut self) -> RenderImpact {
        let mut impact = RenderImpact::None;
        for _ in 0..crate::app::APP_EVENT_DRAIN_LIMIT {
            if self.should_quit.load(Ordering::Acquire) || self.app.scroll_render_pending {
                break;
            }
            let Ok(ev) = self.server_event_rx.try_recv() else {
                break;
            };
            impact.merge(self.handle_server_event_with_render_impact(ev));
        }
        impact
    }

    async fn reject_late_client_connections(&mut self) {
        self.server_event_rx.close();
        while let Some(event) = self.server_event_rx.recv().await {
            if let ServerEvent::ClientConnected { writer, .. } = event {
                if let Ok(message) = Self::frame_server_message(&ServerMessage::ServerShutdown {
                    reason: Some("server is shutting down".to_owned()),
                }) {
                    let _ = writer.control.send(message);
                }
            }
        }
    }

    fn terminal_id_by_string(&self, terminal_id: &str) -> Option<crate::terminal::TerminalId> {
        self.app
            .state
            .terminals
            .keys()
            .find(|id| id.to_string() == terminal_id)
            .cloned()
    }

    fn runtime_for_terminal_id_string(
        &self,
        terminal_id: &str,
    ) -> Option<&crate::terminal::TerminalRuntime> {
        let terminal_id = self.terminal_id_by_string(terminal_id)?;
        self.app.terminal_runtimes.get(&terminal_id)
    }

    fn resolve_terminal_target_id_string(&self, target: &str) -> Option<String> {
        if self.terminal_id_by_string(target).is_some() {
            return Some(target.to_owned());
        }
        self.app
            .resolve_terminal_target(target)
            .ok()
            .map(|resolved| resolved.terminal_id)
    }

    fn write_client_clipboard_image(
        &mut self,
        client_id: u64,
        extension: &str,
        data: &[u8],
    ) -> std::io::Result<String> {
        let staged = crate::server::clipboard_image::stage(client_id, extension, data)?;
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.staged_clipboard_files.push(staged.path);
        }
        info!(client_id, bytes = data.len(), path = %staged.paste_text, "staged client clipboard image");
        Ok(staged.paste_text)
    }

    fn paste_client_clipboard_image_path(&mut self, client_id: u64, path: String) -> bool {
        if let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id },
            ..
        }) = self.clients.get(&client_id)
        {
            if let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) {
                let payload = paste_payload_for_runtime(runtime, &path);
                if let Err(err) = runtime.try_send_bytes(Bytes::from(payload)) {
                    warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach clipboard image paste failed");
                }
            }
            return true;
        }

        let navigation_scope = self.begin_client_navigation_scope(client_id);
        let foreground_changed = self.promote_client_to_foreground(client_id);
        if foreground_changed {
            self.resize_shared_runtime_to_effective_size_before_input();
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.request_semantic_redraw_after_input();
        }
        self.app.route_client_events_from(
            client_id,
            vec![crate::raw_input::RawInputEvent::Paste(path)],
            false,
        );
        if let Some(canonical) = navigation_scope {
            self.finish_client_navigation_scope(client_id, canonical);
        }
        true
    }

    fn resolve_terminal_session_target(
        &mut self,
        client_id: u64,
        target: &str,
        action: &str,
    ) -> Option<String> {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(
                        format!(
                            "terminal session {action} failed: connection is not pending terminal session"
                        ),
                    ),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return None;
        }

        let Some(terminal_id) = self.resolve_terminal_target_id_string(target) else {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal session {action} failed: terminal target {target} not found"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return None;
        };

        Some(terminal_id)
    }

    fn observe_terminal_client(&mut self, client_id: u64, target: String) -> bool {
        let Some(terminal_id) = self.resolve_terminal_session_target(client_id, &target, "observe")
        else {
            return false;
        };

        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        client.mode = ClientConnectionMode::TerminalObserve {
            terminal_id: terminal_id.clone(),
        };
        client.pending_terminal_attach = false;
        client.navigation = None;
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        let was_foreground = self.foreground_client_id == Some(client_id);
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(client_id, cols, rows, terminal_id = %terminal_id, "terminal observe client connected");
        true
    }

    fn control_terminal_client(&mut self, client_id: u64, target: String, takeover: bool) -> bool {
        let Some(terminal_id) = self.resolve_terminal_session_target(client_id, &target, "control")
        else {
            return false;
        };

        self.attach_terminal_client(client_id, terminal_id, takeover)
    }

    fn handle_terminal_attach_scroll(
        &mut self,
        client_id: u64,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    ) -> bool {
        let Some(ClientConnection {
            mode: ClientConnectionMode::TerminalAttach { terminal_id },
            ..
        }) = self.clients.get(&client_id)
        else {
            return false;
        };
        let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) else {
            return false;
        };

        if let Err(err) =
            apply_terminal_attach_scroll(runtime, source, direction, lines, column, row, modifiers)
        {
            warn!(client_id, terminal_id = %terminal_id, err = %err, "terminal attach scroll failed");
        }
        true
    }

    fn pane_effective_state(&self, pane_id: crate::layout::PaneId) -> crate::detect::AgentState {
        self.app
            .state
            .workspaces
            .iter()
            .find_map(|ws| {
                ws.tabs.iter().find_map(|tab| {
                    let pane = tab.panes.get(&pane_id)?;
                    self.app
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .map(|terminal| terminal.state)
                })
            })
            .unwrap_or(crate::detect::AgentState::Unknown)
    }

    fn pane_effective_agent_label(&self, pane_id: crate::layout::PaneId) -> Option<String> {
        self.app.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                let pane = tab.panes.get(&pane_id)?;
                self.app
                    .state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .and_then(|terminal| terminal.effective_agent_label())
                    .map(str::to_string)
            })
        })
    }

    fn forward_pane_state_update_notifications_to_clients(
        &mut self,
        update: &crate::app::actions::PaneStateUpdate,
    ) {
        let delivery = self
            .app
            .state
            .toast_config
            .delivery
            .effective(self.app.state.outer_terminal_focus);
        if self.app.state.toast_config.delay_seconds != 0 {
            return;
        }

        let is_active_tab = self
            .app
            .state
            .pane_is_in_active_tab(update.ws_idx, update.pane_id);
        let suppress_active_tab_notifications =
            self.active_tab_suppresses_notifications(is_active_tab);

        if !update.suppress_completion && self.app.state.sound.allows(update.known_agent) {
            if let Some(sound) =
                crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                    suppress_active_tab_notifications,
                    update.previous_state,
                    update.state,
                    update.previous_agent_label.as_deref(),
                    update.agent_label.as_deref(),
                )
            {
                self.send_notify_to_foreground_client(
                    protocol::NotifyKind::Sound,
                    sound_notify_message(sound),
                    None,
                    None,
                );
            }
        }

        if !should_forward_toast_to_clients(delivery) {
            return;
        }
        let Some(kind) = crate::app::actions::notification_toast_for_pane_state_update(
            suppress_active_tab_notifications,
            update,
        ) else {
            return;
        };
        let Some(ws) = self.app.state.workspaces.get(update.ws_idx) else {
            return;
        };
        let Some(agent_label) = update.agent_label.as_deref() else {
            return;
        };
        let event_text = match kind {
            crate::app::state::ToastKind::NeedsAttention => "needs attention",
            crate::app::state::ToastKind::Finished => "finished",
            crate::app::state::ToastKind::UpdateInstalled => "updated",
        };
        let workspace_label =
            ws.display_name_from(&self.app.state.terminals, &self.app.terminal_runtimes);
        let context = crate::app::actions::notification_context(
            ws,
            &workspace_label,
            update.ws_idx,
            update.pane_id,
        );
        let target = crate::app::state::ToastTarget {
            workspace_id: ws.id.clone(),
            pane_id: update.pane_id,
        };
        self.send_notify_to_foreground_client(
            toast_notify_kind(delivery)
                .expect("toast forwarding requires a client notification kind"),
            format!("{agent_label} {event_text}"),
            non_empty_body(&context),
            Some(&target),
        );
    }

    fn forward_agent_notification_delivery(
        &mut self,
        delivery: &crate::app::state::AgentNotificationDelivery,
    ) {
        let toast_delivery = self
            .app
            .state
            .toast_config
            .delivery
            .effective(self.app.state.outer_terminal_focus);
        if let Some(sound) = delivery.sound {
            self.send_notify_to_foreground_client(
                protocol::NotifyKind::Sound,
                sound_notify_message(sound),
                None,
                None,
            );
        }

        if should_forward_toast_to_clients(toast_delivery) {
            if let Some(toast) = &delivery.client_notification {
                self.send_notify_to_foreground_client(
                    toast_notify_kind(toast_delivery)
                        .expect("toast forwarding requires a client notification kind"),
                    &toast.title,
                    non_empty_body(&toast.context),
                    toast.target.as_ref(),
                );
            }
        }
    }

    fn send_notify_to_foreground_client(
        &mut self,
        kind: protocol::NotifyKind,
        message: impl Into<String>,
        body: Option<String>,
        target: Option<&crate::app::state::ToastTarget>,
    ) -> bool {
        let activation = (kind == protocol::NotifyKind::SystemToast)
            .then_some(target)
            .flatten()
            .zip(self.foreground_client_id)
            .map(
                |(target, recipient_client_id)| protocol::NotificationActivation {
                    recipient_client_id,
                    workspace_id: target.workspace_id.clone(),
                    pane_id: target.pane_id.raw(),
                },
            );
        self.send_to_foreground_client(ServerMessage::Notify {
            kind,
            message: message.into(),
            body,
            activation,
        })
    }

    fn send_flat_toast_to_foreground_client(
        &mut self,
        kind: protocol::NotifyKind,
        message: impl AsRef<str>,
        target: Option<&crate::app::state::ToastTarget>,
    ) -> bool {
        let (title, body) = crate::terminal_notify::split_message(message.as_ref());
        self.send_notify_to_foreground_client(kind, title, body.map(str::to_string), target)
    }

    fn handle_notification_show_api(
        &mut self,
        id: String,
        params: api::schema::NotificationShowParams,
    ) -> String {
        use api::schema::{NotificationShowReason, ResponseResult};

        let Some(title) = sanitize_notification_text(&params.title, 80) else {
            return serde_json::to_string(&api::schema::ErrorResponse {
                id,
                error: api::schema::ErrorBody {
                    code: "invalid_params".into(),
                    message: "notification title is empty".into(),
                },
            })
            .unwrap_or_else(|_| "{}".to_string());
        };

        self.sync_foreground_client_state();
        let delivery = self
            .app
            .state
            .toast_config
            .delivery
            .effective(self.app.state.outer_terminal_focus);
        match delivery {
            config::ToastDelivery::Off => {
                return serde_json::to_string(&api::schema::SuccessResponse {
                    id,
                    result: ResponseResult::NotificationShow {
                        shown: false,
                        reason: NotificationShowReason::Disabled,
                    },
                })
                .unwrap_or_else(|_| "{}".to_string());
            }
            config::ToastDelivery::Herdr => {
                let sound = params.sound;
                let response = self.app.handle_api_request_after_internal_events_drained(
                    api::schema::Request {
                        id,
                        method: api::schema::Method::NotificationShow(params),
                    },
                );
                if notification_show_response_shown(&response) {
                    self.forward_api_notification_sound(sound);
                }
                return response;
            }
            config::ToastDelivery::Terminal | config::ToastDelivery::System => {}
            config::ToastDelivery::Hybrid => {
                unreachable!("hybrid delivery must be resolved before notification routing")
            }
        }

        let body = params
            .body
            .as_deref()
            .and_then(|body| sanitize_notification_text(body, 240));
        if self.app.api_notification_rate_limited(Instant::now()) {
            return serde_json::to_string(&api::schema::SuccessResponse {
                id,
                result: ResponseResult::NotificationShow {
                    shown: false,
                    reason: NotificationShowReason::RateLimited,
                },
            })
            .unwrap_or_else(|_| "{}".to_string());
        }
        let kind = toast_notify_kind(delivery).expect("terminal/system delivery has notify kind");
        let shown = self.send_notify_to_foreground_client(kind, title, body, None);
        if shown {
            self.app.mark_api_notification_shown(Instant::now());
            self.forward_api_notification_sound(params.sound);
        }
        let reason = if shown {
            NotificationShowReason::Shown
        } else {
            NotificationShowReason::NoForegroundClient
        };

        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: ResponseResult::NotificationShow { shown, reason },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    /// Pulls only titles reported dirty by the PTY parser. A focused pane title
    /// is forwarded as an independent client side effect; only sidebar title
    /// tokens require a UI render.
    fn sync_terminal_title_sources(
        &mut self,
        sources: &HashSet<crate::layout::PaneId>,
    ) -> (bool, bool) {
        let focused_source = self
            .app
            .state
            .active
            .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
            .and_then(|workspace| workspace.focused_pane_id())
            .is_some_and(|pane_id| sources.contains(&pane_id));
        let changes = self.app.sync_terminal_titles(sources);
        let outer_title_synced = focused_source && self.app.window_title_uses_terminal_title();
        if outer_title_synced {
            self.sync_window_title();
        }
        (
            self.app.terminal_title_sidebar_changed(&changes),
            outer_title_synced,
        )
    }

    /// Renders `ui.window_title` against current session state. `None` means
    /// window titles are disabled or every token resolved empty, which leaves
    /// the client on Herdr's default title.
    fn configured_window_title(&self) -> Option<String> {
        self.app
            .window_title()
            .and_then(|title| crate::config::sanitize_window_title_text(&title))
    }

    /// Pushes the configured outer window title to the foreground client when it
    /// changed. Herdr consumes each pane's own `OSC 0`/`OSC 2`, so without this
    /// the host terminal title never follows the session — which is what window
    /// managers read for tab and group bar labels.
    fn sync_window_title(&mut self) {
        let title = match &self.api_window_title {
            Some(title) => Some(title.clone()),
            None if self.app.window_title_configured() => self.configured_window_title(),
            None => return,
        };
        if let (Some(client_id), Some((sent_client_id, sent_title))) =
            (self.foreground_client_id, self.sent_window_title.as_ref())
        {
            if *sent_client_id == client_id && *sent_title == title {
                return;
            }
        }
        self.send_window_title(title);
    }

    /// Sends a window title and remembers it only when a foreground client took
    /// it, so the next client to attach is written to rather than skipped.
    fn send_window_title(&mut self, title: Option<String>) -> bool {
        let Some(client_id) = self.foreground_client_id else {
            self.sent_window_title = None;
            return false;
        };
        // A detached client keeps its entry with no writer, and a targeted send
        // to one reports success without queuing anything. Caching the title
        // against that client would skip the send once it attaches again.
        if self
            .clients
            .get(&client_id)
            .is_none_or(|client| client.writer.is_none())
        {
            self.sent_window_title = None;
            return false;
        }
        let sent = self.send_to_client(
            client_id,
            ServerMessage::WindowTitle {
                title: title.clone(),
            },
        );
        self.sent_window_title = sent.then_some((client_id, title));
        sent
    }

    fn handle_client_window_title_api(&mut self, id: String, title: Option<String>) -> String {
        use api::schema::{ClientWindowTitleReason, ResponseResult};

        let title = match title {
            Some(title) => match crate::config::sanitize_window_title_text(&title) {
                Some(title) => Some(title),
                None => {
                    return serde_json::to_string(&api::schema::ErrorResponse {
                        id,
                        error: api::schema::ErrorBody {
                            code: "invalid_params".into(),
                            message: "window title is empty".into(),
                        },
                    })
                    .unwrap_or_else(|_| "{}".to_string());
                }
            },
            None => None,
        };
        let set_title = title.is_some();
        // An explicit title suppresses `ui.window_title` until it is cleared,
        // and clearing restores the configured title rather than only "herdr".
        self.api_window_title = title.clone();
        let title = title.or_else(|| self.configured_window_title());
        let changed = self.send_window_title(title);
        let reason = match (changed, set_title) {
            (true, true) => ClientWindowTitleReason::Set,
            (true, false) => ClientWindowTitleReason::Cleared,
            (false, _) => ClientWindowTitleReason::NoForegroundClient,
        };
        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: ResponseResult::ClientWindowTitle { changed, reason },
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    fn forward_api_notification_sound(&mut self, sound: api::schema::NotificationShowSound) {
        let Some(sound) = sound.to_sound() else {
            return;
        };
        self.send_notify_to_foreground_client(
            protocol::NotifyKind::Sound,
            sound_notify_message(sound),
            None,
            None,
        );
    }

    /// Handles a single internal event with forwarding logic for clipboard,
    /// sound, and toast notifications to connected clients.
    ///
    /// ALL internal events MUST be routed through this method to ensure
    /// clipboard/notify forwarding is never bypassed. Do not call
    /// `self.app.handle_internal_event()` directly for any internal event
    /// in the headless server — use this method instead.
    ///
    /// Returns true if the event changed visual state (requiring a re-render).
    fn handle_internal_event_with_forwarding(&mut self, ev: AppEvent) -> bool {
        let private_pane_id = match &ev {
            AppEvent::PaneDied { pane_id, .. }
            | AppEvent::RemoteExecutionReady { pane_id, .. }
            | AppEvent::TerminalBell { pane_id, .. }
            | AppEvent::PaneClipboardWrite { pane_id, .. }
            | AppEvent::TerminalCwdReported { pane_id, .. } => Some(*pane_id),
            _ => None,
        };
        if private_pane_id.is_some_and(|pane_id| self.retired_private_pane_ids.contains(&pane_id)) {
            return false;
        }
        if let Some(owner_id) = private_pane_id.and_then(|pane_id| {
            self.private_surface_candidates
                .iter()
                .find_map(|(&client_id, candidate)| {
                    (candidate.surface.pane_id() == pane_id).then_some(client_id)
                })
        }) {
            match &ev {
                AppEvent::RemoteExecutionReady { .. } => {
                    let Some(candidate) = self.private_surface_candidates.remove(&owner_id) else {
                        return false;
                    };
                    if Instant::now() >= candidate.deadline {
                        self.retire_private_surface_candidate(
                            candidate,
                            "plugin_pane_open_failed",
                            "remote private popup did not become ready before timeout",
                        );
                        return false;
                    }
                    let activated = self.activate_private_surface(owner_id, candidate.surface);
                    if let Some(pending) = candidate.pending_response {
                        let response = if activated {
                            Self::private_surface_ok_response(pending.id.clone())
                        } else {
                            Self::private_surface_error_response(
                                pending.id.clone(),
                                "view_not_found",
                                "view disconnected before remote popup became ready",
                            )
                        };
                        Self::respond_pending_private_surface(pending, response);
                    }
                    return activated;
                }
                AppEvent::PaneDied { pane_id, .. } => {
                    if let Some(candidate) = self.private_surface_candidates.remove(&owner_id) {
                        warn!(
                            client_id = owner_id,
                            pane = pane_id.raw(),
                            "remote private popup exited before execution became ready; keeping existing surface"
                        );
                        self.retire_private_surface_candidate(
                            candidate,
                            "plugin_pane_open_failed",
                            "remote private popup exited before execution became ready",
                        );
                    }
                    return false;
                }
                AppEvent::TerminalBell { .. }
                | AppEvent::PaneClipboardWrite { .. }
                | AppEvent::TerminalCwdReported { .. } => return false,
                _ => {}
            }
        }
        if let Some(owner_id) = private_pane_id.and_then(|pane_id| {
            self.clients.iter().find_map(|(&client_id, client)| {
                client
                    .private_surface
                    .as_ref()
                    .is_some_and(|surface| surface.pane_id() == pane_id)
                    .then_some(client_id)
            })
        }) {
            match ev {
                AppEvent::PaneDied { .. } => {
                    self.close_private_surface(owner_id);
                    return true;
                }
                AppEvent::TerminalBell { count, .. } => {
                    self.send_to_client(owner_id, ServerMessage::TerminalBell { count });
                    return false;
                }
                AppEvent::TerminalCwdReported { .. } => return false,
                AppEvent::RemoteExecutionReady { .. } => return false,
                AppEvent::PaneClipboardWrite { content, .. } => {
                    let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                    self.send_to_client(owner_id, ServerMessage::Clipboard { data });
                    return false;
                }
                _ => {}
            }
        }
        if let Some((owner_id, pane_id)) = private_pane_id.and_then(|pane_id| {
            self.clients.iter().find_map(|(&client_id, client)| {
                client
                    .private_omp_guest
                    .as_ref()
                    .is_some_and(|guest| guest.runtime_pane_id() == pane_id)
                    .then_some((client_id, pane_id))
            })
        }) {
            let active = self.private_omp_guest_surface_active(owner_id, pane_id);
            match &ev {
                AppEvent::TerminalBell { count, .. } => {
                    if active {
                        self.send_to_client(
                            owner_id,
                            ServerMessage::TerminalBell { count: *count },
                        );
                    }
                    return false;
                }
                AppEvent::PaneClipboardWrite { content, .. } => {
                    if active {
                        let data =
                            base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                        self.send_to_client(owner_id, ServerMessage::Clipboard { data });
                    }
                    return false;
                }
                AppEvent::RemoteExecutionReady { .. } | AppEvent::TerminalCwdReported { .. } => {
                    return false
                }
                _ => {}
            }
        }
        if matches!(
            &ev,
            AppEvent::TerminalBell { pane_id, .. }
                | AppEvent::PaneClipboardWrite { pane_id, .. }
                | AppEvent::TerminalCwdReported { pane_id, .. }
                if !self.shared_pane_exists(*pane_id)
        ) {
            return false;
        }
        match &ev {
            AppEvent::TerminalBell { pane_id, count } => {
                if !self.send_to_foreground_client(ServerMessage::TerminalBell { count: *count }) {
                    debug!(
                        pane = pane_id.raw(),
                        count, "dropped terminal bell without a foreground client"
                    );
                }
                false
            }
            AppEvent::ClipboardWrite { content } | AppEvent::PaneClipboardWrite { content, .. } => {
                // Clipboard writes are client-local side effects. Shared panes route to the
                // foreground client; private panes returned above route to their owner.
                let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                if self.send_to_foreground_client(ServerMessage::Clipboard { data }) {
                    self.app.show_clipboard_feedback();
                }
                true
            }
            AppEvent::PrefixInputSource { active } => {
                // Input-source switching is a client-local host side effect; forward it to the
                // foreground client (which owns the real TIS switch + run-loop pump), like clipboard.
                self.send_to_foreground_client(ServerMessage::PrefixInputSource {
                    active: *active,
                });
                true
            }
            AppEvent::OpenUrl { url, source_id } => {
                if crate::web_url::safe_web_url(url).is_some() {
                    self.send_to_client(*source_id, ServerMessage::OpenUrl { url: url.clone() });
                }
                false
            }
            AppEvent::StateChanged { pane_id, agent, .. } => {
                let toast_before = self.app.state.toast.clone();
                let pane_id_val = *pane_id;
                let agent_val = *agent;
                let prev_state = self.pane_effective_state(pane_id_val);
                let prev_agent_label = self.pane_effective_agent_label(pane_id_val);

                self.sync_foreground_client_state();
                let suppress_completion = self
                    .app
                    .handle_internal_event_with_pane_updates(ev)
                    .iter()
                    .any(|update| update.pane_id == pane_id_val && update.suppress_completion);
                let is_active_tab = self
                    .app
                    .state
                    .active
                    .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
                    .is_some_and(|ws| {
                        ws.find_tab_index_for_pane(pane_id_val)
                            .is_some_and(|tab_idx| ws.active_tab_index() == tab_idx)
                    });
                let suppress_active_tab_notifications =
                    self.active_tab_suppresses_notifications(is_active_tab);
                let next_state = self.pane_effective_state(pane_id_val);
                let next_agent_label = self.pane_effective_agent_label(pane_id_val);
                let toast_delivery = self
                    .app
                    .state
                    .toast_config
                    .delivery
                    .effective(self.app.state.outer_terminal_focus);

                if !suppress_completion
                    && self.app.state.toast_config.delay_seconds == 0
                    && self.app.state.sound.allows(agent_val)
                {
                    if let Some(sound) =
                        crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                            next_agent_label.as_deref(),
                        )
                    {
                        self.send_notify_to_foreground_client(
                            protocol::NotifyKind::Sound,
                            sound_notify_message(sound),
                            None,
                            None,
                        );
                    }
                }

                let toast = if !suppress_completion
                    && self.app.state.toast_config.delay_seconds == 0
                    && should_forward_toast_to_clients(toast_delivery)
                {
                    if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                        self.app.state.toast.as_ref().and_then(|toast| {
                            toast.target.clone().map(|target| {
                                (format!("{}: {}", toast.title, toast.context), target)
                            })
                        })
                    } else {
                        toast_message_from_state_change(
                            &self.app.state,
                            &self.app.terminal_runtimes,
                            pane_id_val,
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                        )
                    }
                } else {
                    None
                };

                if let Some((msg, target)) = toast {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(toast_delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                        Some(&target),
                    );
                }

                true
            }
            AppEvent::HookStateReported {
                pane_id,
                agent_label,
                ..
            } => {
                let toast_before = self.app.state.toast.clone();
                let pane_id_val = *pane_id;
                let agent_val = crate::detect::parse_agent_label(agent_label);
                let prev_state = self.pane_effective_state(pane_id_val);
                let prev_agent_label = self.pane_effective_agent_label(pane_id_val);

                self.sync_foreground_client_state();
                let suppress_completion = self
                    .app
                    .handle_internal_event_with_pane_updates(ev)
                    .iter()
                    .any(|update| update.pane_id == pane_id_val && update.suppress_completion);
                let is_active_tab = self
                    .app
                    .state
                    .active
                    .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
                    .is_some_and(|ws| {
                        ws.find_tab_index_for_pane(pane_id_val)
                            .is_some_and(|tab_idx| ws.active_tab_index() == tab_idx)
                    });
                let suppress_active_tab_notifications =
                    self.active_tab_suppresses_notifications(is_active_tab);
                let next_state = self.pane_effective_state(pane_id_val);
                let next_agent_label = self.pane_effective_agent_label(pane_id_val);
                let toast_delivery = self
                    .app
                    .state
                    .toast_config
                    .delivery
                    .effective(self.app.state.outer_terminal_focus);

                if !suppress_completion
                    && self.app.state.toast_config.delay_seconds == 0
                    && self.app.state.sound.allows(agent_val)
                {
                    if let Some(sound) =
                        crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                            next_agent_label.as_deref(),
                        )
                    {
                        self.send_notify_to_foreground_client(
                            protocol::NotifyKind::Sound,
                            sound_notify_message(sound),
                            None,
                            None,
                        );
                    }
                }

                let toast = if !suppress_completion
                    && self.app.state.toast_config.delay_seconds == 0
                    && should_forward_toast_to_clients(toast_delivery)
                {
                    if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                        self.app.state.toast.as_ref().and_then(|toast| {
                            toast.target.clone().map(|target| {
                                (format!("{}: {}", toast.title, toast.context), target)
                            })
                        })
                    } else {
                        toast_message_from_state_change(
                            &self.app.state,
                            &self.app.terminal_runtimes,
                            pane_id_val,
                            suppress_active_tab_notifications,
                            prev_state,
                            next_state,
                            prev_agent_label.as_deref(),
                        )
                    }
                } else {
                    None
                };

                if let Some((msg, target)) = toast {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(toast_delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                        Some(&target),
                    );
                }

                true
            }
            AppEvent::UpdateReady {
                version,
                install_command,
            } => {
                let toast_before = self.app.state.toast.clone();
                let version = version.clone();
                let install_command = install_command.clone();

                self.app.handle_internal_event(ev);
                let toast_delivery = self
                    .app
                    .state
                    .toast_config
                    .delivery
                    .effective(self.app.state.outer_terminal_focus);
                let toast_msg = if should_forward_toast_to_clients(toast_delivery) {
                    if self.app.state.toast.is_some() && self.app.state.toast != toast_before {
                        self.app
                            .state
                            .toast
                            .as_ref()
                            .map(|toast| format!("{}: {}", toast.title, toast.context))
                    } else {
                        Some(format!(
                            "v{version} available: {}",
                            crate::update::update_install_instruction(&install_command)
                        ))
                    }
                } else {
                    None
                };

                if let Some(msg) = toast_msg {
                    self.send_flat_toast_to_foreground_client(
                        toast_notify_kind(toast_delivery)
                            .expect("toast forwarding requires a client notification kind"),
                        msg,
                        None,
                    );
                }

                true
            }
            AppEvent::PaneDied { pane_id, child_pid } => {
                let pane_id_val = *pane_id;
                if let Some(client_id) = self.clients.iter().find_map(|(&client_id, client)| {
                    client
                        .private_omp_guest
                        .as_ref()
                        .is_some_and(|guest| guest.runtime_pane_id() == pane_id_val)
                        .then_some(client_id)
                }) {
                    self.detach_failed_private_omp_guest(client_id);
                    return true;
                }
                if let Some(event_child_pid) = child_pid {
                    let current_child_pid = self
                        .app
                        .state
                        .terminal_id_for_runtime_pane(pane_id_val)
                        .and_then(|terminal_id| self.app.terminal_runtimes.get(&terminal_id))
                        .and_then(crate::terminal::TerminalRuntime::child_pid);
                    if current_child_pid != Some(*event_child_pid) {
                        debug!(
                            pane = pane_id_val.raw(),
                            event_child_pid,
                            current_child_pid,
                            "ignoring stale PaneDied event before headless publication"
                        );
                        return false;
                    }
                }

                let terminal_id = self.app.state.workspaces.iter().find_map(|ws| {
                    ws.tabs.iter().find_map(|tab| {
                        tab.panes
                            .get(pane_id)
                            .map(|pane| pane.attached_terminal_id.to_string())
                    })
                });
                if let Some(update) = self
                    .app
                    .state
                    .publish_pane_process_exit_if_agent(pane_id_val)
                {
                    self.app.emit_pane_state_update(&update);
                    self.forward_pane_state_update_notifications_to_clients(&update);
                }

                self.app.handle_internal_event(ev);

                if self.app.find_pane(pane_id_val).is_none() {
                    if let Some(terminal_id) = terminal_id {
                        self.shutdown_terminal_stream_clients(
                            &terminal_id,
                            format!("terminal {terminal_id} exited"),
                        );
                    }
                }

                true
            }
            _ => self.app.handle_internal_event_with_render_impact(ev),
        }
    }

    /// Drains internal events, forwarding clipboard, sound, toast, and URL-opening
    /// notifications to connected clients instead of processing them locally.
    ///
    /// In the monolithic mode:
    /// - `ClipboardWrite` events are written to stdout via `write_osc52_bytes`.
    /// - Sound notifications are played locally via `sound::play`.
    /// - Toast notifications are set on AppState and rendered into the frame.
    /// - `OpenUrl` events open their safe HTTP(S) URL on the local desktop.
    ///
    /// In the headless server, there is no stdout terminal, audio subsystem, or local desktop,
    /// so we:
    /// - Forward `ClipboardWrite` as `ServerMessage::Clipboard` to the foreground client only.
    /// - Forward `OpenUrl` as `ServerMessage::OpenUrl` to the originating input client only.
    /// - Detect when a sound would be played and forward as
    ///   `ServerMessage::Notify { kind: Sound }` to the foreground client.
    /// - Detect when a toast is set on AppState and forward as
    ///   `ServerMessage::Notify` to the foreground client for terminal/system delivery.
    fn drain_internal_events_with_forwarding(&mut self) -> bool {
        self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT)
            .1
    }
    #[cfg(any(unix, test))]
    fn drain_internal_event_snapshot_with_forwarding(&mut self) -> bool {
        let queued = self.app.event_rx.len();
        self.drain_internal_events_with_forwarding_up_to(queued).1
    }

    fn drain_all_internal_events_with_forwarding(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event || self.should_quit.load(Ordering::Acquire) {
                break;
            }
        }
        changed
    }

    fn drain_internal_events_with_forwarding_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.app.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        (had_event, changed)
    }

    fn drain_client_config_reload_request(&mut self) {
        if !self.app.state.request_client_config_reload {
            return;
        }
        self.app.state.request_client_config_reload = false;
        self.send_to_all_clients(ServerMessage::ReloadSoundConfig);
    }

    /// Encodes a server message into a length-prefixed frame.
    fn frame_server_message(msg: &ServerMessage) -> Result<Vec<u8>, protocol::FramingError> {
        Self::frame_server_message_with_max(msg, MAX_FRAME_SIZE)
    }

    /// Encodes a server message using an explicit payload cap.
    fn frame_server_message_with_max(
        msg: &ServerMessage,
        max_frame_size: usize,
    ) -> Result<Vec<u8>, protocol::FramingError> {
        let mut framed = Vec::new();
        protocol::write_message(&mut framed, msg)?;
        let payload_len = framed.len().saturating_sub(4);
        if payload_len > max_frame_size {
            return Err(protocol::FramingError::Oversized {
                claimed: payload_len,
                max: max_frame_size,
            });
        }
        Ok(framed)
    }

    /// Sends a message to all connected clients.
    /// Broken connections are tracked and cleaned up.
    fn send_to_all_clients(&mut self, msg: ServerMessage) {
        let serialized = match Self::frame_server_message(&msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(err = %err, "failed to serialize message for clients");
                return;
            }
        };

        let mut broken_clients: Vec<u64> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if let Some(writer) = &client.writer {
                if writer.control.send(serialized.clone()).is_err() {
                    debug!(client_id, "client writer channel closed during broadcast");
                    broken_clients.push(client_id);
                }
            }
        }

        // Remove broken clients and clear any OMP attachment before the socket
        // reader can report a separate disconnect.
        for client_id in broken_clients {
            self.remove_failed_client_and_resize_if_needed(client_id);
        }
    }

    /// Sends a client-local side effect to the foreground client only.
    fn send_to_foreground_client(&mut self, msg: ServerMessage) -> bool {
        let Some(client_id) = self.foreground_client_id else {
            return false;
        };
        self.send_to_client(client_id, msg)
    }

    /// Sends a message to a specific client. Returns false if the client
    /// was not found or the send failed (client removed).
    fn send_to_client(&mut self, client_id: u64, msg: ServerMessage) -> bool {
        let serialized = match Self::frame_server_message(&msg) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize message for client");
                return false;
            }
        };

        if let Some(client) = self.clients.get(&client_id) {
            if let Some(writer) = &client.writer {
                if writer.control.send(serialized).is_err() {
                    debug!(
                        client_id,
                        "client writer channel closed during targeted send"
                    );
                    self.remove_failed_client_and_resize_if_needed(client_id);
                    return false;
                }
            }
            true
        } else {
            false
        }
    }

    fn shutdown_terminal_stream_clients(&mut self, terminal_id: &str, reason: String) {
        let client_ids = terminal_stream_client_ids(&self.clients, terminal_id);

        for client_id in client_ids {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(reason.clone()),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn send_terminal_stream_detach_shutdown(&mut self, client_id: u64) {
        if matches!(
            self.clients.get(&client_id).map(|client| &client.mode),
            Some(
                ClientConnectionMode::TerminalAttach { .. }
                    | ClientConnectionMode::TerminalObserve { .. }
            )
        ) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some("detached".to_owned()),
                },
            );
        }
    }

    #[cfg(unix)]
    fn disconnect_all_clients_for_handoff(&mut self) {
        let canonical_navigation = ClientNavigationState::capture(&self.app.state);
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        for client_id in client_ids {
            self.send_client_graphics_cleanup(client_id);
            self.send_to_client(client_id, live_handoff_client_message());
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.writer = None;
            }
            let _ = self.remove_client(client_id);
        }
        self.foreground_client_id = None;
        canonical_navigation.apply_to(&mut self.app.state);
        self.sync_foreground_client_state();
        self.resize_shared_runtime_to_effective_size();
    }

    fn attach_terminal_client(
        &mut self,
        client_id: u64,
        terminal_id: String,
        takeover: bool,
    ) -> bool {
        if !self.client_is_pending_terminal_mode(client_id) {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(
                        "terminal attach failed: connection is not pending terminal attach"
                            .to_owned(),
                    ),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        let Some(real_terminal_id) = self.terminal_id_by_string(&terminal_id) else {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal attach failed: terminal {terminal_id} not found"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        };

        if self
            .pending_alt_screen_reads
            .iter()
            .any(|pending| pending.terminal_id == real_terminal_id)
        {
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some(format!(
                        "terminal attach failed: terminal {terminal_id} has a read in progress; retry"
                    )),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);
            return false;
        }

        if let Some(existing_owner) = self.terminal_attach_owners.get(&terminal_id).copied() {
            if existing_owner != client_id && !takeover {
                self.send_to_client(
                    client_id,
                    ServerMessage::ServerShutdown {
                        reason: Some(format!(
                            "terminal attach failed: terminal {terminal_id} already has an attached client; retry with --takeover"
                        )),
                    },
                );
                self.remove_client_and_resize_if_needed(client_id);
                return false;
            }
            if existing_owner != client_id {
                self.send_to_client(
                    existing_owner,
                    ServerMessage::ServerShutdown {
                        reason: Some("terminal attach taken over".to_owned()),
                    },
                );
                self.remove_client_and_resize_if_needed(existing_owner);
            }
        }

        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        let cell_size = client.cell_size;
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.clone(),
        };
        client.pending_terminal_attach = false;
        client.navigation = None;
        client.render_state.reset_baseline();
        client.last_activity = stamp;
        let was_foreground = self.foreground_client_id == Some(client_id);
        if was_foreground {
            self.promote_latest_remaining_client();
        }

        info!(client_id, cols, rows, terminal_id = %terminal_id, "terminal attach client connected");
        self.terminal_attach_owners
            .insert(terminal_id.clone(), client_id);
        self.app
            .state
            .direct_attach_resize_locks
            .insert(real_terminal_id.clone());
        self.app
            .start_pending_agent_resume_for_terminal(&real_terminal_id, rows, cols, true);
        if let Some(runtime) = self.app.terminal_runtimes.get(&real_terminal_id) {
            runtime.resize(rows, cols, cell_size.width_px, cell_size.height_px);
        }
        true
    }

    fn client_is_pending_terminal_mode(&self, client_id: u64) -> bool {
        self.clients.get(&client_id).is_some_and(|client| {
            client.pending_terminal_attach && matches!(client.mode, ClientConnectionMode::App)
        })
    }
    fn handle_private_surface_input_events(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> bool {
        let host_surface_redraw = crate::raw_input::events_require_host_surface_redraw(
            &events,
            self.app.state.redraw_on_focus_gained,
        );
        let mouse_scroll_lines = self.app.state.mouse_scroll_lines;
        let Some(view_id) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.view_id.clone())
        else {
            return false;
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.update_outer_focus_from_events(&events);
        let theme_changed = client.update_host_theme_from_events(&events);
        if host_surface_redraw {
            client.request_repaint();
            client.defer_full_render();
        } else if !events.is_empty() {
            client.request_semantic_redraw_after_input();
        }
        let theme = client.host_terminal_theme;
        let appearance = client.host_terminal_appearance;
        if theme_changed {
            if let Some(surface) = client.private_surface.as_ref() {
                surface.apply_host_theme(theme, appearance);
            }
        }

        for event in events {
            let click = {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                let Some(surface) = client.private_surface.as_mut() else {
                    return false;
                };
                surface.route_event(event, mouse_scroll_lines)
            };
            let Some(click) = click else {
                continue;
            };

            let mouse = click.mouse;
            let Some(source_pane_id) = self.app.private_popup_source_pane_id(click.origin) else {
                self.close_private_surface(client_id);
                return true;
            };
            let Some(canonical) = self.begin_client_navigation_scope(client_id) else {
                if let Some(surface) = self
                    .clients
                    .get_mut(&client_id)
                    .and_then(|client| client.private_surface.as_mut())
                {
                    surface.replay_rejected_link_click(mouse, mouse_scroll_lines);
                }
                continue;
            };
            let mut open_url = None;
            let activated = self.app.activate_link_once_from_source_with_fallback(
                client_id,
                &source_pane_id,
                click.url,
                &view_id,
                |url| {
                    open_url = Some(url);
                    true
                },
            );
            self.finish_client_navigation_scope(client_id, canonical);
            if let Some(url) = open_url {
                self.send_to_client(client_id, ServerMessage::OpenUrl { url });
            }
            if let Some(surface) = self
                .clients
                .get_mut(&client_id)
                .and_then(|client| client.private_surface.as_mut())
            {
                if activated {
                    surface.mark_link_click_activated();
                } else {
                    surface.replay_rejected_link_click(mouse, mouse_scroll_lines);
                }
            }
        }
        true
    }

    /// Handles a server event. Returns true if the event requires a re-render.
    fn apply_omp_messages(&mut self, messages: Vec<(u64, ServerMessage)>) -> bool {
        let mut render = false;
        let mut teardown = Vec::new();
        for (client_id, message) in messages {
            let private = self
                .clients
                .get(&client_id)
                .is_some_and(|client| client.private_omp_guest.is_some());
            if !private {
                self.send_to_client(client_id, message);
                continue;
            }
            match message {
                ServerMessage::OmpPane {
                    attachment_epoch,
                    controller,
                    state,
                    ..
                } => {
                    if let Some(guest) = self
                        .clients
                        .get(&client_id)
                        .and_then(|client| client.private_omp_guest.as_ref())
                    {
                        guest.set_attachment_epoch(attachment_epoch);
                        guest.set_controller(controller);
                    }
                    if matches!(state, OmpPaneState::Failed { .. }) {
                        teardown.push(client_id);
                    }
                    render = true;
                }
                ServerMessage::OmpFrame { frame, .. } => {
                    let result =
                        protocol::validate_omp_frame(&frame, OmpFrameDirection::HostToGuest)
                            .ok()
                            .and_then(|payload| std::str::from_utf8(payload).ok())
                            .map(|payload| {
                                format!(r#"{{"t":"frame","fromPeer":0,"frame":{payload}}}"#)
                            })
                            .and_then(|record| {
                                self.clients
                                    .get(&client_id)
                                    .and_then(|client| client.private_omp_guest.as_ref())
                                    .and_then(|guest| guest.send_host_frame(&record).ok())
                            });
                    if result.is_none() {
                        teardown.push(client_id);
                    }
                }
                ServerMessage::OmpError { code, message, .. } => {
                    warn!(client_id, %code, %message, "private OMP renderer route failed");
                    teardown.push(client_id);
                }
                message => {
                    self.send_to_client(client_id, message);
                }
            }
        }
        teardown.sort_unstable();
        teardown.dedup();
        for client_id in teardown {
            self.detach_failed_private_omp_guest(client_id);
            render = true;
        }
        render
    }

    fn enforce_omp_maintenance(&mut self) -> bool {
        let messages = self.omp_service.enforce_maintenance(&self.clients);
        if messages.is_empty() {
            return false;
        }
        self.apply_omp_messages(messages) | self.reconcile_omp_renderers()
    }

    fn detach_failed_private_omp_guest(&mut self, client_id: u64) {
        let route = self
            .clients
            .get_mut(&client_id)
            .and_then(|client| client.private_omp_guest.take())
            .map(|guest| guest.route().clone());
        let Some(route) = route else {
            return;
        };
        warn!(client_id, pane_id = %route.pane_id, "private OMP guest bridge failed; keeping host PTY masked");
        self.clear_private_omp_pending_route(client_id, &route);
        self.mark_private_omp_failed_with_retry(client_id, route);
        let messages = self
            .omp_service
            .detach_private_app(client_id, &self.clients);
        self.apply_omp_messages(messages);
    }

    fn private_omp_guest_failed(&self, client_id: u64) -> bool {
        self.clients
            .get(&client_id)
            .and_then(|client| client.private_omp_guest.as_ref())
            .is_some_and(PrivateOmpGuest::bridge_failed)
    }

    fn client_focused_pane(&self, client_id: u64) -> Option<(usize, crate::layout::PaneId)> {
        let client = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_full_app_client())?;
        let Some(navigation) = client.navigation.as_ref() else {
            return self.app.state.active.and_then(|ws_idx| {
                self.app
                    .state
                    .workspaces
                    .get(ws_idx)
                    .and_then(|workspace| workspace.focused_pane_id())
                    .map(|pane_id| (ws_idx, pane_id))
            });
        };
        let workspace_id = navigation.active_workspace_id.as_deref()?;
        let (ws_idx, workspace) = self
            .app
            .state
            .workspaces
            .iter()
            .enumerate()
            .find(|(_, workspace)| workspace.id == workspace_id)?;
        let tab_id = navigation.active_tab_by_workspace.get(&workspace.id)?;
        let (tab_idx, _) = workspace.tabs.iter().enumerate().find(|(tab_idx, _)| {
            workspace.public_tab_number(*tab_idx).is_some_and(|number| {
                crate::workspace::public_tab_id_for_number(&workspace.id, number) == *tab_id
            })
        })?;
        let pane_id = navigation.focused_pane_by_tab.get(tab_id)?;
        workspace.tabs[tab_idx]
            .layout
            .pane_ids()
            .into_iter()
            .find(|pane_id_val| {
                workspace
                    .public_pane_number(*pane_id_val)
                    .is_some_and(|number| {
                        crate::workspace::public_pane_id_for_number(&workspace.id, number)
                            == *pane_id
                    })
            })
            .map(|pane_id| (ws_idx, pane_id))
    }
    fn desired_private_omp_route(
        &self,
        client_id: u64,
        routes: &[OmpRouteKey],
    ) -> Option<OmpRouteKey> {
        let focused = self.client_focused_pane(client_id)?;
        routes
            .iter()
            .find(|route| self.app.parse_pane_id(&route.pane_id) == Some(focused))
            .cloned()
    }

    fn omp_renderer_route(key: &OmpRouteKey) -> crate::protocol::OmpRendererRoute {
        crate::protocol::OmpRendererRoute {
            pane_id: key.pane_id.clone(),
            omp_session_id: key.omp_session_id.clone(),
            route_generation: key.route_generation,
        }
    }

    fn omp_renderer_prefix(&self, client_id: u64) -> Option<crate::protocol::OmpRendererPrefix> {
        let prefix = self
            .clients
            .get(&client_id)
            .and_then(|client| client.keybindings.as_ref().map(|keys| keys.prefix))
            .unwrap_or(self.server_keybindings.prefix);
        Some(crate::protocol::OmpRendererPrefix {
            code: crate::protocol::ClientKeyCode::from_crossterm(prefix.0)?,
            modifiers: prefix.1.bits(),
        })
    }

    fn client_omp_surface_active(&self, client_id: u64) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let mode = client
            .navigation
            .as_ref()
            .map_or(self.app.state.mode, |navigation| {
                if navigation.findr.is_some() {
                    app::Mode::Findr
                } else {
                    navigation.non_findr_mode.unwrap_or(self.app.state.mode)
                }
            });
        client.private_surface.is_none()
            && client
                .navigation
                .as_ref()
                .is_none_or(|navigation| navigation.focused_workspace_plugin_pane.is_none())
            && mode == app::Mode::Terminal
            && self.app.state.popup_pane.is_none()
    }

    fn private_omp_guest_surface_active(
        &self,
        client_id: u64,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        self.client_omp_surface_active(client_id)
            && self
                .clients
                .get(&client_id)
                .and_then(|client| client.private_omp_guest.as_ref())
                .is_some_and(|guest| {
                    guest.runtime_pane_id() == pane_id
                        && guest.bridge_ready()
                        && !guest.bridge_failed()
                        && self.private_omp_pending_routes.get(&client_id) != Some(guest.route())
                })
    }

    fn client_has_ready_native_renderer(&self, client_id: u64) -> bool {
        self.clients
            .get(&client_id)
            .and_then(|client| client.omp_renderer_target.as_ref())
            .is_some_and(|target| target.ready)
            && self.omp_service.app_has_native_renderer(client_id)
    }

    fn native_omp_surface_active(&self, client_id: u64) -> bool {
        self.clients
            .get(&client_id)
            .and_then(|client| client.omp_renderer_target.as_ref())
            .is_some_and(|target| target.surface_active)
    }

    fn private_omp_fallback_ready(&self, client_id: u64, route: &OmpRouteKey) -> bool {
        self.clients
            .get(&client_id)
            .and_then(|client| client.private_omp_guest.as_ref())
            .is_some_and(|guest| guest.route() == route && guest.bridge_ready())
    }

    fn private_omp_fallback_terminally_failed(&self, client_id: u64, route: &OmpRouteKey) -> bool {
        self.private_omp_failed_routes.get(&client_id) == Some(route)
            && self
                .private_omp_retry_attempted_routes
                .get(&client_id)
                .is_some_and(|(attempted, state)| {
                    attempted == route && matches!(state, PrivateOmpRetryState::Consumed)
                })
    }

    fn private_omp_resolution_is_current(&self, client_id: u64, route: &OmpRouteKey) -> bool {
        let eligible = self.clients.get(&client_id).is_some_and(|client| {
            client.is_full_app_client()
                && client.committed_identity().is_some()
                && client.private_omp_guest.is_none()
        });
        if !eligible
            || self.client_has_ready_native_renderer(client_id)
            || self
                .private_omp_failed_routes
                .get(&client_id)
                .is_some_and(|failed| failed == route)
        {
            return false;
        }
        let mut routes = self.omp_service.live_route_keys();
        routes.sort_by(|left, right| {
            (&left.pane_id, &left.omp_session_id, left.route_generation).cmp(&(
                &right.pane_id,
                &right.omp_session_id,
                right.route_generation,
            ))
        });
        self.desired_private_omp_route(client_id, &routes).as_ref() == Some(route)
    }

    fn allocate_omp_renderer_launch_id(&mut self) -> u64 {
        let launch_id = self.next_omp_renderer_launch_id;
        self.next_omp_renderer_launch_id = self.next_omp_renderer_launch_id.wrapping_add(1).max(1);
        launch_id
    }

    fn update_omp_renderer_target(
        &mut self,
        client_id: u64,
        target: OmpRendererTargetState,
    ) -> bool {
        let previous = self
            .clients
            .get(&client_id)
            .and_then(|client| client.omp_renderer_target.as_ref())
            .cloned();
        if previous.as_ref() == Some(&target) {
            return false;
        }
        let activating = target.surface_active
            && previous
                .as_ref()
                .is_none_or(|previous| !previous.surface_active);
        let message = ServerMessage::OmpRendererTarget {
            launch_id: target.launch_id,
            target_app_client_id: client_id,
            route: target.route.clone(),
            bound: target.bound,
            surface_active: target.surface_active,
            prefix: target.prefix.clone(),
        };
        if activating {
            let Some(next_graphics_cache) = self.queue_native_omp_activation(client_id, &message)
            else {
                // RendererReady is one-shot; retain server-only readiness so reconciliation can retry.
                let Some(current) = self
                    .clients
                    .get_mut(&client_id)
                    .and_then(|client| client.omp_renderer_target.as_mut())
                    .filter(|current| current.launch_id == target.launch_id)
                else {
                    return false;
                };
                let ready_changed = current.ready != target.ready;
                current.ready = target.ready;
                return ready_changed;
            };
            self.retire_direct_graphics_for_client_without_notifications(client_id);
            self.commit_client_graphics_cleanup(client_id, next_graphics_cache);
            let Some(client) = self.clients.get_mut(&client_id) else {
                return false;
            };
            client.omp_renderer_target = Some(target);
            client.request_repaint();
            return true;
        }
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.omp_renderer_target = Some(target);
        client.request_repaint();
        self.send_to_client(client_id, message);
        true
    }

    fn reconcile_native_omp_renderers(&mut self) -> bool {
        let mut routes = self.omp_service.live_route_keys();
        routes.sort_by(|left, right| {
            (&left.pane_id, &left.omp_session_id, left.route_generation).cmp(&(
                &right.pane_id,
                &right.omp_session_id,
                right.route_generation,
            ))
        });
        let mut client_ids = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| client.is_full_app_client().then_some(client_id))
            .collect::<Vec<_>>();
        client_ids.sort_unstable();

        let mut changed = false;
        for client_id in client_ids {
            let Some(prefix) = self.omp_renderer_prefix(client_id) else {
                continue;
            };
            let eligible = self.clients.get(&client_id).is_some_and(|client| {
                client.omp_renderer_capabilities.client_local_native
                    && client.renderer_binding_token.is_some()
                    && client.committed_identity().is_some()
            });
            let desired_key = eligible
                .then(|| self.client_focused_pane(client_id))
                .flatten()
                .and_then(|focused| {
                    routes
                        .iter()
                        .find(|route| self.app.parse_pane_id(&route.pane_id) == Some(focused))
                })
                .cloned();
            let desired_route = desired_key.as_ref().map(Self::omp_renderer_route);
            let current = self
                .clients
                .get(&client_id)
                .and_then(|client| client.omp_renderer_target.clone());

            if current.as_ref().and_then(|target| target.route.as_ref()) != desired_route.as_ref() {
                if let Some(current) = current.as_ref().filter(|target| target.route.is_some()) {
                    changed |= self.update_omp_renderer_target(
                        client_id,
                        OmpRendererTargetState {
                            launch_id: current.launch_id,
                            route: None,
                            bound: false,
                            ready: false,
                            prefix: prefix.clone(),
                            surface_active: false,
                        },
                    );
                    if self.omp_service.app_has_native_renderer(client_id) {
                        continue;
                    }
                }
                if let Some(route) = desired_route {
                    let launch_id = self.allocate_omp_renderer_launch_id();
                    changed |= self.update_omp_renderer_target(
                        client_id,
                        OmpRendererTargetState {
                            launch_id,
                            route: Some(route),
                            bound: false,
                            ready: false,
                            prefix,
                            surface_active: false,
                        },
                    );
                }
                continue;
            }

            let (Some(mut target), Some(key)) = (current, desired_key.as_ref()) else {
                continue;
            };
            let native_bound = self
                .omp_service
                .app_has_native_renderer_for_route(client_id, key);
            if !native_bound
                && target.bound
                && target.ready
                && !self.private_omp_fallback_ready(client_id, key)
                && !self.private_omp_fallback_terminally_failed(client_id, key)
            {
                continue;
            }
            target.bound = native_bound;
            target.ready &= target.bound;
            target.surface_active =
                target.bound && target.ready && self.client_omp_surface_active(client_id);
            target.prefix = prefix;
            changed |= self.update_omp_renderer_target(client_id, target);
        }
        changed
    }

    fn private_omp_executable_for_launch(
        &mut self,
        client_id: u64,
        route: &OmpRouteKey,
    ) -> Option<crate::update::OmpExecutable> {
        if let Some(executable) = self.private_omp_executable.clone() {
            match executable.verify() {
                Ok(()) => return Some(executable),
                Err(error) => {
                    warn!(%error, "cached private OMP executable failed revalidation");
                    self.private_omp_executable = None;
                }
            }
        }
        #[cfg(test)]
        if let Some(executable) = &self.private_omp_test_executable {
            return Some(crate::update::OmpExecutable::Explicit(executable.clone()));
        }

        if self.private_omp_resolving.is_none() {
            self.private_omp_resolving = Some((client_id, route.clone()));
            let event_tx = self.server_event_tx.clone();
            std::thread::spawn(move || {
                let result =
                    crate::update::server_private_omp_executable().and_then(|executable| {
                        executable.verify()?;
                        Ok(executable)
                    });
                let _ = event_tx.blocking_send(ServerEvent::OmpPrivateCompanionResolved { result });
            });
        }
        None
    }

    fn set_private_omp_pending_route(&mut self, client_id: u64, route: &OmpRouteKey) -> bool {
        if self.private_omp_pending_routes.get(&client_id) == Some(route) {
            return false;
        }
        let Some((_, pane_id)) = self.app.parse_pane_id(&route.pane_id) else {
            return false;
        };
        if !self.begin_omp_graphics_replacement(client_id, pane_id) {
            return false;
        }
        self.private_omp_pending_routes
            .insert(client_id, route.clone());
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.request_repaint();
        }
        true
    }

    fn clear_private_omp_pending_route(&mut self, client_id: u64, route: &OmpRouteKey) -> bool {
        if self.private_omp_pending_routes.get(&client_id) != Some(route) {
            return false;
        }
        self.private_omp_pending_routes.remove(&client_id);
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.request_repaint();
        }
        true
    }

    fn mark_private_omp_failed_with_retry(&mut self, client_id: u64, route: OmpRouteKey) {
        let should_retry = !self
            .private_omp_retry_attempted_routes
            .get(&client_id)
            .is_some_and(|(attempted, _)| attempted == &route);
        self.private_omp_failed_routes
            .insert(client_id, route.clone());
        if should_retry {
            let retry_id = self.next_private_omp_retry_id;
            self.next_private_omp_retry_id = retry_id.wrapping_add(1).max(1);
            self.private_omp_retry_attempted_routes.insert(
                client_id,
                (route.clone(), PrivateOmpRetryState::Pending(retry_id)),
            );
            let event_tx = self.server_event_tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(PRIVATE_OMP_COMPANION_RETRY_DELAY);
                let _ = event_tx.blocking_send(ServerEvent::OmpPrivateCompanionRetry {
                    client_id,
                    route,
                    retry_id,
                });
            });
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.request_repaint();
        }
    }

    fn clear_private_omp_failure_state(&mut self, client_id: u64, route: &OmpRouteKey) {
        if self.private_omp_failed_routes.get(&client_id) == Some(route) {
            self.private_omp_failed_routes.remove(&client_id);
        }
        if self
            .private_omp_retry_attempted_routes
            .get(&client_id)
            .is_some_and(|(attempted, _)| attempted == route)
        {
            self.private_omp_retry_attempted_routes.remove(&client_id);
        }
    }

    fn reconcile_private_omp_completion(&mut self) -> bool {
        self.reconcile_omp_renderers()
    }

    fn reconcile_omp_renderers(&mut self) -> bool {
        if !self.independent_omp_renderers_enabled() {
            return false;
        }
        let mut changed = self.reconcile_native_omp_renderers();
        changed |= self.reconcile_private_omp_guests();
        changed | self.reconcile_native_omp_renderers()
    }

    fn try_attach_private_omp_guest(&mut self, client_id: u64, route: OmpRouteKey) -> bool {
        if self
            .private_omp_failed_routes
            .get(&client_id)
            .is_some_and(|failed| failed == &route)
        {
            return false;
        }
        let eligible = self.clients.get(&client_id).is_some_and(|client| {
            client.is_full_app_client()
                && client.committed_identity().is_some()
                && client.private_omp_guest.is_none()
        }) && !self.client_has_ready_native_renderer(client_id);
        let Some((ws_idx, view_pane_id)) = eligible
            .then(|| self.app.parse_pane_id(&route.pane_id))
            .flatten()
        else {
            return false;
        };
        if self.client_focused_pane(client_id) != Some((ws_idx, view_pane_id)) {
            return false;
        }
        let mut changed = self.set_private_omp_pending_route(client_id, &route);
        if self.private_omp_pending_routes.get(&client_id) != Some(&route) {
            return changed;
        }
        let Some(omp_executable) = self.private_omp_executable_for_launch(client_id, &route) else {
            return changed;
        };
        let initial_inner = self
            .app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == view_pane_id)
            .map(|info| info.inner_rect);
        let messages = self
            .omp_service
            .attach_private_app(client_id, route.clone(), &self.clients);
        let attachment = messages.iter().find_map(|(target, message)| match message {
            ServerMessage::OmpPane {
                attachment_epoch,
                controller,
                state: OmpPaneState::Starting { .. } | OmpPaneState::Live { .. },
                ..
            } if *target == client_id => Some((*attachment_epoch, *controller)),
            _ => None,
        });
        let Some((attachment_epoch, controller)) = attachment else {
            changed |= self.clear_private_omp_pending_route(client_id, &route);
            self.apply_omp_messages(messages);
            return changed;
        };
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let (cols, rows) = client.terminal_size;
        let cell_size = client.cell_size;
        let terminal_theme = client.host_terminal_theme;
        let terminal_appearance = client.host_terminal_appearance;
        let Some(launch_env) = self
            .app
            .pane_launch_env(ws_idx, view_pane_id, Vec::new())
            .map(crate::pane::PaneLaunchEnv::without_pane_identity)
        else {
            self.clear_private_omp_pending_route(client_id, &route);
            self.mark_private_omp_failed_with_retry(client_id, route.clone());
            let cleanup = self
                .omp_service
                .detach_private_app(client_id, &self.clients);
            self.apply_omp_messages(cleanup);
            return true;
        };
        let cwd = self
            .app
            .launch_cwd_for_pane_in_workspace(ws_idx, view_pane_id)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let config = PrivateOmpGuestConfig {
            route: route.clone(),
            omp_executable,
            attachment_epoch,
            controller,
            pane_id: crate::layout::PaneId::alloc(),
            rows: initial_inner.map_or(rows.max(1), |inner| inner.height.max(1)),
            cols: initial_inner.map_or(cols.max(1), |inner| inner.width.max(1)),
            cwd,
            launch_env,
            scrollback_limit_bytes: self.app.state.pane_scrollback_limit_bytes,
            terminal_theme,
            terminal_appearance,
            events: self.app.event_tx.clone(),
            render_notify: self.app.render_notify.clone(),
            render_dirty: self.app.render_dirty.clone(),
        };
        match PrivateOmpGuest::spawn(config) {
            Ok(guest) => {
                let guest_rows = initial_inner.map_or(rows.max(1), |inner| inner.height.max(1));
                let guest_cols = initial_inner.map_or(cols.max(1), |inner| inner.width.max(1));
                guest.resize(
                    guest_rows,
                    guest_cols,
                    cell_size.width_px,
                    cell_size.height_px,
                );
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.private_omp_guest = Some(guest);
                    client.request_repaint();
                }
                self.apply_omp_messages(messages);
                true
            }
            Err(error) => {
                warn!(client_id, %error, "failed to spawn private OMP renderer");
                self.clear_private_omp_pending_route(client_id, &route);
                self.mark_private_omp_failed_with_retry(client_id, route);
                let cleanup = self
                    .omp_service
                    .detach_private_app(client_id, &self.clients);
                self.apply_omp_messages(cleanup);
                true
            }
        }
    }

    fn reconcile_private_omp_guests(&mut self) -> bool {
        let mut routes = self.omp_service.live_route_keys();
        routes.sort_by(|left, right| {
            (&left.pane_id, &left.omp_session_id, left.route_generation).cmp(&(
                &right.pane_id,
                &right.omp_session_id,
                right.route_generation,
            ))
        });
        let mut client_ids = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| {
                (client.is_full_app_client() && !self.client_has_ready_native_renderer(client_id))
                    .then_some(client_id)
            })
            .collect::<Vec<_>>();
        client_ids.sort_unstable();
        let mut changed = false;
        for client_id in client_ids {
            let desired_route = self.desired_private_omp_route(client_id, &routes);
            if self
                .private_omp_failed_routes
                .get(&client_id)
                .is_some_and(|failed| desired_route.as_ref() != Some(failed))
            {
                self.private_omp_failed_routes.remove(&client_id);
            }
            if self
                .private_omp_retry_attempted_routes
                .get(&client_id)
                .is_some_and(|(attempted, _)| desired_route.as_ref() != Some(attempted))
            {
                self.private_omp_retry_attempted_routes.remove(&client_id);
            }
            if let Some(pending_route) = self.private_omp_pending_routes.get(&client_id).cloned() {
                if desired_route.as_ref() != Some(&pending_route) {
                    changed |= self.clear_private_omp_pending_route(client_id, &pending_route);
                }
            }
            let current_route = self
                .clients
                .get(&client_id)
                .and_then(|client| client.private_omp_guest.as_ref())
                .map(|guest| guest.route().clone());
            if current_route != desired_route {
                if current_route.is_some() {
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.private_omp_guest.take();
                    }
                    let messages = self
                        .omp_service
                        .detach_private_app(client_id, &self.clients);
                    self.apply_omp_messages(messages);
                    changed = true;
                }
                if let Some(route) = desired_route {
                    changed |= self.try_attach_private_omp_guest(client_id, route);
                }
            }
        }
        changed
    }

    fn encode_private_omp_guest_frame(
        frame: &serde_json::value::RawValue,
    ) -> Result<Vec<u8>, protocol::OmpFrameError> {
        protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, frame.get().as_bytes())
    }

    fn drain_private_omp_guest_records(&mut self) -> bool {
        let failed_clients = self
            .clients
            .keys()
            .copied()
            .filter(|&client_id| self.private_omp_guest_failed(client_id))
            .collect::<Vec<_>>();
        if !failed_clients.is_empty() {
            for client_id in failed_clients {
                self.detach_failed_private_omp_guest(client_id);
            }
            self.reconcile_native_omp_renderers();
            return true;
        }

        let ready_routes = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| {
                let guest = client.private_omp_guest.as_ref()?;
                (guest.bridge_ready()
                    && self.private_omp_pending_routes.get(&client_id) == Some(guest.route()))
                .then(|| (client_id, guest.route().clone()))
            })
            .collect::<Vec<_>>();
        let mut render = false;
        for (client_id, route) in &ready_routes {
            render |= self.clear_private_omp_pending_route(*client_id, route);
        }
        if !ready_routes.is_empty() {
            render |= self.reconcile_omp_renderers();
        }

        let mut events = Vec::new();
        let mut invalid_clients = Vec::new();
        for (&client_id, client) in &mut self.clients {
            let Some(guest) = client.private_omp_guest.as_mut() else {
                continue;
            };
            let route = guest.route().clone();
            let attachment_epoch = guest.attachment_epoch();
            for record in guest.drain_guest_records() {
                match record {
                    PrivateOmpGuestRecord::Frame { frame, mutation } => {
                        let frame = match Self::encode_private_omp_guest_frame(&frame) {
                            Ok(frame) => frame,
                            Err(error) => {
                                warn!(client_id, %error, "invalid private OMP guest frame; retiring renderer");
                                invalid_clients.push(client_id);
                                break;
                            }
                        };
                        if mutation {
                            events.push(ServerEvent::OmpControl {
                                client_id,
                                pane_id: route.pane_id.clone(),
                                omp_session_id: route.omp_session_id.clone(),
                                route_generation: route.route_generation,
                                attachment_epoch,
                                action: OmpControlAction::Mutation { frame },
                            });
                        } else {
                            events.push(ServerEvent::OmpFrame {
                                client_id,
                                pane_id: route.pane_id.clone(),
                                omp_session_id: route.omp_session_id.clone(),
                                route_generation: route.route_generation,
                                attachment_epoch,
                                frame,
                            });
                        }
                    }
                    PrivateOmpGuestRecord::Control(action) => {
                        events.push(ServerEvent::OmpControl {
                            client_id,
                            pane_id: route.pane_id.clone(),
                            omp_session_id: route.omp_session_id.clone(),
                            route_generation: route.route_generation,
                            attachment_epoch,
                            action: match action {
                                PrivateOmpGuestControl::RequestController => {
                                    OmpControlAction::RequestController
                                }
                                PrivateOmpGuestControl::ReleaseController => {
                                    OmpControlAction::ReleaseController
                                }
                            },
                        });
                    }
                }
            }
        }
        invalid_clients.sort_unstable();
        invalid_clients.dedup();
        for client_id in invalid_clients {
            self.detach_failed_private_omp_guest(client_id);
            render = true;
        }
        for event in events {
            render |= self.handle_server_event(event);
        }
        render
    }

    fn native_bound_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        self.client_has_ready_native_renderer(client_id)
            .then_some(())?;
        let ws_idx = self.app.state.active?;
        let workspace = self.app.state.workspaces.get(ws_idx)?;
        let pane_id = workspace.focused_pane_id()?;
        let public_pane_id = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace.public_pane_number(pane_id)?,
        );
        self.omp_service
            .app_has_native_renderer_for_pane(client_id, &public_pane_id)
            .then_some(())?;
        self.app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .cloned()
    }

    fn failed_private_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        let route = self.private_omp_failed_routes.get(&client_id)?;
        let (ws_idx, pane_id) = self.app.parse_pane_id(&route.pane_id)?;
        (self.app.state.active == Some(ws_idx))
            .then(|| {
                self.app
                    .state
                    .view
                    .pane_infos
                    .iter()
                    .find(|info| info.id == pane_id)
                    .cloned()
            })
            .flatten()
    }

    fn pending_private_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        let route = self.private_omp_pending_routes.get(&client_id)?;
        let (ws_idx, pane_id) = self.app.parse_pane_id(&route.pane_id)?;
        (self.app.state.active == Some(ws_idx))
            .then(|| {
                self.app
                    .state
                    .view
                    .pane_infos
                    .iter()
                    .find(|info| info.id == pane_id)
                    .cloned()
            })
            .flatten()
    }

    fn independent_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        self.native_bound_omp_pane_info(client_id)
            .or_else(|| self.failed_private_omp_pane_info(client_id))
            .or_else(|| self.pending_private_omp_pane_info(client_id))
    }
    fn activate_native_omp_link(&mut self, client_id: u64, launch_id: u64, url: String) -> bool {
        if !self.client_omp_surface_active(client_id) {
            return false;
        }
        let Some(route) = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_full_app_client())
            .and_then(|client| client.omp_renderer_target.as_ref())
            .filter(|target| {
                target.launch_id == launch_id
                    && target.bound
                    && target.ready
                    && target.surface_active
            })
            .and_then(|target| target.route.clone())
        else {
            return false;
        };
        let key = OmpRouteKey {
            pane_id: route.pane_id.clone(),
            omp_session_id: route.omp_session_id,
            route_generation: route.route_generation,
        };
        if !self
            .omp_service
            .app_has_native_renderer_for_route(client_id, &key)
        {
            return false;
        }
        let Some((_ws_idx, pane_id)) = self.app.parse_pane_id(&route.pane_id) else {
            return false;
        };
        let view_id = self
            .clients
            .get(&client_id)
            .and_then(|client| client.view_id.clone());
        let Some(canonical) = self.begin_client_navigation_scope(client_id) else {
            return false;
        };
        let activated = self
            .app
            .activate_link_once(client_id, pane_id, url, view_id.as_ref());
        self.finish_client_navigation_scope(client_id, canonical);
        activated
    }

    fn partition_native_omp_input(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> (Vec<crate::raw_input::RawInputEvent>, bool) {
        if !self.client_omp_surface_active(client_id) {
            return (events, false);
        }
        let Some(info) = self.independent_omp_pane_info(client_id) else {
            return (events, false);
        };
        let pending = self.pending_private_omp_pane_info(client_id).is_some();
        let terminal_mode = self.app.state.mode == crate::app::Mode::Terminal;
        let mut remaining = Vec::new();
        let mut consumed = false;
        for event in events {
            match event {
                crate::raw_input::RawInputEvent::Key(key)
                    if terminal_mode && !self.app.state.is_prefix_key(&key) =>
                {
                    consumed = true
                }
                crate::raw_input::RawInputEvent::Text(_)
                | crate::raw_input::RawInputEvent::Paste(_)
                    if terminal_mode =>
                {
                    consumed = true
                }
                crate::raw_input::RawInputEvent::Mouse(mouse)
                    if info.inner_rect.contains((mouse.column, mouse.row).into()) =>
                {
                    consumed = true
                }
                crate::raw_input::RawInputEvent::OuterFocusGained
                | crate::raw_input::RawInputEvent::OuterFocusLost
                    if pending =>
                {
                    consumed = true
                }
                event => remaining.push(event),
            }
        }
        (remaining, consumed)
    }

    fn private_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        let route = self
            .clients
            .get(&client_id)?
            .private_omp_guest
            .as_ref()?
            .route();
        let (ws_idx, pane_id) = self.app.parse_pane_id(&route.pane_id)?;
        (self.app.state.active == Some(ws_idx))
            .then(|| {
                self.app
                    .state
                    .view
                    .pane_infos
                    .iter()
                    .find(|info| info.id == pane_id)
                    .cloned()
            })
            .flatten()
    }

    fn replaced_omp_pane_info(&self, client_id: u64) -> Option<crate::layout::PaneInfo> {
        self.private_omp_pane_info(client_id)
            .or_else(|| self.independent_omp_pane_info(client_id))
    }
    fn private_omp_frame_link_at_cell(
        runtime: &crate::terminal::TerminalRuntime,
        row: u16,
        column: u16,
        width: u16,
        height: u16,
    ) -> Option<String> {
        let link = runtime.hyperlink_at_viewport_cell(column, row, width, height)?;
        crate::app::actions::safe_osc8_url(&link.uri).map(str::to_owned)
    }

    fn partition_private_omp_input(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> (Vec<crate::raw_input::RawInputEvent>, bool) {
        if !self.client_omp_surface_active(client_id) {
            return (events, false);
        }
        let Some(info) = self.private_omp_pane_info(client_id) else {
            return (events, false);
        };
        let keyboard_target = info.is_focused;
        let terminal_mode = self.app.state.mode == crate::app::Mode::Terminal;
        let view_id = self
            .clients
            .get(&client_id)
            .and_then(|client| client.view_id.clone());
        let Some(guest) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.private_omp_guest.as_ref())
        else {
            return (events, false);
        };
        let runtime = guest.runtime();
        let mut remaining = Vec::new();
        let mut consumed = false;
        for event in events {
            match event {
                crate::raw_input::RawInputEvent::Key(key)
                    if keyboard_target
                        && self.app.state.mode == crate::app::Mode::Terminal
                        && !self.app.state.is_prefix_key(&key) =>
                {
                    let bytes = runtime.encode_terminal_key(key);
                    if !bytes.is_empty() {
                        let _ = guest.input(Bytes::from(bytes));
                    }
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::Text(text)
                    if keyboard_target && self.app.state.mode == crate::app::Mode::Terminal =>
                {
                    let _ = guest.input(Bytes::copy_from_slice(text.as_str().as_bytes()));
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::Paste(text)
                    if keyboard_target && self.app.state.mode == crate::app::Mode::Terminal =>
                {
                    let _ = runtime.try_send_paste(text);
                    consumed = true;
                }
                crate::raw_input::RawInputEvent::OuterFocusGained => {
                    if keyboard_target {
                        runtime.try_send_focus_event(crate::ghostty::FocusEvent::Gained);
                    }
                    remaining.push(crate::raw_input::RawInputEvent::OuterFocusGained);
                }
                crate::raw_input::RawInputEvent::OuterFocusLost => {
                    if keyboard_target {
                        runtime.try_send_focus_event(crate::ghostty::FocusEvent::Lost);
                    }
                    remaining.push(crate::raw_input::RawInputEvent::OuterFocusLost);
                }
                crate::raw_input::RawInputEvent::Mouse(mouse)
                    if info.inner_rect.contains((mouse.column, mouse.row).into()) =>
                {
                    if self
                        .app
                        .suppress_pending_url_click_mouse(client_id, mouse.kind)
                    {
                        consumed = true;
                        continue;
                    }
                    let column = mouse.column.saturating_sub(info.inner_rect.x);
                    let row = mouse.row.saturating_sub(info.inner_rect.y);
                    if terminal_mode
                        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        && Self::private_omp_frame_link_at_cell(
                            runtime,
                            row,
                            column,
                            info.inner_rect.width,
                            info.inner_rect.height,
                        )
                        .is_some_and(|url| {
                            self.app
                                .activate_link_click(client_id, info.id, url, view_id.as_ref())
                        })
                    {
                        consumed = true;
                        continue;
                    }
                    let position = crate::input::mouse::Position::Cell { column, row };
                    let bytes = match mouse.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            runtime.encode_mouse_wheel(mouse.kind, position, mouse.modifiers)
                        }
                        MouseEventKind::Moved => {
                            runtime.encode_mouse_motion(mouse.kind, position, mouse.modifiers)
                        }
                        MouseEventKind::Down(_)
                        | MouseEventKind::Up(_)
                        | MouseEventKind::Drag(_) => {
                            runtime.encode_mouse_button(mouse.kind, position, mouse.modifiers)
                        }
                        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => None,
                    };
                    if let Some(bytes) = bytes {
                        let _ = guest.input(Bytes::from(bytes));
                    }
                    consumed = true;
                }
                event => remaining.push(event),
            }
        }
        (remaining, consumed)
    }
    fn route_private_omp_pixel_input(
        &mut self,
        client_id: u64,
        data: &[u8],
        host: crate::input::mouse::HostPixels,
        cell: (u16, u16),
    ) -> bool {
        let Some(info) = self.private_omp_pane_info(client_id) else {
            return false;
        };
        if !info.inner_rect.contains(cell.into()) {
            return false;
        }
        let view_id = self
            .clients
            .get(&client_id)
            .and_then(|client| client.view_id.clone());
        let Some(guest) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.private_omp_guest.as_ref())
        else {
            return false;
        };
        let runtime = guest.runtime();
        let Some(report) = crate::input::mouse::report_at_cell(data, cell.0, cell.1) else {
            return false;
        };
        let Some(mouse) = crate::raw_input::parse_raw_input_bytes_sync(&report)
            .into_iter()
            .find_map(|event| match event {
                crate::raw_input::RawInputEvent::Mouse(mouse) => Some(mouse),
                _ => None,
            })
        else {
            return false;
        };
        if self
            .app
            .suppress_pending_url_click_mouse(client_id, mouse.kind)
        {
            return true;
        }
        let column = cell.0.saturating_sub(info.inner_rect.x);
        let row = cell.1.saturating_sub(info.inner_rect.y);
        if self.app.state.mode == crate::app::Mode::Terminal
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && Self::private_omp_frame_link_at_cell(
                runtime,
                row,
                column,
                info.inner_rect.width,
                info.inner_rect.height,
            )
            .is_some_and(|url| {
                self.app
                    .activate_link_click(client_id, info.id, url, view_id.as_ref())
            })
        {
            return true;
        }
        let fallback = crate::input::mouse::Position::Cell { column, row };
        let position = runtime
            .pixel_size()
            .and_then(|(width_px, height_px)| {
                host.pane_position(info.inner_rect, width_px, height_px)
            })
            .unwrap_or(fallback);
        let bytes = match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                runtime.encode_mouse_wheel(mouse.kind, position, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                runtime.encode_mouse_motion(mouse.kind, position, mouse.modifiers)
            }
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                runtime.encode_mouse_button(mouse.kind, position, mouse.modifiers)
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => None,
        };
        if let Some(bytes) = bytes {
            let _ = guest.input(Bytes::from(bytes));
        }
        true
    }

    fn handle_client_input_events(
        &mut self,
        client_id: u64,
        events: Vec<crate::raw_input::RawInputEvent>,
    ) -> bool {
        if self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.private_surface.is_some())
        {
            return self.handle_private_surface_input_events(client_id, events);
        }

        let (events, identity_changed) = self.intercept_identity_input(client_id, events);
        if events.is_empty() && identity_changed {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.request_semantic_redraw_after_input();
            }
            return true;
        }
        let source_is_full_app = self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_full_app_client);
        let navigation_scope = source_is_full_app
            .then(|| self.begin_client_navigation_scope(client_id))
            .flatten();
        if navigation_scope.is_some() {
            self.compute_client_navigation_view(client_id);
        }
        let (events, native_consumed) = self.partition_native_omp_input(client_id, events);
        let (events, private_consumed) = self.partition_private_omp_input(client_id, events);
        if native_consumed || private_consumed {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.request_semantic_redraw_after_input();
            }
        }
        if events.is_empty() && (native_consumed || private_consumed || identity_changed) {
            if let Some(canonical) = navigation_scope {
                self.finish_client_navigation_scope(client_id, canonical);
            }
            return true;
        }

        let source_was_foreground = self.foreground_client_id == Some(client_id);
        let host_surface_redraw = crate::raw_input::events_require_host_surface_redraw(
            &events,
            self.app.state.redraw_on_focus_gained,
        );
        let render_neutral_mouse_motion =
            events_are_render_neutral_mouse_motion(&events, self.app.state.mode);
        let hover_generation = self.app.hover_generation;
        if let Some(client) = self.clients.get_mut(&client_id) {
            if host_surface_redraw {
                client.request_repaint();
                client.defer_full_render();
            } else if !render_neutral_mouse_motion {
                // Ensure semantic clients receive one post-input frame even if the
                // semantic buffer compares equal. Terminal-ANSI clients must keep their
                // server-side blit baseline; resetting it here forces a full redraw on
                // every keypress and makes remote sessions feel extremely slow.
                client.request_semantic_redraw_after_input();
            }
        }
        if source_is_full_app {
            self.update_client_outer_focus_from_events(client_id, &events);
            if events
                .iter()
                .any(|event| matches!(event, crate::raw_input::RawInputEvent::OuterFocusLost))
            {
                // Focus loss is not a teardown, so the pending URL click stays.
                self.app.release_input_source_headless(client_id);
            }
        }
        let events = events_for_app_routing(events, source_was_foreground, source_is_full_app);
        let interaction = events_include_interaction(&events);
        let foreground_changed = if interaction {
            self.promote_client_to_foreground(client_id)
        } else {
            false
        };
        if foreground_changed {
            self.resize_shared_runtime_to_effective_size_before_input();
        }
        let theme_changed = self.update_client_host_theme_from_events(client_id, &events);
        // Client-local theme reports were applied above; routing them again would update every
        // pane once per palette entry instead of once per captured batch.
        let view_id = self
            .clients
            .get(&client_id)
            .and_then(|client| client.view_id.clone());
        let mut events = events.into_iter().peekable();
        let mut terminal_forward_only = events.peek().is_some();
        while let Some(event) = events.next() {
            let forwarded_only = self.app.route_client_events_from_view(
                client_id,
                view_id.as_ref(),
                vec![event],
                false,
            );
            terminal_forward_only &= forwarded_only;
            if interaction && events.peek().is_some() && !forwarded_only {
                self.compute_client_navigation_view(client_id);
            }
        }
        let hover_changed = self.app.hover_generation != hover_generation;
        if hover_changed && !host_surface_redraw {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.request_semantic_redraw_after_input();
            }
        }
        let deferred_requests_changed =
            navigation_scope.is_some() && self.handle_deferred_requests_headless();
        let config_reloaded = self.app.take_config_reloaded_from_disk();
        if config_reloaded {
            self.reload_server_config(false);
        }

        let needs_render = if self.app.state.detach_requested {
            self.app.state.detach_requested = false;
            info!(client_id, "client detach requested via keybind");

            self.send_client_graphics_cleanup(client_id);
            self.send_to_client(
                client_id,
                ServerMessage::ServerShutdown {
                    reason: Some("detached".to_owned()),
                },
            );
            self.remove_client_and_resize_if_needed(client_id);

            true
        } else {
            deferred_requests_changed
                || foreground_changed
                || theme_changed
                || (self.independent_omp_renderers_enabled() && self.reconcile_private_omp_guests())
                || (interaction
                    && (hover_changed || (!render_neutral_mouse_motion && !terminal_forward_only)))
        };
        if let Some(canonical) = navigation_scope {
            self.finish_client_navigation_scope(client_id, canonical);
        }
        let renderer_changed = self.reconcile_omp_renderers();
        if !config_reloaded {
            self.sync_foreground_client_state();
        }
        needs_render || renderer_changed
    }

    fn activate_notification_target(
        &mut self,
        activation: protocol::NotificationActivation,
    ) -> bool {
        let pane_id = crate::layout::PaneId::from_raw(activation.pane_id);
        let Some(ws_idx) = self
            .app
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == activation.workspace_id)
        else {
            return false;
        };
        if self.app.state.workspaces[ws_idx]
            .find_tab_index_for_pane(pane_id)
            .is_none()
            || !self
                .clients
                .get(&activation.recipient_client_id)
                .is_some_and(|client| client.is_full_app_client() && client.writer.is_some())
        {
            return false;
        }

        if self.promote_client_to_foreground(activation.recipient_client_id) {
            self.resize_shared_runtime_to_effective_size();
        }
        self.app.focus_pane_internal_via_api(ws_idx, pane_id);
        self.app.state.mode = app::Mode::Terminal;
        self.app.toast_deadline = None;
        self.app.state.toast = None;
        if let Some(client) = self.clients.get_mut(&activation.recipient_client_id) {
            client.request_semantic_redraw_after_input();
        }
        true
    }
    fn handle_server_event(&mut self, ev: ServerEvent) -> bool {
        if self.handoff_in_progress {
            if let ServerEvent::NotificationActivated { respond_to, .. } = &ev {
                debug!("rejecting notification activation during live handoff");
                let _ = respond_to.send(false);
                return false;
            }
            if Self::ignore_client_event_during_handoff(&ev) {
                return false;
            }
        }
        let native_app = match &ev {
            ServerEvent::OmpPaneAttach {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                target_app_client_id,
                renderer_launch_id,
                ..
            } => {
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id: omp_session_id.clone(),
                    route_generation: *route_generation,
                };
                self.omp_service.app_client_for_renderer(
                    *client_id,
                    *target_app_client_id,
                    *renderer_launch_id,
                    &key,
                    &self.clients,
                )
            }
            _ => None,
        };
        let mut render = false;
        let omp_client_id = match &ev {
            ServerEvent::OmpPaneAttach { client_id, .. }
            | ServerEvent::OmpPaneDetach { client_id, .. }
            | ServerEvent::OmpControl { client_id, .. }
            | ServerEvent::OmpFrame { client_id, .. } => Some(*client_id),
            ServerEvent::OmpHostStarted { .. }
            | ServerEvent::OmpHostFrame { .. }
            | ServerEvent::OmpHostStopped { .. } => None,
            _ => return self.handle_non_omp_server_event(ev),
        };
        let client_is_omp_pane = omp_client_id.is_some_and(|client_id| {
            self.client_is_omp_pane(client_id)
                && (!matches!(&ev, ServerEvent::OmpPaneAttach { .. }) || native_app.is_some())
        });
        let messages = self
            .omp_service
            .handle_event(ev, client_is_omp_pane, &self.clients);
        render |= self.apply_omp_messages(messages);
        render |= self.reconcile_omp_renderers();
        render
    }

    fn handle_non_omp_server_event(&mut self, ev: ServerEvent) -> bool {
        match ev {
            ServerEvent::OmpPrivateCompanionResolved { result } => {
                let pending = self.private_omp_resolving.take();
                match (pending, result) {
                    (pending, Ok(executable)) => {
                        self.private_omp_executable = Some(executable);
                        let mut changed = false;
                        if let Some((client_id, route)) = pending {
                            if !self.private_omp_resolution_is_current(client_id, &route) {
                                changed |= self.clear_private_omp_pending_route(client_id, &route);
                            }
                        }
                        changed || self.reconcile_private_omp_completion()
                    }
                    (Some((client_id, route)), Err(error)) => {
                        warn!(client_id, %error, "failed to resolve private OMP renderer; keeping host PTY masked");
                        let current = self.private_omp_resolution_is_current(client_id, &route);
                        let mut changed = self.clear_private_omp_pending_route(client_id, &route);
                        if current {
                            self.mark_private_omp_failed_with_retry(client_id, route);
                            changed = true;
                        }
                        let reconciled = self.reconcile_private_omp_completion();
                        changed || reconciled
                    }
                    (None, Err(error)) => {
                        warn!(%error, "failed to resolve private OMP renderer");
                        false
                    }
                }
            }
            ServerEvent::OmpPrivateCompanionRetry {
                client_id,
                route,
                retry_id,
            } => {
                if self.private_omp_failed_routes.get(&client_id) != Some(&route)
                    || !self
                        .private_omp_retry_attempted_routes
                        .get(&client_id)
                        .is_some_and(|(attempted, state)| {
                            attempted == &route && *state == PrivateOmpRetryState::Pending(retry_id)
                        })
                {
                    return false;
                }
                if let Some((_, state)) =
                    self.private_omp_retry_attempted_routes.get_mut(&client_id)
                {
                    *state = PrivateOmpRetryState::Consumed;
                }
                self.private_omp_failed_routes.remove(&client_id);
                let mut changed = true;
                if self.private_omp_resolution_is_current(client_id, &route) {
                    changed |= self.set_private_omp_pending_route(client_id, &route);
                    changed |= self.reconcile_private_omp_completion();
                } else {
                    changed |= self.clear_private_omp_pending_route(client_id, &route);
                    self.private_omp_retry_attempted_routes.remove(&client_id);
                }
                changed
            }
            ServerEvent::ActivateOmpLink {
                client_id,
                launch_id,
                request_id,
                url,
            } => {
                let activated = self.activate_native_omp_link(client_id, launch_id, url);
                self.send_to_client(
                    client_id,
                    ServerMessage::OmpLinkActivationResult {
                        launch_id,
                        request_id,
                        activated,
                    },
                );
                activated
            }
            ServerEvent::OmpRendererReady {
                client_id,
                launch_id,
            } => {
                let Some(mut target) = self
                    .clients
                    .get(&client_id)
                    .filter(|client| client.is_full_app_client())
                    .and_then(|client| client.omp_renderer_target.clone())
                    .filter(|target| {
                        target.launch_id == launch_id && target.bound && !target.ready
                    })
                else {
                    return false;
                };
                let Some(route) = target.route.clone() else {
                    return false;
                };
                let key = OmpRouteKey {
                    pane_id: route.pane_id,
                    omp_session_id: route.omp_session_id,
                    route_generation: route.route_generation,
                };
                if !self
                    .omp_service
                    .app_has_native_renderer_for_route(client_id, &key)
                {
                    return false;
                }
                self.clear_private_omp_failure_state(client_id, &key);
                self.clear_private_omp_pending_route(client_id, &key);
                target.ready = true;
                target.surface_active =
                    target.bound && target.ready && self.client_omp_surface_active(client_id);
                self.update_omp_renderer_target(client_id, target);
                let retired_private = self
                    .clients
                    .get_mut(&client_id)
                    .is_some_and(|client| client.private_omp_guest.take().is_some());
                if retired_private {
                    self.omp_service.retire_private_renderer(client_id);
                }
                self.reconcile_omp_renderers();
                true
            }
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                keybindings,
                writer,
                render_encoding,
                direct_attach_requested,
                direct_graphics,
                omp_pane,
                display_name,
                frontend_profile_id,
                renderer_binding_token,
                renderer_capabilities,
            } => {
                if self.handoff_in_progress {
                    if let Ok(message) = Self::frame_server_message(&live_handoff_client_message())
                    {
                        let _ = writer.control.send(message);
                    }
                    return false;
                }
                let first_app_client =
                    !omp_pane && !direct_attach_requested && self.app_client_count() == 0;
                info!(
                    client_id,
                    cols,
                    rows,
                    cell_width_px,
                    cell_height_px,
                    ?render_encoding,
                    "client connected"
                );
                let last_activity = self.allocate_activity_stamp();
                let mut connection = ClientConnection::new_with_mode(
                    if omp_pane {
                        ClientConnectionMode::OmpPane
                    } else {
                        ClientConnectionMode::App
                    },
                    keybindings,
                    display_name,
                    frontend_profile_id,
                    renderer_binding_token,
                    (cols, rows),
                    crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    },
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    last_activity,
                    render_encoding,
                    direct_attach_requested,
                    Some(writer),
                );
                if !omp_pane && !direct_attach_requested {
                    if let Some(identity) = connection.identity.as_mut() {
                        if identity.committed.is_none() {
                            identity.open_editor();
                        }
                    }
                }
                connection.direct_graphics = direct_graphics;
                connection.pixel_mouse = direct_graphics;
                connection.omp_renderer_capabilities = renderer_capabilities;
                if !direct_attach_requested {
                    connection.navigation = Some(ClientNavigationState::capture(&self.app.state));
                }
                self.clients.insert(client_id, connection);
                if !omp_pane && !direct_attach_requested {
                    self.promote_client_to_foreground(client_id);
                } else {
                    self.sync_foreground_client_state();
                }
                if first_app_client {
                    self.app.mark_git_status_refresh_due(Instant::now());
                }
                self.resize_shared_runtime_to_effective_size();
                self.nudge_handoff_panes_on_first_client_attach();
                if !omp_pane && !direct_attach_requested {
                    self.reconcile_omp_renderers();
                }
                true
            }
            ServerEvent::NotificationActivated {
                activation,
                respond_to,
            } => {
                let activated = self.activate_notification_target(activation);
                let _ = respond_to.send(activated);
                let renderer_changed = activated && self.reconcile_omp_renderers();
                activated || renderer_changed
            }
            ServerEvent::IdentityPersistenceAck {
                client_id,
                request_id,
                display_name,
                success,
                error,
            } => {
                let result = if success {
                    Ok(())
                } else {
                    Err(error.unwrap_or_else(|| "identity persistence failed".to_owned()))
                };
                let applied = self
                    .clients
                    .get_mut(&client_id)
                    .and_then(|client| client.identity.as_mut())
                    .is_some_and(|identity| {
                        identity.apply_persistence_ack(request_id, &display_name, result)
                    });
                if applied {
                    self.reconcile_omp_renderers();
                }
                applied
            }
            ServerEvent::GraphicsTransmissionResult {
                client_id,
                transfer_id,
                image_id,
                success,
            } => self.complete_direct_graphics(client_id, transfer_id, image_id, success),
            ServerEvent::GraphicsTransmissionStarted {
                client_id,
                transfer_id,
                image_id,
            } => self.start_direct_graphics_response(client_id, transfer_id, image_id),
            ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            } => self.attach_terminal_client(client_id, terminal_id, takeover),
            ServerEvent::ClientObserveTerminal { client_id, target } => {
                self.observe_terminal_client(client_id, target)
            }
            ServerEvent::ClientControlTerminal {
                client_id,
                target,
                takeover,
            } => self.control_terminal_client(client_id, target, takeover),
            ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => self.handle_terminal_attach_scroll(
                client_id, source, direction, lines, column, row, modifiers,
            ),
            ServerEvent::ClientInputPixels {
                client_id,
                data,
                geometry,
            } => {
                let Some((x, y)) = crate::input::mouse::parse_report(&data) else {
                    return false;
                };
                let Some(cell_position) = geometry.cell(x, y) else {
                    return false;
                };
                let valid = self.clients.get(&client_id).is_some_and(|client| {
                    let cell = client.cell_size;
                    client.is_full_app_client()
                        && client.host_sgr_pixels_active == Some(true)
                        && client.terminal_size == (geometry.cols, geometry.rows)
                        && cell.is_known()
                        && cell.width_px == geometry.width_px / u32::from(geometry.cols)
                        && cell.height_px == geometry.height_px / u32::from(geometry.rows)
                });
                if !valid || self.handoff_in_progress {
                    return false;
                }
                let view_id = self
                    .clients
                    .get(&client_id)
                    .and_then(|client| client.view_id.clone());
                let Some(canonical) = self.begin_client_navigation_scope(client_id) else {
                    return false;
                };
                self.compute_client_navigation_view(client_id);
                let omp_surface_active = self.client_omp_surface_active(client_id);
                let host = crate::input::mouse::HostPixels { x, y, geometry };
                if omp_surface_active
                    && self
                        .independent_omp_pane_info(client_id)
                        .is_some_and(|info| info.inner_rect.contains(cell_position.into()))
                {
                    self.finish_client_navigation_scope(client_id, canonical);
                    return false;
                }
                if omp_surface_active
                    && self.route_private_omp_pixel_input(client_id, &data, host, cell_position)
                {
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.request_semantic_redraw_after_input();
                    }
                    self.finish_client_navigation_scope(client_id, canonical);
                    return true;
                }
                if !omp_surface_active {
                    self.finish_client_navigation_scope(client_id, canonical);
                    let Some(report) = crate::input::mouse::report_at_cell(
                        &data,
                        cell_position.0,
                        cell_position.1,
                    ) else {
                        return false;
                    };
                    let events = crate::raw_input::parse_raw_input_bytes_sync(&report);
                    return self.handle_client_input_events(client_id, events);
                }
                if !self.focused_pane_graphics_demand() {
                    self.finish_client_navigation_scope(client_id, canonical);
                    return false;
                }
                let foreground_changed = self.promote_client_to_foreground(client_id);
                if foreground_changed {
                    self.resize_shared_runtime_to_effective_size_before_input();
                    self.compute_client_navigation_view(client_id);
                }
                let routed =
                    self.app
                        .route_client_pixel_mouse(client_id, view_id.as_ref(), &data, geometry);
                let deferred_requests_changed = self.handle_deferred_requests_headless();
                self.finish_client_navigation_scope(client_id, canonical);
                routed || foreground_changed || deferred_requests_changed
            }
            ServerEvent::ClientInput { client_id, data } => {
                if self.handoff_in_progress {
                    debug!(
                        client_id,
                        len = data.len(),
                        "ignored client input during handoff"
                    );
                    return false;
                }
                debug!(client_id, len = data.len(), "client input received");
                if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    ..
                }) = self.clients.get(&client_id)
                {
                    if let Some(runtime) = self.runtime_for_terminal_id_string(terminal_id) {
                        if let Err(err) = apply_terminal_attach_input(runtime, data) {
                            warn!(client_id, terminal_id = %terminal_id, err = %err);
                        }
                    }
                    return true;
                }
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(
                        ClientConnectionMode::TerminalObserve { .. }
                            | ClientConnectionMode::OmpPane
                    )
                ) {
                    return false;
                }
                let events = if let Some(client) = self.clients.get_mut(&client_id) {
                    let mut events = client.raw_input.push(&data);
                    // The thin client only forwards a bare ESC after its local input timeout.
                    if data.as_slice() == b"\x1b" {
                        events.extend(client.raw_input.flush_timeout());
                    }
                    events
                } else {
                    Vec::new()
                };
                self.handle_client_input_events(client_id, events)
            }
            ServerEvent::ClientInputEvents { client_id, events } => {
                if self.handoff_in_progress {
                    debug!(
                        client_id,
                        len = events.len(),
                        "ignored client input events during handoff"
                    );
                    return false;
                }
                debug!(
                    client_id,
                    len = events.len(),
                    "client input events received"
                );
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(
                        ClientConnectionMode::TerminalObserve { .. }
                            | ClientConnectionMode::OmpPane
                    )
                ) {
                    return false;
                }
                let events = events
                    .iter()
                    .map(crate::protocol::ClientInputEvent::to_raw_input_event)
                    .collect();
                self.handle_client_input_events(client_id, events)
            }
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                self.send_to_client(
                    client_id,
                    ServerMessage::Notify {
                        kind: protocol::NotifyKind::Toast,
                        message: "Paste rejected".to_owned(),
                        body: Some(format!(
                            "Input message is {size} bytes; Herdr's limit is {max} bytes"
                        )),
                        activation: None,
                    },
                );
                false
            }
            ServerEvent::ClientClipboardImage {
                client_id,
                extension,
                data,
            } => {
                debug!(
                    client_id,
                    len = data.len(),
                    extension = %extension,
                    "client clipboard image received"
                );
                if matches!(
                    self.clients.get(&client_id).map(|client| &client.mode),
                    Some(
                        ClientConnectionMode::TerminalObserve { .. }
                            | ClientConnectionMode::OmpPane
                    )
                ) {
                    return false;
                }
                match self.write_client_clipboard_image(client_id, &extension, &data) {
                    Ok(path) => self.paste_client_clipboard_image_path(client_id, path),
                    Err(err) => {
                        warn!(client_id, err = %err, "failed to stage client clipboard image");
                        true
                    }
                }
            }
            ServerEvent::ClientResize {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } => {
                info!(
                    client_id,
                    cols, rows, cell_width_px, cell_height_px, "client resize"
                );
                let direct_terminal_id = if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalAttach { terminal_id },
                    terminal_size,
                    cell_size,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = (cols, rows);
                    let observed = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                    if observed.is_known() {
                        *cell_size = observed;
                    }
                    render_state.request_repaint();
                    Some((terminal_id.clone(), *cell_size))
                } else {
                    None
                };
                if let Some((terminal_id, cell_size)) = direct_terminal_id {
                    if let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) {
                        runtime.resize(rows, cols, cell_size.width_px, cell_size.height_px);
                    }
                    return true;
                }
                if let Some(ClientConnection {
                    mode: ClientConnectionMode::TerminalObserve { .. },
                    terminal_size,
                    cell_size,
                    render_state,
                    ..
                }) = self.clients.get_mut(&client_id)
                {
                    *terminal_size = (cols, rows);
                    let observed = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                    if observed.is_known() {
                        *cell_size = observed;
                    }
                    render_state.request_repaint();
                    return true;
                }
                if self
                    .clients
                    .get(&client_id)
                    .is_some_and(|client| client.private_surface.is_some())
                {
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.terminal_size = (cols, rows);
                        let observed = crate::kitty_graphics::HostCellSize {
                            width_px: cell_width_px,
                            height_px: cell_height_px,
                        };
                        if observed.is_known() {
                            client.cell_size = observed;
                        }
                        client.request_repaint();
                        client.defer_full_render();
                    }
                    return true;
                }
                if let Some(client) = self.clients.get_mut(&client_id) {
                    client.terminal_size = (cols, rows);
                    let observed = crate::kitty_graphics::HostCellSize {
                        width_px: cell_width_px,
                        height_px: cell_height_px,
                    };
                    if observed.is_known() {
                        client.cell_size = observed;
                    }
                }
                if let (Some(info), Some(guest)) = (
                    self.private_omp_pane_info(client_id),
                    self.clients
                        .get(&client_id)
                        .and_then(|client| client.private_omp_guest.as_ref()),
                ) {
                    let cell_size = self.clients[&client_id].cell_size;
                    guest.resize(
                        info.inner_rect.height.max(1),
                        info.inner_rect.width.max(1),
                        cell_size.width_px,
                        cell_size.height_px,
                    );
                    return true;
                }

                if self
                    .clients
                    .get(&client_id)
                    .is_some_and(|client| matches!(client.mode, ClientConnectionMode::OmpPane))
                {
                    return false;
                }
                let navigation_scope = self.begin_client_navigation_scope(client_id);
                self.promote_client_to_foreground(client_id);
                self.resize_shared_runtime_to_effective_size();
                if let Some(canonical) = navigation_scope {
                    self.finish_client_navigation_scope(client_id, canonical);
                }
                true
            }
            ServerEvent::ClientDetach { client_id } => {
                info!(client_id, "client detached");
                for (client_id, message) in self.omp_service.disconnect(client_id, &self.clients) {
                    self.send_to_client(client_id, message);
                }
                self.send_terminal_stream_detach_shutdown(client_id);
                self.remove_client_and_resize_if_needed(client_id);
                self.reconcile_omp_renderers();
                true
            }
            ServerEvent::ClientDisconnected { client_id } => {
                info!(client_id, "client disconnected");
                for (client_id, message) in self.omp_service.disconnect(client_id, &self.clients) {
                    self.send_to_client(client_id, message);
                }
                self.remove_client_and_resize_if_needed(client_id);
                self.reconcile_omp_renderers();
                true
            }
            ServerEvent::ClientWriterControlDrained { client_id } => {
                if !self.clients.contains_key(&client_id) {
                    return false;
                }
                self.reconcile_omp_renderers()
            }
            ServerEvent::ClientWriterDrained { client_id } => {
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                client.take_deferred_render() != DeferredRender::None
            }
            ServerEvent::OmpPaneAttach { .. }
            | ServerEvent::OmpPaneDetach { .. }
            | ServerEvent::OmpControl { .. }
            | ServerEvent::OmpFrame { .. }
            | ServerEvent::OmpHostStarted { .. }
            | ServerEvent::OmpHostFrame { .. }
            | ServerEvent::OmpHostStopped { .. } => {
                unreachable!("OMP events are delegated before this match")
            }
            ServerEvent::QuitSignal => {
                // The quit check at the top of the loop handles this.
                // No render needed — the next iteration will initiate shutdown.
                false
            }
        }
    }

    fn handle_server_event_with_render_impact(&mut self, ev: ServerEvent) -> RenderImpact {
        if self.handle_server_event(ev) {
            RenderImpact::Full
        } else {
            RenderImpact::None
        }
    }

    fn ignore_client_event_during_handoff(ev: &ServerEvent) -> bool {
        !matches!(
            ev,
            ServerEvent::ClientConnected { .. }
                | ServerEvent::ClientDisconnected { .. }
                | ServerEvent::ClientWriterDrained { .. }
                | ServerEvent::ClientWriterControlDrained { .. }
                | ServerEvent::OmpHostStarted { .. }
                | ServerEvent::OmpHostFrame { .. }
                | ServerEvent::OmpHostStopped { .. }
                | ServerEvent::QuitSignal
        )
    }

    fn agent_read_not_idle_error(
        &self,
        request: &api::schema::Request,
    ) -> Option<api::schema::ErrorBody> {
        use api::schema::{Method, ReadFormat, ReadSource};

        let Method::AgentRead(params) = &request.method else {
            return None;
        };
        let requested = params.lines?;
        if params.format != ReadFormat::Text
            || !matches!(
                params.source,
                ReadSource::Recent | ReadSource::RecentUnwrapped
            )
        {
            return None;
        }
        let target = self.app.resolve_agent_target(&params.target).ok()?;
        let terminal = self
            .app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.as_str() == target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state == crate::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
        let (screen, snapshot) = runtime.screen_text_snapshot()?;
        if screen != crate::ghostty::ActiveScreen::Alternate
            || snapshot.rows.len() >= requested.min(1000) as usize
        {
            return None;
        }
        let status = crate::detect::manifest::agent_state_label(terminal.state);
        Some(api::schema::ErrorBody {
            code: "agent_not_idle".into(),
            message: format!(
                "cannot read {requested} lines while {} is {status}: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible",
                params.target
            ),
        })
    }

    fn alt_screen_read_spec(&self, request: &api::schema::Request) -> Option<AltScreenReadSpec> {
        use api::schema::{Method, ReadFormat, ReadIntent, ReadSource};

        let (target, source, lines, format) = match &request.method {
            Method::AgentRead(params) => (
                self.app.resolve_agent_target(&params.target).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            Method::PaneRead(params) if params.intent == ReadIntent::Interactive => (
                self.app.resolve_terminal_target(&params.pane_id).ok()?,
                params.source,
                params.lines,
                params.format,
            ),
            _ => return None,
        };
        if format != ReadFormat::Text
            || !matches!(source, ReadSource::Recent | ReadSource::RecentUnwrapped)
        {
            return None;
        }
        let lines = lines.unwrap_or(80).min(1000) as usize;
        if lines == 0
            || self
                .terminal_attach_owners
                .contains_key(target.terminal_id.as_str())
            || self
                .pending_alt_screen_reads
                .iter()
                .any(|pending| pending.terminal_id.as_str() == target.terminal_id)
        {
            return None;
        }
        let terminal = self
            .app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.as_str() == target.terminal_id)?;
        if terminal.effective_known_agent().is_none()
            || terminal.state != crate::detect::AgentState::Idle
        {
            return None;
        }
        let runtime = self.app.terminal_runtimes.get(&terminal.id)?;
        if runtime.wheel_routing() != Some(crate::pane::WheelRouting::MouseReport) {
            return None;
        }
        let (screen, initial, content_seq) = runtime.screen_text_snapshot_with_seq()?;
        if screen != crate::ghostty::ActiveScreen::Alternate || initial.rows.len() >= lines {
            return None;
        }
        Some(AltScreenReadSpec {
            terminal_id: terminal.id.clone(),
            lines,
            unwrap: source == ReadSource::RecentUnwrapped,
            initial,
            content_seq,
        })
    }

    fn poll_pending_alt_screen_reads(&mut self, now: Instant) {
        let pending = std::mem::take(&mut self.pending_alt_screen_reads);
        for read in pending {
            let runtime = self.app.terminal_runtimes.get(&read.terminal_id);
            let remains_idle = self
                .app
                .state
                .terminals
                .get(&read.terminal_id)
                .is_some_and(|terminal| terminal.state == crate::detect::AgentState::Idle);
            let attached = self
                .terminal_attach_owners
                .contains_key(read.terminal_id.as_str());
            let outcome = if remains_idle && !attached {
                read.poll(runtime, now)
            } else {
                read.abort(runtime, now)
            };
            if let Some(read) = outcome {
                self.pending_alt_screen_reads.push(read);
            }
        }
    }

    fn alt_screen_read_conflict(&self, request: &api::schema::Request) -> AltScreenReadConflict {
        let (target, source, lines, format) = match &request.method {
            api::schema::Method::AgentRead(params) => (
                self.app.resolve_agent_target(&params.target).ok(),
                params.source,
                params.lines,
                params.format,
            ),
            api::schema::Method::PaneRead(params) => (
                self.app.resolve_terminal_target(&params.pane_id).ok(),
                params.source,
                params.lines,
                params.format,
            ),
            _ => return AltScreenReadConflict::None,
        };
        let Some(target) = target else {
            return AltScreenReadConflict::None;
        };
        let Some(pending) = self
            .pending_alt_screen_reads
            .iter()
            .find(|pending| pending.terminal_id.as_str() == target.terminal_id)
        else {
            return AltScreenReadConflict::None;
        };
        if format == api::schema::ReadFormat::Text {
            AltScreenReadConflict::Frozen(pending.frozen_snapshot(source, lines))
        } else {
            AltScreenReadConflict::Defer
        }
    }

    fn process_deferred_alt_screen_reads(&mut self) -> bool {
        let deferred = std::mem::take(&mut self.deferred_alt_screen_reads);
        let mut changed = false;
        for msg in deferred {
            match self.alt_screen_read_conflict(&msg.request) {
                AltScreenReadConflict::None => {
                    changed |= self.handle_api_request_with_shutdown_check(msg);
                }
                AltScreenReadConflict::Frozen(_) | AltScreenReadConflict::Defer => {
                    self.deferred_alt_screen_reads.push(msg);
                }
            }
        }
        changed
    }

    /// Drains API requests with shutdown awareness.
    ///
    /// During shutdown, remaining requests get a `server_unavailable` error.
    fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..crate::app::APP_EVENT_DRAIN_LIMIT {
            if self.should_quit.load(Ordering::Acquire) {
                break;
            }
            let Ok(msg) = self.app.api_rx.try_recv() else {
                break;
            };
            changed |= self.handle_api_request_with_shutdown_check(msg);
        }
        changed
    }

    fn reject_queued_api_requests_for_shutdown(&mut self) {
        for _ in 0..self.app.api_rx.len() {
            let Ok(msg) = self.app.api_rx.try_recv() else {
                break;
            };
            self.handle_api_request_with_shutdown_check(msg);
        }
    }

    fn drain_api_requests_with_render_impact(&mut self) -> RenderImpact {
        let mut impact = RenderImpact::None;
        for _ in 0..crate::app::APP_EVENT_DRAIN_LIMIT {
            if self.should_quit.load(Ordering::Acquire) {
                break;
            }
            let Ok(msg) = self.app.api_rx.try_recv() else {
                break;
            };
            impact.merge(self.handle_api_request_with_render_impact(msg));
        }
        impact
    }

    fn encode_omp_bridge_discovery_denied(id: String) -> String {
        serde_json::to_string(&api::schema::ErrorResponse {
            id,
            error: api::schema::ErrorBody {
                code: "omp_bridge_discovery_denied".into(),
                message: "OMP bridge discovery is unavailable for this caller".into(),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"omp_bridge_discovery_denied","message":"OMP bridge discovery is unavailable for this caller"}}"#
                .to_string()
        })
    }

    fn encode_omp_maintenance_response(
        id: String,
        result: Result<
            api::schema::ServerOmpMaintenanceStatus,
            crate::server::omp_maintenance::OmpMaintenanceError,
        >,
    ) -> String {
        let response = match result {
            Ok(maintenance) => serde_json::to_string(&api::schema::SuccessResponse {
                id,
                result: api::schema::ResponseResult::OmpMaintenance { maintenance },
            }),
            Err(error) => serde_json::to_string(&api::schema::ErrorResponse {
                id,
                error: api::schema::ErrorBody {
                    code: error.code().into(),
                    message: error.message(),
                },
            }),
        };
        response.unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"serialization_error","message":"failed to encode OMP maintenance response"}}"#
                .to_string()
        })
    }

    fn handle_pane_omp_bridge_api(
        &self,
        id: String,
        _params: &api::schema::PaneOmpBridgeParams,
        context: api::ApiRequestContext,
    ) -> String {
        let Some(peer_pid) = context.local_peer_pid else {
            return Self::encode_omp_bridge_discovery_denied(id);
        };
        let Some(target) = self.app.terminal_target_for_peer_pid(peer_pid) else {
            return Self::encode_omp_bridge_discovery_denied(id);
        };
        let Some(workspace) = self.app.state.workspaces.get(target.ws_idx) else {
            return Self::encode_omp_bridge_discovery_denied(id);
        };
        let Some(pane_number) = workspace.public_pane_number(target.pane_id) else {
            return Self::encode_omp_bridge_discovery_denied(id);
        };
        let pane_id = crate::workspace::public_pane_id_for_number(&workspace.id, pane_number);

        let fallback_id = id.clone();
        let bridge = self.omp_service.bridge();
        serde_json::to_string(&api::schema::SuccessResponse {
            id,
            result: api::schema::ResponseResult::PaneOmpBridge {
                token: bridge.token(&pane_id),
                address: bridge.address().to_string(),
                pane_id,
            },
        })
        .unwrap_or_else(|_| Self::encode_omp_bridge_discovery_denied(fallback_id))
    }

    /// Handles a single API request with shutdown awareness.
    ///
    /// Also forwards any toast/sound notifications that result from the API
    /// request to connected clients. API methods like `pane.report_agent`
    /// trigger internal events that may set toast state or would normally
    /// play sounds — in headless mode we forward these to clients instead.
    fn handle_api_request_with_shutdown_check(&mut self, msg: api::ApiRequestMessage) -> bool {
        self.handle_api_request_with_shutdown_check_inner(msg, false)
    }

    fn handle_api_request_with_render_impact(
        &mut self,
        msg: api::ApiRequestMessage,
    ) -> RenderImpact {
        if matches!(
            &msg.request.method,
            api::schema::Method::PaneGraphicsStreamSet(_)
                | api::schema::Method::PaneGraphicsStreamDirect(_)
        ) {
            return self.handle_pane_graphics_stream_frame(msg);
        }
        if self.handle_api_request_with_shutdown_check_inner(msg, false) {
            RenderImpact::Full
        } else {
            RenderImpact::None
        }
    }

    fn private_popup_terminal_area_for_owner(&mut self, owner_id: u64) -> Option<Rect> {
        let canonical_navigation = self.begin_client_navigation_scope(owner_id)?;
        self.compute_client_navigation_view(owner_id);
        let area = self.app.state.view.terminal_area;
        self.finish_client_navigation_scope(owner_id, canonical_navigation);
        Some(area)
    }

    fn client_private_plugin_popup_spec_for_owner(
        &mut self,
        owner_id: u64,
        params: &api::schema::PluginPaneOpenParams,
    ) -> Result<crate::app::ClientPrivatePluginPopupSpec, (&'static str, String)> {
        let canonical_navigation = ClientNavigationState::capture(&self.app.state);
        if let Some(navigation) = self
            .clients
            .get(&owner_id)
            .and_then(|client| client.navigation.as_ref())
        {
            navigation.apply_to(&mut self.app.state);
        }
        let spec = self.app.client_private_plugin_popup_spec(params);
        canonical_navigation.apply_to(&mut self.app.state);
        spec
    }

    #[cfg(test)]
    fn handle_client_private_plugin_pane_open(
        &mut self,
        id: String,
        params: api::schema::PluginPaneOpenParams,
    ) -> (String, bool) {
        let (response, changed) =
            self.handle_client_private_plugin_pane_open_with_response(id, params, None);
        (response.unwrap_or_default(), changed)
    }

    fn handle_client_private_plugin_pane_open_with_response(
        &mut self,
        id: String,
        params: api::schema::PluginPaneOpenParams,
        respond_to: Option<std::sync::mpsc::Sender<String>>,
    ) -> (Option<String>, bool) {
        let error = |code: &str, message: String| {
            Self::private_surface_error_response(id.clone(), code, message)
        };
        let Some(requested_view_id) = params.view_id.as_ref() else {
            return (
                Some(error(
                    "view_id_required",
                    "client-private plugin panes require view_id".to_string(),
                )),
                false,
            );
        };
        let owner_id = self.clients.iter().find_map(|(&client_id, client)| {
            (client.is_full_app_client()
                && client.writer.is_some()
                && client.view_id.as_ref() == Some(requested_view_id))
            .then_some(client_id)
        });
        let Some(owner_id) = owner_id else {
            return (
                Some(error(
                    "view_not_found",
                    format!("view {requested_view_id} is not connected"),
                )),
                false,
            );
        };

        let spec = match self.client_private_plugin_popup_spec_for_owner(owner_id, &params) {
            Ok(spec) => spec,
            Err((code, message)) => return (Some(error(code, message)), false),
        };
        let wait_for_remote_ready = !spec.execution_target.is_local();
        let deferred = wait_for_remote_ready && respond_to.is_some();
        let Some(area) = self.private_popup_terminal_area_for_owner(owner_id) else {
            return (
                Some(error(
                    "view_not_found",
                    "view disconnected during request".to_string(),
                )),
                false,
            );
        };
        let Some((cell_size, theme, appearance)) = self.clients.get(&owner_id).map(|client| {
            (
                client.cell_size,
                client.host_terminal_theme,
                client.host_terminal_appearance,
            )
        }) else {
            return (
                Some(error(
                    "view_not_found",
                    "view disconnected during request".to_string(),
                )),
                false,
            );
        };

        match crate::server::private_surface::PrivateSurface::spawn(
            spec, area, cell_size, theme, appearance, &self.app,
        ) {
            Ok(surface) => {
                let pending_response = if deferred {
                    respond_to.map(|respond_to| PendingPrivateSurfaceResponse {
                        id: id.clone(),
                        respond_to,
                    })
                } else {
                    None
                };
                if !self.install_private_surface_with_response(
                    owner_id,
                    surface,
                    wait_for_remote_ready,
                    pending_response,
                ) {
                    return if deferred {
                        (None, false)
                    } else {
                        (
                            Some(error(
                                "view_not_found",
                                "view disconnected during request".to_string(),
                            )),
                            false,
                        )
                    };
                }
                if deferred {
                    (None, true)
                } else {
                    (Some(Self::private_surface_ok_response(id)), true)
                }
            }
            Err(err) => (
                Some(error("plugin_pane_open_failed", err.to_string())),
                false,
            ),
        }
    }

    fn handle_api_request_with_shutdown_check_inner(
        &mut self,
        msg: api::ApiRequestMessage,
        skip_default_workspace_for_request: bool,
    ) -> bool {
        if self.shutting_down {
            // During shutdown, respond with server_unavailable.
            let response = serde_json::to_string(&api::schema::ErrorResponse {
                id: msg.request.id,
                error: api::schema::ErrorBody {
                    code: "server_unavailable".into(),
                    message: "server is shutting down".into(),
                },
            })
            .unwrap_or_else(|_| {
                r#"{"id":"","error":{"code":"server_unavailable","message":"server is shutting down"}}"#
                    .to_string()
            });
            let _ = msg.respond_to.send(response);
            return false;
        }
        let mut changed = self.drain_all_internal_events_with_forwarding();
        let refreshed_shared_plugin_pane =
            if let api::schema::Method::PluginPaneOpen(params) = &msg.request.method {
                if let Err(err) = self.app.refresh_installed_plugins() {
                    let response = serde_json::to_string(&api::schema::ErrorResponse {
                        id: msg.request.id.clone(),
                        error: api::schema::ErrorBody {
                            code: "plugin_registry_load_failed".to_string(),
                            message: err.to_string(),
                        },
                    })
                    .unwrap_or_else(|_| "{}".to_string());
                    let _ = msg.respond_to.send(response);
                    return changed;
                }
                if self.app.plugin_pane_effective_scope(params)
                    == api::schema::PluginPaneScope::ClientPrivate
                {
                    let respond_to = msg.respond_to.clone();
                    let (response, plugin_changed) = self
                        .handle_client_private_plugin_pane_open_with_response(
                            msg.request.id.clone(),
                            params.clone(),
                            Some(respond_to),
                        );
                    if let Some(response) = response {
                        let _ = msg.respond_to.send(response);
                    }
                    return changed | plugin_changed;
                }
                Some((msg.request.id.clone(), params.clone()))
            } else {
                None
            };

        if let api::schema::Method::PaneOmpBridge(params) = &msg.request.method {
            let response =
                self.handle_pane_omp_bridge_api(msg.request.id.clone(), params, msg.context);
            let _ = msg.respond_to.send(response);
            return changed;
        }

        if let api::schema::Method::ServerOmpMaintenanceAcquire(params) = &msg.request.method {
            let result = self.omp_service.acquire_maintenance(&params.operation_id);
            let changed = result.is_ok() && self.enforce_omp_maintenance();
            let result = result.and_then(|_| self.omp_service.maintenance_status());
            let response = Self::encode_omp_maintenance_response(msg.request.id.clone(), result);
            let _ = msg.respond_to.send(response);
            return changed;
        }

        if matches!(
            &msg.request.method,
            api::schema::Method::ServerOmpMaintenanceInspect(_)
        ) {
            let response = Self::encode_omp_maintenance_response(
                msg.request.id.clone(),
                self.omp_service.inspect_maintenance(),
            );
            let _ = msg.respond_to.send(response);
            return false;
        }

        if matches!(
            &msg.request.method,
            api::schema::Method::ServerOmpMaintenanceStatus(_)
        ) {
            let changed = self.enforce_omp_maintenance();
            let response = Self::encode_omp_maintenance_response(
                msg.request.id.clone(),
                self.omp_service.maintenance_status(),
            );
            let _ = msg.respond_to.send(response);
            return changed;
        }

        if let api::schema::Method::ServerOmpMaintenancePermit(params) = &msg.request.method {
            let response = Self::encode_omp_maintenance_response(
                msg.request.id.clone(),
                self.omp_service.grant_maintenance_permit(
                    &params.operation_id,
                    &params.session,
                    &params.pane_id,
                ),
            );
            let _ = msg.respond_to.send(response);
            return false;
        }

        if let api::schema::Method::ServerOmpMaintenanceRelease(params) = &msg.request.method {
            let changed = self.enforce_omp_maintenance();
            let response = Self::encode_omp_maintenance_response(
                msg.request.id.clone(),
                self.omp_service.release_maintenance(&params.operation_id),
            );
            let _ = msg.respond_to.send(response);
            return changed;
        }

        let frozen_alt_screen_read = match self.alt_screen_read_conflict(&msg.request) {
            AltScreenReadConflict::None => None,
            AltScreenReadConflict::Frozen(snapshot) => Some(snapshot),
            AltScreenReadConflict::Defer => {
                self.deferred_alt_screen_reads.push(msg);
                return changed;
            }
        };

        let metadata_expired = self.app.expire_due_metadata(Instant::now());
        let stream_open = match &msg.request.method {
            api::schema::Method::PaneGraphicsStreamOpen(params) => Some(params.clone()),
            _ => None,
        };
        let stream_active = msg.stream_active.clone();

        if let api::schema::Method::ServerLiveHandoff(params) = &msg.request.method {
            let handoff_result = self.perform_live_handoff(params.clone());
            let handoff_succeeded = handoff_result.is_ok();
            let response = match handoff_result {
                Ok(()) => serde_json::to_string(&api::schema::SuccessResponse {
                    id: msg.request.id,
                    result: api::schema::ResponseResult::Ok {},
                }),
                Err(err) => serde_json::to_string(&api::schema::ErrorResponse {
                    id: msg.request.id,
                    error: api::schema::ErrorBody {
                        code: "handoff_failed".into(),
                        message: err.to_string(),
                    },
                }),
            }
            .unwrap_or_else(|_| "{}".to_string());
            let _ = msg.respond_to.send(response);
            if handoff_succeeded {
                wait_for_live_handoff_response_write(msg.response_write_complete);
                self.finish_live_handoff_shutdown();
            }
            return true;
        }

        if let api::schema::Method::NotificationShow(params) = &msg.request.method {
            let response =
                self.handle_notification_show_api(msg.request.id.clone(), params.clone());
            let _ = msg.respond_to.send(response);
            return true;
        }

        match &msg.request.method {
            api::schema::Method::ClientWindowTitleSet(params) => {
                let response = self.handle_client_window_title_api(
                    msg.request.id.clone(),
                    Some(params.title.clone()),
                );
                let _ = msg.respond_to.send(response);
                return true;
            }
            api::schema::Method::ClientWindowTitleClear(_) => {
                let response = self.handle_client_window_title_api(msg.request.id.clone(), None);
                let _ = msg.respond_to.send(response);
                return true;
            }
            _ => {}
        }

        let pane_graphics_revision_before = matches!(
            &msg.request.method,
            api::schema::Method::PaneGraphicsSet(_)
                | api::schema::Method::PaneGraphicsClear(_)
                | api::schema::Method::PaneGraphicsStreamOpen(_)
                | api::schema::Method::PaneGraphicsStreamClose(_)
        )
        .then_some(self.app.pane_graphics.revision());
        changed |= metadata_expired
            | (pane_graphics_revision_before.is_none() && api::request_changes_ui(&msg.request));
        let skip_default_workspace = skip_default_workspace_for_request
            || matches!(
                &msg.request.method,
                api::schema::Method::ServerStop(_) | api::schema::Method::ServerLiveHandoff(_)
            );

        // Capture toast and effective pane states before the API call so we can
        // forward resulting client-local notifications. API requests like
        // pane.report_agent trigger handle_internal_event internally, which
        // bypasses drain_internal_events_with_forwarding. Headless mode disables
        // local sound playback, so sound notifications need to be forwarded here.
        let toast_before = self.app.state.toast.clone();
        let pane_states_before: Vec<(
            usize,
            crate::layout::PaneId,
            crate::detect::AgentState,
            Option<String>,
        )> = {
            let terminals = &self.app.state.terminals;
            self.app
                .state
                .workspaces
                .iter()
                .enumerate()
                .flat_map(|(ws_idx, ws)| {
                    ws.tabs.iter().flat_map(move |tab| {
                        tab.panes.iter().filter_map(move |(&pane_id, pane)| {
                            terminals.get(&pane.attached_terminal_id).map(|terminal| {
                                (
                                    ws_idx,
                                    pane_id,
                                    terminal.state,
                                    terminal.effective_agent_label().map(str::to_string),
                                )
                            })
                        })
                    })
                })
                .collect()
        };

        self.sync_foreground_client_state();
        if let Some(error) = self.agent_read_not_idle_error(&msg.request) {
            let response = serde_json::to_string(&api::schema::ErrorResponse {
                id: msg.request.id.clone(),
                error,
            })
            .unwrap_or_else(|_| "{}".to_owned());
            let _ = msg.respond_to.send(response);
            return changed;
        }
        let alt_screen_read_spec = self.alt_screen_read_spec(&msg.request);
        if matches!(
            &msg.request.method,
            api::schema::Method::WorktreeCreate(_) | api::schema::Method::WorktreeRemove(_)
        ) {
            let deferred_changed = self
                .app
                .handle_deferred_worktree_api_request(msg.request, msg.respond_to);
            return changed | deferred_changed;
        }
        let mut response = if matches!(
            &msg.request.method,
            api::schema::Method::ServerReloadConfig(_)
        ) {
            let report = self.reload_server_config(true);
            serde_json::to_string(&api::schema::SuccessResponse {
                id: msg.request.id.clone(),
                result: api::schema::ResponseResult::ConfigReload {
                    status: report.status,
                    diagnostics: report.diagnostics,
                },
            })
            .unwrap_or_else(|err| {
                serde_json::to_string(&api::schema::ErrorResponse {
                    id: String::new(),
                    error: api::schema::ErrorBody {
                        code: "serialization_error".into(),
                        message: err.to_string(),
                    },
                })
                .unwrap_or_else(|_| "{}".to_string())
            })
        } else if let Some((id, params)) = refreshed_shared_plugin_pane {
            self.app.sync_pending_terminal_titles();
            self.app
                .handle_plugin_pane_open_with_refreshed_registry(id, params)
        } else {
            self.app
                .handle_api_request_after_internal_events_drained_with_context(
                    msg.request,
                    msg.context,
                )
        };
        changed |= self.reconcile_omp_renderers();
        if let Some(snapshot) = frozen_alt_screen_read {
            if let Ok(mut success) = serde_json::from_str::<api::schema::SuccessResponse>(&response)
            {
                if let api::schema::ResponseResult::PaneRead { read } = &mut success.result {
                    read.text = snapshot.text;
                    read.truncated = snapshot.truncated;
                    if let Ok(serialized) = serde_json::to_string(&success) {
                        response = serialized;
                    }
                }
            }
        }
        if let (Some(params), Some(active)) = (stream_open.as_ref(), stream_active) {
            self.app
                .attach_pane_graphics_stream_active(params, active, &response);
        }
        if let Some(spec) = alt_screen_read_spec {
            if let Ok(success) = serde_json::from_str::<api::schema::SuccessResponse>(&response) {
                if let api::schema::ResponseResult::PaneRead { read } = success.result {
                    let pending = crate::server::alt_screen_read::PendingAltScreenRead::start(
                        spec.terminal_id,
                        success.id,
                        msg.respond_to,
                        response,
                        read,
                        spec.lines,
                        spec.unwrap,
                        spec.initial,
                        spec.content_seq,
                        Instant::now(),
                    );
                    self.pending_alt_screen_reads.push(pending);
                    return changed;
                }
            }
        }
        let _ = msg.respond_to.send(response);

        if let Some(revision_before) = pane_graphics_revision_before {
            changed |= revision_before != self.app.pane_graphics.revision();
        }
        // Forward new toast state only when a client-local delivery mode is selected.
        // Herdr delivery renders the toast in-frame and must not ask clients to
        // show a terminal or system notification.
        let toast_after = self.app.state.toast.clone();
        let toast_delivery = self
            .app
            .state
            .toast_config
            .delivery
            .effective(self.app.state.outer_terminal_focus);
        let forwarded_toast_from_state = if should_forward_toast_to_clients(toast_delivery)
            && toast_after.is_some()
            && toast_after != toast_before
        {
            if let Some(toast) = &toast_after {
                debug!(title = %toast.title, body = %toast.context, "forwarding toast notification from API request");
                self.send_notify_to_foreground_client(
                    toast_notify_kind(toast_delivery)
                        .expect("toast forwarding requires a client notification kind"),
                    &toast.title,
                    non_empty_body(&toast.context),
                    toast.target.as_ref(),
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        // Forward notifications for effective pane state changes that occurred
        // during the API request. Hook authority is already folded into
        // pane.state, so raw hook transitions must not produce separate sounds.
        for (ws_idx, pane_id, prev_state, prev_agent_label) in &pane_states_before {
            let pane_after = self
                .app
                .state
                .workspaces
                .get(*ws_idx)
                .and_then(|ws| ws.tabs.iter().find_map(|tab| tab.panes.get(pane_id)));

            let Some(pane_after) = pane_after else {
                continue;
            };

            let Some(terminal_after) = self
                .app
                .state
                .terminals
                .get(&pane_after.attached_terminal_id)
            else {
                continue;
            };

            let new_state = terminal_after.state;
            if new_state == *prev_state {
                continue;
            }

            let is_active_tab = self.app.state.pane_is_in_active_tab(*ws_idx, *pane_id);
            let suppress_active_tab_notifications =
                self.active_tab_suppresses_notifications(is_active_tab);

            let agent = terminal_after.effective_known_agent();
            let agent_label = terminal_after.effective_agent_label().map(str::to_string);

            debug!(
                ws_idx,
                pane_id = pane_id.raw(),
                prev_state = ?prev_state,
                new_state = ?new_state,
                agent = ?agent,
                "pane effective state changed during API request, checking notification"
            );

            if !forwarded_toast_from_state
                && self.app.state.toast_config.delay_seconds == 0
                && should_forward_toast_to_clients(toast_delivery)
            {
                if let Some(kind) =
                    crate::app::actions::notification_toast_for_state_change_with_agent_labels(
                        suppress_active_tab_notifications,
                        *prev_state,
                        new_state,
                        prev_agent_label.as_deref(),
                        agent_label.as_deref(),
                    )
                {
                    if let Some(agent_label) = self
                        .app
                        .state
                        .terminals
                        .get(&pane_after.attached_terminal_id)
                        .and_then(|terminal| terminal.effective_agent_label())
                    {
                        let event_text = match kind {
                            crate::app::state::ToastKind::NeedsAttention => "needs attention",
                            crate::app::state::ToastKind::Finished => "finished",
                            crate::app::state::ToastKind::UpdateInstalled => "updated",
                        };
                        let workspace_label = self.app.state.workspaces[*ws_idx].display_name_from(
                            &self.app.state.terminals,
                            &self.app.terminal_runtimes,
                        );
                        let context = crate::app::actions::notification_context(
                            &self.app.state.workspaces[*ws_idx],
                            &workspace_label,
                            *ws_idx,
                            *pane_id,
                        );
                        let target = crate::app::state::ToastTarget {
                            workspace_id: self.app.state.workspaces[*ws_idx].id.clone(),
                            pane_id: *pane_id,
                        };
                        self.send_notify_to_foreground_client(
                            toast_notify_kind(toast_delivery)
                                .expect("toast forwarding requires a client notification kind"),
                            format!("{agent_label} {event_text}"),
                            non_empty_body(&context),
                            Some(&target),
                        );
                    }
                }
            }

            // Forward sound notification when server-side sound policy allows it.
            // Clients still decide locally whether they can execute the side effect.
            if self.app.state.toast_config.delay_seconds == 0 && self.app.state.sound.allows(agent)
            {
                if let Some(sound) =
                    crate::app::actions::notification_sound_for_state_change_with_agent_labels(
                        suppress_active_tab_notifications,
                        *prev_state,
                        new_state,
                        prev_agent_label.as_deref(),
                        agent_label.as_deref(),
                    )
                {
                    debug!(sound = ?sound, "forwarding sound notification from API request");
                    self.send_notify_to_foreground_client(
                        protocol::NotifyKind::Sound,
                        sound_notify_message(sound),
                        None,
                        None,
                    );
                }
            }
        }

        if !skip_default_workspace && latest_app_client(&self.clients).is_some() {
            changed |= self.app.ensure_default_workspace();
        }

        changed
    }

    fn focused_pane_graphics_demand(&self) -> bool {
        self.app
            .state
            .active
            .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
            .and_then(crate::workspace::Workspace::focused_pane_id)
            .is_some_and(|pane_id| self.app.pane_graphics.active_for_pane(pane_id))
    }

    fn stream_host_mouse_capture_mode(&mut self) {
        let enabled = self
            .app
            .state
            .should_capture_host_mouse_from(&self.app.terminal_runtimes);
        let pixel_mouse_requested = self.clients.values().any(|client| {
            client.is_full_app_client() && client.private_surface.is_none() && client.pixel_mouse
        });
        let sgr_pixels = pixel_mouse_requested
            && self.focused_pane_graphics_demand()
            && self
                .app
                .state
                .active
                .and_then(|ws_idx| {
                    self.app
                        .state
                        .workspaces
                        .get(ws_idx)
                        .and_then(crate::workspace::Workspace::focused_pane_id)
                        .and_then(|pane_id| {
                            self.app.state.runtime_for_pane_in_workspace(
                                &self.app.terminal_runtimes,
                                ws_idx,
                                pane_id,
                            )
                        })
                })
                .is_some_and(crate::terminal::TerminalRuntime::sgr_pixel_mouse_enabled);
        let mut broken_clients: Vec<u64> = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if !client.is_full_app_client() {
                continue;
            }
            let client_enabled = client.private_surface.is_some() || enabled;
            let client_sgr_pixels =
                client.private_surface.is_none() && sgr_pixels && client.pixel_mouse;
            if client.host_mouse_capture_active == Some(client_enabled)
                && client.host_sgr_pixels_active == Some(client_sgr_pixels)
            {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(&ServerMessage::MouseCapture {
                enabled: client_enabled,
                sgr_pixels: client_sgr_pixels,
            }) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(err = %err, "failed to serialize mouse capture mode for client");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                debug!(
                    client_id,
                    "client writer channel closed during mouse capture update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_mouse_capture_active = Some(client_enabled);
            client.host_sgr_pixels_active = Some(client_sgr_pixels);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn stream_host_keyboard_enhancement_flags(&mut self) {
        let report_all_keys = self.app.host_keyboard_report_all_requested();

        let mut broken_clients = Vec::new();
        for (&client_id, client) in &mut self.clients {
            if !client.is_full_app_client() {
                continue;
            }
            let client_report_all = client
                .private_surface
                .as_ref()
                .map_or(report_all_keys, |surface| {
                    surface.keyboard_report_all_requested()
                });
            if client.host_keyboard_report_all_active == Some(client_report_all) {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(
                &ServerMessage::KittyKeyboardReportAll {
                    enabled: client_report_all,
                },
            ) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(client_id, err = %err, "failed to serialize keyboard enhancement flags for client");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                debug!(
                    client_id,
                    "client writer channel closed during keyboard enhancement update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_keyboard_report_all_active = Some(client_report_all);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    fn has_pending_presentation_work(
        &self,
        needs_full_render: bool,
        needs_graphics_render: bool,
    ) -> bool {
        needs_full_render || needs_graphics_render || self.app.render_dirty.has_immediate_work()
    }

    fn sync_immediate_pty_sources(&self) {
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        let mut pane_ids = if has_app_target {
            self.app.state.app_surface_pane_ids()
        } else {
            HashSet::new()
        };
        pane_ids.extend(self.clients.values().filter_map(|client| {
            (client.writer.is_some() && client.is_full_app_client())
                .then(|| {
                    client
                        .private_surface
                        .as_ref()
                        .map(|surface| surface.pane_id())
                })
                .flatten()
        }));
        if !direct_terminal_targets.is_empty() {
            for workspace in &self.app.state.workspaces {
                for tab in &workspace.tabs {
                    pane_ids.extend(tab.panes.iter().filter_map(|(&pane_id, pane)| {
                        direct_terminal_targets
                            .contains(pane.attached_terminal_id.as_str())
                            .then_some(pane_id)
                    }));
                }
            }
            if let Some(popup) = &self.app.state.popup_pane {
                if direct_terminal_targets.contains(popup.terminal_id.as_str()) {
                    pane_ids.insert(popup.pane_id);
                }
            }
        }
        self.app.render_dirty.set_immediate_pty_sources(pane_ids);
    }

    fn pty_render_targets(&self) -> (bool, HashSet<&str>) {
        let mut has_app_target = false;
        let mut direct_terminal_targets = HashSet::new();
        for client in self
            .clients
            .values()
            .filter(|client| client.writer.is_some())
        {
            match &client.mode {
                ClientConnectionMode::App if client.is_full_app_client() => {
                    has_app_target = true;
                }
                ClientConnectionMode::TerminalAttach { terminal_id }
                | ClientConnectionMode::TerminalObserve { terminal_id } => {
                    direct_terminal_targets.insert(terminal_id.as_str());
                }
                ClientConnectionMode::App | ClientConnectionMode::OmpPane => {}
            }
        }
        (has_app_target, direct_terminal_targets)
    }

    fn pty_source_visible_to_render_targets(
        &self,
        pane_id: crate::layout::PaneId,
        has_app_target: bool,
        direct_terminal_targets: &HashSet<&str>,
    ) -> bool {
        let terminal_id = self.terminal_id_for_pane(pane_id);
        (has_app_target && (terminal_id.is_none() || self.app_surface_contains_pane(pane_id)))
            || terminal_id.is_none_or(|source| direct_terminal_targets.contains(source.as_str()))
    }

    fn pty_sources_visible_to_any_render_target(
        &self,
        sources: &HashSet<crate::layout::PaneId>,
    ) -> bool {
        if sources.iter().any(|pane_id| {
            self.clients.values().any(|client| {
                client.writer.is_some()
                    && client
                        .private_surface
                        .as_ref()
                        .is_some_and(|surface| surface.pane_id() == *pane_id)
            })
        }) {
            return true;
        }
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        if !has_app_target && direct_terminal_targets.is_empty() {
            return false;
        }

        sources.iter().copied().any(|pane_id| {
            self.pty_source_visible_to_render_targets(
                pane_id,
                has_app_target,
                &direct_terminal_targets,
            )
        })
    }

    fn terminal_id_for_pane(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::terminal::TerminalId> {
        if let Some(popup) = self
            .app
            .state
            .popup_pane
            .as_ref()
            .filter(|popup| popup.pane_id == pane_id)
        {
            return Some(&popup.terminal_id);
        }
        self.app
            .find_pane(pane_id)
            .map(|(_, pane)| &pane.attached_terminal_id)
    }

    fn app_surface_contains_pane(&self, pane_id: crate::layout::PaneId) -> bool {
        if self
            .app
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.pane_id == pane_id)
        {
            return true;
        }
        let Some(workspace) = self
            .app
            .state
            .active
            .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
        else {
            return false;
        };
        let Some(tab) = workspace.active_tab() else {
            return false;
        };
        if !tab.panes.contains_key(&pane_id) {
            return false;
        }
        !tab.zoomed || tab.layout.focused() == pane_id
    }

    fn render_retained_pty_update_and_stream(&mut self) -> bool {
        crate::render_prof::event("retained.attempt");
        let retained_started = crate::render_prof::timer();
        macro_rules! retained_fallback {
            ($reason:literal) => {{
                crate::render_prof::event(concat!("retained_fallback.", $reason));
                crate::render_prof::duration_since("retained.total", retained_started);
                return false;
            }};
        }
        macro_rules! retained_success {
            ($reason:literal) => {{
                crate::render_prof::event("retained.success");
                crate::render_prof::event(concat!("retained_success.", $reason));
                crate::render_prof::duration_since("retained.total", retained_started);
                return true;
            }};
        }

        if !self.retained_pty_update_allowed_by_app_state() {
            retained_fallback!("unsafe_app_state");
        }

        let render_targets = render_targets(&self.clients, self.foreground_client_id);
        let [(client_id, (cols, rows), cell_size, _is_foreground, mode)] =
            render_targets.as_slice()
        else {
            retained_fallback!("multiple_or_no_target");
        };
        if !matches!(mode, ClientConnectionMode::App) {
            retained_fallback!("not_app_client");
        }
        if self.replaced_omp_pane_info(*client_id).is_some() {
            retained_fallback!("independent_omp_pane");
        }
        let Some(client) = self.clients.get(client_id) else {
            retained_fallback!("client_missing");
        };
        if client.private_surface.is_some() {
            retained_fallback!("private_surface");
        }
        if client.deferred_render() != DeferredRender::None {
            retained_fallback!("render_pending");
        }
        if self.app.state.kitty_graphics_enabled && !client.graphics_cache.is_empty() {
            retained_fallback!("graphics_cache_active");
        }
        if client.graphics_surface_reset_pending {
            retained_fallback!("graphics_surface_reset");
        }
        if self.app.state.kitty_graphics_enabled
            && cell_size.is_known()
            && crate::kitty_graphics::has_visible_pane_graphics(
                &self.app.state,
                &self.app.pane_graphics,
                &self.app.terminal_runtimes,
                self.app.state.view.tab_surface(),
                *cell_size,
            )
        {
            retained_fallback!("visible_kitty_graphics");
        }
        let Some(mut frame) = client.render_state.last_frame().cloned() else {
            retained_fallback!("no_last_frame");
        };
        if frame.width != *cols || frame.height != *rows {
            retained_fallback!("frame_size_mismatch");
        }
        frame.graphics.clear();

        let Some(ws_idx) = self.app.state.active else {
            retained_fallback!("no_active_workspace");
        };
        let pane_infos = self.app.state.view.pane_infos.clone();
        if pane_infos.is_empty() {
            retained_fallback!("no_pane_info");
        }

        let mut touched = false;
        for info in pane_infos {
            if !rect_fits_frame(info.inner_rect, &frame) {
                retained_fallback!("pane_rect_outside_frame");
            }
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                ws_idx,
                info.id,
            ) else {
                retained_fallback!("missing_runtime");
            };
            match runtime.collect_dirty_patch(info.inner_rect.width, info.inner_rect.height) {
                crate::pane::TerminalDirtyPatchOutcome::Clean => {
                    crate::render_prof::event("retained.pane_clean");
                }
                crate::pane::TerminalDirtyPatchOutcome::Fallback => {
                    retained_fallback!("dirty_patch_fallback");
                }
                crate::pane::TerminalDirtyPatchOutcome::Patch(patch) => {
                    crate::render_prof::event("retained.pane_patch");
                    crate::render_prof::counter("retained.patch_rows", patch.rows.len() as u64);
                    if dirty_patch_intersects_hyperlinks(&frame, info.inner_rect, &patch) {
                        retained_fallback!("hyperlink_intersection");
                    }
                    if !apply_terminal_dirty_patch(&mut frame, info.inner_rect, patch) {
                        retained_fallback!("patch_apply_failed");
                    }
                    touched = true;
                }
            }
        }

        let previous_cursor = frame.cursor.clone();
        frame.cursor = crate::server::render_stream::focused_terminal_cursor(
            &self.app.state,
            &self.app.terminal_runtimes,
        );
        let cursor_changed = frame.cursor != previous_cursor;

        if !touched && !cursor_changed {
            retained_success!("clean_no_cursor_change");
        }

        let mut broken_clients = Vec::new();
        let sent = self.send_retained_frame_to_client(*client_id, frame, &mut broken_clients);
        for broken_client in broken_clients {
            self.remove_client_and_resize_if_needed(broken_client);
        }
        if sent {
            retained_success!("sent");
        }
        retained_fallback!("send_failed");
    }

    fn retained_pty_update_allowed_by_app_state(&self) -> bool {
        self.app.state.mode == app::Mode::Terminal
            && self.app.state.focused_workspace_plugin_pane().is_none()
            && self.app.state.popup_pane.is_none()
            && self.app.state.hovered_link.is_none()
            && self.app.state.selection.is_none()
            && self.app.state.copy_mode.is_none()
            && self.app.state.context_menu.is_none()
            && self.app.state.toast.is_none()
            && self.app.state.copy_feedback.is_none()
            && !self.app.full_redraw_pending
    }

    fn send_retained_frame_to_client(
        &mut self,
        client_id: u64,
        frame: FrameData,
        broken_clients: &mut Vec<u64>,
    ) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            crate::render_prof::event("retained_send_fallback.client_missing");
            return false;
        };
        let Some(writer) = client.writer.as_ref().cloned() else {
            crate::render_prof::event("retained_send_fallback.writer_missing");
            return false;
        };
        let prepare_started = crate::render_prof::timer();
        let Some(prepared) = client.render_state.prepare_frame(frame) else {
            client.clear_deferred_render();
            crate::render_prof::event("retained_send.skip_identical");
            crate::render_prof::duration_since("retained_send.prepare_frame", prepare_started);
            return true;
        };
        crate::render_prof::duration_since("retained_send.prepare_frame", prepare_started);
        let serialize_started = crate::render_prof::timer();
        let serialized = match Self::frame_server_message(prepared.message()) {
            Ok(framed) => {
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                framed
            }
            Err(protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    client_id,
                    claimed, max, "skipping oversized retained frame for client"
                );
                crate::render_prof::event("retained_send_fallback.serialize_oversized");
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                return false;
            }
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize retained frame for client");
                broken_clients.push(client_id);
                crate::render_prof::event("retained_send_fallback.serialize_error");
                crate::render_prof::duration_since("retained_send.serialize", serialize_started);
                return false;
            }
        };
        crate::render_prof::counter("retained_send.bytes", serialized.len() as u64);

        let send_started = crate::render_prof::timer();
        match writer.render.try_send(serialized) {
            Ok(()) => {
                client.clear_deferred_render();
                client.render_state.commit_sent_frame(prepared);
                crate::render_prof::event("retained_send.sent");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                true
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                client.defer_full_render();
                crate::render_prof::event("retained_send_fallback.queue_full");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                debug!(
                    client_id,
                    "render queue full, deferring latest retained frame"
                );
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                debug!(client_id, "client writer channel closed, marking as broken");
                broken_clients.push(client_id);
                crate::render_prof::event("retained_send_fallback.writer_disconnected");
                crate::render_prof::duration_since("retained_send.try_send", send_started);
                false
            }
        }
    }

    fn render_and_stream(&mut self) {
        let full_started = crate::render_prof::timer();
        let render_targets = render_targets(&self.clients, self.foreground_client_id);

        if render_targets.is_empty() {
            // Keep canonical geometry current for clientless work such as
            // restored agent resumes, without constructing a synthetic frame.
            self.compute_foreground_navigation_view();
            crate::render_prof::duration_since("full_render.total", full_started);
            return;
        }
        let canonical_navigation = self.sync_canonical_navigation_to_foreground();
        let foreground_pre_compute_suppresses_focused_terminal_cursor =
            self.app.state.popup_pane.is_none()
                && crate::server::render_stream::focused_terminal_suppresses_host_cursor(
                    &self.app.state,
                    &self.app.terminal_runtimes,
                );
        let mut broken_clients: Vec<u64> = Vec::new();
        let mut deferred_frame = false;
        for (client_id, (cols, rows), cell_size, is_foreground, mode) in render_targets {
            let area = Rect::new(0, 0, cols, rows);
            let is_app_client = matches!(mode, ClientConnectionMode::App);
            let foreground_hover = if is_app_client && !is_foreground {
                Some((
                    self.app.state.hovered_pane_cell.take(),
                    self.app.state.hovered_link.take(),
                ))
            } else {
                None
            };
            let preserved_scroll = (is_app_client && !is_foreground).then_some((
                self.app.state.workspace_scroll,
                self.app.state.agent_panel_scroll,
                self.app.state.tab_scroll,
                self.app.state.mobile_switcher_scroll,
            ));
            if is_app_client {
                self.apply_client_navigation(client_id, &canonical_navigation);
            }
            let mut findr_changed = false;
            let mut private_surface = is_app_client
                .then(|| {
                    self.clients
                        .get_mut(&client_id)
                        .and_then(|client| client.private_surface.take())
                })
                .flatten();
            let has_private_surface = private_surface.is_some();
            let mut frame = match mode {
                ClientConnectionMode::OmpPane => continue,
                ClientConnectionMode::App => {
                    let render_started = crate::render_prof::timer();
                    let render_cell_size =
                        if self.app.state.kitty_graphics_enabled && cell_size.is_known() {
                            cell_size
                        } else {
                            crate::kitty_graphics::HostCellSize::default()
                        };
                    let identity = crate::server::render_stream::identity_ui_state(
                        self.clients
                            .get(&client_id)
                            .and_then(|client| client.identity.as_ref()),
                    );
                    let pre_compute_suppresses_focused_terminal_cursor =
                        is_foreground && foreground_pre_compute_suppresses_focused_terminal_cursor;
                    if is_foreground && !has_private_surface {
                        crate::ui::compute_view_with_cell_size(
                            &mut self.app.state,
                            &self.app.terminal_runtimes,
                            area,
                            render_cell_size,
                        );
                    } else {
                        crate::ui::compute_view_without_resizing_panes(
                            &mut self.app.state,
                            &self.app.terminal_runtimes,
                            area,
                        );
                    }
                    findr_changed = self.app.refresh_findr_visible_if_needed(&HashSet::new());
                    let (mut buffer, mut cursor) =
                        crate::server::render_stream::render_precomputed_virtual_with_runtime_registry_and_private_surface(
                            &self.app.state,
                            &self.app.terminal_runtimes,
                            area,
                            pre_compute_suppresses_focused_terminal_cursor,
                            &identity,
                            private_surface.as_ref(),
                        );
                    if let Some(info) = self.independent_omp_pane_info(client_id) {
                        let inner = info.inner_rect;
                        if inner.width > 0 && inner.height > 0 {
                            let message = if self.failed_private_omp_pane_info(client_id).is_some()
                            {
                                "OMP renderer unavailable"
                            } else if self.pending_private_omp_pane_info(client_id).is_some() {
                                "OMP renderer starting"
                            } else {
                                "OMP is open in its native renderer"
                            };
                            let style = ratatui::style::Style::default()
                                .fg(self.app.state.palette.overlay0)
                                .bg(self.app.state.palette.panel_bg);
                            ratatui::widgets::Clear.render(inner, &mut buffer);
                            ratatui::widgets::Paragraph::new(
                                ratatui::text::Line::from(message).centered(),
                            )
                            .style(style)
                            .render(inner, &mut buffer);
                            cursor = None;
                        }
                    }
                    if let Some((position, link)) = foreground_hover {
                        self.app.state.hovered_pane_cell = position;
                        self.app.state.hovered_link = link;
                    }
                    if let Some((workspace, agent_panel, tab, mobile_switcher)) = preserved_scroll {
                        self.app.state.workspace_scroll = workspace;
                        self.app.state.agent_panel_scroll = agent_panel;
                        self.app.state.tab_scroll = tab;
                        self.app.state.mobile_switcher_scroll = mobile_switcher;
                    }
                    crate::render_prof::duration_since(
                        "full_render.render_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let mut hyperlinks = crate::server::render_stream::visible_hyperlinks(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                    );
                    if let Some(info) = self.independent_omp_pane_info(client_id) {
                        hyperlinks
                            .retain(|((x, y), _, _)| !info.inner_rect.contains((*x, *y).into()));
                    }
                    if let (Some(info), Some(guest)) = (
                        self.private_omp_pane_info(client_id),
                        self.clients
                            .get(&client_id)
                            .and_then(|client| client.private_omp_guest.as_ref()),
                    ) {
                        let inner = info.inner_rect;
                        if inner.width > 0 && inner.height > 0 {
                            guest.resize(
                                inner.height,
                                inner.width,
                                cell_size.width_px,
                                cell_size.height_px,
                            );
                            let local = Rect::new(0, 0, inner.width, inner.height);
                            let (guest_buffer, guest_cursor) =
                                crate::server::render_stream::render_terminal_virtual(
                                    guest.runtime(),
                                    local,
                                );
                            for y in 0..inner.height {
                                for x in 0..inner.width {
                                    buffer[(inner.x + x, inner.y + y)] =
                                        guest_buffer[(x, y)].clone();
                                }
                            }
                            if info.is_focused {
                                cursor = guest_cursor.map(|guest_cursor| CursorState {
                                    x: inner.x.saturating_add(guest_cursor.x),
                                    y: inner.y.saturating_add(guest_cursor.y),
                                    visible: guest_cursor.visible,
                                    shape: guest_cursor.shape,
                                });
                            }
                            hyperlinks.retain(|((x, y), _, _)| !inner.contains((*x, *y).into()));
                            hyperlinks.extend(
                                guest
                                    .runtime()
                                    .visible_hyperlinks(local)
                                    .into_iter()
                                    .map(|((x, y), id, uri)| ((inner.x + x, inner.y + y), id, uri)),
                            );
                        }
                    }
                    if let Some(surface) = private_surface.as_mut() {
                        let private_area = self.app.state.view.terminal_area;
                        surface.resize(private_area, cell_size);
                        crate::server::render_stream::overlay_private_surface(
                            &mut buffer,
                            surface,
                            &self.app.state.palette,
                            private_area,
                        );
                        cursor = surface.cursor(private_area);
                        if let Some(outer) = surface.outer_rect(private_area) {
                            hyperlinks.retain(|((x, y), _, _)| {
                                !outer.contains(ratatui::layout::Position::new(*x, *y))
                            });
                        }
                        hyperlinks.extend(surface.visible_hyperlinks(private_area));
                    }
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    frame
                }
                ClientConnectionMode::TerminalAttach { terminal_id }
                | ClientConnectionMode::TerminalObserve { terminal_id } => {
                    let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) else {
                        self.send_to_client(
                            client_id,
                            ServerMessage::ServerShutdown {
                                reason: Some(format!(
                                    "terminal attach ended: terminal {terminal_id} not found"
                                )),
                            },
                        );
                        broken_clients.push(client_id);
                        continue;
                    };
                    let render_started = crate::render_prof::timer();
                    let (buffer, cursor) =
                        crate::server::render_stream::render_terminal_virtual(runtime, area);
                    crate::render_prof::duration_since(
                        "full_render.render_terminal_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let hyperlinks = runtime.visible_hyperlinks(area);
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    frame
                }
            };

            let rendered_findr = (is_app_client && findr_changed)
                .then(|| crate::server::clients::capture_findr(&self.app.state));

            let native_omp_surface_active =
                !has_private_surface && self.native_omp_surface_active(client_id);
            let excluded_graphics_pane = (!native_omp_surface_active)
                .then(|| self.replaced_omp_pane_info(client_id).map(|info| info.id))
                .flatten();
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if let Some(findr) = rendered_findr {
                if let Some(navigation) = client.navigation.as_mut() {
                    navigation.findr = findr;
                }
            }
            if let Some(surface) = private_surface {
                client.private_surface = Some(surface);
            }
            let mut next_graphics_cache = client.graphics_cache.clone();
            let mut reset_graphics = Vec::new();
            let mut encoded = if native_omp_surface_active {
                crate::kitty_graphics::EncodedGraphics {
                    bytes: Vec::new(),
                    incomplete: false,
                }
            } else if has_private_surface {
                crate::kitty_graphics::EncodedGraphics {
                    bytes: next_graphics_cache.clear_bytes(),
                    incomplete: false,
                }
            } else if is_app_client && self.app.state.kitty_graphics_enabled && cell_size.is_known()
            {
                if client.graphics_surface_reset_pending {
                    if self.app.pane_graphics.slots.is_empty() {
                        reset_graphics = next_graphics_cache.clear_bytes();
                    } else {
                        next_graphics_cache = crate::kitty_graphics::HostGraphicsCache::default();
                    }
                }
                let graphics_started = crate::render_prof::timer();
                let encoded = crate::kitty_graphics::encode_local_pane_graphics(
                    &self.app.state,
                    &self.app.pane_graphics,
                    &self.app.terminal_runtimes,
                    self.app.state.view.tab_surface(),
                    cell_size,
                    excluded_graphics_pane,
                    Some(crate::kitty_graphics::HEADLESS_GRAPHICS_TRANSACTION_BUDGET),
                    &mut next_graphics_cache,
                );
                crate::render_prof::duration_since("full_render.graphics_encode", graphics_started);
                encoded
            } else if self.app.pane_graphics.slots.is_empty() {
                crate::kitty_graphics::EncodedGraphics {
                    bytes: next_graphics_cache.clear_bytes(),
                    incomplete: false,
                }
            } else {
                next_graphics_cache.clear_next()
            };
            if !reset_graphics.is_empty() {
                reset_graphics.extend(encoded.bytes);
                encoded.bytes = reset_graphics;
            }
            frame.graphics = encoded.bytes;

            let Some(writer) = client.writer.as_ref().cloned() else {
                crate::render_prof::event("full_render.writer_missing");
                continue;
            };
            let mut commit_graphics_cache = true;
            if frame.graphics.len() > MAX_GRAPHICS_FRAME_SIZE {
                warn!(
                    client_id,
                    graphics_bytes = frame.graphics.len(),
                    max = MAX_GRAPHICS_FRAME_SIZE,
                    "dropping oversized graphics payload for client frame"
                );
                frame.graphics.clear();
                commit_graphics_cache = false;
                encoded.incomplete = false;
            }
            let has_graphics = !frame.graphics.is_empty();
            let Some(mut prepared) = client.render_state.prepare_frame(frame) else {
                if commit_graphics_cache {
                    client.graphics_cache = next_graphics_cache;
                    client.graphics_surface_reset_pending = false;
                }
                if encoded.incomplete {
                    client.defer_full_render();
                    deferred_frame = true;
                } else {
                    client.clear_deferred_render();
                }
                crate::render_prof::event("full_render.skip_identical");
                continue;
            };
            let max = if has_graphics {
                MAX_GRAPHICS_FRAME_SIZE
            } else {
                crate::protocol::MAX_FRAME_SIZE
            };
            let serialized = match Self::frame_server_message_with_max(prepared.message(), max) {
                Ok(frame) => frame,
                Err(protocol::FramingError::Oversized { claimed, max }) if has_graphics => {
                    warn!(
                        client_id,
                        claimed, max, "dropping graphics from oversized frame for client"
                    );
                    let Some(mut text_only_frame) = prepared.into_frame() else {
                        crate::render_prof::event("full_render.serialize_error");
                        continue;
                    };
                    text_only_frame.graphics.clear();
                    let Some(text_only_prepared) =
                        client.render_state.prepare_frame(text_only_frame)
                    else {
                        client.clear_deferred_render();
                        crate::render_prof::event("full_render.skip_identical_text_only");
                        continue;
                    };
                    let framed = match Self::frame_server_message(text_only_prepared.message()) {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to serialize text-only frame for client");
                            broken_clients.push(client_id);
                            crate::render_prof::event("full_render.serialize_error");
                            continue;
                        }
                    };
                    prepared = text_only_prepared;
                    commit_graphics_cache = false;
                    encoded.incomplete = false;
                    framed
                }
                Err(protocol::FramingError::Oversized { claimed, max }) => {
                    warn!(
                        client_id,
                        claimed, max, "skipping oversized frame for client"
                    );
                    crate::render_prof::event("full_render.serialize_oversized");
                    continue;
                }
                Err(err) => {
                    warn!(client_id, err = %err, "failed to serialize frame");
                    broken_clients.push(client_id);
                    crate::render_prof::event("full_render.serialize_error");
                    continue;
                }
            };
            match writer.render.try_send(serialized) {
                Ok(()) => {
                    if commit_graphics_cache {
                        client.graphics_cache = next_graphics_cache;
                        client.graphics_surface_reset_pending = false;
                    }
                    client.render_state.commit_sent_frame(prepared);
                    if encoded.incomplete {
                        client.defer_full_render();
                        deferred_frame = true;
                    } else {
                        client.clear_deferred_render();
                    }
                    crate::render_prof::event("full_render.sent");
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                    deferred_frame = true;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    broken_clients.push(client_id);
                }
            }
        }

        if !broken_clients.is_empty() {
            for client_id in broken_clients {
                self.remove_client_and_resize_if_needed(client_id);
            }
        }
        // App targets render background-first and foreground-last, so the final
        // layout/navigation projection is already canonical for local input.
        self.restore_foreground_navigation(&canonical_navigation);

        self.compute_foreground_navigation_view();
        let (cols, rows) = self.effective_size;
        if !deferred_frame {
            self.app.full_redraw_pending = false;
        }
        crate::render_prof::duration_since("full_render.total", full_started);
        debug!(cols, rows, foreground_client_id = ?self.foreground_client_id, "rendered virtual frame(s)");
    }

    /// Handle scheduled tasks for the headless server.
    ///
    /// Similar to `App::handle_scheduled_tasks` but without resize polling
    /// (the server doesn't have a terminal to resize).
    fn handle_scheduled_tasks_headless(&mut self, now: Instant, geometry_dirty: bool) -> bool {
        let mut changed = false;

        let expired_clients = self
            .private_surface_candidates
            .iter()
            .filter_map(|(&client_id, candidate)| (now >= candidate.deadline).then_some(client_id))
            .collect::<Vec<_>>();
        for client_id in expired_clients {
            let Some(candidate) = self.private_surface_candidates.remove(&client_id) else {
                continue;
            };
            warn!(
                client_id,
                "remote private popup did not become ready before timeout"
            );
            self.retire_private_surface_candidate(
                candidate,
                "plugin_pane_open_failed",
                "remote private popup did not become ready before timeout",
            );
            changed = true;
        }

        if self
            .app
            .config_diagnostic_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.config_diagnostic_deadline = None;
            self.app.state.config_diagnostic = None;
            changed = true;
        }

        if self
            .app
            .toast_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.toast_deadline = None;
            self.app.state.toast = None;
            changed = true;
        }

        if self
            .app
            .state
            .next_pending_agent_notification_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let previous_toast = self.app.state.toast.clone();
            let mut deliveries = self.app.state.drain_due_agent_notifications(now);
            if !deliveries.is_empty() {
                self.app
                    .refresh_agent_notification_delivery_contexts(&mut deliveries);
                self.app.sync_toast_deadline(previous_toast);
                for delivery in &deliveries {
                    self.forward_agent_notification_delivery(delivery);
                }
                changed = true;
            }
        }

        if self
            .app
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.copy_feedback_deadline = None;
            self.app.state.copy_feedback = None;
            changed = true;
        }

        if self
            .app
            .selection_autoscroll_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.tick_selection_autoscroll(now);
            changed = true;
        }

        changed |= self.app.clear_due_selection_highlight(now);
        let findr_changed = self.app.tick_findr_scan(now);
        changed |= findr_changed && self.has_app_client();

        if self.has_app_client() {
            self.app.start_git_status_refresh_if_due(now);
        }

        if self
            .app
            .next_auto_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.run_auto_update_check();
        }

        if self
            .app
            .next_agent_manifest_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.run_agent_manifest_update_check();
        }

        if self
            .app
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.app.start_background_session_save();
        }

        if let Some(deadline) = self
            .app
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        {
            self.app.expire_metadata_at(deadline, now);
            changed = true;
        }

        changed |= self.app.handle_tab_bar_status_tasks(now);

        if geometry_dirty {
            self.app.pending_agent_resume_retry_at = None;
        } else {
            self.app.sync_pending_agent_resume_retry_at(now);
            changed |= self
                .app
                .start_pending_agent_resumes(now, self.app.pending_agent_resume_retry_due(now));
        }
        changed
    }

    /// Initiates graceful shutdown.
    fn initiate_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        info!("server shutdown initiated");
        self.shutting_down = true;

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        self.send_all_clients_graphics_cleanup();
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        };
        self.send_to_all_clients(shutdown_msg);

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        // Signal the main loop to exit.
        self.should_quit.store(true, Ordering::Release);
        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence: send ServerShutdown to clients,
    /// close client connections, remove socket files, and clean up.
    async fn complete_shutdown(&mut self) -> io::Result<()> {
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            self.send_all_clients_graphics_cleanup();
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some("server is shutting down".to_owned()),
            };
            self.send_to_all_clients(shutdown_msg);

            // Give writer threads a moment to flush before closing.
            std::thread::sleep(Duration::from_millis(50));
        }

        // Reject only the requests already queued when shutdown reached cleanup.
        self.reject_queued_api_requests_for_shutdown();

        // Close all client connections.
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, client)| client.staged_clipboard_files)
            .collect::<Vec<_>>();
        crate::server::clipboard_image::remove_files(staged_files);

        // Remove socket files.
        self.cleanup_sockets()?;

        Ok(())
    }

    /// Removes socket files created by the server.
    fn cleanup_sockets(&self) -> io::Result<()> {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
        {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(
                    path = %self.client_socket_path.display(),
                    err = %err,
                    "failed to remove client socket on shutdown"
                );
            }
        }
        Ok(())
    }
}

// Pane applications render their own motion responses through PTY output. Only Herdr modes with
// hover selection mutate the current frame directly from a plain mouse-move event.
fn events_are_render_neutral_mouse_motion(
    events: &[crate::raw_input::RawInputEvent],
    mode: crate::app::Mode,
) -> bool {
    !events.is_empty()
        && !mode.mouse_motion_changes_view()
        && events.iter().all(|event| {
            matches!(
                event,
                crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Moved,
                    ..
                })
            )
        })
}

fn events_for_app_routing(
    events: Vec<crate::raw_input::RawInputEvent>,
    mut source_is_foreground: bool,
    source_is_full_app: bool,
) -> Vec<crate::raw_input::RawInputEvent> {
    events
        .into_iter()
        .filter_map(|event| match event {
            crate::raw_input::RawInputEvent::OuterFocusGained
            | crate::raw_input::RawInputEvent::OuterFocusLost
                if !source_is_full_app =>
            {
                None
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                source_is_foreground = true;
                Some(event)
            }
            crate::raw_input::RawInputEvent::OuterFocusLost if !source_is_foreground => None,
            crate::raw_input::RawInputEvent::Key(_)
            | crate::raw_input::RawInputEvent::Text(_)
            | crate::raw_input::RawInputEvent::Mouse(_)
            | crate::raw_input::RawInputEvent::Paste(_) => {
                source_is_foreground = true;
                Some(event)
            }
            _ => Some(event),
        })
        .collect()
}

impl Drop for HeadlessServer {
    fn drop(&mut self) {
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, mut client)| {
                if let Some(surface) = client.private_surface.take() {
                    surface.shutdown();
                }
                client.staged_clipboard_files
            })
            .collect::<Vec<_>>();
        let candidates = self
            .private_surface_candidates
            .drain()
            .map(|(_, candidate)| candidate)
            .collect::<Vec<_>>();
        for candidate in candidates {
            self.retire_private_surface_candidate(
                candidate,
                "server_unavailable",
                "server is shutting down",
            );
        }
        crate::server::clipboard_image::remove_files(staged_files);
        let _ = self.cleanup_sockets();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Installs a Ctrl+C handler that sets the should_quit flag and wakes up
/// the event loop by sending a QuitSignal on the server event channel.
fn ctrlc_handler(should_quit: Arc<AtomicBool>, server_event_tx: mpsc::Sender<ServerEvent>) {
    let _ = ctrlc::set_handler(move || {
        should_quit.store(true, Ordering::Release);
        // Wake up the event loop so the quit flag is checked promptly.
        let _ = server_event_tx.try_send(ServerEvent::QuitSignal);
    });
}

/// Sleep until a deadline, or return pending if none.
async fn sleep_until_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

fn sanitize_notification_text(value: &str, max_chars: usize) -> Option<String> {
    let mut sanitized = String::new();
    let mut previous_space = false;
    for ch in value.chars() {
        let replacement = if ch == '\n' || ch == '\r' || ch == '\t' {
            Some(' ')
        } else if ch.is_control() {
            None
        } else {
            Some(ch)
        };
        let Some(ch) = replacement else {
            continue;
        };
        if ch.is_whitespace() {
            if previous_space {
                continue;
            }
            previous_space = true;
            sanitized.push(' ');
        } else {
            previous_space = false;
            sanitized.push(ch);
        }
        if sanitized.chars().count() >= max_chars {
            break;
        }
    }
    let sanitized = sanitized.trim().to_string();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn server_config_diagnostic_summaries(diagnostics: &[String]) -> (Option<String>, Option<String>) {
    let without_keybindings = diagnostics
        .iter()
        .filter(|diagnostic| !is_keybinding_config_diagnostic(diagnostic))
        .cloned()
        .collect::<Vec<_>>();
    (
        config::config_diagnostic_summary(diagnostics),
        config::config_diagnostic_summary(&without_keybindings),
    )
}

fn is_keybinding_config_diagnostic(diagnostic: &str) -> bool {
    diagnostic.contains("keybinding") || diagnostic.contains("keys.")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server() -> io::Result<()> {
    init_logging();
    crate::platform::raise_server_nofile_limit();

    let args: Vec<String> = std::env::args().collect();
    if args.get(2).map(String::as_str) == Some("--handoff-import") {
        let socket_path = args
            .get(3)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff socket"))?;
        let token = args
            .get(4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff token"))?;
        return run_handoff_import_server(&socket_path, token);
    }

    let loaded_config = config::Config::load();
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let should_quit = Arc::new(AtomicBool::new(false));

    // Start the JSON API socket server.
    let _api_server = match api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        should_quit.clone(),
    ) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: herdr server is already running");
            eprintln!("api socket: {}", api::socket_path().display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    let no_session = false; // Server always does session persistence.

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        let prepared_omp_bridge = omp_bridge::bind()?;
        // Restore panes only after the bridge is ready so their shells inherit
        // the exact address and pane-scoped token used by this server.
        let mut app = app::App::new_with_omp_bridge(
            &loaded_config.config,
            no_session,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub,
            Some(prepared_omp_bridge.1.clone()),
        );
        seed_startup_workspace_if_empty(&mut app);

        // The server runs headless — disable local notification side effects.
        // Sound and terminal notifications are forwarded to connected clients
        // as ServerMessage::Notify instead of emitted by the server process.
        // The prefix input-source switch is likewise forwarded to the foreground
        // client (ServerMessage::PrefixInputSource), never applied in-process.
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;

        // Create the headless server.
        let mut server = match HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(_api_server),
            should_quit,
            Some(prepared_omp_bridge),
        ) {
            Ok(server) => server,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("error: herdr server is already running");
                eprintln!("client socket: {}", client_socket_path().display());
                std::process::exit(1);
            }
            Err(err) => return Err(err),
        };

        info!(
            api_socket = %api::socket_path().display(),
            client_socket = %client_socket_path().display(),
            "herdr server started"
        );
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();

        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

fn seed_startup_workspace_if_empty(app: &mut app::App) {
    let Some(cwd) = take_startup_cwd() else {
        return;
    };

    if !app.state.workspaces.is_empty() {
        info!(
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    match app.create_workspace_with_options(cwd.clone(), true) {
        Ok(_) => {
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

fn take_startup_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR);
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(unix)]
fn run_handoff_import_server(socket_path: &Path, token: &str) -> io::Result<()> {
    let loaded_config = config::Config::load();
    let mut received = crate::server::handoff::receive(socket_path, token)?;
    crate::server::handoff::log_import_result(received.manifest.panes.len());

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let should_quit = Arc::new(AtomicBool::new(false));

    let mut imports = HashMap::new();
    for (pane, fd) in received.manifest.panes.into_iter().zip(received.fds) {
        let pane_id = pane.pane_id;
        imports.insert(
            pane_id,
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: pane,
            },
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        let mut app = app::App::new_from_handoff(
            &loaded_config.config,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
            &received.manifest.snapshot,
            &mut imports,
        )?;
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;
        crate::server::handoff::report_restored(&mut received.stream)?;
        if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("after_restored") {
            return Err(io::Error::other(
                "test handoff import failure after restored",
            ));
        }
        wait_for_old_public_sockets_to_close(Duration::from_secs(5))?;

        let api_server = api::start_server_with_stop_control(
            api_tx.clone(),
            event_hub.clone(),
            should_quit.clone(),
        )?;
        let mut server = HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(api_server),
            should_quit,
            None,
        )?;
        server
            .omp_service
            .validate_maintenance_handoff_state(received.manifest.omp_maintenance.as_ref())
            .map_err(|error| io::Error::other(error.message()))?;
        // Carried across before any client attaches, so the first title sent is
        // the override rather than the configured one it replaced.
        server.api_window_title = received.manifest.api_window_title.take();
        crate::server::handoff::report_ready(&mut received.stream)?;
        crate::server::handoff::wait_committed(&mut received.stream)?;
        server.app.assume_handoff_ownership();
        server.app.unpause_handoff_readers();
        server.pending_handoff_repaint_nudge = true;
        if let Err(err) = crate::server::handoff::report_owned(&mut received.stream) {
            warn!(err = %err, "failed to report handoff ownership; continuing as owner");
        }
        info!("handoff import server started");
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();
        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

#[cfg(unix)]
fn wait_for_old_public_sockets_to_close(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let api_socket = api::socket_path();
    let client_socket = client_socket_path();
    while Instant::now() < deadline {
        let api_open = api_socket.exists() && crate::ipc::connect_local_stream(&api_socket).is_ok();
        let client_open =
            client_socket.exists() && crate::ipc::connect_local_stream(&client_socket).is_ok();
        if !api_open && !client_open {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "old server sockets did not close before handoff import bind",
    ))
}

#[cfg(not(unix))]
fn run_handoff_import_server(_socket_path: &Path, _token: &str) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path) {
    eprintln!("herdr server running; you can use any herdr CLI command in another terminal.");
    eprintln!("api socket: {}", api_socket.display());
    eprintln!("client socket: {}", client_socket.display());
    eprintln!(
        "logs: {}",
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    eprintln!("did you mean to open the Herdr TUI? run `herdr`; you do not need `herdr server`.");
}

/// Initialize logging for the server process.
fn init_logging() {
    crate::logging::init_file_logging("herdr-server.log");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Read as _;

    fn test_host_admission_sender(
    ) -> std::sync::mpsc::SyncSender<crate::server::client_transport::OmpHostAdmission> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = receiver.recv();
        });
        sender
    }

    use crate::app::{AppState, ClientPrivatePluginPopupOrigin};
    use crate::protocol::{CellData, CursorState};
    use unicode_width::UnicodeWidthStr;

    #[path = "pane_graphics.rs"]
    mod pane_graphics_tests;

    #[test]
    fn retained_render_plan_covers_each_render_path() {
        assert_eq!(
            retained_render_plan(RetainedRenderInput {
                needs_full_render: true,
                needs_graphics_render: true,
                pty: PtyRenderState::Hidden,
            }),
            RetainedRenderPlan::Full
        );
        assert_eq!(
            retained_render_plan(RetainedRenderInput {
                needs_full_render: false,
                needs_graphics_render: true,
                pty: PtyRenderState::Hidden,
            }),
            RetainedRenderPlan::Graphics
        );
        assert_eq!(
            retained_render_plan(RetainedRenderInput {
                needs_full_render: false,
                needs_graphics_render: false,
                pty: PtyRenderState::Visible,
            }),
            RetainedRenderPlan::Pty
        );
        assert_eq!(
            retained_render_plan(RetainedRenderInput {
                needs_full_render: false,
                needs_graphics_render: false,
                pty: PtyRenderState::Hidden,
            }),
            RetainedRenderPlan::HiddenPty
        );
    }

    #[test]
    fn retained_pty_update_disables_workspace_plugin_footer_overlay() {
        let mut server = test_headless_server();
        server.app.state.mode = app::Mode::Navigate;
        assert!(!server.retained_pty_update_allowed_by_app_state());

        server.app.state.mode = app::Mode::Terminal;
        assert!(server.retained_pty_update_allowed_by_app_state());

        server.app.state.hovered_link = Some(crate::app::HoveredPaneLink {
            pane_id: crate::layout::PaneId::alloc(),
            inner_rect: Rect::default(),
            cells: vec![(0, 0)],
        });
        assert!(!server.retained_pty_update_allowed_by_app_state());
        server.app.state.hovered_link = None;

        let workspace = crate::workspace::Workspace::test_new("retained-plugin-footer");
        let workspace_id = workspace.id.clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.view.layout = crate::app::state::ViewLayout::Desktop;
        server.app.state.view.workspace_plugin_pane_inner = Rect::new(80, 1, 20, 20);
        server.app.state.workspace_plugin_panes.insert(
            workspace_id,
            crate::app::state::WorkspacePluginPaneState {
                pane_id: crate::layout::PaneId::alloc(),
                terminal_id: crate::terminal::TerminalId::alloc(),
                plugin_id: "example.explorer".into(),
                entrypoint: "explorer".into(),
                width: None,
                focused: true,
                collapsed: false,
            },
        );

        assert!(!server.retained_pty_update_allowed_by_app_state());
    }

    #[tokio::test]
    async fn clicking_workspace_plugin_then_escape_returns_to_tiled_pane() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("plugin-focus-escape");
        let workspace_id = workspace.id.clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Terminal;
        server.app.state.view.layout = crate::app::state::ViewLayout::Desktop;

        let plugin_pane_id = crate::layout::PaneId::alloc();
        let plugin_terminal_id = crate::terminal::TerminalId::alloc();
        let (plugin_runtime, _plugin_input) =
            crate::terminal::TerminalRuntime::test_with_channel(40, 24);
        server
            .app
            .terminal_runtimes
            .insert(plugin_terminal_id.clone(), plugin_runtime);
        server.app.state.workspace_plugin_panes.insert(
            workspace_id.clone(),
            crate::app::state::WorkspacePluginPaneState {
                pane_id: plugin_pane_id,
                terminal_id: plugin_terminal_id,
                plugin_id: "example.explorer".into(),
                entrypoint: "explorer".into(),
                width: Some(crate::popup_size::PopupSize::Cells(26)),
                focused: false,
                collapsed: false,
            },
        );

        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.compute_client_navigation_view(1);
        let plugin_inner = server.app.state.view.workspace_plugin_pane_inner;
        assert!(!plugin_inner.is_empty());

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Down(
                    crate::protocol::ClientMouseButton::Left,
                ),
                column: plugin_inner.x.saturating_add(1),
                row: plugin_inner.y.saturating_add(1),
                modifiers: 0,
            }],
        }));
        assert!(server.app.state.workspace_plugin_panes[&workspace_id].focused);

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Esc,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert!(!server.app.state.workspace_plugin_panes[&workspace_id].focused);
        assert!(server
            .clients
            .get(&1)
            .and_then(|client| client.navigation.as_ref())
            .is_some_and(|navigation| navigation.focused_workspace_plugin_pane.is_none()));

        shutdown_test_runtimes(&mut server);
    }

    static NEXT_TEST_SERVER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    fn test_headless_server() -> HeadlessServer {
        test_headless_server_with_event_hub(api::EventHub::default())
    }

    fn test_headless_server_with_event_hub(event_hub: api::EventHub) -> HeadlessServer {
        let config = crate::config::Config::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(&config, true, None, api_rx, event_hub);
        app.state.local_sound_playback = false;
        app.local_terminal_notifications = false;
        app.local_input_source_switch = false;

        let dir = std::env::temp_dir().join(format!(
            "hh-{}-{}",
            std::process::id(),
            NEXT_TEST_SERVER_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::create_dir_all(&dir);
        let socket_path = dir.join("client.sock");
        let _ = fs::remove_file(&socket_path);
        let listener = bind_local_listener(&socket_path).expect("bind test listener");
        let client_socket_identity =
            socket_file_identity(&socket_path).expect("test listener socket identity");
        #[cfg(unix)]
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .expect("set listener nonblocking");
        let (server_event_tx, server_event_rx) = mpsc::channel(64);
        let should_quit = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());
        let server_keybindings = app_keybindings(&app);
        let headless_size = app.state.headless_size;

        HeadlessServer {
            app,
            #[cfg(unix)]
            api_tx: None,
            api_server: None,
            #[cfg(unix)]
            client_listener: listener,
            client_socket_path: socket_path,
            client_socket_identity,
            clients: HashMap::new(),
            private_surface_candidates: HashMap::new(),
            independent_omp_renderers_enabled: true,
            private_omp_failed_routes: HashMap::new(),
            private_omp_retry_attempted_routes: HashMap::new(),
            next_private_omp_retry_id: 1,
            private_omp_pending_routes: HashMap::new(),
            private_omp_executable: None,
            private_omp_resolving: None,
            #[cfg(test)]
            private_omp_test_executable: Some(
                std::env::current_exe().expect("locate the headless test executable"),
            ),
            next_omp_renderer_launch_id: 1,
            omp_service: OmpService::new(Some(omp_bridge::bind().expect("bind test OMP bridge")))
                .expect("create test OMP service"),
            retired_private_pane_ids: VecDeque::new(),
            #[cfg(unix)]
            next_client_id: 1,
            foreground_client_id: None,
            sent_window_title: None,
            api_window_title: None,
            server_keybindings,
            server_config_diagnostic: None,
            server_config_diagnostic_without_keybindings: None,
            terminal_attach_owners: HashMap::new(),
            pending_alt_screen_reads: Vec::new(),
            deferred_alt_screen_reads: Vec::new(),
            next_activity_stamp: 1,
            headless_size,
            effective_size: headless_size,
            shutting_down: false,
            handoff_in_progress: false,
            #[cfg(unix)]
            pending_handoff_repaint_nudge: false,
            should_quit,
            server_event_rx,
            server_event_tx,
        }
    }
    fn start_test_omp_host(
        server: &mut HeadlessServer,
        pane_id: String,
        omp_session_id: &str,
        host_id: u64,
    ) -> (std::net::TcpStream, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
            .expect("connect OMP host peer");
        let (socket, _) = listener.accept().expect("accept OMP host peer");
        let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(8);
        let _ = server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id,
            omp_session_id: omp_session_id.into(),
            route_generation: 1,
            host_id,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        });
        (peer, outbound_rx)
    }

    #[tokio::test]
    async fn pending_private_companion_masks_host_pane_and_input() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-pending");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        let route = OmpRouteKey {
            pane_id: crate::workspace::public_pane_id_for_number(&workspace.id, 1),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let host_uri = "https://example.com/host";
        let (runtime, mut host_input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                format!("HOST PTY \x1b[?1000h\x1b]8;;{host_uri}\x1b\\HOST LINK\x1b]8;;\x1b\\")
                    .as_bytes(),
                8,
            );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(terminal_id, runtime);
        let (writer, _control_rx, render_rx) = test_client_writer();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), Some(writer)));
        server.foreground_client_id = Some(1);
        server.private_omp_test_executable = None;
        server.private_omp_resolving = Some((1, route.clone()));
        let (_host, _host_messages) =
            start_test_omp_host(&mut server, route.pane_id.clone(), "session", 1);

        assert_eq!(server.private_omp_pending_routes.get(&1), Some(&route));
        server.render_and_stream();
        let frame = read_server_frame(render_rx.recv_timeout(Duration::from_millis(100)).unwrap());
        let text = frame_text(&frame);
        assert!(text.contains("OMP renderer starting"));
        assert!(!text.contains("HOST PTY"));
        assert!(!frame.hyperlinks.iter().any(|link| link == host_uri));
        assert_eq!(frame.cursor, None);
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("host runtime")
            .test_process_pty_bytes(b"\rHOST RETAINED LEAK");
        assert!(!server.render_retained_pty_update_and_stream());
        assert!(render_rx.recv_timeout(Duration::from_millis(50)).is_err());
        server.render_and_stream();
        assert!(render_rx.recv_timeout(Duration::from_millis(50)).is_err());
        let rerendered = server.clients[&1]
            .render_state
            .last_frame()
            .expect("masked frame remains retained");
        let rerendered_text = frame_text(rerendered);
        assert!(rerendered_text.contains("OMP renderer starting"));
        assert!(!rerendered_text.contains("HOST RETAINED LEAK"));
        assert_eq!(rerendered.cursor, None);

        let canonical = server.begin_client_navigation_scope(1).unwrap();
        server.compute_client_navigation_view(1);
        let inner = server
            .pending_private_omp_pane_info(1)
            .expect("pending OMP route is masked")
            .inner_rect;
        server.finish_client_navigation_scope(1, canonical);
        assert!(server.handle_client_input_events(
            1,
            vec![
                crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
                    KeyCode::Char('x'),
                    KeyModifiers::empty(),
                )),
                crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new("text")),
                crate::raw_input::RawInputEvent::Paste("paste".into()),
                crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: inner.x,
                    row: inner.y,
                    modifiers: KeyModifiers::empty(),
                }),
                crate::raw_input::RawInputEvent::OuterFocusGained,
            ],
        ));
        assert!(host_input.try_recv().is_err());

        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.host_sgr_pixels_active = Some(true);
        let x = u32::from(inner.x) * 10 + 1;
        let y = u32::from(inner.y) * 20 + 1;
        assert!(!server.handle_server_event(ServerEvent::ClientInputPixels {
            client_id: 1,
            data: format!("\x1b[<0;{x};{y}M").into_bytes(),
            geometry,
        }));
        assert!(host_input.try_recv().is_err());
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn private_companion_retry_masks_then_releases_stale_native_target() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-resolution-failure");
        let pane_id = workspace.tabs[0].root_pane;
        let route = OmpRouteKey {
            pane_id: crate::workspace::public_pane_id_for_number(&workspace.id, 1),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: Rect::new(0, 0, 80, 24),
            inner_rect: Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        }];
        // Inspect the queue synchronously: test_client_writer forwards control records on a
        // background thread, so try_recv can race with delivery of an already queued message.
        let writer = ClientWriter::test_backpressured();
        let mut client = test_identity_client(Some("Ada"), Some(writer.clone()));
        client.renderer_binding_token = Some("binding".into());
        client.omp_renderer_capabilities.client_local_native = true;
        server.clients.insert(7, client);
        server.private_omp_test_executable = None;
        server.private_omp_resolving = Some((7, route.clone()));
        let (_host, _host_messages) =
            start_test_omp_host(&mut server, route.pane_id.clone(), "session", 1);
        // The native child is gone from OmpService, but its last target is still bound.
        let target = server.clients[&7]
            .omp_renderer_target
            .clone()
            .expect("server offers the focused native route");
        let renderer_launch_id = target.launch_id;
        server.clients.get_mut(&7).unwrap().omp_renderer_target = Some(OmpRendererTargetState {
            bound: true,
            ready: true,
            surface_active: true,
            ..target
        });
        while writer.test_pop_control().is_some() {}
        assert_eq!(server.private_omp_pending_routes.get(&7), Some(&route));

        assert!(
            server.handle_server_event(ServerEvent::OmpPrivateCompanionResolved {
                result: Err("OMP companion unavailable".into()),
            })
        );
        assert_eq!(server.private_omp_failed_routes.get(&7), Some(&route));
        let retry_id = match server.private_omp_retry_attempted_routes.get(&7) {
            Some((attempted, PrivateOmpRetryState::Pending(retry_id))) if attempted == &route => {
                *retry_id
            }
            other => panic!("unexpected retry state: {other:?}"),
        };
        let native_target = server.clients[&7]
            .omp_renderer_target
            .as_ref()
            .expect("stale native target remains masked during retry delay");
        assert!(native_target.bound);
        assert!(native_target.ready);
        assert!(native_target.surface_active);
        assert!(writer.test_control_records().is_empty());
        assert!(server.independent_omp_pane_info(7).is_some());

        let (remaining, consumed) = server.partition_native_omp_input(
            7,
            vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty()),
            )],
        );
        assert!(consumed);
        assert!(remaining.is_empty());

        server.private_omp_test_executable = Some(
            server
                .client_socket_path
                .with_extension("missing-private-omp"),
        );
        assert!(
            server.handle_server_event(ServerEvent::OmpPrivateCompanionRetry {
                client_id: 7,
                route: route.clone(),
                retry_id,
            })
        );
        assert_eq!(server.private_omp_failed_routes.get(&7), Some(&route));
        assert_eq!(
            server.private_omp_retry_attempted_routes.get(&7),
            Some(&(route.clone(), PrivateOmpRetryState::Consumed))
        );
        let fallback_target = server.clients[&7]
            .omp_renderer_target
            .as_ref()
            .expect("terminal fallback failure releases the stale native target");
        assert!(!fallback_target.bound);
        assert!(!fallback_target.ready);
        assert!(!fallback_target.surface_active);
        match read_server_message(
            writer
                .test_pop_control()
                .expect("terminal failure target release"),
        ) {
            ServerMessage::OmpRendererTarget {
                launch_id,
                route: Some(released_route),
                bound: false,
                surface_active: false,
                ..
            } => {
                assert_eq!(launch_id, renderer_launch_id);
                assert_eq!(released_route, HeadlessServer::omp_renderer_route(&route));
            }
            message => panic!("expected unbound renderer target, got {message:?}"),
        }
    }

    #[test]
    fn cached_private_omp_executable_is_reverified_before_reuse() {
        let mut server = test_headless_server();
        let route = OmpRouteKey {
            pane_id: "w1:p1".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let missing = server
            .client_socket_path
            .with_extension("missing-private-omp");
        server.private_omp_test_executable = None;
        server.private_omp_executable = Some(crate::update::OmpExecutable::Explicit(missing));
        server.private_omp_resolving = Some((99, route.clone()));

        assert!(server
            .private_omp_executable_for_launch(7, &route)
            .is_none());
        assert!(server.private_omp_executable.is_none());
        assert!(matches!(
            server.private_omp_resolving.as_ref(),
            Some((99, resolving_route)) if resolving_route == &route
        ));
    }

    #[test]
    fn stale_private_retry_ticket_cannot_reopen_same_route() {
        let mut server = test_headless_server();
        let route = OmpRouteKey {
            pane_id: "w1:p1".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        server.private_omp_failed_routes.insert(7, route.clone());
        server
            .private_omp_retry_attempted_routes
            .insert(7, (route.clone(), PrivateOmpRetryState::Pending(2)));

        assert!(
            !server.handle_server_event(ServerEvent::OmpPrivateCompanionRetry {
                client_id: 7,
                route: route.clone(),
                retry_id: 1,
            })
        );
        assert_eq!(server.private_omp_failed_routes.get(&7), Some(&route));
        assert_eq!(
            server.private_omp_retry_attempted_routes.get(&7),
            Some(&(route, PrivateOmpRetryState::Pending(2)))
        );
    }

    #[tokio::test]
    async fn failed_private_resolution_restarts_for_waiter_after_owner_disconnect() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-disconnect");
        let route = OmpRouteKey {
            pane_id: crate::workspace::public_pane_id_for_number(&workspace.id, 1),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.private_omp_test_executable = None;
        let (_host, _host_messages) =
            start_test_omp_host(&mut server, route.pane_id.clone(), "session", 1);
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));
        server
            .clients
            .insert(2, test_identity_client(Some("Bea"), None));
        server.private_omp_resolving = Some((1, route.clone()));
        assert!(server.reconcile_omp_renderers());
        assert_eq!(server.private_omp_pending_routes.get(&1), Some(&route));
        assert_eq!(server.private_omp_pending_routes.get(&2), Some(&route));

        assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 1 }));
        assert!(!server.clients.contains_key(&1));
        assert_eq!(server.private_omp_pending_routes.get(&2), Some(&route));
        let _ = server.handle_server_event(ServerEvent::OmpPrivateCompanionResolved {
            result: Err("OMP companion unavailable".into()),
        });

        assert!(!server.private_omp_failed_routes.contains_key(&1));
        assert!(matches!(
            server.private_omp_resolving.as_ref(),
            Some((2, resolving_route)) if resolving_route == &route
        ));
        assert_eq!(server.private_omp_pending_routes.get(&2), Some(&route));
    }

    #[tokio::test]
    async fn stale_private_resolution_restarts_for_current_route() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-route-change");
        let pane_id = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        let old_route = OmpRouteKey {
            pane_id: pane_id.clone(),
            omp_session_id: "old".into(),
            route_generation: 1,
        };
        let replacement_route = OmpRouteKey {
            pane_id: pane_id.clone(),
            omp_session_id: "a".into(),
            route_generation: 1,
        };
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.private_omp_test_executable = None;
        let (_old_host, _old_messages) =
            start_test_omp_host(&mut server, pane_id.clone(), "old", 1);
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));
        server.private_omp_resolving = Some((1, old_route.clone()));
        assert!(server.reconcile_omp_renderers());
        assert_eq!(server.private_omp_pending_routes.get(&1), Some(&old_route));

        let (_new_host, _new_messages) = start_test_omp_host(&mut server, pane_id, "a", 2);
        assert_eq!(
            server.private_omp_pending_routes.get(&1),
            Some(&replacement_route)
        );
        let _ = server.handle_server_event(ServerEvent::OmpPrivateCompanionResolved {
            result: Err("OMP companion unavailable".into()),
        });

        assert!(!server.private_omp_failed_routes.contains_key(&1));
        assert!(matches!(
            server.private_omp_resolving.as_ref(),
            Some((1, resolving_route)) if resolving_route == &replacement_route
        ));
        assert_eq!(
            server.private_omp_pending_routes.get(&1),
            Some(&replacement_route)
        );
    }

    fn shutdown_test_runtimes(server: &mut HeadlessServer) {
        for (_, runtime) in server.app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    fn request_pane_omp_bridge(
        server: &mut HeadlessServer,
        id: &str,
        pane_id: &str,
        local_peer_pid: Option<u32>,
    ) -> String {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            !server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: id.into(),
                    method: api::schema::Method::PaneOmpBridge(api::schema::PaneOmpBridgeParams {
                        pane_id: pane_id.into(),
                    },),
                },
                context: api::ApiRequestContext { local_peer_pid },
                respond_to,
                response_write_complete: None,
                stream_active: None,
            })
        );
        response_rx.recv().expect("OMP bridge response")
    }

    fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
        let mut cursor = std::io::Cursor::new(bytes);
        protocol::read_message(&mut cursor, MAX_FRAME_SIZE).expect("decode server message")
    }

    fn read_server_frame(bytes: Vec<u8>) -> FrameData {
        match protocol::read_message(&mut std::io::Cursor::new(bytes), MAX_GRAPHICS_FRAME_SIZE)
            .expect("decode server frame")
        {
            ServerMessage::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        }
    }

    fn frame_text(frame: &FrameData) -> String {
        frame
            .cells
            .chunks(usize::from(frame.width))
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn read_server_shutdown_reason(bytes: Vec<u8>) -> Option<String> {
        match read_server_message(bytes) {
            ServerMessage::ServerShutdown { reason } => reason,
            other => panic!("expected shutdown, got {other:?}"),
        }
    }

    #[test]
    fn default_headless_size_is_effective_without_clients() {
        let server = test_headless_server();

        assert_eq!(
            server.headless_size,
            (
                crate::config::DEFAULT_HEADLESS_COLS,
                crate::config::DEFAULT_HEADLESS_ROWS
            )
        );
        assert_eq!(server.effective_size, server.headless_size);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn pane_omp_bridge_resolves_attributed_pane_over_stale_request() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("omp-bridge-discovery");
        let source_pane = workspace.tabs[0].root_pane;
        let target_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let source_terminal_id = server.app.state.workspaces[0]
            .terminal_id(source_pane)
            .cloned()
            .expect("source terminal");
        let target_terminal_id = server.app.state.workspaces[0]
            .terminal_id(target_pane)
            .cloned()
            .expect("target terminal");
        let (source_runtime, _source_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        source_runtime.test_set_child_pid(std::process::id());
        server
            .app
            .terminal_runtimes
            .insert(source_terminal_id.clone(), source_runtime);
        let (target_runtime, _target_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        server
            .app
            .terminal_runtimes
            .insert(target_terminal_id, target_runtime);

        let workspace = &server.app.state.workspaces[0];
        let source_pane_id = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace
                .public_pane_number(source_pane)
                .expect("source pane number"),
        );
        let target_pane_id = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace
                .public_pane_number(target_pane)
                .expect("target pane number"),
        );

        let response: api::schema::SuccessResponse =
            serde_json::from_str(&request_pane_omp_bridge(
                &mut server,
                "attributed-pane",
                &target_pane_id,
                Some(std::process::id()),
            ))
            .expect("authorized OMP bridge response");
        let api::schema::ResponseResult::PaneOmpBridge {
            pane_id,
            address,
            token,
        } = response.result
        else {
            panic!("expected pane OMP bridge response");
        };
        assert_eq!(pane_id, source_pane_id);
        assert_ne!(pane_id, target_pane_id);
        assert_eq!(address, server.omp_service.bridge().address());
        assert!(server.omp_service.bridge().validates(&pane_id, &token));

        for (id, requested_pane, peer_pid) in [
            ("missing-peer", source_pane_id.as_str(), None),
            ("unknown-peer", source_pane_id.as_str(), Some(2_000_000_000)),
        ] {
            let response: api::schema::ErrorResponse = serde_json::from_str(
                &request_pane_omp_bridge(&mut server, id, requested_pane, peer_pid),
            )
            .expect("denied OMP bridge response");
            assert_eq!(response.id, id);
            assert_eq!(response.error.code, "omp_bridge_discovery_denied");
            assert_eq!(
                response.error.message,
                "OMP bridge discovery is unavailable for this caller"
            );
        }

        server
            .app
            .terminal_runtimes
            .get(&source_terminal_id)
            .expect("source runtime")
            .test_set_child_pid(0);
        shutdown_test_runtimes(&mut server);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn headless_api_dispatch_uses_origin_context_for_cross_pane_guard() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("headless-input-guard");
        let source_pane = workspace.tabs[0].root_pane;
        let target_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let source_terminal_id = server.app.state.workspaces[0]
            .terminal_id(source_pane)
            .cloned()
            .expect("source terminal");
        let target_terminal_id = server.app.state.workspaces[0]
            .terminal_id(target_pane)
            .cloned()
            .expect("target terminal");
        server
            .app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state")
            .set_agent_name("source-agent".into());
        server
            .app
            .state
            .terminals
            .get_mut(&source_terminal_id)
            .expect("source state")
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        server
            .app
            .state
            .terminals
            .get_mut(&target_terminal_id)
            .expect("target state")
            .set_agent_name("target-agent".into());
        server
            .app
            .state
            .terminals
            .get_mut(&target_terminal_id)
            .expect("target state")
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );

        let (source_runtime, _source_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        source_runtime.test_set_child_pid(std::process::id());
        server
            .app
            .terminal_runtimes
            .insert(source_terminal_id.clone(), source_runtime);
        let (target_runtime, mut target_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        server
            .app
            .terminal_runtimes
            .insert(target_terminal_id, target_runtime);

        let workspace_id = server.app.state.workspaces[0].id.clone();
        let pane_number = server.app.state.workspaces[0]
            .public_pane_number(target_pane)
            .expect("target pane number");
        let target_pane_id =
            crate::workspace::public_pane_id_for_number(&workspace_id, pane_number);
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "headless-cross-pane".into(),
                method: api::schema::Method::PaneSendText(api::schema::PaneSendTextParams {
                    pane_id: target_pane_id,
                    text: "blocked".into(),
                    allow_cross_pane: false,
                }),
            },
            context: api::ApiRequestContext {
                local_peer_pid: Some(std::process::id()),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        let response: api::schema::ErrorResponse =
            serde_json::from_str(&response_rx.recv().expect("headless response")).unwrap();
        assert_eq!(response.error.code, "cross_pane_input_denied");
        assert!(target_rx.try_recv().is_err());
        server
            .app
            .terminal_runtimes
            .get(&source_terminal_id)
            .expect("source runtime")
            .test_set_child_pid(0);
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn headless_api_reads_latest_title_without_spinner_event_flooding() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.sidebar_agents.rows = vec![vec![
            crate::config::AgentSidebarToken::TerminalTitleStripped,
        ]];
        let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = server.app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .detected_agent = Some(crate::detect::Agent::Claude);
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);
        server.app.render_dirty.request_terminal_title(pane_id);

        let first = headless_pane_list(&mut server).pop().unwrap();
        assert_eq!(first.terminal_title.as_deref(), Some("⠋ task"));
        assert_eq!(first.terminal_title_stripped.as_deref(), Some("task"));
        assert_eq!(pane_updated_events(&event_hub), 1);
        let (buffer, _) = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            Rect::new(0, 0, 100, 30),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("task"), "rendered frame: {rendered:?}");

        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b]2;\xe2\xa0\x99 task\x1b\\");
        server.app.render_dirty.request_terminal_title(pane_id);
        let second = headless_pane_list(&mut server).pop().unwrap();
        assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
        assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
        assert_eq!(pane_updated_events(&event_hub), 1);
    }

    fn headless_pane_list(server: &mut HeadlessServer) -> Vec<api::schema::PaneInfo> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "list-titles".into(),
                method: api::schema::Method::PaneList(api::schema::PaneListParams::default()),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });
        let response: api::schema::SuccessResponse =
            serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        let api::schema::ResponseResult::PaneList { panes } = response.result else {
            panic!("expected pane list");
        };
        panes
    }

    #[tokio::test]
    async fn api_pane_focus_reconciles_private_omp_guest_route() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("private-omp-focus");
        let old_pane = workspace.tabs[0].root_pane;
        workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].layout.focus_pane(old_pane);
        let old_route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        let new_route = crate::workspace::public_pane_id_for_number(&workspace.id, 2);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.ensure_test_terminals();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));

        let mut host_receivers = Vec::new();
        let mut host_peers = Vec::new();
        let mut host_socket = |host_id| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
            let peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
                .expect("connect OMP host peer");
            host_peers.push(peer);
            let (socket, _) = listener.accept().expect("accept OMP host peer");
            let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(1);
            host_receivers.push(outbound_rx);
            ServerEvent::OmpHostStarted {
                pane_id: if host_id == 1 {
                    old_route.clone()
                } else {
                    new_route.clone()
                },
                omp_session_id: format!("session-{host_id}"),
                route_generation: 1,
                host_id,
                outbound,
                socket,
                admission: test_host_admission_sender(),
            }
        };
        assert!(server.handle_server_event(host_socket(1)));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("focused route attaches private guest")
                .route()
                .pane_id,
            old_route
        );
        assert!(!server.handle_server_event(host_socket(2)));
        headless_pane_list(&mut server);
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("non-focus API leaves private guest attached")
                .route()
                .pane_id,
            old_route
        );

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "focus-private-omp".into(),
                    method: api::schema::Method::PaneFocus(api::schema::PaneTarget {
                        pane_id: new_route.clone(),
                    }),
                },
                context: api::ApiRequestContext::default(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            })
        );

        let response: api::schema::SuccessResponse =
            serde_json::from_str(&response_rx.recv().expect("focus response")).unwrap();
        assert!(matches!(
            response.result,
            api::schema::ResponseResult::PaneInfo { .. }
        ));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("API focus swaps private guest immediately")
                .route()
                .pane_id,
            new_route
        );
    }
    #[tokio::test]
    async fn private_guest_reconciliation_uses_each_app_navigation_projection() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("private-omp-projections");
        let foreground_pane = workspace.tabs[0].root_pane;
        let background_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let foreground_route = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace.public_pane_number(foreground_pane).unwrap(),
        );
        let background_route = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace.public_pane_number(background_pane).unwrap(),
        );
        workspace.tabs[0].layout.focus_pane(foreground_pane);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].tabs[0]
            .layout
            .focus_pane(background_pane);
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);
        server.app.state.ensure_test_terminals();

        let mut foreground = test_identity_client(Some("Ada"), None);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(1, foreground);
        let mut background = test_identity_client(Some("Bea"), None);
        background.navigation = Some(background_navigation);
        server.clients.insert(2, background);
        server.foreground_client_id = Some(1);

        let mut host_receivers = Vec::new();
        let mut host_started = |pane_id: String, host_id| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
            let _peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
                .expect("connect OMP host peer");
            let (socket, _) = listener.accept().expect("accept OMP host peer");
            let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(1);
            host_receivers.push(outbound_rx);
            ServerEvent::OmpHostStarted {
                pane_id,
                omp_session_id: format!("session-{host_id}"),
                route_generation: 1,
                host_id,
                outbound,
                socket,
                admission: test_host_admission_sender(),
            }
        };
        assert!(server.handle_server_event(host_started(foreground_route.clone(), 1)));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("foreground App attaches its focused route")
                .route()
                .pane_id,
            foreground_route
        );
        assert!(server.clients[&2].private_omp_guest.is_none());
        assert!(server.handle_server_event(host_started(background_route.clone(), 2)));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("foreground App keeps its focused route")
                .route()
                .pane_id,
            foreground_route
        );
        assert_eq!(
            server.clients[&2]
                .private_omp_guest
                .as_ref()
                .expect("background App attaches its projected route")
                .route()
                .pane_id,
            background_route
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn private_omp_side_effects_require_a_promoted_active_owner_surface() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-effects");
        let pane_id = workspace.tabs[0].root_pane;
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Terminal;
        server.app.state.ensure_test_terminals();

        let (owner_writer, owner_control_rx, _owner_render_rx) = test_client_writer();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), Some(owner_writer)));
        let (foreground_writer, foreground_control_rx, _foreground_render_rx) =
            test_client_writer();
        let mut foreground = test_app_client(Some(true), 2);
        foreground.writer = Some(foreground_writer);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);

        let (_host, _host_messages) = start_test_omp_host(&mut server, route, "session", 1);
        let private_pane_id = server.clients[&1]
            .private_omp_guest
            .as_ref()
            .expect("private OMP guest")
            .runtime_pane_id();
        assert!(server.private_omp_pending_routes.contains_key(&1));

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
                pane_id: private_pane_id,
                count: 1,
            })
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneClipboardWrite {
                pane_id: private_pane_id,
                content: b"warming".to_vec(),
            })
        );
        assert!(owner_control_rx.try_recv().is_err());
        assert!(foreground_control_rx.try_recv().is_err());

        server.clients[&1]
            .private_omp_guest
            .as_ref()
            .expect("private OMP guest")
            .test_set_bridge_ready();
        assert!(server.drain_private_omp_guest_records());
        assert!(!server.private_omp_pending_routes.contains_key(&1));

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
                pane_id: private_pane_id,
                count: 2,
            })
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneClipboardWrite {
                pane_id: private_pane_id,
                content: b"promoted".to_vec(),
            })
        );
        assert!(matches!(
            read_server_message(
                owner_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("owner bell")
            ),
            ServerMessage::TerminalBell { count: 2 }
        ));
        assert!(matches!(
            read_server_message(
                owner_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("owner clipboard")
            ),
            ServerMessage::Clipboard { data } if data == "cHJvbW90ZWQ="
        ));
        assert!(foreground_control_rx.try_recv().is_err());

        server.clients.get_mut(&1).unwrap().private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(pane_id),
                b"hidden",
            ),
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
                pane_id: private_pane_id,
                count: 3,
            })
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneClipboardWrite {
                pane_id: private_pane_id,
                content: b"hidden".to_vec(),
            })
        );
        assert!(owner_control_rx.try_recv().is_err());
        assert!(foreground_control_rx.try_recv().is_err());
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn native_surface_activation_uses_client_projection_and_hides_for_popup() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("native-surface-projection");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Terminal;
        let terminal_navigation = ClientNavigationState::capture(&server.app.state);

        server.app.state.findr = Some(crate::app::state::FindrState::new(pane_id));
        server.app.state.mode = app::Mode::Findr;
        let findr_navigation = ClientNavigationState::capture(&server.app.state);

        let mut terminal_client = test_identity_client(Some("Ada"), None);
        terminal_client.navigation = Some(terminal_navigation);
        server.clients.insert(1, terminal_client);
        let mut findr_client = test_identity_client(Some("Bea"), None);
        findr_client.navigation = Some(findr_navigation);
        server.clients.insert(2, findr_client);

        assert!(server.client_omp_surface_active(1));
        server.app.state.mode = app::Mode::Terminal;
        assert!(!server.client_omp_surface_active(2));

        server.app.state.popup_pane = Some(crate::app::state::PopupPaneState {
            pane_id: crate::layout::PaneId::alloc(),
            terminal_id: crate::terminal::TerminalId::alloc(),
            width: None,
            height: None,
        });
        assert!(!server.client_omp_surface_active(1));
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn native_surface_activation_retries_after_control_queue_drains() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("native-control-backpressure");
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        let route_key = OmpRouteKey {
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Terminal;

        let writer = crate::server::client_transport::ClientWriter::test_backpressured();
        let mut client = test_identity_client(Some("Ada"), Some(writer.clone()));
        client.renderer_binding_token = Some("binding".into());
        client.omp_renderer_capabilities.client_local_native = true;
        server.clients.insert(1, client);
        server.clients.insert(
            2,
            ClientConnection::new_with_mode(
                ClientConnectionMode::OmpPane,
                None,
                None,
                Some("profile".into()),
                Some("binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                false,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server
            .private_omp_failed_routes
            .insert(1, route_key.clone());

        let (_host, _host_messages) = start_test_omp_host(&mut server, route.clone(), "session", 1);
        let renderer_launch_id = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .launch_id;
        while writer.test_pop_control().is_some() {}
        assert!(server.handle_server_event(ServerEvent::OmpPaneAttach {
            client_id: 2,
            pane_id: route,
            omp_session_id: "session".into(),
            route_generation: 1,
            target_app_client_id: Some(1),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_launch_id: Some(renderer_launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        }));
        while writer.test_pop_control().is_some() {}

        server
            .clients
            .get_mut(&1)
            .expect("app client")
            .graphics_cache
            .test_mark_non_empty();
        let direct_key = (
            server.app.state.workspaces[0].tabs[0].root_pane,
            "native-activation-direct".into(),
        );
        let direct_image_id = server
            .app
            .pane_graphics
            .reserve_image_id(&direct_key)
            .expect("direct image id");
        let direct_transfer_id = 41;
        let (direct_respond_to, direct_response_rx) = std::sync::mpsc::channel();
        let mut direct_slot = crate::app::pane_graphics::Slot::test(direct_image_id, None);
        direct_slot.direct_gate = Some(crate::app::pane_graphics::DirectGate {
            transfer_id: direct_transfer_id,
            client_id: 1,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            written: true,
            success_response: "ack".into(),
            respond_to: direct_respond_to,
        });
        server
            .app
            .pane_graphics
            .slots
            .insert(direct_key.clone(), direct_slot);
        writer.test_fill_control(vec![b'x']);
        assert!(server.handle_server_event(ServerEvent::OmpRendererReady {
            client_id: 1,
            launch_id: renderer_launch_id,
        }));
        let target = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("pending native target");
        assert!(target.ready, "renderer readiness must survive backpressure");
        assert!(!target.surface_active, "surface waits for host cleanup");
        assert!(!server.clients[&1].graphics_cache.is_empty());
        assert!(server.app.pane_graphics.slots.contains_key(&direct_key));
        assert!(matches!(
            direct_response_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        let blocked = writer.test_control_records();
        assert_eq!(blocked.len(), 64);
        assert!(blocked.iter().all(|message| message == b"x"));
        assert_eq!(writer.test_pop_control(), Some(vec![b'x']));
        assert!(
            server.handle_server_event(ServerEvent::ClientWriterControlDrained { client_id: 1 })
        );

        assert!(
            server.clients.contains_key(&1),
            "one free control slot must not disconnect the App"
        );
        assert!(server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .is_some_and(|target| target.surface_active));
        assert!(server.clients[&1].graphics_cache.is_empty());
        assert!(!server.app.pane_graphics.slots.contains_key(&direct_key));
        assert!(matches!(
            direct_response_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));

        let queued = writer.test_control_records();
        assert_eq!(queued.len(), 64);
        assert!(queued[..63].iter().all(|message| message == b"x"));
        let combined = queued.last().expect("combined activation record");
        let mut cursor = std::io::Cursor::new(combined.as_slice());
        match protocol::read_message::<_, ServerMessage>(&mut cursor, MAX_FRAME_SIZE)
            .expect("graphics cleanup frame")
        {
            ServerMessage::Graphics { bytes } => {
                let cleanup = String::from_utf8_lossy(&bytes);
                assert!(
                    cleanup.contains(&format!("i={direct_image_id}")),
                    "{cleanup:?}"
                );
            }
            other => panic!("expected graphics cleanup, got {other:?}"),
        }
        assert!(matches!(
            protocol::read_message::<_, ServerMessage>(&mut cursor, MAX_FRAME_SIZE)
                .expect("direct graphics retirement frame"),
            ServerMessage::GraphicsTransmissionRetired {
                transfer_id,
                image_id,
            } if transfer_id == direct_transfer_id && image_id == direct_image_id
        ));
        assert!(matches!(
            protocol::read_message::<_, ServerMessage>(&mut cursor, MAX_FRAME_SIZE)
                .expect("active renderer target frame"),
            ServerMessage::OmpRendererTarget {
                launch_id,
                surface_active: true,
                ..
            } if launch_id == renderer_launch_id
        ));
        assert_eq!(cursor.position() as usize, combined.len());
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn native_renderer_attach_uses_the_bound_background_app_projection() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("native-background-projection");
        let foreground_pane = workspace.tabs[0].root_pane;
        let background_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let background_route = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace.public_pane_number(background_pane).unwrap(),
        );
        workspace.tabs[0].layout.focus_pane(foreground_pane);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].tabs[0]
            .layout
            .focus_pane(background_pane);
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);
        server.app.state.ensure_test_terminals();

        let mut foreground = test_identity_client(Some("Ada"), None);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(1, foreground);
        let (background_writer, background_control_rx, _background_render_rx) =
            test_client_writer();
        let mut background = test_identity_client(Some("Bea"), Some(background_writer));
        background.navigation = Some(background_navigation);
        background.renderer_binding_token = Some("background-binding".into());
        background.omp_renderer_capabilities.client_local_native = true;
        server.clients.insert(2, background);
        server.clients.insert(
            3,
            ClientConnection::new_with_mode(
                ClientConnectionMode::OmpPane,
                None,
                None,
                Some("background-profile".into()),
                Some("background-binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                3,
                RenderEncoding::SemanticFrame,
                false,
                None,
            ),
        );
        server.foreground_client_id = Some(1);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
        let _peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
            .expect("connect OMP host peer");
        let (socket, _) = listener.accept().expect("accept OMP host peer");
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(8);
        assert!(server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id: background_route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            host_id: 1,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        }));
        assert!(server.clients[&2].private_omp_guest.is_some());
        let renderer_launch_id = server.clients[&2]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .launch_id;
        let renderer_route = server.clients[&2]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .route
            .clone()
            .expect("server offers an exact native route");

        assert!(server.handle_server_event(ServerEvent::OmpPaneAttach {
            client_id: 3,
            pane_id: background_route,
            omp_session_id: "session".into(),
            route_generation: 1,
            target_app_client_id: Some(2),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_launch_id: Some(renderer_launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        }));
        assert!(server.omp_service.app_has_native_renderer(2));
        while background_control_rx
            .recv_timeout(Duration::from_millis(20))
            .is_ok()
        {}
        assert!(server.clients[&2].private_omp_guest.is_some());
        assert!(
            !server.clients[&2]
                .omp_renderer_target
                .as_ref()
                .expect("native target")
                .ready
        );
        server.clients[&2]
            .private_omp_guest
            .as_ref()
            .expect("private guest")
            .test_set_bridge_ready();
        assert!(server.drain_private_omp_guest_records());
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let canonical = server.begin_client_navigation_scope(2).unwrap();
        server.compute_client_navigation_view(2);
        let inner = server
            .private_omp_pane_info(2)
            .expect("ready private renderer pane")
            .inner_rect;
        server.finish_client_navigation_scope(2, canonical);
        let x = u32::from(inner.x) * 10 + 1;
        let y = u32::from(inner.y) * 20 + 1;
        let data = format!("\x1b[<35;{x};{y}M").into_bytes();
        let client = server.clients.get_mut(&2).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.host_sgr_pixels_active = Some(true);
        assert!(server.handle_server_event(ServerEvent::ClientInputPixels {
            client_id: 2,
            data: data.clone(),
            geometry,
        }));
        assert_eq!(server.foreground_client_id, Some(1));
        assert!(!server.handle_server_event(ServerEvent::OmpRendererReady {
            client_id: 2,
            launch_id: renderer_launch_id + 1,
        }));
        assert!(server.clients[&2].private_omp_guest.is_some());
        server.app.state.mode = crate::app::Mode::Prefix;
        server
            .clients
            .get_mut(&2)
            .and_then(|client| client.navigation.as_mut())
            .expect("background projection")
            .non_findr_mode = Some(crate::app::Mode::Prefix);
        assert!(server.handle_server_event(ServerEvent::OmpRendererReady {
            client_id: 2,
            launch_id: renderer_launch_id,
        }));
        assert!(server.clients[&2].private_omp_guest.is_none());
        assert!(
            server.clients[&2]
                .omp_renderer_target
                .as_ref()
                .expect("native target")
                .ready
        );
        match read_server_message(
            background_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("inactive ready confirms the exact renderer target"),
        ) {
            ServerMessage::OmpRendererTarget {
                launch_id,
                route: Some(route),
                bound: true,
                surface_active: false,
                ..
            } => {
                assert_eq!(launch_id, renderer_launch_id);
                assert_eq!(route, renderer_route);
            }
            message => panic!("expected inactive renderer confirmation, got {message:?}"),
        }
        let key = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        ));
        let (remaining, consumed) = server.partition_native_omp_input(2, vec![key]);
        assert!(!consumed);
        assert!(matches!(
            remaining.as_slice(),
            [crate::raw_input::RawInputEvent::Key(_)]
        ));
        assert!(server.handle_server_event(ServerEvent::ClientInputPixels {
            client_id: 2,
            data,
            geometry,
        }));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(
            server.clients[&2]
                .committed_identity()
                .map(|identity| identity.name.as_str()),
            Some("Bea")
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn background_native_omp_input_preserves_foreground_projection() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("native-background-input");
        let foreground_pane = workspace.tabs[0].root_pane;
        let background_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let foreground_terminal_id = workspace
            .terminal_id(foreground_pane)
            .expect("foreground terminal")
            .clone();
        let background_route = crate::workspace::public_pane_id_for_number(
            &workspace.id,
            workspace.public_pane_number(background_pane).unwrap(),
        );
        workspace.tabs[0].layout.focus_pane(foreground_pane);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Terminal;
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].tabs[0]
            .layout
            .focus_pane(background_pane);
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);
        server.app.state.ensure_test_terminals();
        let (runtime, mut host_input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"foreground host",
                8,
            );
        server
            .app
            .terminal_runtimes
            .insert(foreground_terminal_id, runtime);

        let mut foreground = test_identity_client(Some("Ada"), None);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(1, foreground);
        let mut background = test_identity_client(Some("Bea"), None);
        background.navigation = Some(background_navigation);
        background.renderer_binding_token = Some("background-binding".into());
        background.omp_renderer_capabilities.client_local_native = true;
        server.clients.insert(2, background);
        server.clients.insert(
            3,
            ClientConnection::new_with_mode(
                ClientConnectionMode::OmpPane,
                None,
                None,
                Some("background-profile".into()),
                Some("background-binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                3,
                RenderEncoding::SemanticFrame,
                false,
                None,
            ),
        );
        server.foreground_client_id = Some(1);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
        let _peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
            .expect("connect OMP host peer");
        let (socket, _) = listener.accept().expect("accept OMP host peer");
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(8);
        assert!(server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id: background_route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            host_id: 1,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        }));
        let renderer_launch_id = server.clients[&2]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .launch_id;
        assert!(server.handle_server_event(ServerEvent::OmpPaneAttach {
            client_id: 3,
            pane_id: background_route,
            omp_session_id: "session".into(),
            route_generation: 1,
            target_app_client_id: Some(2),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_launch_id: Some(renderer_launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        }));
        assert!(server.omp_service.app_has_native_renderer(2));

        let key = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        ));
        assert!(server.handle_client_input_events(2, vec![key]));
        assert!(host_input.try_recv().is_err());
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.client_focused_pane(1), Some((0, foreground_pane)));
        assert_eq!(server.client_focused_pane(2), Some((0, background_pane)));
        assert_eq!(
            server.app.state.focused_terminal_pane_id(0),
            Some(foreground_pane)
        );
        assert_eq!(server.app.state.mode, app::Mode::Terminal);
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn pane_local_omp_keeps_host_runtime_chrome_input_and_scrollback() {
        let mut server = test_headless_server();
        server.independent_omp_renderers_enabled = false;
        let mut workspace = crate::workspace::Workspace::test_new("pane-local-omp");
        let omp_pane = workspace.tabs[0].root_pane;
        workspace.test_split(ratatui::layout::Direction::Horizontal);
        let other_pane = workspace.tabs[0]
            .layout
            .pane_ids()
            .into_iter()
            .find(|pane_id| *pane_id != omp_pane)
            .expect("split pane");
        workspace.tabs[0].layout.focus_pane(omp_pane);
        let terminal_id = workspace.terminal_id(omp_pane).unwrap().clone();
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        let mut output = Vec::new();
        for line in 0..80 {
            output.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        output.extend_from_slice(b"LIVE OMP\r\n");
        let (runtime, mut host_input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 4096, &output, 8,
            );
        runtime.test_set_child_pid(4242);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);
        let (writer, control_rx, render_rx) = test_client_writer();
        let mut client = test_identity_client(Some("Ada"), Some(writer));
        client.omp_renderer_capabilities.client_local_native = true;
        server.clients.insert(1, client);
        server.foreground_client_id = Some(1);
        let (_host, _host_messages) = start_test_omp_host(&mut server, route, "session", 1);

        assert!(server.clients[&1].omp_renderer_target.is_none());
        assert!(server.clients[&1].private_omp_guest.is_none());
        assert!(server.private_omp_pending_routes.is_empty());
        assert!(control_rx.try_recv().is_err());
        server.render_and_stream();
        let frame = read_server_frame(render_rx.recv_timeout(Duration::from_millis(100)).unwrap());
        let text = frame_text(&frame);
        assert!(text.contains("LIVE OMP"));
        assert!(!text.contains("native renderer"));

        let inner = server
            .app
            .state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == omp_pane)
            .expect("OMP pane info")
            .inner_rect;
        assert!(server.handle_client_input_events(
            1,
            vec![crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: inner.x,
                    row: inner.y,
                    modifiers: KeyModifiers::empty(),
                },
            )],
        ));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .scroll_metrics()
                .unwrap()
                .offset_from_bottom,
            server.app.state.mouse_scroll_lines
        );
        assert!(host_input.try_recv().is_err());

        let ordinary_key = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        ));
        assert!(server.handle_client_input_events(1, vec![ordinary_key]));
        assert_eq!(host_input.try_recv().unwrap().as_ref(), b"x");
        let prefix = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            server.app.state.prefix_code,
            server.app.state.prefix_mods,
        ));
        assert!(server.handle_client_input_events(1, vec![prefix]));
        assert_eq!(server.app.state.mode, crate::app::Mode::Prefix);
        server.render_and_stream();
        let prefix_frame =
            read_server_frame(render_rx.recv_timeout(Duration::from_millis(100)).unwrap());
        assert!(frame_text(&prefix_frame).contains("LIVE OMP"));

        server.app.state.workspaces[0].tabs[0]
            .layout
            .focus_pane(other_pane);
        assert!(!server.reconcile_omp_renderers());
        server.app.state.workspaces[0].tabs[0]
            .layout
            .focus_pane(omp_pane);
        assert!(!server.reconcile_omp_renderers());
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .child_pid(),
            Some(4242)
        );
        assert!(server.clients[&1].private_omp_guest.is_none());
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn native_bound_app_masks_host_pane_and_consumes_terminal_input() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("native-omp");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        let host_uri = "https://example.com/host";
        let (runtime, mut host_input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                format!("HOST PTY \x1b]8;;{host_uri}\x1b\\HOST LINK\x1b]8;;\x1b\\").as_bytes(),
                8,
            );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(terminal_id, runtime);
        let (app_writer, app_control, app_render) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::App,
                None,
                Some("Ada".into()),
                Some("profile".into()),
                Some("binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                false,
                Some(app_writer),
            ),
        );
        server
            .clients
            .get_mut(&1)
            .unwrap()
            .omp_renderer_capabilities
            .client_local_native = true;
        server.clients.insert(
            2,
            ClientConnection::new_with_mode(
                ClientConnectionMode::OmpPane,
                None,
                None,
                Some("profile".into()),
                Some("binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                false,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: Rect::new(0, 0, 80, 24),
            inner_rect: Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        }];

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _host_peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(8);
        assert!(server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            host_id: 1,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        }));
        let renderer_launch_id = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .launch_id;
        assert_eq!(server.client_focused_pane(1), Some((0, pane_id)));
        assert!(server.clients[&1].private_omp_guest.is_some());
        assert!(server.handle_server_event(ServerEvent::OmpPaneAttach {
            client_id: 2,
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            target_app_client_id: Some(1),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true
            },
            renderer_launch_id: Some(renderer_launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        }));
        assert!(server.clients[&1].private_omp_guest.is_some());
        while app_control.try_recv().is_ok() {}
        server
            .clients
            .get_mut(&1)
            .expect("app client")
            .graphics_cache
            .test_mark_non_empty();
        assert!(server.handle_server_event(ServerEvent::OmpRendererReady {
            client_id: 1,
            launch_id: renderer_launch_id,
        }));
        assert!(server.clients[&1].private_omp_guest.is_none());
        assert!(server.clients[&1].graphics_cache.is_empty());
        server
            .clients
            .get_mut(&1)
            .expect("app client")
            .graphics_cache
            .test_mark_non_empty();

        server.render_and_stream();
        let frame = read_server_frame(app_render.recv_timeout(Duration::from_millis(100)).unwrap());
        let text = frame_text(&frame);
        assert!(text.contains("OMP is open in its native renderer"));
        assert!(!text.contains("HOST PTY"));
        assert!(!frame.hyperlinks.iter().any(|link| link == host_uri));
        assert_eq!(frame.cursor, None);
        assert!(frame.graphics.is_empty());

        let ordinary_key = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        ));
        assert!(server.handle_client_input_events(1, vec![ordinary_key]));
        assert!(host_input.try_recv().is_err());
        assert!(server.handle_client_input_events(
            1,
            vec![
                crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new("text")),
                crate::raw_input::RawInputEvent::Paste("paste".into()),
                crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::empty(),
                }),
            ]
        ));
        assert!(host_input.try_recv().is_err());
        let prefix = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            server.app.state.prefix_code,
            server.app.state.prefix_mods,
        ));
        assert!(server.handle_client_input_events(1, vec![prefix]));
        assert_eq!(server.app.state.mode, crate::app::Mode::Prefix);
        shutdown_test_runtimes(&mut server);
    }
    #[tokio::test]
    async fn native_renderer_detach_waits_for_private_bridge_before_releasing_target() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("native-detach");
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.ensure_test_terminals();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));
        server.clients.insert(
            2,
            ClientConnection::new_with_mode(
                ClientConnectionMode::OmpPane,
                None,
                None,
                Some("profile".into()),
                Some("binding".into()),
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                false,
                None,
            ),
        );
        server.clients.get_mut(&1).unwrap().renderer_binding_token = Some("binding".into());
        server
            .clients
            .get_mut(&1)
            .unwrap()
            .omp_renderer_capabilities
            .client_local_native = true;
        server.private_omp_test_executable = None;
        server.private_omp_executable = Some(crate::update::OmpExecutable::Explicit(
            std::env::current_exe().expect("locate the headless test executable"),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _host_peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(8);
        assert!(server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            host_id: 1,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        }));
        assert!(server.clients[&1].private_omp_guest.is_some());
        assert!(server.private_omp_executable.is_some());
        assert!(server.private_omp_resolving.is_none());
        let renderer_launch_id = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("server offers the focused native route")
            .launch_id;
        assert!(server.handle_server_event(ServerEvent::OmpPaneAttach {
            client_id: 2,
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            target_app_client_id: Some(1),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true
            },
            renderer_launch_id: Some(renderer_launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        }));
        assert!(server.omp_service.app_has_native_renderer(1));
        assert!(server.handle_server_event(ServerEvent::OmpRendererReady {
            client_id: 1,
            launch_id: renderer_launch_id,
        }));
        assert!(server.clients[&1].private_omp_guest.is_none());
        assert!(server.private_omp_executable.is_some());
        assert!(server.private_omp_resolving.is_none());

        assert!(server.handle_server_event(ServerEvent::OmpPaneDetach {
            client_id: 2,
            pane_id: route.clone(),
            omp_session_id: "session".into(),
            route_generation: 1,
            attachment_epoch: 1,
        }));
        let private_guest = server.clients[&1]
            .private_omp_guest
            .as_ref()
            .expect("native detach restores the App private guest immediately");
        assert_eq!(private_guest.route().pane_id, route);
        let private_guest_pane = private_guest.runtime_pane_id();
        let private_route = private_guest.route().clone();
        assert!(server.private_omp_executable.is_some());
        assert!(server.private_omp_resolving.is_none());
        let native_target = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("native target stays active while the fallback bridge connects");
        assert!(native_target.bound);
        assert!(native_target.ready);
        assert!(native_target.surface_active);
        assert_eq!(
            server.private_omp_pending_routes.get(&1),
            Some(&private_route)
        );

        server.clients[&1]
            .private_omp_guest
            .as_ref()
            .expect("private guest")
            .test_set_bridge_ready();
        assert!(server.drain_private_omp_guest_records());
        let fallback_target = server.clients[&1]
            .omp_renderer_target
            .as_ref()
            .expect("renderer target after fallback readiness");
        assert!(!fallback_target.bound);
        assert!(!fallback_target.ready);
        assert!(!fallback_target.surface_active);
        assert!(!server.private_omp_pending_routes.contains_key(&1));

        assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 2 }));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("connection detach leaves the restored guest intact")
                .runtime_pane_id(),
            private_guest_pane
        );
        assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 2 }));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .expect("repeated connection detach does not spawn another guest")
                .runtime_pane_id(),
            private_guest_pane
        );
        shutdown_test_runtimes(&mut server);
    }

    #[cfg(unix)]
    fn live_omp_route(server: &mut HeadlessServer) -> std::net::TcpStream {
        let pane_id = "w1:p1".to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
            .expect("connect OMP host peer");
        let (socket, _) = listener.accept().expect("accept OMP host peer");
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(1);
        assert!(!server.handle_server_event(ServerEvent::OmpHostStarted {
            pane_id,
            omp_session_id: "session".into(),
            route_generation: 1,
            host_id: 1,
            outbound,
            socket,
            admission: test_host_admission_sender(),
        }));
        peer
    }

    #[cfg(unix)]
    #[test]
    fn live_handoff_rejects_live_omp_routes_without_side_effects() {
        let mut server = test_headless_server();
        let (writer, control_rx, _render_rx) = test_client_writer();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), Some(writer)));
        server.foreground_client_id = Some(1);
        let mut host = live_omp_route(&mut server);

        let error = server
            .perform_live_handoff(api::schema::ServerLiveHandoffParams::default())
            .expect_err("live OMP route must block handoff");
        assert_eq!(
            error.to_string(),
            "live handoff is unavailable while OMP host routes are live; restart Herdr normally"
        );
        assert!(!server.handoff_in_progress);
        assert!(server.clients.contains_key(&1));
        assert_eq!(server.foreground_client_id, Some(1));
        assert!(
            control_rx.try_recv().is_err(),
            "client must not be disconnected"
        );
        host.set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        assert!(
            matches!(host.read(&mut [0]), Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut)
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_handoff_authorization_allows_no_omp_routes() {
        let server = test_headless_server();
        assert!(server.authorize_live_handoff().is_ok());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn live_handoff_rejects_an_unready_ssh_pane() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("remote-starting");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .execution_target = crate::execution::ExecutionTarget::ssh("build.example").unwrap();
        let (runtime, _input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        runtime.set_remote_execution_ready_for_test(false);
        server.app.terminal_runtimes.insert(terminal_id, runtime);

        let error = server
            .authorize_live_handoff()
            .expect_err("unready SSH pane must block handoff");
        assert!(error
            .to_string()
            .contains("is still starting; wait for it to become ready and retry"));

        server
            .app
            .terminal_runtimes
            .values()
            .next()
            .unwrap()
            .set_remote_execution_ready_for_test(true);
        assert!(server.authorize_live_handoff().is_ok());
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn live_handoff_event_drain_stops_at_the_queued_snapshot() {
        let mut server = test_headless_server();
        assert_eq!(server.app.event_rx.len(), 0);
        for index in 0..crate::app::APP_EVENT_CHANNEL_CAPACITY {
            server
                .app
                .event_tx
                .try_send(AppEvent::UpdateReady {
                    version: format!("queued-{index}"),
                    install_command: "herdr update".into(),
                })
                .unwrap();
        }
        let event_tx = server.app.event_tx.clone();
        let producer = std::thread::spawn(move || {
            while event_tx.capacity() == 0 {
                std::thread::yield_now();
            }
            event_tx
                .blocking_send(AppEvent::UpdateReady {
                    version: "after-snapshot".into(),
                    install_command: "herdr update".into(),
                })
                .unwrap();
        });

        server.drain_internal_event_snapshot_with_forwarding();
        producer.join().unwrap();

        assert_eq!(server.app.event_rx.len(), 1);
        assert!(matches!(
            server.app.event_rx.try_recv(),
            Ok(AppEvent::UpdateReady { version, .. }) if version == "after-snapshot"
        ));
    }

    #[test]
    fn oversized_private_guest_frame_is_rejected_instead_of_dropped() {
        let payload = format!("\"{}\"", "x".repeat(crate::protocol::MAX_OMP_FRAME_PAYLOAD));
        let frame: Box<serde_json::value::RawValue> = serde_json::from_str(&payload).unwrap();
        assert!(matches!(
            HeadlessServer::encode_private_omp_guest_frame(&frame),
            Err(protocol::OmpFrameError::Oversized { .. })
        ));
    }

    #[tokio::test]
    async fn host_stop_reconciles_private_guest_to_started_replacement() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("private-omp-replacement");
        let route = crate::workspace::public_pane_id_for_number(&workspace.id, 1);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.ensure_test_terminals();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));

        let mut host_receivers = Vec::new();
        let mut host_started = |host_id, session: &str| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind OMP host listener");
            let _peer = std::net::TcpStream::connect(listener.local_addr().unwrap())
                .expect("connect OMP host peer");
            let (socket, _) = listener.accept().expect("accept OMP host peer");
            let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(8);
            host_receivers.push(outbound_rx);
            ServerEvent::OmpHostStarted {
                pane_id: route.clone(),
                omp_session_id: session.into(),
                route_generation: 1,
                host_id,
                outbound,
                socket,
                admission: test_host_admission_sender(),
            }
        };

        assert!(server.handle_server_event(host_started(1, "old")));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .unwrap()
                .route()
                .omp_session_id,
            "old"
        );
        assert!(!server.handle_server_event(host_started(2, "replacement")));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .unwrap()
                .route()
                .omp_session_id,
            "old",
            "deterministic ordering keeps the selected route while both are live"
        );

        assert!(server.handle_server_event(ServerEvent::OmpHostStopped {
            pane_id: route,
            omp_session_id: "old".into(),
            route_generation: 1,
            host_id: 1,
        }));
        assert_eq!(
            server.clients[&1]
                .private_omp_guest
                .as_ref()
                .unwrap()
                .route()
                .omp_session_id,
            "replacement",
            "stopping the selected live route immediately attaches its replacement"
        );
    }
    fn pane_updated_events(event_hub: &api::EventHub) -> usize {
        event_hub
            .events_after(0)
            .iter()
            .filter(|(_, event)| event.event == api::schema::EventKind::PaneUpdated)
            .count()
    }

    #[test]
    fn server_stop_interrupts_server_event_backlog() {
        let mut server = test_headless_server();
        for client_id in 1..=64 {
            server
                .server_event_tx
                .try_send(ServerEvent::ClientDisconnected { client_id })
                .unwrap();
        }

        server.should_quit.store(true, Ordering::Release);

        assert!(!server.drain_server_events());
        assert!(server.server_event_rx.try_recv().is_ok());
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn headless_api_request_drains_all_pending_internal_events_before_reading_state() {
        let mut server = test_headless_server();
        for i in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
            server
                .app
                .event_tx
                .try_send(AppEvent::UpdateReady {
                    version: format!("4.0.{i}"),
                    install_command: "herdr install".into(),
                })
                .unwrap();
        }

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "headless_stop_after_events".into(),
                    method: api::schema::Method::ServerStop(api::schema::EmptyParams::default()),
                },
                context: api::ApiRequestContext::default(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            })
        );
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["result"]["type"], "ok");
        let expected_version = format!("4.0.{}", crate::app::APP_EVENT_DRAIN_LIMIT);
        assert_eq!(
            server.app.state.update_available.as_deref(),
            Some(expected_version.as_str())
        );
        assert!(server.app.event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn headless_deferred_workspace_create_uses_runtime_events() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());

        server.app.state.request_new_workspace = true;

        assert!(server.handle_deferred_requests_headless());
        assert!(!server.app.state.request_new_workspace);
        assert_eq!(
            event_hub
                .events_after(0)
                .into_iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            vec![
                api::schema::EventKind::WorkspaceCreated,
                api::schema::EventKind::TabCreated,
                api::schema::EventKind::PaneCreated,
                api::schema::EventKind::LayoutUpdated,
            ]
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn headless_deferred_named_tab_create_uses_runtime_events() {
        let event_hub = api::EventHub::default();
        let mut server = test_headless_server_with_event_hub(event_hub.clone());
        server
            .app
            .create_workspace_with_options(std::env::temp_dir(), true)
            .unwrap();
        let after_setup = event_hub.current_sequence();

        server.app.state.request_new_tab = true;
        server.app.state.requested_new_tab_name = Some("ops".into());

        assert!(server.handle_deferred_requests_headless());
        assert!(!server.app.state.request_new_tab);
        assert_eq!(server.app.state.requested_new_tab_name, None);
        let events = event_hub.events_after(after_setup);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            vec![
                api::schema::EventKind::TabCreated,
                api::schema::EventKind::PaneCreated,
                api::schema::EventKind::LayoutUpdated,
            ]
        );
        let tab_created = events
            .iter()
            .find_map(|(_, event)| match &event.data {
                api::schema::EventData::TabCreated { tab } => Some(tab),
                _ => None,
            })
            .expect("tab created event");
        assert_eq!(tab_created.label, "ops");
        shutdown_test_runtimes(&mut server);
    }

    fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;

        let (client_tx, control_rx, _render_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.promote_client_to_foreground(1);
        drain_window_titles(&control_rx);
        (server, control_rx)
    }

    /// The test client writer drains its queue on a background thread, so
    /// reading a pushed message needs a timeout rather than `try_recv`.
    fn next_window_title(
        control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    ) -> Option<Option<String>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let Ok(bytes) = control_rx.recv_timeout(remaining) else {
                return None;
            };
            if let ServerMessage::WindowTitle { title } = read_server_message(bytes) {
                return Some(title);
            }
        }
        None
    }

    fn drain_window_titles(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
        while control_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
    }

    fn no_window_title(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> bool {
        while let Ok(bytes) = control_rx.recv_timeout(Duration::from_millis(200)) {
            if let ServerMessage::WindowTitle { .. } = read_server_message(bytes) {
                return false;
            }
        }
        true
    }

    #[test]
    fn window_title_waits_for_a_foreground_client_to_exist() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
        server.app.state.active = Some(0);
        server.app.configure_window_title("{workspace}");

        // The server renders before the first client attaches. Nothing was
        // delivered, so nothing may be recorded as delivered either.
        server.sync_window_title();
        assert_eq!(server.sent_window_title, None);

        let (client_tx, control_rx, _render_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.promote_client_to_foreground(1);
        server.sync_window_title();

        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herd".to_string()))
        );
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn an_attaching_client_gets_the_title_even_when_it_has_not_changed() {
        let (mut server, first_control_rx) = window_title_test_server();
        server.app.configure_window_title("{workspace}");
        server.sync_window_title();
        assert_eq!(
            next_window_title(&first_control_rx),
            Some(Some("herd".to_string()))
        );

        // A newly foreground client must receive the current title even when
        // the title itself has not changed.
        let (client_tx, second_control_rx, _render_rx) = test_client_writer();
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_window_title();

        assert_eq!(
            next_window_title(&second_control_rx),
            Some(Some("herd".to_string()))
        );
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn configured_window_title_reaches_the_foreground_client_once_per_change() {
        let (mut server, control_rx) = window_title_test_server();
        server.app.configure_window_title("{workspace}/{tab}");

        server.sync_window_title();
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herd/1".to_string()))
        );

        // An unchanged title must not re-emit an OSC on every render.
        server.sync_window_title();
        assert!(no_window_title(&control_rx));

        server.app.state.workspaces[0].tabs[0].custom_name = Some("build".into());
        server.sync_window_title();
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herd/build".to_string()))
        );

        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn focused_terminal_title_syncs_without_requesting_a_sidebar_render() {
        let (mut server, control_rx) = window_title_test_server();
        server.app.configure_window_title("{terminal_title}");
        server.app.state.ensure_test_terminals();
        let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = server.app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes("\x1b]0;⠋ building\x07".as_bytes());
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);

        assert_eq!(
            server.sync_terminal_title_sources(&HashSet::from([pane_id])),
            (false, true)
        );
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("building".to_string()))
        );

        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .test_process_pty_bytes("\x1b]0;⠙ building\x07".as_bytes());
        assert_eq!(
            server.sync_terminal_title_sources(&HashSet::from([pane_id])),
            (false, true)
        );
        assert!(no_window_title(&control_rx));

        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn a_foreground_client_without_a_writer_does_not_cache_the_window_title() {
        let (mut server, _control_rx) = window_title_test_server();
        server.app.configure_window_title("{workspace}");

        // A detached client keeps its entry but loses its writer, so nothing
        // reaches a terminal even though the targeted send reports success.
        if let Some(client) = server.clients.get_mut(&1) {
            client.writer = None;
        }
        server.sync_window_title();
        assert!(server.sent_window_title.is_none());

        // Attaching again has to deliver the title rather than skip it as sent.
        let (client_tx, control_rx, _render_rx) = test_client_writer();
        if let Some(client) = server.clients.get_mut(&1) {
            client.writer = Some(client_tx);
        }
        server.sync_window_title();
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herd".to_string()))
        );

        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn empty_window_title_config_leaves_the_outer_title_alone() {
        let (mut server, control_rx) = window_title_test_server();
        server.app.configure_window_title("");

        server.sync_window_title();

        assert!(no_window_title(&control_rx));
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn api_window_title_wins_until_it_is_cleared() {
        let (mut server, control_rx) = window_title_test_server();
        server.app.configure_window_title("{workspace}");

        server.handle_client_window_title_api("set".into(), Some("herdr api".into()));
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herdr api".to_string()))
        );

        server.app.state.workspaces[0].custom_name = Some("ops".into());
        server.sync_window_title();
        assert!(no_window_title(&control_rx));

        // Clearing hands the title back to ui.window_title, not to "herdr".
        server.handle_client_window_title_api("clear".into(), None);
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("ops".to_string()))
        );

        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn clearing_the_api_title_falls_back_to_herdr_when_window_titles_are_disabled() {
        let (mut server, control_rx) = window_title_test_server();
        server.app.configure_window_title("");

        server.handle_client_window_title_api("set".into(), Some("herdr api".into()));
        assert_eq!(
            next_window_title(&control_rx),
            Some(Some("herdr api".to_string()))
        );

        server.handle_client_window_title_api("clear".into(), None);
        assert_eq!(next_window_title(&control_rx), Some(None));

        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn a_newly_promoted_client_gets_the_window_title_again() {
        let (mut server, first_control_rx) = window_title_test_server();
        server.app.configure_window_title("{workspace}");
        server.sync_window_title();
        assert_eq!(
            next_window_title(&first_control_rx),
            Some(Some("herd".to_string()))
        );

        // A second terminal starts on whatever its shell or ssh left behind.
        let (client_tx, second_control_rx, _render_rx) = test_client_writer();
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.promote_client_to_foreground(2);
        server.sync_window_title();

        assert_eq!(
            next_window_title(&second_control_rx),
            Some(Some("herd".to_string()))
        );
        shutdown_test_runtimes(&mut server);
    }

    fn test_client_writer() -> (
        ClientWriter,
        std::sync::mpsc::Receiver<Vec<u8>>,
        std::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let (control_tx, control_rx) = std::sync::mpsc::channel();
        let (render_tx, render_rx) = std::sync::mpsc::sync_channel(1);
        (
            ClientWriter::test_channel(control_tx, render_tx),
            control_rx,
            render_rx,
        )
    }

    fn notification_activation_test_server() -> (HeadlessServer, String, crate::layout::PaneId) {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("activation");
        let target_tab = workspace.test_add_tab(Some("target"));
        let target_pane = workspace.tabs[target_tab].root_pane;
        let workspace_id = workspace.id.clone();
        server.app.state.workspaces =
            vec![workspace, crate::workspace::Workspace::test_new("other")];
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = app::Mode::Navigator;
        server.app.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::Finished,
            title: "done".to_owned(),
            context: "context".to_owned(),
            position: None,
            target: None,
        });
        server.app.toast_deadline = Some(Instant::now());

        let (target_writer, _target_control, _target_render) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(target_writer),
            ),
        );
        let (foreground_writer, _foreground_control, _foreground_render) = test_client_writer();
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_writer),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        (server, workspace_id, target_pane)
    }
    fn notification_activation_event(
        activation: protocol::NotificationActivation,
    ) -> (ServerEvent, std::sync::mpsc::Receiver<bool>) {
        let (respond_to, response) = std::sync::mpsc::channel();
        (
            ServerEvent::NotificationActivated {
                activation,
                respond_to,
            },
            response,
        )
    }

    #[test]
    fn notification_activation_promotes_and_focuses_its_target() {
        let (mut server, workspace_id, target_pane) = notification_activation_test_server();

        let (event, response) = notification_activation_event(protocol::NotificationActivation {
            recipient_client_id: 1,
            workspace_id,
            pane_id: target_pane.raw(),
        });
        assert!(server.handle_server_event(event));
        assert!(response.recv().expect("processed activation result"));

        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (120, 40));
        assert_eq!(server.app.state.active, Some(0));
        assert_eq!(server.app.state.mode, app::Mode::Terminal);
        assert!(server.app.state.toast.is_none());
        assert!(server.app.toast_deadline.is_none());
        assert_eq!(
            server.app.state.workspaces[0].focused_pane_id(),
            Some(target_pane)
        );
    }

    #[test]
    fn stale_or_mismatched_notification_activation_is_ignored() {
        let (mut server, workspace_id, target_pane) = notification_activation_test_server();

        let (event, response) = notification_activation_event(protocol::NotificationActivation {
            recipient_client_id: 99,
            workspace_id: workspace_id.clone(),
            pane_id: target_pane.raw(),
        });
        assert!(!server.handle_server_event(event));
        assert!(!response.recv().expect("stale client rejection"));

        let (event, response) = notification_activation_event(protocol::NotificationActivation {
            recipient_client_id: 1,
            workspace_id,
            pane_id: crate::layout::PaneId::alloc().raw(),
        });
        assert!(!server.handle_server_event(event));
        assert!(!response.recv().expect("stale pane rejection"));

        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.effective_size, (80, 24));
        assert_eq!(server.app.state.active, Some(1));
        assert_eq!(server.app.state.mode, app::Mode::Navigator);
        assert!(server.app.state.toast.is_some());
    }
    #[test]
    fn live_handoff_rejects_notification_activation() {
        let (mut server, workspace_id, target_pane) = notification_activation_test_server();
        server.handoff_in_progress = true;
        let (event, response) = notification_activation_event(protocol::NotificationActivation {
            recipient_client_id: 1,
            workspace_id,
            pane_id: target_pane.raw(),
        });

        assert!(!server.handle_server_event(event));
        assert!(!response.recv().expect("handoff rejection"));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.app.state.mode, app::Mode::Navigator);
    }

    fn retained_test_server(
        initial_screen: &[u8],
    ) -> (
        HeadlessServer,
        std::sync::mpsc::Receiver<Vec<u8>>,
        crate::layout::PaneId,
    ) {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.focused_pane_id().expect("focused pane");
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        (server, client_rx, pane_id)
    }

    fn hidden_pty_visibility_test_server(
        client_sizes: &[(u16, u16)],
    ) -> (HeadlessServer, crate::layout::PaneId) {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let background_tab = workspace.test_add_tab(Some("background"));
        let background_pane = workspace.tabs[background_tab].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        for (index, &terminal_size) in client_sizes.iter().enumerate() {
            let client_id = index as u64 + 1;
            let (client_tx, _client_control_rx, _client_rx) = test_client_writer();
            server.clients.insert(
                client_id,
                ClientConnection::new(
                    terminal_size,
                    crate::kitty_graphics::HostCellSize::default(),
                    crate::terminal_theme::TerminalTheme::default(),
                    None,
                    client_id,
                    RenderEncoding::SemanticFrame,
                    Some(client_tx),
                ),
            );
        }

        (server, background_pane)
    }

    fn assert_frame_data_eq(actual: &FrameData, expected: &FrameData) {
        assert_eq!(
            (actual.width, actual.height),
            (expected.width, expected.height)
        );
        assert_eq!(actual.cursor, expected.cursor, "cursor mismatch");
        assert_eq!(actual.hyperlinks, expected.hyperlinks, "hyperlink mismatch");
        assert_eq!(actual.graphics, expected.graphics, "graphics mismatch");
        assert_eq!(
            actual.cells.len(),
            expected.cells.len(),
            "cell length mismatch"
        );
        for (idx, (actual_cell, expected_cell)) in
            actual.cells.iter().zip(expected.cells.iter()).enumerate()
        {
            if cells_equivalent_for_frame_compare(
                &actual.cells,
                &expected.cells,
                usize::from(actual.width),
                idx,
                actual_cell,
                expected_cell,
            ) {
                continue;
            }
            assert_eq!(
                actual_cell,
                expected_cell,
                "cell mismatch at index {idx} (x={}, y={})",
                idx % usize::from(actual.width),
                idx / usize::from(actual.width),
            );
        }
    }

    fn cells_equivalent_for_frame_compare(
        actual_cells: &[CellData],
        expected_cells: &[CellData],
        width: usize,
        idx: usize,
        actual: &CellData,
        expected: &CellData,
    ) -> bool {
        if actual == expected {
            return true;
        }
        if !cell_style_without_symbol_eq(actual, expected) {
            return false;
        }
        if !matches!(
            (actual.symbol.as_str(), expected.symbol.as_str()),
            ("", " ") | (" ", "")
        ) {
            return false;
        }
        covered_by_previous_wide_cell(actual_cells, width, idx)
            || covered_by_previous_wide_cell(expected_cells, width, idx)
    }

    fn cell_style_without_symbol_eq(a: &CellData, b: &CellData) -> bool {
        a.fg == b.fg
            && a.bg == b.bg
            && a.modifier == b.modifier
            && a.skip == b.skip
            && a.hyperlink == b.hyperlink
    }

    fn covered_by_previous_wide_cell(cells: &[CellData], width: usize, idx: usize) -> bool {
        if idx == 0 || idx.is_multiple_of(width) {
            return false;
        }
        frame_cell_display_width(&cells[idx - 1]) > 1
    }

    fn frame_cell_display_width(cell: &CellData) -> usize {
        if is_halfwidth_katakana_voiced_grapheme(&cell.symbol) {
            return 2;
        }
        cell.symbol.width()
    }

    fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
        let mut chars = symbol.chars();
        let Some(base) = chars.next() else {
            return false;
        };
        let Some(mark) = chars.next() else {
            return false;
        };
        chars.next().is_none()
            && ('\u{ff66}'..='\u{ff9d}').contains(&base)
            && matches!(mark, '\u{ff9e}' | '\u{ff9f}')
    }

    #[test]
    fn direct_graphics_requires_one_negotiated_app_client() {
        let mut server = test_headless_server();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 10,
            cell_height_px: 20,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: true,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_a,
        }));
        assert!(server.clients[&1].direct_graphics);
        assert!(server.clients[&1].pixel_mouse);
        assert!(server.direct_graphics_available());

        let (writer_b, _control_b, _render_b) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 10,
            cell_height_px: 20,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_b,
        }));
        assert!(!server.direct_graphics_available());
    }

    #[test]
    fn foreground_client_applies_client_keybindings() {
        let mut server = test_headless_server();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"
"#,
        )
        .unwrap();
        let local_keybindings = local_config.live_keybinds().unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_a,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('a')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+t"));

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('b')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+c"));
    }

    #[test]
    fn local_keybinding_client_hides_server_keybinding_warnings() {
        let mut server = test_headless_server();
        let diagnostics = vec![
            "unsafe direct keybinding: keys.close_pane = \"x\" would intercept typing".to_owned(),
            "theme warning".to_owned(),
        ];
        let (full, without_keybindings) = server_config_diagnostic_summaries(&diagnostics);
        server.server_config_diagnostic = full.clone();
        server.server_config_diagnostic_without_keybindings = without_keybindings.clone();
        server.app.state.config_diagnostic = full;
        let local_keybindings = crate::config::Config::default().live_keybinds().unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_a,
        }));
        assert_eq!(server.app.state.config_diagnostic, without_keybindings);

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.config_diagnostic,
            server.server_config_diagnostic
        );
    }

    #[test]
    fn local_keybinding_client_keeps_local_keybindings_after_settings_save() {
        let path = std::env::temp_dir().join(format!(
            "herdr-headless-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, "onboarding = false\n").unwrap();
        let _guard = crate::config::test_config_env_lock().lock();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let mut server = test_headless_server();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
        )
        .unwrap();
        let local_keybindings = local_config.live_keybinds().unwrap();
        let (writer, _control, _render) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_keybindings)),
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: Some("Test".into()),
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        server.app.state.mode = crate::app::Mode::Settings;
        server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
        server.app.state.settings.list.selected = 1;

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\r".to_vec(),
        }));

        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('a')
        );
        assert!(server
            .app
            .state
            .keybinds
            .new_workspace
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+n"));
        assert!(server.app.state.toast.is_none());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("delivery = \"herdr\""));

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_server_keybindings_apply_valid_subset_after_settings_save_without_caching_local_keybindings(
    ) {
        let path = std::env::temp_dir().join(format!(
            "herdr-headless-invalid-settings-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(
            &path,
            "onboarding = false\n[keys]\nnew_workspace = \"x\"\n[ui.toast]\ndelivery = \"off\"\n",
        )
        .unwrap();
        let _guard = crate::config::test_config_env_lock().lock();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

        let mut server = test_headless_server();
        let previous_server_config: crate::config::Config =
            toml::from_str("[keys]\nprefix = \"ctrl+c\"\nnew_workspace = \"prefix+m\"\n").unwrap();
        server.server_keybindings = previous_server_config.live_keybinds().unwrap();
        let local_config: crate::config::Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
        )
        .unwrap();
        let (writer_a, _control_a, _render_a) = test_client_writer();
        let (writer_b, _control_b, _render_b) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 1,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: Some(Box::new(local_config.live_keybinds().unwrap())),
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: Some("Test".into()),
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_a,
        }));
        server.app.state.mode = crate::app::Mode::Settings;
        server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
        server.app.state.settings.list.selected = 1;

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\r".to_vec(),
        }));

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer: writer_b,
        }));
        assert_eq!(
            server.app.state.prefix_code,
            crossterm::event::KeyCode::Char('b')
        );
        assert!(!server
            .app
            .state
            .keybinds
            .new_workspace
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+n"));
        assert!(server.app.state.keybinds.new_workspace.bindings.is_empty());

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn terminal_attach_rejects_missing_terminal_and_removes_client() {
        let mut server = test_headless_server();
        let (writer, control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        assert!(server.clients.contains_key(&7));

        assert!(
            !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: "term_missing".to_owned(),
                takeover: false,
            })
        );
        assert!(!server.clients.contains_key(&7));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(
            reason,
            Some("terminal attach failed: terminal term_missing not found".to_owned())
        );
    }

    fn with_terminal_session_test_server(
        test: impl FnOnce(&mut HeadlessServer, crate::terminal::TerminalId, String, String),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let terminal_id_string = terminal_id.to_string();
        let public_pane_id = format!("{}:p1", workspace.id);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );

        test(&mut server, terminal_id, terminal_id_string, public_pane_id);

        drop(server);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn connect_pending_terminal_client(server: &mut HeadlessServer, client_id: u64) {
        let _control_rx = connect_pending_terminal_client_with_control_rx(server, client_id);
    }

    fn connect_pending_terminal_client_with_control_rx(
        server: &mut HeadlessServer,
        client_id: u64,
    ) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (writer, control_rx, _render_rx) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        control_rx
    }

    #[test]
    fn explicit_agent_history_read_requires_idle_on_alternate_screen() {
        with_terminal_session_test_server(
            |server, terminal_id, _terminal_id_string, public_pane_id| {
                let terminal = server
                    .app
                    .state
                    .terminals
                    .get_mut(&terminal_id)
                    .expect("terminal");
                terminal.detected_agent = Some(crate::detect::Agent::Claude);
                terminal.state = crate::detect::AgentState::Working;
                server.app.terminal_runtimes.insert(
                    terminal_id,
                    crate::terminal::TerminalRuntime::test_with_screen_bytes(
                        80,
                        24,
                        b"\x1b[?1049hworking",
                    ),
                );
                let request = api::schema::Request {
                    id: "read".into(),
                    method: api::schema::Method::AgentRead(api::schema::AgentReadParams {
                        target: public_pane_id.clone(),
                        source: api::schema::ReadSource::Recent,
                        lines: Some(200),
                        format: api::schema::ReadFormat::Text,
                        strip_ansi: true,
                    }),
                };

                assert_eq!(
                    server.agent_read_not_idle_error(&request),
                    Some(api::schema::ErrorBody {
                        code: "agent_not_idle".into(),
                        message: format!(
                            "cannot read 200 lines while {public_pane_id} is working: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible"
                        ),
                    })
                );

                let mut default_request = request.clone();
                let api::schema::Method::AgentRead(params) = &mut default_request.method else {
                    unreachable!();
                };
                params.lines = None;
                assert_eq!(server.agent_read_not_idle_error(&default_request), None);

                let mut visible_request = request;
                let api::schema::Method::AgentRead(params) = &mut visible_request.method else {
                    unreachable!();
                };
                params.source = api::schema::ReadSource::Visible;
                assert_eq!(server.agent_read_not_idle_error(&visible_request), None);
            },
        );
    }

    #[test]
    fn terminal_observe_allows_multiple_clients_without_attach_ownership() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            let initial_size = server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size();

            for client_id in [7, 8] {
                connect_pending_terminal_client(server, client_id);
                assert!(
                    server.handle_server_event(ServerEvent::ClientObserveTerminal {
                        client_id,
                        target: terminal_id_string.clone(),
                    })
                );
            }

            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
            assert_eq!(
                server
                    .app
                    .terminal_runtimes
                    .get(&terminal_id)
                    .expect("runtime")
                    .current_size(),
                initial_size
            );
            assert_eq!(
                terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
                2
            );
        });
    }

    #[test]
    fn terminal_observe_resolves_public_pane_id() {
        with_terminal_session_test_server(|server, terminal_id, _, public_pane_id| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: public_pane_id,
                })
            );

            assert!(matches!(
                server.clients.get(&7).map(|client| &client.mode),
                Some(ClientConnectionMode::TerminalObserve { terminal_id: observed })
                    if observed == &terminal_id.to_string()
            ));
        });
    }

    #[test]
    fn terminal_control_resolves_public_pane_id_and_takes_ownership() {
        with_terminal_session_test_server(
            |server, terminal_id, terminal_id_string, public_pane_id| {
                connect_pending_terminal_client(server, 7);
                assert!(
                    server.handle_server_event(ServerEvent::ClientControlTerminal {
                        client_id: 7,
                        target: public_pane_id,
                        takeover: false,
                    })
                );

                assert!(matches!(
                    server.clients.get(&7).map(|client| &client.mode),
                    Some(ClientConnectionMode::TerminalAttach { terminal_id: attached })
                        if attached == &terminal_id_string
                ));
                assert_eq!(
                    server.terminal_attach_owners.get(&terminal_id_string),
                    Some(&7)
                );
                assert!(server
                    .app
                    .state
                    .direct_attach_resize_locks
                    .contains(&terminal_id));
            },
        );
    }

    #[test]
    fn terminal_control_rejects_attach_during_alt_screen_read() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            let (respond_to, _response_rx) = std::sync::mpsc::channel();
            server.pending_alt_screen_reads.push(
                crate::server::alt_screen_read::PendingAltScreenRead::start(
                    terminal_id,
                    "read".into(),
                    respond_to,
                    "fallback".into(),
                    api::schema::PaneReadResult {
                        pane_id: "w1:p1".into(),
                        workspace_id: "w1".into(),
                        tab_id: "w1:t1".into(),
                        source: api::schema::ReadSource::Recent,
                        format: api::schema::ReadFormat::Text,
                        text: String::new(),
                        revision: 0,
                        truncated: false,
                    },
                    120,
                    false,
                    crate::terminal::ScreenSnapshot {
                        cols: 80,
                        rows: Vec::new(),
                    },
                    0,
                    Instant::now(),
                ),
            );
            let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);

            assert!(
                !server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );
            assert!(!server.clients.contains_key(&7));
            assert!(!server
                .terminal_attach_owners
                .contains_key(&terminal_id_string));
            let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
            assert_eq!(
                reason,
                Some(format!(
                    "terminal attach failed: terminal {terminal_id_string} has a read in progress; retry"
                ))
            );
        });
    }

    #[test]
    fn terminal_control_rejects_second_controller_without_takeover() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                !server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            assert!(server.clients.contains_key(&7));
            assert!(!server.clients.contains_key(&8));
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
        });
    }

    #[test]
    fn terminal_control_takeover_replaces_existing_controller() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                    takeover: true,
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.clients.contains_key(&8));
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&8)
            );
        });
    }

    #[test]
    fn terminal_observe_can_coexist_with_terminal_control() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            connect_pending_terminal_client(server, 8);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 8,
                    target: terminal_id_string.clone(),
                })
            );

            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
            assert!(matches!(
                server.clients.get(&8).map(|client| &client.mode),
                Some(ClientConnectionMode::TerminalObserve { terminal_id })
                    if terminal_id == &terminal_id_string
            ));
            assert_eq!(
                terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
                2
            );
        });
    }

    #[test]
    fn terminal_control_detach_sends_shutdown_before_removal() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
            let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientControlTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                    takeover: false,
                })
            );

            assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 7 }));

            assert!(!server.clients.contains_key(&7));
            assert!(!server
                .terminal_attach_owners
                .contains_key(&terminal_id_string));
            let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
            assert_eq!(reason, Some("detached".to_owned()));
        });
    }

    #[test]
    fn terminal_observe_rejects_later_attach_upgrade() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                })
            );
            assert!(
                !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                    client_id: 7,
                    terminal_id: terminal_id_string,
                    takeover: true,
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
        });
    }

    #[test]
    fn terminal_attach_rejects_later_observe_and_clears_ownership() {
        with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
            connect_pending_terminal_client(server, 7);
            assert!(
                server.handle_server_event(ServerEvent::ClientAttachTerminal {
                    client_id: 7,
                    terminal_id: terminal_id_string.clone(),
                    takeover: false,
                })
            );
            assert_eq!(
                server.terminal_attach_owners.get(&terminal_id_string),
                Some(&7)
            );
            assert!(server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));

            assert!(
                !server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id: 7,
                    target: terminal_id_string.clone(),
                })
            );

            assert!(!server.clients.contains_key(&7));
            assert!(server.terminal_attach_owners.is_empty());
            assert!(!server
                .app
                .state
                .direct_attach_resize_locks
                .contains(&terminal_id));
        });
    }

    fn app_client_marks_git_refresh_due_on_first_attach(render_encoding: RenderEncoding) {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let future = Instant::now() + Duration::from_secs(60);
        server.app.last_git_remote_status_refresh = future;
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));

        assert!(server.has_app_client());
        assert!(server
            .app
            .git_refresh_deadline()
            .is_some_and(|deadline| deadline <= Instant::now()));
    }

    #[test]
    fn terminal_ansi_app_client_enables_headless_git_refresh() {
        app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::TerminalAnsi);
    }

    #[test]
    fn pending_terminal_attach_client_does_not_enable_headless_git_refresh() {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));

        assert!(!server.has_app_client());
        assert_eq!(
            server.app.next_headless_loop_deadline_with_git_refresh(
                Instant::now(),
                false,
                server.has_app_client()
            ),
            None
        );
    }

    #[test]
    fn writerless_app_client_does_not_enable_headless_git_refresh() {
        let mut server = test_headless_server();
        server
            .app
            .state
            .workspaces
            .push(crate::workspace::Workspace::test_new("test"));
        let (writer, _control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        assert!(server.has_app_client());

        server.clients.get_mut(&7).expect("client").writer = None;

        assert!(!server.has_app_client());
        assert_eq!(
            server.app.next_headless_loop_deadline_with_git_refresh(
                Instant::now(),
                false,
                server.has_app_client()
            ),
            None
        );
    }

    #[test]
    fn semantic_app_client_marks_git_refresh_due_on_first_attach() {
        app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::SemanticFrame);
    }

    #[test]
    fn unchanged_git_refresh_does_not_request_headless_render() {
        let mut server = test_headless_server();
        server.app.git_refresh_in_flight = true;
        let mut workspace = crate::workspace::Workspace::test_new("one");
        let workspace_id = workspace.id.clone();
        let cwd = workspace.identity_cwd.clone();
        workspace.cached_auto_label = "cached".into();
        workspace.cached_git_status_key = cwd.clone();
        workspace.cached_git_branch = None;
        server.app.state.workspaces.push(workspace);

        let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
            results: vec![crate::workspace::WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                demand: crate::workspace::GitStatusRefreshDemand::ALL,
                auto_label: "cached".into(),
                branch: None,
                ahead_behind: None,
                space: None,
            }],
            cache_updates: Vec::new(),
        });

        assert!(!changed);
        assert!(!server.app.git_refresh_in_flight);
    }

    #[test]
    fn changed_git_refresh_requests_headless_render() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("one");
        let workspace_id = workspace.id.clone();
        let cwd = workspace.identity_cwd.clone();
        server.app.state.workspaces.push(workspace);

        let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
            results: vec![crate::workspace::WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                demand: crate::workspace::GitStatusRefreshDemand::ALL,
                auto_label: "one".into(),
                branch: Some("changed".into()),
                ahead_behind: None,
                space: None,
            }],
            cache_updates: Vec::new(),
        });

        assert!(changed);
    }

    #[test]
    fn terminal_attach_client_exits_when_attached_pane_dies() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("attached");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane")
            .attached_terminal_id
            .to_string();
        let (writer, control_rx, _render_rx) = test_client_writer();

        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 7,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id.clone(),
                takeover: false,
            })
        );
        assert_eq!(server.terminal_attach_owners.get(&terminal_id), Some(&7));

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id,
                child_pid: None,
            })
        );

        assert!(!server.clients.contains_key(&7));
        assert!(!server.terminal_attach_owners.contains_key(&terminal_id));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(reason, Some(format!("terminal {terminal_id} exited")));
    }
    #[tokio::test]
    async fn stale_pane_died_does_not_publish_exit_for_replacement_runtime() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("replacement");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        let terminal = server.app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.detected_agent = Some(crate::detect::Agent::Codex);
        terminal.state = crate::detect::AgentState::Working;
        let (runtime, _input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        runtime.test_set_child_pid(202);
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id,
                child_pid: Some(101),
            })
        );

        assert!(server.app.find_pane(pane_id).is_some());
        assert_eq!(
            server.app.state.terminals[&terminal_id].state,
            crate::detect::AgentState::Working
        );
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .and_then(crate::terminal::TerminalRuntime::child_pid),
            Some(202)
        );
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn terminal_attach_scroll_moves_attached_runtime_viewport() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);

        apply_terminal_attach_scroll(
            &runtime,
            AttachScrollSource::Wheel,
            AttachScrollDirection::Up,
            3,
            None,
            None,
            0,
        )
        .expect("scroll up");
        let metrics = runtime.scroll_metrics().expect("scroll metrics");
        assert_eq!(metrics.offset_from_bottom, 3);

        apply_terminal_attach_scroll(
            &runtime,
            AttachScrollSource::Wheel,
            AttachScrollDirection::Down,
            2,
            None,
            None,
            0,
        )
        .expect("scroll down");
        let metrics = runtime.scroll_metrics().expect("scroll metrics");
        assert_eq!(metrics.offset_from_bottom, 1);
        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    #[test]
    fn terminal_attach_input_resets_scrolled_viewport() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                20, 5, 4096, &bytes, 4,
            );

        runtime.scroll_up(4);
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );

        apply_terminal_attach_input(&runtime, b"x".to_vec()).expect("attach input");
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded input"),
            Bytes::from("x")
        );

        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn with_terminal_attach_runtime(
        initial_bytes: &[u8],
        initial_scroll: usize,
        test: impl FnOnce(&crate::terminal::TerminalRuntime, &mut mpsc::Receiver<Bytes>),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut bytes = initial_bytes.to_vec();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                20, 5, 4096, &bytes, 4,
            );
        if initial_scroll > 0 {
            runtime.scroll_up(initial_scroll);
        }

        test(&runtime, &mut input_rx);

        drop(runtime);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    fn apply_terminal_attach_page_up(runtime: &crate::terminal::TerminalRuntime) {
        apply_terminal_attach_scroll(
            runtime,
            AttachScrollSource::PageKey {
                input: b"\x1b[5~".to_vec(),
            },
            AttachScrollDirection::Up,
            4,
            None,
            None,
            0,
        )
        .expect("page key");
    }

    #[test]
    fn terminal_attach_paste_uses_plain_text_when_runtime_did_not_enable_brackets() {
        with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
            apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
                .expect("attach paste");

            assert_eq!(
                input_rx.try_recv().expect("forwarded paste"),
                Bytes::from_static(b"line one\nline two")
            );
        });
    }

    #[test]
    fn terminal_attach_paste_preserves_brackets_when_runtime_enabled_them() {
        with_terminal_attach_runtime(b"\x1b[?2004h", 0, |runtime, input_rx| {
            apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
                .expect("attach paste");

            assert_eq!(
                input_rx.try_recv().expect("forwarded paste"),
                Bytes::from_static(b"\x1b[200~line one\nline two\x1b[201~")
            );
        });
    }

    #[test]
    fn terminal_attach_page_key_host_scrolls_plain_terminal() {
        with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                4
            );
            assert!(input_rx.try_recv().is_err());
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_when_mouse_reporting() {
        with_terminal_attach_runtime(b"\x1b[?1000h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_when_application_cursor() {
        with_terminal_attach_runtime(b"\x1b[?1h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn terminal_attach_page_key_host_scrolls_shell_like_decckm_with_bracketed_paste() {
        with_terminal_attach_runtime(b"\x1b[?1h\x1b[?2004h", 0, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                4
            );
            assert!(input_rx.try_recv().is_err());
        });
    }

    #[test]
    fn terminal_attach_page_key_forwards_in_alternate_screen_without_mouse_reporting() {
        with_terminal_attach_runtime(b"\x1b[?1049h", 3, |runtime, input_rx| {
            apply_terminal_attach_page_up(runtime);

            assert_eq!(
                runtime
                    .scroll_metrics()
                    .expect("scroll metrics")
                    .offset_from_bottom,
                0
            );
            assert_eq!(
                input_rx.try_recv().expect("forwarded page key"),
                Bytes::from_static(b"\x1b[5~")
            );
        });
    }

    #[test]
    fn headless_scheduled_tasks_expire_agent_metadata() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("metadata");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::HookStateReported {
                pane_id,
                source: "custom:pi".into(),
                agent_label: "pi".into(),
                state: crate::detect::AgentState::Working,
                message: None,
                seq: None,
                session_ref: None,
            })
        );
        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::HookMetadataReported {
                pane_id,
                source: "user:pi-display".into(),
                agent_label: Some("pi".into()),
                applies_to_source: Some("custom:pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
                clear_title: false,
                clear_display_agent: false,
                clear_state_labels: false,
                seq: None,
                // Expiry is advanced with the captured deadline below; keep the
                // pre-expiry assertion independent of wall-clock scheduling.
                ttl: Some(Duration::from_secs(60)),
            })
        );

        let deadline = server
            .app
            .agent_metadata_deadline
            .expect("metadata deadline");
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane")
            .attached_terminal_id
            .clone();
        assert_eq!(
            server
                .app
                .state
                .terminals
                .get(&terminal_id)
                .expect("terminal")
                .effective_title()
                .as_deref(),
            Some("short lived")
        );

        assert!(server.handle_scheduled_tasks_headless(deadline + Duration::from_millis(1), false));

        assert_eq!(server.app.agent_metadata_deadline, None);
        assert_eq!(
            server
                .app
                .state
                .terminals
                .get(&terminal_id)
                .expect("terminal")
                .effective_title(),
            None
        );
        assert!(server
            .app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| {
                event.event == crate::api::schema::EventKind::PaneAgentStatusChanged
                    && matches!(
                        &event.data,
                        crate::api::schema::EventData::PaneAgentStatusChanged {
                            title,
                            ..
                        } if title.is_none()
                    )
            }));
    }
    #[tokio::test]
    async fn headless_scheduled_tasks_advance_findr_scrollback_scan_without_requesting_render() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("findr");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let scrollback = (0..5_000)
            .map(|row| format!("needle {row}\r\n"))
            .collect::<String>();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.mode = app::Mode::Findr;
        server.app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                80,
                24,
                128 * 1024,
                scrollback.as_bytes(),
            ),
        );
        let now = Instant::now();
        let mut findr = crate::app::state::FindrState::new(pane_id);
        findr.query = "needle".into();
        findr.scrollback = true;
        findr.scan_end_row_exclusive = 5_001;
        findr.visible_range = Some((4_977, 5_001));
        findr.visible_geometry = Some((80, 24));
        findr.complete = false;
        server.app.state.findr = Some(findr);
        server.app.findr_scan_deadline = Some(now);

        assert!(!server.handle_scheduled_tasks_headless(now, false));

        let findr = server.app.state.findr.as_ref().expect("Findr state");
        assert!(findr.scan_end_row_exclusive < 5_001);
        assert!(findr.scan_end_row_exclusive > 0);
        assert!(!findr.complete);
        assert_eq!(
            server.app.findr_scan_deadline,
            Some(now + crate::app::state::FINDR_SCAN_INTERVAL)
        );
        shutdown_test_runtimes(&mut server);
    }
    #[tokio::test]
    async fn headless_render_refreshes_visible_findr_after_foreground_reflow() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("findr");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Findr;
        server.app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"needle"),
        );
        let mut findr = crate::app::state::FindrState::new(pane_id);
        findr.query = "needle".into();
        findr.visible_geometry = Some((80, 24));
        server.app.state.findr = Some(findr);
        let (writer, _control_rx, _render_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                Some(writer),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        server.render_and_stream();

        let findr = server.app.state.findr.as_ref().expect("Findr state");
        let info = server
            .app
            .state
            .pane_info_by_id(pane_id)
            .expect("pane info");
        assert_eq!(
            findr.visible_geometry,
            Some((info.inner_rect.width, info.inner_rect.height))
        );
        assert_ne!(findr.visible_geometry, Some((80, 24)));
        assert_eq!(findr.matches.len(), 1);
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn background_client_render_does_not_replace_foreground_findr_geometry() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("findr-clients");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let mut scrollback = (0..30)
            .map(|row| format!("before {row}\r\n"))
            .collect::<String>();
        scrollback.push_str("needle-old\r\n");
        scrollback.extend((0..29).map(|row| format!("after {row}\r\n")));
        scrollback.push_str("needle-current");
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Findr;
        server.app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                80,
                24,
                4096,
                scrollback.as_bytes(),
            ),
        );
        let mut findr = crate::app::state::FindrState::new(pane_id);
        findr.query = "needle".into();
        server.app.state.findr = Some(findr);
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.findr = None;
        server.app.state.mode = app::Mode::Terminal;
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);

        let (background_writer, _background_control, background_render) = test_client_writer();
        let mut background = ClientConnection::new(
            (80, 80),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            Some(background_writer),
        );
        background.navigation = Some(background_navigation);
        server.clients.insert(1, background);
        let (foreground_writer, _foreground_control, foreground_render) = test_client_writer();
        let mut foreground = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_writer),
        );
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        assert!(server.app.refresh_findr_visible_if_needed(&HashSet::new()));
        let foreground_findr = server.app.state.findr.clone().expect("Findr state");
        let foreground_geometry = foreground_findr
            .visible_geometry
            .expect("foreground geometry");
        let foreground_matches = foreground_findr.matches.len();
        assert_eq!(
            foreground_matches, 1,
            "only the current match is foreground-visible"
        );

        server.render_and_stream();

        let background_frame =
            read_server_frame(background_render.recv().expect("background frame"));
        let foreground_frame =
            read_server_frame(foreground_render.recv().expect("foreground frame"));
        assert!(
            !frame_text(&background_frame).contains("visible matches"),
            "background non-Findr projection must not render foreground Findr state: {}",
            frame_text(&background_frame),
        );
        assert!(
            frame_text(&foreground_frame)
                .contains(&format!("{foreground_matches} visible matches")),
            "foreground frame must retain its Findr results: {}",
            frame_text(&foreground_frame),
        );
        assert!(
            server.clients[&1]
                .navigation
                .as_ref()
                .expect("background projection")
                .findr
                .is_none(),
            "background client state must remain non-Findr",
        );
        assert_eq!(
            server.clients[&2]
                .navigation
                .as_ref()
                .expect("foreground projection")
                .findr
                .as_ref()
                .map(|(_, findr)| findr),
            Some(&foreground_findr),
        );
        assert_eq!(server.app.state.findr.as_ref(), Some(&foreground_findr));
        assert_eq!(server.app.state.mode, app::Mode::Findr);
        let info = server
            .app
            .state
            .pane_info_by_id(pane_id)
            .expect("pane info");
        assert_eq!(
            server.app.state.findr.as_ref().unwrap().visible_geometry,
            Some(foreground_geometry)
        );
        assert_eq!(
            server.app.state.findr.as_ref().unwrap().visible_geometry,
            Some((info.inner_rect.width, info.inner_rect.height))
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn background_findr_render_refreshes_its_projection_without_leaking_foreground() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("background-findr");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let mut scrollback = (0..30)
            .map(|row| format!("before {row}\r\n"))
            .collect::<String>();
        scrollback.push_str("needle-old\r\n");
        scrollback.extend((0..29).map(|row| format!("after {row}\r\n")));
        scrollback.push_str("needle-current");
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = app::Mode::Prefix;
        server.app.terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                80,
                24,
                4096,
                scrollback.as_bytes(),
            ),
        );
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        let mut findr = crate::app::state::FindrState::new(pane_id);
        findr.query = "needle".into();
        server.app.state.findr = Some(findr);
        server.app.state.mode = app::Mode::Findr;
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);

        let (background_writer, _background_control, background_render) = test_client_writer();
        let mut background = ClientConnection::new(
            (80, 80),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            Some(background_writer),
        );
        background.navigation = Some(background_navigation);
        server.clients.insert(1, background);
        let (foreground_writer, _foreground_control, foreground_render) = test_client_writer();
        let mut foreground = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_writer),
        );
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);

        server.render_and_stream();

        let background_frame =
            read_server_frame(background_render.recv().expect("background frame"));
        let foreground_frame =
            read_server_frame(foreground_render.recv().expect("foreground frame"));
        let background_findr = server.clients[&1]
            .navigation
            .as_ref()
            .expect("background projection")
            .findr
            .as_ref()
            .expect("refreshed background Findr")
            .1
            .clone();
        assert_eq!(background_findr.query, "needle");
        assert_eq!(background_findr.matches.len(), 2);
        assert!(
            background_findr
                .visible_geometry
                .is_some_and(|(_, height)| height > 24),
            "background Findr must use its 80-row client geometry"
        );
        assert!(frame_text(&background_frame).contains(&format!(
            "{} visible matches",
            background_findr.matches.len()
        )));
        assert!(!frame_text(&foreground_frame).contains("visible matches"));
        assert!(server.clients[&2]
            .navigation
            .as_ref()
            .expect("foreground projection")
            .findr
            .is_none());
        assert!(server.app.state.findr.is_none());
        assert_eq!(server.app.state.mode, app::Mode::Prefix);
        server
            .clients
            .get_mut(&1)
            .unwrap()
            .navigation
            .as_mut()
            .unwrap()
            .findr
            .as_mut()
            .unwrap()
            .1
            .complete = false;
        server.app.findr_scan_deadline = None;
        assert!(server.handle_client_input_events(
            1,
            vec![crate::raw_input::RawInputEvent::OuterFocusGained],
        ));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.app.state.mode, app::Mode::Findr);
        assert!(server
            .app
            .state
            .findr
            .as_ref()
            .is_some_and(|findr| !findr.complete));
        assert!(server.app.findr_scan_deadline.is_some());
        shutdown_test_runtimes(&mut server);
    }

    #[test]
    fn headless_scheduled_tasks_clears_disabled_agent_manifest_update_deadline() {
        let mut server = test_headless_server();
        let now = Instant::now();
        server.app.next_agent_manifest_update_check = Some(now - Duration::from_millis(1));

        assert!(!server.handle_scheduled_tasks_headless(now, false));
        assert_eq!(server.app.next_agent_manifest_update_check, None);
    }

    #[tokio::test]
    async fn headless_scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                server.app.state.host_terminal_theme,
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.effective_size = (100, 30);
        server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });
        server.app.pending_agent_resume_retry_at = Some(Instant::now() - Duration::from_millis(1));

        assert!(!server.handle_scheduled_tasks_headless(Instant::now(), true));
        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(server.app.pending_agent_resume_retry_at.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        server.render_and_stream();
        assert_ne!(server.app.state.view.terminal_area, Rect::default());

        let now = Instant::now();
        assert!(!server.handle_scheduled_tasks_headless(now, false));
        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        let deadline = server
            .app
            .pending_agent_resume_retry_at
            .expect("clientless resume should wait briefly for a host theme");

        assert!(server.handle_scheduled_tasks_headless(deadline, false));
        assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_none());
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn headless_pre_input_resize_does_not_start_pending_agent_resume() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        server.app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.ensure_test_terminals();
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                server.app.state.host_terminal_theme,
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.effective_size = (100, 30);
        server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });
        server.app.pending_agent_resume_retry_at = Some(Instant::now() - Duration::from_millis(1));

        server.resize_shared_runtime_to_effective_size_before_input();

        assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(server.app.pending_agent_resume_retry_at.is_none());
    }

    #[test]
    fn virtual_render_produces_nonempty_buffer() {
        let mut state = AppState::test_new();
        let area = Rect::new(0, 0, 80, 24);
        let (buffer, _cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        assert_eq!(buffer.area.width, 80);
        assert_eq!(buffer.area.height, 24);
    }

    #[test]
    fn virtual_render_without_frame_cursor_keeps_cursor_hidden() {
        let mut state = AppState::test_new();
        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn virtual_render_preserves_explicit_frame_cursor_position() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: true,
                shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_preserves_hidden_focused_pane_cursor_position() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: false,
                shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, false);

        assert_eq!(
            cursor, None,
            "child cursor positions are unstable while synchronized output is active"
        );
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output_resize() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let initial_area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, initial_area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());

        let resized_area = Rect::new(0, 0, 100, 30);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, resized_area, true);

        assert_eq!(
            cursor, None,
            "pre-resize synchronized output should suppress the cursor even if resize clears the mode"
        );
    }

    #[tokio::test]
    async fn headless_precomputed_render_hides_cursor_during_synchronized_output_resize() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("sync-resize");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );
        let (writer, _control_rx, render_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                Some(writer),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.render_and_stream();
        let _ = render_rx.recv().expect("initial frame");

        let runtime = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pane runtime");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());
        server.clients.get_mut(&1).unwrap().terminal_size = (100, 30);

        server.render_and_stream();

        let frame = read_server_frame(render_rx.recv().expect("resized frame"));
        assert_eq!(frame.cursor, None);
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn foreground_cursor_guard_survives_background_navigation_projection() {
        let mut server = test_headless_server();
        let first = crate::workspace::Workspace::test_new("foreground-sync");
        let first_pane = first.tabs[0].root_pane;
        let first_terminal = first
            .terminal_id(first_pane)
            .expect("first terminal")
            .clone();
        let second = crate::workspace::Workspace::test_new("background-view");
        let second_pane = second.tabs[0].root_pane;
        let second_terminal = second
            .terminal_id(second_pane)
            .expect("second terminal")
            .clone();
        server.app.state.workspaces = vec![first, second];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(
            first_terminal.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"first"),
        );
        server.app.terminal_runtimes.insert(
            second_terminal,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"second"),
        );
        crate::ui::compute_view_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            Rect::new(0, 0, 80, 24),
        );
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        foreground_navigation.apply_to(&mut server.app.state);
        crate::ui::compute_view_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            Rect::new(0, 0, 80, 24),
        );
        let runtime = server
            .app
            .terminal_runtimes
            .get(&first_terminal)
            .expect("foreground runtime");
        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
        assert!(runtime.synchronized_output_active());

        let (background_writer, _background_control, _background_render) = test_client_writer();
        let mut background = ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(false),
            1,
            RenderEncoding::SemanticFrame,
            Some(background_writer),
        );
        background.navigation = Some(background_navigation);
        server.clients.insert(1, background);
        let (foreground_writer, _foreground_control, foreground_render) = test_client_writer();
        let mut foreground = ClientConnection::new(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_writer),
        );
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);

        server.render_and_stream();

        let frame = read_server_frame(foreground_render.recv().expect("foreground frame"));
        assert_eq!(frame.cursor, None);
        shutdown_test_runtimes(&mut server);
    }
    #[tokio::test]
    async fn virtual_render_exposes_hidden_pane_cursor_when_reveal_hidden_for_cjk_ime() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x + 4,
                y: pane.inner_rect.y,
                visible: true,
                shape: state.cjk_ime_cursor_shape,
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_keeps_cursor_hidden_when_scrolled_back_even_with_reveal_hidden_for_cjk_ime(
    ) {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.scroll_up(6);
        assert!(crate::ui::pane_is_scrolled_back(runtime));

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "scrolled-back focused pane should keep the cursor hidden even when reveal_hidden_cursor_for_cjk_ime is true; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_fallback_cursor_when_viewport_none_and_reveal_hidden_for_cjk_ime() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        // Feed only ?25l with no prior cursor movement — exercises the fallback
        // path for TUIs whose viewport has no cursor position.
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);
        let pane = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == pane_id)
            .expect("focused pane info");

        assert_eq!(
            cursor,
            Some(CursorState {
                x: pane.inner_rect.x,
                y: pane.inner_rect.y,
                visible: true,
                shape: state.cjk_ime_cursor_shape,
            }),
            "fallback should anchor at pane top-left with the configured shape",
        );
    }

    #[tokio::test]
    async fn virtual_render_skips_reveal_when_focused_pane_has_no_detected_agent() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        // Filter only Claude, but the test pane has no detected agent, so the
        // reveal must not apply.
        state.cjk_ime_agent_filter_configured = true;
        state.cjk_ime_agents = vec![crate::detect::Agent::Claude];
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "agent filter should suppress reveal when the focused pane's detected agent is not on the list; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_skips_reveal_when_agent_filter_has_no_valid_entries() {
        let mut state = AppState::test_new();
        state.reveal_hidden_cursor_for_cjk_ime = true;
        state.cjk_ime_agent_filter_configured = true;
        state.cjk_ime_agents = Vec::new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "agent filter with no valid entries should suppress reveal; got {cursor:?}",
        );
    }

    #[tokio::test]
    async fn virtual_render_omits_focused_pane_cursor_while_mobile_switcher_open() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        ws.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
        );

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Navigate;

        let area = Rect::new(0, 0, 44, 24);
        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert_eq!(cursor, None);
    }

    #[tokio::test]
    async fn virtual_render_hides_focused_pane_cursor_while_scrolled_back() {
        let mut state = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
        ws.insert_test_runtime(pane_id, runtime);

        state.workspaces = vec![ws];
        state.active = Some(0);
        state.selected = 0;
        state.mode = crate::app::Mode::Terminal;

        let area = Rect::new(0, 0, 80, 24);
        let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let runtime = state
            .runtime_for_pane(&terminal_runtimes, pane_id)
            .expect("pane runtime after initial render");
        runtime.scroll_up(6);
        assert!(crate::ui::pane_is_scrolled_back(runtime));

        let (_buffer, cursor) =
            crate::server::render_stream::render_virtual(&mut state, area, true);

        assert!(
            cursor.as_ref().is_none_or(|cursor| !cursor.visible),
            "cursor: {cursor:?}"
        );
    }

    #[test]
    fn latest_active_client_drives_shared_size_theme_and_fallback() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (160, 45),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: Some(crate::terminal_theme::RgbColor {
                        r: 0xaa,
                        g: 0xbb,
                        b: 0xcc,
                    }),
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 0x11,
                        g: 0x22,
                        b: 0x33,
                    }),
                    ..Default::default()
                },
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: Some(crate::terminal_theme::RgbColor {
                        r: 0x10,
                        g: 0x20,
                        b: 0x30,
                    }),
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 0xdd,
                        g: 0xee,
                        b: 0xff,
                    }),
                    ..Default::default()
                },
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (160, 45));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&1].host_terminal_theme
        );

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.effective_size, (80, 24));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&2].host_terminal_theme
        );

        assert!(server.remove_client(2));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (160, 45));
        assert_eq!(
            server.app.state.host_terminal_theme,
            server.clients[&1].host_terminal_theme
        );
    }

    #[tokio::test]
    async fn deferred_new_tab_stays_with_requesting_client_across_following_navigation() {
        let mut server = test_headless_server();
        let requester_workspace = crate::workspace::Workspace::test_new("requester");
        let requester_workspace_id = requester_workspace.id.clone();
        let following_workspace = crate::workspace::Workspace::test_new("following");
        server.app.state.workspaces = vec![requester_workspace, following_workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let requester_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        let following_navigation = ClientNavigationState::capture(&server.app.state);

        let mut requester = test_app_client(Some(true), 1);
        requester.navigation = Some(requester_navigation);
        server.clients.insert(1, requester);
        let mut following = test_app_client(Some(true), 2);
        following.navigation = Some(following_navigation);
        server.clients.insert(2, following);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        server.app.state.request_new_tab = true;
        assert!(server.handle_client_input_events(1, vec![]));

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 2,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
        }));
        let _ = server.handle_deferred_requests_headless();

        assert_eq!(server.app.state.workspaces[0].tabs.len(), 2);
        assert_eq!(server.app.state.workspaces[1].tabs.len(), 1);
        assert_eq!(server.app.state.active, Some(1));
        assert_eq!(server.app.state.workspaces[1].active_tab, 0);
        assert!(!server.app.state.request_new_tab);
        let new_tab_id = crate::workspace::public_tab_id_for_number(&requester_workspace_id, 2);
        assert_eq!(
            server.clients[&1]
                .navigation
                .as_ref()
                .and_then(|navigation| navigation
                    .active_tab_by_workspace
                    .get(&requester_workspace_id)),
            Some(&new_tab_id)
        );
        shutdown_test_runtimes(&mut server);
    }

    #[cfg(unix)]
    #[test]
    fn handoff_disconnect_preserves_foreground_navigation_across_removal_order() {
        let mut server = test_headless_server();
        let mut first_workspace = crate::workspace::Workspace::test_new("first");
        let first_second_tab = first_workspace.test_add_tab(Some("first-second"));
        let mut second_workspace = crate::workspace::Workspace::test_new("second");
        let second_second_tab = second_workspace.test_add_tab(Some("second-second"));
        first_workspace.active_tab = first_second_tab;
        second_workspace.active_tab = 0;
        server.app.state.workspaces = vec![first_workspace, second_workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        let first_navigation = ClientNavigationState::capture(&server.app.state);

        server.app.state.workspaces[0].active_tab = 0;
        server.app.state.workspaces[1].active_tab = second_second_tab;
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        let second_navigation = ClientNavigationState::capture(&server.app.state);

        let mut first = test_app_client(Some(true), 1);
        first.navigation = Some(first_navigation);
        server.clients.insert(1, first);
        let mut second = test_app_client(Some(true), 2);
        second.navigation = Some(second_navigation);
        server.clients.insert(2, second);

        let removal_order = server.clients.keys().copied().collect::<Vec<_>>();
        let original_foreground = removal_order[0];
        let expected_navigation = server.clients[&original_foreground]
            .navigation
            .clone()
            .unwrap()
            .apply_to(&mut server.app.state);
        let expected_workspace = server.app.state.active.unwrap();
        let expected_tab = server.app.state.workspaces[expected_workspace].active_tab;
        server.foreground_client_id = Some(original_foreground);
        server.sync_foreground_client_state();

        server.disconnect_all_clients_for_handoff();

        assert!(server.clients.is_empty());
        assert_eq!(server.foreground_client_id, None);
        assert_eq!(
            ClientNavigationState::capture(&server.app.state),
            expected_navigation
        );
        assert_eq!(server.app.state.active, Some(expected_workspace));
        assert_eq!(
            server.app.state.workspaces[expected_workspace].active_tab,
            expected_tab
        );
    }

    #[test]
    fn detach_keybind_promotes_and_renders_surviving_client_navigation() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("detach-navigation");
        let second_tab = workspace.test_add_tab(Some("second"));
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.detach_exits = false;

        let survivor_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].active_tab = second_tab;
        let detached_navigation = ClientNavigationState::capture(&server.app.state);

        let (survivor_writer, _survivor_control_rx, survivor_render_rx) = test_client_writer();
        let mut survivor = ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            Some(survivor_writer),
        );
        survivor.navigation = Some(survivor_navigation.clone());
        server.clients.insert(1, survivor);

        let (detached_writer, detached_control_rx, _detached_render_rx) = test_client_writer();
        let mut detached = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            Some(detached_writer),
        );
        detached.navigation = Some(detached_navigation);
        server.clients.insert(2, detached);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        let key = |code, modifiers| crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char(code),
            modifiers,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        };
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 2,
            events: vec![key('b', KeyModifiers::CONTROL.bits()), key('q', 0)],
        }));

        assert!(!server.clients.contains_key(&2));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.effective_size, (120, 40));
        assert_eq!(
            ClientNavigationState::capture(&server.app.state),
            survivor_navigation
        );
        match read_server_message(
            detached_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("detached client shutdown"),
        ) {
            ServerMessage::ServerShutdown { reason } => {
                assert_eq!(reason.as_deref(), Some("detached"));
            }
            other => panic!("expected detached shutdown, got {other:?}"),
        }

        server.render_and_stream();
        survivor_render_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("surviving client repaint");
    }

    #[test]
    fn background_focus_loss_does_not_mark_its_projected_tab_seen() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("focus-loss-navigation");
        let foreground_pane = workspace.tabs[0].root_pane;
        let background_tab = workspace.test_add_tab(Some("background"));
        let background_pane = workspace.tabs[background_tab].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;

        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].active_tab = background_tab;
        let background_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].active_tab = 0;
        server.app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&foreground_pane)
            .unwrap()
            .seen = false;
        server.app.state.workspaces[0].tabs[background_tab]
            .panes
            .get_mut(&background_pane)
            .unwrap()
            .seen = false;

        let mut background = test_app_client(Some(true), 1);
        background.navigation = Some(background_navigation);
        server.clients.insert(1, background);
        let mut foreground = test_app_client(Some(true), 2);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        assert!(server.app.state.workspaces[0].tabs[0].panes[&foreground_pane].seen);
        assert!(!server.app.state.workspaces[0].tabs[background_tab].panes[&background_pane].seen);

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::FocusLost],
        }));

        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.app.state.workspaces[0].active_tab, 0);
        assert!(!server.app.state.workspaces[0].tabs[background_tab].panes[&background_pane].seen);
    }

    #[test]
    fn foreground_disconnect_marks_the_survivors_projected_tab_seen() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("disconnect-navigation");
        let outgoing_pane = workspace.tabs[0].root_pane;
        let survivor_tab = workspace.test_add_tab(Some("survivor"));
        let survivor_pane = workspace.tabs[survivor_tab].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;

        let outgoing_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].active_tab = survivor_tab;
        let survivor_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.workspaces[0].active_tab = 0;
        server.app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&outgoing_pane)
            .unwrap()
            .seen = false;
        server.app.state.workspaces[0].tabs[survivor_tab]
            .panes
            .get_mut(&survivor_pane)
            .unwrap()
            .seen = false;

        let mut survivor = test_app_client(Some(true), 1);
        survivor.navigation = Some(survivor_navigation);
        server.clients.insert(1, survivor);
        let mut outgoing = test_app_client(Some(true), 2);
        outgoing.navigation = Some(outgoing_navigation);
        server.clients.insert(2, outgoing);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        assert!(server.app.state.workspaces[0].tabs[0].panes[&outgoing_pane].seen);
        assert!(!server.app.state.workspaces[0].tabs[survivor_tab].panes[&survivor_pane].seen);

        assert!(server.remove_client(2));

        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.app.state.workspaces[0].active_tab, survivor_tab);
        assert!(server.app.state.workspaces[0].tabs[survivor_tab].panes[&survivor_pane].seen);
    }

    #[test]
    fn foreground_client_without_host_theme_clears_previous_host_theme() {
        let mut server = test_headless_server();
        let known_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 0x10,
                g: 0x20,
                b: 0x30,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 0x40,
                g: 0x50,
                b: 0x60,
            }),
            ..Default::default()
        };
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                known_theme,
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.app.state.host_terminal_theme, known_theme);

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(
            server.app.state.host_terminal_theme,
            crate::terminal_theme::TerminalTheme::default()
        );
    }

    #[test]
    fn foreground_client_appearance_controls_auto_theme() {
        let mut server = test_headless_server();
        server.app.state.theme_runtime.auto_switch = true;
        server.app.state.theme_runtime.dark_name = "catppuccin".to_string();
        server.app.state.theme_runtime.light_name = "catppuccin-latte".to_string();
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: None,
                    background: Some(crate::terminal_theme::RgbColor { r: 0, g: 0, b: 0 }),
                    ..Default::default()
                },
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme {
                    foreground: None,
                    background: Some(crate::terminal_theme::RgbColor {
                        r: 255,
                        g: 255,
                        b: 255,
                    }),
                    ..Default::default()
                },
                None,
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        assert!(server.promote_client_to_foreground(1));
        assert_eq!(server.app.state.theme_name, "catppuccin");

        assert!(server.promote_client_to_foreground(2));
        assert_eq!(server.app.state.theme_name, "catppuccin-latte");
    }

    #[test]
    fn color_scheme_change_event_is_inert_on_server() {
        let mut server = test_headless_server();
        let initial_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 0x10,
                g: 0x20,
                b: 0x30,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 0x40,
                g: 0x50,
                b: 0x60,
            }),
            ..Default::default()
        };
        server.app.state.host_terminal_theme = initial_theme;
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                initial_theme,
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: crate::raw_input::GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.foreground_client_id, None);
        assert_eq!(server.clients[&1].host_terminal_theme, initial_theme);
        assert_eq!(server.app.state.host_terminal_theme, initial_theme);
    }

    #[test]
    fn focus_lost_updates_client_without_promoting_foreground() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[test]
    fn focus_gained_promotes_client_to_foreground() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        });

        assert!(changed);
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[tokio::test]
    async fn foreground_focus_gained_reaches_pane_with_focus_reporting() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");

        server.clients.insert(1, test_app_client(Some(false), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded focus gained report"),
            Bytes::from_static(b"\x1b[I")
        );

        assert!(!server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded focus lost report"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn outer_focus_events_do_not_reach_pane_without_focus_reporting() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"");
        server.clients.insert(1, test_app_client(Some(false), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn background_focus_batch_only_forwards_events_after_promotion() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(false), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 2,
            data: b"\x1b[O\x1b[I".to_vec(),
        }));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("focus gained after client promotion"),
            Bytes::from_static(b"\x1b[I")
        );
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn background_client_focus_loss_releases_its_owned_keys() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[>15u");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(true), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('j'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::FocusLost],
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded press"),
            Bytes::from_static(b"\x1b[106;1:1u")
        );
        assert_eq!(
            input_rx
                .try_recv()
                .expect("synthetic release from background client"),
            Bytes::from_static(b"\x1b[106;1:3u")
        );
        assert!(server.app.input_leases.is_empty());
    }

    #[tokio::test]
    async fn structured_outer_focus_events_reach_reporting_pane() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![
                crate::protocol::ClientInputEvent::FocusGained,
                crate::protocol::ClientInputEvent::FocusLost,
            ],
        }));
        assert_eq!(
            input_rx.try_recv().expect("structured focus gained report"),
            Bytes::from_static(b"\x1b[I")
        );
        assert_eq!(
            input_rx.try_recv().expect("structured focus lost report"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn background_key_makes_later_focus_lost_eligible() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(true), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 2,
            events: vec![
                crate::protocol::ClientInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('x'),
                    modifiers: 0,
                    kind: crate::protocol::ClientKeyKind::Release,

                    repeat_count: 1,
                    generated_text: None,
                    source: crate::protocol::ClientKeySource::Synthesized,
                },
                crate::protocol::ClientInputEvent::FocusLost,
            ],
        }));
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(
            input_rx.try_recv().expect("focus lost after promotion"),
            Bytes::from_static(b"\x1b[O")
        );
    }

    #[tokio::test]
    async fn structured_non_app_focus_is_ignored_without_suppressing_keys() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
        server.clients.insert(1, test_app_client(Some(true), 1));

        let mut attached = test_app_client(Some(false), 2);
        attached.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: "attached".to_owned(),
        };
        server.clients.insert(2, attached);

        let mut pending = test_app_client(Some(false), 3);
        pending.pending_terminal_attach = true;
        server.clients.insert(3, pending);
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        for client_id in [2, 3] {
            assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
                client_id,
                events: vec![crate::protocol::ClientInputEvent::FocusGained],
            }));
            assert_eq!(server.foreground_client_id, Some(1));
            assert_eq!(server.app.state.outer_terminal_focus, Some(true));
            assert_eq!(server.clients[&client_id].outer_terminal_focus, Some(false));
        }

        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 3,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Release,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert_eq!(server.foreground_client_id, Some(3));
    }

    #[test]
    fn terminal_attach_resize_preserves_known_cell_size_when_pixels_are_omitted() {
        with_terminal_session_test_server(|server, _terminal_id, terminal_id, _pane_id| {
            let mut client = ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            );
            client.mode = ClientConnectionMode::TerminalAttach {
                terminal_id: terminal_id.clone(),
            };
            server.clients.insert(1, client);

            assert!(server.handle_server_event(ServerEvent::ClientResize {
                client_id: 1,
                cols: 100,
                rows: 30,
                cell_width_px: 0,
                cell_height_px: 0,
            }));

            assert_eq!(
                server
                    .runtime_for_terminal_id_string(&terminal_id)
                    .unwrap()
                    .pixel_size(),
                Some((1_000, 600))
            );
            assert_eq!(
                server.clients[&1].cell_size,
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                }
            );
        });
    }

    #[tokio::test]
    async fn forwarded_terminal_keys_skip_full_render_unless_local_view_changes() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded terminal key"),
            Bytes::from_static(b"x")
        );

        let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
        server.app.state.selection = Some(crate::selection::Selection::anchor(pane_id, 0, 0, None));
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('y'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert!(server.app.state.selection.is_none());
        assert_eq!(
            input_rx
                .try_recv()
                .expect("forwarded key after selection clear"),
            Bytes::from_static(b"y")
        );

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL.bits(),
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert_eq!(server.app.state.mode, crate::app::Mode::Prefix);
        assert!(matches!(
            input_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn hovering_a_link_requests_headless_render() {
        let mut server = test_headless_server();
        let _input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.app.state.hovered_link.is_some());
    }

    #[tokio::test]
    async fn leaving_a_link_requests_headless_render() {
        let mut server = test_headless_server();
        let _input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 40,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.app.state.hovered_link.is_none());
    }

    #[tokio::test]
    async fn forwarded_key_clearing_hover_requests_headless_render() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert!(server.app.state.hovered_link.is_none());
        assert_eq!(
            input_rx.try_recv().expect("forwarded key"),
            Bytes::from_static(b"x")
        );

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('y'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded key"),
            Bytes::from_static(b"y")
        );

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::TextCommit("text".into())],
        }));
        assert!(server.app.state.hovered_link.is_none());
        assert_eq!(
            input_rx.try_recv().expect("forwarded text"),
            Bytes::from_static(b"text")
        );

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Paste {
                text: "paste".into()
            }],
        }));
        assert!(server.app.state.hovered_link.is_none());
        assert_eq!(
            input_rx.try_recv().expect("forwarded paste"),
            Bytes::from_static(b"paste")
        );
    }

    #[tokio::test]
    async fn foreground_disconnect_clears_inherited_hover_before_rendering_successor() {
        let mut server = test_headless_server();
        let _input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        let (successor_tx, _successor_control_rx, successor_rx) = test_client_writer();
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                Some(successor_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.app.state.hovered_link.is_some());

        assert!(server.remove_client(1));
        assert_eq!(server.foreground_client_id, Some(2));
        assert!(server.app.state.hovered_link.is_none());
        assert!(server.app.state.hovered_pane_cell.is_none());
        server.render_and_stream();
        let successor_frame = read_server_frame(successor_rx.recv().expect("successor frame"));
        assert!(!successor_frame.cells.iter().any(|cell| {
            cell.symbol == "h" && cell.modifier & ratatui::style::Modifier::UNDERLINED.bits() != 0
        }));
    }

    #[tokio::test]
    async fn foreground_connect_clears_inherited_hover() {
        let mut server = test_headless_server();
        let _input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        assert!(server.app.state.hovered_link.is_some());

        let (writer, _control_rx, _render_rx) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::SemanticFrame,
            keybindings: None,
            direct_attach_requested: false,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));

        assert_eq!(server.foreground_client_id, Some(2));
        assert!(server.app.state.hovered_link.is_none());
        assert!(server.app.state.hovered_pane_cell.is_none());
    }

    #[tokio::test]
    async fn passive_mouse_motion_forwards_without_requesting_render() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let baseline = FrameData {
            cells: Vec::new(),
            width: 0,
            height: 0,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let client = server.clients.get_mut(&1).unwrap();
        let prepared = client
            .render_state
            .prepare_frame(baseline.clone())
            .expect("new semantic baseline");
        client.render_state.commit_sent_frame(prepared);
        let pane = server.app.state.view.pane_infos[0].clone();
        let column = pane.inner_rect.x + 2;
        let row = pane.inner_rect.y + 3;
        let input = format!("\x1b[<35;{};{}M", column + 1, row + 1).into_bytes();

        assert!(!server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: input,
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded mouse motion"),
            Bytes::from_static(b"\x1b[<35;3;4M")
        );
        assert_eq!(
            server.clients[&1].render_state.last_frame(),
            Some(&baseline)
        );
    }

    #[test]
    fn background_mouse_motion_promotes_once_then_becomes_render_neutral() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::Terminal;
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.clients.insert(2, test_app_client(Some(true), 2));
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        let motion = || ServerEvent::ClientInputEvents {
            client_id: 2,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: 10,
                row: 5,
                modifiers: 0,
            }],
        };

        assert!(server.handle_server_event(motion()));
        assert_eq!(server.foreground_client_id, Some(2));
        assert!(!server.handle_server_event(motion()));
    }

    #[test]
    fn mouse_motion_in_hover_modes_requires_render() {
        let events = [crate::raw_input::RawInputEvent::Mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::empty(),
            },
        )];

        assert!(events_are_render_neutral_mouse_motion(
            &events,
            crate::app::Mode::Terminal
        ));
        for mode in [
            crate::app::Mode::GlobalMenu,
            crate::app::Mode::ContextMenu,
            crate::app::Mode::Navigator,
        ] {
            assert!(!events_are_render_neutral_mouse_motion(&events, mode));
        }
    }

    fn install_focused_test_runtime(
        server: &mut HeadlessServer,
        terminal_bytes: &[u8],
    ) -> tokio::sync::mpsc::Receiver<Bytes> {
        let workspace = crate::workspace::Workspace::test_new("focus-reporting");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
        let (runtime, input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                terminal_bytes,
                4,
            );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(terminal_id, runtime);
        input_rx
    }

    fn test_app_client(outer_terminal_focus: Option<bool>, last_activity: u64) -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            outer_terminal_focus,
            last_activity,
            RenderEncoding::SemanticFrame,
            None,
        )
    }
    fn test_private_popup_params(
        view_id: Option<crate::api::schema::ViewId>,
    ) -> crate::api::schema::PluginPaneOpenParams {
        crate::api::schema::PluginPaneOpenParams {
            plugin_id: "test.private".to_string(),
            entrypoint: "popup".to_string(),
            placement: Some(crate::api::schema::PluginPanePlacement::Popup),
            scope: Some(crate::api::schema::PluginPaneScope::ClientPrivate),
            view_id,
            width: None,
            height: None,
            workspace_id: None,
            target_pane_id: None,
            direction: None,
            cwd: None,
            focus: false,
            env: std::collections::HashMap::new(),
        }
    }

    fn private_popup_error_code(
        server: &mut HeadlessServer,
        view_id: Option<crate::api::schema::ViewId>,
    ) -> String {
        let (response, changed) = server.handle_client_private_plugin_pane_open(
            "private-test".to_string(),
            test_private_popup_params(view_id),
        );
        assert!(!changed);
        serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
            .expect("private popup error response")
            .error
            .code
    }

    fn test_identity_client(
        display_name: Option<&str>,
        writer: Option<ClientWriter>,
    ) -> ClientConnection {
        ClientConnection::new_with_mode(
            ClientConnectionMode::App,
            None,
            display_name.map(str::to_owned),
            None,
            None,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            false,
            writer,
        )
    }

    fn left_click(column: u16, row: u16) -> crate::raw_input::RawInputEvent {
        crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::empty(),
        })
    }

    #[test]
    fn identity_editor_ignores_unrelated_closed_header_clicks() {
        let mut server = test_headless_server();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));

        let (remaining, changed) = server.intercept_identity_input(1, vec![left_click(79, 23)]);

        assert!(!changed);
        assert_eq!(remaining.len(), 1);
        assert!(!server.clients[&1].identity.as_ref().unwrap().editor.open);
    }

    #[test]
    fn identity_header_click_opens_only_its_client_editor() {
        let mut server = test_headless_server();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), None));
        server
            .clients
            .insert(2, test_identity_client(Some("Bea"), None));
        let identity =
            crate::server::render_stream::identity_ui_state(server.clients[&1].identity.as_ref());
        let header = crate::ui::identity_name_hit_rect(
            &server.app.state,
            Rect::new(0, 0, 80, 24),
            &identity,
        );
        assert!(header.width > 0 && header.height > 0);

        let (remaining, changed) =
            server.intercept_identity_input(1, vec![left_click(header.x, header.y)]);

        assert!(changed);
        assert!(remaining.is_empty());
        assert!(server.clients[&1].identity.as_ref().unwrap().editor.open);
        assert!(!server.clients[&2].identity.as_ref().unwrap().editor.open);
    }

    #[test]
    fn uncommitted_identity_consumes_text_before_shared_routing() {
        let mut server = test_headless_server();
        let mut client = test_identity_client(None, None);
        client.identity.as_mut().unwrap().open_editor();
        server.clients.insert(1, client);

        let (remaining, changed) = server.intercept_identity_input(
            1,
            vec![crate::raw_input::RawInputEvent::Text(
                crate::input::TextCommit::new("Ada"),
            )],
        );

        assert!(changed);
        assert!(remaining.is_empty());
        let identity = server.clients[&1].identity.as_ref().unwrap();
        assert!(identity.editor.open);
        assert_eq!(identity.editor.draft, "Ada");
    }

    #[test]
    fn identity_editor_accepts_repeated_generated_key_text() {
        let mut server = test_headless_server();
        let mut client = test_identity_client(Some("Ada"), None);
        client.identity.as_mut().unwrap().open_editor();
        server.clients.insert(1, client);
        let key = crate::input::TerminalKey::new(KeyCode::Char('E'), KeyModifiers::SHIFT)
            .with_generated_text(Some("É".to_owned()))
            .with_repeat_count(2);

        let (remaining, changed) =
            server.intercept_identity_input(1, vec![crate::raw_input::RawInputEvent::Key(key)]);

        assert!(changed);
        assert!(remaining.is_empty());
        assert_eq!(
            server.clients[&1].identity.as_ref().unwrap().editor.draft,
            "AdaÉÉ"
        );
    }

    #[test]
    fn identity_enter_sends_targeted_persistence_and_keeps_pending_modal_open() {
        let mut server = test_headless_server();
        let (writer, control_rx, _render_rx) = test_client_writer();
        let mut client = test_identity_client(None, Some(writer));
        client.identity.as_mut().unwrap().open_editor();
        client.identity.as_mut().unwrap().insert_editor_text("Ada");
        server.clients.insert(1, client);

        let (remaining, changed) = server.intercept_identity_input(
            1,
            vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
            )],
        );

        assert!(changed);
        assert!(remaining.is_empty());
        let identity = server.clients[&1].identity.as_ref().unwrap();
        assert!(identity.editor.open);
        assert_eq!(
            identity
                .pending
                .as_ref()
                .map(|pending| pending.name.as_str()),
            Some("Ada")
        );
        let bytes = control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("persistence request");
        let message: ServerMessage = protocol::read_message(&mut bytes.as_slice(), MAX_FRAME_SIZE)
            .expect("decode persistence request");
        assert!(
            matches!(message, ServerMessage::PersistIdentity { display_name, .. } if display_name == "Ada")
        );
    }

    #[test]
    fn pending_identity_modal_consumes_text_paste_and_key_release() {
        let mut server = test_headless_server();
        let mut client = test_identity_client(Some("Ada"), None);
        client.identity.as_mut().unwrap().open_editor();
        client.identity.as_mut().unwrap().pending =
            Some(crate::server::clients::PendingIdentityPersistence {
                request_id: 1,
                name: "Ada".to_owned(),
            });
        server.clients.insert(1, client);

        let (remaining, changed) = server.intercept_identity_input(
            1,
            vec![
                crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new("x")),
                crate::raw_input::RawInputEvent::Paste("y".into()),
                crate::raw_input::RawInputEvent::Key(
                    crate::input::TerminalKey::new(KeyCode::Char('z'), KeyModifiers::empty())
                        .with_kind(KeyEventKind::Release),
                ),
            ],
        );

        assert!(changed);
        assert!(remaining.is_empty());
        assert_eq!(
            server.clients[&1].identity.as_ref().unwrap().editor.draft,
            "Ada"
        );
    }

    #[test]
    fn identity_cancel_requires_exact_committed_modal_button() {
        let mut server = test_headless_server();
        let mut client = test_identity_client(Some("Ada"), None);
        client.identity.as_mut().unwrap().open_editor();
        server.clients.insert(1, client);
        let inner = crate::ui::identity_modal_inner_rect(Rect::new(0, 0, 80, 24)).unwrap();
        let (_, cancel) = crate::ui::identity_modal_button_rects(inner, true);

        server.intercept_identity_input(1, vec![left_click(0, 0)]);
        assert!(server.clients[&1].identity.as_ref().unwrap().editor.open);
        server.intercept_identity_input(1, vec![left_click(cancel.x, cancel.y)]);
        assert!(!server.clients[&1].identity.as_ref().unwrap().editor.open);
    }

    #[test]
    fn scroll_input_yields_server_event_drain_until_rendered() {
        let mut server = test_headless_server();
        server.clients.insert(1, test_app_client(Some(true), 1));
        server.foreground_client_id = Some(1);
        server
            .server_event_tx
            .try_send(ServerEvent::ClientInputEvents {
                client_id: 1,
                events: vec![crate::protocol::ClientInputEvent::Mouse {
                    kind: crate::protocol::ClientMouseKind::ScrollUp,
                    column: 10,
                    row: 5,
                    modifiers: 0,
                }],
            })
            .unwrap();
        server
            .server_event_tx
            .try_send(ServerEvent::ClientInput {
                client_id: 99,
                data: b"queued after scroll".to_vec(),
            })
            .unwrap();

        let _ = server.drain_server_events();

        assert!(server.app.scroll_render_pending);
        assert!(matches!(
            server.server_event_rx.try_recv(),
            Ok(ServerEvent::ClientInput { client_id: 99, .. })
        ));
    }

    #[test]
    fn foreground_client_focus_event_updates_app_focus_state() {
        let mut server = test_headless_server();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let changed = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[O".to_vec(),
        });

        assert!(!changed);
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
        assert_eq!(server.app.state.outer_terminal_focus, Some(false));
    }

    #[test]
    fn app_client_lone_escape_closes_navigate_mode() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Navigate;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b".to_vec(),
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
    }

    #[test]
    fn semantic_client_input_events_route_through_app_input() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::Onboarding;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Enter,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::Settings);
        assert_eq!(
            server.app.state.settings.section,
            crate::app::state::SettingsSection::Integrations
        );
    }

    #[test]
    fn semantic_client_escape_closes_keybind_help() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::KeybindHelp;
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Esc,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::Navigate);
    }

    #[test]
    fn semantic_client_down_scrolls_keybind_help() {
        let mut server = test_headless_server();
        server.app.state.mode = crate::app::Mode::KeybindHelp;
        server.clients.insert(
            1,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        assert!(server.app.state.keybind_help_max_scroll() > 0);
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Down,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));

        assert_eq!(server.app.state.mode, crate::app::Mode::KeybindHelp);
        assert_eq!(server.app.state.keybind_help.scroll, 1);
    }

    #[tokio::test]
    async fn split_default_background_response_updates_theme_without_forwarding_tail() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let focused = workspace.focused_pane_id().unwrap();
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        workspace.tabs[0].runtimes.insert(focused, runtime);
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let _ = server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b]".to_vec(),
        });
        assert!(rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"11;#123456\x07".to_vec(),
        }));

        assert!(rx.try_recv().is_err());
        assert_eq!(
            server.clients[&1].host_terminal_theme.background,
            Some(crate::terminal_theme::RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );
        assert_eq!(
            server.app.state.host_terminal_theme.background,
            Some(crate::terminal_theme::RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            })
        );
    }

    #[tokio::test]
    async fn background_app_frame_does_not_render_or_clear_foreground_hover() {
        let mut server = test_headless_server();
        let _input_rx = install_focused_test_runtime(&mut server, b"https://example.com/hover");
        let (foreground_tx, _foreground_control_rx, foreground_rx) = test_client_writer();
        let (background_tx, _background_control_rx, background_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (44, 20),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                2,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let pane = server.app.state.view.pane_infos[0].clone();
        assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: pane.inner_rect.x + 8,
                row: pane.inner_rect.y,
                modifiers: 0,
            }],
        }));
        let foreground_hover = server.app.state.hovered_link.clone();

        server.render_and_stream();

        let background_frame = read_server_frame(background_rx.recv().expect("background frame"));
        let _foreground_frame = read_server_frame(foreground_rx.recv().expect("foreground frame"));
        assert!(!background_frame.cells.iter().any(|cell| {
            cell.symbol == "h" && cell.modifier & ratatui::style::Modifier::UNDERLINED.bits() != 0
        }));
        assert_eq!(server.app.state.hovered_link, foreground_hover);
    }

    #[tokio::test]
    async fn render_and_stream_uses_each_client_terminal_size() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let active_pane = workspace.tabs[0].root_pane;
        let background_tab = workspace.test_add_tab(Some("background"));
        let background_pane = workspace.tabs[background_tab].root_pane;
        workspace.tabs[0].runtimes.insert(
            active_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"active"),
        );
        workspace.tabs[background_tab].runtimes.insert(
            background_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"background"),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (desktop_tx, _desktop_control_rx, desktop_rx) = test_client_writer();
        let (mobile_tx, _mobile_control_rx, mobile_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(desktop_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (44, 20),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(mobile_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        server.render_and_stream();

        let desktop_frame = read_server_frame(desktop_rx.recv().expect("desktop frame"));
        let mobile_frame = read_server_frame(mobile_rx.recv().expect("mobile frame"));

        assert_eq!((desktop_frame.width, desktop_frame.height), (120, 40));
        assert_eq!((mobile_frame.width, mobile_frame.height), (44, 20));
        let mobile_text = frame_text(&mobile_frame);
        let mut mobile_rows = mobile_text.lines();
        let mobile_header = mobile_rows.by_ref().take(2).collect::<String>();
        let mobile_surface = mobile_rows.collect::<String>();
        assert!(mobile_header.contains("test"), "header: {mobile_header:?}");
        assert!(
            mobile_surface.contains("active"),
            "surface: {mobile_surface:?}"
        );
        assert!(!mobile_surface.contains("background"));

        let foreground_terminal_area = Rect::new(26, 1, 94, 39);
        let expected_pane_size = (
            foreground_terminal_area.height,
            foreground_terminal_area.width.saturating_sub(1),
        );
        assert_eq!(
            server.app.state.view.layout,
            crate::app::state::ViewLayout::Desktop
        );
        assert_eq!(server.app.state.view.mobile_header_rect, Rect::default());
        assert_eq!(
            server.app.state.view.terminal_area,
            foreground_terminal_area
        );
        assert_eq!(
            server.app.state.workspaces[0].tabs[0].runtimes[&active_pane].current_size(),
            expected_pane_size
        );
        assert_eq!(
            server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
                .current_size(),
            expected_pane_size
        );
    }

    #[tokio::test]
    async fn resize_shared_runtime_resizes_background_tabs() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let background_tab = workspace.test_add_tab(Some("background"));
        let active_pane = workspace.tabs[0].root_pane;
        let background_pane = workspace.tabs[background_tab].root_pane;
        workspace.tabs[0].runtimes.insert(
            active_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        workspace.tabs[background_tab].runtimes.insert(
            background_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        let terminal_area = server.app.state.view.terminal_area;
        let expected = (terminal_area.height, terminal_area.width.saturating_sub(1));
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane(&server.app.terminal_runtimes, active_pane)
                .unwrap()
                .current_size(),
            expected
        );
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane(&server.app.terminal_runtimes, background_pane)
                .unwrap()
                .current_size(),
            expected
        );
    }

    #[test]
    fn terminal_attach_disconnect_restores_app_pane_size() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let _runtime_guard = rt.enter();
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
        let terminal_id_string = terminal_id.to_string();
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.terminal_runtimes.insert(
            terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        let expected_app_size = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size();
        assert_ne!(expected_app_size, (24, 80));

        let (writer, _control_rx, _render_rx) = test_client_writer();
        assert!(server.handle_server_event(ServerEvent::ClientConnected {
            client_id: 2,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            render_encoding: RenderEncoding::TerminalAnsi,
            keybindings: None,
            direct_attach_requested: true,
            direct_graphics: false,
            omp_pane: false,
            display_name: None,
            frontend_profile_id: None,
            renderer_binding_token: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            writer,
        }));
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 2,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );
        assert_eq!(server.foreground_client_id, Some(1));
        assert!(server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size(),
            (24, 80)
        );

        assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));

        assert!(!server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size(),
            expected_app_size
        );
        drop(server);
        drop(_runtime_guard);
        rt.shutdown_timeout(Duration::from_millis(100));
    }

    #[test]
    fn render_and_stream_sends_terminal_frame_for_terminal_ansi_client() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();

        match read_server_message(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("terminal frame"),
        ) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 1);
                assert_eq!((frame.width, frame.height), (80, 24));
                assert!(frame.full);
                assert!(!frame.bytes.is_empty());
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
    }

    #[test]
    fn render_and_stream_sends_large_terminal_frame_for_terminal_ansi_client() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (278, 85),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        server.render_and_stream();
        match read_server_message(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial terminal frame"),
        ) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 1);
                assert_eq!((frame.width, frame.height), (278, 85));
                assert!(frame.full);
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 710,
            rows: 202,
            cell_width_px: 0,
            cell_height_px: 0,
        }));
        server.render_and_stream();

        match read_server_message(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("large terminal frame"),
        ) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 2);
                assert_eq!((frame.width, frame.height), (710, 202));
                assert!(frame.full);
                assert!(!frame.bytes.is_empty());
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }

        server.app.state.mode = crate::app::Mode::Navigate;
        server.render_and_stream();
        match read_server_message(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("follow-up terminal frame"),
        ) {
            ServerMessage::Terminal(frame) => assert_eq!(frame.seq, 3),
            other => panic!("expected terminal frame, got {other:?}"),
        }
    }

    #[test]
    fn terminal_ansi_input_does_not_reset_blit_baseline() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );

        assert!(!server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: Vec::new(),
        }));
        server.render_and_stream();

        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn outer_focus_gained_repaints_terminal_ansi_without_clearing() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        server.render_and_stream();

        match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
            ServerMessage::Terminal(frame) => {
                assert_eq!(frame.seq, 2);
                assert!(frame.full);
                assert!(!frame.bytes.windows(4).any(|bytes| bytes == b"\x1b[2J"));
            }
            other => panic!("expected terminal frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn outer_focus_gained_client_render_pending_survives_semantic_render_queue_full() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial semantic frame");

        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        server.clients[&1]
            .writer
            .as_ref()
            .unwrap()
            .test_fill_render(queued);

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );

        server.render_and_stream();

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
        server.render_and_stream();

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::Frame(_)
        ));
    }

    #[test]
    fn outer_focus_gained_does_not_force_terminal_ansi_full_redraw_when_disabled() {
        let mut server = test_headless_server();
        server.app.state.redraw_on_focus_gained = false;
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame");

        server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        });
        server.render_and_stream();

        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
    }

    #[test]
    fn outer_focus_gained_does_not_mark_semantic_render_pending_when_disabled() {
        let mut server = test_headless_server();
        server.app.state.redraw_on_focus_gained = false;
        let (client_tx, _client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        assert!(server.handle_server_event(ServerEvent::ClientInput {
            client_id: 1,
            data: b"\x1b[I".to_vec(),
        }));

        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
        assert!(!server.app.full_redraw_pending);
        assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    }

    #[test]
    fn full_render_queue_does_not_advance_terminal_ansi_baseline() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        client_tx.test_fill_render(queued);

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();

        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            0
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn writer_drained_retries_pending_terminal_ansi_render() {
        let mut server = test_headless_server();
        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        client_tx.test_fill_render(queued);

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        server.render_and_stream();
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
        server.render_and_stream();

        match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
            ServerMessage::Terminal(frame) => assert_eq!(frame.seq, 1),
            other => panic!("expected terminal frame, got {other:?}"),
        }
        assert_eq!(
            server
                .clients
                .get(&1)
                .unwrap()
                .render_state
                .terminal_seq()
                .unwrap(),
            1
        );
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::None
        );
    }

    #[test]
    fn render_and_stream_skips_identical_frame_sends() {
        let mut server = test_headless_server();
        server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, _client_control_rx, client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();

        server.render_and_stream();
        let first = client_rx.recv_timeout(Duration::from_millis(100));
        assert!(first.is_ok(), "expected first frame to be sent");

        server.render_and_stream();
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "identical frame should not be sent twice"
        );
    }

    #[test]
    fn visible_source_wakes_pending_hidden_work() {
        let (server, background_pane) = hidden_pty_visibility_test_server(&[(120, 40)]);
        let visible_pane = server.app.state.workspaces[0].tabs[0].root_pane;
        server.sync_immediate_pty_sources();

        assert!(server.app.render_dirty.request_pty(background_pane));
        assert!(!server.has_pending_presentation_work(false, false));
        assert!(server.app.render_dirty.request_pty(visible_pane));
        assert!(server.has_pending_presentation_work(false, false));
    }

    #[test]
    fn inactive_tab_pty_source_is_hidden_until_tab_focus() {
        let (server, background_pane) = hidden_pty_visibility_test_server(&[]);
        let sources = HashSet::from([background_pane]);
        assert!(!server.pty_sources_visible_to_any_render_target(&sources));

        let (mut server, background_pane) =
            hidden_pty_visibility_test_server(&[(120, 40), (44, 20)]);
        let sources = HashSet::from([background_pane]);
        assert!(!server.pty_sources_visible_to_any_render_target(&sources));

        server.app.state.workspaces[0].switch_tab(1);
        assert!(server.pty_sources_visible_to_any_render_target(&sources));
    }

    #[tokio::test]
    async fn hidden_pty_output_appears_after_switching_to_its_tab() {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let background_tab = workspace.test_add_tab(Some("background"));
        let background_pane = workspace.tabs[background_tab].root_pane;
        workspace.insert_test_runtime(
            background_pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"before"),
        );
        server.app.state.workspaces = vec![workspace];
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, _client_control_rx, client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        server.render_and_stream();
        let _initial_frame = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, background_pane)
            .expect("background runtime");
        runtime.test_process_pty_bytes(b"\rhidden-update");
        assert!(server.app.render_dirty.request_pty(background_pane));
        let request = server.app.render_dirty.take();
        let pty = if server.pty_sources_visible_to_any_render_target(&request.pty_sources) {
            PtyRenderState::Visible
        } else {
            PtyRenderState::Hidden
        };
        assert_eq!(
            retained_render_plan(RetainedRenderInput {
                needs_full_render: false,
                needs_graphics_render: false,
                pty,
            }),
            RetainedRenderPlan::HiddenPty
        );
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.app.state.workspaces[0].switch_tab(background_tab);
        server.render_and_stream();
        let visible_frame = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("frame after tab switch"),
        );
        assert!(frame_text(&visible_frame).contains("hidden-update"));
    }

    #[test]
    fn direct_terminal_observer_keeps_hidden_pty_source_renderable_with_app_client() {
        let (mut server, background_pane) = hidden_pty_visibility_test_server(&[(120, 40)]);
        assert!(!server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));

        let terminal_id = server.app.state.workspaces[0]
            .terminal_id(background_pane)
            .expect("background terminal id")
            .to_string();
        let (client_tx, _client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            2,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalObserve { terminal_id },
                None,
                None,
                None,
                None,
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                false,
                Some(client_tx),
            ),
        );

        assert!(server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));

        let hidden_pane = server.app.state.workspaces[0].tabs[0].root_pane;
        server.sync_immediate_pty_sources();
        assert!(server.app.render_dirty.request_pty(background_pane));
        assert!(server.has_pending_presentation_work(false, false));
        assert!(server.app.render_dirty.request_pty(hidden_pane));
    }

    #[tokio::test]
    async fn retained_pty_update_streams_dirty_row_from_last_frame() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.render_and_stream();
        let first = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        assert!(first.cells.iter().any(|cell| cell.symbol == "a"));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame"),
        );
        assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
        assert_eq!((patched.width, patched.height), (80, 24));
    }

    #[tokio::test]
    async fn hovered_plain_url_dirty_row_falls_back_to_full_render() {
        let url = "https://example.com/link";
        let initial = format!("{url} old");
        let update = format!("\r{url} new");
        let (mut server, client_rx, pane_id) = retained_test_server(initial.as_bytes());
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let pane = server.app.state.view.pane_infos[0].clone();
        server.app.state.hovered_link = Some(crate::app::HoveredPaneLink {
            pane_id,
            inner_rect: pane.inner_rect,
            cells: (0..url.len() as u16)
                .map(|col| (pane.inner_rect.x + col, pane.inner_rect.y))
                .collect(),
        });
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime")
            .test_process_pty_bytes(update.as_bytes());

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.render_and_stream();
        let refreshed = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full hover frame"),
        );
        let h = refreshed
            .cells
            .iter()
            .find(|cell| cell.symbol == "h")
            .expect("URL cell");
        assert_ne!(h.modifier & ratatui::style::Modifier::UNDERLINED.bits(), 0);
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_popup_is_visible() {
        let (mut server, client_rx, _) = retained_test_server(b"tiled");
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup-aaaa");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial popup frame"),
        );
        assert!(frame_text(&initial).contains("popup-aaaa"));
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        server.render_and_stream();
        let updated = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full popup fallback frame"),
        );
        assert!(frame_text(&updated).contains("Zopup-aaaa"));
    }

    #[tokio::test]
    async fn popup_forces_host_mouse_capture_for_headless_client() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.mouse_capture = false;
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup");
        server.app.install_test_popup_runtime(popup_runtime);

        server.stream_host_mouse_capture_mode();

        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("mouse capture message")
            ),
            ServerMessage::MouseCapture { enabled: true, .. }
        ));
    }

    #[tokio::test]
    async fn command_mode_updates_headless_client_keyboard_flags() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );

        server.app.state.mode = crate::app::Mode::Prefix;
        server.stream_host_keyboard_enhancement_flags();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("command-mode keyboard enhancement message")
            ),
            ServerMessage::KittyKeyboardReportAll { enabled: true }
        ));

        server.app.state.mode = crate::app::Mode::Terminal;
        server.stream_host_keyboard_enhancement_flags();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("IME-compatible keyboard enhancement message")
            ),
            ServerMessage::KittyKeyboardReportAll { enabled: false }
        ));
    }

    #[tokio::test]
    async fn focused_report_all_pane_updates_headless_client_keyboard_flags() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"\x1b[>15u");
        server.app.install_test_popup_runtime(popup_runtime);

        server.stream_host_keyboard_enhancement_flags();

        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("keyboard enhancement message")
            ),
            ServerMessage::KittyKeyboardReportAll { enabled: true }
        ));

        assert!(server.app.close_popup_pane());
        server.app.state.mode = crate::app::Mode::Terminal;
        server.stream_host_keyboard_enhancement_flags();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("IME-compatible keyboard enhancement message")
            ),
            ServerMessage::KittyKeyboardReportAll { enabled: false }
        ));
    }

    #[tokio::test]
    async fn virtual_render_uses_popup_cursor() {
        let (mut server, _client_rx, _) = retained_test_server(b"\x1b[2;2H");
        let popup_runtime =
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"\x1b[4;5H");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

        let (_, cursor) = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            ratatui::layout::Rect::new(0, 0, 80, 24),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let (_, inner) =
            crate::ui::popup_pane_rects(&server.app.state, server.app.state.view.terminal_area)
                .unwrap();
        let expected = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .cursor_state(inner, true)
            .unwrap();

        assert_eq!(
            cursor,
            Some(crate::protocol::CursorState {
                x: expected.x,
                y: expected.y,
                visible: expected.visible,
                shape: expected.shape,
            })
        );
    }

    #[tokio::test]
    async fn virtual_render_does_not_resize_directly_attached_popup() {
        let (mut server, _client_rx, _) = retained_test_server(b"tiled");
        let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(50, 13, b"");
        let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);
        server
            .app
            .state
            .direct_attach_resize_locks
            .insert(terminal_id.clone());

        let _ = crate::server::render_stream::render_virtual_with_runtime_registry(
            &mut server.app.state,
            &server.app.terminal_runtimes,
            ratatui::layout::Rect::new(0, 0, 80, 24),
            true,
            crate::kitty_graphics::HostCellSize::default(),
        );

        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .current_size(),
            (13, 50)
        );
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_toast_is_visible() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: "pi needs attention".to_owned(),
            context: "background · 2".to_owned(),
            position: None,
            target: None,
        });
        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        assert!(
            frame_text(&initial).contains("pi needs attention"),
            "expected initial full frame to include toast text"
        );

        let toast_row = server.app.state.view.toast_hit_area.y;
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let pane_row = toast_row
            .checked_sub(inner_rect.y)
            .expect("toast should overlap the pane")
            + 1;
        assert!(pane_row <= inner_rect.height);
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "retained path should not stream a frame that can overwrite toast cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_declines_while_copy_feedback_is_visible() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.copy_feedback = Some(crate::app::state::CopyFeedback {
            message: "copied to clipboard".to_owned(),
        });
        server.render_and_stream();
        let initial = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("initial frame"),
        );
        let initial_text = frame_text(&initial);
        assert!(
            initial_text.contains("copied to clipboard"),
            "expected initial full frame to include copy feedback"
        );

        let feedback_row = initial_text
            .lines()
            .position(|line| line.contains("copied to clipboard"))
            .expect("copy feedback row") as u16;
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let pane_row = feedback_row
            .checked_sub(inner_rect.y)
            .expect("copy feedback should overlap the pane")
            + 1;
        assert!(pane_row <= inner_rect.height);
        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(
            client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "retained path should not stream a frame that can overwrite copy feedback cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_matches_full_render_frame() {
        let initial = b"\x1b[6 qleft \xe4\xb8\xad";
        let update = b"\r\x1b[44mZ\x1b[0m";
        let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
        let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

        retained_server.render_and_stream();
        let _ = retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial retained baseline");
        full_server.render_and_stream();
        let _ = full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial full baseline");

        retained_server
            .app
            .state
            .runtime_for_pane_in_workspace(
                &retained_server.app.terminal_runtimes,
                0,
                retained_pane_id,
            )
            .expect("retained runtime")
            .test_process_pty_bytes(update);
        full_server
            .app
            .state
            .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
            .expect("full runtime")
            .test_process_pty_bytes(update);

        assert!(retained_server.render_retained_pty_update_and_stream());
        full_server.render_and_stream();

        let retained_frame = read_server_frame(
            retained_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame"),
        );
        let full_frame = read_server_frame(
            full_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full frame"),
        );
        assert_frame_data_eq(&retained_frame, &full_frame);
    }

    #[tokio::test]
    async fn retained_pty_update_streams_cursor_only_change() {
        let initial = b"abcd";
        let update = b"\x1b[D";
        let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
        let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

        retained_server.render_and_stream();
        let _ = retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial retained baseline");
        full_server.render_and_stream();
        let _ = full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial full baseline");

        retained_server
            .app
            .state
            .runtime_for_pane_in_workspace(
                &retained_server.app.terminal_runtimes,
                0,
                retained_pane_id,
            )
            .expect("retained runtime")
            .test_process_pty_bytes(update);
        full_server
            .app
            .state
            .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
            .expect("full runtime")
            .test_process_pty_bytes(update);

        assert!(retained_server.render_retained_pty_update_and_stream());
        full_server.render_and_stream();

        let retained_frame = read_server_frame(
            retained_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained cursor frame"),
        );
        let full_frame = read_server_frame(
            full_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full cursor frame"),
        );
        assert_frame_data_eq(&retained_frame, &full_frame);
    }

    #[tokio::test]
    async fn retained_pty_update_declines_unsafe_mode_without_consuming_dirty_rows() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        server.app.state.mode = crate::app::Mode::Navigate;
        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.app.state.mode = crate::app::Mode::Terminal;
        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame after safe mode"),
        );
        assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
    }

    #[tokio::test]
    async fn headless_full_render_clears_full_redraw_pending_for_future_retained_updates() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.full_redraw_pending = true;

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full redraw frame");
        assert!(!server.app.full_redraw_pending);

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
    }

    #[tokio::test]
    async fn retained_pty_update_declines_when_patch_would_stale_hyperlinks() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"link");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");
        let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
        let client = server.clients.get_mut(&1).unwrap();
        let mut frame = client.render_state.last_frame().unwrap().clone();
        frame.hyperlinks = vec!["https://example.com".to_owned()];
        let hyperlink_idx =
            usize::from(inner_rect.y) * usize::from(frame.width) + usize::from(inner_rect.x);
        frame.cells[hyperlink_idx].hyperlink = Some(0);
        let prepared = client
            .render_state
            .prepare_frame(frame)
            .expect("hyperlink frame differs");
        client.render_state.commit_sent_frame(prepared);

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rplain");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

        server.render_and_stream();
        let full = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("full frame after hyperlink overwrite"),
        );
        assert!(
            full.cells.iter().all(|cell| cell.hyperlink.is_none()),
            "full render should clear overwritten hyperlink cells"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_allows_dirty_row_that_creates_plain_url() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"plain");
        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rhttps://example.com/new");

        assert!(server.render_retained_pty_update_and_stream());
        let patched = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame after plain URL"),
        );
        assert!(
            patched.hyperlinks.is_empty(),
            "retained render should not synthesize plain URL hyperlink metadata"
        );
    }

    #[tokio::test]
    async fn retained_pty_update_allows_kitty_enabled_empty_graphics_cache() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.kitty_graphics_enabled = true;
        server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(server.render_retained_pty_update_and_stream());
        let retained = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("retained frame with kitty enabled"),
        );
        assert!(retained.cells.iter().any(|cell| cell.symbol == "Z"));
    }

    #[tokio::test]
    async fn retained_pty_update_declines_when_graphics_cache_has_content() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        server.app.state.kitty_graphics_enabled = true;
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };

        server.render_and_stream();
        let _ = client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame");
        server
            .clients
            .get_mut(&1)
            .unwrap()
            .graphics_cache
            .test_mark_non_empty();

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[tokio::test]
    async fn full_redraw_pending_survives_full_render_queue_full() {
        let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
        let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
            .expect("serialize dummy message");
        server.clients[&1]
            .writer
            .as_ref()
            .unwrap()
            .test_fill_render(queued);
        server.app.full_redraw_pending = true;

        server.render_and_stream();

        assert!(server.app.full_redraw_pending);
        assert_eq!(
            server.clients.get(&1).unwrap().deferred_render(),
            DeferredRender::Full
        );
        assert!(matches!(
            read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
            ServerMessage::ReloadSoundConfig
        ));

        let runtime = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("runtime");
        runtime.test_process_pty_bytes(b"\rZ");

        assert!(!server.render_retained_pty_update_and_stream());
        assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn client_config_reload_request_refreshes_attached_clients() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.request_client_config_reload = true;

        server.drain_client_config_reload_request();

        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("client config reload message"),
        ) {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected ReloadSoundConfig, got {other:?}"),
        }
        assert!(!server.app.state.request_client_config_reload);
    }

    #[test]
    fn terminal_bell_targets_foreground_client_only() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("terminal-bell");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);

        let changed = server
            .handle_internal_event_with_forwarding(AppEvent::TerminalBell { pane_id, count: 3 });

        assert!(!changed);
        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground terminal bell message"),
        ) {
            ServerMessage::TerminalBell { count } => assert_eq!(count, 3),
            other => panic!("expected terminal bell message, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive terminal bells"
        );

        server.foreground_client_id = None;
        server.handle_internal_event_with_forwarding(AppEvent::TerminalBell { pane_id, count: 1 });
        assert!(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "bells without a foreground client must not be retained"
        );
    }
    #[test]
    fn open_url_targets_originating_client_without_changing_foreground() {
        let mut server = test_headless_server();
        let (origin_tx, origin_control_rx, _origin_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(origin_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);

        let changed = server.handle_internal_event_with_forwarding(AppEvent::OpenUrl {
            url: "https://example.com/issues/21".into(),
            source_id: 1,
        });

        assert!(!changed);
        assert_eq!(server.foreground_client_id, Some(2));
        match read_server_message(
            origin_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("originating client open URL message"),
        ) {
            ServerMessage::OpenUrl { url } => assert_eq!(url, "https://example.com/issues/21"),
            other => panic!("expected OpenUrl message, got {other:?}"),
        }
        assert!(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "non-originating client should not receive URL opens"
        );
        server.handle_internal_event_with_forwarding(AppEvent::OpenUrl {
            url: "file:///tmp/example.rs".into(),
            source_id: 1,
        });
        assert!(
            origin_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "server must not forward file URLs to the originating client"
        );
        assert!(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "server must not forward file URLs to another client"
        );
    }

    #[test]
    fn clipboard_write_targets_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert_eq!(
            server
                .app
                .state
                .copy_feedback
                .as_ref()
                .map(|feedback| feedback.message.as_str()),
            Some("copied to clipboard")
        );
        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground clipboard message"),
        ) {
            ServerMessage::Clipboard { data } => assert_eq!(data, "dGVzdA=="),
            other => panic!("expected clipboard message, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive clipboard writes"
        );
    }

    #[tokio::test]
    async fn private_clipboard_write_targets_private_surface_owner() {
        let mut server = test_headless_server();
        let (owner_tx, owner_control_rx, _owner_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
        let mut owner = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(owner_tx),
        );
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(crate::layout::PaneId::from_raw(1)),
                b"private",
            ),
        );
        let private_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneClipboardWrite {
                pane_id: private_pane_id,
                content: b"private".to_vec(),
            }),
            "private clipboard writes should not change shared visual state"
        );
        assert!(
            server.app.state.copy_feedback.is_none(),
            "private clipboard writes must not set shared copy feedback"
        );

        match read_server_message(
            owner_control_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
        ) {
            ServerMessage::Clipboard { data } => assert_eq!(data, "cHJpdmF0ZQ=="),
            other => panic!("expected clipboard message, got {other:?}"),
        }
        assert!(foreground_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
    }

    #[test]
    fn clipboard_write_without_foreground_client_does_not_show_feedback() {
        let mut server = test_headless_server();
        server.foreground_client_id = None;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert!(
            server.app.state.copy_feedback.is_none(),
            "clipboard feedback should only show when a foreground client can receive the write"
        );
    }

    #[test]
    fn clipboard_write_failed_foreground_send_does_not_show_feedback() {
        let mut server = test_headless_server();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
        drop(foreground_control_rx);
        foreground_tx.test_close();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(1);

        let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
            content: b"test".to_vec(),
        });

        assert!(changed);
        assert!(
            server.app.state.copy_feedback.is_none(),
            "clipboard feedback should only show after the foreground client receives the write"
        );
        assert!(
            !server.clients.contains_key(&1),
            "failed targeted send should remove the broken foreground client"
        );
    }

    #[test]
    fn prefix_input_source_targets_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        // Drain any setup messages (e.g. mouse-capture sync) before exercising the event.
        while foreground_control_rx
            .recv_timeout(Duration::from_millis(20))
            .is_ok()
        {}

        let changed = server
            .handle_internal_event_with_forwarding(AppEvent::PrefixInputSource { active: true });

        assert!(changed);
        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground prefix input-source message"),
        ) {
            ServerMessage::PrefixInputSource { active } => assert!(active),
            other => panic!("expected prefix input-source message, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive prefix input-source changes"
        );
    }

    #[test]
    fn headless_app_keeps_prefix_input_source_switch_off_process() {
        // An App-internal drain (e.g. the exhaustive drain at the top of
        // handle_api_request) can consume a queued PrefixInputSource intent
        // before the forwarding drain sees it. The headless App must treat the
        // event as inert instead of switching the host input source from the
        // server process.
        struct CountingPrefixInputSource(std::rc::Rc<std::cell::Cell<usize>>);
        impl crate::platform::PrefixInputSource for CountingPrefixInputSource {
            fn switch_to_ascii(&mut self) {
                self.0.set(self.0.get() + 1);
            }
            fn restore(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let mut server = test_headless_server();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        server
            .app
            .set_prefix_input_source(Box::new(CountingPrefixInputSource(calls.clone())));

        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: true });
        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: false });
        assert_eq!(
            calls.get(),
            0,
            "headless server must not apply the host input-source switch"
        );

        // Sanity: the same event does apply once the flag is on (monolithic semantics).
        server.app.local_input_source_switch = true;
        server
            .app
            .handle_internal_event(AppEvent::PrefixInputSource { active: true });
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn client_local_notifications_target_foreground_client_only() {
        let mut server = test_headless_server();
        let (background_tx, background_control_rx, _background_rx) = test_client_writer();
        let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(background_tx),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_tx),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        assert!(server.send_to_foreground_client(ServerMessage::Notify {
            kind: protocol::NotifyKind::Toast,
            message: "pi finished".to_string(),
            body: Some("workspace 1".to_string()),
            activation: None,
        }));

        match read_server_message(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("foreground toast message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Toast);
                assert_eq!(message, "pi finished");
                assert_eq!(body.as_deref(), Some("workspace 1"));
                assert!(activation.is_none());
            }
            other => panic!("expected toast notify, got {other:?}"),
        }
        assert!(
            background_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "background client should not receive client-local notifications"
        );
    }

    #[test]
    fn oversized_paste_rejection_notifies_only_the_sending_client() {
        let mut server = test_headless_server();
        let (sender_writer, sender_control_rx, _sender_render_rx) = test_client_writer();
        let (foreground_writer, foreground_control_rx, _foreground_render_rx) =
            test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (120, 40),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(sender_writer),
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(foreground_writer),
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        assert!(
            !server.handle_server_event(ServerEvent::ClientPasteRejected {
                client_id: 1,
                size: 5_000_012,
                max: 1_048_576,
            })
        );

        match read_server_message(
            sender_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("sending client rejection notification"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Toast);
                assert_eq!(message, "Paste rejected");
                assert_eq!(
                    body.as_deref(),
                    Some("Input message is 5000012 bytes; Herdr's limit is 1048576 bytes")
                );
                assert!(activation.is_none());
            }
            other => panic!("expected paste rejection notification, got {other:?}"),
        }
        assert!(
            foreground_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "foreground client must not receive another client's rejection"
        );
        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.clients.len(), 2);
        assert!(server.app.state.toast.is_none());
    }

    #[test]
    fn herdr_toast_delivery_keeps_toast_in_frame_without_client_notify() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
            version: "9.9.9".to_string(),
            install_command: "herdr update".into(),
        });

        assert!(changed);
        assert!(server.app.state.toast.is_some());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "herdr delivery should render in-frame instead of forwarding a client-local notification"
        );
    }

    #[test]
    fn hybrid_unfocused_update_forwards_system_notify_kind() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;

        let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
            version: "9.9.9".to_string(),
            install_command: "herdr update".into(),
        });

        assert!(changed);
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("system toast message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "v9.9.9 available");
                assert_eq!(
                    body.as_deref(),
                    Some("detach, run `herdr update`, then follow its restart guidance")
                );
                assert!(activation.is_none());
            }
            other => panic!("expected system toast notify, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_hybrid_unfocused_forwards_system_notification() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: Some("api workspace".into()),
                        position: Some(crate::config::ToastHerdrPosition::TopLeft),
                        sound: api::schema::NotificationShowSound::Request,
                    },
                ),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api notification message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api sound message"),
        );

        match first {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "build failed");
                assert_eq!(body.as_deref(), Some("api workspace"));
                assert!(activation.is_none());
            }
            other => panic!("expected api notification, got {other:?}"),
        }
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Sound);
                assert_eq!(message, "agent attention");
                assert!(body.is_none());
                assert!(activation.is_none());
            }
            other => panic!("expected api sound, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_preserves_colon_in_forwarded_title() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build: failed".into(),
                        body: Some("api workspace".into()),
                        position: None,
                        sound: api::schema::NotificationShowSound::None,
                    },
                ),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api notification message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "build: failed");
                assert_eq!(body.as_deref(), Some("api workspace"));
                assert!(activation.is_none());
            }
            other => panic!("expected api notification, got {other:?}"),
        }
    }

    #[test]
    fn notification_show_api_validates_empty_title_before_disabled_delivery() {
        let mut server = test_headless_server();
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Off;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "\n\t".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::None,
                    },
                ),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.error.code, "invalid_params");
        assert_eq!(parsed.error.message, "notification title is empty");
    }

    #[test]
    fn notification_show_api_reports_no_foreground_client() {
        let mut server = test_headless_server();
        server.foreground_client_id = None;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::Request,
                    },
                ),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        assert!(changed);
        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: false,
                reason: api::schema::NotificationShowReason::NoForegroundClient,
            }
        );
    }

    #[test]
    fn notification_show_api_herdr_toast_expires_headless() {
        let mut server = test_headless_server();
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "notify".into(),
                    method: api::schema::Method::NotificationShow(
                        api::schema::NotificationShowParams {
                            title: "build failed".into(),
                            body: None,
                            position: None,
                            sound: api::schema::NotificationShowSound::None,
                        },
                    ),
                },
                context: api::ApiRequestContext::default(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            })
        );

        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        let deadline = server.app.toast_deadline.expect("api toast deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));
        assert!(server.app.state.toast.is_none());
        assert!(server.app.toast_deadline.is_none());
    }

    #[test]
    fn notification_show_api_hybrid_focused_stays_in_frame() {
        let mut server = test_headless_server();
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();

        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(
            server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "notify".into(),
                    method: api::schema::Method::NotificationShow(
                        api::schema::NotificationShowParams {
                            title: "build failed".into(),
                            body: None,
                            position: None,
                            sound: api::schema::NotificationShowSound::Done,
                        },
                    ),
                },
                context: api::ApiRequestContext::default(),
                respond_to,
                response_write_complete: None,
                stream_active: None,
            })
        );

        let response = response_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            parsed.result,
            api::schema::ResponseResult::NotificationShow {
                shown: true,
                reason: api::schema::NotificationShowReason::Shown,
            }
        );
        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("api sound message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::Sound);
                assert_eq!(message, "agent done");
                assert!(body.is_none());
                assert!(activation.is_none());
            }
            other => panic!("expected api sound, got {other:?}"),
        }
    }

    #[test]
    fn startup_idle_does_not_forward_completion() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("active");
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        server.app.state.toast_config.delay_seconds = 0;
        server.app.state.sound.enabled = true;

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
                pane_id,
                agent: crate::detect::Agent::Pi,
                observed_at: Instant::now(),
            })
        );

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        while client_control_rx
            .recv_timeout(Duration::from_millis(20))
            .is_ok()
        {}

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Idle,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            })
        );
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "startup readiness should not forward a completion notification"
        );
    }

    #[test]
    fn delayed_hybrid_unfocused_agent_notification_forwards_system_toast() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let background_id = background.id.clone();
        let pane_id = background.tabs[0].root_pane;
        let foreground = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![background, foreground];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;
        server.app.state.toast_config.delay_seconds = 1;
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Blocked,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now()
            })
        );
        assert!(server.app.state.toast.is_none());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "delayed transition should not notify immediately"
        );
        let deadline = server
            .app
            .state
            .next_pending_agent_notification_deadline()
            .expect("pending notification deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));
        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed sound message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed toast message"),
        );
        assert!(matches!(
            first,
            ServerMessage::Notify {
                kind: protocol::NotifyKind::Sound,
                activation: None,
                ..
            }
        ));
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "pi needs attention");
                assert_eq!(body.as_deref(), Some("background · 1"));
                assert_eq!(
                    activation,
                    Some(protocol::NotificationActivation {
                        recipient_client_id: 1,
                        workspace_id: background_id,
                        pane_id: pane_id.raw()
                    })
                );
            }
            other => panic!("expected delayed system toast, got {other:?}"),
        }
        assert!(server.app.state.pending_agent_notifications.is_empty());
    }

    #[test]
    fn immediate_hybrid_unfocused_agent_notification_carries_activation_target() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let background_id = background.id.clone();
        let pane_id = background.tabs[0].root_pane;
        server.app.state.workspaces = vec![
            background,
            crate::workspace::Workspace::test_new("foreground"),
        ];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;
        server.app.state.toast_config.delay_seconds = 0;
        server.app.state.sound.enabled = false;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Blocked,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            })
        );

        match read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("immediate system toast message"),
        ) {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "pi needs attention");
                assert_eq!(body.as_deref(), Some("background · 1"));
                assert_eq!(
                    activation,
                    Some(protocol::NotificationActivation {
                        recipient_client_id: 1,
                        workspace_id: background_id,
                        pane_id: pane_id.raw(),
                    })
                );
            }
            other => panic!("expected immediate system toast, got {other:?}"),
        }
    }

    #[test]
    fn hybrid_focused_agent_notification_stays_in_frame() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let pane_id = background.tabs[0].root_pane;
        let foreground = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![background, foreground];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::Hybrid;
        server.app.state.toast_config.delay_seconds = 0;
        server.app.state.sound.enabled = false;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(true),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Blocked,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            })
        );
        assert!(server.app.state.toast.is_some());
        assert!(client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
    }

    #[test]
    fn delayed_active_tab_unfocused_agent_notification_forwards_after_deadline() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("active");
        let workspace_id = workspace.id.clone();
        let pane_id = workspace.tabs[0].root_pane;
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        server.app.state.toast_config.delay_seconds = 1;
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(false),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
                pane_id,
                agent: Some(crate::detect::Agent::Pi),
                state: crate::detect::AgentState::Blocked,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now()
            })
        );
        assert!(server.app.state.toast.is_none());
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "delayed transition should not notify immediately"
        );
        let deadline = server
            .app
            .state
            .next_pending_agent_notification_deadline()
            .expect("pending notification deadline");
        assert!(server.handle_scheduled_tasks_headless(deadline, false));
        let first = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed sound message"),
        );
        let second = read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("delayed toast message"),
        );
        assert!(matches!(
            first,
            ServerMessage::Notify {
                kind: protocol::NotifyKind::Sound,
                activation: None,
                ..
            }
        ));
        match second {
            ServerMessage::Notify {
                kind,
                message,
                body,
                activation,
            } => {
                assert_eq!(kind, protocol::NotifyKind::SystemToast);
                assert_eq!(message, "pi needs attention");
                assert_eq!(body.as_deref(), Some("active · 1"));
                assert_eq!(
                    activation,
                    Some(protocol::NotificationActivation {
                        recipient_client_id: 1,
                        workspace_id,
                        pane_id: pane_id.raw()
                    })
                );
            }
            other => panic!("expected delayed system toast, got {other:?}"),
        }
    }

    #[test]
    fn stale_api_agent_report_does_not_forward_done_sound() {
        let mut server = test_headless_server();
        let background = crate::workspace::Workspace::test_new("background");
        let pane_id = background.tabs[0].root_pane;
        let public_pane_id = format!("{}:p1", background.id);
        let foreground = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![background, foreground];
        server.app.state.ensure_test_terminals();
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:pi".into(),
                agent: "pi".into(),
                session_ref: crate::agent_resume::AgentSessionRef::path(
                    std::env::current_dir()
                        .unwrap()
                        .join("headless-pi-session.jsonl")
                        .display()
                        .to_string(),
                )
                .unwrap(),
            });
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_hook_authority(
                "herdr:pi".into(),
                "pi".into(),
                crate::detect::AgentState::Working,
                None,
                Some(20),
            );
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        server.app.state.mode = crate::app::Mode::Terminal;

        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();

        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "stale".into(),
                method: api::schema::Method::PaneReportAgent(api::schema::PaneReportAgentParams {
                    pane_id: public_pane_id,
                    source: "herdr:pi".into(),
                    agent: "pi".into(),
                    state: api::schema::PaneAgentState::Idle,
                    message: None,
                    seq: Some(19),
                    agent_session_id: None,
                    agent_session_path: None,
                }),
            },
            context: api::ApiRequestContext::default(),
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });

        assert!(changed);
        assert!(response_rx.recv_timeout(Duration::from_millis(100)).is_ok());
        assert_eq!(
            server.app.state.terminals.get(&terminal_id).unwrap().state,
            crate::detect::AgentState::Working
        );
        assert!(
            client_control_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "stale idle report must not forward a done sound"
        );
    }
    #[test]
    fn reconnecting_full_app_client_gets_a_distinct_view_id() {
        let first = test_app_client(None, 1).view_id.expect("first view id");
        let second = test_app_client(None, 2).view_id.expect("second view id");

        assert_ne!(first, second);
    }

    #[test]
    fn private_popup_requires_a_view_id() {
        let mut server = test_headless_server();

        assert_eq!(
            private_popup_error_code(&mut server, None),
            "view_id_required"
        );
    }

    #[test]
    fn private_popup_rejects_an_unknown_view_id() {
        let mut server = test_headless_server();
        let unknown = crate::api::schema::ViewId::from_opaque("view_unknown").unwrap();

        assert_eq!(
            private_popup_error_code(&mut server, Some(unknown)),
            "view_not_found"
        );
    }

    #[test]
    fn private_popup_rejects_a_stale_disconnected_view_id() {
        let mut server = test_headless_server();
        let (writer, _control_rx, _render_rx) = test_client_writer();
        let client = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(writer),
        );
        let stale = client.view_id.clone().expect("connected view id");
        server.clients.insert(1, client);
        assert_eq!(server.clients[&1].view_id.as_ref(), Some(&stale));
        server.clients.remove(&1);

        assert_eq!(
            private_popup_error_code(&mut server, Some(stale)),
            "view_not_found"
        );
    }

    #[test]
    fn untargeted_private_popup_uses_owner_navigation_source() {
        let mut server = test_headless_server();
        let owner_workspace = crate::workspace::Workspace::test_new("owner");
        let owner_pane_id = owner_workspace.tabs[0].root_pane;
        let owner_workspace_id = owner_workspace.id.clone();
        let foreground_workspace = crate::workspace::Workspace::test_new("foreground");
        let foreground_workspace_id = foreground_workspace.id.clone();
        server.app.state.workspaces = vec![owner_workspace, foreground_workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let owner_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);
        let mut owner = test_app_client(None, 1);
        owner.navigation = Some(owner_navigation);
        server.clients.insert(1, owner);
        let mut foreground = test_app_client(None, 2);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);

        let plugin: crate::api::schema::InstalledPluginInfo =
            serde_json::from_value(serde_json::json!({
                "plugin_id": "test.private",
                "name": "Private test",
                "version": "1.0.0",
                "manifest_path": "/tmp/test.private/herdr-plugin.toml",
                "plugin_root": "/tmp/test.private",
                "enabled": true,
                "panes": [{
                    "id": "popup",
                    "title": "Private test",
                    "placement": "popup",
                    "scope": "client_private",
                    "command": ["true"]
                }]
            }))
            .unwrap();
        server
            .app
            .state
            .installed_plugins
            .insert(plugin.plugin_id.clone(), plugin);
        let spec = server
            .client_private_plugin_popup_spec_for_owner(1, &test_private_popup_params(None))
            .expect("private popup spec");

        assert_eq!(
            spec.origin,
            ClientPrivatePluginPopupOrigin::Pane(owner_pane_id)
        );
        assert_eq!(
            server.app.private_popup_source_pane_id(spec.origin),
            Some(format!("{owner_workspace_id}:p1"))
        );
        assert_eq!(
            server.app.state.active,
            server
                .app
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == foreground_workspace_id)
        );
    }

    #[test]
    fn private_popup_geometry_uses_owner_terminal_area() {
        let mut server = test_headless_server();
        let owner_workspace = crate::workspace::Workspace::test_new("owner");
        let foreground_workspace = crate::workspace::Workspace::test_new("foreground");
        server.app.state.workspaces = vec![owner_workspace, foreground_workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;
        let owner_navigation = ClientNavigationState::capture(&server.app.state);
        server.app.state.active = Some(1);
        server.app.state.selected = 1;
        let foreground_navigation = ClientNavigationState::capture(&server.app.state);

        let mut owner = test_app_client(None, 1);
        owner.terminal_size = (120, 40);
        owner.navigation = Some(owner_navigation);
        server.clients.insert(1, owner);
        let mut foreground = test_app_client(None, 2);
        foreground.navigation = Some(foreground_navigation);
        server.clients.insert(2, foreground);
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();

        let area = server
            .private_popup_terminal_area_for_owner(1)
            .expect("owner terminal area");

        assert_eq!(area, Rect::new(26, 1, 94, 39));
        assert_ne!(area, Rect::new(0, 0, 120, 40));
        assert_eq!(server.app.state.active, Some(1));
        assert_eq!(server.effective_size, (80, 24));
    }

    #[tokio::test]
    async fn opening_private_popup_releases_shared_terminal_input_leases() {
        let mut server = test_headless_server();
        let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[>15u");
        server.app.state.ensure_test_terminals();
        let (writer, _control_rx, _render_rx) = test_client_writer();
        let mut client = test_app_client(Some(true), 1);
        client.writer = Some(writer);
        server.clients.insert(1, client);
        server.foreground_client_id = Some(1);
        server.sync_foreground_client_state();
        let view_id = server.clients[&1]
            .view_id
            .clone()
            .expect("connected app view");
        let plugin: crate::api::schema::InstalledPluginInfo =
            serde_json::from_value(serde_json::json!({
                "plugin_id": "test.private",
                "name": "Private test",
                "version": "1.0.0",
                "manifest_path": "/tmp/test.private/herdr-plugin.toml",
                "plugin_root": "/tmp/test.private",
                "enabled": true,
                "panes": [{
                    "id": "popup",
                    "title": "Private test",
                    "placement": "popup",
                    "scope": "client_private",
                    "command": ["true"]
                }]
            }))
            .unwrap();
        server
            .app
            .state
            .installed_plugins
            .insert(plugin.plugin_id.clone(), plugin);

        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id: 1,
            events: vec![crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('j'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            }],
        }));
        assert_eq!(
            input_rx.try_recv().expect("forwarded shared press"),
            Bytes::from_static(b"\x1b[106;1:1u")
        );
        assert!(!server.app.input_leases.is_empty());

        let (response, changed) = server.handle_client_private_plugin_pane_open(
            "private-input-epoch".into(),
            test_private_popup_params(Some(view_id)),
        );

        assert!(changed, "response={response}");
        assert_eq!(
            input_rx
                .try_recv()
                .expect("synthetic shared release before private routing"),
            Bytes::from_static(b"\x1b[106;1:3u")
        );
        assert!(server.app.input_leases.is_empty());
        assert!(server.clients[&1].private_surface.is_some());
        server.close_private_surface(1);
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn failed_private_popup_replacement_keeps_the_existing_surface() {
        let mut server = test_headless_server();
        let workspace = crate::workspace::Workspace::test_new("source");
        let source_pane = workspace.tabs[0].root_pane;
        let source_pane_id = format!("{}:p1", workspace.id);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        server.app.state.mode = crate::app::Mode::Terminal;

        let plugin: crate::api::schema::InstalledPluginInfo =
            serde_json::from_value(serde_json::json!({
                "plugin_id": "test.private",
                "name": "Private test",
                "version": "1.0.0",
                "manifest_path": "/tmp/test.private/herdr-plugin.toml",
                "plugin_root": "/tmp/test.private",
                "enabled": true,
                "panes": [{
                    "id": "popup",
                    "title": "Private test",
                    "placement": "popup",
                    "scope": "client_private",
                    "command": ["true"]
                }]
            }))
            .unwrap();
        server
            .app
            .state
            .installed_plugins
            .insert(plugin.plugin_id.clone(), plugin);

        let (writer, _control_rx, _render_rx) = test_client_writer();
        let mut client = ClientConnection::new(
            (1, 1),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(writer),
        );
        let view_id = client.view_id.clone().expect("connected view id");
        client.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(source_pane),
                b"existing",
            ),
        );
        let existing_pane_id = client.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, client);

        let mut params = test_private_popup_params(Some(view_id));
        params.target_pane_id = Some(source_pane_id);
        let (response, changed) =
            server.handle_client_private_plugin_pane_open("replace-private".to_string(), params);
        let response = serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
            .expect("private popup launch error");

        assert!(!changed);
        assert_eq!(response.error.code, "plugin_pane_open_failed");
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(existing_pane_id)
        );
    }

    #[tokio::test]
    async fn remote_private_popup_replacement_promotes_only_after_ready() {
        let mut server = test_headless_server();
        let source_pane = crate::layout::PaneId::from_raw(41);
        let mut owner = test_app_client(None, 1);
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(source_pane),
                b"existing",
            ),
        );
        let existing_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);
        let candidate = crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
            Rect::new(0, 0, 80, 24),
            ClientPrivatePluginPopupOrigin::Pane(source_pane),
            b"candidate",
        );
        let candidate_pane_id = candidate.pane_id();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let pending = PendingPrivateSurfaceResponse {
            id: "remote-open".into(),
            respond_to,
        };

        assert!(server.install_private_surface_with_response(1, candidate, true, Some(pending)));
        assert!(
            response_rx.try_recv().is_err(),
            "remote open must wait for readiness"
        );
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(existing_pane_id)
        );
        assert_eq!(
            server
                .private_surface_candidates
                .get(&1)
                .map(|candidate| candidate.surface.pane_id()),
            Some(candidate_pane_id)
        );

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::RemoteExecutionReady {
                pane_id: candidate_pane_id,
                child_pid: 42,
                hostname: Some("remote.example".into()),
                cwd: Some("/remote".into()),
            })
        );
        let response = response_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("remote readiness response");
        let response = serde_json::from_str::<crate::api::schema::SuccessResponse>(&response)
            .expect("remote readiness success");
        assert_eq!(response.id, "remote-open");
        assert!(server.private_surface_candidates.is_empty());
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(candidate_pane_id)
        );
        assert!(server.retired_private_pane_ids.contains(&existing_pane_id));
    }
    #[tokio::test]
    async fn active_private_popup_death_preserves_pending_replacement() {
        let mut server = test_headless_server();
        let source_pane = crate::layout::PaneId::from_raw(42);
        let mut owner = test_app_client(None, 1);
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(source_pane),
                b"existing",
            ),
        );
        let existing_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);
        let candidate = crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
            Rect::new(0, 0, 80, 24),
            ClientPrivatePluginPopupOrigin::Pane(source_pane),
            b"candidate",
        );
        let candidate_pane_id = candidate.pane_id();

        assert!(server.install_private_surface(1, candidate, true));
        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id: existing_pane_id,
                child_pid: None,
            })
        );

        assert!(server.clients[&1].private_surface.is_none());
        assert_eq!(
            server
                .private_surface_candidates
                .get(&1)
                .map(|candidate| candidate.surface.pane_id()),
            Some(candidate_pane_id)
        );
        assert!(server.retired_private_pane_ids.contains(&existing_pane_id));
        assert!(!server.retired_private_pane_ids.contains(&candidate_pane_id));

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::RemoteExecutionReady {
                pane_id: candidate_pane_id,
                child_pid: 42,
                hostname: Some("remote.example".into()),
                cwd: Some("/remote".into()),
            })
        );
        assert!(server.private_surface_candidates.is_empty());
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(candidate_pane_id)
        );
    }

    #[tokio::test]
    async fn remote_private_popup_candidate_death_keeps_existing_surface() {
        let mut server = test_headless_server();
        let source_pane = crate::layout::PaneId::from_raw(42);
        let mut owner = test_app_client(None, 1);
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(source_pane),
                b"existing",
            ),
        );
        let existing_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);
        let candidate = crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
            Rect::new(0, 0, 80, 24),
            ClientPrivatePluginPopupOrigin::Pane(source_pane),
            b"candidate",
        );
        let candidate_pane_id = candidate.pane_id();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let pending = PendingPrivateSurfaceResponse {
            id: "remote-open".into(),
            respond_to,
        };

        assert!(server.install_private_surface_with_response(1, candidate, true, Some(pending)));
        assert!(
            response_rx.try_recv().is_err(),
            "remote open must wait for readiness"
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id: candidate_pane_id,
                child_pid: None,
            })
        );
        let response = response_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("pre-ready remote failure response");
        let response = serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
            .expect("pre-ready remote failure error");
        assert_eq!(response.id, "remote-open");
        assert_eq!(response.error.code, "plugin_pane_open_failed");
        assert!(server.private_surface_candidates.is_empty());
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(existing_pane_id)
        );
        assert!(server.retired_private_pane_ids.contains(&candidate_pane_id));
        assert!(!server.retired_private_pane_ids.contains(&existing_pane_id));

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::RemoteExecutionReady {
                pane_id: candidate_pane_id,
                child_pid: 42,
                hostname: Some("remote.example".into()),
                cwd: Some("/remote".into()),
            })
        );
        assert_eq!(
            server.clients[&1]
                .private_surface
                .as_ref()
                .map(|surface| surface.pane_id()),
            Some(existing_pane_id)
        );
    }

    #[tokio::test]
    async fn private_pane_died_removes_only_the_owner_surface() {
        let mut server = test_headless_server();
        let mut owner = test_app_client(None, 1);
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(crate::layout::PaneId::from_raw(2)),
                b"private",
            ),
        );
        let private_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);
        server.clients.insert(2, test_app_client(None, 2));
        let shared_counts = (
            server.app.state.workspaces.len(),
            server.app.state.terminals.len(),
        );

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id: private_pane_id,
                child_pid: None,
            })
        );

        assert!(server.clients[&1].private_surface.is_none());
        assert!(server.clients.contains_key(&2));
        assert_eq!(
            (
                server.app.state.workspaces.len(),
                server.app.state.terminals.len(),
            ),
            shared_counts,
        );
    }

    #[tokio::test]
    async fn foreground_private_pane_death_resynchronizes_shared_runtime() {
        let (mut server, _render_rx, pane_id) = retained_test_server(b"shared");
        let private_pane_id = {
            let client = server.clients.get_mut(&1).expect("foreground owner");
            client.private_surface = Some(
                crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                    Rect::new(0, 0, 80, 24),
                    ClientPrivatePluginPopupOrigin::Pane(pane_id),
                    b"private",
                ),
            );
            client.private_surface.as_ref().unwrap().pane_id()
        };
        let initial_shared_size = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .expect("shared runtime")
            .current_size();

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 120,
            rows: 40,
            cell_width_px: 0,
            cell_height_px: 0,
        }));
        server.render_and_stream();
        assert_eq!(server.effective_size, (80, 24));
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
                .expect("shared runtime")
                .current_size(),
            initial_shared_size,
        );

        assert!(
            server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                pane_id: private_pane_id,
                child_pid: None,
            })
        );

        let terminal_area = server.app.state.view.terminal_area;
        assert_eq!(server.effective_size, (120, 40));
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
                .expect("resized shared runtime")
                .current_size(),
            (terminal_area.height, terminal_area.width.saturating_sub(1),),
        );
        shutdown_test_runtimes(&mut server);
    }

    #[tokio::test]
    async fn disconnected_private_pane_died_event_is_consumed() {
        let mut server = test_headless_server();
        let mut owner = test_app_client(None, 1);
        owner.private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(crate::layout::PaneId::from_raw(3)),
                b"private",
            ),
        );
        let private_pane_id = owner.private_surface.as_ref().unwrap().pane_id();
        server.clients.insert(1, owner);

        server.remove_client(1);
        assert!(server.retired_private_pane_ids.contains(&private_pane_id));

        for _ in 0..2 {
            assert!(
                !server.handle_internal_event_with_forwarding(AppEvent::PaneDied {
                    pane_id: private_pane_id,
                    child_pid: None,
                })
            );
        }
        assert!(server.retired_private_pane_ids.contains(&private_pane_id));
    }

    #[test]
    fn retired_private_pane_ids_are_bounded_and_reclaim_oldest() {
        let mut server = test_headless_server();
        for raw in 1..=RETIRED_PRIVATE_PANE_ID_LIMIT as u32 + 1 {
            server.retire_private_pane_id(crate::layout::PaneId::from_raw(raw));
        }

        assert_eq!(
            server.retired_private_pane_ids.len(),
            RETIRED_PRIVATE_PANE_ID_LIMIT
        );
        assert!(!server
            .retired_private_pane_ids
            .contains(&crate::layout::PaneId::from_raw(1)));
        assert!(server
            .retired_private_pane_ids
            .contains(&crate::layout::PaneId::from_raw(
                RETIRED_PRIVATE_PANE_ID_LIMIT as u32 + 1
            )));
    }
    #[test]
    fn evicted_private_pane_side_effects_do_not_reach_the_foreground_client() {
        let mut server = test_headless_server();
        let (writer, control_rx, _render_rx) = test_client_writer();
        server
            .clients
            .insert(1, test_identity_client(Some("Ada"), Some(writer)));
        server.foreground_client_id = Some(1);
        let old_private_pane = crate::layout::PaneId::from_raw(u32::MAX);
        server.retire_private_pane_id(old_private_pane);
        for raw in 1..=RETIRED_PRIVATE_PANE_ID_LIMIT as u32 {
            server.retire_private_pane_id(crate::layout::PaneId::from_raw(raw));
        }
        assert!(!server.retired_private_pane_ids.contains(&old_private_pane));

        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
                pane_id: old_private_pane,
                count: 1,
            })
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::PaneClipboardWrite {
                pane_id: old_private_pane,
                content: b"private".to_vec(),
            })
        );
        assert!(
            !server.handle_internal_event_with_forwarding(AppEvent::TerminalCwdReported {
                pane_id: old_private_pane,
                cwd: std::path::PathBuf::from("/private"),
            })
        );

        assert!(control_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn private_link_click_closes_surface_after_origin_retires() {
        let (mut server, _render_rx, _) = retained_test_server(b"shared");
        let area = server.app.state.view.terminal_area;
        let surface = crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
            area,
            ClientPrivatePluginPopupOrigin::Pane(crate::layout::PaneId::from_raw(u32::MAX)),
            b"\x1b]8;;https://example.com/stale\x1b\\open\x1b]8;;\x1b\\",
        );
        let private_pane_id = surface.pane_id();
        let ((column, row), _, _) = surface
            .visible_hyperlinks(area)
            .into_iter()
            .next()
            .expect("private hyperlink");
        server.clients.get_mut(&1).unwrap().private_surface = Some(surface);

        assert!(server.handle_private_surface_input_events(
            1,
            vec![crate::raw_input::RawInputEvent::Mouse(
                crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                }
            )],
        ));
        assert!(server.clients[&1].private_surface.is_none());
        assert!(server.retired_private_pane_ids.contains(&private_pane_id));
    }

    #[tokio::test]
    async fn private_resize_does_not_promote_or_resize_shared_runtime() {
        let (mut server, _render_rx, pane_id) = retained_test_server(b"shared");
        server.clients.insert(2, test_app_client(None, 2));
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        server.resize_shared_runtime_to_effective_size();
        server.clients.get_mut(&1).unwrap().private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(pane_id),
                b"private",
            ),
        );
        let shared_size = server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .unwrap()
            .current_size();
        let effective_size = server.effective_size;

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
        }));
        server.render_and_stream();

        assert_eq!(server.foreground_client_id, Some(2));
        assert_eq!(server.effective_size, effective_size);
        assert_eq!(
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
                .unwrap()
                .current_size(),
            shared_size,
        );
        let surface = server.clients[&1].private_surface.as_ref().unwrap();
        let render_area = surface.render_area_for_test();
        assert_ne!(render_area, Rect::new(0, 0, 100, 30));
        let expected = crate::popup_size::resolve_popup_geometry(None, None, render_area)
            .unwrap()
            .inner;
        assert_eq!(
            surface.runtime_size_for_test(),
            Some((expected.height, expected.width)),
        );
    }

    #[tokio::test]
    async fn private_render_cursor_and_hyperlinks_are_owner_only() {
        let (mut server, owner_rx, pane_id) = retained_test_server(b"shared");
        let (observer_tx, _observer_control_rx, observer_rx) = test_client_writer();
        server.clients.insert(
            2,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                2,
                RenderEncoding::SemanticFrame,
                Some(observer_tx),
            ),
        );
        server.clients.get_mut(&1).unwrap().private_surface = Some(
            crate::server::private_surface::PrivateSurface::test_with_screen_bytes(
                Rect::new(0, 0, 80, 24),
                ClientPrivatePluginPopupOrigin::Pane(pane_id),
                b"\x1b[3;4H\x1b]8;;file:///tmp/private.txt\x1b\\private\x1b]8;;\x1b\\",
            ),
        );

        server.render_and_stream();

        let owner = read_server_frame(owner_rx.recv_timeout(Duration::from_millis(100)).unwrap());
        let observer = read_server_frame(
            observer_rx
                .recv_timeout(Duration::from_millis(100))
                .unwrap(),
        );
        assert!(owner
            .hyperlinks
            .iter()
            .any(|url| url == "file:///tmp/private.txt"));
        assert!(!observer
            .hyperlinks
            .iter()
            .any(|url| url == "file:///tmp/private.txt"));
        let expected_cursor = server.clients[&1]
            .private_surface
            .as_ref()
            .unwrap()
            .cursor(server.app.state.view.terminal_area);
        assert_eq!(owner.cursor, expected_cursor);
        assert_ne!(observer.cursor, expected_cursor);
    }

    /// Verify that no direct calls to `self.app.handle_internal_event`
    /// (or its `handle_internal_event_with_prefix_sync` wrapper) exist
    /// outside of `handle_internal_event_with_forwarding` in this
    /// module. This ensures the forwarding bypass cannot be reintroduced.
    ///
    /// The search pattern looks for `handle_internal_event` calls that
    /// are NOT inside the `handle_internal_event_with_forwarding` method.
    #[test]
    fn no_handle_internal_event_bypass_in_module() {
        let source = include_str!("headless.rs");

        // Find all lines containing handle_internal_event
        let mut bypass_lines: Vec<String> = Vec::new();
        let mut inside_forwarding_method = false;
        let mut forwarding_method_brace_depth = 0u32;

        for (i, line) in source.lines().enumerate() {
            let line_num = i + 1;

            // Track when we're inside handle_internal_event_with_forwarding
            if line.contains("fn handle_internal_event_with_forwarding") {
                inside_forwarding_method = true;
                forwarding_method_brace_depth = 0;
            }

            if inside_forwarding_method {
                // Count braces to track when we exit the method
                for ch in line.chars() {
                    match ch {
                        '{' => forwarding_method_brace_depth += 1,
                        '}' => {
                            forwarding_method_brace_depth =
                                forwarding_method_brace_depth.saturating_sub(1);
                            if forwarding_method_brace_depth == 0 {
                                inside_forwarding_method = false;
                            }
                        }
                        _ => {}
                    }
                }
            } else if (line.contains("self.app.handle_internal_event(")
                || line.contains("self.app.handle_internal_event_with_prefix_sync("))
                && !line.trim().starts_with("///")
                && !line.contains("contains(")
            {
                // Direct call to handle_internal_event outside the forwarding method
                bypass_lines.push(format!("line {}: {}", line_num, line.trim()));
            }
        }

        assert!(
            bypass_lines.is_empty(),
            "Found direct calls to self.app.handle_internal_event outside \
             handle_internal_event_with_forwarding (bypass risk):\n  {}",
            bypass_lines.join("\n  ")
        );
    }
}
