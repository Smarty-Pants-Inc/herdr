use super::*;

use crate::protocol::media::{MediaControl, MediaOpen, MediaPeerState, MediaSdp, MediaStateUpdate};

fn connect_media_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    media_capable: bool,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            peer_context: crate::api::ApiRequestContext::default(),
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            media_capable,
            principal: None,
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
    (control, render)
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

fn media_close_sent(
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    wait: Duration,
) -> Option<crate::protocol::media::MediaClose> {
    while let Ok(bytes) = control.recv_timeout(wait) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if let Some(Ok(MediaControl::Close(close))) = MediaControl::decode(&kind, &data) {
                return Some(close);
            }
        }
    }
    None
}

fn media_close_try_recv(
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Option<crate::protocol::media::MediaClose> {
    while let Ok(bytes) = control.try_recv() {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if let Some(Ok(MediaControl::Close(close))) = MediaControl::decode(&kind, &data) {
                return Some(close);
            }
        }
    }
    None
}

fn media_api_request_with_sender(
    server: &mut HeadlessServer,
    request: api::schema::Request,
    respond_to: std::sync::mpsc::Sender<String>,
) {
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        peer_pidfd: None,
        context: crate::api::ApiRequestContext::default(),
        request,
        respond_to,
        response_write_complete: None,
    });
}

fn media_api_request(
    server: &mut HeadlessServer,
    request: api::schema::Request,
) -> std::sync::mpsc::Receiver<String> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    media_api_request_with_sender(server, request, respond_to);
    response_rx
}

fn media_api(
    server: &mut HeadlessServer,
    method: api::schema::Method,
) -> std::sync::mpsc::Receiver<String> {
    media_api_request(
        server,
        api::schema::Request {
            id: "media".into(),
            method,
        },
    )
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

fn media_answer_sent(
    control: &std::sync::mpsc::Receiver<Vec<u8>>,
    wait: Duration,
) -> Option<MediaSdp> {
    while let Ok(bytes) = control.recv_timeout(wait) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if let Some(Ok(MediaControl::Answer(answer))) = MediaControl::decode(&kind, &data) {
                return Some(answer);
            }
        }
    }
    None
}

struct MediaServerWithClient {
    server: HeadlessServer,
    pane_id: String,
    control: std::sync::mpsc::Receiver<Vec<u8>>,
    render: std::sync::mpsc::Receiver<Vec<u8>>,
    _input: tokio::sync::mpsc::Receiver<Bytes>,
}

