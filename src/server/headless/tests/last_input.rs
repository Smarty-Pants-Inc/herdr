use super::*;

pub(super) fn connect(
    server: &mut HeadlessServer,
    client_id: u64,
    user: Option<&str>,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, control, _render) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        client_id,
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
        client_id,
        user: user.map(str::to_owned),
    });
    control
}

fn request(server: &mut HeadlessServer, method: api::schema::Method) -> serde_json::Value {
    let (respond_to, response) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: crate::api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "attribution".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    serde_json::from_str(&response.try_recv().expect("immediate response")).unwrap()
}

pub(super) fn last_input(server: &mut HeadlessServer, pane: &str) -> serde_json::Value {
    let response = request(
        server,
        api::schema::Method::PaneLastInput(api::schema::PaneLastInputParams { pane: pane.into() }),
    );
    assert_eq!(response["result"]["type"], "pane_last_input");
    response["result"]["last_input"].clone()
}

pub(super) fn type_into(server: &mut HeadlessServer, client_id: u64, pane_id: &str) {
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id,
        pane_id: pane_id.into(),
        events: vec![protocol::ClientPaneInputEvent::TextCommit("hello".into())],
    });
}

fn setup() -> (
    HeadlessServer,
    String,
    crate::terminal::TerminalId,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    setup_with_channel_capacity(64)
}

pub(super) fn setup_with_channel_capacity(
    channel_capacity: usize,
) -> (
    HeadlessServer,
    String,
    crate::terminal::TerminalId,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("attribution");
    let pane = workspace.tabs[0].root_pane;
    let (runtime, input) =
        crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, channel_capacity);
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
    // Use the real server-owned registry, including guarded input's pinned lookup.
    // A legacy pane-keyed test override would bypass the path this test must prove.
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    (server, pane_id, terminal_id, input)
}

// A synthetic clock proves both the routing reference and the unchanged owner age
// without sleeps or access to the broker's private owner record.
pub(super) fn assert_media_owner_and_age(
    server: &mut HeadlessServer,
    pane_ref: &str,
    client_id: u64,
    owner_at: Instant,
) {
    let (_, pane) = server.app.parse_pane_id(pane_ref).unwrap();
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
            client_id: owner,
            control: crate::protocol::media::MediaControl::Open(open),
        } if *owner == client_id && open.pane_id == pane_ref
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
}

