use super::*;

const ALICE: u64 = 31;
const DIRECT: u64 = 90;
const CAPACITY: usize = 64;
const KITTY_EVENT_TYPES: &[u8] = b"\x1b[>3u";
const DIRECT_PRESS: &[u8] = b"\x1b[97;1:1u";
const DIRECT_RELEASE: &[u8] = b"\x1b[97;1:3u";

type InputReceiver = tokio::sync::mpsc::Receiver<Bytes>;

fn setup() -> (
    HeadlessServer,
    String,
    crate::terminal::TerminalId,
    InputReceiver,
) {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("scroll-attribution");
    let pane = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let pane_id = server.app.public_pane_id(0, pane).unwrap();
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane)
        .unwrap()
        .clone();
    let (runtime, input) =
        crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, CAPACITY);
    // Exercise the real terminal-ID registry, not legacy pane-keyed overrides.
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    let (writer, _control, _render) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id: ALICE,
        surface_cols: 80,
        surface_rows: 23,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        direct_graphics: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active: true,
        surface_reuse: false,
        surface_delta: false,
        surface_scroll: false,
        media_capable: true,
        principal: None,
        writer,
    });
    server.handle_server_event(ServerEvent::ClientUser {
        client_id: ALICE,
        user: Some("Alice".into()),
    });
    attach(&mut server, DIRECT, &terminal_id);
    (server, pane_id, terminal_id, input)
}

fn attach(server: &mut HeadlessServer, client_id: u64, terminal: &crate::terminal::TerminalId) {
    server.clients.insert(
        client_id,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalAttach {
                terminal_id: terminal.as_str().to_owned(),
            },
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            0,
            protocol::RenderEncoding::TerminalAnsi,
            None,
        ),
    );
}

fn named_input(server: &mut HeadlessServer, pane: &str, input: &mut InputReceiver) {
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: ALICE,
        pane_id: pane.into(),
        events: vec![protocol::ClientPaneInputEvent::TextCommit("hello".into())],
    });
    assert_eq!(
        input.try_recv().expect("accepted named input"),
        Bytes::from_static(b"hello")
    );
    assert_eq!(last_input(server, pane)["user"], "Alice");
}

fn enable_kitty_event_types(
    server: &mut HeadlessServer,
    terminal_id: &crate::terminal::TerminalId,
) {
    let runtime = server
        .app
        .terminal_runtimes
        .get(terminal_id)
        .expect("registered terminal runtime");
    runtime.test_process_pty_bytes(KITTY_EVENT_TYPES);
    assert!(runtime.keyboard_protocol().reports_event_types());
}

fn alice_record_and_media_age(
    server: &mut HeadlessServer,
    pane_id: &str,
    input: &mut InputReceiver,
) -> (serde_json::Value, Instant) {
    named_input(server, pane_id, input);
    let (_, pane) = server.app.parse_pane_id(pane_id).unwrap();
    let owner_at = Instant::now() - Duration::from_secs(9);
    server.media.note_pane_input(ALICE, pane, pane_id, owner_at);
    let before = last_input(server, pane_id);
    assert_eq!(before["user"], "Alice");
    assert_eq!(before["client_id"], ALICE);
    assert!(before["at"].as_u64().is_some());
    (before, owner_at)
}

fn last_input(server: &mut HeadlessServer, pane: &str) -> serde_json::Value {
    let (respond_to, response) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: crate::api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "scroll-attribution".into(),
            method: api::schema::Method::PaneLastInput(api::schema::PaneLastInputParams {
                pane: pane.into(),
            }),
        },
        respond_to,
        response_write_complete: None,
    });
    let response: serde_json::Value =
        serde_json::from_str(&response.try_recv().expect("immediate last-input response")).unwrap();
    assert_eq!(response["result"]["type"], "pane_last_input");
    response["result"]["last_input"].clone()
}

