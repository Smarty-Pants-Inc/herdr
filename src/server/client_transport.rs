//! Blocking client socket transport for the headless server.
//!
//! This module owns the thin-client handshake, read loop, and writer loop.
//! It converts socket I/O into [`ServerEvent`] values consumed by
//! `HeadlessServer`.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::ipc::LocalStream;
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientInputEvent, ClientKeybindings,
    ClientLaunchMode, ClientMessage, NotificationActivation, RenderEncoding, ServerMessage,
    MAX_CLIPBOARD_IMAGE_PAYLOAD, MAX_FRAME_SIZE, MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};

/// Minimum accepted attached client size.
///
/// Narrow observers must be allowed to drive narrow renders, otherwise the
/// server wraps pane content against a wider width and the client sees the
/// right edge clipped.
const MIN_CLIENT_COLS: u16 = 1;
const MIN_CLIENT_ROWS: u16 = 1;

/// How long to wait for a client handshake before closing the connection.
/// Set to 4 seconds (rather than 5) to guarantee the connection is closed
/// within the 5-second deadline, even with OS timer slack, thread scheduling,
/// and cleanup overhead.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// Maximum input payload size (bytes) for a single `ClientMessage::Input`.
const MAX_INPUT_PAYLOAD: usize = 1024 * 1024; // 1 MB
/// Maximum structured input events accepted in one client message.
const MAX_INPUT_EVENT_BATCH: usize = 4096;
/// Maximum encoded mouse report accepted with pixel geometry.
const MAX_PIXEL_MOUSE_PAYLOAD: usize = 128;
/// Maximum reliable control records buffered per client writer.
const CONTROL_QUEUE_CAPACITY: usize = 64;

/// Channels owned by the server side of a client writer thread.
#[derive(Clone, Debug)]
pub(crate) struct ClientWriter {
    /// Reliable control messages such as shutdown, notifications, and clipboard writes.
    /// Capacity is bounded so semantic messages can fail closed for a stalled client.
    pub(crate) control: ClientControlWriter,
    /// Droppable render messages. Capacity is one so slow clients cannot build lag.
    pub(crate) render: ClientRenderWriter,
}

impl ClientWriter {
    pub(crate) fn replace_with_cleanup(&self, data: Vec<u8>) -> bool {
        self.render.queue.replace_with_cleanup(data).is_ok()
    }

    pub(crate) fn replace_with_pane_cleanup(
        &self,
        pane_id: crate::layout::PaneId,
        data: Vec<u8>,
    ) -> bool {
        self.render
            .queue
            .replace_with_pane_cleanup(pane_id, data)
            .is_ok()
    }

    #[cfg(test)]
    pub(crate) fn test_fill_render(&self, data: Vec<u8>) {
        self.render.try_send(data).unwrap();
    }

    #[cfg(test)]
    pub(crate) fn test_close(&self) {
        self.render.queue.close_writer();
    }

    #[cfg(test)]
    pub(crate) fn test_backpressured() -> Self {
        let queue = ClientWriterQueue::new();
        Self {
            control: ClientControlWriter::queue(queue.clone()),
            render: ClientRenderWriter::queue(queue),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_fill_control(&self, data: Vec<u8>) {
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            self.control.send(data.clone()).unwrap();
        }
    }

    #[cfg(test)]
    pub(crate) fn test_pop_control(&self) -> Option<Vec<u8>> {
        self.control.queue.lock_state().control.pop_front()
    }

    #[cfg(test)]
    pub(crate) fn test_control_records(&self) -> Vec<Vec<u8>> {
        self.control
            .queue
            .lock_state()
            .control
            .iter()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn test_has_render_records(&self) -> bool {
        let state = self.control.queue.lock_state();
        !state.ordered.is_empty() || state.render.is_some()
    }

    #[cfg(test)]
    pub(crate) fn test_channel(
        control: std::sync::mpsc::Sender<Vec<u8>>,
        render: std::sync::mpsc::SyncSender<Vec<u8>>,
    ) -> Self {
        let queue = ClientWriterQueue::new();
        let drain = queue.clone();
        let control_writer = ClientControlWriter::queue(queue.clone());
        let mut render_writer = ClientRenderWriter::queue(queue);
        render_writer.test_render = Some(render.clone());
        let writer = Self {
            control: control_writer,
            render: render_writer,
        };
        std::thread::spawn(move || {
            while let Some(item) = drain.recv() {
                let sent = match item {
                    ClientWriteItem::Control(data) => control.send(data).is_ok(),
                    ClientWriteItem::Render(data) => render.send(data).is_ok(),
                };
                if !sent {
                    break;
                }
            }
            drain.close_writer();
        });
        writer
    }
}

#[derive(Debug)]
pub(crate) struct ClientControlWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

#[derive(Debug)]
pub(crate) struct ClientRenderWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

macro_rules! writer_handle {
    ($type:ty) => {
        impl Clone for $type {
            fn clone(&self) -> Self {
                self.queue.add_sender();
                Self {
                    queue: self.queue.clone(),
                    #[cfg(test)]
                    test_render: self.test_render.clone(),
                }
            }
        }
        impl Drop for $type {
            fn drop(&mut self) {
                self.queue.remove_sender();
            }
        }
    };
}
writer_handle!(ClientControlWriter);
writer_handle!(ClientRenderWriter);

impl ClientControlWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        self.queue.send_control(data)
    }
}

impl ClientRenderWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn try_send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        #[cfg(test)]
        if let Some(sender) = &self.test_render {
            return sender.try_send(data);
        }
        self.queue.try_send_render(data)
    }

    pub(crate) fn send_ordered(
        &self,
        pane_id: crate::layout::PaneId,
        data: Vec<u8>,
    ) -> Result<(), TrySendError<Vec<u8>>> {
        self.queue.send_ordered(pane_id, data)
    }
}

