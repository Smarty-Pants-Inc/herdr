//! Stdin input reading for the thin client.
//!
//! On Unix, reads stdin bytes and forwards framed input to the main event loop.
//! The server handles semantic parsing. On Windows, crossterm may surface
//! terminal control strings as character key events, so the reader re-frames
//! those control bytes before forwarding semantic client input events.
//!
//! This is simpler and more reliable because:
//! - The server has the same input parsing code
//! - We avoid duplicating parsing logic in the client
//! - Host terminal control replies can be buffered or discarded before they leak

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::time::Duration;
use tokio::sync::mpsc;

use super::ClientLoopEvent;
#[cfg(unix)]
use super::{HostInputSnapshot, HostInputState};

#[cfg(any(windows, test))]
mod windows_vti;

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// Reads raw bytes from stdin and sends them to the main event loop.
///
/// This runs on a dedicated thread because stdin reading is blocking.
/// The main loop receives the raw bytes and forwards them as
/// `ClientMessage::Input` to the server.
pub fn stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    #[cfg(unix)] host_input_state: Arc<HostInputState>,
    #[cfg(unix)] input_wake: OwnedFd,
    #[cfg(unix)] direct_response: Arc<std::sync::Mutex<super::direct_graphics::ResponseMatcher>>,
    #[cfg(unix)] direct_response_active: Arc<AtomicBool>,
) {
    #[cfg(windows)]
    {
        let _ = (host_color_query_sent, host_cell_size_query_sent);
        windows_stdin_reader_loop(event_tx, should_quit);
    }

    #[cfg(unix)]
    unix_stdin_reader_loop(
        event_tx,
        should_quit,
        host_color_query_sent,
        host_cell_size_query_sent,
        host_input_state,
        input_wake,
        direct_response,
        direct_response_active,
    );
}