#[tokio::test]
async fn direct_raw_key_release_preserves_complete_sender_and_media_age() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    enable_kitty_event_types(&mut server, &terminal_id);
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: DIRECT_PRESS.to_vec(),
    });
    assert_eq!(
        input
            .try_recv()
            .expect("accepted direct Kitty press")
            .as_ref(),
        DIRECT_PRESS
    );

    let (before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: DIRECT_RELEASE.to_vec(),
    });
    assert_eq!(
        input
            .try_recv()
            .expect("accepted direct Kitty release")
            .as_ref(),
        DIRECT_RELEASE,
        "release bytes must reach the child unchanged"
    );
    assert!(input.try_recv().is_err(), "release packet was not split");
    assert_eq!(last_input(&mut server, &pane_id), before);
    super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn direct_raw_release_packets_preserve_sender_when_repeated_or_coalesced() {
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![DIRECT_RELEASE.to_vec()],
        vec![DIRECT_RELEASE.to_vec(), DIRECT_RELEASE.to_vec()],
        vec![[DIRECT_RELEASE, DIRECT_RELEASE].concat()],
    ];

    for (case_index, packets) in cases.into_iter().enumerate() {
        let (mut server, pane_id, terminal_id, mut input) = setup();
        enable_kitty_event_types(&mut server, &terminal_id);
        let (before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);

        for (packet_index, packet) in packets.into_iter().enumerate() {
            server.handle_server_event(ServerEvent::ClientInput {
                client_id: DIRECT,
                data: packet.clone(),
            });
            assert_eq!(
                input
                    .try_recv()
                    .expect("accepted pure release packet")
                    .as_ref(),
                packet.as_slice(),
                "case {case_index}, packet {packet_index}"
            );
            assert_eq!(last_input(&mut server, &pane_id), before);
            super::last_input_tests::assert_media_owner_and_age(
                &mut server,
                &pane_id,
                ALICE,
                owner_at,
            );
        }
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn direct_raw_mixed_input_clears_only_after_accepted_enqueue() {
    let mixed_cases: &[&[u8]] = &[
        b"\x1b[97;1:3u\x1b[98;1:1u",
        b"\x1b[97;1:3uX",
        b"\x1b[97;1:3u\x1b[200~paste\x1b[201~",
    ];

    for (case_index, data) in mixed_cases.iter().enumerate() {
        let (mut server, pane_id, terminal_id, mut input) = setup();
        enable_kitty_event_types(&mut server, &terminal_id);
        let (_before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);
        server.handle_server_event(ServerEvent::ClientInput {
            client_id: DIRECT,
            data: data.to_vec(),
        });
        assert_eq!(
            input
                .try_recv()
                .expect("accepted mixed direct packet")
                .as_ref(),
            *data,
            "case {case_index} must remain one byte-for-byte packet"
        );
        assert!(input.try_recv().is_err(), "mixed packet was split");
        assert!(last_input(&mut server, &pane_id).is_null());
        super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
        shutdown_test_runtimes(&mut server);
    }

    let mixed = mixed_cases[0];
    let (mut server, pane_id, terminal_id, mut input) = setup();
    enable_kitty_event_types(&mut server, &terminal_id);
    let (before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);
    for _ in 0..CAPACITY {
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .try_send_bytes(Bytes::from_static(b"fill"))
            .expect("fill exact channel capacity");
    }
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: mixed.to_vec(),
    });
    assert_eq!(last_input(&mut server, &pane_id), before);
    super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);

    input.try_recv().expect("free one queue slot for retry");
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: mixed.to_vec(),
    });
    for _ in 1..CAPACITY {
        assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
    }
    assert_eq!(
        input.try_recv().expect("accepted mixed retry").as_ref(),
        mixed
    );
    assert!(input.try_recv().is_err(), "mixed retry packet was split");
    assert!(last_input(&mut server, &pane_id).is_null());
    super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
    shutdown_test_runtimes(&mut server);

    let (mut server, pane_id, terminal_id, mut input) = setup();
    enable_kitty_event_types(&mut server, &terminal_id);
    let (before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);
    // A closed receiver rejects the mixed packet without erasing Alice.
    // Keep this as a real ClientInput delivery through the registry path.
    drop(input);
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: mixed.to_vec(),
    });
    assert_eq!(last_input(&mut server, &pane_id), before);
    super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn direct_raw_malformed_and_long_packets_follow_enqueue_receipts() {
    // Both long packets fit the transport's inclusive 1 MiB input limit.
    const MAX_INPUT_BYTES: usize = 1024 * 1024;
    let mut release_then_esc = vec![0x1b; MAX_INPUT_BYTES];
    release_then_esc[..DIRECT_RELEASE.len()].copy_from_slice(DIRECT_RELEASE);
    let cases = [
        (
            "sgr-embedded-kitty-press",
            b"\x1b[<0;3;2;\x1b[98;1:1um".to_vec(),
        ),
        ("sgr-extra-parameter", b"\x1b[<0;3;2;999m".to_vec()),
        ("one-mib-esc", vec![0x1b; MAX_INPUT_BYTES]),
        ("release-then-esc-one-mib", release_then_esc),
    ];

    for (case, data) in cases {
        for queue in ["accepted", "full", "closed"] {
            let (mut server, pane_id, terminal_id, mut input) = setup();
            enable_kitty_event_types(&mut server, &terminal_id);
            let (before, owner_at) = alice_record_and_media_age(&mut server, &pane_id, &mut input);
            if queue == "full" {
                for _ in 0..CAPACITY {
                    server
                        .app
                        .terminal_runtimes
                        .get(&terminal_id)
                        .unwrap()
                        .try_send_bytes(Bytes::from_static(b"fill"))
                        .expect("fill exact channel capacity");
                }
            } else if queue == "closed" {
                input.close();
            }

            server.handle_server_event(ServerEvent::ClientInput {
                client_id: DIRECT,
                data: data.clone(),
            });
            if queue == "accepted" {
                let received = input.try_recv().expect(case);
                assert_eq!(received.len(), data.len(), "{case}: accepted length");
                // Boolean comparison avoids dumping a megabyte on failure.
                assert!(
                    received.as_ref() == data.as_slice(),
                    "{case}: changed bytes"
                );
                assert!(last_input(&mut server, &pane_id).is_null(), "{case}");
            } else {
                assert_eq!(
                    last_input(&mut server, &pane_id),
                    before,
                    "{case}: {queue} rejection must retain the complete Alice record"
                );
                if queue == "full" {
                    for _ in 0..CAPACITY {
                        assert_eq!(
                            input.try_recv().expect("original queue filler"),
                            Bytes::from_static(b"fill"),
                            "{case}: {queue} must not replace queued bytes"
                        );
                    }
                }
            }
            assert!(input.try_recv().is_err(), "{case}: {queue} extra packet");
            super::last_input_tests::assert_media_owner_and_age(
                &mut server,
                &pane_id,
                ALICE,
                owner_at,
            );
            shutdown_test_runtimes(&mut server);
        }
    }
}