#[derive(Debug)]
struct ClientWriterQueue {
    state: Mutex<ClientWriterQueueState>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct ClientWriterQueueState {
    control: VecDeque<Vec<u8>>,
    ordered: VecDeque<ClientOrderedRender>,
    render: Option<Vec<u8>>,
    senders: usize,
    writer_alive: bool,
}

#[derive(Debug)]
struct ClientOrderedRender {
    pane_id: Option<crate::layout::PaneId>,
    data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum ClientWriteItem {
    Control(Vec<u8>),
    Render(Vec<u8>),
}

impl ClientWriterQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClientWriterQueueState {
                writer_alive: true,
                ..ClientWriterQueueState::default()
            }),
            ready: Condvar::new(),
        })
    }

    fn add_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_add(1);
    }

    fn remove_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_sub(1);
        self.ready.notify_one();
    }

    fn send_control(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if state.control.len() >= CONTROL_QUEUE_CAPACITY {
            return Err(TrySendError::Full(data));
        }
        state.control.push_back(data);
        self.ready.notify_one();
        Ok(())
    }

    fn try_send_render(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if state.render.is_some() {
            return Err(TrySendError::Full(data));
        }
        state.render = Some(data);
        self.ready.notify_one();
        Ok(())
    }

    fn send_ordered(
        &self,
        pane_id: crate::layout::PaneId,
        data: Vec<u8>,
    ) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if !state.ordered.is_empty() {
            return Err(TrySendError::Full(data));
        }
        if let Some(older) = state.render.take() {
            state.ordered.push_back(ClientOrderedRender {
                pane_id: None,
                data: older,
            });
        }
        state.ordered.push_back(ClientOrderedRender {
            pane_id: Some(pane_id),
            data,
        });
        self.ready.notify_one();
        Ok(())
    }

    fn replace_with_cleanup(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        self.replace_cleanup(data, None)
    }

    fn replace_with_pane_cleanup(
        &self,
        pane_id: crate::layout::PaneId,
        data: Vec<u8>,
    ) -> Result<(), TrySendError<Vec<u8>>> {
        self.replace_cleanup(data, Some(pane_id))
    }

    fn replace_cleanup(
        &self,
        data: Vec<u8>,
        pane_id: Option<crate::layout::PaneId>,
    ) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if state.control.len() >= CONTROL_QUEUE_CAPACITY {
            return Err(TrySendError::Full(data));
        }
        let data = if let Some(pane_id) = pane_id {
            // Full renders commit graphics cache state when enqueued. Preserve their wire
            // payload, plus unrelated ordered work, before deleting the target pane.
            let mut combined = Vec::new();
            for render in state.ordered.drain(..) {
                if render.pane_id != Some(pane_id) {
                    combined.extend(render.data);
                }
            }
            if let Some(render) = state.render.take() {
                combined.extend(render);
            }
            combined.extend(data);
            combined
        } else {
            state.render = None;
            state.ordered.clear();
            data
        };
        state.control.push_back(data);
        self.ready.notify_one();
        Ok(())
    }

    fn recv(&self) -> Option<ClientWriteItem> {
        let mut state = self.lock_state();
        loop {
            if let Some(data) = state.control.pop_front() {
                return Some(ClientWriteItem::Control(data));
            }
            if let Some(render) = state.ordered.pop_front() {
                self.ready.notify_one();
                return Some(ClientWriteItem::Render(render.data));
            }
            if let Some(data) = state.render.take() {
                return Some(ClientWriteItem::Render(data));
            }
            if state.senders == 0 {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close_writer(&self) {
        let mut state = self.lock_state();
        state.writer_alive = false;
        state.render = None;
        state.ordered.clear();
        self.ready.notify_all();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ClientWriterQueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug)]
pub(crate) enum OmpHostAdmission {
    Accepted,
    Rejected { code: String, message: String },
}

/// Internal event sent from client transport threads to the main event loop.
#[derive(Debug)]
pub(crate) enum ServerEvent {
    /// A new client completed the handshake.
    ClientConnected {
        client_id: u64,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        render_encoding: RenderEncoding,
        keybindings: Option<Box<crate::config::LiveKeybindConfig>>,
        direct_attach_requested: bool,
        direct_graphics: bool,
        omp_pane: bool,
        display_name: Option<String>,
        frontend_profile_id: Option<String>,
        renderer_binding_token: Option<String>,
        renderer_capabilities: crate::protocol::OmpRendererCapabilities,
        writer: ClientWriter,
    },
    /// A one-shot system-notification callback selected this target.
    NotificationActivated {
        activation: NotificationActivation,
        respond_to: std::sync::mpsc::Sender<bool>,
    },
    /// The client completed one exact local identity persistence request.
    IdentityPersistenceAck {
        client_id: u64,
        request_id: u64,
        display_name: String,
        success: bool,
        error: Option<String>,
    },
    /// A client sent an input message.
    ClientInput { client_id: u64, data: Vec<u8> },
    /// A client reported the one armed Kitty regular-file response.
    GraphicsTransmissionResult {
        client_id: u64,
        transfer_id: u64,
        image_id: u32,
        success: bool,
    },
    GraphicsTransmissionStarted {
        client_id: u64,
        transfer_id: u64,
        image_id: u32,
    },
    /// One confirmed SGR pixel report with client read-time geometry.
    ClientInputPixels {
        client_id: u64,
        data: Vec<u8>,
        geometry: crate::input::mouse::HostGeometry,
    },
    /// A client sent structured input events.
    ClientInputEvents {
        client_id: u64,
        events: Vec<crate::protocol::ClientInputEvent>,
    },
    /// A fully decoded interactive paste exceeded the text-input limit.
    ClientPasteRejected {
        client_id: u64,
        size: usize,
        max: usize,
    },
    /// A client sent local clipboard image bytes to paste into a remote pane.
    ClientClipboardImage {
        client_id: u64,
        extension: String,
        data: Vec<u8>,
    },
    /// A client requested direct attach to one terminal.
    ClientAttachTerminal {
        client_id: u64,
        terminal_id: String,
        takeover: bool,
    },
    /// A client requested read-only observation of one terminal.
    ClientObserveTerminal { client_id: u64, target: String },
    /// A client requested writable control of one terminal.
    ClientControlTerminal {
        client_id: u64,
        target: String,
        takeover: bool,
    },
    /// A direct terminal attach client requested scrollback movement.
    ClientAttachScroll {
        client_id: u64,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    },
    /// A client sent a resize message.
    ClientResize {
        client_id: u64,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    },
    /// A client detached gracefully.
    ClientDetach { client_id: u64 },
    /// A client connection was lost.
    ClientDisconnected { client_id: u64 },
    /// The App displayed the first frame for an exact client-local renderer launch.
    OmpRendererReady { client_id: u64, launch_id: u64 },
    /// The exact native sideband finalized its initial snapshot or a resync.
    OmpReplicaReady {
        client_id: u64,
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        attachment_epoch: u64,
    },
    /// A background managed OMP companion resolution completed for the server-private fallback.
    OmpPrivateCompanionResolved {
        result: Result<crate::update::OmpExecutable, String>,
    },
    /// A delayed retry may clear one transient private companion resolution failure.
    OmpPrivateCompanionRetry {
        client_id: u64,
        route: crate::server::omp_route::OmpRouteKey,
        retry_id: u64,
    },
    /// A client attached to an OMP logical pane.
    OmpPaneAttach {
        client_id: u64,
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        target_app_client_id: Option<u64>,
        renderer_capabilities: crate::protocol::OmpRendererCapabilities,
        renderer_launch_id: Option<u64>,
        renderer_request: crate::protocol::OmpRendererRequest,
    },
    /// A client detached from one OMP logical pane attachment.
    OmpPaneDetach {
        client_id: u64,
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        attachment_epoch: u64,
    },
    /// A client requested an OMP controller operation or semantic action.
    OmpControl {
        client_id: u64,
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        attachment_epoch: u64,
        action: crate::protocol::OmpControlAction,
    },
    /// An opaque guest-to-host OMP envelope.
    OmpFrame {
        client_id: u64,
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        attachment_epoch: u64,
        frame: Vec<u8>,
    },

    /// A trusted local OMP host became live for one route.
    OmpHostStarted {
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        host_id: u64,
        outbound: std::sync::mpsc::SyncSender<String>,
        socket: std::net::TcpStream,
        admission: std::sync::mpsc::SyncSender<OmpHostAdmission>,
    },
    /// A trusted local OMP host emitted one opaque host-to-guest envelope.
    OmpHostFrame {
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        host_id: u64,
        target_client_id: Option<u64>,
        frame: Vec<u8>,
    },
    /// The trusted local OMP host bridge closed.
    OmpHostStopped {
        pane_id: String,
        omp_session_id: String,
        route_generation: u64,
        host_id: u64,
    },
    /// A client writer popped a control record and can accept another control record.
    ClientWriterControlDrained { client_id: u64 },
    /// A client writer drained its render slot and can accept another render.
    ClientWriterDrained { client_id: u64 },
    /// Ctrl+C or external shutdown signal received.
    QuitSignal,
}

/// Clamp client-reported terminal dimensions to a minimum viable size.
pub(crate) fn clamp_terminal_size(cols: u16, rows: u16) -> (u16, u16) {
    let clamped_cols = cols.max(MIN_CLIENT_COLS);
    let clamped_rows = rows.max(MIN_CLIENT_ROWS);
    (clamped_cols, clamped_rows)
}

fn parse_client_keybindings(
    keybindings: ClientKeybindings,
) -> Result<Option<Box<crate::config::LiveKeybindConfig>>, String> {
    match keybindings {
        ClientKeybindings::Server => Ok(None),
        ClientKeybindings::Local { keys_toml } => {
            let mut config = toml::from_str::<crate::config::Config>(&keys_toml)
                .map_err(|err| format!("invalid client keybindings: {err}"))?;
            config.keys.command.clear();
            Ok(Some(Box::new(crate::config::LiveKeybindConfig {
                prefix: config.prefix_key(),
                keybinds: config.keybinds(),
            })))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEventLimit {
    WithinLimits,
    TooManyEvents,
    PasteTooLarge { size: usize },
    InputPayloadTooLarge { size: usize },
}

fn input_event_limit(events: &[ClientInputEvent]) -> InputEventLimit {
    let mut expanded_events = 0usize;
    let mut paste_bytes = 0usize;
    let mut input_bytes = 0usize;
    for event in events {
        expanded_events = expanded_events.saturating_add(match event {
            ClientInputEvent::Key { repeat_count, .. } => usize::from((*repeat_count).max(1)),
            _ => 1,
        });
        match event {
            ClientInputEvent::Key {
                repeat_count,
                generated_text,
                source,
                ..
            } => {
                if let Some(text) = generated_text {
                    input_bytes = input_bytes.saturating_add(
                        text.len()
                            .saturating_mul(usize::from((*repeat_count).max(1))),
                    );
                }
                if let crate::protocol::ClientKeySource::Vt { bytes } = source {
                    input_bytes = input_bytes.saturating_add(bytes.len());
                }
            }
            ClientInputEvent::TextCommit(text) => {
                input_bytes = input_bytes.saturating_add(text.len());
            }
            ClientInputEvent::Paste { text } => {
                paste_bytes = paste_bytes.saturating_add(text.len());
            }
            ClientInputEvent::Mouse { .. }
            | ClientInputEvent::FocusGained
            | ClientInputEvent::FocusLost => {}
        }
    }

    if expanded_events > MAX_INPUT_EVENT_BATCH {
        return InputEventLimit::TooManyEvents;
    }

    let payload_bytes = paste_bytes.saturating_add(input_bytes);
    if payload_bytes <= MAX_INPUT_PAYLOAD {
        InputEventLimit::WithinLimits
    } else if input_bytes == 0 {
        InputEventLimit::PasteTooLarge {
            size: payload_bytes,
        }
    } else {
        InputEventLimit::InputPayloadTooLarge {
            size: payload_bytes,
        }
    }
}

#[cfg(windows)]
fn set_client_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
    client_id: u64,
) -> io::Result<()> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(client_id, err = %err, context, "client socket receive timeout unavailable");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(not(windows))]
fn set_client_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
    _client_id: u64,
) -> io::Result<()> {
    stream.set_recv_timeout(timeout)
}

/// Handles the client handshake on a blocking thread.
///
/// Reads the `Hello` message, validates the version, sends `Welcome`,
/// and then enters a read loop forwarding messages to the server event channel.
pub(crate) fn handle_client_handshake(
    mut stream: LocalStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    // Reset to blocking mode — the accept loop sets nonblocking but
    // the handshake thread needs blocking I/O for read_message/write_message.
    stream.set_nonblocking(false)?;

    set_client_recv_timeout(
        &stream,
        Some(HANDSHAKE_TIMEOUT),
        "client handshake read timeout unavailable",
        client_id,
    )?;

    // Read the Hello message.
    let hello: ClientMessage = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
        Ok(msg) => msg,
        Err(protocol::FramingError::UnexpectedEof) => {
            debug!(client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(protocol::FramingError::Oversized { claimed, max }) => {
            warn!(client_id, claimed, max, "oversized handshake from client");
            return Ok(());
        }
        Err(err) => {
            debug!(client_id, err = %err, "failed to read client hello");
            return Ok(());
        }
    };

    let (
        client_cols,
        client_rows,
        cell_width_px,
        cell_height_px,
        render_encoding,
        keybindings,
        launch_mode,
        display_name,
        frontend_profile_id,
        renderer_binding_token,
        renderer_capabilities,
    ) = match hello {
        ClientMessage::Hello {
            version,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            requested_encoding,
            keybindings,
            launch_mode,
            display_name,
            frontend_profile_id,
            renderer_binding_token,
            renderer_capabilities,
        } => {
            // Version check.
            match protocol::check_client_version(version) {
                protocol::VersionCheck::Compatible => {}
                protocol::VersionCheck::Incompatible(reason) => {
                    // Send rejection Welcome.
                    let welcome = ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: Some(reason),
                    };
                    let _ = protocol::write_message(&mut stream, &welcome);
                    return Ok(());
                }
            }

            let keybindings = match parse_client_keybindings(keybindings) {
                Ok(keybindings) => keybindings,
                Err(error) => {
                    let welcome = ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: Some(error),
                    };
                    let _ = protocol::write_message(&mut stream, &welcome);
                    return Ok(());
                }
            };
            let (clamped_cols, clamped_rows) = clamp_terminal_size(cols, rows);
            if display_name
                .as_deref()
                .is_some_and(|name| crate::config::validate_display_name(name).is_err())
                || frontend_profile_id.as_deref().is_some_and(|profile_id| {
                    crate::config::validate_frontend_profile_id(profile_id).is_err()
                })
                || renderer_binding_token.as_deref().is_some_and(|token| {
                    crate::config::validate_frontend_profile_id(token).is_err()
                })
                || (launch_mode == ClientLaunchMode::OmpPane
                    && (display_name.is_some()
                        || frontend_profile_id.is_none()
                        || renderer_binding_token.is_none()))
            {
                let _ = protocol::write_message(
                    &mut stream,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: Some("invalid identity handshake metadata".to_owned()),
                    },
                );
                return Ok(());
            }
            let display_name = (launch_mode != ClientLaunchMode::OmpPane)
                .then_some(display_name)
                .flatten();
            let renderer_capabilities = if matches!(
                launch_mode,
                ClientLaunchMode::App | ClientLaunchMode::AppDirectGraphics
            ) && requested_encoding == RenderEncoding::SemanticFrame
            {
                renderer_capabilities
            } else {
                crate::protocol::OmpRendererCapabilities::default()
            };
            (
                clamped_cols,
                clamped_rows,
                cell_width_px,
                cell_height_px,
                requested_encoding,
                keybindings,
                launch_mode,
                display_name,
                frontend_profile_id,
                renderer_binding_token,
                renderer_capabilities,
            )
        }
        _ => {
            // First message must be Hello.
            debug!(client_id, "first message was not Hello, closing");
            let welcome = ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: Some("expected Hello as first message".to_owned()),
            };
            let _ = protocol::write_message(&mut stream, &welcome);
            return Ok(());
        }
    };

    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    // Send Welcome.
    let welcome = ServerMessage::Welcome {
        version: PROTOCOL_VERSION,
        encoding: render_encoding,
        error: None,
    };
    protocol::write_message(&mut stream, &welcome).map_err(|e| io::Error::other(e.to_string()))?;

    let (direct_attach_requested, direct_graphics, omp_pane) = match launch_mode {
        ClientLaunchMode::App => (false, false, false),
        ClientLaunchMode::AppDirectGraphics => (false, true, false),
        ClientLaunchMode::TerminalAttach => (true, false, false),
        ClientLaunchMode::OmpPane => (false, false, true),
        ClientLaunchMode::NotificationActivator => {
            let activation = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
                Ok(ClientMessage::ActivateNotification { activation }) => activation,
                Ok(_) => {
                    debug!(client_id, "notification activator sent unexpected message");
                    return Ok(());
                }
                Err(err) => {
                    debug!(client_id, err = %err, "notification activator did not send activation");
                    return Ok(());
                }
            };
            let (respond_to, response_rx) = std::sync::mpsc::channel();
            if server_event_tx
                .blocking_send(ServerEvent::NotificationActivated {
                    activation,
                    respond_to,
                })
                .is_err()
            {
                debug!(client_id, "notification activation event channel closed");
                return Ok(());
            }
            match response_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
                Ok(activated) => {
                    let response = ServerMessage::NotificationActivationProcessed { activated };
                    if let Err(err) = protocol::write_message(&mut stream, &response) {
                        debug!(client_id, err = %err, "failed to acknowledge notification activation");
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    debug!(
                        client_id,
                        "timed out waiting for notification activation result"
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    debug!(client_id, "notification activation was not processed");
                }
            }
            return Ok(());
        }
    };

    set_client_recv_timeout(
        &stream,
        None,
        "failed to clear client handshake read timeout",
        client_id,
    )?;

    // Create separate channels for reliable control messages and droppable renders.
    let writer_queue = ClientWriterQueue::new();
    let writer = ClientWriter {
        control: ClientControlWriter::queue(writer_queue.clone()),
        render: ClientRenderWriter::queue(writer_queue.clone()),
    };

    // Spawn a writer thread that forwards messages from the channels to the stream.
    let write_stream = stream.try_clone()?;
    let writer_event_tx = server_event_tx.clone();
    std::thread::spawn(move || {
        client_writer_loop(write_stream, client_id, writer_queue, writer_event_tx);
    });

    if should_quit.load(Ordering::Acquire) {
        send_shutdown_to_unregistered_client(&writer);
        return Ok(());
    }

    // Notify the main loop about the new client.
    let connected = ServerEvent::ClientConnected {
        client_id,
        cols: client_cols,
        rows: client_rows,
        cell_width_px,
        cell_height_px,
        render_encoding,
        keybindings,
        direct_attach_requested,
        direct_graphics,
        omp_pane,
        display_name,
        frontend_profile_id,
        renderer_binding_token,
        renderer_capabilities,
        writer,
    };
    if let Err(err) = server_event_tx.blocking_send(connected) {
        if let ServerEvent::ClientConnected { writer, .. } = err.0 {
            send_shutdown_to_unregistered_client(&writer);
        }
    }

    // Enter read loop — read client messages and forward to main loop.
    client_read_loop(stream, client_id, server_event_tx, should_quit)
}

