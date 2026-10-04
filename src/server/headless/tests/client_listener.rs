use super::*;
use std::future::Future as _;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct ReadinessWakes(AtomicUsize);

impl Wake for ReadinessWakes {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn client_listener_idle_wait_has_no_readiness_wakes() {
    let mut server = test_headless_server();
    let readiness = crate::platform::local_listener_readiness(&server.client_listener).unwrap();
    let wakes = Arc::new(ReadinessWakes::default());
    let waker = Waker::from(wakes.clone());
    let mut waiting = std::pin::pin!(accept_ready_client_connections(
        &server.client_listener,
        &readiness,
        &mut server.next_client_id,
        &server.should_quit,
        &server.server_event_tx,
        false,
    ));
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));

    // Cross the former 250 ms poll interval without polling the future again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    assert!(matches!(
        waiting.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
}

#[tokio::test]
async fn client_listener_attach_wakes_and_rearms_after_would_block() {
    let mut server = test_headless_server();
    let readiness = crate::platform::local_listener_readiness(&server.client_listener).unwrap();

    for client_id in 1..=2 {
        let mut client = crate::ipc::connect_local_stream(&server.client_socket_path).unwrap();
        protocol::write_message(
            &mut client,
            &protocol::ClientMessage::TerminalHello {
                version: protocol::PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            },
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            accept_ready_client_connections(
                &server.client_listener,
                &readiness,
                &mut server.next_client_id,
                &server.should_quit,
                &server.server_event_tx,
                false,
            ),
        )
        .await
        .expect("fresh attach wakes the listener")
        .unwrap();
        assert_eq!(server.next_client_id, client_id + 1);

        // Prove the real handshake path, not just acceptance into the backlog.
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), server.server_event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let ServerEvent::ClientConnected {
                client_id: actual, ..
            } = event
            {
                assert_eq!(actual, client_id);
                break;
            }
        }
        let welcome: ServerMessage = protocol::read_message(&mut client, MAX_FRAME_SIZE).unwrap();
        assert!(matches!(
            welcome,
            ServerMessage::Welcome { error: None, .. }
        ));
        drop(client);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                accept_ready_client_connections(
                    &server.client_listener,
                    &readiness,
                    &mut server.next_client_id,
                    &server.should_quit,
                    &server.server_event_tx,
                    false,
                ),
            )
            .await
            .is_err(),
            "drained listener must wait, not spin on cached readiness"
        );
    }
    server.should_quit.store(true, Ordering::Release);
}

