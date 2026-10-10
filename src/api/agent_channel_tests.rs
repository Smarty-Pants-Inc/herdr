//! Deterministic tests for the actual dispatch/receipt state machine.
use super::*;
use crate::api::schema::AdmissionReason;
use std::cell::Cell;

fn channel() -> (Arc<Channel>, std::sync::mpsc::Receiver<Arc<Delivery>>) {
    let identity = ProcessIdentity {
        pid: 123,
        start_time: 456,
    };
    let pair = Channel::new(
        "term_test".into(),
        "epoch_test".into(),
        "session_test".into(),
        identity,
        identity,
        "w1".into(),
        crate::layout::PaneId::from_raw(1),
        None,
    );
    pair.0.mark_ready();
    pair
}
fn reserve(channel: &Channel, id: &str) -> (Arc<Delivery>, bool, ReceiptWaiter) {
    channel
        .reserve(
            id.into(),
            "literal; /command\nEnter".into(),
            Duration::from_secs(1),
        )
        .expect("reserve")
}
fn ack(status: AdmissionStatus) -> AdmissionAck {
    AdmissionAck {
        kind: "ack".into(),
        registration_epoch: "epoch_test".into(),
        request_id: "r".into(),
        session_generation: "session_test".into(),
        status,
        reason: None,
        duplicate: false,
    }
}
fn code(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Failure { code, .. } => code,
        other => panic!("unexpected outcome: {other:?}"),
    }
}
#[test]
fn channel_dedup_reserves_before_dispatch_and_shares_one_outcome() {
    let (channel, receiver) = channel();
    let (first, duplicate, _waiter) = reserve(&channel, "r");
    assert!(!duplicate);
    let (second, duplicate, _waiter2) = reserve(&channel, "r");
    assert!(duplicate);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(receiver.try_recv().unwrap().text, first.text);
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        code(
            channel
                .reserve("r".into(), "different".into(), Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "payload_mismatch"
    );
    let mut socket = Vec::new();
    write_delivery(&mut socket, &channel, &first, || true, || true).unwrap();
    assert!(socket.ends_with(b"\n"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&socket).unwrap()["text"],
        first.text
    );
    channel.ack(ack(AdmissionStatus::Accepted)).unwrap();
    assert_eq!(first.wait(), second.wait());
    let result: serde_json::Value =
        serde_json::from_str(&second.wait().response("caller".into(), true)).unwrap();
    assert_eq!(result["result"]["status"], "accepted");
    assert_eq!(result["result"]["duplicate"], true);
}
#[test]
fn channel_ack_requires_complete_dispatch_and_all_correlations() {
    let (channel, _receiver) = channel();
    let (delivery, _, _waiter) = reserve(&channel, "r");
    assert!(channel.ack(ack(AdmissionStatus::Accepted)).is_err());
    delivery.state.lock().unwrap().possible_dispatch = true;
    assert!(channel.ack(ack(AdmissionStatus::Accepted)).is_err());
    delivery.state.lock().unwrap().complete_dispatch = true;
    for field in ["epoch", "session", "request", "type"] {
        let mut receipt = ack(AdmissionStatus::Accepted);
        match field {
            "epoch" => receipt.registration_epoch = "old".into(),
            "session" => receipt.session_generation = "new".into(),
            "request" => receipt.request_id = "other".into(),
            _ => receipt.kind = "deliver".into(),
        }
        assert!(channel.ack(receipt).is_err());
        assert!(delivery.pending());
    }
    channel.ack(ack(AdmissionStatus::Queued)).unwrap();
    assert!(matches!(delivery.wait(), Outcome::Receipt(value) if value["status"] == "queued"));
}
#[test]
fn channel_rejected_ack_is_typed_failure_not_success() {
    let (channel, _receiver) = channel();
    let (delivery, _, _waiter) = reserve(&channel, "r");
    let mut socket = Vec::new();
    write_delivery(&mut socket, &channel, &delivery, || true, || true).unwrap();
    assert!(channel.ack(ack(AdmissionStatus::Rejected)).is_err());
    let mut rejection = ack(AdmissionStatus::Rejected);
    rejection.reason = Some(AdmissionReason::AdmissionRefused);
    channel.ack(rejection).unwrap();
    let response: serde_json::Value =
        serde_json::from_str(&delivery.wait().response("caller".into(), false)).unwrap();
    assert_eq!(response["error"]["code"], "agent_prompt_rejected");
    assert_eq!(response["error"]["reason"], "admission_refused");
    assert!(response.get("result").is_none());
    let first = delivery.wait();
    channel.revoke();
    let (duplicate, dup, _waiter) = channel.duplicate("r", &delivery.text).unwrap().unwrap();
    assert!(dup);
    assert_eq!(duplicate.wait(), first);
    let response: serde_json::Value =
        serde_json::from_str(&duplicate.wait().response("retry".into(), true)).unwrap();
    assert_eq!(response["error"]["reason"], "admission_refused");
    assert_eq!(response["error"]["duplicate"], true);
}
#[test]
fn channel_successful_attachment_check_then_exit_has_zero_socket_bytes() {
    let (channel, _receiver) = channel();
    let (delivery, _, _waiter) = reserve(&channel, "r");
    let checked = Cell::new(false);
    let mut socket = Vec::new();
    write_delivery(
        &mut socket,
        &channel,
        &delivery,
        || {
            checked.set(true);
            true
        },
        || {
            assert!(checked.get());
            false
        },
    )
    .unwrap();
    assert!(socket.is_empty());
    assert_eq!(code(delivery.wait()), "agent_channel_unavailable");
}
struct ScriptWriter {
    steps: std::collections::VecDeque<io::Result<usize>>,
    bytes: Vec<u8>,
}
impl Write for ScriptWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.steps.pop_front().unwrap_or(Ok(bytes.len()))?;
        self.bytes
            .extend_from_slice(&bytes[..count.min(bytes.len())]);
        Ok(count.min(bytes.len()))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn channel_partial_interrupted_and_wouldblock_retries_recheck_eligibility() {
    for initial in [
        Ok(3),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Err(io::Error::from(io::ErrorKind::WouldBlock)),
    ] {
        let partial = initial.is_ok();
        let (channel, _receiver) = channel();
        let (delivery, _, _waiter) = reserve(&channel, "r");
        let mut socket = ScriptWriter {
            steps: std::collections::VecDeque::from([initial]),
            bytes: Vec::new(),
        };
        let boundaries = Cell::new(0);
        let result = write_delivery(
            &mut socket,
            &channel,
            &delivery,
            || {
                let count = boundaries.get();
                boundaries.set(count + 1);
                count == 0
            },
            || true,
        );
        assert_eq!(result.is_err(), partial);
        assert_eq!(socket.bytes.len(), if partial { 3 } else { 0 });
        assert_eq!(boundaries.get(), 2);
        assert_eq!(code(delivery.wait()), "delivery_unknown");
        channel.revoke();
    }
}
#[test]
fn channel_revoke_timeout_and_lost_ack_never_replay() {
    for dispatched in [false, true] {
        let (channel, receiver) = channel();
        let (delivery, _, _waiter) = reserve(&channel, "r");
        if dispatched {
            let mut socket = Vec::new();
            write_delivery(&mut socket, &channel, &delivery, || true, || true).unwrap();
        }
        channel.revoke();
        assert_eq!(
            code(delivery.wait()),
            if dispatched {
                "delivery_unknown"
            } else {
                "agent_channel_unavailable"
            }
        );
        assert!(channel
            .reserve("r".into(), delivery.text.clone(), Duration::from_secs(1))
            .is_err());
        assert!(channel.ack(ack(AdmissionStatus::Accepted)).is_err());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
    let (channel, _receiver) = channel();
    let (delivery, _, _waiter) = channel
        .reserve("r".into(), "text".into(), Duration::ZERO)
        .unwrap();
    assert_eq!(code(delivery.wait()), "agent_channel_unavailable");
    assert!(
        channel
            .reserve("r".into(), "text".into(), Duration::from_secs(1))
            .unwrap()
            .1
    ); // Completed refusal remains deduplicated.
}
#[test]
fn channel_receipt_timeout_after_full_dispatch_stays_unknown_for_duplicates() {
    let (channel, _receiver) = channel();
    let (delivery, _, _waiter) = channel
        .reserve("r".into(), "text".into(), Duration::from_millis(20))
        .unwrap();
    write_delivery(&mut Vec::new(), &channel, &delivery, || true, || true).unwrap();
    assert_eq!(code(delivery.wait()), "delivery_unknown");
    channel.ack(ack(AdmissionStatus::Accepted)).unwrap(); // Late ACK cannot turn unknown into false success.
    let (duplicate, is_duplicate, _waiter2) = channel
        .reserve("r".into(), "text".into(), Duration::from_secs(1))
        .unwrap();
    assert!(is_duplicate);
    assert_eq!(code(duplicate.wait()), "delivery_unknown");
}
#[test]
fn channel_rotation_fences_new_keys_until_pending_is_terminal_and_keeps_unknown() {
    let (channel, receiver) = channel();
    for index in 0..ROTATE_LEDGER_KEYS - 1 {
        let (delivery, _, _waiter) = reserve(&channel, &format!("r{index}"));
        delivery.finish(Outcome::failure("test", "first reason"));
        let _ = receiver.try_recv().unwrap();
    }
    let (last, _, _waiter) = channel
        .reserve("last".into(), "literal".into(), Duration::from_millis(20))
        .unwrap();
    assert!(!channel.rotation_drained()); // Never rotate an unresolved reservation.
    assert_eq!(
        code(reserve_error(&channel, "new")),
        "agent_channel_rotating"
    );
    assert_eq!(receiver.try_iter().count(), 1); // Refusal is definite non-delivery.
    assert_eq!(
        channel.ledger.lock().unwrap().requests.len(),
        ROTATE_LEDGER_KEYS
    );
    assert!(reserve(&channel, "r0").1); // First refusal reason still readable during drain.
    write_delivery(&mut Vec::new(), &channel, &last, || true, || true).unwrap();
    assert_eq!(code(last.wait()), "delivery_unknown");
    assert!(channel.rotation_drained()); // A terminal unknown cannot strand the epoch forever.
    channel.revoke();
    let (duplicate, dup, _waiter) = channel.duplicate("last", "literal").unwrap().unwrap();
    assert!(dup);
    assert_eq!(code(duplicate.wait()), "delivery_unknown");
    assert!(channel.duplicate("unseen", "literal").unwrap().is_none());
    assert_eq!(
        code(channel.duplicate("last", "different").err().unwrap()),
        "payload_mismatch"
    );
    assert!(receiver.try_recv().is_err());
}
#[test]
fn channel_byte_high_water_rotates_before_hard_capacity() {
    let (channel, receiver) = channel();
    loop {
        let count = channel.ledger.lock().unwrap().requests.len();
        let (delivery, _, _waiter) = channel
            .reserve(
                format!("r{count}"),
                "x".repeat(MAX_FRAME_BYTES - 512),
                Duration::from_secs(1),
            )
            .unwrap();
        delivery.finish(Outcome::failure("test", "complete"));
        let _ = receiver.try_recv().unwrap();
        if channel.rotation_drained() {
            break;
        }
    }
    let ledger = channel.ledger.lock().unwrap();
    assert!(ledger.requests.len() < ROTATE_LEDGER_KEYS);
    assert!(ledger.bytes >= ROTATE_LEDGER_BYTES && ledger.bytes < MAX_LEDGER_BYTES);
    drop(ledger);
    assert_eq!(
        code(reserve_error(&channel, "new")),
        "agent_channel_rotating"
    );
    assert!(receiver.try_recv().is_err());
}
#[test]
fn channel_capacity_never_evicts_keys_and_duplicate_waiters_are_bounded() {
    let (channel, receiver) = channel();
    let (delivery, _, _waiter) = reserve(&channel, "r");
    let waiters: Vec<_> = (1..MAX_WAITERS_PER_REQUEST)
        .map(|_| reserve(&channel, "r").2)
        .collect();
    assert_eq!(code(reserve_error(&channel, "r")), "agent_channel_capacity");
    drop(waiters);
    delivery.finish(Outcome::failure("test", "complete"));
    let _ = receiver.try_recv();
    // Isolate the hard no-eviction limit; normal serving rotates at the high-water.
    for index in 1..MAX_LEDGER_KEYS {
        let (request, _, _waiter) = reserve(&channel, &format!("r{index}"));
        request.finish(Outcome::failure("test", "complete"));
        channel.rotating.store(false, Ordering::Release);
        let _ = receiver.try_recv();
    }
    assert_eq!(
        code(reserve_error(&channel, "overflow")),
        "agent_channel_capacity"
    );
    assert!(reserve(&channel, "r").1);
    assert_eq!(
        channel.ledger.lock().unwrap().requests.len(),
        MAX_LEDGER_KEYS
    );
}
#[test]
fn channel_retained_byte_capacity_accepts_exact_fit_and_refuses_one_more_before_dispatch() {
    let (channel, receiver) = channel();
    // Completed keys retain their real frame + text charge; do not seed a synthetic byte count.
    for index in 0..128 {
        let (delivery, duplicate, _waiter) = channel
            .reserve(
                format!("fill-{index:03}"),
                "x".repeat(MAX_FRAME_BYTES - 512),
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(!duplicate);
        assert!(Arc::ptr_eq(&delivery, &receiver.try_recv().unwrap()));
        delivery.finish(Outcome::failure("test", "complete"));
        // Exercise the hard byte bound separately from the earlier rotation trigger.
        channel.rotating.store(false, Ordering::Release);
    }
    assert!(receiver.try_recv().is_err());
    let retained = channel.ledger.lock().unwrap().bytes;
    let remaining = MAX_LEDGER_BYTES - retained;
    let overhead = serde_json::json!({"type":"deliver","registration_epoch":channel.epoch,
        "request_id":"r","session_generation":channel.session_generation,"text":""})
    .to_string()
    .len()
        + 1; // The delimiter is retained too. ASCII text adds one frame byte + one text byte.
    let request_id = if (remaining - overhead).is_multiple_of(2) {
        "r"
    } else {
        "rr"
    };
    let text = "x".repeat((remaining - overhead - (request_id.len() - 1)) / 2);
    let overflow_id = format!("{request_id}x"); // Exactly one extra retained byte, not two.
    let overflow_frame_bytes = overhead + overflow_id.len() - 1 + text.len();
    assert!(overflow_frame_bytes <= MAX_FRAME_BYTES);
    assert_eq!(overflow_frame_bytes + text.len(), remaining + 1);
    assert_eq!(
        code(
            channel
                .reserve(overflow_id.clone(), text.clone(), Duration::from_secs(30))
                .err()
                .unwrap()
        ),
        "agent_channel_capacity"
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(channel.ledger.lock().unwrap().bytes, retained);
    assert!(!channel
        .ledger
        .lock()
        .unwrap()
        .requests
        .contains_key(&overflow_id));
    assert_eq!(channel.waiters.load(Ordering::Acquire), 0);

    let (delivery, duplicate, _waiter) = channel
        .reserve(request_id.into(), text.clone(), Duration::from_secs(30))
        .unwrap();
    assert!(!duplicate);
    assert!(delivery.frame.ends_with(b"\n"));
    assert_eq!(delivery.frame.len() + delivery.text.len(), remaining);
    assert_eq!(channel.ledger.lock().unwrap().bytes, MAX_LEDGER_BYTES);
    assert!(!delivery.state.lock().unwrap().possible_dispatch);
    let (pending_duplicate, duplicate, _waiter2) = channel
        .reserve(request_id.into(), text.clone(), Duration::from_secs(30))
        .unwrap();
    assert!(duplicate);
    assert!(Arc::ptr_eq(&delivery, &pending_duplicate));
    assert!(Arc::ptr_eq(&delivery, &receiver.try_recv().unwrap()));
    assert!(receiver.try_recv().is_err());
    delivery.finish(Outcome::failure("test", "complete"));
    channel.rotating.store(false, Ordering::Release);
    let (completed_duplicate, duplicate, _waiter3) = channel
        .reserve(request_id.into(), text, Duration::from_secs(30))
        .unwrap();
    assert!(duplicate);
    assert!(Arc::ptr_eq(&delivery, &completed_duplicate));
    assert_eq!(completed_duplicate.wait(), delivery.wait());
    assert_eq!(
        code(reserve_error(&channel, request_id)),
        "payload_mismatch"
    );
    assert_eq!(
        code(reserve_error(&channel, "extra")),
        "agent_channel_capacity"
    );
    assert!(receiver.try_recv().is_err());
    let ledger = channel.ledger.lock().unwrap();
    assert_eq!(ledger.bytes, MAX_LEDGER_BYTES);
    assert_eq!(ledger.requests.len(), 129); // No dedup key eviction on completion or refusal.
}
#[test]
fn channel_waiter_budget_survives_300_epoch_replacements() {
    let (old, receiver) = channel();
    let mut waiters = Vec::new();
    for request in 0..(MAX_RECEIPT_WAITERS / MAX_WAITERS_PER_REQUEST) {
        for _ in 0..MAX_WAITERS_PER_REQUEST {
            waiters.push(reserve(&old, &format!("r{request}")).2);
        }
    }
    assert_eq!(receiver.try_iter().count(), 16);
    old.revoke();
    let mut current = old.clone();
    for index in 0..300 {
        let (next, receiver) = Channel::new(
            old.terminal_id.clone(),
            format!("epoch-{index}"),
            "session_test".into(),
            old.peer,
            old.root,
            old.workspace_id.clone(),
            old.pane_id,
            Some(&current),
        );
        next.mark_ready();
        assert!(Arc::ptr_eq(&next.waiters, &old.waiters));
        assert_eq!(code(reserve_error(&next, "new")), "agent_channel_capacity");
        assert!(receiver.try_recv().is_err());
        current = next;
    }
    drop(waiters);
    assert_eq!(old.waiters.load(Ordering::Acquire), 0);
    let (_, duplicate, _waiter) = reserve(&current, "new");
    assert!(!duplicate);
}
#[test]
fn channel_total_waiter_capacity_refuses_new_dispatch_before_effect() {
    let (channel, receiver) = channel();
    let mut waiters = Vec::new();
    for request in 0..(MAX_RECEIPT_WAITERS / MAX_WAITERS_PER_REQUEST) {
        for _ in 0..MAX_WAITERS_PER_REQUEST {
            waiters.push(reserve(&channel, &format!("r{request}")).2);
        }
    }
    assert_eq!(waiters.len(), MAX_RECEIPT_WAITERS);
    assert_eq!(
        code(reserve_error(&channel, "extra")),
        "agent_channel_capacity"
    );
    assert_eq!(
        receiver.try_iter().count(),
        MAX_RECEIPT_WAITERS / MAX_WAITERS_PER_REQUEST
    );
    assert!(!channel
        .ledger
        .lock()
        .unwrap()
        .requests
        .contains_key("extra"));
}
fn reserve_error(channel: &Channel, id: &str) -> Outcome {
    channel
        .reserve(
            id.into(),
            "literal; /command\nEnter".into(),
            Duration::from_secs(1),
        )
        .err()
        .unwrap()
}
#[test]
fn channel_inflight_and_frame_capacities_refuse_before_effect() {
    let (channel, receiver) = channel();
    for index in 0..MAX_IN_FLIGHT {
        let _ = reserve(&channel, &format!("r{index}"));
    }
    assert_eq!(
        code(reserve_error(&channel, "overflow")),
        "agent_channel_capacity"
    );
    assert_eq!(receiver.try_iter().count(), MAX_IN_FLIGHT);
    let (channel, receiver) = self::channel();
    assert_eq!(
        code(
            channel
                .reserve(
                    "large".into(),
                    "x".repeat(MAX_FRAME_BYTES),
                    Duration::from_secs(1)
                )
                .err()
                .unwrap()
        ),
        "agent_channel_capacity"
    );
    assert!(receiver.try_recv().is_err());
}
#[test]
fn channel_epoch_is_fresh_across_restart_handoff_and_slot_cancellation() {
    if crate::platform::capabilities().registered_agent_channel {
        let first = crate::platform::fresh_registration_epoch().unwrap();
        let second = crate::platform::fresh_registration_epoch().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
    }
    let transport = RegistrationTransport::default();
    assert!(transport.take().is_none());
    assert!(transport.0.lock().unwrap().closed); // Timed-out transport cannot install later.
}
#[cfg(target_os = "linux")]
mod transport {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    struct Fixture {
        path: std::path::PathBuf,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        _pty: portable_pty::PtyPair,
        running: Arc<AtomicBool>,
        app: Option<std::thread::JoinHandle<()>>,
        server: Option<std::thread::JoinHandle<io::Result<()>>>,
        channel: Arc<Channel>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Release);
            if let Some(gate) = self.channel.reader_gate.lock().unwrap().as_ref() {
                *gate.0.lock().unwrap() = true;
                gate.1.notify_all();
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
            if let Some(app) = self.app.take() {
                let _ = app.join();
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "transport condition timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    impl Fixture {
        fn new(mode: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "c176-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let socket = path.join("s");
            let listener = crate::ipc::bind_local_listener(&socket).unwrap();
            listener
                .set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Both)
                .unwrap();
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "api::agent_channel::tests::transport::channel_transport_peer",
                "--nocapture",
                "--test-threads=1",
            ]);
            command.env("CHANNEL_TEST_PATH", &path);
            command.env("CHANNEL_TEST_MODE", mode);
            let child = pair.slave.spawn_command(command).unwrap();
            let identity = crate::platform::process_identity(child.process_id().unwrap()).unwrap();
            let (channel, receiver) = Channel::new(
                "term_test".into(),
                if mode == "replacement" {
                    "replacement"
                } else {
                    "epoch_test"
                }
                .into(),
                "session_test".into(),
                identity,
                identity,
                "w1".into(),
                crate::layout::PaneId::from_raw(1),
                None,
            );
            if mode.starts_with("drain-") {
                *channel.reader_gate.lock().unwrap() =
                    Some(Arc::new((Mutex::new(false), Condvar::new())));
            }
            let mut fixture = Self {
                path,
                child,
                _pty: pair,
                running: Arc::new(AtomicBool::new(true)),
                app: None,
                server: None,
                channel: channel.clone(),
            };
            let mut stream = None;
            until(|| match listener.accept() {
                Ok(accepted) => {
                    stream = Some(accepted);
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
                Err(error) => panic!("accept: {error}"),
            });
            let stream = stream.unwrap();
            assert_eq!(
                crate::ipc::local_stream_peer_identity(&stream),
                Some(identity)
            );
            until(|| crate::platform::registered_process_is_foreground(identity, identity));
            let LocalStream::UdSocket(socket) = &stream;
            let size: libc::c_int = 4096;
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        socket.inner().as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
            let running = fixture.running.clone();
            let checked = channel.clone();
            fixture.app = Some(std::thread::spawn(move || {
                while running.load(Ordering::Acquire) {
                    if let Ok(message) = rx.try_recv() {
                        assert!(matches!(
                            message.request.method,
                            Method::AgentChannelInfo(_)
                        ));
                        let _ = message.respond_to.send(json_success(
                            message.request.id,
                            serde_json::json!({"ready":checked.is_ready(), "registration_epoch":checked.epoch}),
                        ));
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }));
            let running = fixture.running.clone();
            fixture.server = Some(std::thread::spawn(move || {
                serve(stream, channel, receiver, &tx, &running, None)
            }));
            until(|| fixture.channel.is_ready());
            fixture
        }
        fn release(&self) {
            std::fs::write(self.path.join("release"), b"").unwrap();
        }
        fn release_reader(&self) {
            let gate = self.channel.reader_gate.lock().unwrap().clone().unwrap();
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }
        fn join_server(&mut self) -> io::Result<()> {
            self.server.take().unwrap().join().unwrap()
        }
    }
    fn frame(stream: &mut LocalStream) -> Vec<u8> {
        let mut line = Vec::new();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => line.push(byte[0]),
                Err(error) if crate::ipc::is_connection_closed_error(&error) => break,
                Err(error) => panic!("peer frame read: {error}"),
            }
            if byte[0] == b'\n' {
                break;
            }
        }
        line
    }
    fn send_ack(stream: &mut LocalStream, request: &serde_json::Value, status: &str, mode: &str) {
        let mut receipt = serde_json::json!({"type":"ack", "registration_epoch":request["registration_epoch"],
            "session_generation":request["session_generation"], "request_id":request["request_id"], "status":status});
        if mode.ends_with("stale") {
            receipt["registration_epoch"] = "old_epoch".into();
        } else if mode == "replacement" {
            receipt["registration_epoch"] = "epoch_test".into();
        } else if mode == "early-b" {
            receipt["request_id"] = "b".into();
        }
        let line = format!("{receipt}\n");
        stream
            .write_all(if mode.ends_with("partial") {
                &line.as_bytes()[..line.len() / 2]
            } else {
                line.as_bytes()
            })
            .unwrap();
    }
    // Reexecuted in a real foreground PTY process; no mock writer or direct ack() call.
    #[test]
    fn channel_transport_peer() {
        let Some(path) = std::env::var_os("CHANNEL_TEST_PATH") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let mode = std::env::var("CHANNEL_TEST_MODE").unwrap();
        if mode.starts_with("rollover") || mode == "registrations" {
            let mut delivered = 0;
            let mut epochs = std::collections::HashSet::new();
            for registration in 0..300 {
                let mut stream = crate::ipc::connect_local_stream(&path.join("s")).unwrap();
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"id":"register","method":"agent.register_self",
                    "params":{"session_generation":"session_test"}})
                )
                .unwrap();
                let response: serde_json::Value =
                    serde_json::from_slice(&frame(&mut stream)).unwrap();
                assert_eq!(response["result"]["ready"], true, "{response}");
                let epoch = response["result"]["registration_epoch"].as_str().unwrap();
                assert!(epochs.insert(epoch.to_owned()));
                std::fs::write(path.join("registrations"), (registration + 1).to_string()).unwrap();
                if mode == "registrations" {
                    continue;
                }
                loop {
                    let line = frame(&mut stream);
                    assert!(
                        !line.is_empty(),
                        "rollover must send an explicit control frame"
                    );
                    let request: serde_json::Value = serde_json::from_slice(&line).unwrap();
                    assert_eq!(request["registration_epoch"], epoch);
                    assert_eq!(request["session_generation"], "session_test");
                    if request["type"] == "rotate" {
                        std::fs::write(path.join("rotated"), b"").unwrap();
                        break;
                    }
                    assert_eq!(request["type"], "deliver");
                    assert_eq!(request["request_id"], format!("prompt-{delivered}"));
                    assert_eq!(request["text"], format!("literal-{delivered}"));
                    if mode != "rollover-unknown" || delivered != 191 {
                        send_ack(
                            &mut stream,
                            &request,
                            if delivered % 2 == 0 {
                                "accepted"
                            } else {
                                "queued"
                            },
                            "normal",
                        );
                    }
                    delivered += 1;
                    if delivered == 300 {
                        std::fs::write(path.join("delivered"), delivered.to_string()).unwrap();
                        until(|| path.join("done").exists());
                        return;
                    }
                }
            }
            std::fs::write(path.join("registered"), epochs.len().to_string()).unwrap();
            until(|| path.join("done").exists());
            return;
        }
        let mut stream = crate::ipc::connect_local_stream(&path.join("s")).unwrap();
        let a: serde_json::Value = serde_json::from_slice(&frame(&mut stream)).unwrap();
        let mut first = [0];
        stream.read_exact(&mut first).unwrap(); // B has actually started its partial frame.
        if !mode.ends_with("absent") && mode != "late" {
            send_ack(
                &mut stream,
                &a,
                if mode.ends_with("queued") {
                    "queued"
                } else {
                    "accepted"
                },
                &mode,
            );
        }
        std::fs::write(path.join("blocked"), first).unwrap();
        until(|| path.join("release").exists()); // Stop draining B: native socket backpressure.
        if mode == "late" {
            send_ack(&mut stream, &a, "accepted", &mode);
        }
        let mut b = first.to_vec();
        b.extend(frame(&mut stream));
        std::fs::write(path.join("b-frame"), &b).unwrap();
        if !b.ends_with(b"\n") {
            return;
        }
        let b: serde_json::Value = serde_json::from_slice(&b).unwrap();
        send_ack(&mut stream, &b, "queued", "normal");
        for _ in 0..8 {
            let line = frame(&mut stream);
            if line.is_empty() {
                return;
            }
            let request: serde_json::Value = serde_json::from_slice(&line).unwrap();
            send_ack(&mut stream, &request, "accepted", "normal");
        }
        until(|| path.join("done").exists());
    }
    fn deliveries(
        fixture: &Fixture,
        b_timeout: Duration,
    ) -> (Arc<Delivery>, ReceiptWaiter, Arc<Delivery>, ReceiptWaiter) {
        let (a, _, aw) = fixture
            .channel
            .reserve("a".into(), "A".into(), Duration::from_millis(600))
            .unwrap();
        let (b, _, bw) = fixture
            .channel
            .reserve("b".into(), "B".repeat(48 * 1024), b_timeout)
            .unwrap();
        until(|| fixture.path.join("blocked").exists());
        assert!(a.state.lock().unwrap().complete_dispatch);
        assert!(b.state.lock().unwrap().possible_dispatch);
        assert!(!b.state.lock().unwrap().complete_dispatch);
        (a, aw, b, bw)
    }
    #[test]
    fn channel_transport_ack_survives_backpressure_past_deadline_and_multiple_acks() {
        for status in ["accepted", "queued"] {
            let mut fixture = Fixture::new(status);
            let (a, _aw, b, _bw) = deliveries(&fixture, Duration::from_secs(4));
            let receipt = a.wait();
            assert!(matches!(&receipt, Outcome::Receipt(value) if value["status"] == status));
            std::thread::sleep(
                a.deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100),
            );
            assert!(b.pending());
            assert!(!b.state.lock().unwrap().complete_dispatch);
            assert_eq!(a.wait(), receipt);
            let (duplicate, dup, _dw) = fixture
                .channel
                .reserve("a".into(), "A".into(), Duration::from_secs(1))
                .unwrap();
            assert!(dup);
            assert_eq!(duplicate.wait(), receipt);
            fixture.release();
            assert!(matches!(b.wait(), Outcome::Receipt(value) if value["status"] == "queued"));
            let requests: Vec<_> = (0..8)
                .map(|index| {
                    fixture
                        .channel
                        .reserve(format!("c{index}"), "C".into(), Duration::from_secs(2))
                        .unwrap()
                })
                .collect();
            for (request, _, _) in &requests {
                assert!(matches!(request.wait(), Outcome::Receipt(_)));
            }
            std::fs::write(fixture.path.join("done"), b"").unwrap();
            assert!(fixture.join_server().is_ok());
            assert!(!fixture.channel.is_active());
        }
    }
    #[test]
    fn channel_transport_ack_survives_partial_expiry_without_extra_complete_frame() {
        for status in ["accepted", "queued"] {
            let mut fixture = Fixture::new(status);
            let (a, _aw, b, _bw) = deliveries(&fixture, Duration::from_millis(1100));
            let receipt = a.wait();
            assert!(matches!(&receipt, Outcome::Receipt(value) if value["status"] == status));
            assert!(fixture.join_server().is_err());
            assert_eq!(a.wait(), receipt);
            assert_eq!(code(b.wait()), "delivery_unknown");
            assert!(!fixture.channel.is_active());
            assert!(fixture
                .channel
                .reserve("b".into(), b.text.clone(), Duration::from_secs(1))
                .is_err());
            fixture.release();
            until(|| fixture.path.join("b-frame").exists());
            let bytes = std::fs::read(fixture.path.join("b-frame")).unwrap();
            assert!(!bytes.contains(&b'\n'));
            assert!(bytes.len() < b.frame.len());
        }
    }
    #[test]
    fn channel_transport_stop_drains_buffered_ack_after_partial_expiry_before_reader_scheduled() {
        for mode in [
            "drain-accepted",
            "drain-queued",
            "drain-absent",
            "drain-partial",
            "drain-stale",
            "drain-revoked",
        ] {
            let mut fixture = Fixture::new(mode);
            let (a, _, _aw) = fixture
                .channel
                .reserve("a".into(), "A".into(), Duration::from_secs(3))
                .unwrap();
            let (b, _, _bw) = fixture
                .channel
                .reserve(
                    "b".into(),
                    "B".repeat(48 * 1024),
                    Duration::from_millis(250),
                )
                .unwrap();
            until(|| fixture.path.join("blocked").exists());
            until(|| fixture.channel.writer_stopped.load(Ordering::Acquire));
            assert!(a.pending()); // ACK is in the socket; reader has never run.
            assert!(a.state.lock().unwrap().complete_dispatch);
            assert!(!b.state.lock().unwrap().complete_dispatch);
            assert!(fixture.channel.is_active());
            if mode == "drain-revoked" {
                fixture.channel.revoke(); // Buffered old ACK must not survive structural revocation.
            }
            fixture.release_reader();
            assert!(fixture.join_server().is_err()); // Partial B always retires the transport.
            let first = a.wait();
            if mode == "drain-accepted" || mode == "drain-queued" {
                let status = mode.strip_prefix("drain-").unwrap();
                assert!(matches!(&first, Outcome::Receipt(value) if value["status"] == status));
            } else {
                assert_eq!(code(first.clone()), "delivery_unknown");
            }
            assert_eq!(a.wait(), first);
            assert_eq!(code(b.wait()), "delivery_unknown");
            assert!(!fixture.channel.is_active());
            fixture.release();
            until(|| fixture.path.join("b-frame").exists());
            assert!(!std::fs::read(fixture.path.join("b-frame"))
                .unwrap()
                .contains(&b'\n'));
        }
    }
    #[test]
    fn channel_transport_absent_stale_and_late_ack_never_create_success() {
        for mode in ["absent", "stale", "late", "replacement", "early-b"] {
            let mut fixture = Fixture::new(mode);
            let (a, _aw, b, _bw) = deliveries(&fixture, Duration::from_secs(3));
            assert_eq!(code(a.wait()), "delivery_unknown");
            if matches!(mode, "stale" | "replacement" | "early-b") {
                if mode == "replacement" {
                    assert_eq!(fixture.channel.epoch, "replacement");
                }
                assert!(fixture.join_server().is_err());
                assert!(!fixture.channel.is_active());
                assert_eq!(code(b.wait()), "delivery_unknown");
                fixture.release();
                until(|| fixture.path.join("b-frame").exists());
                assert!(!std::fs::read(fixture.path.join("b-frame"))
                    .unwrap()
                    .contains(&b'\n'));
            } else {
                fixture.release();
                assert!(matches!(b.wait(), Outcome::Receipt(_)));
                assert_eq!(code(a.wait()), "delivery_unknown");
                std::fs::write(fixture.path.join("done"), b"").unwrap();
                fixture.running.store(false, Ordering::Release);
                assert!(fixture.join_server().is_ok());
            }
        }
    }
}