#[cfg(unix)]
fn unix_stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_input_state: Arc<HostInputState>,
    input_wake: OwnedFd,
    direct_response: Arc<std::sync::Mutex<super::direct_graphics::ResponseMatcher>>,
    direct_response_active: Arc<AtomicBool>,
) {
    let stdin_fd = io::stdin().as_raw_fd();
    let mut scratch = [0u8; 4096];
    let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
    if host_color_query_sent {
        framer.host_color_query_sent();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();
    }
    if host_cell_size_query_sent {
        framer.host_cell_size_query_sent();
    }
    let mut pending_palette = Vec::new();
    let mut pending_input_state = None;
    let mut direct_pending_state = None;
    let mut last_geometry = None;
    let mut direct_filter = super::direct_graphics::InputFilter::default();

    while !should_quit.load(Ordering::Acquire) {
        if direct_filter.has_pending()
            && poll_read_ready(stdin_fd, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
                == Some(false)
        {
            let released = direct_response
                .lock()
                .ok()
                .and_then(|mut matcher| direct_filter.flush_if_inactive(&mut matcher));
            if let Some(data) = released {
                let (input_state, geometry) = direct_pending_state
                    .take()
                    .unwrap_or_else(|| host_input_state.load_context());
                let mut events = Vec::new();
                frame_input(
                    &data,
                    input_state,
                    geometry,
                    &mut framer,
                    &mut pending_input_state,
                    &mut last_geometry,
                    &mut pending_palette,
                    &mut events,
                );
                if !events.is_empty()
                    && host_input_state
                        .send_event(&event_tx, ClientLoopEvent::OrderedInput(events))
                        .is_err()
                {
                    return;
                }
                let (current_input_state, current_geometry) = host_input_state.load_context();
                if let Some(events) = flush_framer_after_idle(
                    &mut framer,
                    &mut pending_input_state,
                    &mut last_geometry,
                    &mut pending_palette,
                    current_input_state,
                    current_geometry,
                    |timeout_ms| poll_read_ready(stdin_fd, timeout_ms),
                ) {
                    if !events.is_empty()
                        && host_input_state
                            .send_event(&event_tx, ClientLoopEvent::OrderedInput(events))
                            .is_err()
                    {
                        return;
                    }
                }
            }
            continue;
        }

        let readiness = match poll_stdin_and_wake(stdin_fd, input_wake.as_raw_fd(), -1) {
            Ok(readiness) => readiness,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if readiness.wake_ready && crate::pty::fd::drain_wake_fd(input_wake.as_raw_fd()).is_err() {
            break;
        }
        if !readiness.pty_read_ready {
            continue;
        }
        let read = host_input_state.read_input_and_send(
            &event_tx,
            &mut scratch,
            |bytes, input_state, observed_geometry| {
                let mut events = Vec::new();
                let filtered = filter_direct_input(
                    bytes,
                    &mut direct_filter,
                    &direct_response,
                    &direct_response_active,
                );
                if let Some((raw_chunks, responses, pending_uses_old)) = filtered {
                    events.extend(
                        responses
                            .into_iter()
                            .map(ClientLoopEvent::DirectGraphicsResponse),
                    );
                    let current_context = (input_state, observed_geometry);
                    let filter_pending_context = direct_pending_state.unwrap_or(current_context);
                    direct_pending_state =
                        direct_filter.has_pending().then_some(if pending_uses_old {
                            filter_pending_context
                        } else {
                            current_context
                        });
                    for chunk in raw_chunks {
                        if chunk.pending_bytes > 0 {
                            frame_input(
                                &chunk.data[..chunk.pending_bytes],
                                filter_pending_context.0,
                                filter_pending_context.1,
                                &mut framer,
                                &mut pending_input_state,
                                &mut last_geometry,
                                &mut pending_palette,
                                &mut events,
                            );
                        }
                        if chunk.pending_bytes < chunk.data.len() {
                            frame_input(
                                &chunk.data[chunk.pending_bytes..],
                                input_state,
                                observed_geometry,
                                &mut framer,
                                &mut pending_input_state,
                                &mut last_geometry,
                                &mut pending_palette,
                                &mut events,
                            );
                        }
                    }
                } else {
                    frame_input(
                        bytes,
                        input_state,
                        observed_geometry,
                        &mut framer,
                        &mut pending_input_state,
                        &mut last_geometry,
                        &mut pending_palette,
                        &mut events,
                    );
                }
                (!events.is_empty()).then_some(ClientLoopEvent::OrderedInput(events))
            },
        );
        match read {
            Ok(0) => break,
            Ok(_) => {
                let (current_input_state, current_geometry) = host_input_state.load_context();
                if let Some(events) = flush_framer_after_idle(
                    &mut framer,
                    &mut pending_input_state,
                    &mut last_geometry,
                    &mut pending_palette,
                    current_input_state,
                    current_geometry,
                    |timeout_ms| poll_read_ready(stdin_fd, timeout_ms),
                ) {
                    if !events.is_empty()
                        && host_input_state
                            .send_event(&event_tx, ClientLoopEvent::OrderedInput(events))
                            .is_err()
                    {
                        return;
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[cfg(unix)]
fn filter_direct_input(
    bytes: &[u8],
    filter: &mut super::direct_graphics::InputFilter,
    response: &std::sync::Mutex<super::direct_graphics::ResponseMatcher>,
    active: &AtomicBool,
) -> Option<(
    Vec<super::direct_graphics::FilteredInput>,
    Vec<super::direct_graphics::Response>,
    bool,
)> {
    if !active.load(Ordering::Acquire) && !filter.has_pending() {
        return None;
    }
    Some(
        response
            .lock()
            .map(|mut matcher| filter.push(bytes, &mut matcher))
            .unwrap_or_else(|_| {
                (
                    vec![super::direct_graphics::FilteredInput {
                        data: bytes.to_vec(),
                        pending_bytes: 0,
                    }],
                    Vec::new(),
                    false,
                )
            }),
    )
}

#[cfg(unix)]
type HostInputContext = (HostInputSnapshot, Option<crate::input::mouse::HostGeometry>);

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn frame_input(
    bytes: &[u8],
    input_state: HostInputSnapshot,
    observed_geometry: Option<crate::input::mouse::HostGeometry>,
    framer: &mut crate::raw_input::RawInputByteFramer,
    pending_input_state: &mut Option<HostInputContext>,
    last_geometry: &mut Option<(u64, crate::input::mouse::HostGeometry)>,
    pending_palette: &mut Vec<Vec<u8>>,
    events: &mut Vec<ClientLoopEvent>,
) {
    let geometry = input_geometry(last_geometry, input_state, observed_geometry);
    let mut offset = 0;
    if let Some((pending_state, pending_geometry)) = *pending_input_state {
        let continued_state = pending_state.continued_with(input_state);
        let continued_geometry = (pending_geometry == geometry).then_some(geometry).flatten();
        if continued_state != input_state || continued_geometry != geometry {
            while offset < bytes.len() && framer.has_pending_input() {
                let chunks = framer.push(&bytes[offset..offset + 1]);
                offset += 1;
                let emitted = !chunks.is_empty();
                append_unix_input_chunks(
                    chunks,
                    pending_palette,
                    continued_state,
                    continued_geometry,
                    events,
                );
                if emitted {
                    break;
                }
            }
            if framer.has_pending_input() && offset == bytes.len() {
                *pending_input_state = Some((continued_state, continued_geometry));
                return;
            }
            *pending_input_state = framer
                .has_pending_input()
                .then_some((input_state, geometry));
            if offset == bytes.len() {
                return;
            }
        }
    }
    let chunks = framer.push(&bytes[offset..]);
    *pending_input_state = framer
        .has_pending_input()
        .then_some((input_state, geometry));
    append_unix_input_chunks(chunks, pending_palette, input_state, geometry, events);
}

#[cfg(unix)]
fn append_unix_input_chunks(
    chunks: Vec<Vec<u8>>,
    pending_palette: &mut Vec<Vec<u8>>,
    input_state: HostInputSnapshot,
    geometry: Option<crate::input::mouse::HostGeometry>,
    events: &mut Vec<ClientLoopEvent>,
) {
    for data in chunks {
        let palette_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_palette_color_response)
            .is_some();
        if palette_response {
            pending_palette.push(data);
            if pending_palette.len() == 256 {
                flush_unix_palette_input(pending_palette, input_state, events);
            }
            continue;
        }
        let default_color_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_default_color_response)
            .is_some();
        if !default_color_response {
            flush_unix_palette_input(pending_palette, input_state, events);
        }
        if let Some(event) = classify_unix_input(data, input_state, geometry) {
            events.push(event);
        }
    }
}

#[cfg(unix)]
fn input_geometry(
    last: &mut Option<(u64, crate::input::mouse::HostGeometry)>,
    input_state: HostInputSnapshot,
    observed: Option<crate::input::mouse::HostGeometry>,
) -> Option<crate::input::mouse::HostGeometry> {
    if !input_state.sgr_pixels_active() {
        return None;
    }
    if let Some(geometry) = observed {
        *last = Some((input_state.generation(), geometry));
        return Some(geometry);
    }
    last.as_ref()
        .filter(|(generation, _)| *generation == input_state.generation())
        .map(|(_, geometry)| *geometry)
}

#[cfg(unix)]
fn classify_unix_input(
    data: Vec<u8>,
    input_state: HostInputSnapshot,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> Option<ClientLoopEvent> {
    if input_state.sgr_pixels_active() && crate::input::mouse::parse_report(&data).is_some() {
        return geometry.map(|geometry| ClientLoopEvent::PixelMouse(data, geometry, input_state));
    }
    Some(ClientLoopEvent::StdinInput(data, input_state))
}

#[cfg(unix)]
fn flush_unix_palette_input(
    pending_palette: &mut Vec<Vec<u8>>,
    input_state: HostInputSnapshot,
    events: &mut Vec<ClientLoopEvent>,
) {
    if pending_palette.is_empty() {
        return;
    }
    let data = std::mem::take(pending_palette).concat();
    events.push(ClientLoopEvent::StdinInput(data, input_state));
}

#[cfg(unix)]
fn idle_flush_timeout_ms(
    framer: &crate::raw_input::RawInputByteFramer,
    host_mouse_capture_active: bool,
) -> i32 {
    if host_mouse_capture_active
        && (framer.has_pending_lone_escape() || framer.has_pending_incomplete_mouse_sequence())
    {
        crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
    } else {
        crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
    }
}

#[cfg(unix)]
fn flush_framer_after_idle(
    framer: &mut crate::raw_input::RawInputByteFramer,
    pending_input_state: &mut Option<HostInputContext>,
    last_geometry: &mut Option<(u64, crate::input::mouse::HostGeometry)>,
    pending_palette: &mut Vec<Vec<u8>>,
    current_input_state: HostInputSnapshot,
    current_geometry: Option<crate::input::mouse::HostGeometry>,
    mut wait_for_input: impl FnMut(i32) -> Option<bool>,
) -> Option<Vec<ClientLoopEvent>> {
    if !framer.has_pending_input() && pending_palette.is_empty() {
        return None;
    }

    let current_geometry = input_geometry(last_geometry, current_input_state, current_geometry);
    let (input_state, geometry) =
        (*pending_input_state).unwrap_or((current_input_state, current_geometry));
    let timeout_ms = idle_flush_timeout_ms(framer, input_state.capture_active());
    if wait_for_input(timeout_ms) != Some(false) {
        return None;
    }

    let had_pending = framer.has_pending_input();
    let chunks = framer.flush_timeout();
    let held_escape = had_pending && chunks.is_empty();
    *pending_input_state = framer
        .has_pending_input()
        .then_some((input_state, geometry));
    let mut events = Vec::new();
    append_unix_input_chunks(chunks, pending_palette, input_state, geometry, &mut events);
    flush_unix_palette_input(pending_palette, input_state, &mut events);

    if held_escape
        && wait_for_input(crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS) == Some(false)
    {
        let (input_state, geometry) =
            (*pending_input_state).unwrap_or((current_input_state, current_geometry));
        let chunks = framer.flush_timeout();
        *pending_input_state = framer
            .has_pending_input()
            .then_some((input_state, geometry));
        append_unix_input_chunks(chunks, pending_palette, input_state, geometry, &mut events);
    }

    Some(events)
}

#[cfg(windows)]
fn windows_stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
) {
    if !super::windows_vti_input_backend_enabled() {
        windows_crossterm_reader_loop(event_tx, should_quit);
    } else {
        match windows_vti::console_input_handle() {
            Ok(handle) if windows_vti::virtual_terminal_input_enabled(handle) => {
                windows_vti::raw_console_reader_loop(handle, event_tx, should_quit);
            }
            _ => windows_crossterm_reader_loop(event_tx, should_quit),
        }
    }
}

#[cfg(windows)]
fn windows_crossterm_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
) {
    let mut framer = crate::raw_input::RawInputFramer::for_host_input();

    while !should_quit.load(Ordering::Acquire) {
        match crossterm::event::poll(Duration::from_millis(10)) {
            Ok(true) => {}
            Ok(false) => {
                if framer.has_pending_input() {
                    tracing::debug!("windows input raw sequence timed out; flushing");
                    if !send_windows_raw_events(framer.flush_timeout(), &event_tx) {
                        return;
                    }
                }
                continue;
            }
            Err(_) => break,
        }

        let event = match crossterm::event::read() {
            Ok(event) => event,
            Err(_) => break,
        };

        let raw_sequence_pending = framer.has_pending_input();
        if let Some(bytes) = windows_key_raw_bytes(&event, raw_sequence_pending) {
            tracing::debug!(
                bytes = ?bytes,
                pending_before = raw_sequence_pending,
                "windows input routed through raw framer"
            );
            if !send_windows_raw_events(framer.push(&bytes), &event_tx) {
                return;
            }
            continue;
        }

        if raw_sequence_pending {
            tracing::debug!("windows input raw sequence interrupted by semantic event; flushing");
            if !send_windows_raw_events(framer.flush_timeout(), &event_tx) {
                return;
            }
        }

        if windows_event_is_control_key(&event) {
            tracing::debug!(event = ?event, "windows control key forwarded as semantic input");
        }

        let Some(event) = windows_crossterm_input_event(event) else {
            continue;
        };
        if event_tx
            .blocking_send(ClientLoopEvent::StdinEvents(vec![event]))
            .is_err()
        {
            return;
        }
    }

    if framer.has_pending_input() {
        let _ = send_windows_raw_events(framer.flush_timeout(), &event_tx);
    }
}

#[cfg(any(windows, test))]
fn windows_crossterm_input_event(
    event: crossterm::event::Event,
) -> Option<crate::protocol::ClientInputEvent> {
    let event = crate::protocol::ClientInputEvent::from_crossterm(event)?;
    match event {
        crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char(codepoint),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            source,
            ..
        } => Some(crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char(codepoint),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: Some(codepoint.to_string()),
            source,
        }),
        event => Some(event),
    }
}

#[cfg(windows)]
fn windows_event_is_control_key(event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyModifiers};

    matches!(
        event,
        Event::Key(key)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || matches!(key.code, crossterm::event::KeyCode::Char(ch) if ch.is_control())
    )
}

#[cfg(any(windows, test))]
fn windows_key_raw_bytes(
    event: &crossterm::event::Event,
    raw_sequence_pending: bool,
) -> Option<Vec<u8>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }

    match key.code {
        KeyCode::Esc if key.modifiers.is_empty() => Some(vec![0x1b]),
        KeyCode::Char('[') if !raw_sequence_pending && key.modifiers == KeyModifiers::CONTROL => {
            Some(vec![0x1b])
        }
        KeyCode::Char(ch)
            if !raw_sequence_pending
                && matches!(ch, 'i' | 'I')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let mut buf = [0; 4];
            Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Char(ch) if raw_sequence_pending || ch.is_control() => {
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            let mut buf = [0; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            Some(bytes)
        }
        _ => None,
    }
}

#[cfg(windows)]
fn send_windows_raw_events(
    events: Vec<crate::raw_input::RawInputEvent>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
) -> bool {
    let raw_event_count = events.len();
    let events = events
        .into_iter()
        .filter_map(windows_client_input_event_from_raw)
        .collect::<Vec<_>>();
    if events.is_empty() {
        return true;
    }

    tracing::debug!(
        raw_event_count,
        forwarded_event_count = events.len(),
        "windows raw-framed input events forwarded"
    );
    event_tx
        .blocking_send(ClientLoopEvent::StdinEvents(events))
        .is_ok()
}

#[cfg(any(windows, test))]
fn windows_client_input_event_from_raw(
    event: crate::raw_input::RawInputEvent,
) -> Option<crate::protocol::ClientInputEvent> {
    match event {
        crate::raw_input::RawInputEvent::Text(text) => Some(
            crate::protocol::ClientInputEvent::TextCommit(text.into_string()),
        ),
        crate::raw_input::RawInputEvent::Key(key) => {
            let code = crate::protocol::ClientKeyCode::from_crossterm(key.code)?;
            let modifiers = key.modifiers.bits();
            let kind = crate::protocol::ClientKeyKind::from_crossterm(key.kind);
            let source = if let Some(bytes) = key.vt_bytes() {
                crate::protocol::ClientKeySource::Vt {
                    bytes: bytes.to_vec(),
                }
            } else if let Some(record) = key.windows_record() {
                crate::protocol::ClientKeySource::WindowsConsole { record }
            } else {
                crate::protocol::ClientKeySource::Synthesized
            };
            Some(crate::protocol::ClientInputEvent::Key {
                code,
                modifiers,
                kind,
                repeat_count: key.repeat_count,
                generated_text: key.generated_text.clone(),
                source,
            })
        }
        crate::raw_input::RawInputEvent::Mouse(mouse) => {
            Some(crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::from_crossterm(mouse.kind)?,
                column: mouse.column,
                row: mouse.row,
                modifiers: mouse.modifiers.bits(),
            })
        }
        crate::raw_input::RawInputEvent::Paste(text) => {
            Some(crate::protocol::ClientInputEvent::Paste { text })
        }
        crate::raw_input::RawInputEvent::OuterFocusGained => {
            Some(crate::protocol::ClientInputEvent::FocusGained)
        }
        crate::raw_input::RawInputEvent::OuterFocusLost => {
            Some(crate::protocol::ClientInputEvent::FocusLost)
        }
        crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => None,
    }
}

