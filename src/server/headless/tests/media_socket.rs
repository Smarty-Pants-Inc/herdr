//! JSON socket -> production API router -> headless broker integration.
//! Keep these for the transport/identity and microphone-release failure boundaries.
use super::*;
use crate::protocol::media::{MediaApplied, MediaDeviceControl, MediaInfo, MediaLevels};
use interprocess::os::unix::uds_local_socket;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

const SOCKET_BUDGET: Duration = Duration::from_secs(8);

struct Socket {
    client: Option<BufReader<UnixStream>>,
    running: Arc<AtomicBool>,
    done: mpsc::Receiver<std::io::Result<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Socket {
    fn new(tx: &api::ApiRequestSender, hub: &api::EventHub, request: Value) -> Self {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(SOCKET_BUDGET)).unwrap();
        writeln!(client, "{request}").unwrap();
        let server = uds_local_socket::Stream::from(server).into();
        let tx = tx.clone();
        let hub = hub.clone();
        let running = Arc::new(AtomicBool::new(true));
        let owner = running.clone();
        let (done_tx, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = api::test_handle_connection(server, &tx, &hub, &owner, None);
            let _ = done_tx.send(result);
        });
        Self {
            client: Some(BufReader::new(client)),
            running,
            done,
            worker: Some(worker),
        }
    }

    fn response(&mut self) -> Value {
        let mut line = String::new();
        self.client.as_mut().unwrap().read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("a complete JSON socket response")
    }

    fn assert_held(&self) {
        assert!(
            matches!(self.done.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "control must stay held until its own applied ack, timeout or session end"
        );
    }

    fn finish(&mut self) {
        self.done
            .recv_timeout(SOCKET_BUDGET)
            .expect("bounded API handler shutdown")
            .unwrap();
        self.worker.take().unwrap().join().unwrap();
    }

    fn disconnect(&mut self) {
        self.client.take();
        self.finish();
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.client.take();
        if let Some(worker) = self.worker.take() {
            let completed = self.done.recv_timeout(SOCKET_BUDGET).is_ok();
            if completed || worker.is_finished() {
                let _ = worker.join();
            } else if !std::thread::panicking() {
                panic!("socket handler outlived its test owner");
            }
        }
    }
}

fn media_server_with_client(client_id: u64) -> MediaServerWithClient {
    let mut fixture = super::media_server_with_client(client_id);
    // The broker-only fixture installs topology/runtime but no terminal metadata.
    // Real pane.get bootstrap needs the same complete state as workspace creation.
    fixture.server.app.state.ensure_test_terminals();
    fixture.server.app.state.assert_invariants_for_test();
    fixture
}

fn sockets(server: &mut HeadlessServer) -> api::ApiRequestSender {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = rx;
    tx
}

async fn service(server: &mut HeadlessServer) {
    let msg = tokio::time::timeout(Duration::from_secs(2), server.app.api_rx.recv())
        .await
        .unwrap()
        .expect("JSON request dispatched through the actual API router");
    server.handle_api_request_with_shutdown_check(msg);
}

fn request(id: &str, method: &str, params: Value) -> Value {
    json!({"id": id, "method": method, "params": params})
}

fn client_control(server: &mut HeadlessServer, client: u64, control: MediaControl) {
    server.handle_server_event(ServerEvent::ClientMediaControl {
        client_id: client,
        control,
    });
}

fn device_control(control: &mpsc::Receiver<Vec<u8>>, wait: Duration) -> Option<MediaDeviceControl> {
    while let Ok(bytes) = control.recv_timeout(wait) {
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if let Some(Ok(MediaControl::Control(control))) = MediaControl::decode(&kind, &data) {
                return Some(control);
            }
        }
    }
    None
}

fn applied(
    server: &mut HeadlessServer,
    client: u64,
    control: &MediaDeviceControl,
    muted: bool,
    suppressed: bool,
) {
    client_control(
        server,
        client,
        MediaControl::Applied(MediaApplied {
            session_id: control.session_id.clone(),
            control_id: control.control_id,
            input_muted: muted,
            output_suppressed: suppressed,
        }),
    );
}

