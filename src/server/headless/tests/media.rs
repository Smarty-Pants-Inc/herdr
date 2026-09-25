use super::*;

use crate::protocol::media::{MediaControl, MediaOpen, MediaSdp};

fn connect_media_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    media_capable: bool,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, control, _render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            media_capable,
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
            writer,
        })
    );
    control
}

/// Controls reach the test channel through the writer's drain thread, so wait for them.
fn media_open_sent(
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    wait: Duration,
) -> Option<MediaOpen> {
    while let Ok(bytes) = control.recv_timeout(wait) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if let Some(Ok(MediaControl::Open(open))) = MediaControl::decode(&kind, &data) {
                return Some(open);
            }
        }
    }
    None
}

fn media_api(
    server: &mut HeadlessServer,
    method: api::schema::Method,
) -> std::sync::mpsc::Receiver<String> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: crate::api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "media".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    response_rx
}

fn json(rx: &std::sync::mpsc::Receiver<String>) -> serde_json::Value {
    serde_json::from_str(&rx.try_recv().expect("an API response")).expect("json response")
}

fn type_into(server: &mut HeadlessServer, client_id: u64, pane_id: &str) {
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id,
        pane_id: pane_id.to_owned(),
        events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
            "v".into(),
        )],
    });
}

#[tokio::test]
async fn pane_media_open_binds_to_the_client_that_typed_last() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("media");
    let pane = workspace.tabs[0].root_pane;
    let (runtime, _input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 4);
    workspace.insert_test_runtime(pane, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let pane_id = server.app.public_pane_id(0, pane).unwrap();

    let capable = connect_media_shell(&mut server, 31, true);
    let legacy = connect_media_shell(&mut server, 32, false);
    let pane_open = |pane_id: &str| {
        api::schema::Method::PaneMediaOpen(api::schema::PaneTarget {
            pane_id: pane_id.to_owned(),
        })
    };

    // Nobody typed into the pane yet.
    let rx = media_api(&mut server, pane_open(&pane_id));
    assert_eq!(json(&rx)["error"]["code"], "media_no_client");

    // The capable client types; the request is routed to it with its own pane id.
    type_into(&mut server, 31, &pane_id);
    let rx = media_api(&mut server, pane_open(&pane_id));
    assert!(rx.try_recv().is_err(), "the caller waits for the offer");
    let open = media_open_sent(&capable, Duration::from_secs(5))
        .expect("media.open sent to the typing client");
    assert_eq!(open.pane_id, pane_id);
    assert!(media_open_sent(&legacy, Duration::from_millis(200)).is_none());

    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id: 31,
        control: MediaControl::Offer(MediaSdp {
            session_id: open.session_id.clone(),
            sdp: "v=0 offer".into(),
        }),
    });
    let offer = json(&rx);
    assert_eq!(offer["result"]["type"], "media_offer");
    assert_eq!(offer["result"]["sdp"], "v=0 offer");

    // A newer keystroke from a client without media support wins the binding and fails clearly.
    type_into(&mut server, 32, &pane_id);
    let rx = media_api(&mut server, pane_open(&pane_id));
    assert_eq!(json(&rx)["error"]["code"], "media_unsupported_client");
    assert!(media_open_sent(&capable, Duration::from_millis(200)).is_none());

    // Closing the capable client ends its session.
    server.remove_client_and_resize_if_needed(31);
    let rx = media_api(
        &mut server,
        api::schema::Method::MediaState(api::schema::MediaSessionTarget {
            session_id: open.session_id.clone(),
        }),
    );
    let state = json(&rx);
    assert_eq!(state["result"]["type"], "media_session");
    assert_eq!(state["result"]["state"], "closed");
    assert_eq!(state["result"]["code"], "disconnected");

    let rx = media_api(&mut server, pane_open("w_missing:p9"));
    assert_eq!(json(&rx)["error"]["code"], "pane_not_found");
    shutdown_test_runtimes(&mut server);
}