#[cfg(unix)]
pub(super) fn pending_input_bytes(fd: RawFd) -> io::Result<usize> {
    let mut pending: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut pending) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pending.max(0) as usize)
}

#[cfg(unix)]
fn poll_stdin_and_wake(
    stdin_fd: RawFd,
    wake_fd: RawFd,
    timeout_ms: i32,
) -> io::Result<crate::pty::fd::PtyWakeReadiness> {
    crate::pty::fd::poll_pty_and_wake(stdin_fd, wake_fd, true, false, timeout_ms)
}

#[cfg(unix)]
fn poll_read_ready(fd: i32, timeout_ms: i32) -> Option<bool> {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }

    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
    }

    const POLLIN: i16 = 0x0001;

    let mut pfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };

    let result = unsafe { poll(&mut pfd as *mut PollFd, 1, timeout_ms) };
    if result < 0 {
        None
    } else {
        Some(result > 0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    // The stdin reader thread is hard to unit test since it reads from actual stdin.
    // Integration tests will verify the full client→server input flow.
    // Here we test the event type construction.

    use super::*;
    fn read_exact_input(state: &HostInputState, len: usize) -> (Vec<u8>, HostInputSnapshot) {
        let mut data = Vec::with_capacity(len);
        let mut captured = None;
        while data.len() < len {
            let mut scratch = vec![0; len - data.len()];
            let (read, snapshot, _) = state.read_input(&mut scratch).unwrap();
            assert!(read > 0);
            if let Some(captured) = captured {
                assert_eq!(snapshot, captured);
            } else {
                captured = Some(snapshot);
            }
            data.extend_from_slice(&scratch[..read]);
        }
        (data, captured.unwrap())
    }

    #[test]
    fn stdin_input_event_carries_raw_bytes_and_snapshot() {
        let data = vec![0x1b, b'[', b'A'];
        let snapshot = HostInputSnapshot::from_parts(7, true, false);
        match ClientLoopEvent::StdinInput(data.clone(), snapshot) {
            ClientLoopEvent::StdinInput(actual, captured) => {
                assert_eq!(actual, data);
                assert_eq!(captured, snapshot);
            }
            _ => panic!("expected stdin input"),
        }
    }

    #[test]
    fn inactive_direct_input_bypasses_filter() {
        let response =
            std::sync::Mutex::new(super::super::direct_graphics::ResponseMatcher::default());
        let active = response.lock().unwrap().active_handle();
        let mut filter = super::super::direct_graphics::InputFilter::default();
        assert!(filter_direct_input(b"typed", &mut filter, &response, &active).is_none());
        assert!(!filter.has_pending());
    }

    #[test]
    fn pixel_mouse_classification_is_narrow_and_uses_read_geometry() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let snapshot = HostInputSnapshot::from_parts(7, true, true);
        let report = b"\x1b[<35;321;241M".to_vec();
        let Some(ClientLoopEvent::PixelMouse(data, captured, captured_snapshot)) =
            classify_unix_input(report.clone(), snapshot, Some(geometry))
        else {
            panic!("expected pixel mouse event");
        };
        assert_eq!(data, report);
        assert_eq!(captured, geometry);
        assert_eq!(captured_snapshot, snapshot);
        assert!(classify_unix_input(report, snapshot, None).is_none());

        for raw in [
            b"key".as_slice(),
            b"\x1b[200~paste\x1b[201~".as_slice(),
            b"\x1b_Gi=7;unrelated\x1b\\".as_slice(),
            b"\x1b[<35;2;3Mtail".as_slice(),
        ] {
            let Some(ClientLoopEvent::StdinInput(data, captured_snapshot)) =
                classify_unix_input(raw.to_vec(), snapshot, Some(geometry))
            else {
                panic!("unrelated input must remain raw");
            };
            assert_eq!(data, raw);
            assert_eq!(captured_snapshot, snapshot);
        }
    }

    #[test]
    fn geometry_cache_does_not_cross_input_generations() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let old = HostInputSnapshot::from_parts(7, true, true);
        let current = HostInputSnapshot::from_parts(8, true, true);
        let mut cached = None;
        assert_eq!(
            input_geometry(&mut cached, old, Some(geometry)),
            Some(geometry)
        );
        assert_eq!(input_geometry(&mut cached, old, None), Some(geometry));
        assert_eq!(input_geometry(&mut cached, current, None), None);
    }

    #[test]
    fn resize_boundary_drains_only_pre_transition_mouse_bytes() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (input_state, wake) =
            HostInputState::with_input_fd(true, false, reader.as_raw_fd()).unwrap();
        let old = input_state.load();
        let stale = b"\x1b[<0;2;2M";
        let fresh = b"\x1b[<0;3;3M";
        writer.write_all(stale).unwrap();

        let (tx, mut rx) = mpsc::channel(2);
        input_state.send_resize(&tx, (80, 24, 8, 16), None).unwrap();
        let current = input_state.load();
        writer.write_all(fresh).unwrap();

        let readiness = poll_stdin_and_wake(reader.as_raw_fd(), wake.as_raw_fd(), 0).unwrap();
        assert!(readiness.pty_read_ready);
        assert!(readiness.wake_ready);
        crate::pty::fd::drain_wake_fd(wake.as_raw_fd()).unwrap();

        let (stale_data, stale_snapshot) = read_exact_input(&input_state, stale.len());
        let (fresh_data, fresh_snapshot) = read_exact_input(&input_state, fresh.len());
        assert_eq!(stale_data, stale);
        assert_eq!(stale_snapshot, old);
        assert_eq!(fresh_data, fresh);
        assert_eq!(fresh_snapshot, current);

        let ClientLoopEvent::Resize(_, _, _, _, applied_generation, _) = rx.try_recv().unwrap()
        else {
            panic!("expected resize event");
        };
        assert!(!super::super::mouse_input_is_current(
            stale_snapshot,
            current,
            applied_generation,
            true,
        ));
        assert!(super::super::mouse_input_is_current(
            fresh_snapshot,
            current,
            applied_generation,
            true,
        ));
    }

    #[test]
    fn mouse_mode_transition_wakes_before_first_post_transition_input() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (input_state, wake) =
            HostInputState::with_input_fd(false, false, reader.as_raw_fd()).unwrap();
        let input_state = Arc::new(input_state);
        let reader_state = input_state.clone();
        let (woke_tx, woke_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let report = b"\x1b[<0;4;4M";

        let reader_thread = std::thread::spawn(move || {
            let readiness = poll_stdin_and_wake(reader.as_raw_fd(), wake.as_raw_fd(), -1).unwrap();
            if readiness.wake_ready {
                crate::pty::fd::drain_wake_fd(wake.as_raw_fd()).unwrap();
            }
            woke_tx.send(readiness).unwrap();
            continue_rx.recv().unwrap();

            let readiness = poll_stdin_and_wake(reader.as_raw_fd(), wake.as_raw_fd(), -1).unwrap();
            assert!(readiness.pty_read_ready);
            read_exact_input(&reader_state, report.len())
        });

        let current = input_state
            .transition_mouse_mode(true, false, || Ok(()))
            .unwrap();
        let readiness = woke_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(readiness.wake_ready);
        assert!(!readiness.pty_read_ready);

        writer.write_all(report).unwrap();
        continue_tx.send(()).unwrap();
        let (data, snapshot) = reader_thread.join().unwrap();
        assert_eq!(data, report);
        assert_eq!(snapshot, current);
        assert!(super::super::mouse_input_is_current(
            snapshot,
            input_state.load(),
            current.generation(),
            true,
        ));
    }

    #[test]
    fn mouse_mode_transition_separates_boundary_bytes_from_new_generation() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (input_state, _wake) =
            HostInputState::with_input_fd(false, false, reader.as_raw_fd()).unwrap();
        let old = input_state.load();
        let boundary_report = b"\x1b[<0;4;4M";
        let stable_report = b"\x1b[<0;5;5M";

        let current = input_state
            .transition_mouse_mode(true, false, || writer.write_all(boundary_report))
            .unwrap();
        writer.write_all(stable_report).unwrap();

        let (boundary_data, boundary_snapshot) =
            read_exact_input(&input_state, boundary_report.len());
        let (stable_data, stable_snapshot) = read_exact_input(&input_state, stable_report.len());

        assert_eq!(boundary_data, boundary_report);
        assert_ne!(current.generation(), old.generation());
        assert!(!boundary_snapshot.stable);
        assert!(!super::super::mouse_input_is_current(
            boundary_snapshot,
            input_state.load(),
            current.generation(),
            true,
        ));
        assert_eq!(stable_data, stable_report);
        assert_eq!(stable_snapshot, current);
        assert!(super::super::mouse_input_is_current(
            stable_snapshot,
            input_state.load(),
            current.generation(),
            true,
        ));
    }

    #[test]
    fn stdin_read_is_published_before_a_resize_boundary() {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        let down = b"\x1b[<0;4;4M";
        let up = b"\x1b[<0;4;4m";
        let old_geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let new_geometry = crate::input::mouse::HostGeometry::new(80, 24, 809, 480).unwrap();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer
            .write_all(&[down.as_slice(), up.as_slice()].concat())
            .unwrap();
        let (input_state, _wake) =
            HostInputState::with_input_fd(true, true, reader.as_raw_fd()).unwrap();
        input_state
            .event_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .geometry = Some(old_geometry);
        let input_state = Arc::new(input_state);
        let applied_before_resize = input_state.load();
        let (event_tx, mut event_rx) = mpsc::channel(2);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let reader_state = input_state.clone();
        let reader_tx = event_tx.clone();
        let reader_thread = std::thread::spawn(move || {
            let mut scratch = [0; 64];
            reader_state.read_input_and_send(
                &reader_tx,
                &mut scratch,
                |bytes, snapshot, geometry| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    let geometry = geometry.expect("captured host geometry");
                    Some(ClientLoopEvent::OrderedInput(vec![
                        ClientLoopEvent::PixelMouse(
                            bytes[..down.len()].to_vec(),
                            geometry,
                            snapshot,
                        ),
                        ClientLoopEvent::PixelMouse(
                            bytes[down.len()..].to_vec(),
                            geometry,
                            snapshot,
                        ),
                    ]))
                },
            )
        });
        entered_rx.recv().unwrap();

        let resize_state = input_state.clone();
        let resize_tx = event_tx.clone();
        let resize_thread = std::thread::spawn(move || {
            resize_state.send_resize(&resize_tx, (80, 24, 10, 20), Some(new_geometry))
        });
        release_tx.send(()).unwrap();
        assert_eq!(
            reader_thread.join().unwrap().unwrap(),
            down.len() + up.len()
        );
        resize_thread.join().unwrap().unwrap();

        let ClientLoopEvent::OrderedInput(events) = event_rx.try_recv().unwrap() else {
            panic!("stdin batch must be published first");
        };
        assert!(matches!(
            events.as_slice(),
            [ClientLoopEvent::PixelMouse(first, first_geometry, first_snapshot), ClientLoopEvent::PixelMouse(second, second_geometry, second_snapshot)]
                if first == down
                    && second == up
                    && *first_geometry == old_geometry
                    && *second_geometry == old_geometry
                    && *first_snapshot == applied_before_resize
                    && *second_snapshot == applied_before_resize
        ));
        assert!(super::super::pixel_input_is_current(
            applied_before_resize,
            input_state.load(),
            applied_before_resize.generation(),
            old_geometry,
            Some(old_geometry),
        ));

        let ClientLoopEvent::Resize(_, _, _, _, applied_generation, applied_geometry) =
            event_rx.try_recv().unwrap()
        else {
            panic!("resize must follow the stdin batch");
        };
        assert!(!super::super::pixel_input_is_current(
            applied_before_resize,
            input_state.load(),
            applied_generation,
            old_geometry,
            applied_geometry,
        ));
        let current = input_state.load();
        assert!(super::super::pixel_input_is_current(
            current,
            current,
            applied_generation,
            new_geometry,
            applied_geometry,
        ));
    }

    #[test]
    fn palette_replies_are_forwarded_as_one_input_batch() {
        let snapshot = HostInputSnapshot::from_parts(7, false, false);
        let mut pending = Vec::new();
        let mut events = Vec::new();
        append_unix_input_chunks(
            vec![
                b"\x1b]4;0;rgb:1111/2222/3333\x1b\\".to_vec(),
                b"\x1b]4;1;rgb:4444/5555/6666\x1b\\".to_vec(),
            ],
            &mut pending,
            snapshot,
            None,
            &mut events,
        );
        assert!(events.is_empty());
        flush_unix_palette_input(&mut pending, snapshot, &mut events);
        let ClientLoopEvent::StdinInput(data, captured) = events.remove(0) else {
            panic!("expected palette input");
        };
        assert_eq!(
            data.windows(4)
                .filter(|window| *window == b"\x1b]4;")
                .count(),
            2
        );
        assert_eq!(captured, snapshot);
        assert!(pending.is_empty());
    }

    #[test]
    fn direct_filter_idle_release_flushes_lone_escape_with_captured_state() {
        let response =
            std::sync::Mutex::new(super::super::direct_graphics::ResponseMatcher::default());
        let active = response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_handle();
        assert!(response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .arm(1, 1));
        let mut direct_filter = super::super::direct_graphics::InputFilter::default();
        let snapshot = HostInputSnapshot::from_parts(7, true, false);

        let Some((chunks, responses, _)) =
            filter_direct_input(b"\x1b", &mut direct_filter, &response, &active)
        else {
            panic!("active direct filter must retain a partial response prefix");
        };
        assert!(chunks.is_empty());
        assert!(responses.is_empty());
        response
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel(1);
        let released = response
            .lock()
            .ok()
            .and_then(|mut matcher| direct_filter.flush_if_inactive(&mut matcher))
            .expect("inactive direct filter must release its pending prefix");

        let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
        let mut pending_input_state = None;
        framer.host_cell_size_query_sent();
        let mut last_geometry = None;
        let mut pending_palette = Vec::new();
        let mut initial_events = Vec::new();
        frame_input(
            &released,
            snapshot,
            None,
            &mut framer,
            &mut pending_input_state,
            &mut last_geometry,
            &mut pending_palette,
            &mut initial_events,
        );
        assert!(initial_events.is_empty());
        assert_eq!(pending_input_state, Some((snapshot, None)));

        let mut timeouts = Vec::new();
        let events = flush_framer_after_idle(
            &mut framer,
            &mut pending_input_state,
            &mut last_geometry,
            &mut pending_palette,
            snapshot,
            None,
            |timeout_ms| {
                timeouts.push(timeout_ms);
                Some(false)
            },
        )
        .expect("idle raw framer must be flushed after direct-filter release");

        assert_eq!(
            timeouts,
            vec![
                crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS,
                crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
            ]
        );
        assert!(matches!(
            events.as_slice(),
            [ClientLoopEvent::StdinInput(data, captured)] if data == b"\x1b" && *captured == snapshot
        ));
        assert!(!framer.has_pending_input());
        assert_eq!(pending_input_state, None);
    }

    #[test]
    fn raw_input_idle_flush_timeout_keeps_escape_responsive() {
        let timeout_ms = std::hint::black_box(crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS);
        assert!(timeout_ms <= 20);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_repeated_escape_keeps_second_escape_pending() {
        let mut framer = crate::raw_input::RawInputFramer::for_host_input();

        let events = framer.push(b"\x1b\x1b");

        assert_eq!(events.len(), 1);
        assert!(framer.has_pending_input());
        assert_eq!(framer.flush_timeout().len(), 1);
    }

    #[test]
    fn mouse_active_escape_sequences_get_longer_reassembly_window() {
        let mut escape = crate::raw_input::RawInputByteFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        let mut sgr_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(sgr_mouse.push(b"\x1b[<3").is_empty());
        let mut default_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(default_mouse.push(b"\x1b[MC").is_empty());
        let mut unrelated = crate::raw_input::RawInputByteFramer::default();
        assert!(unrelated.push(b"\x1b[49:33;2:").is_empty());

        for framer in [&escape, &sgr_mouse, &default_mouse, &unrelated] {
            assert_eq!(
                idle_flush_timeout_ms(framer, false),
                crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
            );
        }
        for framer in [&escape, &sgr_mouse, &default_mouse] {
            assert_eq!(
                idle_flush_timeout_ms(framer, true),
                crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
            );
        }
        assert_eq!(
            idle_flush_timeout_ms(&unrelated, true),
            crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );

        let mouse_timeout_ms =
            std::hint::black_box(crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS);
        assert!(mouse_timeout_ms > 100);
    }
}