fn scroll(
    server: &mut HeadlessServer,
    client_id: u64,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
) {
    server.handle_server_event(ServerEvent::ClientAttachScroll {
        client_id,
        source,
        direction,
        lines: 3,
        column: Some(0),
        row: Some(0),
        modifiers: 0,
    });
}

fn byte_scroll_cases() -> Vec<(&'static [u8], AttachScrollSource, &'static [u8])> {
    vec![
        (
            b"\x1b[?1049h",
            AttachScrollSource::PageKey {
                input: b"\x1b[6~".to_vec(),
            },
            b"\x1b[6~",
        ),
        (
            b"\x1b[?1049h\x1b[?1000h\x1b[?1006h",
            AttachScrollSource::Wheel,
            b"\x1b[<65;1;1M",
        ),
        (
            b"\x1b[?1049h\x1b[?1007h\x1b[?1l",
            AttachScrollSource::Wheel,
            b"\x1b[B",
        ),
    ]
}

#[tokio::test]
async fn accepted_direct_page_and_wheel_scroll_clear_display_only_and_named_input_restores() {
    for (modes, source, expected) in byte_scroll_cases() {
        let (mut server, pane_id, terminal_id, mut input) = setup();
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(modes);
        named_input(&mut server, &pane_id, &mut input);
        let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
        let owner_at = Instant::now() - Duration::from_secs(9);
        server
            .media
            .note_pane_input(ALICE, pane, &pane_id, owner_at);

        scroll(&mut server, DIRECT, source, AttachScrollDirection::Down);
        assert_eq!(
            input
                .try_recv()
                .expect("accepted direct scroll bytes")
                .as_ref(),
            expected
        );
        assert!(last_input(&mut server, &pane_id).is_null());
        assert!(
            input.try_recv().is_err(),
            "one scroll must enqueue only once"
        );

        super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);

        named_input(&mut server, &pane_id, &mut input);
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn rejected_direct_page_and_wheel_scroll_keep_prior_sender_until_accepted_retry() {
    for (modes, source, expected) in byte_scroll_cases() {
        let (mut server, pane_id, terminal_id, mut input) = setup();
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(modes);
        named_input(&mut server, &pane_id, &mut input);
        let before = last_input(&mut server, &pane_id);
        for _ in 0..CAPACITY {
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .try_send_bytes(Bytes::from_static(b"fill"))
                .expect("fill exact channel capacity");
        }
        scroll(
            &mut server,
            DIRECT,
            source.clone(),
            AttachScrollDirection::Down,
        );
        assert_eq!(
            last_input(&mut server, &pane_id),
            before,
            "queue rejection must retain Alice"
        );
        input.try_recv().expect("free exactly one slot");
        scroll(&mut server, DIRECT, source, AttachScrollDirection::Down);
        assert!(last_input(&mut server, &pane_id).is_null());
        for _ in 1..CAPACITY {
            assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
        }
        assert_eq!(input.try_recv().expect("accepted retry").as_ref(), expected);
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn closed_direct_scroll_queue_keeps_prior_sender() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?1049h");
    named_input(&mut server, &pane_id, &mut input);
    let before = last_input(&mut server, &pane_id);
    drop(input);
    scroll(
        &mut server,
        DIRECT,
        AttachScrollSource::PageKey {
            input: b"\x1b[6~".to_vec(),
        },
        AttachScrollDirection::Down,
    );
    assert_eq!(last_input(&mut server, &pane_id), before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn direct_host_scrollback_scrolls_without_injecting_child_bytes() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    let mut history = Vec::new();
    for line in 0..80 {
        history.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, receiver) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80, 24, 4096, &history, CAPACITY,
        );
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime)
        .unwrap()
        .shutdown();
    input.close();
    let mut input = receiver;
    named_input(&mut server, &pane_id, &mut input);
    for source in [
        AttachScrollSource::PageKey {
            input: b"\x1b[5~".to_vec(),
        },
        AttachScrollSource::Wheel,
    ] {
        scroll(&mut server, DIRECT, source, AttachScrollDirection::Up);
        // End the runtime read before invoking the mutable API query.
        let offset = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .scroll_metrics()
            .unwrap()
            .offset_from_bottom;
        assert!(offset >= 3);
        assert!(
            input.try_recv().is_err(),
            "host scrollback must not inject bytes"
        );
        assert!(last_input(&mut server, &pane_id).is_null());
        named_input(&mut server, &pane_id, &mut input);
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn direct_scroll_invalidates_only_the_current_attachment_after_rebinding() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    named_input(&mut server, &pane_id, &mut input);
    let before = last_input(&mut server, &pane_id);
    let other_workspace = crate::workspace::Workspace::test_new("other-scroll");
    let other_pane = other_workspace.tabs[0].root_pane;
    let other_terminal = other_workspace.terminal_id(other_pane).unwrap().clone();
    server.app.state.workspaces.push(other_workspace);
    server.app.state.ensure_test_terminals();
    let other_ref = server.app.public_pane_id(1, other_pane).unwrap();
    let (runtime, mut other_input) =
        crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, CAPACITY);
    runtime.test_process_pty_bytes(b"\x1b[?1049h");
    server
        .app
        .terminal_runtimes
        .insert(other_terminal.clone(), runtime);
    attach(&mut server, DIRECT + 1, &other_terminal);
    server
        .media
        .note_pane_input(ALICE, other_pane, &other_ref, Instant::now());
    scroll(
        &mut server,
        DIRECT + 1,
        AttachScrollSource::PageKey {
            input: b"\x1b[6~".to_vec(),
        },
        AttachScrollDirection::Down,
    );
    assert_eq!(
        other_input.try_recv().unwrap(),
        Bytes::from_static(b"\x1b[6~")
    );
    assert_eq!(last_input(&mut server, &pane_id), before);
    assert!(last_input(&mut server, &other_ref).is_null());

    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    server
        .media
        .note_pane_input(ALICE, other_pane, &other_ref, Instant::now());
    let other_before = last_input(&mut server, &other_ref);
    // Swap unique attachments, then reorder workspaces: never trust cached placement.
    server.app.state.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane)
        .unwrap()
        .attached_terminal_id = other_terminal.clone();
    server.app.state.workspaces[1].tabs[0]
        .panes
        .get_mut(&other_pane)
        .unwrap()
        .attached_terminal_id = terminal_id;
    server.app.state.workspaces.swap(0, 1);
    scroll(
        &mut server,
        DIRECT + 1,
        AttachScrollSource::PageKey {
            input: b"\x1b[6~".to_vec(),
        },
        AttachScrollDirection::Down,
    );
    assert_eq!(
        other_input.try_recv().unwrap(),
        Bytes::from_static(b"\x1b[6~")
    );
    assert!(last_input(&mut server, &pane_id).is_null());
    assert_eq!(last_input(&mut server, &other_ref), other_before);

    // The runtime can remain alive after its pane detaches. Such accepted input
    // must not invalidate an unrelated owner merely because history exists.
    server.app.state.workspaces.remove(1);
    scroll(
        &mut server,
        DIRECT + 1,
        AttachScrollSource::PageKey {
            input: b"\x1b[6~".to_vec(),
        },
        AttachScrollDirection::Down,
    );
    assert_eq!(
        other_input.try_recv().unwrap(),
        Bytes::from_static(b"\x1b[6~")
    );
    assert_eq!(last_input(&mut server, &other_ref), other_before);
    shutdown_test_runtimes(&mut server);
}

