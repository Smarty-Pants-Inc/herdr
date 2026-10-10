use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use crate::protocol::{AttachScrollDirection, AttachScrollSource, ClientPaneInputEvent};
use crate::pty::input_consumer::InputSource;

pub(super) fn downgrade_ineligible_pixel_mouse(
    events: &mut [ClientPaneInputEvent],
    pixel_mouse: bool,
    runtime_size: (u16, u16),
    runtime_pixels: Option<(u32, u32)>,
) {
    let (runtime_rows, runtime_cols) = runtime_size;
    for event in events {
        let ClientPaneInputEvent::Mouse {
            position, geometry, ..
        } = event
        else {
            continue;
        };
        let crate::protocol::ClientMousePosition::Pixels { x, y, column, row } = *position else {
            continue;
        };
        let exact = pixel_mouse
            && geometry.is_some_and(|geometry| {
                (runtime_rows, runtime_cols) == (geometry.rows, geometry.cols)
                    && runtime_pixels == Some((geometry.width_px, geometry.height_px))
                    && column < geometry.cols
                    && row < geometry.rows
                    && x > 0
                    && y > 0
                    && x <= geometry.width_px
                    && y <= geometry.height_px
            });
        if !exact {
            *position = crate::protocol::ClientMousePosition::Cell { column, row };
            *geometry = None;
        }
    }
}

pub(super) fn terminal_attach_mouse_position(
    runtime: &crate::terminal::TerminalRuntime,
    terminal_size: (u16, u16),
    cell_size: crate::kitty_graphics::HostCellSize,
    pixel_mouse: bool,
    host_sgr_pixels_active: bool,
    position: crate::protocol::ClientMousePosition,
    geometry: Option<crate::protocol::ClientMouseGeometry>,
) -> Option<crate::protocol::ClientMousePosition> {
    let runtime_size = runtime.current_size();
    let cell_fallback = |column, row| {
        (column < runtime_size.1 && row < runtime_size.0)
            .then_some(crate::protocol::ClientMousePosition::Cell { column, row })
    };
    let (x, y, column, row) = match position {
        crate::protocol::ClientMousePosition::Cell { column, row } => {
            return cell_fallback(column, row);
        }
        crate::protocol::ClientMousePosition::Pixels { x, y, column, row } => (x, y, column, row),
    };
    let Some(geometry) = geometry else {
        return cell_fallback(column, row);
    };
    let host_geometry = crate::input::mouse::HostGeometry::new(
        geometry.cols,
        geometry.rows,
        geometry.width_px,
        geometry.height_px,
    )?;
    if host_geometry.cell(x, y) != Some((column, row)) {
        return None;
    }
    let exact = (|| {
        let average_width = (geometry.width_px / u32::from(geometry.cols)).max(1);
        let average_height = (geometry.height_px / u32::from(geometry.rows)).max(1);
        let (child_width_px, child_height_px) = runtime.pixel_size()?;
        if !pixel_mouse
            || !host_sgr_pixels_active
            || !runtime.sgr_pixel_mouse_enabled()
            || terminal_size != (geometry.cols, geometry.rows)
            || runtime_size != (geometry.rows, geometry.cols)
            || !cell_size.is_known()
            || average_width != cell_size.width_px
            || average_height != cell_size.height_px
        {
            return None;
        }
        let crate::input::mouse::Position::Pixels { x, y } = (crate::input::mouse::HostPixels {
            x,
            y,
            geometry: host_geometry,
        })
        .pane_position(
            ratatui::layout::Rect::new(0, 0, geometry.cols, geometry.rows),
            child_width_px,
            child_height_px,
        )?
        else {
            return None;
        };
        Some(crate::protocol::ClientMousePosition::Pixels { x, y, column, row })
    })();
    exact.or_else(|| cell_fallback(column, row))
}

pub(super) fn apply_terminal_attach_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    column: Option<u16>,
    row: Option<u16>,
    modifiers: u8,
    input_source: InputSource,
) -> Result<bool, String> {
    apply_scroll(
        runtime,
        source,
        direction,
        lines,
        crate::input::mouse::Position::Cell {
            column: column.unwrap_or(0),
            row: row.unwrap_or(0),
        },
        modifiers,
        input_source,
    )
}