async fn offered(
    server: &mut HeadlessServer,
    tx: &api::ApiRequestSender,
    pane: &str,
    control: &mpsc::Receiver<Vec<u8>>,
    client: u64,
    open_id: &str,
) -> MediaOpen {
    let mut socket = Socket::new(
        tx,
        &server.app.event_hub,
        request(
            "open",
            "pane.media_open",
            json!({"pane_id": pane, "open_id": open_id}),
        ),
    );
    service(server).await;
    let open = media_open_sent(control, Duration::from_secs(2)).unwrap();
    client_control(
        server,
        client,
        MediaControl::Offer(MediaSdp {
            session_id: open.session_id.clone(),
            sdp: "v=0 offer".into(),
        }),
    );
    socket.finish();
    let response = socket.response();
    assert_eq!(response["result"]["session_id"], open.session_id);
    assert_eq!(response["result"]["epoch"], 1);
    open
}

async fn subscribe(server: &mut HeadlessServer, tx: &api::ApiRequestSender, pane: &str) -> Socket {
    let mut socket = Socket::new(
        tx,
        &server.app.event_hub,
        request(
            "subscribe",
            "events.subscribe",
            json!({"subscriptions": [{"type": "media", "pane_id": pane}]}),
        ),
    );
    service(server).await; // canonical pane_get validation, not a stand-in response
    let response = socket.response();
    assert_eq!(
        response["result"]["type"], "subscription_started",
        "subscription bootstrap returned: {response}"
    );
    socket
}