fn send_shutdown_to_unregistered_client(writer: &ClientWriter) {
    let mut framed = Vec::new();
    if protocol::write_message(
        &mut framed,
        &ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        },
    )
    .is_ok()
    {
        let _ = writer.control.send(framed);
    }
}

#[cfg(target_vendor = "apple")]
fn suppress_client_writer_sigpipe(stream: &LocalStream) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let LocalStream::UdSocket(stream) = stream;
    let enabled: libc::c_int = 1;
    let result = unsafe {
        libc::setsockopt(
            stream.inner().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_vendor = "apple"))]
fn suppress_client_writer_sigpipe(_stream: &LocalStream) -> io::Result<()> {
    Ok(())
}

/// The client writer loop — prioritizes control messages over render frames.
fn client_writer_loop(
    mut stream: LocalStream,
    client_id: u64,
    writer_queue: Arc<ClientWriterQueue>,
    server_event_tx: mpsc::Sender<ServerEvent>,
) {
    if let Err(err) = suppress_client_writer_sigpipe(&stream) {
        debug!(err = %err, "failed to suppress client writer SIGPIPE");
        let _ = server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
        writer_queue.close_writer();
        return;
    }
    while let Some(item) = writer_queue.recv() {
        let written = match item {
            ClientWriteItem::Control(data) => {
                let _ = server_event_tx
                    .blocking_send(ServerEvent::ClientWriterControlDrained { client_id });
                write_framed_bytes(&mut stream, &data)
            }
            ClientWriteItem::Render(data) => {
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientWriterDrained { client_id });
                write_framed_bytes(&mut stream, &data)
            }
        };
        if !written {
            let _ = server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
            break;
        }
    }
    writer_queue.close_writer();
    debug!("client writer thread exiting");
}

