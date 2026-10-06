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
    for index in 1..MAX_LEDGER_KEYS {
        let (request, _, _waiter) = reserve(&channel, &format!("r{index}"));
        request.finish(Outcome::failure("test", "complete"));
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
