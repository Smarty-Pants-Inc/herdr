use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
fn request(seq: u64, token: &str, cut: u64, bytes: &[u8]) -> CutRequest {
    CutRequest {
        epoch: "epoch".into(),
        epoch_key: "secret".into(),
        seq,
        token: token.into(),
        cut,
        digest: format!("{:x}", Sha256::digest(bytes)),
        kind: CutKind::Submit,
    }
}
fn result(response: ConsumerResponse) -> CutResult {
    match response {
        ConsumerResponse::Cut(r) => r,
        _ => panic!("not a cut"),
    }
}
fn sink() -> AuditSink {
    Arc::new(|_| Ok(()))
}
fn client(id: u64) -> InputSource {
    InputSource::Client {
        connection_id: id,
        principal: Some(Principal {
            smarty_id: format!("p{id}"),
            display_name: format!("Person{id}"),
        }),
    }
}
#[test]
fn input_consumer_ledger_sources_partial_cuts_and_after_cut_preserved() {
    let now = Instant::now();
    let audit = sink();
    for (sources, expected) in [
        (
            vec![client(1), client(1)],
            CutResult::Client {
                principal: match client(1) {
                    InputSource::Client { principal, .. } => principal,
                    _ => None,
                },
            },
        ),
        (
            vec![
                InputSource::Client {
                    connection_id: 1,
                    principal: None,
                },
                InputSource::Neutral,
            ],
            CutResult::Client { principal: None },
        ),
        (vec![client(1), client(2)], CutResult::Mixed),
        (vec![client(1), InputSource::Api], CutResult::Mixed),
        (vec![InputSource::Api, InputSource::Neutral], CutResult::Api),
        (
            vec![client(1), InputSource::Unknown],
            unknown("unknown_input"),
        ),
        (
            vec![InputSource::Neutral, InputSource::Neutral],
            unknown("empty_interval"),
        ),
    ] {
        let mut ledger = Ledger::new("epoch".into(), "secret".into());
        ledger.record(b"a", &sources[0], now);
        ledger.record(b"\r", &sources[1], now);
        assert_eq!(
            result(ledger.cut(request(1, "t", 2, b"a\r"), Some(&audit), now)),
            expected
        );
        assert_eq!(ledger.previous, 2);
        assert!(ledger.spans.is_empty());
    }
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"A\rB", &client(1), now);
    assert!(matches!(
        result(ledger.cut(request(1, "a", 2, b"A\r"), Some(&audit), now)),
        CutResult::Client { .. }
    ));
    ledger.record(b"\r", &InputSource::Api, now);
    assert_eq!(
        result(ledger.cut(request(2, "b", 4, b"B\r"), Some(&audit), now)),
        CutResult::Mixed
    );
}
#[test]
fn input_consumer_ledger_exact_retry_conflict_kind_seq_and_durable_once() {
    let now = Instant::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let audit: AuditSink = Arc::new(move |r| {
        let json = serde_json::to_string(r).expect("record json");
        assert!(
            !json.contains("secret")
                && !json.contains("epoch_key")
                && !json.contains("nonce")
                && !json.contains("raw")
        );
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"x\r", &client(1), now);
    let req = request(1, "one", 2, b"x\r");
    let first = result(ledger.cut(req.clone(), Some(&audit), now));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result(ledger.cut(req.clone(), None, now)), first);
    for altered in [
        CutRequest {
            seq: 2,
            ..req.clone()
        },
        CutRequest {
            cut: 3,
            ..req.clone()
        },
        CutRequest {
            digest: "bad".into(),
            ..req.clone()
        },
        CutRequest {
            kind: CutKind::Discard,
            ..req.clone()
        },
    ] {
        assert!(
            matches!(ledger.cut(altered,Some(&audit),now),ConsumerResponse::Refused { reason } if reason == "token_conflict")
        );
    }
    assert_eq!(
        result(ledger.cut(
            CutRequest {
                token: "new".into(),
                ..req
            },
            Some(&audit),
            now
        )),
        unknown("seq_mismatch")
    );
    // Review F2: every returned result is logged, but a stale seq is not
    // replayed and does not consume the next valid seq.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    ledger.record(b"y\r", &client(1), now);
    assert_eq!(
        result(ledger.cut(request(2, "two", 4, b"y\r"), Some(&audit), now)),
        first
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    ledger.poison("termios_changed");
    assert_eq!(
        result(ledger.cut(request(1, "one", 2, b"x\r"), Some(&audit), now)),
        unknown("termios_changed")
    );
    // The replay's changed answer is logged (review #188 P2), and so is a new token.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        result(ledger.cut(request(3, "three", 4, b""), Some(&audit), now)),
        unknown("termios_changed")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}
#[test]
fn input_consumer_ledger_early_unknown_with_failed_log_fails_closed() {
    let now = Instant::now();
    let failing: AuditSink = Arc::new(|_: &AuditRecord| Err(io::Error::other("disk")));
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"x\r", &client(1), now);
    assert_eq!(
        result(ledger.cut(request(5, "stale", 2, b"x\r"), Some(&failing), now)),
        unknown("input_log_unavailable")
    );
    assert_eq!(
        result(ledger.cut(request(1, "one", 2, b"x\r"), Some(&sink()), now)),
        unknown("input_log_unavailable")
    );
}
#[test]
fn input_consumer_ledger_poisoned_replay_logs_its_changed_answer() {
    // Review #188 P2: a replayed token on a poisoned epoch gets `unknown`, which differs from
    // its logged answer; the new answer must be durably logged before it is returned.
    let now = Instant::now();
    let records = Arc::new(std::sync::Mutex::new(Vec::<AuditRecord>::new()));
    let seen = Arc::clone(&records);
    let audit: AuditSink = Arc::new(move |r| {
        seen.lock().unwrap().push(r.clone());
        Ok(())
    });
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"x\r", &client(1), now);
    let req = request(1, "one", 2, b"x\r");
    let first = result(ledger.cut(req.clone(), Some(&audit), now));
    assert_ne!(first, unknown("termios_changed"));
    // Counterpart: an exact retry before poison keeps the stored answer and logs nothing new.
    assert_eq!(result(ledger.cut(req.clone(), Some(&audit), now)), first);
    assert_eq!(records.lock().unwrap().len(), 1);
    ledger.poison("termios_changed");
    assert_eq!(
        result(ledger.cut(req.clone(), Some(&audit), now)),
        unknown("termios_changed")
    );
    {
        let logged = records.lock().unwrap();
        assert_eq!(logged.len(), 2, "changed replay answer is logged");
        assert_eq!(logged[1].token, "one");
        assert_eq!(logged[1].result, unknown("termios_changed"));
    }
    // A failed log fails closed even on replay.
    let failing: AuditSink = Arc::new(|_: &AuditRecord| Err(io::Error::other("disk")));
    assert_eq!(
        result(ledger.cut(req, Some(&failing), now)),
        unknown("input_log_unavailable")
    );
}
#[test]
fn input_consumer_ledger_audit_failure_and_missing_sink_poison_no_attribution() {
    for audit in [
        None,
        Some(Arc::new(|_: &AuditRecord| Err(io::Error::other("disk failure"))) as AuditSink),
    ] {
        let now = Instant::now();
        let mut ledger = Ledger::new("epoch".into(), "secret".into());
        ledger.record(b"x", &client(1), now);
        assert_eq!(
            result(ledger.cut(request(1, "x", 1, b"x"), audit.as_ref(), now)),
            unknown("input_log_unavailable")
        );
        assert_eq!(
            result(ledger.cut(request(1, "x", 1, b"x"), Some(&sink()), now)),
            unknown("input_log_unavailable")
        );
        assert_eq!(
            result(ledger.cut(request(2, "y", 1, b""), Some(&sink()), now)),
            unknown("input_log_unavailable")
        );
    }
}
#[test]
fn input_consumer_ledger_expiry_uses_endpoint_successful_write_not_latest_input() {
    let start = Instant::now();
    let audit = sink();
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"A\r", &client(1), start);
    ledger.record(b"B", &InputSource::Api, start + Duration::from_secs(29));
    assert_eq!(
        result(ledger.cut(
            request(1, "a", 2, b"A\r"),
            Some(&audit),
            start + Duration::from_secs(31)
        )),
        unknown("cut_expired")
    );
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"A\r", &client(1), start);
    assert!(matches!(
        result(ledger.cut(
            request(1, "a", 2, b"A\r"),
            Some(&audit),
            start + Duration::from_secs(30)
        )),
        CutResult::Client { .. }
    ));
}
#[test]
fn input_consumer_ledger_desync_and_old_cut_fail_closed() {
    let now = Instant::now();
    let audit = sink();
    for (req, reason) in [
        (request(1, "t", 3, b"ab"), "cut_beyond_written"),
        (request(1, "t", 2, b"bad"), "digest_mismatch"),
    ] {
        let mut ledger = Ledger::new("epoch".into(), "secret".into());
        ledger.record(b"ab", &client(1), now);
        assert_eq!(result(ledger.cut(req, Some(&audit), now)), unknown(reason));
        assert_eq!(
            result(ledger.cut(request(2, "next", 2, b"ab"), Some(&audit), now)),
            unknown(reason)
        );
    }
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"ab", &client(1), now);
    let mut wrong = request(1, "t", 2, b"ab");
    wrong.epoch_key = "wrong".into();
    assert!(matches!(
        ledger.cut(wrong, Some(&audit), now),
        ConsumerResponse::Refused { .. }
    ));
    assert!(matches!(
        result(ledger.cut(request(1, "t", 2, b"ab"), Some(&audit), now)),
        CutResult::Client { .. }
    ));
    assert_eq!(
        result(ledger.cut(request(2, "old", 1, b""), Some(&audit), now)),
        unknown("cut_beyond_written")
    );
    assert_eq!(
        result(ledger.cut(request(1, "t", 2, b"ab"), Some(&audit), now)),
        unknown("cut_beyond_written")
    );
}
#[test]
fn input_consumer_ledger_raw_and_replay_budget_no_eviction_and_retention() {
    let now = Instant::now();
    let audit = sink();
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(&vec![b'x'; BUDGET + 1], &client(1), now);
    assert_eq!(
        result(ledger.cut(request(1, "t", 1, b"x"), Some(&audit), now)),
        unknown("receipt_overflow")
    );
    assert!(ledger.spans.is_empty());
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.record(b"x", &client(1), now);
    ledger.expire(now + RETENTION + Duration::from_nanos(1));
    assert_eq!(ledger.poison.as_deref(), Some("receipt_expired"));
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    let mut seq = 1;
    loop {
        ledger.record(b"x", &client(1), now);
        let answer = result(ledger.cut(
            request(seq, &format!("t{seq}"), seq, b"x"),
            Some(&audit),
            now,
        ));
        if matches!(answer, CutResult::Unknown { .. }) {
            break;
        }
        seq += 1;
        assert!(seq < 3000);
    }
    assert!(ledger.replay_cost <= BUDGET);
    assert!(!ledger.replays.is_empty());
    assert!(matches!(
        ledger.poison.as_deref(),
        Some("replay_overflow" | "receipt_overflow")
    ));
    assert_eq!(
        ledger.replays.first().expect("no eviction").request.token,
        "t1"
    );
    assert!(matches!(
        result(ledger.cut(request(1, "t1", 1, b"x"), Some(&audit), now)),
        CutResult::Unknown { .. }
    ));
    // Replay cap has its own reason when a pending new token would exceed it.
    let mut ledger = Ledger::new("epoch".into(), "secret".into());
    ledger.replay_cost = BUDGET - 100;
    assert_eq!(
        result(ledger.cut(request(1, "t", 0, b""), Some(&audit), now)),
        unknown("replay_overflow")
    );
}
