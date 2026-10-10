use super::*;
use crate::pty::input_consumer::{InputSource, Principal};

// This is an accepted internal server fact, not a hello claim or evidence that
// a real SSH peer/root-owned principal map was authenticated by this fixture.
fn accepted_principal() -> Principal {
    Principal {
        smarty_id: "accepted-smarty-id".into(),
        display_name: "Accepted principal".into(),
    }
}

async fn assert_accepted_binding_before_queued_input(shell: bool, principal: Option<Principal>) {
    let mut server = test_headless_server();
    // Seed pure workspace state so a shell connection need not launch a PTY.
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("principal")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let client_id = 73;
    let expected_principal = principal.clone();
    let expected = InputSource::Client {
        connection_id: client_id,
        principal: principal.clone(),
    };
    let (writer, _control_rx, _render_rx) = test_client_writer();
    let connected = if shell {
        ServerEvent::ClientShellConnected {
            client_id,
            principal,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: false,
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            media_capable: true,
            writer,
        }
    } else {
        ServerEvent::ClientConnected {
            client_id,
            principal,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            writer,
        }
    };
    for event in [
        connected,
        // The transport forwards hello.user as ClientUser; it is presentation
        // metadata and must not replace the accepted principal.
        ServerEvent::ClientUser {
            client_id,
            user: Some("spoofed-smarty-id / Spoofed display name".into()),
        },
        ServerEvent::ClientInput {
            client_id,
            data: b"queued input".to_vec(),
        },
    ] {
        server.server_event_tx.try_send(event).unwrap();
    }

    let connected = server.server_event_rx.try_recv().unwrap();
    server.handle_server_event(connected);
    assert_eq!(server.clients.len(), 1);
    assert_eq!(server.clients[&client_id].principal, expected_principal);
    assert_eq!(server.client_input_source(client_id), expected);

    let user = server.server_event_rx.try_recv().unwrap();
    assert!(matches!(&user, ServerEvent::ClientUser { .. }));
    server.handle_server_event(user);
    assert_eq!(server.client_input_source(client_id), expected);

    // Inspect at the exact dequeue boundary: both connected-event paths have
    // already installed the binding before the input handler can run. Raw input
    // is intentionally a no-op without a terminal attachment in this unit test;
    // real write attribution is covered by the parent-owned transport/PTY tests.
    let input = server.server_event_rx.try_recv().unwrap();
    assert!(
        matches!(&input, ServerEvent::ClientInput { client_id: 73, data } if data == b"queued input")
    );
    assert_eq!(server.client_input_source(client_id), expected);
    server.handle_server_event(input);
    assert_eq!(server.client_input_source(client_id), expected);
    assert!(server.server_event_rx.try_recv().is_err());

    // Subsequent self-declared user changes (including clearing the value) do
    // not mutate the accepted owned principal, or upgrade an unmapped client.
    for user in [Some("another claimed identity".into()), None] {
        server.handle_server_event(ServerEvent::ClientUser { client_id, user });
        assert_eq!(server.client_input_source(client_id), expected);
    }
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn input_consumer_terminal_connection_binds_accepted_principal_before_queued_input() {
    assert_accepted_binding_before_queued_input(false, Some(accepted_principal())).await;
}

#[tokio::test]
async fn input_consumer_shell_connection_binds_accepted_principal_before_queued_input() {
    assert_accepted_binding_before_queued_input(true, Some(accepted_principal())).await;
}

#[tokio::test]
async fn input_consumer_unique_unmapped_connections_never_trust_hello_user() {
    assert_accepted_binding_before_queued_input(false, None).await;
    assert_accepted_binding_before_queued_input(true, None).await;
}

#[test]
fn input_consumer_missing_connection_metadata_is_unknown_not_unmapped_client() {
    let server = test_headless_server();
    assert_eq!(server.client_input_source(73), InputSource::Unknown);
}
