use super::*;
use bytes::Bytes;
use std::time::{Duration, Instant};
#[test]
fn input_consumer_real_pty_nonleader_first_and_second_same_group_refused() {
    let peer=Peer::scripted("import os,tty,time\np=os.fork()\nif p: time.sleep(30)\nelse:\n tty.setraw(0); print('READY',os.getpid(),flush=True)\n while True:\n  b=os.read(0,4096)\n  if not b: break\n  print(b.hex(),flush=True)");
    assert_ne!(peer.identity.pid, peer.child.process_id().expect("leader"));
    let (actor, rx) = actor(&peer);
    let (epoch, key, nonce) = enroll(&actor, &peer);
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(receive_bytes(&rx, marker.len()), marker.as_bytes());
    let leader = crate::platform::process_identity(peer.child.process_id().expect("leader"))
        .expect("leader identity");
    assert!(
        matches!(response(&actor,ConsumerOperation::Enroll { peer:leader }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
    );
    // Ruling r3 B: after release the group stays owned by its first consumer.
    assert!(matches!(
        response(
            &actor,
            ConsumerOperation::Release {
                epoch: epoch.clone(),
                epoch_key: key
            }
        ),
        ConsumerResponse::Released
    ));
    assert!(
        matches!(response(&actor,ConsumerOperation::Enroll { peer:leader }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
    );
    let (fresh, _, nonce) = enroll(&actor, &peer);
    assert_ne!(fresh, epoch);
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(receive_bytes(&rx, marker.len()), marker.as_bytes());
    assert!(
        matches!(response(&actor,ConsumerOperation::Enroll { peer:leader }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
    );
    actor.shutdown();
}
fn termios(fd: i32) -> libc::termios {
    let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: real owned test master; successful tcgetattr initializes t.
    assert_eq!(unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) }, 0);
    unsafe { t.assume_init() }
}
#[test]
fn input_consumer_real_pty_all_raw_flags_semantic_change_and_retry_poison() {
    for change in 0..8 {
        let peer = Peer::new(true);
        let (actor, rx) = actor(&peer);
        let (epoch, key, nonce) = enroll(&actor, &peer);
        let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
        receive_bytes(&rx, marker.len());
        actor
            .try_write_user_input_with_source(
                Bytes::from_static(b"x\r"),
                InputSource::Client {
                    connection_id: 1,
                    principal: None,
                },
            )
            .expect("write");
        let actual = receive_bytes(&rx, 2);
        let req = cut(&epoch, &key, 1, "one", 2, &actual);
        expect_cut(&actor, req.clone(), CutResult::Client { principal: None });
        let original = termios(peer.fd());
        let mut changed = original;
        match change {
            0 => changed.c_lflag |= libc::ICANON,
            1 => changed.c_lflag |= libc::ECHO,
            2 => changed.c_iflag |= libc::ICRNL,
            3 => changed.c_iflag |= libc::ISTRIP,
            4 => changed.c_iflag |= libc::IXON,
            5 => changed.c_lflag |= libc::IEXTEN,
            6 => changed.c_oflag ^= libc::OPOST,
            _ => changed.c_cc[libc::VMIN] = 2,
        }
        // SAFETY: master owns a live test tty and changed is initialized.
        assert_eq!(
            unsafe { libc::tcsetattr(peer.fd(), libc::TCSANOW, &changed) },
            0
        );
        assert!(
            matches!(response(&actor,ConsumerOperation::Enroll { peer:peer.identity }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
        );
        expect_cut(
            &actor,
            req.clone(),
            CutResult::Unknown {
                reason: "termios_changed".into(),
            },
        );
        assert_eq!(
            unsafe { libc::tcsetattr(peer.fd(), libc::TCSANOW, &original) },
            0
        );
        expect_cut(
            &actor,
            req,
            CutResult::Unknown {
                reason: "termios_changed".into(),
            },
        );
        let (fresh, fresh_key, nonce) = enroll(&actor, &peer);
        assert_ne!(fresh, epoch);
        receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
        actor
            .try_write_user_input_with_source(
                Bytes::from_static(b"y\r"),
                InputSource::Client {
                    connection_id: 1,
                    principal: None,
                },
            )
            .expect("write");
        let actual = receive_bytes(&rx, 2);
        expect_cut(
            &actor,
            cut(&fresh, &fresh_key, 1, "one", 2, &actual),
            CutResult::Client { principal: None },
        );
        actor.shutdown();
    }
}
#[test]
fn input_consumer_real_pty_release_death_and_idle_epoch_hint_end() {
    for dies in [false, true] {
        let mut peer = Peer::new(true);
        let (actor, rx) = actor(&peer);
        let (epoch, key, nonce) = enroll(&actor, &peer);
        receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
        assert!(actor.input_consumer_epoch_matches(&epoch));
        actor
            .try_write_user_input_with_source(
                Bytes::from_static(b"x"),
                InputSource::Client {
                    connection_id: 1,
                    principal: None,
                },
            )
            .expect("write");
        let actual = receive_bytes(&rx, 1);
        let req = cut(&epoch, &key, 1, "one", 1, &actual);
        expect_cut(&actor, req.clone(), CutResult::Client { principal: None });
        if dies {
            peer.child.kill().expect("kill consumer");
            peer.child.wait().expect("reap consumer");
            let deadline = Instant::now() + Duration::from_secs(2);
            while actor.input_consumer_epoch_matches(&epoch) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        } else {
            assert!(matches!(
                response(
                    &actor,
                    ConsumerOperation::Release {
                        epoch: epoch.clone(),
                        epoch_key: key.clone()
                    }
                ),
                ConsumerResponse::Released
            ));
            let (fresh, _, nonce) = enroll(&actor, &peer);
            assert_ne!(fresh, epoch);
            receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
            assert!(
                matches!(response(&actor,ConsumerOperation::Enroll { peer:peer.identity }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
            );
        }
        assert!(!actor.input_consumer_epoch_matches(&epoch));
        match actor.queue_input_consumer_operation(ConsumerOperation::Cut(req), None) {
            Ok(rx) => assert!(matches!(
                rx.recv_timeout(Duration::from_secs(3)),
                Ok(ConsumerResponse::Refused { .. }) | Err(_)
            )),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
        }
        actor.shutdown();
    }
}
#[test]
fn input_consumer_real_pty_enroll_waits_for_active_delayed_submission() {
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let completed = actor
        .queue_user_input_submission(
            Bytes::from_static(b"pre"),
            Bytes::from_static(b"\r"),
            Duration::from_millis(80),
        )
        .expect("submission");
    let enrollment = actor
        .queue_input_consumer_operation(
            ConsumerOperation::Enroll {
                peer: peer.identity,
            },
            None,
        )
        .expect("enroll");
    assert!(enrollment.recv_timeout(Duration::from_millis(20)).is_err());
    completed
        .recv_timeout(Duration::from_secs(3))
        .expect("completion")
        .expect("submitted");
    let (epoch, key, nonce) = match enrollment
        .recv_timeout(Duration::from_secs(3))
        .expect("enrollment")
    {
        ConsumerResponse::Enrolled {
            epoch,
            epoch_key,
            nonce,
        } => (epoch, epoch_key, nonce),
        _ => panic!("not enrolled"),
    };
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(
        receive_bytes(&rx, 4 + marker.len()),
        [b"pre\r".as_slice(), marker.as_bytes()].concat()
    );
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"C\r"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("C");
    let actual = receive_bytes(&rx, 2);
    expect_cut(
        &actor,
        cut(&epoch, &key, 1, "c", 2, &actual),
        CutResult::Client { principal: None },
    );
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_large_actual_writes_split_interval_and_neutral_downgrade() {
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let prem = vec![b'p'; 256 * 1024];
    actor
        .try_write_user_input(Bytes::from(prem.clone()))
        .expect("large premarker");
    let (epoch, key, nonce) = enroll(&actor, &peer);
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(
        receive_bytes(&rx, prem.len() + marker.len()),
        [prem, marker.into_bytes()].concat()
    );
    let payload = vec![b'x'; 256 * 1024];
    actor
        .try_write_user_input_with_source(
            Bytes::from(payload.clone()),
            InputSource::Client {
                connection_id: 9,
                principal: None,
            },
        )
        .expect("large client write");
    let actual = receive_bytes(&rx, payload.len());
    assert_eq!(actual, payload);
    let half = 13017;
    expect_cut(
        &actor,
        cut(&epoch, &key, 1, "partial", half as u64, &actual[..half]),
        CutResult::Client { principal: None },
    );
    expect_cut(
        &actor,
        cut(
            &epoch,
            &key,
            2,
            "rest",
            actual.len() as u64,
            &actual[half..],
        ),
        CutResult::Client { principal: None },
    );
    actor
        .try_write_user_input_with_source(Bytes::from_static(b"\x1b[0n"), InputSource::Neutral)
        .expect("forged neutral");
    let reply = receive_bytes(&rx, 4);
    expect_cut(
        &actor,
        cut(
            &epoch,
            &key,
            3,
            "unknown",
            (actual.len() + 4) as u64,
            &reply,
        ),
        CutResult::Unknown {
            reason: "unknown_input".into(),
        },
    );
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_audit_blocks_reply_failure_poison_and_retry_no_repeat() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    };
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let (epoch, key, nonce) = enroll(&actor, &peer);
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"x\r"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("client");
    let actual = receive_bytes(&rx, 2);
    let req = cut(&epoch, &key, 1, "one", 2, &actual);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let gate = Mutex::new(continue_rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let audit: AuditSink = Arc::new(move |record| {
        count.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(record.result, CutResult::Client { .. }));
        entered_tx.send(()).expect("audit entered");
        gate.lock()
            .expect("gate")
            .recv_timeout(Duration::from_secs(3))
            .expect("release durability result");
        Err(std::io::Error::other("append/fsync failed"))
    });
    let receipt = actor
        .queue_input_consumer_operation(ConsumerOperation::Cut(req.clone()), Some(audit))
        .expect("cut");
    entered_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("audit invoked");
    assert!(
        receipt.recv_timeout(Duration::from_millis(20)).is_err(),
        "no attribution before durable audit"
    );
    continue_tx.send(()).expect("unblock audit");
    assert!(
        matches!(receipt.recv_timeout(Duration::from_secs(3)).expect("failed audit result"),ConsumerResponse::Cut(CutResult::Unknown { reason }) if reason=="input_log_unavailable")
    );
    expect_cut(
        &actor,
        req,
        CutResult::Unknown {
            reason: "input_log_unavailable".into(),
        },
    );
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"y\r"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("next client");
    let next = receive_bytes(&rx, 2);
    expect_cut(
        &actor,
        cut(&epoch, &key, 2, "two", 4, &next),
        CutResult::Unknown {
            reason: "input_log_unavailable".into(),
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_actor_reply_classifier_and_sanitizer_across_reply_origins() {
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let (epoch, key, nonce) = enroll(&actor, &peer);
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor.write_terminal_response(|| Some(Bytes::from_static(b"\x1b[0n\x1b[?1;2c")));
    let neutral = receive_bytes(&rx, 11);
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"x\r"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("client");
    let client = receive_bytes(&rx, 2);
    let first = [neutral, client].concat();
    expect_cut(
        &actor,
        cut(&epoch, &key, 1, "allowed", first.len() as u64, &first),
        CutResult::Client { principal: None },
    );
    actor.write_terminal_response(|| {
        Some(Bytes::from_static(
            b"\x1b[0n\x1bP1$rattacker\x1b\\\x1b[1;2R",
        ))
    });
    let unknown = receive_bytes(&rx, 25);
    expect_cut(
        &actor,
        cut(
            &epoch,
            &key,
            2,
            "echo",
            (first.len() + unknown.len()) as u64,
            &unknown,
        ),
        CutResult::Unknown {
            reason: "unknown_input".into(),
        },
    );
    actor
        .try_write_user_input_with_source(Bytes::from_static(b"\x1b_he"), InputSource::Api)
        .expect("introducer");
    actor.write_terminal_response(|| Some(Bytes::from_static(b"rdr")));
    let sanitized = receive_bytes(&rx, 7);
    assert_eq!(sanitized, b"\x1b_herdR");
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_foreground_handover_revokes_cached_result() {
    let peer=Peer::scripted("import os,tty,time,signal\ntty.setraw(0); print('READY',flush=True)\nwhile True:\n b=os.read(0,4096)\n print(b.hex(),flush=True)\n if b==b'H':\n  p=os.fork()\n  if not p:\n   os.setpgid(0,0); time.sleep(30)\n  else:\n   time.sleep(.05); signal.signal(signal.SIGTTOU,signal.SIG_IGN); os.tcsetpgrp(0,p); print('CHANGED',flush=True); time.sleep(30)");
    let (actor, rx) = actor(&peer);
    let (epoch, key, nonce) = enroll(&actor, &peer);
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"x"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("client");
    let actual = receive_bytes(&rx, 1);
    let req = cut(&epoch, &key, 1, "one", 1, &actual);
    expect_cut(&actor, req.clone(), CutResult::Client { principal: None });
    actor
        .try_write_user_input(Bytes::from_static(b"H"))
        .expect("handover");
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !output.windows(7).any(|b| b == b"CHANGED") {
        output.extend(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("handover signal"),
        );
    }
    assert!(matches!(
        response(&actor, ConsumerOperation::Cut(req)),
        ConsumerResponse::Refused { .. }
    ));
    assert!(!actor.input_consumer_epoch_matches(&epoch));
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_foreground_return_original_reenrolls_fresh_epoch() {
    let peer=Peer::scripted("import os,tty,time,signal\ntty.setraw(0); print('READY',flush=True)\nwhile True:\n b=os.read(0,4096)\n print(b.hex(),flush=True)\n if b==b'Z':\n  p=os.fork()\n  if not p:\n   os.setpgid(0,0); time.sleep(30)\n  else:\n   time.sleep(.05); signal.signal(signal.SIGTTOU,signal.SIG_IGN); os.tcsetpgrp(0,p); print('AWAY',flush=True); time.sleep(.5); os.tcsetpgrp(0,os.getpgrp()); print('BACK',flush=True)");
    let (actor, rx) = actor(&peer);
    let (epoch, key, nonce) = enroll(&actor, &peer);
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor
        .try_write_user_input(Bytes::from_static(b"Z"))
        .expect("suspend");
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !output.windows(4).any(|b| b == b"BACK") {
        output.extend(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("resume signal"),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while actor.input_consumer_epoch_matches(&epoch) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !actor.input_consumer_epoch_matches(&epoch),
        "foreground change ends the epoch"
    );
    let (fresh, fresh_key, nonce) = enroll(&actor, &peer);
    assert_ne!(fresh, epoch);
    assert!(matches!(
        response(
            &actor,
            ConsumerOperation::Cut(cut(&epoch, &key, 1, "old", 0, b""))
        ),
        ConsumerResponse::Refused { .. }
    ));
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor
        .try_write_user_input_with_source(
            Bytes::from_static(b"q\r"),
            InputSource::Client {
                connection_id: 3,
                principal: None,
            },
        )
        .expect("write");
    let actual = receive_bytes(&rx, 2);
    expect_cut(
        &actor,
        cut(&fresh, &fresh_key, 1, "one", 2, &actual),
        CutResult::Client { principal: None },
    );
    actor.shutdown();
}