#[cfg(test)]
mod windows_tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn windows_control_chars_are_reframed_as_raw_bytes() {
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&escape, false).as_deref(),
            Some(b"\x1b".as_slice())
        );

        let enter = Event::Key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&enter, false).as_deref(),
            Some(b"\r".as_slice())
        );

        let printable = Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()));
        assert_eq!(windows_key_raw_bytes(&printable, false), None);

        let pending_arrow_tail =
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&pending_arrow_tail, true).as_deref(),
            Some(b"[".as_slice())
        );
    }

    #[test]
    fn windows_crossterm_printable_press_keeps_key_semantics_and_text() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('你'), KeyModifiers::empty()));

        assert_eq!(
            windows_crossterm_input_event(event),
            Some(crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('你'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: Some("你".to_string()),
                source: crate::protocol::ClientKeySource::Synthesized,
            })
        );
    }

    #[test]
    fn windows_ctrl_bracket_starts_raw_escape_sequence() {
        let ctrl_bracket = Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL));
        assert_eq!(
            windows_key_raw_bytes(&ctrl_bracket, false).as_deref(),
            Some(b"\x1b".as_slice())
        );

        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[<35;48;26M");
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw mouse converts");
        assert!(matches!(
            event,
            crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: 47,
                row: 25,
                modifiers: _,
            }
        ));
    }

    #[test]
    fn windows_ctrl_shift_bracket_stays_semantic() {
        let ctrl_shift_bracket = Event::Key(KeyEvent::new(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(windows_key_raw_bytes(&ctrl_shift_bracket, false), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_ctrl_d_semantic_event_encodes_to_eot() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(windows_key_raw_bytes(&event, false), None);

        let event =
            crate::protocol::ClientInputEvent::from_crossterm(event).expect("ctrl-d converts");
        let raw = event.to_raw_input_event();
        let crate::raw_input::RawInputEvent::Key(key) = raw else {
            panic!("expected key");
        };
        assert_eq!(key.code, KeyCode::Char('d'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
        assert_eq!(
            crate::input::encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
            b"\x04"
        );
    }

    #[test]
    fn windows_pasted_printable_ctrl_i_routes_as_literal_i() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        assert_eq!(
            windows_key_raw_bytes(&event, false).as_deref(),
            Some(b"i".as_slice())
        );

        let event = Event::Key(KeyEvent::new(
            KeyCode::Char('I'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(
            windows_key_raw_bytes(&event, false).as_deref(),
            Some(b"I".as_slice())
        );
    }

    #[test]
    fn windows_eot_control_char_normalizes_to_ctrl_d() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('\u{4}'), KeyModifiers::empty()));
        let bytes = windows_key_raw_bytes(&event, false).expect("eot routes through raw framer");
        assert_eq!(bytes, b"\x04");

        let mut framer = crate::raw_input::RawInputFramer::default();
        let events = framer.push(&bytes);
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw eot converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL.bits(),
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt { bytes: vec![4] },
            }
        );
    }

    #[test]
    fn windows_pending_escape_sequence_converts_to_semantic_arrow() {
        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.push(b"[").is_empty());
        let events = framer.push(b"A");
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw arrow converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Up,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt {
                    bytes: b"\x1b[A".to_vec()
                },
            }
        );
    }

    #[test]
    fn windows_bare_escape_flushes_to_semantic_escape() {
        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw escape converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Esc,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt { bytes: vec![0x1b] },
            }
        );
    }
}