#[test]
fn channel_ack_parser_is_closed_typed_and_complete() {
    let base = serde_json::json!({"type":"ack","registration_epoch":"e","request_id":"r","session_generation":"s","status":"accepted"});
    let bytes = base.to_string().into_bytes();
    for length in 0..bytes.len() {
        assert!(serde_json::from_slice::<AdmissionAck>(&bytes[..length]).is_err());
    }
    let mut invalid = base.clone();
    invalid["status"] = "received".into();
    assert!(serde_json::from_value::<AdmissionAck>(invalid).is_err());
    let mut invalid = base;
    invalid["reason"] = "invented".into();
    assert!(serde_json::from_value::<AdmissionAck>(invalid).is_err());
}

#[test]
fn channel_draft_guard_identity_deadline_and_retired_duplicate_are_immutable() {
    let (channel, receiver) = channel();
    channel.set_draft_guard(true);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (delivery, duplicate, _waiter) = channel
        .reserve_prompt("r".into(), "literal".into(), true, Duration::from_secs(1))
        .unwrap();
    assert!(!duplicate);
    let frame: serde_json::Value = serde_json::from_slice(&delivery.frame).unwrap();
    assert_eq!(frame["if_draft_empty"], true);
    let deadline = frame["deadline_ms"].as_u64().unwrap();
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!((before + 1000..=after + 1000).contains(&deadline));
    assert!(deadline <= MAX_SAFE_JSON_INTEGER);
    let (same, duplicate, _waiter2) = channel
        .reserve_prompt("r".into(), "literal".into(), true, Duration::from_secs(10))
        .unwrap();
    assert!(duplicate);
    assert!(Arc::ptr_eq(&same, &delivery));
    assert_eq!(same.frame, delivery.frame); // A retry never extends receiver admission authority.
    assert_eq!(
        code(
            channel
                .reserve_prompt("r".into(), "literal".into(), false, Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "payload_mismatch"
    );
    assert_eq!(
        code(
            channel
                .reserve_draft_state("r".into(), Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "payload_mismatch"
    );
    write_delivery(&mut Vec::new(), &channel, &delivery, || true, || true).unwrap();
    channel.ack(ack(AdmissionStatus::Accepted)).unwrap();
    let first = delivery.wait();
    channel.revoke();
    let (old, duplicate, _waiter3) = channel
        .duplicate_prompt("r", "literal", true)
        .unwrap()
        .unwrap();
    assert!(duplicate);
    assert_eq!(old.wait(), first);
    assert_eq!(
        code(
            channel
                .duplicate_prompt("r", "literal", false)
                .err()
                .unwrap()
        ),
        "payload_mismatch"
    );
    assert_eq!(receiver.try_iter().count(), 1);
}

/// End-to-end tests cannot force preemption between the two clock reads, so this
/// models it deterministically: one simulated real timeline serves both injected
/// clocks, and every read is followed by a scheduling delay before the next.
/// Assumption: the wall clock stays in step with the monotonic clock (no wall-clock
/// adjustment). Under that assumption only, read-order delay cannot let the receiver
/// deadline outlive Herdr's; a later backward wall-clock step is out of scope here.
#[test]
fn channel_guarded_deadline_wall_before_monotonic_bounds_read_delay_on_stable_wall_clock() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timeout = Duration::from_millis(1_500);
    for delay_ms in [0_u64, 1, 250, 5_000] {
        let real_ms = Cell::new(1_000_000_u64);
        let reads = std::cell::RefCell::new(Vec::new());
        let base = Instant::now();
        let read = |clock: &'static str| {
            reads.borrow_mut().push(clock);
            let now = real_ms.get();
            real_ms.set(now + delay_ms); // Preempted right after this read.
            now
        };
        let (deadline, deadline_ms) = request_deadlines(
            timeout,
            true,
            || UNIX_EPOCH + Duration::from_millis(read("wall")),
            || base + Duration::from_millis(read("monotonic")),
        )
        .unwrap();
        assert_eq!(*reads.borrow(), ["wall", "monotonic"]);
        // Wall time equals simulated real time, so deadline_ms names a real instant.
        // Herdr's own deadline in the same timeline is its monotonic offset.
        let receiver_deadline = deadline_ms.unwrap();
        let herdr_deadline = u64::try_from((deadline - base).as_millis()).unwrap();
        assert_eq!(receiver_deadline, 1_000_000 + 1_500);
        assert_eq!(herdr_deadline, 1_000_000 + delay_ms + 1_500);
        assert!(receiver_deadline <= herdr_deadline, "delay {delay_ms}");
    }
    // Unguarded prompts and draft queries never read the wall clock, so their
    // monotonic timeout behaviour is unchanged.
    let base = Instant::now();
    assert_eq!(
        request_deadlines(
            timeout,
            false,
            || panic!("unguarded request read the wall clock"),
            || base
        ),
        Some((base + timeout, None))
    );
    // Unrepresentable wall deadlines refuse without sampling Herdr's deadline.
    let at_limit = UNIX_EPOCH + Duration::from_millis(MAX_SAFE_JSON_INTEGER - 1_500);
    assert_eq!(
        request_deadlines(timeout, true, || at_limit, || base).map(|(_, ms)| ms),
        Some(Some(MAX_SAFE_JSON_INTEGER))
    );
    for (wall, timeout) in [
        (at_limit + Duration::from_millis(1), timeout),
        (UNIX_EPOCH - Duration::from_secs(10), timeout),
        (SystemTime::now(), Duration::MAX),
    ] {
        assert_eq!(
            request_deadlines(
                timeout,
                true,
                || wall,
                || panic!("monotonic deadline sampled after wall refusal")
            ),
            None
        );
    }
}

#[test]
fn channel_draft_guard_expired_before_dispatch_writes_nothing_and_keeps_legacy_frame() {
    let (channel, receiver) = channel();
    channel.set_draft_guard(true);
    let (guarded, _, _waiter) = channel
        .reserve_prompt("expired".into(), "literal".into(), true, Duration::ZERO)
        .unwrap();
    let mut socket = Vec::new();
    write_delivery(
        &mut socket,
        &channel,
        &guarded,
        || panic!("expired before attribution"),
        || true,
    )
    .unwrap();
    assert!(socket.is_empty());
    assert_eq!(code(guarded.wait()), "agent_channel_unavailable");
    assert!(!guarded.state.lock().unwrap().possible_dispatch);
    let (legacy, _, _waiter2) = channel
        .reserve_prompt(
            "legacy".into(),
            "literal".into(),
            false,
            Duration::from_secs(1),
        )
        .unwrap();
    let frame: serde_json::Value = serde_json::from_slice(&legacy.frame).unwrap();
    assert!(frame.get("if_draft_empty").is_none());
    assert!(frame.get("deadline_ms").is_none());
    assert_eq!(receiver.try_iter().count(), 2);
}

#[test]
fn channel_draft_guard_missing_capability_refuses_before_ledger_or_dispatch() {
    let (channel, receiver) = channel();
    assert!(!channel.supports_draft_guard());
    let outcome = channel
        .reserve_prompt("r".into(), "literal".into(), true, Duration::from_secs(1))
        .err()
        .unwrap();
    let response: serde_json::Value =
        serde_json::from_str(&outcome.response("caller".into(), false)).unwrap();
    assert_eq!(response["error"]["code"], "agent_prompt_rejected");
    assert_eq!(response["error"]["reason"], "unsupported");
    assert_eq!(
        channel
            .reserve_draft_state("q".into(), Duration::from_secs(1))
            .err()
            .unwrap(),
        Outcome::draft_unknown(DraftStateUnknownReason::Unsupported)
    );
    assert!(channel.ledger.lock().unwrap().requests.is_empty());
    assert_eq!(channel.waiters.load(Ordering::Acquire), 0);
    assert!(receiver.try_recv().is_err());
    // The capability does not disable the established unguarded path.
    assert!(channel
        .reserve("legacy".into(), "literal".into(), Duration::from_secs(1))
        .is_ok());
}

#[test]
fn channel_draft_guard_rejection_reasons_remain_typed_and_retained() {
    for (reason, expected) in [
        (AdmissionReason::DraftPresent, "draft_present"),
        (AdmissionReason::UiHold, "ui_hold"),
        (AdmissionReason::Unknown, "unknown"),
        (AdmissionReason::Expired, "expired"),
    ] {
        let (channel, receiver) = channel();
        channel.set_draft_guard(true);
        let (delivery, _, _waiter) = channel
            .reserve_prompt("r".into(), "literal".into(), true, Duration::from_secs(1))
            .unwrap();
        write_delivery(&mut Vec::new(), &channel, &delivery, || true, || true).unwrap();
        let mut rejection = ack(AdmissionStatus::Rejected);
        rejection.reason = Some(reason);
        channel.ack(rejection).unwrap();
        let response: serde_json::Value =
            serde_json::from_str(&delivery.wait().response("caller".into(), false)).unwrap();
        assert_eq!(response["error"]["reason"], expected);
        channel.revoke();
        let (retained, duplicate, _waiter2) = channel
            .duplicate_prompt("r", "literal", true)
            .unwrap()
            .unwrap();
        assert!(duplicate);
        assert_eq!(retained.wait(), delivery.wait());
        assert_eq!(receiver.try_iter().count(), 1);
    }
}

fn draft_ack_value() -> serde_json::Value {
    serde_json::json!({"type":"draft_state", "registration_epoch":"epoch_test", "request_id":"q",
        "session_generation":"session_test", "empty":true, "hold":null})
}

#[test]
fn channel_draft_query_known_and_unknown_project_no_text_or_correlation() {
    for (empty, hold) in [
        (true, serde_json::Value::Null),
        (false, "dialog".into()),
        (true, "custom".into()),
        (false, "editor".into()),
    ] {
        let (channel, receiver) = channel();
        channel.set_draft_guard(true);
        let (query, _, _waiter) = channel
            .reserve_draft_state("q".into(), Duration::from_secs(1))
            .unwrap();
        let mut bytes = Vec::new();
        write_delivery(&mut bytes, &channel, &query, || true, || true).unwrap();
        let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(frame["type"], "draft_state");
        assert!(frame.get("text").is_none());
        assert_eq!(frame.as_object().unwrap().len(), 4);
        let mut receipt = draft_ack_value();
        receipt["empty"] = empty.into();
        receipt["hold"] = hold.clone();
        channel
            .draft_ack(serde_json::from_value(receipt).unwrap())
            .unwrap();
        let response: serde_json::Value =
            serde_json::from_str(&query.wait().response("caller".into(), false)).unwrap();
        assert_eq!(
            response["result"],
            serde_json::json!({"status":"known", "empty":empty, "hold":hold})
        );
        assert_eq!(receiver.try_iter().count(), 1);
    }
    let (channel, _receiver) = channel();
    channel.set_draft_guard(true);
    let (query, _, _waiter) = channel
        .reserve_draft_state("q".into(), Duration::from_secs(1))
        .unwrap();
    write_delivery(&mut Vec::new(), &channel, &query, || true, || true).unwrap();
    channel.draft_ack(serde_json::from_value(serde_json::json!({"type":"draft_state", "registration_epoch":"epoch_test", "request_id":"q", "session_generation":"session_test", "unknown":true})).unwrap()).unwrap();
    assert_eq!(
        query.wait(),
        Outcome::draft_unknown(DraftStateUnknownReason::Unknown)
    );
}

#[test]
fn channel_draft_query_parser_and_correlation_fail_closed() {
    let base = draft_ack_value();
    for duplicate in ["\"request_id\":\"q\"", "\"empty\":true"] {
        let bytes = format!("{{{},{}", duplicate, &base.to_string()[1..]);
        assert!(
            serde_json::from_str::<DraftStateAck>(&bytes).is_err(),
            "duplicate field: {duplicate}"
        );
    }
    for field in [
        "type",
        "registration_epoch",
        "request_id",
        "session_generation",
        "empty",
        "hold",
    ] {
        let mut absent = base.clone();
        absent.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<DraftStateAck>(absent).is_err(),
            "missing {field}"
        );
    }
    for (field, invalid) in [
        ("text", "private draft".into()),
        // A draft size is private too: even a well-formed count is an unknown field.
        ("chars", 0.into()),
        ("chars", 7.into()),
        ("length", 7.into()),
        ("empty", serde_json::Value::Null),
        ("hold", "unknown".into()),
        ("unknown", true.into()),
    ] {
        let mut invalid_frame = base.clone();
        invalid_frame[field] = invalid;
        assert!(
            serde_json::from_value::<DraftStateAck>(invalid_frame).is_err(),
            "invalid {field}"
        );
    }
    let (channel, _receiver) = channel();
    channel.set_draft_guard(true);
    let (query, _, _waiter) = channel
        .reserve_draft_state("q".into(), Duration::from_secs(1))
        .unwrap();
    assert!(channel
        .draft_ack(serde_json::from_value(base.clone()).unwrap())
        .is_err());
    write_delivery(&mut Vec::new(), &channel, &query, || true, || true).unwrap();
    for field in [
        "type",
        "registration_epoch",
        "request_id",
        "session_generation",
    ] {
        let mut wrong = base.clone();
        wrong[field] = "other".into();
        assert!(channel
            .draft_ack(serde_json::from_value(wrong).unwrap())
            .is_err());
        assert!(query.pending());
    }
    let unknown_false = serde_json::json!({"type":"draft_state", "registration_epoch":"epoch_test", "request_id":"q", "session_generation":"session_test", "unknown":false});
    assert!(channel
        .draft_ack(serde_json::from_value(unknown_false).unwrap())
        .is_err());
    let mut admission = ack(AdmissionStatus::Accepted);
    admission.request_id = "q".into();
    assert!(channel.ack(admission).is_err());
    assert_eq!(
        code(
            channel
                .reserve_prompt("q".into(), String::new(), false, Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "payload_mismatch"
    );
    let (prompt, _, _waiter2) = channel
        .reserve("p".into(), "literal".into(), Duration::from_secs(1))
        .unwrap();
    write_delivery(&mut Vec::new(), &channel, &prompt, || true, || true).unwrap();
    let mut wrong_kind = base;
    wrong_kind["request_id"] = "p".into();
    assert!(channel
        .draft_ack(serde_json::from_value(wrong_kind).unwrap())
        .is_err());
}

#[test]
fn channel_draft_query_timeout_is_bounded_retained_and_separate_from_prompt_uncertainty() {
    for dispatch in [false, true] {
        let (channel, receiver) = channel();
        channel.set_draft_guard(true);
        let (query, _, _waiter) = channel
            .reserve_draft_state("q".into(), Duration::from_millis(15))
            .unwrap();
        if dispatch {
            write_delivery(&mut Vec::new(), &channel, &query, || true, || true).unwrap();
        }
        assert_eq!(
            query.wait(),
            Outcome::draft_unknown(DraftStateUnknownReason::Timeout)
        );
        if dispatch {
            channel
                .draft_ack(serde_json::from_value(draft_ack_value()).unwrap())
                .unwrap();
        }
        let (duplicate, is_duplicate, _waiter2) = channel
            .reserve_draft_state("q".into(), Duration::from_secs(1))
            .unwrap();
        assert!(is_duplicate);
        assert_eq!(
            duplicate.wait(),
            Outcome::draft_unknown(DraftStateUnknownReason::Timeout)
        );
        assert_eq!(receiver.try_iter().count(), 1);
    }
    let (channel, _receiver) = channel();
    channel.set_draft_guard(true);
    let (query, _, _waiter) = channel
        .reserve_draft_state("q".into(), Duration::ZERO)
        .unwrap();
    let mut bytes = Vec::new();
    write_delivery(&mut bytes, &channel, &query, || true, || true).unwrap();
    assert!(bytes.is_empty());
    assert_eq!(
        query.wait(),
        Outcome::draft_unknown(DraftStateUnknownReason::Timeout)
    );
    let (prompt, _, _waiter2) = channel
        .reserve_prompt(
            "r".into(),
            "literal".into(),
            true,
            Duration::from_millis(15),
        )
        .unwrap();
    write_delivery(&mut bytes, &channel, &prompt, || true, || true).unwrap();
    assert_eq!(code(prompt.wait()), "delivery_unknown");
}

#[test]
fn channel_draft_queries_share_dispatch_and_waiter_budgets_with_prompts() {
    let (channel, receiver) = channel();
    channel.set_draft_guard(true);
    for index in 0..MAX_IN_FLIGHT {
        if index % 2 == 0 {
            let _ = reserve(&channel, &format!("p{index}"));
        } else {
            let _ = channel
                .reserve_draft_state(format!("q{index}"), Duration::from_secs(1))
                .unwrap();
        }
    }
    assert_eq!(
        code(
            channel
                .reserve_draft_state("overflow".into(), Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "agent_channel_capacity"
    );
    assert_eq!(receiver.try_iter().count(), MAX_IN_FLIGHT);
    let (channel, receiver) = self::channel();
    channel.set_draft_guard(true);
    let waiters: Vec<_> = (0..MAX_WAITERS_PER_REQUEST)
        .map(|_| {
            channel
                .reserve_draft_state("q".into(), Duration::from_secs(1))
                .unwrap()
                .2
        })
        .collect();
    assert_eq!(
        code(
            channel
                .reserve_draft_state("q".into(), Duration::from_secs(1))
                .err()
                .unwrap()
        ),
        "agent_channel_capacity"
    );
    assert_eq!(
        channel.waiters.load(Ordering::Acquire),
        MAX_WAITERS_PER_REQUEST
    );
    assert_eq!(receiver.try_iter().count(), 1);
    drop(waiters);
    assert_eq!(channel.waiters.load(Ordering::Acquire), 0);
}
