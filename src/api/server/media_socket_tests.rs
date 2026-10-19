//! Real socket/API-channel boundaries for the microphone-release failure path.

use super::channel_contract_tests::{media_request, MediaHarness};
use super::*;
use interprocess::os::unix::uds_local_socket;
use std::io::{BufRead, BufReader, Read};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

struct Connection {
    client: Option<UnixStream>,
    api_rx: tokio::sync::mpsc::UnboundedReceiver<ApiRequestMessage>,
    running: Arc<AtomicBool>,
    stop: ServerStop,
    done: Receiver<io::Result<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Connection {
    fn new() -> Self {
        Self::for_request(media_request())
    }

    fn for_request(request: Request) -> Self {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
        writeln!(client, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        let server: LocalStream = uds_local_socket::Stream::from(server).into();
        let context = ApiRequestContext::capture(local_stream_peer_identity(&server));
        let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let stop = ServerStop::default();
        let worker_stop = stop.clone();
        let (done_tx, done) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = handle_connection_with_stop(
                server,
                context,
                &api_tx,
                &EventHub::default(),
                &worker_running,
                default_capabilities(),
                Some(&worker_stop),
                None,
            );
            let _ = done_tx.send(result);
        });
        Self {
            client: Some(client),
            api_rx,
            running,
            stop,
            done,
            worker: Some(worker),
        }
    }

    fn request(&mut self) -> ApiRequestMessage {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime
            .block_on(async { tokio::time::timeout(TEST_TIMEOUT, self.api_rx.recv()).await })
            .expect("media request reaches the app within the test budget")
            .expect("media request, not handler exit")
    }