fn media_server_with_client(client_id: u64) -> MediaServerWithClient {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("media-flow");
    let pane = workspace.tabs[0].root_pane;
    let (runtime, input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 4);
    workspace.insert_test_runtime(pane, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let pane_id = server.app.public_pane_id(0, pane).unwrap();
    let (control, render) = connect_media_shell(&mut server, client_id, true);
    type_into(&mut server, client_id, &pane_id);
    MediaServerWithClient {
        server,
        pane_id,
        control,
        render,
        _input: input,
    }
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

    let (capable, _capable_render) = connect_media_shell(&mut server, 31, true);
    let (legacy, _legacy_render) = connect_media_shell(&mut server, 32, false);
    let pane_open = |pane_id: &str| {
        api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
            pane_id: pane_id.to_owned(),
            open_id: None,
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

#[tokio::test]
async fn pending_media_cancellation_enqueues_open_refusal_before_close_success() {
    for cancel_by_open_id in [true, false] {
        let client_id = if cancel_by_open_id { 41 } else { 42 };
        let MediaServerWithClient {
            mut server,
            pane_id,
            control,
            render: _render,
            _input,
        } = media_server_with_client(client_id);
        let open_id = format!("cancel-order-{client_id}");
        let (responses_tx, responses_rx) = std::sync::mpsc::channel();

        media_api_request_with_sender(
            &mut server,
            api::schema::Request {
                id: "pending-open".into(),
                method: api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
                    pane_id: pane_id.clone(),
                    open_id: Some(open_id.clone()),
                }),
            },
            responses_tx.clone(),
        );
        let open = media_open_sent(&control, Duration::from_secs(5)).expect("media.open sent");
        assert!(
            responses_rx.try_recv().is_err(),
            "the caller waits for the offer"
        );

        let (session_id, close_open_id) = if cancel_by_open_id {
            (None, Some(open_id))
        } else {
            (Some(open.session_id.clone()), None)
        };
        media_api_request_with_sender(
            &mut server,
            api::schema::Request {
                id: "cancel-open".into(),
                method: api::schema::Method::MediaClose(api::schema::MediaCloseParams {
                    session_id,
                    open_id: close_open_id,
                }),
            },
            responses_tx.clone(),
        );

        // One app response channel proves enqueue order only; separate API connections
        // use separate sockets and have no wire-arrival ordering guarantee.
        let refusal: serde_json::Value = serde_json::from_str(
            &responses_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("pending open refusal is enqueued during cancellation"),
        )
        .expect("open refusal is JSON");
        let close_success: serde_json::Value = serde_json::from_str(
            &responses_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("close success follows the pending open refusal"),
        )
        .expect("close success is JSON");
        assert_eq!(refusal["id"], "pending-open");
        assert_eq!(refusal["error"]["code"], "media_refused");
        assert_eq!(
            refusal["error"]["message"],
            "closed: the caller ended the session"
        );
        assert_eq!(close_success["id"], "cancel-open");
        assert_eq!(close_success["result"]["type"], "ok");

        let closed = media_close_sent(&control, Duration::from_secs(5))
            .expect("cancellation releases the client's media session");
        assert_eq!(closed.session_id, open.session_id);
        assert_eq!(closed.code.as_deref(), Some("closed"));
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn unknown_media_open_id_close_is_idempotent_and_refuses_a_later_open() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(43);
    let open_id = "cancel-before-open";

    for request_id in ["cancel-1", "cancel-2"] {
        let response = media_api_request(
            &mut server,
            api::schema::Request {
                id: request_id.into(),
                method: api::schema::Method::MediaClose(api::schema::MediaCloseParams {
                    session_id: None,
                    open_id: Some(open_id.into()),
                }),
            },
        );
        let response = json(&response);
        assert_eq!(response["id"], request_id);
        assert_eq!(response["result"]["type"], "ok");
    }

    let response = media_api_request(
        &mut server,
        api::schema::Request {
            id: "late-open".into(),
            method: api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
                pane_id,
                open_id: Some(open_id.into()),
            }),
        },
    );
    let response = json(&response);
    assert_eq!(response["id"], "late-open");
    assert_eq!(response["error"]["code"], "media_refused");
    assert_eq!(
        response["error"]["message"],
        "closed: cancelled before open"
    );
    assert!(
        media_open_sent(&control, Duration::from_millis(100)).is_none(),
        "a tombstoned open must never reach the client"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn media_open_id_cancels_pending_offered_and_connected_sessions_without_resurrection() {
    for stage in ["pending", "offered", "connected"] {
        let client_id = 51;
        let MediaServerWithClient {
            mut server,
            pane_id,
            control,
            render: _render,
            _input,
        } = media_server_with_client(client_id);
        let open_id = format!("cancel-{stage}");
        let open_response = media_api(
            &mut server,
            api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
                pane_id: pane_id.clone(),
                open_id: Some(open_id.clone()),
            }),
        );
        let open = media_open_sent(&control, Duration::from_secs(5)).expect("media.open sent");

        if stage == "pending" {
            assert!(open_response.try_recv().is_err());
            // Explicit open_id cancellation must not depend on the caller retaining its receiver.
            drop(open_response);
        } else {
            server.handle_server_event(ServerEvent::ClientMediaControl {
                client_id,
                control: MediaControl::Offer(MediaSdp {
                    session_id: open.session_id.clone(),
                    sdp: "v=0 offer".into(),
                }),
            });
            let offer = json(&open_response);
            assert_eq!(offer["result"]["type"], "media_offer");
            assert_eq!(offer["result"]["epoch"], 1);
        }

        if stage == "connected" {
            let answer_response = media_api(
                &mut server,
                api::schema::Method::MediaAnswer(api::schema::MediaAnswerParams {
                    session_id: open.session_id.clone(),
                    sdp: "v=0 answer".into(),
                }),
            );
            assert_eq!(json(&answer_response)["result"]["type"], "ok");
            let answer = media_answer_sent(&control, Duration::from_secs(5))
                .expect("answer sent to the media client");
            assert_eq!(answer.sdp, "v=0 answer");
            server.handle_server_event(ServerEvent::ClientMediaControl {
                client_id,
                control: MediaControl::State(MediaStateUpdate {
                    session_id: open.session_id.clone(),
                    state: MediaPeerState::Connected,
                    muted: false,
                    detail: None,
                }),
            });
            let state = media_api(
                &mut server,
                api::schema::Method::MediaState(api::schema::MediaSessionTarget {
                    session_id: open.session_id.clone(),
                }),
            );
            assert_eq!(json(&state)["result"]["state"], "connected");
        }

        let cancelled = media_api(
            &mut server,
            api::schema::Method::MediaClose(api::schema::MediaCloseParams {
                session_id: None,
                open_id: Some(open_id),
            }),
        );
        assert_eq!(json(&cancelled)["result"]["type"], "ok");
        let closed = media_close_sent(&control, Duration::from_secs(5))
            .expect("exactly one media.close for cancellation");
        assert_eq!(closed.session_id, open.session_id);
        assert_eq!(closed.code.as_deref(), Some("closed"));

        // A client offer arriving after cancellation is inert. An API answer is also
        // rejected, rather than reviving the closed session or reaching the client.
        server.handle_server_event(ServerEvent::ClientMediaControl {
            client_id,
            control: MediaControl::Offer(MediaSdp {
                session_id: open.session_id.clone(),
                sdp: "v=0 late offer".into(),
            }),
        });
        let late_answer = media_api(
            &mut server,
            api::schema::Method::MediaAnswer(api::schema::MediaAnswerParams {
                session_id: open.session_id.clone(),
                sdp: "v=0 late answer".into(),
            }),
        );
        assert_eq!(
            json(&late_answer)["error"]["code"],
            "media_session_not_found"
        );
        let state = media_api(
            &mut server,
            api::schema::Method::MediaState(api::schema::MediaSessionTarget {
                session_id: open.session_id,
            }),
        );
        let state = json(&state);
        assert_eq!(state["result"]["state"], "closed");
        assert_eq!(state["result"]["code"], "closed");
        assert!(media_answer_sent(&control, Duration::from_millis(100)).is_none());
        assert!(media_close_sent(&control, Duration::from_millis(100)).is_none());
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn duplicate_live_open_id_and_invalid_media_close_selectors_are_rejected_by_api() {
    let client_id = 52;
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(client_id);
    let first_response = media_api(
        &mut server,
        api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
            pane_id: pane_id.clone(),
            open_id: Some("voice-live".into()),
        }),
    );
    let first = media_open_sent(&control, Duration::from_secs(5)).expect("first open sent");

    let duplicate: api::schema::Request = serde_json::from_value(serde_json::json!({
        "id": "duplicate",
        "method": "pane.media_open",
        "params": {"pane_id": pane_id, "open_id": "voice-live"}
    }))
    .expect("decode duplicate open request");
    let duplicate_response = media_api_request(&mut server, duplicate);
    assert_eq!(json(&duplicate_response)["error"]["code"], "invalid_params");
    assert_eq!(
        json(&media_api(
            &mut server,
            api::schema::Method::MediaState(api::schema::MediaSessionTarget {
                session_id: first.session_id.clone(),
            }),
        ))["result"]["state"],
        "opening",
        "the live session remains intact"
    );
    assert!(media_open_sent(&control, Duration::from_millis(100)).is_none());

    for params in [
        serde_json::json!({}),
        serde_json::json!({
            "session_id": first.session_id,
            "open_id": "voice-live"
        }),
    ] {
        let invalid_close: api::schema::Request = serde_json::from_value(serde_json::json!({
            "id": "invalid-close",
            "method": "media.close",
            "params": params
        }))
        .expect("decode media.close request with invalid selector count");
        assert_eq!(
            json(&media_api_request(&mut server, invalid_close))["error"]["code"],
            "invalid_params"
        );
    }
    assert!(first_response.try_recv().is_err());

    let cancelled = media_api(
        &mut server,
        api::schema::Method::MediaClose(api::schema::MediaCloseParams {
            session_id: None,
            open_id: Some("voice-live".into()),
        }),
    );
    assert_eq!(json(&cancelled)["result"]["type"], "ok");
    let close = media_close_sent(&control, Duration::from_secs(5)).expect("cancel close sent");
    assert_eq!(close.session_id, first.session_id);
    assert_eq!(json(&first_response)["error"]["code"], "media_refused");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn failed_waiting_media_open_response_closes_through_headless_send_failure_path() {
    let client_id = 53;
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(client_id);
    let (respond_to, open_response) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        peer_pidfd: None,
        context: crate::api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "abandoned-open".into(),
            method: api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
                pane_id,
                open_id: Some("abandoned-open".into()),
            }),
        },
        respond_to,
        response_write_complete: None,
    });
    let open = media_open_sent(&control, Duration::from_secs(5)).expect("media.open sent");
    drop(open_response);

    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::Offer(MediaSdp {
            session_id: open.session_id.clone(),
            sdp: "v=0 offer".into(),
        }),
    });
    let close = media_close_sent(&control, Duration::from_secs(5))
        .expect("failed offer response triggers media.close");
    assert_eq!(close.session_id, open.session_id);
    assert_eq!(close.code.as_deref(), Some("abandoned"));
    let state = media_api(
        &mut server,
        api::schema::Method::MediaState(api::schema::MediaSessionTarget {
            session_id: open.session_id.clone(),
        }),
    );
    assert_eq!(json(&state)["result"]["code"], "abandoned");

    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::Offer(MediaSdp {
            session_id: open.session_id.clone(),
            sdp: "v=0 late offer".into(),
        }),
    });
    assert_eq!(
        json(&media_api(
            &mut server,
            api::schema::Method::MediaState(api::schema::MediaSessionTarget {
                session_id: open.session_id,
            }),
        ))["result"]["code"],
        "abandoned"
    );
    assert!(media_close_sent(&control, Duration::from_millis(100)).is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn headless_media_unanswered_deadline_expires_and_later_open_advances_epoch() {
    let client_id = 54;
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(client_id);
    let first_response = media_api(
        &mut server,
        api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
            pane_id: pane_id.clone(),
            open_id: Some("epoch-first".into()),
        }),
    );
    let first = media_open_sent(&control, Duration::from_secs(5)).expect("first open sent");
    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::Offer(MediaSdp {
            session_id: first.session_id.clone(),
            sdp: "v=0 offer".into(),
        }),
    });
    let offer = json(&first_response);
    assert_eq!(offer["result"]["epoch"], 1);
    let expiry = Instant::now() + crate::server::media::MEDIA_ANSWER_TIMEOUT;
    server.expire_media_sessions(expiry);
    let close = media_close_sent(&control, Duration::from_secs(5)).expect("unanswered close sent");
    assert_eq!(close.session_id, first.session_id);
    assert_eq!(close.code.as_deref(), Some("unanswered"));
    let state = media_api(
        &mut server,
        api::schema::Method::MediaState(api::schema::MediaSessionTarget {
            session_id: first.session_id.clone(),
        }),
    );
    assert_eq!(json(&state)["result"]["code"], "unanswered");

    let second_response = media_api(
        &mut server,
        api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
            pane_id,
            open_id: Some("epoch-second".into()),
        }),
    );
    let second = media_open_sent(&control, Duration::from_secs(5)).expect("second open sent");
    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::Offer(MediaSdp {
            session_id: second.session_id,
            sdp: "v=0 offer again".into(),
        }),
    });
    assert_eq!(json(&second_response)["result"]["epoch"], 2);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn successful_media_answer_prevents_unanswered_expiry_in_headless_server() {
    let client_id = 55;
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(client_id);
    let open_response = media_api(
        &mut server,
        api::schema::Method::PaneMediaOpen(api::schema::MediaOpenParams {
            pane_id,
            open_id: None,
        }),
    );
    let open = media_open_sent(&control, Duration::from_secs(5)).expect("media.open sent");
    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::Offer(MediaSdp {
            session_id: open.session_id.clone(),
            sdp: "v=0 offer".into(),
        }),
    });
    assert_eq!(json(&open_response)["result"]["epoch"], 1);

    let answer_response = media_api(
        &mut server,
        api::schema::Method::MediaAnswer(api::schema::MediaAnswerParams {
            session_id: open.session_id.clone(),
            sdp: "v=0 answer".into(),
        }),
    );
    assert_eq!(json(&answer_response)["result"]["type"], "ok");
    let answer = media_answer_sent(&control, Duration::from_secs(5)).expect("answer sent");
    assert_eq!(answer.sdp, "v=0 answer");
    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id,
        control: MediaControl::State(MediaStateUpdate {
            session_id: open.session_id.clone(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        }),
    });

    server.expire_media_sessions(
        Instant::now() + crate::server::media::MEDIA_ANSWER_TIMEOUT + Duration::from_secs(1),
    );
    let state = media_api(
        &mut server,
        api::schema::Method::MediaState(api::schema::MediaSessionTarget {
            session_id: open.session_id,
        }),
    );
    let state = json(&state);
    assert_eq!(state["result"]["state"], "connected");
    assert!(state["result"]["code"].is_null());
    assert!(media_close_sent(&control, Duration::from_millis(100)).is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn idle_headless_loop_wakes_for_media_deadlines_and_delivery_failure() {
    for mode in ["pending", "unanswered", "delivery"] {
        let client_id = 56;
        let MediaServerWithClient {
            mut server,
            pane_id,
            control,
            mut render,
            _input,
        } = media_server_with_client(client_id);
        // Keep the runtime's API channel open so a closed-channel branch cannot create a busy loop.
        let (api_keepalive, api_rx) = tokio::sync::mpsc::unbounded_channel();
        server.app.api_rx = api_rx;
        let api_tx = api_keepalive.clone();

        let target_deadline = Instant::now() + Duration::from_millis(200);
        let offer_at = match mode {
            "unanswered" => Some(target_deadline - crate::server::media::MEDIA_ANSWER_TIMEOUT),
            "delivery" => Some(Instant::now()),
            _ => None,
        };
        let opened_at = match offer_at {
            Some(offer_at) if mode == "unanswered" => {
                offer_at - crate::server::media::MEDIA_OPEN_TIMEOUT + Duration::from_secs(1)
            }
            Some(_) => Instant::now(),
            None => target_deadline - crate::server::media::MEDIA_OPEN_TIMEOUT,
        };
        let (delivery_marker, delivery_receiver) = if mode == "delivery" {
            let (sender, receiver) = std::sync::mpsc::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let (respond_to, open_response) = std::sync::mpsc::channel();
        let (_, pane) = server.app.parse_pane_id(&pane_id).expect("test pane id");
        let actions = server.media.open(
            format!("idle-{mode}"),
            respond_to,
            pane,
            None,
            delivery_receiver,
            |_| true,
            opened_at,
        );
        let session_id = actions
            .iter()
            .find_map(|action| match action {
                crate::server::media::MediaAction::Send {
                    control: MediaControl::Open(open),
                    ..
                } => Some(open.session_id.clone()),
                _ => None,
            })
            .expect("broker opened a session");
        server.perform_media_actions(actions);

        let expected_code = match mode {
            "pending" => "timeout",
            "unanswered" => "unanswered",
            "delivery" => "abandoned",
            _ => unreachable!(),
        };
        let mut pending_response = None;
        if let Some(offer_at) = offer_at {
            let actions = server.media.client_control(
                client_id,
                MediaControl::Offer(MediaSdp {
                    session_id: session_id.clone(),
                    sdp: "v=0 offer".into(),
                }),
                offer_at,
            );
            server.perform_media_actions(actions);
            let response = open_response
                .recv_timeout(Duration::from_secs(1))
                .expect("offer response delivered before idle wait");
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["result"]["type"], "media_offer");
            assert_eq!(response["result"]["epoch"], 1);
        } else {
            pending_response = Some(open_response);
        }

        let should_quit = server.should_quit.clone();
        let event_tx = server.server_event_tx.clone();
        let state_session_id = session_id.clone();
        let started = Instant::now();
        let render_rx = &mut render;
        let run_finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let run_finished_signal = std::sync::Arc::clone(&run_finished);
        let run_server = async {
            let result = server.run().await;
            run_finished_signal.store(true, Ordering::Release);
            result
        };
        let observe_and_shutdown = async move {
            let mut render_poll = tokio::time::interval(Duration::from_millis(10));
            if let Some(delivery_marker) = delivery_marker {
                // Let the server pass an empty receipt poll and reach idle. Signaling the
                // receipt below deliberately sends no HeadlessServer event.
                let marker_deadline = tokio::time::sleep(Duration::from_millis(250));
                tokio::pin!(marker_deadline);
                loop {
                    tokio::select! {
                        _ = &mut marker_deadline => break,
                        _ = render_poll.tick() => {
                            while render_rx.try_recv().is_ok() {}
                        }
                    }
                }
                delivery_marker
                    .send(())
                    .expect("the idle server retains its delivery receiver");
            }

            // Keep the fake client's render sink alive and drain it while the real
            // headless loop runs. In particular, don't let an ordered frame block
            // the writer thread behind the test channel's one-frame buffer.
            let close_deadline = tokio::time::sleep(Duration::from_secs(1));
            tokio::pin!(close_deadline);
            let close = loop {
                tokio::select! {
                    _ = &mut close_deadline => break None,
                    _ = render_poll.tick() => {
                        while render_rx.try_recv().is_ok() {}
                        if let Some(close) = media_close_try_recv(&control) {
                            break Some(close);
                        }
                    }
                }
            };
            let elapsed = started.elapsed();

            // Query while the server is still running: its shutdown check rejects API
            // requests once QuitSignal starts teardown, so this also proves that the
            // idle close was visible through the live API path before stopping it.
            let (respond_to, state_response) = std::sync::mpsc::channel();
            api_tx
                .send(api::ApiRequestMessage {
                    peer_pidfd: None,
                    context: crate::api::ApiRequestContext::default(),
                    request: api::schema::Request {
                        id: "idle-media-state".into(),
                        method: api::schema::Method::MediaState(api::schema::MediaSessionTarget {
                            session_id: state_session_id,
                        }),
                    },
                    respond_to,
                    response_write_complete: None,
                })
                .expect("the live API channel accepts the state query");
            let state_deadline = tokio::time::sleep(Duration::from_secs(1));
            tokio::pin!(state_deadline);
            let state: serde_json::Value = loop {
                tokio::select! {
                    _ = &mut state_deadline => panic!("live media-state API query timed out"),
                    _ = render_poll.tick() => {
                        while render_rx.try_recv().is_ok() {}
                        match state_response.try_recv() {
                            Ok(response) => {
                                break serde_json::from_str(&response)
                                    .expect("live media-state API response is JSON");
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                panic!("live media-state API response channel closed");
                            }
                        }
                    }
                }
            };

            should_quit.store(true, Ordering::Release);
            event_tx
                .send(ServerEvent::QuitSignal)
                .await
                .expect("wake the idle server to shut down");
            while !run_finished.load(Ordering::Acquire) {
                render_poll.tick().await;
                while render_rx.try_recv().is_ok() {}
            }
            while render_rx.try_recv().is_ok() {}
            (close, elapsed, state)
        };
        let joined = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(run_server, observe_and_shutdown)
        })
        .await;
        if joined.is_err() {
            server.should_quit.store(true, Ordering::Release);
            let _ = server.server_event_tx.try_send(ServerEvent::QuitSignal);
            shutdown_test_runtimes(&mut server);
            panic!("idle server did not reach its media close and shutdown");
        }
        let (run_result, (close, elapsed, state)) = joined.expect("run/observer completed");
        run_result.expect("headless runtime exits cleanly");
        drop(api_keepalive);
        let close = close.expect("idle timer sent media.close");
        assert_eq!(close.session_id, session_id);
        assert_eq!(close.code.as_deref(), Some(expected_code));
        if mode == "delivery" {
            assert!(
                elapsed >= Duration::from_millis(250),
                "receipt should be noticed by a bounded idle poll, elapsed {elapsed:?}"
            );
        } else {
            assert!(
                elapsed < Duration::from_secs(1),
                "synthetic media deadline should wake the idle loop, elapsed {elapsed:?}"
            );
        }
        if let Some(response) = pending_response {
            let response = response
                .try_recv()
                .expect("pending caller receives timeout response");
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], "media_timeout");
        }
        assert_eq!(state["result"]["state"], "closed");
        assert_eq!(state["result"]["code"], expected_code);
        shutdown_test_runtimes(&mut server);
    }
}