fn write_framed_bytes(stream: &mut LocalStream, data: &[u8]) -> bool {
    if let Err(err) = stream.write_all(data) {
        debug!(err = %err, "client write failed, closing writer");
        return false;
    }
    if let Err(err) = stream.flush() {
        debug!(err = %err, "client flush failed, closing writer");
        return false;
    }
    true
}

/// The client read loop — reads messages from the client and forwards to the server event channel.
fn client_read_loop(
    mut stream: LocalStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    while !should_quit.load(Ordering::Acquire) {
        let msg: ClientMessage = match protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE)
        {
            Ok(msg) => msg,
            Err(protocol::FramingError::UnexpectedEof) => {
                // Client disconnected.
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    client_id,
                    claimed, max, "oversized message from client, closing"
                );
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(err) => {
                debug!(client_id, err = %err, "client read error, closing");
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
        };

        let event = match msg {
            ClientMessage::Input { data } => {
                // Validate input size.
                if data.len() > MAX_INPUT_PAYLOAD {
                    if crate::raw_input::is_complete_text_bracketed_paste(&data) {
                        warn!(
                            client_id,
                            size = data.len(),
                            max = MAX_INPUT_PAYLOAD,
                            "oversized bracketed paste from client, rejecting"
                        );
                        ServerEvent::ClientPasteRejected {
                            client_id,
                            size: data.len(),
                            max: MAX_INPUT_PAYLOAD,
                        }
                    } else {
                        warn!(
                            client_id,
                            size = data.len(),
                            "oversized input from client, closing"
                        );
                        let _ = server_event_tx
                            .blocking_send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                } else {
                    ServerEvent::ClientInput { client_id, data }
                }
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
                    warn!(
                        client_id,
                        cols,
                        rows,
                        width_px,
                        height_px,
                        "invalid pixel mouse geometry from client, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                };
                if data.len() > MAX_PIXEL_MOUSE_PAYLOAD
                    || crate::input::mouse::parse_report(&data).is_none()
                {
                    warn!(
                        client_id,
                        size = data.len(),
                        max = MAX_PIXEL_MOUSE_PAYLOAD,
                        "invalid pixel mouse report from client, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                ServerEvent::ClientInputPixels {
                    client_id,
                    data,
                    geometry,
                }
            }
            ClientMessage::InputEvents { events } => match input_event_limit(&events) {
                InputEventLimit::WithinLimits => {
                    ServerEvent::ClientInputEvents { client_id, events }
                }
                InputEventLimit::TooManyEvents => {
                    warn!(
                        client_id,
                        count = events.len(),
                        "oversized input event batch from client, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                InputEventLimit::PasteTooLarge { size } => {
                    warn!(
                        client_id,
                        size,
                        max = MAX_INPUT_PAYLOAD,
                        "oversized structured paste from client, rejecting"
                    );
                    ServerEvent::ClientPasteRejected {
                        client_id,
                        size,
                        max: MAX_INPUT_PAYLOAD,
                    }
                }
                InputEventLimit::InputPayloadTooLarge { size } => {
                    warn!(
                        client_id,
                        size,
                        max = MAX_INPUT_PAYLOAD,
                        "oversized structured input payload from client, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
            },
            ClientMessage::ObserveTerminal { target } => {
                ServerEvent::ClientObserveTerminal { client_id, target }
            }
            ClientMessage::ControlTerminal { target, takeover } => {
                ServerEvent::ClientControlTerminal {
                    client_id,
                    target,
                    takeover,
                }
            }
            ClientMessage::GraphicsTransmissionResult {
                transfer_id,
                image_id,
                success,
            } => ServerEvent::GraphicsTransmissionResult {
                client_id,
                transfer_id,
                image_id,
                success,
            },
            ClientMessage::GraphicsTransmissionStarted {
                transfer_id,
                image_id,
            } => ServerEvent::GraphicsTransmissionStarted {
                client_id,
                transfer_id,
                image_id,
            },
            ClientMessage::ClipboardImage { extension, data } => {
                if data.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
                    warn!(
                        client_id,
                        size = data.len(),
                        "oversized clipboard image from client, closing"
                    );
                    let _ = server_event_tx
                        .blocking_send(ServerEvent::ClientDisconnected { client_id });
                    break;
                } else {
                    ServerEvent::ClientClipboardImage {
                        client_id,
                        extension,
                        data,
                    }
                }
            }
            ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            } => {
                let (clamped_cols, clamped_rows) = clamp_terminal_size(cols, rows);
                ServerEvent::ClientResize {
                    client_id,
                    cols: clamped_cols,
                    rows: clamped_rows,
                    cell_width_px,
                    cell_height_px,
                }
            }
            ClientMessage::Detach => ServerEvent::ClientDetach { client_id },
            ClientMessage::AttachTerminal {
                terminal_id,
                takeover,
            } => ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            },
            ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            },
            ClientMessage::ActivateNotification { .. } => {
                warn!(
                    client_id,
                    "registered client sent notification activation, closing"
                );
                let _ =
                    server_event_tx.blocking_send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            ClientMessage::OmpPaneAttach {
                pane_id,
                omp_session_id,
                route_generation,
                target_app_client_id,
                renderer_capabilities,
                renderer_request,
                renderer_launch_id,
            } => ServerEvent::OmpPaneAttach {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                target_app_client_id,
                renderer_capabilities,
                renderer_request,
                renderer_launch_id,
            },
            ClientMessage::OmpPaneDetach {
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
            } => ServerEvent::OmpPaneDetach {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
            },
            ClientMessage::OmpControl {
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                action,
            } => ServerEvent::OmpControl {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                action,
            },
            ClientMessage::OmpFrame {
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                frame,
            } => ServerEvent::OmpFrame {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                frame,
            },
            ClientMessage::OmpRendererReady { launch_id } => ServerEvent::OmpRendererReady {
                client_id,
                launch_id,
            },
            ClientMessage::OmpReplicaReady {
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
            } => ServerEvent::OmpReplicaReady {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
            },
            ClientMessage::IdentityPersistenceAck {
                request_id,
                display_name,
                success,
                error,
            } => ServerEvent::IdentityPersistenceAck {
                client_id,
                request_id,
                display_name,
                success,
                error,
            },
            ClientMessage::Hello { .. } => {
                // Duplicate Hello — ignore.
                continue;
            }
        };

        if server_event_tx.blocking_send(event).is_err() {
            break; // Main loop gone.
        }
    }

    debug!(client_id, "client read thread exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;

    struct TestSocketPath(PathBuf);

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_test_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let filename = format!("h{}-{nanos}.sock", std::process::id());
        #[cfg(unix)]
        {
            let _ = name;
            PathBuf::from("/tmp").join(filename)
        }
        #[cfg(windows)]
        {
            std::env::temp_dir().join(format!("herdr-{name}-{filename}"))
        }
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, TestSocketPath) {
        let path = unique_test_path(name);
        let _ = std::fs::remove_file(&path);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, TestSocketPath(path))
    }

    fn recv_server_event(receiver: &mut mpsc::Receiver<ServerEvent>, context: &str) -> ServerEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match receiver.try_recv() {
                Ok(event) => return event,
                Err(mpsc::error::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => panic!("{context}: {err}"),
            }
        }
    }
    #[test]
    fn omp_attach_carries_exact_app_client_id_to_server_event() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("omp-exact-app-target");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(2);
        let should_quit = Arc::new(AtomicBool::new(false));
        let reader_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &reader_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::OmpPaneAttach {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 1,
                target_app_client_id: Some(42),
                renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                    client_local_native: true,
                },
                renderer_request: crate::protocol::OmpRendererRequest::Independent,
                renderer_launch_id: Some(9),
            },
        )
        .unwrap();

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "OMP attach event"),
            ServerEvent::OmpPaneAttach {
                client_id: 7,
                target_app_client_id: Some(42),
                ..
            }
        ));
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::OmpRendererReady { launch_id: 9 },
        )
        .unwrap();
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "OMP renderer ready event"),
            ServerEvent::OmpRendererReady {
                client_id: 7,
                launch_id: 9,
            }
        ));
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::OmpReplicaReady {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 3,
                attachment_epoch: 4,
            },
        )
        .unwrap();
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "OMP replica ready event"),
            ServerEvent::OmpReplicaReady {
                client_id: 7,
                pane_id,
                omp_session_id,
                route_generation: 3,
                attachment_epoch: 4,
            } if pane_id == "pane" && omp_session_id == "session"
        ));
        drop(client_stream);
        handle.join().unwrap().unwrap();
    }

    fn bracketed_paste_with_total_len(total_len: usize) -> Vec<u8> {
        const DELIMITER_BYTES: usize = b"\x1b[200~".len() + b"\x1b[201~".len();
        assert!(total_len >= DELIMITER_BYTES);
        let mut data = Vec::with_capacity(total_len);
        data.extend_from_slice(b"\x1b[200~");
        data.resize(total_len - b"\x1b[201~".len(), b'x');
        data.extend_from_slice(b"\x1b[201~");
        data
    }

    fn test_queue_writer() -> (ClientWriter, Arc<ClientWriterQueue>) {
        let queue = ClientWriterQueue::new();
        (
            ClientWriter {
                control: ClientControlWriter::queue(queue.clone()),
                render: ClientRenderWriter::queue(queue.clone()),
            },
            queue,
        )
    }

    fn frame_server_message(message: &ServerMessage) -> Vec<u8> {
        let mut bytes = Vec::new();
        protocol::write_message(&mut bytes, message).expect("frame server message");
        bytes
    }

    #[test]
    fn client_writer_queue_keeps_render_slot_bounded() {
        let (writer, _queue) = test_queue_writer();
        let first = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("first".into()),
        });
        let second = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("second".into()),
        });

        writer.render.try_send(first).expect("first render fits");
        assert!(matches!(
            writer.render.try_send(second),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn client_writer_cleanup_replaces_queued_render_and_ordered_work() {
        let (writer, queue) = test_queue_writer();
        let pane_id = crate::layout::PaneId::alloc();
        writer.render.try_send(b"old-render".to_vec()).unwrap();
        writer
            .render
            .send_ordered(pane_id, b"ordered-graphics".to_vec())
            .unwrap();
        writer.render.try_send(b"new-render".to_vec()).unwrap();

        assert!(writer.replace_with_cleanup(b"graphics-cleanup".to_vec()));

        assert_eq!(
            queue.recv(),
            Some(ClientWriteItem::Control(b"graphics-cleanup".to_vec()))
        );
        let state = queue.lock_state();
        assert!(state.ordered.is_empty());
        assert!(state.render.is_none());
    }

    #[test]
    fn pane_cleanup_preserves_unscoped_and_unrelated_work_before_cleanup() {
        let (writer, queue) = test_queue_writer();
        let replaced_pane_id = crate::layout::PaneId::alloc();
        let unrelated_pane_id = crate::layout::PaneId::alloc();
        writer.render.try_send(b"stale-render".to_vec()).unwrap();
        writer
            .render
            .send_ordered(unrelated_pane_id, b"unrelated-graphics".to_vec())
            .unwrap();
        writer.render.try_send(b"new-render".to_vec()).unwrap();

        assert!(writer.replace_with_pane_cleanup(replaced_pane_id, b"pane-cleanup".to_vec()));

        assert_eq!(
            queue.recv(),
            Some(ClientWriteItem::Control(
                b"stale-renderunrelated-graphicsnew-renderpane-cleanup".to_vec()
            ))
        );
        let state = queue.lock_state();
        assert!(state.ordered.is_empty());
        assert!(state.render.is_none());
    }

    #[test]
    fn pane_cleanup_preserves_unscoped_work_and_drops_replaced_pane() {
        let (writer, queue) = test_queue_writer();
        let pane_id = crate::layout::PaneId::alloc();
        writer.render.try_send(b"stale-render".to_vec()).unwrap();
        writer
            .render
            .send_ordered(pane_id, b"stale-direct-graphics".to_vec())
            .unwrap();
        writer.render.try_send(b"new-render".to_vec()).unwrap();

        assert!(writer.replace_with_pane_cleanup(pane_id, b"pane-cleanup".to_vec()));

        assert_eq!(
            queue.recv(),
            Some(ClientWriteItem::Control(
                b"stale-rendernew-renderpane-cleanup".to_vec()
            ))
        );
        let state = queue.lock_state();
        assert!(state.ordered.is_empty());
        assert!(state.render.is_none());
    }

    #[test]
    fn client_writer_queue_bounds_control_records() {
        let (writer, _queue) = test_queue_writer();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            writer.control.send(vec![b'x']).expect("control fits");
        }
        assert!(matches!(
            writer.control.send(vec![b'y']),
            Err(TrySendError::Full(data)) if data == vec![b'y']
        ));
    }

    #[test]
    fn ordered_direct_follows_older_render_and_stays_bounded() {
        let (writer, queue) = test_queue_writer();
        let pane_id = crate::layout::PaneId::alloc();
        writer.render.try_send(b"old".to_vec()).unwrap();
        writer
            .render
            .send_ordered(pane_id, b"direct".to_vec())
            .unwrap();
        assert!(matches!(
            writer.render.send_ordered(pane_id, b"second".to_vec()),
            Err(TrySendError::Full(_))
        ));
        writer.render.try_send(b"new".to_vec()).unwrap();

        for expected in [b"old".as_slice(), b"direct", b"new"] {
            assert_eq!(
                queue.recv(),
                Some(ClientWriteItem::Render(expected.to_vec()))
            );
        }
        queue.close_writer();
        assert!(matches!(
            writer.render.send_ordered(pane_id, b"closed".to_vec()),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[test]
    fn client_writer_prioritizes_control_and_reports_capacity_drains() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-writer-priority");
        let (writer, queue) = test_queue_writer();
        writer
            .render
            .try_send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");
        writer
            .control
            .send(frame_server_message(&ServerMessage::ReloadSoundConfig))
            .expect("queue control");

        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let handle = std::thread::spawn(move || {
            client_writer_loop(server_stream, 9, queue, server_event_tx);
        });

        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read control") {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected control message first, got {other:?}"),
        }
        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read render") {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("render")),
            other => panic!("expected render message second, got {other:?}"),
        }
        match recv_server_event(&mut server_event_rx, "writer freed control capacity") {
            ServerEvent::ClientWriterControlDrained { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected control drain event, got {other:?}"),
        }
        match recv_server_event(&mut server_event_rx, "writer drained render slot") {
            ServerEvent::ClientWriterDrained { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected render drain event, got {other:?}"),
        }

        drop(writer);
        handle.join().expect("writer exits after senders drop");
    }

    #[test]
    fn client_writer_exits_when_all_writer_handles_drop() {
        let (_client_stream, server_stream, _path) = local_stream_pair("client-writer-drop");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 11, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        drop(writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits without polling after senders drop");
    }

    #[test]
    fn client_writer_clone_keeps_loop_alive_until_final_drop() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-clone-drop");
        let (writer, queue) = test_queue_writer();
        let cloned_writer = writer.clone();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 12, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        drop(writer);
        cloned_writer
            .control
            .send(frame_server_message(&ServerMessage::ReloadSoundConfig))
            .expect("cloned writer still sends after original drops");
        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE)
            .expect("read control from cloned writer")
        {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected cloned control message, got {other:?}"),
        }
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "writer exited while cloned handles were still alive"
        );

        drop(cloned_writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits after final cloned writer drops");
    }

    #[test]
    fn client_writer_closes_queue_after_socket_write_failure() {
        let (client_stream, server_stream, _path) =
            local_stream_pair("client-writer-socket-failure");
        #[cfg(not(windows))]
        server_stream
            .set_send_timeout(Some(Duration::from_millis(100)))
            .expect("set test send timeout");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 13, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        writer
            .control
            .send(vec![b'x'; 1024 * 1024])
            .expect("message is accepted before the writer observes socket failure");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "writer dequeued failure payload"),
            ServerEvent::ClientWriterControlDrained { client_id: 13 }
        ));
        drop(client_stream);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer exits after socket write failure");

        assert!(matches!(
            writer.control.send(vec![b'y']),
            Err(TrySendError::Disconnected(_))
        ));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[test]
    fn clamp_terminal_size_zero_zero() {
        assert_eq!(
            clamp_terminal_size(0, 0),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn clamp_terminal_size_one_one() {
        assert_eq!(clamp_terminal_size(1, 1), (1, 1));
    }

    #[test]
    fn clamp_terminal_size_preserves_narrow_client_size() {
        assert_eq!(clamp_terminal_size(40, 12), (40, 12));
    }

    #[test]
    fn clamp_terminal_size_valid() {
        assert_eq!(clamp_terminal_size(120, 40), (120, 40));
    }

    #[test]
    fn clamp_terminal_size_exact_minimum() {
        assert_eq!(
            clamp_terminal_size(MIN_CLIENT_COLS, MIN_CLIENT_ROWS),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn parse_client_keybindings_accepts_local_profile() {
        let keybindings = parse_client_keybindings(ClientKeybindings::Local {
            keys_toml: r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"

[[keys.command]]
key = "prefix+g"
command = "lazygit"
"#
            .to_owned(),
        })
        .expect("valid client keybindings")
        .expect("local profile");

        assert_eq!(keybindings.prefix.0, crossterm::event::KeyCode::Char('a'));
        assert!(keybindings
            .keybinds
            .new_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+t"));
        assert!(keybindings.keybinds.custom_commands.is_empty());
    }

    #[test]
    fn parse_client_keybindings_tolerates_disabled_bindings() {
        let keybindings = parse_client_keybindings(ClientKeybindings::Local {
            keys_toml: r#"
[keys]
new_tab = "ctrl+notakey"
"#
            .to_owned(),
        })
        .expect("diagnostic-only client keybindings should be accepted")
        .expect("local profile");

        assert!(keybindings.keybinds.new_tab.bindings.is_empty());
        assert!(keybindings
            .keybinds
            .next_tab
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+n"));
    }

    #[test]
    fn handshake_negotiates_terminal_ansi_encoding() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-handshake-ansi");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 42, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 100,
                rows: 30,
                cell_width_px: 8,
                cell_height_px: 16,
                requested_encoding: RenderEncoding::TerminalAnsi,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::App,
                display_name: None,
                frontend_profile_id: None,
                renderer_binding_token: None,
                renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            },
        )
        .expect("write hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        match welcome {
            ServerMessage::Welcome {
                version,
                encoding,
                error,
            } => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(encoding, RenderEncoding::TerminalAnsi);
                assert_eq!(error, None);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        match server_event_rx
            .blocking_recv()
            .expect("client connected event")
        {
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                render_encoding,
                keybindings,
                direct_attach_requested,
                direct_graphics,
                omp_pane,
                display_name,
                frontend_profile_id,
                renderer_binding_token,
                renderer_capabilities,
                writer,
            } => {
                assert_eq!(client_id, 42);
                assert_eq!((cols, rows), (100, 30));
                assert_eq!((cell_width_px, cell_height_px), (8, 16));
                assert_eq!(render_encoding, RenderEncoding::TerminalAnsi);
                assert!(keybindings.is_none());
                assert!(!direct_attach_requested);
                assert!(!direct_graphics);
                assert!(!omp_pane);
                assert!(display_name.is_none());
                assert!(frontend_profile_id.is_none());
                assert!(renderer_binding_token.is_none());
                assert_eq!(
                    renderer_capabilities,
                    crate::protocol::OmpRendererCapabilities::default()
                );
                drop(writer);
            }
            other => panic!("expected ClientConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn handshake_marks_terminal_attach_launch_mode() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-handshake-terminal-attach");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 42, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols: 100,
                rows: 30,
                cell_width_px: 8,
                cell_height_px: 16,
                requested_encoding: RenderEncoding::TerminalAnsi,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::TerminalAttach,
                display_name: None,
                frontend_profile_id: None,
                renderer_binding_token: None,
                renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            },
        )
        .expect("write hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        match welcome {
            ServerMessage::Welcome {
                version,
                encoding,
                error,
            } => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(encoding, RenderEncoding::TerminalAnsi);
                assert_eq!(error, None);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        match server_event_rx
            .blocking_recv()
            .expect("client connected event")
        {
            ServerEvent::ClientConnected {
                direct_attach_requested,
                writer,
                ..
            } => {
                assert!(direct_attach_requested);
                drop(writer);
            }
            other => panic!("expected ClientConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn notification_activator_acknowledges_processed_result_without_connecting() {
        for activated in [true, false] {
            let (mut client_stream, server_stream, _path) =
                local_stream_pair("client-handshake-notification-activator");
            let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
            let should_quit = Arc::new(AtomicBool::new(false));
            let handshake_quit = should_quit.clone();
            let handle = std::thread::spawn(move || {
                handle_client_handshake(server_stream, 42, &server_event_tx, &handshake_quit)
            });

            protocol::write_message(
                &mut client_stream,
                &ClientMessage::Hello {
                    version: PROTOCOL_VERSION,
                    cols: 1,
                    rows: 1,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    requested_encoding: RenderEncoding::SemanticFrame,
                    keybindings: ClientKeybindings::Server,
                    launch_mode: ClientLaunchMode::NotificationActivator,
                    display_name: None,
                    frontend_profile_id: None,
                    renderer_binding_token: None,
                    renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
                },
            )
            .expect("write activator hello");
            let welcome: ServerMessage =
                protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
            assert!(matches!(
                welcome,
                ServerMessage::Welcome { error: None, .. }
            ));

            let activation = NotificationActivation {
                recipient_client_id: 7,
                workspace_id: "workspace".to_owned(),
                pane_id: 42,
            };
            protocol::write_message(
                &mut client_stream,
                &ClientMessage::ActivateNotification {
                    activation: activation.clone(),
                },
            )
            .expect("write activation");

            let respond_to =
                match recv_server_event(&mut server_event_rx, "notification activation event") {
                    ServerEvent::NotificationActivated {
                        activation: received,
                        respond_to,
                    } => {
                        assert_eq!(received, activation);
                        respond_to
                    }
                    other => panic!("expected NotificationActivated, got {other:?}"),
                };
            respond_to.send(activated).expect("send processed result");
            assert_eq!(
                protocol::read_message::<_, ServerMessage>(&mut client_stream, MAX_FRAME_SIZE)
                    .expect("read activation result"),
                ServerMessage::NotificationActivationProcessed { activated }
            );

            handle
                .join()
                .expect("handshake thread join")
                .expect("handshake thread result");
            assert!(matches!(
                server_event_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }

    #[test]
    fn client_read_loop_rejects_oversized_bracketed_paste_without_disconnect() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-oversized");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD),
            },
        )
        .expect("write maximum-size bracketed paste");

        match recv_server_event(&mut server_event_rx, "maximum-size paste event") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data.len(), MAX_INPUT_PAYLOAD);
            }
            other => panic!("expected maximum-size ClientInput, got {other:?}"),
        }

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1),
            },
        )
        .expect("write oversized bracketed paste");

        match recv_server_event(&mut server_event_rx, "oversized paste rejection") {
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(size, MAX_INPUT_PAYLOAD + 1);
                assert_eq!(max, MAX_INPUT_PAYLOAD);
            }
            ServerEvent::ClientDisconnected { .. } => {
                panic!("oversized input must be rejected without disconnecting the client")
            }
            other => panic!("expected ClientPasteRejected, got {other:?}"),
        }

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: b"still connected".to_vec(),
            },
        )
        .expect("write valid input after rejection");

        match recv_server_event(&mut server_event_rx, "valid input after rejection") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data, b"still connected");
            }
            other => panic!("expected ClientInput after rejection, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_oversized_non_paste_input() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-non-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: vec![b'x'; MAX_INPUT_PAYLOAD + 1],
            },
        )
        .expect("write oversized non-paste input");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "oversized non-paste disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_invalid_pixel_mouse_geometry() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-invalid-pixel-geometry");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputPixels {
                data: b"\x1b[<35;1;1M".to_vec(),
                cols: 0,
                rows: 24,
                width_px: 800,
                height_px: 480,
            },
        )
        .expect("write invalid pixel geometry");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "invalid pixel geometry disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));
        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_invalid_pixel_mouse_report() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-invalid-pixel-report");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputPixels {
                data: vec![b'x'; MAX_PIXEL_MOUSE_PAYLOAD + 1],
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            },
        )
        .expect("write invalid pixel report");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "invalid pixel report disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));
        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_marker_wrapped_invalid_utf8() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-invalid-utf8-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });
        let mut data = bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1);
        data[b"\x1b[200~".len()] = 0xff;

        protocol::write_message(&mut client_stream, &ClientMessage::Input { data })
            .expect("write marker-wrapped invalid UTF-8 input");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "invalid UTF-8 input disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_forwards_input_events() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-events");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });
        let events = vec![
            ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Enter,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            },
            ClientInputEvent::FocusGained,
        ];

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputEvents {
                events: events.clone(),
            },
        )
        .expect("write input events");

        match server_event_rx
            .blocking_recv()
            .expect("client input events event")
        {
            ServerEvent::ClientInputEvents {
                client_id,
                events: actual,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(actual, events);
            }
            other => panic!("expected ClientInputEvents, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_rejects_oversized_input_event_batch() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-events");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputEvents {
                events: vec![ClientInputEvent::FocusGained; MAX_INPUT_EVENT_BATCH + 1],
            },
        )
        .expect("write oversized input events");

        match server_event_rx
            .blocking_recv()
            .expect("client disconnected event")
        {
            ServerEvent::ClientDisconnected { client_id } => assert_eq!(client_id, 7),
            other => panic!("expected ClientDisconnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_rejects_oversized_input_event_paste() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        let maximum = vec![
            ClientInputEvent::Paste {
                text: "x".repeat(MAX_INPUT_PAYLOAD / 2),
            },
            ClientInputEvent::Paste {
                text: "y".repeat(MAX_INPUT_PAYLOAD - (MAX_INPUT_PAYLOAD / 2)),
            },
        ];
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputEvents {
                events: maximum.clone(),
            },
        )
        .expect("write maximum-size structured paste");

        match recv_server_event(&mut server_event_rx, "maximum-size structured paste") {
            ServerEvent::ClientInputEvents { client_id, events } => {
                assert_eq!(client_id, 7);
                assert_eq!(events, maximum);
            }
            other => panic!("expected maximum-size ClientInputEvents, got {other:?}"),
        }

        let oversized = vec![
            ClientInputEvent::FocusGained,
            ClientInputEvent::Paste {
                text: "x".repeat(MAX_INPUT_PAYLOAD / 2),
            },
            ClientInputEvent::Paste {
                text: "y".repeat(MAX_INPUT_PAYLOAD - (MAX_INPUT_PAYLOAD / 2) + 1),
            },
            ClientInputEvent::FocusLost,
            ClientInputEvent::Paste {
                text: "tail".to_owned(),
            },
        ];
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputEvents { events: oversized },
        )
        .expect("write oversized structured paste");

        match recv_server_event(&mut server_event_rx, "oversized structured paste rejection") {
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(size, MAX_INPUT_PAYLOAD + 5);
                assert_eq!(max, MAX_INPUT_PAYLOAD);
            }
            other => panic!("expected ClientPasteRejected, got {other:?}"),
        }

        let valid = vec![ClientInputEvent::FocusGained];
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::InputEvents {
                events: valid.clone(),
            },
        )
        .expect("write valid structured input after rejection");

        match recv_server_event(&mut server_event_rx, "structured input after rejection") {
            ServerEvent::ClientInputEvents { client_id, events } => {
                assert_eq!(client_id, 7);
                assert_eq!(events, valid);
            }
            other => panic!("expected ClientInputEvents after rejection, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn structured_input_limits_charge_grouped_repeats_and_text_payloads() {
        let grouped = ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('x'),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: (MAX_INPUT_EVENT_BATCH + 1) as u16,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        };
        assert_eq!(
            input_event_limit(&[grouped]),
            InputEventLimit::TooManyEvents
        );

        let repeated_text = ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('x'),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: MAX_INPUT_EVENT_BATCH as u16,
            generated_text: Some("x".repeat((MAX_INPUT_PAYLOAD / MAX_INPUT_EVENT_BATCH) + 1)),
            source: crate::protocol::ClientKeySource::Synthesized,
        };
        assert!(matches!(
            input_event_limit(&[repeated_text]),
            InputEventLimit::InputPayloadTooLarge { size } if size > MAX_INPUT_PAYLOAD
        ));

        let text = ClientInputEvent::TextCommit("x".repeat(MAX_INPUT_PAYLOAD + 1));
        assert_eq!(
            input_event_limit(&[text]),
            InputEventLimit::InputPayloadTooLarge {
                size: MAX_INPUT_PAYLOAD + 1
            }
        );
    }
}
