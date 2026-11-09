//! JSON socket -> production API router -> headless broker integration.
//! Keep these for the transport/identity and microphone-release failure boundaries.
use super::*;
use crate::protocol::media::{
    MediaApplied, MediaControlFailed, MediaDeviceControl, MediaInfo, MediaLevels,
    MEDIA_DEVICE_CONTROL_CAPACITY,
};
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
    assert_eq!(
        response["result"]["epoch"],
        server.media.state(&open.session_id).unwrap().epoch
    );
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
async fn media_control_method_routes_through_broker_intent_acceptance() {
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
    let mut unknown = a.clone();
    unknown.control_id = u64::MAX;
    applied(&mut server, 81, &unknown, true, false);
    applied(&mut server, 81, &a, false, false); // not the recorded expected pair
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

// The name above retains B's historical internal acceptance terminology only.
// Its observable public result is exclusively media_controls_applied after ACK.

async fn control_request(
    server: &mut HeadlessServer,
    tx: &api::ApiRequestSender,
    id: &str,
    session_id: &str,
    input_muted: Option<bool>,
    output_suppressed: Option<bool>,
) -> Socket {
    let socket = Socket::new(
        tx,
        &server.app.event_hub,
        request(
            id,
            "media.control",
            json!({"session_id": session_id, "input_muted": input_muted,
                "output_suppressed": output_suppressed}),
        ),
    );
    service(server).await;
    socket
}

fn age_media_input(server: &mut HeadlessServer, client_id: u64, pane_ref: &str) {
    let (_, pane) = server
        .app
        .parse_pane_id(pane_ref)
        .expect("fixture pane alias resolves");
    server.media.note_pane_input(
        client_id,
        pane,
        pane_ref,
        Instant::now() - crate::protocol::media::MEDIA_INPUT_WINDOW - Duration::from_secs(1),
    );
}

async fn answer_and_connect(
    server: &mut HeadlessServer,
    tx: &api::ApiRequestSender,
    control: &mpsc::Receiver<Vec<u8>>,
    client: u64,
    session_id: &str,
) {
    let mut answer = Socket::new(
        tx,
        &server.app.event_hub,
        request(
            "connect",
            "media.answer",
            json!({"session_id": session_id, "sdp": "v=0 answer"}),
        ),
    );
    service(server).await;
    answer.finish();
    assert_eq!(answer.response()["result"]["type"], "ok");
    assert_eq!(
        media_answer_sent(control, Duration::from_secs(2))
            .unwrap()
            .session_id,
        session_id
    );
    client_control(
        server,
        client,
        MediaControl::State(MediaStateUpdate {
            session_id: session_id.into(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        }),
    );
}

async fn close_session(server: &mut HeadlessServer, tx: &api::ApiRequestSender, session: &str) {
    let mut close = Socket::new(
        tx,
        &server.app.event_hub,
        request("cleanup", "media.close", json!({"session_id": session})),
    );
    service(server).await;
    close.finish();
    assert_eq!(close.response()["result"]["type"], "ok");
}

type SecondaryMediaClientFixture = (
    String,
    mpsc::Receiver<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
    tokio::sync::mpsc::Receiver<Bytes>,
);

fn another_media_client(server: &mut HeadlessServer, client: u64) -> SecondaryMediaClientFixture {
    let mut workspace = crate::workspace::Workspace::test_new("independent-media");
    let pane = workspace.tabs[0].root_pane;
    let (runtime, input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 4);
    workspace.insert_test_runtime(pane, runtime);
    let index = server.app.state.workspaces.len();
    server.app.state.workspaces.push(workspace);
    server.app.state.active = Some(index);
    server.app.state.selected = index;
    server.app.state.ensure_test_terminals();
    server.app.state.assert_invariants_for_test();
    let pane_id = server.app.public_pane_id(index, pane).unwrap();
    let (control, render) = connect_media_shell(server, client, true);
    type_into(server, client, &pane_id);
    (pane_id, control, render, input)
}

#[tokio::test]
async fn media_control_missing_writer_is_not_delivery() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(91);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 91, "no-writer").await;
    answer_and_connect(&mut server, &tx, &control, 91, &open.session_id).await;
    let mut subscription = subscribe(&mut server, &tx, &pane_id).await;
    server.drain_media_interest_transitions();
    let internal = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 91, &internal, false, false);
    age_media_input(&mut server, 91, &pane_id);
    let mut renewal = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "pending-renewal",
            "pane.media_open",
            json!({"pane_id": pane_id, "open_id": "no-writer-renewal"}),
        ),
    );
    service(&mut server).await;
    let successor = media_open_sent(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(
        server.media.state(&open.session_id).unwrap().state,
        api::schema::MediaSessionState::Connected
    );
    assert_eq!(
        server.media.state(&successor.session_id).unwrap().state,
        api::schema::MediaSessionState::Opening
    );
    renewal.assert_held();
    let internal = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 91, &internal, false, false);
    // A detached entry is still registered. Generic send_to_client returns true
    // for precisely this case, without writing a byte.
    let _writer = server.clients.get_mut(&91).unwrap().writer.take().unwrap();
    let mut pending = control_request(
        &mut server,
        &tx,
        "undelivered",
        &open.session_id,
        Some(true),
        Some(true),
    )
    .await;
    pending.finish();
    let response = pending.response();
    assert_eq!(response["error"]["code"], "media_control_failed");
    assert_eq!(response["error"]["close_code"], "disconnected");
    assert!(response.get("result").is_none());
    assert!(server.clients.contains_key(&91), "no global endpoint reset");
    assert_eq!(
        server
            .media
            .state(&open.session_id)
            .unwrap()
            .code
            .as_deref(),
        Some("disconnected")
    );
    assert!(device_control(&control, Duration::from_millis(50)).is_none());
    renewal.finish();
    assert_eq!(renewal.response()["error"]["close_code"], "closed");
    let events = [subscription.response(), subscription.response()];
    for (session, code) in [
        (&open.session_id, "disconnected"),
        (&successor.session_id, "closed"),
    ] {
        let stopped = events
            .iter()
            .find(|event| event["data"]["session_id"] == *session)
            .unwrap();
        assert_eq!(stopped["event"], "media.stopped");
        assert_eq!(stopped["data"]["pane_id"], pane_id);
        assert_eq!(stopped["data"]["code"], code);
    }
    // The queued successor Control was canceled by the source finish, not
    // delivered after its terminal actions or treated as another global loss.
    assert!(device_control(&control, Duration::from_millis(50)).is_none());
    subscription.disconnect();
    server.drain_media_interest_transitions();
    // The real teardown may race the already-settled target without recursing
    // into the absent writer or creating another public outcome.
    server.remove_client_and_resize_if_needed(91);
    assert!(server
        .media
        .control_delivery_failed(91, &open.session_id, Instant::now())
        .is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn media_control_failed_targeted_send_preserves_intent_and_terminates_session() {
    // Keep: data safety and the otherwise rare synchronous writer-failure path.
    for missing_client in [false, true] {
        let MediaServerWithClient {
            mut server,
            pane_id,
            control,
            render: _render,
            _input,
        } = media_server_with_client(92);
        let tx = sockets(&mut server);
        let open = offered(&mut server, &tx, &pane_id, &control, 92, "bad-target").await;
        let (other_pane, other_control, _other_render, _other_input) =
            another_media_client(&mut server, 93);
        let other = offered(
            &mut server,
            &tx,
            &other_pane,
            &other_control,
            93,
            "survivor",
        )
        .await;
        if missing_client {
            // Simulate transport-map removal preceding its queued disconnect event.
            server.clients.remove(&92);
        } else {
            // A present writer whose reliable queue rejects the actual encoded send.
            server.clients[&92].writer.as_ref().unwrap().test_close();
        }
        let mut failed = control_request(
            &mut server,
            &tx,
            "delivery-failed",
            &open.session_id,
            Some(true),
            Some(true),
        )
        .await;
        failed.finish();
        let response = failed.response();
        assert_eq!(response["error"]["code"], "media_control_failed");
        assert_eq!(response["error"]["close_code"], "disconnected");
        let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
        assert!(server.media.intent(pane).input_muted);
        assert!(server.media.intent(pane).output_suppressed);
        assert!(server
            .media
            .control_delivery_failed(92, &open.session_id, Instant::now())
            .is_empty());
        let mut survivor = control_request(
            &mut server,
            &tx,
            "still-usable",
            &other.session_id,
            Some(false),
            Some(true),
        )
        .await;
        let command = device_control(&other_control, Duration::from_secs(2)).unwrap();
        survivor.assert_held();
        applied(&mut server, 93, &command, false, true);
        survivor.finish();
        assert_eq!(survivor.response()["result"]["request_id"], "still-usable");
        // Genuine transport cleanup is separate; it is harmless after the local
        // failure. A newly authenticated input authorizes a protected fresh open.
        server.remove_client_and_resize_if_needed(92);
        server.app.state.active = Some(0);
        server.app.state.selected = 0;
        let (fresh_control, _fresh_render) = connect_media_shell(&mut server, 92, true);
        type_into(&mut server, 92, &pane_id);
        let mut fresh = Socket::new(
            &tx,
            &server.app.event_hub,
            request(
                "fresh",
                "pane.media_open",
                json!({"pane_id": pane_id, "open_id": "fresh"}),
            ),
        );
        service(&mut server).await;
        let initial = media_initial_open_sent(&fresh_control, Duration::from_secs(2)).unwrap();
        assert!(initial.input_muted && initial.output_suppressed);
        assert!(
            media_open_sent(&fresh_control, Duration::from_millis(50)).is_none(),
            "a protected open has no v1 downgrade or duplicate open"
        );
        client_control(
            &mut server,
            92,
            MediaControl::Offer(MediaSdp {
                session_id: initial.session_id.clone(),
                sdp: "v=0 protected offer".into(),
            }),
        );
        fresh.finish();
        assert_eq!(fresh.response()["result"]["session_id"], initial.session_id);
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn media_control_terminal_failure_never_becomes_applied() {
    // Keep: authenticated correlation boundary; observation is not desired policy.
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(94);
    let tx = sockets(&mut server);
    let open = offered(&mut server, &tx, &pane_id, &control, 94, "terminal").await;
    let mut older = control_request(
        &mut server,
        &tx,
        "older",
        &open.session_id,
        Some(true),
        Some(false),
    )
    .await;
    let old = device_control(&control, Duration::from_secs(2)).unwrap();
    let mut newer = control_request(
        &mut server,
        &tx,
        "newer",
        &open.session_id,
        Some(false),
        Some(true),
    )
    .await;
    let new = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 94, &new, false, true);
    newer.finish();
    assert_eq!(newer.response()["result"]["request_id"], "newer");
    older.assert_held();
    applied(&mut server, 94, &old, true, false);
    older.finish();
    assert_eq!(older.response()["result"]["request_id"], "older");
    assert_eq!(
        server.media.applied(&open.session_id).unwrap().control_id,
        new.control_id
    );
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    assert!(!server.media.intent(pane).input_muted);
    assert!(server.media.intent(pane).output_suppressed);
    let mut failed = control_request(
        &mut server,
        &tx,
        "failed",
        &open.session_id,
        Some(true),
        Some(true),
    )
    .await;
    let bad = device_control(&control, Duration::from_secs(2)).unwrap();
    let failure = |id| {
        MediaControl::ControlFailed(MediaControlFailed {
            session_id: open.session_id.clone(),
            control_id: id,
            code: "control_timeout".into(),
            message: Some("device did not acknowledge".into()),
        })
    };
    for (client, id) in [
        (999, Some(bad.control_id)),
        (94, None),
        (94, Some(u64::MAX)),
    ] {
        client_control(&mut server, client, failure(id));
        failed.assert_held();
    }
    client_control(&mut server, 94, failure(Some(bad.control_id)));
    failed.finish();
    let response = failed.response();
    assert_eq!(response["error"]["code"], "media_control_failed");
    assert!(response["error"].get("close_code").is_none());
    let mut next = control_request(
        &mut server,
        &tx,
        "next",
        &open.session_id,
        Some(false),
        Some(false),
    )
    .await;
    let last = device_control(&control, Duration::from_secs(2)).unwrap();
    client_control(&mut server, 94, failure(Some(bad.control_id)));
    applied(&mut server, 94, &bad, true, true);
    next.assert_held();
    assert_eq!(
        server.media.applied(&open.session_id).unwrap().control_id,
        new.control_id
    );
    let mut subscription = subscribe(&mut server, &tx, &pane_id).await;
    server.drain_media_interest_transitions();
    let internal = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(internal.levels, Some(true));
    applied(&mut server, 94, &internal, false, false);
    next.assert_held(); // an internal levels CID cannot consume the API waiter
    applied(&mut server, 94, &last, false, false);
    next.finish();
    assert_eq!(next.response()["result"]["request_id"], "next");
    assert_eq!(
        server.media.applied(&open.session_id).unwrap().control_id,
        internal.control_id
    );
    let event = subscription.response();
    assert_eq!(event["event"], "media.controls_applied");
    assert_eq!(event["data"]["request_id"], "next");
    subscription.disconnect();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn media_control_fanout_preflight_is_atomic() {
    // Keep: atomic privacy policy across authorized renewal targets. Only the
    // capacity seed uses the broker seam; rejection and recovery use real JSON.
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(95);
    let tx = sockets(&mut server);
    let source = offered(&mut server, &tx, &pane_id, &control, 95, "atomic-source").await;
    answer_and_connect(&mut server, &tx, &control, 95, &source.session_id).await;
    age_media_input(&mut server, 95, &pane_id);
    let mut renewal = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "renewal",
            "pane.media_open",
            json!({"pane_id": pane_id, "open_id": "atomic-renewal"}),
        ),
    );
    service(&mut server).await;
    let successor = media_open_sent(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(
        server.media.state(&source.session_id).unwrap().state,
        api::schema::MediaSessionState::Connected
    );
    assert_eq!(
        server.media.state(&successor.session_id).unwrap().state,
        api::schema::MediaSessionState::Opening
    );
    renewal.assert_held();
    let mut unready = control_request(
        &mut server,
        &tx,
        "before-offer",
        &successor.session_id,
        Some(true),
        Some(true),
    )
    .await;
    unready.finish();
    let error = unready.response();
    assert_eq!(error["error"]["code"], "media_session_not_found");
    assert!(error["error"].get("close_code").is_none());
    let mut seeded = Vec::new();
    for _ in 0..MEDIA_DEVICE_CONTROL_CAPACITY {
        let dispatch = server
            .media
            .dispatch_control(
                &successor.session_id,
                None,
                None,
                Some(false),
                Instant::now(),
            )
            .unwrap();
        assert_eq!(dispatch.session_ids, vec![successor.session_id.clone()]);
        server.perform_media_actions(dispatch.actions);
        let command = device_control(&control, Duration::from_secs(2)).unwrap();
        assert_eq!(command.session_id, successor.session_id);
        seeded.push(command);
    }
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    let intent = server.media.intent(pane);
    let source_before = server.media.state(&source.session_id).unwrap();
    let successor_before = server.media.state(&successor.session_id).unwrap();
    let mut rejected = control_request(
        &mut server,
        &tx,
        "atomic-reject",
        &source.session_id,
        Some(true),
        Some(true),
    )
    .await;
    rejected.finish();
    let failure = rejected.response();
    assert_eq!(failure["error"]["code"], "media_control_failed");
    assert!(failure["error"].get("close_code").is_none());
    assert!(!failure["error"]["message"].as_str().unwrap().is_empty());
    assert_eq!(server.media.intent(pane), intent);
    assert_eq!(
        server.media.state(&source.session_id).unwrap(),
        source_before
    );
    assert_eq!(
        server.media.state(&successor.session_id).unwrap(),
        successor_before
    );
    assert!(
        device_control(&control, Duration::from_millis(50)).is_none(),
        "no partial source send when only the successor is full"
    );
    // Release exactly one seeded record. The rejected transaction consumed no
    // CID and changed neither target's expected pair nor retained pane policy.
    let last = seeded.last().unwrap();
    applied(&mut server, 95, last, false, false);
    let mut barrier = control_request(
        &mut server,
        &tx,
        "after-reject",
        &source.session_id,
        None,
        None,
    )
    .await;
    let command = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(command.control_id, last.control_id + 1);
    assert_eq!(command.session_id, source.session_id);
    applied(&mut server, 95, &command, false, false);
    barrier.finish();
    assert_eq!(barrier.response()["result"]["request_id"], "after-reject");
    // Supplement the actual delivery-failure socket tests with the opposite
    // lineage direction: losing only a successor must leave its source usable.
    let terminal = server
        .media
        .control_delivery_failed(95, &successor.session_id, Instant::now());
    server.perform_media_actions(terminal);
    renewal.finish();
    assert_eq!(renewal.response()["error"]["close_code"], "disconnected");
    assert_eq!(
        server.media.state(&source.session_id).unwrap().state,
        api::schema::MediaSessionState::Connected
    );
    let mut usable = control_request(
        &mut server,
        &tx,
        "source-survives",
        &source.session_id,
        Some(false),
        Some(false),
    )
    .await;
    let command = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 95, &command, false, false);
    usable.finish();
    assert_eq!(usable.response()["result"]["request_id"], "source-survives");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_fresh_input_second_open_replaces_connected_source_immediately() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(98);
    let tx = sockets(&mut server);
    let source = offered(&mut server, &tx, &pane_id, &control, 98, "fresh-source").await;
    answer_and_connect(&mut server, &tx, &control, 98, &source.session_id).await;
    assert_eq!(
        server.media.state(&source.session_id).unwrap().state,
        api::schema::MediaSessionState::Connected
    );

    // New real input selects immediate replacement, unlike an idle handover.
    type_into(&mut server, 98, &pane_id);
    let mut replacement = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "fresh-replacement",
            "pane.media_open",
            json!({"pane_id": pane_id, "open_id": "fresh-replacement"}),
        ),
    );
    service(&mut server).await;
    let successor = media_open_sent(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(
        server.media.state(&source.session_id).unwrap().state,
        api::schema::MediaSessionState::Closed
    );
    assert_eq!(
        server
            .media
            .state(&source.session_id)
            .unwrap()
            .code
            .as_deref(),
        Some("replaced")
    );
    assert_eq!(
        server.media.state(&successor.session_id).unwrap().state,
        api::schema::MediaSessionState::Opening
    );
    replacement.assert_held();

    let mut old_control = control_request(
        &mut server,
        &tx,
        "old-source-control",
        &source.session_id,
        Some(true),
        None,
    )
    .await;
    old_control.finish();
    let error = old_control.response();
    assert_eq!(error["error"]["code"], "media_session_not_found");
    assert_eq!(error["error"]["close_code"], "replaced");
    assert!(
        device_control(&control, Duration::from_millis(50)).is_none(),
        "a replaced source does not authorize control fanout to the successor"
    );

    close_session(&mut server, &tx, &successor.session_id).await;
    replacement.finish();
    assert_eq!(replacement.response()["error"]["close_code"], "closed");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_control_fanout_settles_only_named_session_and_attributes_successor_events() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(96);
    let (_, pane) = server.app.parse_pane_id(&pane_id).unwrap();
    let alias = format!("p_{}", pane.raw());
    type_into(&mut server, 96, &alias);
    let tx = sockets(&mut server);
    let source = offered(&mut server, &tx, &alias, &control, 96, "identity-source").await;
    answer_and_connect(&mut server, &tx, &control, 96, &source.session_id).await;
    let mut subscription = subscribe(&mut server, &tx, &alias).await;
    server.drain_media_interest_transitions();
    let internal = device_control(&control, Duration::from_secs(2)).unwrap();
    applied(&mut server, 96, &internal, false, false);
    age_media_input(&mut server, 96, &alias);
    let mut renewal = Socket::new(
        &tx,
        &server.app.event_hub,
        request(
            "renewal",
            "pane.media_open",
            json!({"pane_id": alias, "open_id": "identity-renewal"}),
        ),
    );
    service(&mut server).await;
    let successor = media_open_sent(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(
        server.media.state(&source.session_id).unwrap().state,
        api::schema::MediaSessionState::Connected
    );
    assert_eq!(
        server.media.state(&successor.session_id).unwrap().state,
        api::schema::MediaSessionState::Opening
    );
    renewal.assert_held();
    let internal = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(internal.session_id, successor.session_id);
    assert_eq!(internal.levels, Some(true));
    applied(&mut server, 96, &internal, false, false);
    for replacing in [false, true] {
        let outer = if replacing {
            "replaced-before-ack"
        } else {
            "named-only"
        };
        let mut held = control_request(
            &mut server,
            &tx,
            outer,
            &source.session_id,
            Some(true),
            Some(replacing),
        )
        .await;
        let first = device_control(&control, Duration::from_secs(2)).unwrap();
        let second = device_control(&control, Duration::from_secs(2)).unwrap();
        assert_eq!(first.control_id, second.control_id, "one combined dispatch");
        let (named, next) = if first.session_id == source.session_id {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(named.session_id, source.session_id);
        assert_eq!(next.session_id, successor.session_id);
        held.assert_held();
        if replacing {
            client_control(
                &mut server,
                96,
                MediaControl::Close(crate::protocol::media::MediaClose::new(
                    &source.session_id,
                    "replaced",
                    "successor starting",
                )),
            );
            held.finish();
            let failure = held.response();
            assert_eq!(failure["error"]["code"], "media_control_failed");
            assert_eq!(failure["error"]["close_code"], "replaced");
            let stopped = subscription.response();
            assert_eq!(stopped["event"], "media.stopped");
            assert_eq!(stopped["data"]["session_id"], source.session_id);
            assert_eq!(stopped["data"]["code"], "replaced");
            applied(&mut server, 96, &named, true, true); // cannot revive old waiter
        }
        applied(&mut server, 96, &next, true, replacing);
        let event = subscription.response();
        assert_eq!(event["event"], "media.controls_applied");
        assert_eq!(event["data"]["session_id"], successor.session_id);
        assert_eq!(event["data"]["request_id"], outer);
        assert_eq!(event["data"]["pane_id"], pane_id);
        assert_eq!(event["data"]["epoch"], 2);
        assert!(event["data"].get("type").is_none());
        if !replacing {
            held.assert_held(); // not first, any or all fanout targets
            applied(&mut server, 96, &named, true, false);
            held.finish();
            let response = held.response();
            assert_eq!(response["result"]["session_id"], source.session_id);
            assert_eq!(response["result"]["request_id"], outer);
            let event = subscription.response();
            assert_eq!(event["data"]["session_id"], source.session_id);
            assert_eq!(event["data"]["pane_id"], pane_id);
        }
    }
    close_session(&mut server, &tx, &successor.session_id).await;
    renewal.finish();
    assert_eq!(renewal.response()["error"]["close_code"], "closed");
    subscription.disconnect();
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn json_last_subscriber_capacity_rejection_drops_levels_and_reconciles_on_state() {
    let MediaServerWithClient {
        mut server,
        pane_id,
        control,
        render: _render,
        _input,
    } = media_server_with_client(97);
    let tx = sockets(&mut server);
    let mut subscription = subscribe(&mut server, &tx, &pane_id).await;
    server.drain_media_interest_transitions();
    let open = offered(&mut server, &tx, &pane_id, &control, 97, "full-levels").await;
    let enabled = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(enabled.levels, Some(true));
    answer_and_connect(&mut server, &tx, &control, 97, &open.session_id).await;
    let mut pending = Vec::new();
    for index in 1..MEDIA_DEVICE_CONTROL_CAPACITY {
        let held = control_request(
            &mut server,
            &tx,
            &format!("capacity-{index}"),
            &open.session_id,
            None,
            None,
        )
        .await;
        let command = device_control(&control, Duration::from_secs(2)).unwrap();
        held.assert_held();
        pending.push((held, command));
    }
    subscription.disconnect();
    server.drain_media_interest_transitions();
    assert!(
        device_control(&control, Duration::from_millis(50)).is_none(),
        "full admission rejects off without pretending delivery"
    );
    client_control(
        &mut server,
        97,
        MediaControl::Levels(MediaLevels {
            session_id: open.session_id.clone(),
            input_peak: 0.9,
            output_peak: 0.8,
        }),
    );
    assert!(
        server.media.levels(&open.session_id).is_none(),
        "no-subscriber server gate is immediate despite rejected device off"
    );
    applied(&mut server, 97, &enabled, false, false); // free an internal slot
    client_control(
        &mut server,
        97,
        MediaControl::State(MediaStateUpdate {
            session_id: open.session_id.clone(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        }),
    );
    let disabled = device_control(&control, Duration::from_secs(2)).unwrap();
    assert_eq!(disabled.session_id, open.session_id);
    assert_eq!(disabled.levels, Some(false));
    assert!(disabled.control_id > pending.last().unwrap().1.control_id);
    applied(&mut server, 97, &disabled, false, false);
    for (held, _) in &pending {
        held.assert_held(); // neither levels CID settles one of the public requests
    }
    close_session(&mut server, &tx, &open.session_id).await;
    for (mut held, _) in pending {
        held.finish();
        assert_eq!(held.response()["error"]["close_code"], "closed");
    }
    let _fresh = offered(&mut server, &tx, &pane_id, &control, 97, "default-off").await;
    assert!(
        device_control(&control, Duration::from_millis(50)).is_none(),
        "a fresh session without interest defaults off"
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
    let command = device_control(&control, Duration::from_secs(2)).unwrap();
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
    applied(&mut server, 82, &command, true, false);
    let mut after_timeout = control_request(
        &mut server,
        &tx,
        "after-timeout",
        &open.session_id,
        Some(false),
        None,
    )
    .await;
    let next = device_control(&control, Duration::from_secs(2)).unwrap();
    after_timeout.assert_held();
    applied(&mut server, 82, &command, true, false); // duplicate late result cannot settle next
    after_timeout.assert_held();
    applied(&mut server, 82, &next, false, false);
    after_timeout.finish();
    assert_eq!(
        after_timeout.response()["result"]["request_id"],
        "after-timeout"
    );
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