#[tokio::test]
async fn accepted_client_api_direct_and_clipboard_input_have_explicit_sender_semantics() {
    let (mut server, pane_id, terminal_id, _input) = setup();
    let _alice = connect(&mut server, 31, Some("Al\u{1b}ice"));
    let _bob = connect(&mut server, 32, Some("Bob"));
    let _anonymous = connect(&mut server, 33, None);
    assert!(last_input(&mut server, &pane_id).is_null());
    type_into(&mut server, 31, &pane_id);
    let alice = last_input(&mut server, &pane_id);
    assert_eq!(alice["user"], "Alice");
    assert_eq!(alice["client_id"], 31);
    assert!(alice["at"].as_u64().unwrap() > 1_000_000_000_000);
    server.handle_server_event(ServerEvent::ClientShellFocus {
        client_id: 32,
        focused: true,
    });
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 32,
        pane_id: pane_id.clone(),
        events: vec![protocol::ClientPaneInputEvent::Key {
            code: protocol::ClientKeyCode::Enter,
            modifiers: 0,
            kind: protocol::ClientKeyKind::Release,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: true,
            physical_key_id: None,
            windows_record: None,
        }],
    });
    assert_eq!(
        last_input(&mut server, &pane_id),
        alice,
        "focus and key releases do not claim ownership"
    );
    type_into(&mut server, 32, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");
    type_into(&mut server, 33, &pane_id);
    let anonymous = last_input(&mut server, &pane_id);
    assert!(anonymous["user"].is_null());
    assert_eq!(anonymous["client_id"], 33);
    assert!(anonymous["at"].as_u64().unwrap() > 0);

    type_into(&mut server, 31, &pane_id);
    let before = last_input(&mut server, &pane_id);
    // A fail-closed guard rejection does not erase accepted client input.
    let rejected: api::schema::Request = serde_json::from_value(serde_json::json!({
        "id":"reject", "method":"pane.send_input_guarded",
        "params":{"pane_id":pane_id,"text":"bad", "expected_terminal":"wrong-terminal"}
    }))
    .unwrap();
    assert!(request(&mut server, rejected.method).get("error").is_some());
    assert_eq!(last_input(&mut server, &pane_id), before);
    for (method, params) in [
        (
            "pane.send_text",
            serde_json::json!({"pane_id":pane_id,"text":"API"}),
        ),
        (
            "pane.send_keys",
            serde_json::json!({"pane_id":pane_id,"keys":["enter"]}),
        ),
        (
            "pane.send_input",
            serde_json::json!({"pane_id":pane_id,"text":"API"}),
        ),
        (
            "pane.send_input_guarded",
            serde_json::json!({"pane_id":pane_id,"text":"API","expected_terminal":terminal_id.as_str()}),
        ),
    ] {
        type_into(&mut server, 31, &pane_id);
        let api_request: api::schema::Request =
            serde_json::from_value(serde_json::json!({"id":"api","method":method,"params":params}))
                .unwrap();
        assert_eq!(
            request(&mut server, api_request.method)["result"]["type"],
            "ok",
            "{method}"
        );
        assert!(
            last_input(&mut server, &pane_id).is_null(),
            "{method} must not borrow Alice"
        );
    }
    // The API input cleared attribution, not the microphone routing owner.
    let (tx, _rx) = std::sync::mpsc::channel();
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    let actions = server
        .media
        .open("media-owner".into(), tx, pane, |_| true, Instant::now());
    assert!(actions.iter().any(|action| matches!(
        action,
        crate::server::media::MediaAction::Send {
            client_id: 31,
            control: crate::protocol::media::MediaControl::Open(_),
        }
    )));
    type_into(&mut server, 31, &pane_id);
    assert!(server.paste_client_clipboard_image_path(
        32,
        protocol::ClientClipboardImageTarget::Pane(pane_id.clone()),
        "/tmp/image.png".into()
    ));
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");

    // Anonymous direct attach writes invalidate only attribution, not media's owner.
    server.clients.insert(
        90,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalAttach {
                terminal_id: terminal_id.as_str().to_owned(),
            },
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            0,
            protocol::RenderEncoding::TerminalAnsi,
            None,
        ),
    );
    server.handle_server_event(ServerEvent::ClientInput {
        client_id: 90,
        data: b"direct".to_vec(),
    });
    assert!(last_input(&mut server, &pane_id).is_null());
    type_into(&mut server, 31, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Alice");
    server.paste_client_clipboard_image_path(
        90,
        protocol::ClientClipboardImageTarget::DirectTerminal,
        "/tmp/image.png".into(),
    );
    assert!(last_input(&mut server, &pane_id).is_null());
    type_into(&mut server, 31, &pane_id);
    server.remove_client_and_resize_if_needed(31);
    assert!(last_input(&mut server, &pane_id).is_null());
    let missing = request(
        &mut server,
        api::schema::Method::PaneLastInput(api::schema::PaneLastInputParams {
            pane: "p_missing".into(),
        }),
    );
    assert_eq!(missing["error"]["code"], "pane_not_found");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_input_attributes_only_accepted_prefix() {
    let (mut server, pane_id, terminal_id, mut input) = setup_with_channel_capacity(2);
    let _bob = connect(&mut server, 32, Some("Bob"));
    let _alice = connect(&mut server, 31, Some("Alice"));

    // A wholly rejected Alice event must preserve the last accepted Bob input.
    type_into(&mut server, 32, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");
    let runtime = server.app.terminal_runtimes.get(&terminal_id).unwrap();
    runtime.try_send_bytes(Bytes::from_static(b"fill")).unwrap();
    type_into(&mut server, 31, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");

    // Accept Alice's first event but reject the second: the prefix still wins.
    input.try_recv().expect("free one queue slot");
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 31,
        pane_id: pane_id.clone(),
        events: vec![
            protocol::ClientPaneInputEvent::TextCommit("first".into()),
            protocol::ClientPaneInputEvent::TextCommit("second".into()),
        ],
    });
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Alice");
    assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
    assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"first"));
    assert!(
        input.try_recv().is_err(),
        "rejected suffix was not enqueued"
    );

    // An accepted text prefix followed by ignored DEC1000 motion still wins;
    // the ignored suffix must neither erase the prefix nor consume a queue slot.
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 32,
        pane_id: pane_id.clone(),
        events: vec![
            protocol::ClientPaneInputEvent::TextCommit("prefix".into()),
            protocol::ClientPaneInputEvent::Mouse {
                kind: protocol::ClientMouseKind::Moved,
                position: protocol::ClientMousePosition::Cell { column: 0, row: 0 },
                geometry: None,
                modifiers: 0,
                lines: 1,
            },
        ],
    });
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");
    assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"prefix"));
    assert!(input.try_recv().is_err());

    // Sender identity is not sticky after either kind of suffix.
    type_into(&mut server, 31, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Alice");
    assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"hello"));
    type_into(&mut server, 32, &pane_id);
    assert_eq!(last_input(&mut server, &pane_id)["user"], "Bob");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn rejected_api_enqueue_keeps_attribution_and_empty_input_does_not_invalidate() {
    let (mut server, pane_id, terminal_id, mut input) = setup();
    let _client = connect(&mut server, 31, Some("Alice"));
    type_into(&mut server, 31, &pane_id);
    let before = last_input(&mut server, &pane_id);
    let empty: api::schema::Request = serde_json::from_value(serde_json::json!({
        "id":"empty", "method":"pane.send_input", "params":{"pane_id":pane_id}
    }))
    .unwrap();
    assert_eq!(request(&mut server, empty.method)["result"]["type"], "ok");
    assert_eq!(last_input(&mut server, &pane_id), before);
    let runtime = server.app.terminal_runtimes.get(&terminal_id).unwrap();
    for _ in 0..64 {
        let _ = runtime.try_send_bytes(Bytes::from_static(b"fill"));
    }
    let text: api::schema::Request = serde_json::from_value(serde_json::json!({
        "id":"full", "method":"pane.send_text", "params":{"pane_id":pane_id,"text":"API"}
    }))
    .unwrap();
    assert_eq!(
        request(&mut server, text.method.clone())["error"]["code"],
        "pane_send_failed"
    );
    assert_eq!(last_input(&mut server, &pane_id), before);
    input.try_recv().expect("free one queue slot");
    assert_eq!(request(&mut server, text.method)["result"]["type"], "ok");
    assert!(last_input(&mut server, &pane_id).is_null());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn api_input_receipts_are_drained_for_every_pane_not_overwritten() {
    let (mut server, pane_id, _terminal_id, _input) = setup();
    let _client = connect(&mut server, 31, Some("Alice"));
    type_into(&mut server, 31, &pane_id);
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    // Multiple enqueues between server turns are receipts, not another identity tracker.
    let other_workspace = crate::workspace::Workspace::test_new("other");
    let other = other_workspace.tabs[0].root_pane;
    server
        .media
        .note_pane_input(31, other, "other", Instant::now());
    assert!(server.media.last_input(other).is_some());
    server.app.accepted_api_inputs.extend([pane, other]);
    assert_eq!(server.app.accepted_api_inputs.len(), 2);
    assert!(last_input(&mut server, &pane_id).is_null());
    assert!(server.media.last_input(other).is_none());
    assert!(server.app.accepted_api_inputs.is_empty());
    shutdown_test_runtimes(&mut server);
}