#[tokio::test]
async fn client_listener_stale_readiness_clears_only_after_would_block() {
    let mut server = test_headless_server();
    server.handoff_in_progress = true;
    let readiness = crate::platform::local_listener_readiness(&server.client_listener).unwrap();
    let client = crate::ipc::connect_local_stream(&server.client_socket_path).unwrap();
    // Keep the cached readiness but drain the backlog using the sync test path.
    drop(
        tokio::time::timeout(Duration::from_secs(1), readiness.readable())
            .await
            .unwrap()
            .unwrap(),
    );
    server.accept_client_connections().unwrap();
    drop(client);

    tokio::time::timeout(
        Duration::from_secs(1),
        accept_ready_client_connections(
            &server.client_listener,
            &readiness,
            &mut server.next_client_id,
            &server.should_quit,
            &server.server_event_tx,
            true,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        server.next_client_id, 1,
        "handoff rejects without handshakes"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), readiness.readable())
            .await
            .is_err()
    );

    // Clearing stale readiness must not lose the next arrival.
    let _fresh_client = crate::ipc::connect_local_stream(&server.client_socket_path).unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        accept_ready_client_connections(
            &server.client_listener,
            &readiness,
            &mut server.next_client_id,
            &server.should_quit,
            &server.server_event_tx,
            true,
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn client_listener_running_server_accepts_fresh_attach() {
    run_client_listener_attach(0).await;
}

#[tokio::test]
async fn client_listener_rendering_server_accepts_fresh_attach() {
    RENDER_ACCEPT_TEST_COUNT.store(0, Ordering::Relaxed);
    let mut server = test_headless_server();
    // Keep the API receiver open so render notifications, not a closed channel,
    // drive the loop between render iterations.
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    let pane_id = install_shared_view_test_runtime(&mut server);
    let terminal_id = server.app.state.workspaces[0].tabs[0]
        .terminal_id(pane_id)
        .unwrap()
        .to_string();
    let path = server.client_socket_path.clone();
    let first = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            let mut client = crate::ipc::connect_local_stream(&path).unwrap();
            protocol::write_message(
                &mut client,
                &protocol::ClientMessage::TerminalHello {
                    version: protocol::PROTOCOL_VERSION,
                    cols: 80,
                    rows: 24,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    pixel_mouse: false,
                },
            )
            .unwrap();
            let welcome: ServerMessage =
                protocol::read_message(&mut client, MAX_FRAME_SIZE).unwrap();
            assert!(matches!(
                welcome,
                ServerMessage::Welcome { error: None, .. }
            ));
            client
        }
    });
    let render_dirty = server.app.render_dirty.clone();
    let render_notify = server.app.render_notify.clone();
    let should_quit = server.should_quit.clone();
    let event_tx = server.server_event_tx.clone();
    let second_should_quit = should_quit.clone();
    let second_render_notify = render_notify.clone();
    let second = async move {
        let mut first_client = first.await.unwrap();
        let terminal_id_for_attach = terminal_id.clone();
        first_client = tokio::task::spawn_blocking(move || {
            protocol::write_message(
                &mut first_client,
                &protocol::ClientMessage::AttachTerminal {
                    terminal_id: terminal_id_for_attach,
                    takeover: false,
                },
            )
            .unwrap();
            first_client
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let accepts_before_second = RENDER_ACCEPT_TEST_COUNT.load(Ordering::Relaxed);
        // Hold the idle select's accept path in its existing error backoff so
        // only the render-iteration drain can accept the fresh client.
        set_idle_client_accept_test_blocked(true);
        let second_path = path.clone();
        let welcome = tokio::task::spawn_blocking(move || {
            let mut client = crate::ipc::connect_local_stream(&second_path).unwrap();
            protocol::write_message(
                &mut client,
                &protocol::ClientMessage::TerminalHello {
                    version: protocol::PROTOCOL_VERSION,
                    cols: 80,
                    rows: 24,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    pixel_mouse: false,
                },
            )
            .unwrap();
            let welcome: ServerMessage =
                protocol::read_message(&mut client, MAX_FRAME_SIZE).unwrap();
            matches!(welcome, ServerMessage::Welcome { error: None, .. })
        });
        assert!(tokio::time::timeout(Duration::from_secs(2), welcome)
            .await
            .expect("fresh attach must not wait behind rendering")
            .unwrap());
        set_idle_client_accept_test_blocked(false);
        assert!(
            RENDER_ACCEPT_TEST_COUNT.load(Ordering::Relaxed) > accepts_before_second,
            "the fresh attach must be accepted on a render iteration"
        );
        tokio::task::spawn_blocking(move || {
            protocol::write_message(&mut first_client, &protocol::ClientMessage::Detach).unwrap();
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        second_should_quit.store(true, Ordering::Release);
        event_tx.send(ServerEvent::QuitSignal).await.unwrap();
        second_render_notify.notify_one();
    };
    let renderer = async move {
        loop {
            if should_quit.load(Ordering::Acquire) {
                break;
            }
            render_dirty.request_generic();
            render_notify.notify_one();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let run = tokio::time::timeout(Duration::from_secs(4), server.run());
    let (result, ()) = tokio::join!(run, async {
        tokio::join!(second, renderer);
    });
    result
        .expect("continuously rendering server must remain responsive")
        .unwrap();
}

#[tokio::test]
async fn client_listener_transient_accept_errors_back_off_without_stopping_server() {
    let attempts = run_client_listener_attach(3).await;
    assert_eq!(
        attempts.len(),
        5,
        "three errors, one accepted client, one WouldBlock"
    );
    for pair in attempts[..4].windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= CLIENT_ACCEPT_ERROR_RETRY_INTERVAL,
            "cached readiness must not retry accept during error backoff"
        );
    }
}

async fn run_client_listener_attach(injected_errors: usize) -> Vec<Instant> {
    let mut server = test_headless_server();
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    CLIENT_ACCEPT_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(ClientAcceptTestHook {
            remaining_errors: injected_errors,
            attempts: attempts.clone(),
        });
    });
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    let path = server.client_socket_path.clone();
    let should_quit = server.should_quit.clone();
    let event_tx = server.server_event_tx.clone();
    let attach = async move {
        // Attach after run() has rendered once and reached its idle select.
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::task::spawn_blocking(move || {
            let mut client = crate::ipc::connect_local_stream(&path).unwrap();
            let crate::ipc::LocalStream::UdSocket(socket) = &client;
            socket
                .inner()
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            protocol::write_message(
                &mut client,
                &protocol::ClientMessage::TerminalHello {
                    version: protocol::PROTOCOL_VERSION,
                    cols: 80,
                    rows: 24,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    pixel_mouse: false,
                },
            )
            .unwrap();
            let welcome: ServerMessage =
                protocol::read_message(&mut client, MAX_FRAME_SIZE).unwrap();
            assert!(matches!(
                welcome,
                ServerMessage::Welcome { error: None, .. }
            ));
        })
        .await
        .unwrap();
        should_quit.store(true, Ordering::Release);
        event_tx.send(ServerEvent::QuitSignal).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        let (run, ()) = tokio::join!(server.run(), attach);
        run.unwrap();
    })
    .await
    .expect("idle run accepts a fresh client and shuts down without polling");
    assert_eq!(server.next_client_id, 2);
    assert!(!server.client_socket_path.exists());
    CLIENT_ACCEPT_TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
    let attempts = attempts.lock().unwrap().clone();
    attempts
}

#[tokio::test]
async fn client_listener_idle_server_shutdown_is_channel_driven() {
    let mut server = test_headless_server();
    // Keep the API channel open: a closed receiver is not an idle wait.
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    server.app.api_rx = api_rx;
    assert_eq!(
        server
            .app
            .next_headless_loop_deadline_with_git_refresh(Instant::now(), false, false),
        None,
        "the empty test server has no scheduled poll"
    );
    let should_quit = server.should_quit.clone();
    let event_tx = server.server_event_tx.clone();
    let shutdown = async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        should_quit.store(true, Ordering::Release);
        event_tx.send(ServerEvent::QuitSignal).await.unwrap();
    };
    let run = async {
        tokio::time::timeout(Duration::from_secs(2), server.run())
            .await
            .expect("shutdown wakes the select without an accept poll")
            .unwrap();
    };
    tokio::join!(run, shutdown);
    assert!(server.shutting_down);
    assert!(!server.client_socket_path.exists());
}