#[tokio::test]
async fn json_controls_hold_correlate_in_order_and_work_before_answer() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(81);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 81, "preanswer").await;
    let mut first = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "outer-first",
            "media.control",
            json!({"session_id": open.session_id, "epoch": 1, "input_muted": true}),
        ),
    );
    service(&mut server).await;
    let a = device_control(&control, Duration::from_secs(2)).unwrap();
    let mut second = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "outer-second",
            "media.control",
            json!({"session_id": open.session_id, "output_suppressed": true}),
        ),
    );
    service(&mut server).await;
    let b = device_control(&control, Duration::from_secs(2)).unwrap();
    assert!(a.control_id < b.control_id);
    assert_eq!(a.input_muted, Some(true));
    assert_eq!(b.output_suppressed, Some(true));
    first.assert_held();
    second.assert_held();
    applied(&mut server, 999, &a, true, false); // wrong-client ack is inert
    first.assert_held();
    applied(&mut server, 81, &a, true, false);
    first.finish();
    let response = first.response();
    assert_eq!(response["id"], "outer-first");
    assert_eq!(
        response["result"],
        json!({"type": "media_controls_applied", "session_id": open.session_id,
        "request_id": "outer-first", "input_muted": true, "output_suppressed": false, "epoch": 1})
    );
    second.assert_held();
    applied(&mut server, 81, &a, false, false); // duplicate cannot settle another request
    second.assert_held();
    applied(&mut server, 81, &b, true, true);
    second.finish();
    assert_eq!(second.response()["result"]["request_id"], "outer-second");
    assert_eq!(
        server.media.state(&open.session_id).unwrap().state,
        api::schema::MediaSessionState::Offered
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_control_broker_timeout_wins_after_generic_app_timeout_and_keeps_session() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(82);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 82, "timeout").await;
    let mut socket = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "uncertain",
            "media.control",
            json!({"session_id": open.session_id, "input_muted": true}),
        ),
    );
    service(&mut server).await;
    let _control = device_control(&control, Duration::from_secs(2)).unwrap();
    // Deliberately let APP_RESPONSE_TIMEOUT's 5s budget pass before servicing
    // the broker. An APP_RESPONSE_TIMEOUT-bound dispatcher would already reply.
    std::thread::sleep(Duration::from_millis(5100));
    socket.assert_held();
    server.expire_media_sessions(Instant::now());
    socket.finish();
    let response = socket.response();
    assert_eq!(response["id"], "uncertain");
    assert_eq!(response["error"]["code"], "media_timeout");
    assert!(response["error"].get("close_code").is_none());
    assert_eq!(
        server.media.state(&open.session_id).unwrap().state,
        api::schema::MediaSessionState::Offered
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_control_disconnect_and_listener_stop_never_abandon_session() {
    for owner_stops in [false, true] {
        let MediaServerWithClient {
            mut server,
            pane_id,
            control,
            render: _render,
            _input,
        } = media_server_with_client(83);
        let tx = sockets(&mut server);
        let open = offered(&mut server, &tx, &pane_id, &control, 83, "disconnect").await;
        let mut socket = Socket::new(
            &tx,
            &server.app.event_hub,
            request(
                "gone",
                "media.control",
                json!({"session_id": open.session_id, "input_muted": true}),
            ),
        );
        service(&mut server).await;
        let command = device_control(&control, Duration::from_secs(2)).unwrap();
        if owner_stops {
            socket.running.store(false, Ordering::Relaxed);
            socket.finish();
        } else {
            socket.disconnect();
        }
        // A failed Respond send is not an offer-delivery failure.
        applied(&mut server, 83, &command, true, false);
        server.expire_media_sessions(Instant::now());
        let state = server.media.state(&open.session_id).unwrap();
        assert_eq!(state.state, api::schema::MediaSessionState::Offered);
        assert!(state.code.is_none());
        assert!(media_close_sent(&control, Duration::from_millis(50)).is_none());
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn json_epoch_gate_precedes_answer_control_and_open_id_cancellation() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(84);
    let tx = sockets(&mut server);
    // Unknown expected identity must not install the legacy cancel-before-open tombstone.
    let mut unknown = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "unknown",
            "media.close",
            json!({"open_id": "guarded-open", "epoch": 1}),
        ),
    );
    service(&mut server).await;
    unknown.finish();
    assert_eq!(
        unknown.response()["error"]["code"],
        "media_session_not_found"
    );
    let open = offered(&mut server, &tx, &pane_id, &control, 84, "guarded-open").await;
    for (method, params) in [
        (
            "media.answer",
            json!({"session_id": open.session_id, "epoch": 99, "sdp": "v=0 stale"}),
        ),
        (
            "media.control",
            json!({"session_id": open.session_id, "epoch": 99, "input_muted": true}),
        ),
        (
            "media.close",
            json!({"session_id": open.session_id, "epoch": 99}),
        ),
        (
            "media.close",
            json!({"open_id": "guarded-open", "epoch": 99}),
        ),
    ] {
        let mut stale = Socket::new(&tx, &server.app.event_hub, request("stale", method, params));
        service(&mut server).await;
        stale.finish();
        let response = stale.response();
        assert_eq!(response["error"]["code"], "media_session_not_found");
        assert!(response["error"].get("close_code").is_none());
        assert_eq!(
            server.media.state(&open.session_id).unwrap().state,
            api::schema::MediaSessionState::Offered
        );
    }
    assert!(device_control(&control, Duration::from_millis(50)).is_none());
    assert!(media_answer_sent(&control, Duration::from_millis(50)).is_none());
    // Omitted epoch preserves legacy answer semantics; response is immediate,
    // without waiting for peer Connected or helper info.
    let mut answer = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "legacy",
            "media.answer",
            json!({"session_id": open.session_id, "sdp": "v=0 answer"}),
        ),
    );
    service(&mut server).await;
    answer.finish();
    assert_eq!(answer.response()["result"]["type"], "ok");
    assert!(media_answer_sent(&control, Duration::from_secs(2)).is_some());
    let mut close = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "matching",
            "media.close",
            json!({"open_id": "guarded-open", "epoch": 1}),
        ),
    );
    service(&mut server).await;
    close.finish();
    assert_eq!(close.response()["result"]["type"], "ok");
    assert_eq!(
        media_close_sent(&control, Duration::from_secs(2))
            .unwrap()
            .code
            .as_deref(),
        Some("closed")
    );
    for (method, params) in [
        (
            "media.answer",
            json!({"session_id": open.session_id, "epoch": 1, "sdp": "v=0 late"}),
        ),
        (
            "media.control",
            json!({"session_id": open.session_id, "epoch": 1, "input_muted": false}),
        ),
        (
            "media.close",
            json!({"session_id": open.session_id, "epoch": 1}),
        ),
    ] {
        let mut retired = Socket::new(
            &tx,
            &server.app.event_hub,
            request("retired", method, params),
        );
        service(&mut server).await;
        retired.finish();
        let response = retired.response();
        assert_eq!(response["error"]["code"], "media_session_not_found");
        assert_eq!(response["error"]["close_code"], "closed");
    }
    let mut legacy_close = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "idempotent",
            "media.close",
            json!({"session_id": open.session_id}),
        ),
    );
    service(&mut server).await;
    legacy_close.finish();
    assert_eq!(legacy_close.response()["result"]["type"], "ok");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_media_subscription_filters_canonical_pane_and_emits_untagged_broker_events() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(85);
    let (_, raw) = server.app.parse_pane_id(&pane_id).unwrap();
    let alias = format!("p_{}", raw.raw());
    type_into(&mut server, 85, &alias);
    let tx = sockets(&mut server);
    let mut subscription = subscribe(&mut server, &tx, &alias).await;
    server.drain_media_interest_transitions(); // interest exists even before open
    let open = offered(&mut server, &tx, &alias, &control, 85, "events").await;
    assert_eq!(
        open.pane_id, alias,
        "endpoint keeps the authenticated input alias"
    );
    let levels = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(levels.levels, Some(true));
    // General/tagged events and other canonical pane ids must never leak.
    server.app.event_hub.push(api::schema::EventEnvelope {
        event: api::schema::EventKind::WorkspaceFocused,
        data: api::schema::EventData::WorkspaceFocused {
            workspace_id: "other".into(),
        },
    });
    server.perform_media_actions(vec![crate::server::media::MediaAction::Publish {
        event: api::schema::SubscriptionEventEnvelope {
            event: api::schema::SubscriptionEventKind::MediaLevels,
            data: api::schema::SubscriptionEventData::MediaLevels(api::schema::MediaLevelsEvent {
                pane_id: format!("{pane_id}-not-the-pane"),
                session_id: "other".into(),
                input_peak: 0.1,
                output_peak: 0.2,
            }),
        },
    }]);
    client_control(
        &mut server,
        85,
        MediaControl::Info(MediaInfo {
            session_id: open.session_id.clone(),
            version: "test-helper".into(),
            commit: "abc".into(),
            pid: 7,
            input_device: "test input".into(),
            output_device: "test output".into(),
        }),
    );
    let mut answer = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "answer",
            "media.answer",
            json!({"session_id": open.session_id, "epoch": 1, "sdp": "v=0 answer"}),
        ),
    );
    service(&mut server).await;
    answer.finish();
    assert_eq!(answer.response()["result"]["type"], "ok");
    client_control(
        &mut server,
        85,
        MediaControl::State(MediaStateUpdate {
            session_id: open.session_id.clone(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        }),
    );
    let started = subscription.response();
    assert_eq!(started["event"], "media.started");
    assert_eq!(started["data"]["pane_id"], pane_id);
    assert_eq!(started["data"]["session_id"], open.session_id);
    assert_eq!(started["data"]["helper"]["version"], "test-helper");
    assert_eq!(started["data"]["input_device"], "test input");
    assert!(started["data"].get("type").is_none());
    client_control(
        &mut server,
        85,
        MediaControl::Levels(MediaLevels {
            session_id: open.session_id.clone(),
            input_peak: 0.25,
            output_peak: 0.5,
        }),
    );
    let level = subscription.response();
    assert_eq!(level["event"], "media.levels");
    assert_eq!(level["data"]["pane_id"], pane_id);
    assert!(level["data"].get("type").is_none());
    let mut pending = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "event-control",
            "media.control",
            json!({"session_id": open.session_id, "output_suppressed": true}),
        ),
    );
    service(&mut server).await;
    let command = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 85, &command, false, true);
    pending.finish();
    assert_eq!(pending.response()["result"]["request_id"], "event-control");
    let event = subscription.response();
    assert_eq!(event["event"], "media.controls_applied");
    assert_eq!(event["data"]["request_id"], "event-control");
    assert_eq!(event["data"]["pane_id"], pane_id);
    assert!(event["data"].get("type").is_none());
    client_control(
        &mut server,
        85,
        MediaControl::State(MediaStateUpdate {
            session_id: open.session_id.clone(),
            state: MediaPeerState::Failed,
            muted: false,
            detail: Some("test device failure".into()),
        }),
    );
    let error = subscription.response();
    assert_eq!(error["event"], "media.error");
    assert_eq!(error["data"]["pane_id"], pane_id);
    assert_eq!(error["data"]["code"], "device_error");
    assert!(error["data"].get("type").is_none());
    let stopped = subscription.response();
    assert_eq!(stopped["event"], "media.stopped");
    assert_eq!(stopped["data"]["session_id"], open.session_id);
    subscription.disconnect();
    server.drain_media_interest_transitions();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_media_subscribers_enable_first_disable_last_and_cleanup_failed_setup_or_write() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(86);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 86, "interest").await;
    let mut first = subscribe(&mut server, &tx, &pane_id).await;
    server.drain_media_interest_transitions();
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(true)
    );
    let (_, raw) = server.app.parse_pane_id(&pane_id).unwrap();
    let mut second = subscribe(&mut server, &tx, &format!("p_{}", raw.raw())).await;
    server.drain_media_interest_transitions();
    assert!(device_control(&control, Duration::from_millis(50)).is_none());
    first.disconnect();
    server.drain_media_interest_transitions();
    assert!(device_control(&control, Duration::from_millis(50)).is_none());
    second.running.store(false, Ordering::Relaxed);
    second.finish(); // owner shutdown
    server.drain_media_interest_transitions();
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(false)
    );

    let mut failed = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "failed",
            "events.subscribe",
            json!({"subscriptions": [{"type": "media", "pane_id": pane_id},
            {"type": "media", "pane_id": "w_missing:p9"}]}),
        ),
    );
    service(&mut server).await;
    service(&mut server).await;
    failed.finish();
    assert_eq!(failed.response()["error"]["code"], "pane_not_found");
    server.drain_media_interest_transitions();
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(true)
    );
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(false)
    );

    let mut failed_write = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "write",
            "events.subscribe",
            json!({"subscriptions": [{"type": "media", "pane_id": pane_id}]}),
        ),
    );
    failed_write
        .client
        .as_ref()
        .unwrap()
        .get_ref()
        .shutdown(Shutdown::Read)
        .unwrap();
    service(&mut server).await;
    failed_write.finish();
    server.drain_media_interest_transitions();
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(true)
    );
    assert_eq!(
        device_control(&control, Duration::from_secs(2))
            .unwrap()
            .levels,
        Some(false)
    );
    assert!(server.media.state(&open.session_id).unwrap().code.is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn idle_headless_loop_wakes_on_json_subscription_drop_without_api_or_periodic_timer() {
    idle_headless_loop_json_interest(false).await;
}

#[tokio::test]
async fn idle_headless_loop_preserves_json_interest_queued_before_run() {
    idle_headless_loop_json_interest(true).await;
}

async fn idle_headless_loop_json_interest(queue_before_run: bool) {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render,
        _input,
    } = media_server_with_client(89);
    let tx = sockets(&mut server);
    let _open = offered(&mut server, &tx, &pane_id, &control, 89, "idle-interest").await;
    if queue_before_run {
        let mut prestart = subscribe(&mut server, &tx, &pane_id).await;
        prestart.disconnect();
        // Both transitions precede run/notified: one coalesced permit must
        // deliver the whole queue, without a later API request to wake it.
        assert!(device_control(&control, Duration::from_millis(50)).is_none());
    }
    let (_, raw) = server.app.parse_pane_id(&pane_id).unwrap();
    let alias = format!("p_{}", raw.raw());
    let hub = server.app.event_hub.clone();
    let should_quit = server.should_quit.clone();
    let events = server.server_event_tx.clone();
    // Drain the fake client's render sink by channel readiness, not test polling.
    let render_drain =
        std::thread::spawn(move || while render.recv_timeout(SOCKET_BUDGET).is_ok() {});
    let observer = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if queue_before_run {
                for enabled in [true, false] {
                    assert_eq!(
                        device_control(&control, Duration::from_secs(2))
                            .expect("pre-run queued interest reaches the real client")
                            .levels,
                        Some(enabled)
                    );
                }
            }
            let mut subscription = Socket::new(
                &tx,
                &hub,
                request(
                    "idle-sub",
                    "events.subscribe",
                    json!({"subscriptions": [{"type": "media", "pane_id": pane_id}]}),
                ),
            );
            let response = subscription.response();
            assert_eq!(
                response["result"]["type"], "subscription_started",
                "subscription bootstrap returned: {response}"
            );
            assert_eq!(
                device_control(&control, Duration::from_secs(2))
                    .unwrap()
                    .levels,
                Some(true)
            );
            let mut overlap = Socket::new(
                &tx,
                &hub,
                request(
                    "alias-sub",
                    "events.subscribe",
                    json!({"subscriptions": [{"type": "media", "pane_id": alias}]}),
                ),
            );
            let response = overlap.response();
            assert_eq!(
                response["result"]["type"], "subscription_started",
                "alias bootstrap returned: {response}"
            );
            assert!(device_control(&control, Duration::from_millis(50)).is_none());
            subscription.running.store(false, Ordering::Relaxed);
            subscription.finish(); // owner stop is not the last subscriber
            assert!(device_control(&control, Duration::from_millis(50)).is_none());

            let mut failed = Socket::new(
                &tx,
                &hub,
                request(
                    "failed-overlap",
                    "events.subscribe",
                    json!({"subscriptions": [{"type": "media", "pane_id": pane_id},
                        {"type": "media", "pane_id": "w_missing:p9"}]}),
                ),
            );
            failed.finish();
            let response = failed.response();
            assert_eq!(
                response["error"]["code"], "pane_not_found",
                "partial setup must fail without dropping existing interest: {response}"
            );
            assert!(device_control(&control, Duration::from_millis(50)).is_none());
            std::thread::sleep(Duration::from_millis(150));
            overlap.disconnect();
            // No API request/client event follows the last drop. Only the shared
            // pending-transition Notify can wake the otherwise idle runtime.
            assert_eq!(
                device_control(&control, Duration::from_secs(2))
                    .unwrap()
                    .levels,
                Some(false)
            );

            let mut again = Socket::new(
                &tx,
                &hub,
                request(
                    "resubscribe",
                    "events.subscribe",
                    json!({"subscriptions": [{"type": "media", "pane_id": alias}]}),
                ),
            );
            let response = again.response();
            assert_eq!(
                response["result"]["type"], "subscription_started",
                "subscription after last drop returned: {response}"
            );
            assert_eq!(
                device_control(&control, Duration::from_secs(2))
                    .unwrap()
                    .levels,
                Some(true)
            );
            again.disconnect();
            assert_eq!(
                device_control(&control, Duration::from_secs(2))
                    .unwrap()
                    .levels,
                Some(false)
            );
        }));
        should_quit.store(true, Ordering::Release);
        let _ = events.try_send(ServerEvent::QuitSignal);
        result
    });
    tokio::time::timeout(SOCKET_BUDGET, server.run())
        .await
        .expect("idle runtime finishes")
        .unwrap();
    observer.join().unwrap().unwrap();
    render_drain.join().unwrap();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_answer_returns_immediately_then_negotiation_deadline_emits_stopped_and_state() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(88);
    let tx = sockets(&mut server);
    let (workspace_index, raw) = server.app.parse_pane_id(&pane_id).unwrap();
    let terminal_id = server.app.state.workspaces[workspace_index]
        .terminal_id(raw)
        .unwrap()
        .clone();
    // Reproduce the original invalid fixture explicitly: topology/runtime alone
    // cannot bootstrap a subscription, and rejection must not acquire interest.
    let terminal = server.app.state.terminals.remove(&terminal_id).unwrap();
    let mut invalid = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "missing-terminal",
            "events.subscribe",
            json!({"subscriptions": [{"type": "media", "pane_id": pane_id}]}),
        ),
    );
    service(&mut server).await;
    invalid.finish();
    let response = invalid.response();
    assert_eq!(response["id"], "missing-terminal");
    assert_eq!(
        response["error"]["code"], "pane_not_found",
        "incomplete pane state must still be rejected: {response}"
    );
    assert!(server
        .app
        .event_hub
        .take_media_interest_transitions(|_| Some(raw))
        .is_empty());
    server.app.state.terminals.insert(terminal_id, terminal);
    server.app.state.assert_invariants_for_test();
    // Subscribe through the raw alias, then require canonical ids on real broker
    // events below. Successful bootstrap must not reuse the caller's alias.
    let alias = format!("p_{}", raw.raw());
    let mut subscription = subscribe(&mut server, &tx, &alias).await;
    server.drain_media_interest_transitions();
    let open = offered(&mut server, &tx, &pane_id, &control, 88, "deadline").await;
    let _levels = device_control(&control, Duration::from_secs(2)).unwrap();
    let mut answer = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "answer-now",
            "media.answer",
            json!({"session_id": open.session_id, "epoch": 1, "sdp": "v=0 answer"}),
        ),
    );
    service(&mut server).await;
    answer.finish();
    assert_eq!(answer.response()["result"]["type"], "ok");
    assert_eq!(
        server.media.state(&open.session_id).unwrap().state,
        api::schema::MediaSessionState::Connecting
    );
    server.expire_media_sessions(Instant::now() + Duration::from_secs(16));
    let error = subscription.response();
    assert_eq!(error["event"], "media.error");
    assert_eq!(error["data"]["code"], "negotiation_timeout");
    let stopped = subscription.response();
    assert_eq!(stopped["event"], "media.stopped");
    assert_eq!(stopped["data"]["pane_id"], pane_id);
    assert_eq!(stopped["data"]["epoch"], 1);
    assert_eq!(stopped["data"]["code"], "negotiation_timeout");
    let mut state = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "expired",
            "media.state",
            json!({"session_id": open.session_id}),
        ),
    );
    service(&mut server).await;
    state.finish();
    assert_eq!(state.response()["result"]["code"], "negotiation_timeout");
    let mut late_answer = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "late",
            "media.answer",
            json!({"session_id": open.session_id, "sdp": "v=0 late"}),
        ),
    );
    service(&mut server).await;
    late_answer.finish();
    let error = late_answer.response();
    assert_eq!(error["error"]["code"], "media_session_not_found");
    assert_eq!(error["error"]["close_code"], "negotiation_timeout");
    subscription.disconnect();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_late_answer_reports_deadline_cause_before_maintenance_retains_closed_state() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(90);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 90, "late-answer").await;
    // Seed only the clock boundary, then exercise the actual JSON answer router.
    let actions = server
        .media
        .answer(
            &open.session_id,
            "v=0 initial".into(),
            Instant::now() - Duration::from_secs(16),
        )
        .unwrap();
    server.perform_media_actions(actions);
    let mut late = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "late-live",
            "media.answer",
            json!({"session_id": open.session_id, "sdp": "v=0 late"}),
        ),
    );
    service(&mut server).await;
    late.finish();
    let error = late.response();
    assert_eq!(error["error"]["code"], "media_refused");
    assert_eq!(error["error"]["close_code"], "negotiation_timeout");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("negotiation_timeout:"));
    assert!(
        server.media.state(&open.session_id).unwrap().code.is_none(),
        "no maintenance close ran yet"
    );
    server.expire_media_sessions(Instant::now());
    assert_eq!(
        server
            .media
            .state(&open.session_id)
            .unwrap()
            .code
            .as_deref(),
        Some("negotiation_timeout")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_session_end_settles_held_control_with_causal_close_code() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(87);
    let tx = sockets(&mut server);
    let mut subscription = subscribe(&mut server, &tx, &pane_id).await;
    server.drain_media_interest_transitions();
    let open = offered(&mut server, &tx, &pane_id, &control, 87, "negotiation").await;
    let _levels = device_control(&control, Duration::from_secs(2)).unwrap();
    let mut answer = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "answer",
            "media.answer",
            json!({"session_id": open.session_id, "epoch": 1, "sdp": "v=0 answer"}),
        ),
    );
    service(&mut server).await;
    answer.finish();
    assert_eq!(answer.response()["result"]["type"], "ok");
    let mut pending = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "pending",
            "media.control",
            json!({"session_id": open.session_id, "input_muted": true}),
        ),
    );
    service(&mut server).await;
    let _command = device_control(&control, Duration::from_secs(2)).unwrap();
    // End via an authenticated client callback before the 5s control deadline.
    client_control(
        &mut server,
        87,
        MediaControl::Close(crate::protocol::media::MediaClose::new(
            &open.session_id,
            "negotiation_timeout",
            "peer never connected",
        )),
    );
    pending.finish();
    let failure = pending.response();
    assert_eq!(failure["error"]["code"], "media_control_failed");
    assert_eq!(failure["error"]["close_code"], "negotiation_timeout");
    let error = subscription.response();
    assert_eq!(error["event"], "media.error");
    assert_eq!(error["data"]["code"], "negotiation_timeout");
    let stopped = subscription.response();
    assert_eq!(stopped["event"], "media.stopped");
    assert_eq!(stopped["data"]["code"], "negotiation_timeout");
    assert_eq!(stopped["data"]["epoch"], 1);
    assert!(stopped["data"].get("type").is_none());
    let mut state = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "state",
            "media.state",
            json!({"session_id": open.session_id}),
        ),
    );
    service(&mut server).await;
    state.finish();
    assert_eq!(state.response()["result"]["code"], "negotiation_timeout");
    subscription.disconnect();
    shutdown_test_runtimes(&mut server);
}