fn mouse(server: &mut HeadlessServer, client_id: u64, pane: &str) {
    if client_id == DIRECT {
        server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id,
            kind: protocol::ClientMouseKind::Moved,
            position: protocol::ClientMousePosition::Cell { column: 10, row: 5 },
            geometry: None,
            modifiers: 0,
            lines: 1,
        });
    } else {
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id,
            pane_id: pane.into(),
            events: vec![protocol::ClientPaneInputEvent::Mouse {
                kind: protocol::ClientMouseKind::Moved,
                position: protocol::ClientMousePosition::Cell { column: 10, row: 5 },
                geometry: None,
                modifiers: 0,
                lines: 1,
            }],
        });
    }
}

#[tokio::test]
async fn empty_clipboard_and_direct_input_preserve_complete_sender_and_media_age() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    let _bob = super::last_input_tests::connect(&mut server, 32, Some("Bob"));
    named_input(&mut server, &pane_id, &mut input);
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    let owner_at = Instant::now() - Duration::from_secs(9);
    server
        .media
        .note_pane_input(ALICE, pane, &pane_id, owner_at);
    let before = last_input(&mut server, &pane_id);
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?2004l");
    // No bracketed-paste mode: empty paste has no child bytes to accept.
    for (client_id, target) in [
        (
            32,
            protocol::ClientClipboardImageTarget::Pane(pane_id.clone()),
        ),
        (DIRECT, protocol::ClientClipboardImageTarget::DirectTerminal),
    ] {
        server.paste_client_clipboard_image_path(client_id, target, String::new());
        assert_eq!(last_input(&mut server, &pane_id), before);
        assert!(input.try_recv().is_err());
    }
    for data in [Vec::new(), b"\x1b[200~\x1b[201~".to_vec()] {
        server.handle_server_event(ServerEvent::ClientInput {
            client_id: DIRECT,
            data,
        });
        assert_eq!(last_input(&mut server, &pane_id), before);
        assert!(input.try_recv().is_err());
    }
    // Even with bracketed-paste enabled, a truly empty raw event is ignored.
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?2004h");
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: DIRECT,
        data: Vec::new(),
    });
    assert_eq!(last_input(&mut server, &pane_id), before);
    assert!(input.try_recv().is_err());
    super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn ignored_direct_and_bob_motion_preserve_complete_sender_and_media_age() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    let _bob = super::last_input_tests::connect(&mut server, 32, Some("Bob"));
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    named_input(&mut server, &pane_id, &mut input);
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    let owner_at = Instant::now() - Duration::from_secs(9);
    server
        .media
        .note_pane_input(ALICE, pane, &pane_id, owner_at);
    let before = last_input(&mut server, &pane_id);
    for client_id in [DIRECT, 32] {
        mouse(&mut server, client_id, &pane_id);
        assert!(
            input.try_recv().is_err(),
            "DEC1000 ignores unpressed motion"
        );
        assert_eq!(last_input(&mut server, &pane_id), before);
        super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn accepted_and_rejected_direct_and_bob_motion_follow_enqueue_receipts() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    let _bob = super::last_input_tests::connect(&mut server, 32, Some("Bob"));
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
    for client_id in [DIRECT, 32] {
        named_input(&mut server, &pane_id, &mut input);
        let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
        let owner_at = Instant::now() - Duration::from_secs(9);
        server
            .media
            .note_pane_input(ALICE, pane, &pane_id, owner_at);
        let before = last_input(&mut server, &pane_id);
        for _ in 0..CAPACITY {
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .unwrap()
                .try_send_bytes(Bytes::from_static(b"fill"))
                .unwrap();
        }
        mouse(&mut server, client_id, &pane_id);
        assert_eq!(last_input(&mut server, &pane_id), before);
        super::last_input_tests::assert_media_owner_and_age(&mut server, &pane_id, ALICE, owner_at);
        for _ in 0..CAPACITY {
            assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
        }
        assert!(
            input.try_recv().is_err(),
            "rejected motion enqueues no bytes"
        );
        let accepted_at = Instant::now();
        mouse(&mut server, client_id, &pane_id);
        assert_eq!(
            input.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[<35;11;6M")
        );
        assert!(input.try_recv().is_err());
        if client_id == DIRECT {
            assert!(last_input(&mut server, &pane_id).is_null());
            super::last_input_tests::assert_media_owner_and_age(
                &mut server,
                &pane_id,
                ALICE,
                owner_at,
            );
        } else {
            assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");
            super::last_input_tests::assert_media_owner_and_age(
                &mut server,
                &pane_id,
                32,
                accepted_at,
            );
        }
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn direct_invalidation_with_one_and_fifteen_panes_keeps_sole_owner_semantics() {
    for count in [1, 15] {
        let (mut server, pane_id, terminal_id, mut input) = setup();
        for index in 1..count {
            server
                .app
                .state
                .workspaces
                .push(crate::workspace::Workspace::test_new(&format!(
                    "extra-{index}"
                )));
        }
        server.app.state.ensure_test_terminals();
        assert!(!server.media.has_input_owners());
        server.invalidate_terminal_input_attribution(terminal_id.as_str());
        assert!(
            !server.media.has_input_owners(),
            "no-owner fast path must create no state"
        );
        named_input(&mut server, &pane_id, &mut input);
        let before = last_input(&mut server, &pane_id);
        assert!(server.media.has_input_owners());
        server.invalidate_terminal_input_attribution("unattached-terminal");
        assert_eq!(last_input(&mut server, &pane_id), before);
        server.invalidate_terminal_input_attribution(terminal_id.as_str());
        assert!(last_input(&mut server, &pane_id).is_null());
        assert!(
            server.media.has_input_owners(),
            "display invalidation must retain media owner"
        );
        named_input(&mut server, &pane_id, &mut input);
        shutdown_test_runtimes(&mut server);
    }
}
