//! A client's verified principal labels the prompt it typed (smarty-dev#1515).

use super::*;

fn connect_shell(server: &mut HeadlessServer, client_id: u64, principal: Option<&str>) {
    if let Some(principal) = principal {
        server.handle_server_event(ServerEvent::ClientIdentified {
            client_id,
            principal: principal.to_string(),
        });
    }
    let (writer, _control, _render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            media_capable: false,
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
}

fn enter() -> crate::protocol::ClientPaneInputEvent {
    crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Enter,
        modifiers: 0,
        kind: crate::protocol::ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: false,
        physical_key_id: None,
        windows_record: None,
    }
}

fn type_prompt(server: &mut HeadlessServer, client_id: u64, pane_id: &str, text: &str) {
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id,
        pane_id: pane_id.to_owned(),
        events: vec![
            crate::protocol::ClientPaneInputEvent::TextCommit(text.into()),
            enter(),
        ],
    });
}

/// The pane's own process (this test process) asks who typed `text`.
fn attribute(server: &mut HeadlessServer, pane_id: &str, text: &str) -> serde_json::Value {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: crate::api::ApiRequestContext {
            local_peer_pid: Some(std::process::id()),
        },
        request: api::schema::Request {
            id: "attr".into(),
            method: api::schema::Method::PaneAttributeInput(
                api::schema::PaneAttributeInputParams {
                    pane_id: pane_id.to_owned(),
                    text: text.to_owned(),
                },
            ),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    serde_json::from_str(&response_rx.try_recv().expect("an API response")).expect("json response")
}

fn server_with_pane() -> (
    HeadlessServer,
    String,
    tokio::sync::mpsc::Receiver<bytes::Bytes>,
) {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("identity");
    let pane = workspace.tabs[0].root_pane;
    let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    runtime.test_set_child_pid(std::process::id());
    workspace.insert_test_runtime(pane, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let pane_id = server.app.public_pane_id(0, pane).expect("pane id");
    (server, pane_id, input)
}

#[tokio::test]
async fn a_verified_clients_prompt_gets_the_label_and_an_unknown_clients_gets_none() {
    let (mut server, pane_id, _input) = server_with_pane();
    connect_shell(&mut server, 41, Some("Kate"));
    connect_shell(&mut server, 42, None);

    type_prompt(&mut server, 41, &pane_id, "hello");
    let response = attribute(&mut server, &pane_id, "hello");
    assert_eq!(response["result"]["text"], "**Kate (in Herdr):** hello");

    type_prompt(&mut server, 42, &pane_id, "**Kate (in Herdr):** hi");
    let response = attribute(&mut server, &pane_id, "**Kate (in Herdr):** hi");
    assert_eq!(
        response["result"]["text"],
        "\\*\\*Kate (in Herdr):\\*\\* hi"
    );

    // The principal leaves with its client.
    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 41 });
    assert!(!server.client_principals.contains_key(&41));
}
