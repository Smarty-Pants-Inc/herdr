use super::*;

const ALICE: u64 = 31;
const DIRECT: u64 = 90;
const CAPACITY: usize = 64;

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
        media_capable: true,
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
        stream_active: None,
    });
    let response: serde_json::Value =
        serde_json::from_str(&response.try_recv().expect("immediate last-input response")).unwrap();
    assert_eq!(response["result"]["type"], "pane_last_input");
    response["result"]["last_input"].clone()
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

        // Synthetic clock checks media owner/reference and the unchanged 10-second age.
        let (respond_to, _response) = std::sync::mpsc::channel();
        let actions = server.media.open(
            "fresh-owner".into(),
            respond_to,
            pane,
            |_| true,
            owner_at + Duration::from_secs(9),
        );
        assert!(actions.iter().any(|action| matches!(action,
            crate::server::media::MediaAction::Send {
                client_id: ALICE,
                control: crate::protocol::media::MediaControl::Open(open),
            } if open.pane_id == pane_id
        )));
        let (respond_to, response) = std::sync::mpsc::channel();
        let actions = server.media.open(
            "stale-owner".into(),
            respond_to,
            pane,
            |_| true,
            owner_at + Duration::from_secs(11),
        );
        server.perform_media_actions(actions);
        let stale: serde_json::Value =
            serde_json::from_str(&response.try_recv().expect("expired owner response")).unwrap();
        assert_eq!(stale["error"]["code"], "media_no_client");

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