    fn assert_waiting(&self) {
        assert!(
            matches!(
                self.done.recv_timeout(CONNECTION_POLL_INTERVAL * 3),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "request must remain waiting for the client's media offer"
        );
    }

    fn finish(&mut self) {
        self.done
            .recv_timeout(TEST_TIMEOUT)
            .expect("connection handler must finish within its owner budget")
            .unwrap();
        self.worker.take().unwrap().join().unwrap();
        assert!(
            matches!(
                self.api_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "no connection work or API sender may outlive the handler"
        );
    }

    fn response(&mut self) -> serde_json::Value {
        let mut line = String::new();
        BufReader::new(self.client.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(&line).expect("a complete response on the real socket")
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.client.take();
        if let Some(worker) = self.worker.take() {
            // Also clean up on an assertion failure. The full close wakes the
            // old implementation even when its shutdown handling is regressed.
            let completed = self.done.recv_timeout(TEST_TIMEOUT).is_ok();
            if completed || worker.is_finished() {
                let _ = worker.join();
            } else if !std::thread::panicking() {
                panic!("connection handler outlived its test owner");
            }
        }
    }
}

#[test]
fn media_response_probe_preserves_half_close_and_unconsumed_input() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut server: LocalStream = uds_local_socket::Stream::from(server).into();
    client.write_all(b"next request\n").unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert!(!local_stream_response_peer_closed(&mut server).unwrap());
    let mut input = [0; 13];
    server.read_exact(&mut input).unwrap();
    assert_eq!(&input, b"next request\n");
    assert!(!local_stream_response_peer_closed(&mut server).unwrap());
    drop(client);
    assert!(local_stream_response_peer_closed(&mut server).unwrap());
}

#[test]
fn media_socket_disconnect_while_waiting_abandons_and_releases_microphone() {
    let mut connection = Connection::new();
    let mut media = MediaHarness::new();
    media.service(connection.request());
    let id = media.opened_session();
    connection.client.take();
    connection.finish();
    // The handler's bounded socket observation produces the marker; broker
    // maintenance then sends media.close.v1 to release the microphone.
    media.expire();
    media.assert_abandoned(&id);
}

#[test]
fn media_socket_disconnect_before_app_service_never_opens_microphone() {
    let mut connection = Connection::new();
    let request = connection.request();
    connection.client.take();
    connection.finish();
    let mut media = MediaHarness::new();
    media.service(request);
    assert!(media.controls.is_empty());
    assert!(!media.broker.has_sessions());
}

fn successful_offer(write_half_closed: bool, extra_input: bool) {
    let mut connection = Connection::new();
    let mut media = MediaHarness::new();
    media.service(connection.request());
    let id = media.opened_session();
    if extra_input {
        connection
            .client
            .as_mut()
            .unwrap()
            .write_all(b"next request\n")
            .unwrap();
    }
    if write_half_closed {
        connection
            .client
            .as_mut()
            .unwrap()
            .shutdown(Shutdown::Write)
            .unwrap();
    }
    connection.assert_waiting();
    media.offer(&id);
    connection.finish();
    let response = connection.response();
    assert_eq!(response["id"], "media-open");
    assert_eq!(response["result"]["type"], "media_offer");
    assert_eq!(response["result"]["session_id"], id);
    assert_eq!(response["result"]["epoch"], 1);
    connection.client.take();
    media.expire();
    assert!(media.broker.has_sessions());
    assert_eq!(
        media.broker.state(&id).unwrap().state,
        crate::api::schema::MediaSessionState::Offered
    );
    assert_eq!(
        media.controls.len(),
        1,
        "successful response teardown must not send media.close"
    );
}

#[test]
fn media_socket_successful_offer_survives_caller_disconnect_after_delivery() {
    successful_offer(false, false);
}

#[test]
fn media_socket_write_half_closed_caller_still_receives_offer() {
    successful_offer(true, false);
}

#[test]
fn media_socket_extra_input_is_not_disconnect_evidence() {
    successful_offer(false, true);
}

#[test]
fn media_socket_half_close_with_extra_input_still_receives_offer() {
    successful_offer(true, true);
}

#[test]
fn media_socket_failed_response_write_abandons_offered_session() {
    let mut connection = Connection::new();
    let mut media = MediaHarness::new();
    media.service(connection.request());
    let id = media.opened_session();
    // Stop receiving without closing the request side: the response write,
    // rather than read EOF, must reveal that the offer cannot be delivered.
    connection
        .client
        .as_mut()
        .unwrap()
        .shutdown(Shutdown::Read)
        .unwrap();
    media.offer(&id);
    connection.finish();
    media.expire();
    media.assert_abandoned(&id);
}

#[test]
fn media_socket_wait_exits_and_abandons_when_listener_or_server_owner_stops() {
    for stop_server in [false, true] {
        let mut connection = Connection::new();
        let mut media = MediaHarness::new();
        media.service(connection.request());
        let id = media.opened_session();
        if stop_server {
            connection.stop.request(ShutdownReason::HostShutdown);
        } else {
            connection.running.store(false, Ordering::Relaxed);
        }
        // Leave both caller directions open: only owner shutdown can end the
        // still-pending handler. No detached per-request monitor is involved.
        connection.finish();
        media.expire();
        media.assert_abandoned(&id);
    }
}

#[test]
fn media_control_socket_owner_stop_releases_waiter_without_abandonment_receipt() {
    let mut connection = Connection::for_request(Request {
        id: "held-control".into(),
        method: Method::MediaControl(crate::api::schema::MediaControlParams {
            session_id: "established".into(),
            input_muted: Some(true),
            output_suppressed: None,
            epoch: None,
        }),
    });
    let request = connection.request();
    assert!(
        request.response_write_complete.is_none(),
        "controls must not acquire open's abandonment receipt"
    );
    connection.assert_waiting();
    connection.stop.request(ShutdownReason::HostShutdown);
    connection.finish();
    assert!(request
        .respond_to
        .send("late broker outcome".into())
        .is_err());
}

#[test]
fn media_socket_app_response_loss_returns_unavailable_and_releases_receipt() {
    let mut connection = Connection::new();
    let request = connection.request();
    let receipt = request.response_write_complete.unwrap();
    drop(request.respond_to);
    connection.finish();
    assert_eq!(connection.response()["error"]["code"], "server_unavailable");
    assert_eq!(receipt.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}