fn apply_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    position: crate::input::mouse::Position,
    modifiers: u8,
    input_source: InputSource,
) -> Result<bool, String> {
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
            return Ok(true);
        }
        return apply_terminal_attach_input(runtime, input, input_source);
    }

    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
            runtime.scroll_reset();
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
                .try_send_bytes_with_source(Bytes::from(bytes), input_source)
                .map_err(|err| format!("terminal attach mouse wheel input failed: {err}"))?;
            return Ok(true);
        }
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(false);
            };
            if bytes.is_empty() {
                return Ok(false);
            }
            runtime
                .try_send_bytes_with_source(Bytes::from(bytes), input_source)
                .map_err(|err| format!("terminal attach alternate scroll input failed: {err}"))?;
            return Ok(true);
        }
        Some(crate::pane::WheelRouting::HostScroll) => {
            match direction {
                AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
                AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
            }
            return Ok(true);
        }
        None => match direction {
            AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(false)
}

/// Returns whether input was actually enqueued, not whether it was a new
/// interaction. Raw key/mouse releases still need delivery to the child.
pub(super) fn apply_terminal_attach_input(
    runtime: &crate::terminal::TerminalRuntime,
    data: Vec<u8>,
    input_source: InputSource,
) -> Result<bool, String> {
    runtime.scroll_reset();
    if let Some(text) = crate::raw_input::complete_text_bracketed_paste(&data) {
        runtime
            .try_send_paste_with_source(text.to_owned(), input_source)
            .map_err(|err| format!("terminal attach paste failed: {err}"))
    } else {
        if data.is_empty() {
            return Ok(false);
        }
        runtime
            .try_send_bytes_with_source(Bytes::from(data), input_source)
            .map_err(|err| format!("terminal attach input failed: {err}"))?;
        Ok(true)
    }
}

#[cfg(test)]
pub(crate) fn test_apply_client_pane_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
) -> Result<(), String> {
    apply_client_pane_input_events(
        runtime,
        events,
        InputSource::Client {
            connection_id: 0,
            principal: None,
        },
    )
    .map(|_| ())
}

pub(super) fn apply_client_pane_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
    input_source: InputSource,
) -> Result<bool, String> {
    apply_client_terminal_input_events(runtime, events, true, input_source)
}

pub(super) fn apply_client_popup_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
    input_source: InputSource,
) -> Result<bool, String> {
    apply_client_terminal_input_events(runtime, events, false, input_source)
}

fn apply_client_terminal_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
    host_page_keys: bool,
    input_source: InputSource,
) -> Result<bool, String> {
    let mut accepted = false;
    for event in events {
        if let ClientPaneInputEvent::Mouse {
            kind,
            position,
            modifiers,
            lines,
            ..
        } = event
        {
            let kind = kind.to_crossterm();
            let modifiers = KeyModifiers::from_bits_truncate(*modifiers);
            let position = match position {
                crate::protocol::ClientMousePosition::Cell { column, row } => {
                    crate::input::mouse::Position::Cell {
                        column: *column,
                        row: *row,
                    }
                }
                crate::protocol::ClientMousePosition::Pixels { x, y, column, row } => {
                    if runtime.sgr_pixel_mouse_enabled() {
                        crate::input::mouse::Position::Pixels { x: *x, y: *y }
                    } else {
                        crate::input::mouse::Position::Cell {
                            column: *column,
                            row: *row,
                        }
                    }
                }
            };
            let bytes = match kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let direction = if kind == MouseEventKind::ScrollUp {
                        AttachScrollDirection::Up
                    } else {
                        AttachScrollDirection::Down
                    };
                    accepted |= apply_scroll(
                        runtime,
                        AttachScrollSource::Wheel,
                        direction,
                        (*lines).max(1),
                        position,
                        modifiers.bits(),
                        input_source.clone(),
                    )?;
                    continue;
                }
                MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => runtime
                    .encode_mouse_wheel(kind, position, modifiers)
                    .unwrap_or_default(),
                MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                    runtime
                        .encode_mouse_button(kind, position, modifiers)
                        .unwrap_or_default()
                }
                MouseEventKind::Moved => runtime
                    .encode_mouse_motion(kind, position, modifiers)
                    .unwrap_or_default(),
            };
            if !bytes.is_empty() {
                if kind != MouseEventKind::Moved {
                    runtime.scroll_reset();
                }
                runtime
                    .try_send_bytes_with_source(Bytes::from(bytes), input_source.clone())
                    .map_err(|err| format!("targeted pane mouse input failed: {err}"))?;
                accepted = true;
            }
            continue;
        }

        match event.to_raw_input_event() {
            crate::raw_input::RawInputEvent::Key(key) => {
                let key_event = key.as_key_event();
                if host_page_keys
                    && matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
                    && key_event.modifiers.is_empty()
                    && runtime.plain_page_keys_use_host_scrollback() == Some(true)
                {
                    match key_event.kind {
                        KeyEventKind::Release => continue,
                        KeyEventKind::Press | KeyEventKind::Repeat => {
                            let lines = runtime.current_size().0.max(1) as usize;
                            if key_event.code == KeyCode::PageUp {
                                runtime.scroll_up(lines);
                            } else {
                                runtime.scroll_down(lines);
                            }
                            accepted = true;
                            continue;
                        }
                    }
                }

                runtime.scroll_reset();
                let bytes = runtime.encode_terminal_key(key);
                if !bytes.is_empty() {
                    runtime
                        .try_send_bytes_with_source(Bytes::from(bytes), input_source.clone())
                        .map_err(|err| format!("targeted pane key input failed: {err}"))?;
                    accepted = true;
                }
            }
            crate::raw_input::RawInputEvent::Text(text) => {
                runtime.scroll_reset();
                let bytes = text.as_str().as_bytes();
                if bytes.is_empty() {
                    continue;
                }
                runtime
                    .try_send_bytes_with_source(Bytes::copy_from_slice(bytes), input_source.clone())
                    .map_err(|err| format!("targeted pane text input failed: {err}"))?;
                accepted = true;
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                runtime.scroll_reset();
                accepted |= runtime
                    .try_send_paste_with_source(text, input_source.clone())
                    .map_err(|err| format!("targeted pane paste failed: {err}"))?;
            }
            crate::raw_input::RawInputEvent::Mouse(_)
            | crate::raw_input::RawInputEvent::OuterFocusGained
            | crate::raw_input::RawInputEvent::OuterFocusLost
            | crate::raw_input::RawInputEvent::HostDefaultColor { .. }
            | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
            | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
            | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
            | crate::raw_input::RawInputEvent::Unsupported => {
                return Err("non-pane input reached targeted pane input".to_owned());
            }
        }
    }
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_attach_stale_geometry_falls_back_to_the_canonical_cell() {
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"");
        let position = crate::protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };

        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (20, 5),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                position,
                Some(crate::protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            Some(crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (20, 5),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                crate::protocol::ClientMousePosition::Pixels {
                    x: 120,
                    y: 80,
                    column: 12,
                    row: 4,
                },
                Some(crate::protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            None
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                false,
                false,
                crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                None,
            ),
            Some(crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
    }

    #[test]
    fn ineligible_shell_pixel_mouse_uses_its_canonical_cell_position() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, false, (5, 20), Some((200, 100)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn input_receipts_distinguish_accepted_ignored_and_rejected() {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
        assert_eq!(
            apply_client_pane_input_events(
                &runtime,
                &[ClientPaneInputEvent::TextCommit("x".to_owned())],
                InputSource::Unknown,
            ),
            Ok(true)
        );
        assert_eq!(
            input_rx.try_recv().expect("accepted input"),
            Bytes::from_static(b"x")
        );

        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
        runtime.test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            apply_client_pane_input_events(
                &runtime,
                &[ClientPaneInputEvent::Mouse {
                    kind: crate::protocol::ClientMouseKind::Moved,
                    position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                    geometry: None,
                    modifiers: 0,
                    lines: 1,
                }],
                InputSource::Unknown,
            ),
            Ok(false)
        );
        assert!(
            input_rx.try_recv().is_err(),
            "ignored motion must not enqueue"
        );

        let (runtime, _input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
        runtime
            .try_send_bytes(Bytes::from_static(b"occupied"))
            .expect("fill input queue");
        assert!(apply_client_pane_input_events(
            &runtime,
            &[ClientPaneInputEvent::TextCommit("rejected".to_owned())],
            InputSource::Unknown,
        )
        .is_err());
    }

    #[tokio::test]
    async fn terminal_attach_release_receipts_preserve_exact_bytes() {
        for packet in [
            b"\x1b[97;1:3u".as_slice(),
            b"\x1b[97;1:3u\x1b[98;1:3u",
            b"\x1b[97;1:3u\x1b[<0;3;2m",
            b"\x1b[97;1:3ux", // Mixed input is also delivered as one exact packet.
            b"\x1b\x1b\x1b\x1b",
            b"\x1b\x1b\x1b\x1btext",
            b"\x1b\x1b\x1b\x1b[98;1:3u",
            b"\x1b[97;1:3u\x1b\x1b\x1b\x1b",
            b"\x1b[<0;3;2;\x1b[98;1:1um",
            b"\x1b[97;1:3u\x1b[<0;3;2;999m",
        ] {
            let (runtime, mut input_rx) =
                crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
            assert_eq!(
                apply_terminal_attach_input(&runtime, packet.to_vec(), InputSource::Unknown),
                Ok(true),
                "successful enqueue must not be confused with interaction: {packet:?}"
            );
            assert_eq!(
                input_rx.try_recv().expect("raw release or mixed packet"),
                Bytes::copy_from_slice(packet)
            );
            assert!(input_rx.try_recv().is_err(), "one packet, one enqueue");
        }
    }

    #[tokio::test]
    async fn terminal_attach_rejected_packets_do_not_report_an_enqueue() {
        for packet in [
            b"\x1b[97;1:3u".as_slice(),
            b"\x1b[97;1:3ux",
            b"\x1b[200~\x1b[97;1:3u\x1b[201~",
            b"\x1b\x1b\x1b\x1b",
            b"\x1b\x1b\x1b\x1btext",
            b"\x1b\x1b\x1b\x1b[98;1:3u",
            b"\x1b[97;1:3u\x1b\x1b\x1b\x1b",
            b"\x1b[<0;3;2;\x1b[98;1:1um",
            b"\x1b[97;1:3u\x1b[<0;3;2;999m",
        ] {
            let (runtime, mut input_rx) =
                crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
            runtime
                .try_send_bytes(Bytes::from_static(b"occupied"))
                .expect("fill input queue");
            assert!(
                apply_terminal_attach_input(&runtime, packet.to_vec(), InputSource::Unknown)
                    .is_err()
            );
            assert_eq!(
                input_rx.try_recv().expect("previous queue contents"),
                Bytes::from_static(b"occupied")
            );
            assert!(input_rx.try_recv().is_err(), "rejected packet not queued");
        }
    }

    #[tokio::test]
    async fn empty_paste_receipt_matches_actual_enqueued_payload() {
        let empty_paste = b"\x1b[200~\x1b[201~".to_vec();
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
        assert_eq!(
            apply_terminal_attach_input(&runtime, empty_paste.clone(), InputSource::Unknown),
            Ok(false)
        );
        assert!(input_rx.try_recv().is_err());

        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, 1);
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        assert_eq!(
            apply_terminal_attach_input(&runtime, empty_paste, InputSource::Unknown),
            Ok(true)
        );
        assert_eq!(
            input_rx.try_recv().expect("bracketed paste wrappers"),
            Bytes::from_static(b"\x1b[200~\x1b[201~")
        );
    }

    #[test]
    fn eligible_shell_pixel_mouse_remains_exact() {
        let position = crate::protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position,
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, true, (5, 20), Some((200, 100)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: current,
                ..
            }] if *current == position
        ));
    }

    #[test]
    fn stale_shell_pixel_geometry_downgrades_to_its_canonical_cell() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, true, (6, 20), Some((200, 120)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                geometry: None,
                ..
            }]
        ));
    }
}
