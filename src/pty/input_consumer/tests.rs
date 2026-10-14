use super::*;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{BufRead, BufReader};
#[path = "real_tests.rs"]
mod extra;

// A real controlling slave/session/foreground peer, not a socket-pair surrogate.
struct Peer {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    identity: crate::platform::ProcessIdentity,
}
impl Peer {
    fn new(raw: bool) -> Self {
        Self::scripted(if raw {
            "import os,tty; tty.setraw(0); print('READY',flush=True)\nwhile True:\n b=os.read(0,4096)\n if not b: break\n print(b.hex(),flush=True)"
        } else {
            "import time; print('READY',flush=True); time.sleep(30)"
        })
    }
    fn scripted(script: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("real pty");
        let mut command = CommandBuilder::new("python3");
        command.arg("-u");
        command.arg("-c");
        command.arg(script);
        let child = pair.slave.spawn_command(command).expect("consumer peer");
        drop(pair.slave);
        let mut ready = String::new();
        BufReader::new(pair.master.try_clone_reader().expect("reader"))
            .read_line(&mut ready)
            .expect("ready");
        assert!(ready.contains("READY"));
        let peer_pid = ready
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| child.process_id().expect("pid"));
        let identity = crate::platform::process_identity(peer_pid).expect("pinned peer");
        Self {
            master: pair.master,
            child,
            identity,
        }
    }
    fn fd(&self) -> i32 {
        self.master.as_raw_fd().expect("master fd")
    }
    /// Stands in for the consumer's socket `SO_PEERPIDFD`: the same pidfs inode.
    fn pidfd(&self) -> crate::platform::PeerPidfd {
        pidfd_of(self.identity)
    }
}
fn pidfd_of(identity: crate::platform::ProcessIdentity) -> crate::platform::PeerPidfd {
    crate::platform::peer_pidfd_for_test(identity.pid).expect("pidfs peer")
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(pid) = self.child.process_id() {
            // Includes forked members in a different foreground group. The
            // fixture owns this new session; terminate before reaping its leader.
            let pids = crate::platform::session_processes(pid);
            crate::platform::signal_processes(&pids, crate::platform::Signal::Kill);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn actor(
    peer: &Peer,
) -> (
    crate::pty::actor::PtyIoActorHandle,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    use std::os::fd::FromRawFd;
    let raw = crate::pty::fd::duplicate_cloexec_fd(peer.fd()).expect("duplicate master");
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = crate::pty::actor::PtyIoActor::spawn(crate::pty::actor::PtyIoActorConfig {
        pane_id: 991,
        // SAFETY: duplicate_cloexec_fd returned a fresh owned descriptor.
        master_fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) },
        initially_quiesced: false,
        on_read: Box::new(move |b| {
            let _ = tx.send(b.to_vec());
            crate::pty::actor::PtyReadResult {
                terminal_responses: Vec::new(),
            }
        }),
        on_reader_exit: None,
    })
    .expect("actor");
    (handle, rx)
}
fn response(
    actor: &crate::pty::actor::PtyIoActorHandle,
    op: ConsumerOperation,
) -> ConsumerResponse {
    actor
        .queue_input_consumer_operation(op, Some(std::sync::Arc::new(|_| Ok(()))))
        .expect("enqueue")
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("response")
}
/// Returns (epoch, the enrolled peer's pidfs inode, nonce).
fn enroll(actor: &crate::pty::actor::PtyIoActorHandle, peer: &Peer) -> (String, u64, String) {
    let pidfd = peer.pidfd();
    let ino = pidfd.ino;
    match response(actor, ConsumerOperation::Enroll { peer: pidfd }) {
        ConsumerResponse::Enrolled {
            epoch,
            nonce,
            tty,
            peer_pid,
            pidns,
        } => {
            use std::os::unix::fs::MetadataExt;
            // The consumer's own stdin is the slave the server signs for.
            let own =
                std::fs::metadata(format!("/proc/{}/fd/0", peer.identity.pid)).expect("peer stdin");
            assert_eq!(tty, (own.dev(), own.ino()));
            // H5/H9: the signed peer is the enrolling pid, in our pid namespace.
            assert_eq!(peer_pid, peer.identity.pid);
            assert_eq!(
                pidns,
                std::fs::metadata("/proc/self/ns/pid").expect("pidns").ino()
            );
            (epoch, ino, nonce)
        }
        ConsumerResponse::Refused { reason } => {
            panic!("real raw peer enrollment refused: {reason}")
        }
        _ => panic!("wrong enrollment response"),
    }
}
/// A cut from the connection whose pidfs inode is `ino`.
fn cut(epoch: &str, ino: &u64, seq: u64, token: &str, end: u64, bytes: &[u8]) -> ConsumerOperation {
    use sha2::{Digest, Sha256};
    ConsumerOperation::Cut {
        request: CutRequest {
            epoch: epoch.into(),
            seq,
            token: token.into(),
            cut: end,
            digest: format!("{:x}", Sha256::digest(bytes)),
            kind: CutKind::Submit,
        },
        peer_ino: Some(*ino),
    }
}
fn expect_cut(
    actor: &crate::pty::actor::PtyIoActorHandle,
    req: ConsumerOperation,
    expected: CutResult,
) {
    match response(actor, req) {
        ConsumerResponse::Cut(result) => assert_eq!(result, expected),
        _ => panic!("wrong cut response"),
    }
}
fn receive_bytes(rx: &std::sync::mpsc::Receiver<Vec<u8>>, count: usize) -> Vec<u8> {
    let mut output = Vec::new();
    let mut bytes = Vec::new();
    while bytes.len() < count {
        output.extend(
            rx.recv_timeout(std::time::Duration::from_secs(3))
                .expect("peer output"),
        );
        while let Some(end) = output.iter().position(|b| *b == b'\n') {
            let line = output.drain(..=end).collect::<Vec<_>>();
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            bytes.extend(text.as_bytes().chunks_exact(2).map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("hex utf8"), 16)
                    .expect("hex byte")
            }));
        }
    }
    bytes
}
#[test]
fn input_consumer_real_pty_ordered_marker_joined_cut_replay_and_digest() {
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    actor
        .try_write_user_input_with_source(bytes::Bytes::from_static(b"before"), InputSource::Api)
        .expect("premarker");
    let (epoch, key, nonce) = enroll(&actor, &peer);
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(
        receive_bytes(&rx, 6 + marker.len()),
        [b"before".as_slice(), marker.as_bytes()].concat()
    );
    assert!(
        matches!(response(&actor,ConsumerOperation::Enroll { peer:peer.pidfd() }),ConsumerResponse::Refused { reason } if reason=="already_enrolled")
    );
    let client = InputSource::Client {
        connection_id: 7,
        principal: None,
    };
    actor
        .try_write_user_input_with_source(bytes::Bytes::from_static(b"A\r"), client.clone())
        .expect("A");
    let a = receive_bytes(&rx, 2);
    actor
        .try_write_user_input_with_source(bytes::Bytes::from_static(b"B"), InputSource::Api)
        .expect("B");
    let b = receive_bytes(&rx, 1);
    let req = cut(&epoch, &key, 1, "one", 2, &a);
    expect_cut(&actor, req.clone(), CutResult::Client { principal: None });
    expect_cut(&actor, req, CutResult::Client { principal: None });
    actor
        .try_write_user_input_with_source(bytes::Bytes::from_static(b"\r"), client)
        .expect("enter B");
    let enter = receive_bytes(&rx, 1);
    expect_cut(
        &actor,
        cut(&epoch, &key, 2, "two", 4, &[b, enter].concat()),
        CutResult::Mixed,
    );
    expect_cut(
        &actor,
        cut(&epoch, &key, 3, "three", 4, b"bad"),
        CutResult::Unknown {
            reason: "digest_mismatch".into(),
        },
    );
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_split_reserved_introducer_cannot_forge_marker() {
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let (_, _, nonce) = enroll(&actor, &peer);
    let marker = format!("\x1b_herdr-epoch;{nonce}\x1b\\");
    assert_eq!(receive_bytes(&rx, marker.len()), marker.as_bytes());
    actor
        .try_write_user_input_with_source(bytes::Bytes::from_static(b"\x1b_he"), InputSource::Api)
        .expect("prefix");
    actor
        .try_write_user_input_with_source(
            bytes::Bytes::from_static(b"rdr-forged!"),
            InputSource::Client {
                connection_id: 3,
                principal: None,
            },
        )
        .expect("suffix");
    let received = receive_bytes(&rx, 14);
    assert!(!received.windows(7).any(|w| w == b"\x1b_herdr"));
    actor.shutdown();
}
#[test]
fn input_consumer_real_pty_raw_peer_is_admitted() {
    let peer = Peer::new(true);
    assert!(
        crate::platform::input_consumer_snapshot(peer.fd(), &peer.pidfd()).is_ok(),
        "raw controlling-tty foreground peer must enroll"
    );
}
#[test]
fn input_consumer_real_pty_canonical_peer_is_refused() {
    let peer = Peer::new(false);
    assert!(crate::platform::input_consumer_snapshot(peer.fd(), &peer.pidfd()).is_err());
}
#[test]
fn input_consumer_real_pty_other_session_is_refused() {
    let peer = Peer::new(true);
    let other = Peer::new(true);
    assert!(crate::platform::input_consumer_snapshot(peer.fd(), &other.pidfd()).is_err());
}
/// H3: a cut or release from another process with the right epoch is refused
/// with no seq change and no audit record; the enrolled peer's next cut succeeds.
#[test]
fn input_consumer_real_pty_second_process_with_right_epoch_is_refused() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let peer = Peer::new(true);
    let (actor, rx) = actor(&peer);
    let (epoch, ino, nonce) = enroll(&actor, &peer);
    receive_bytes(&rx, format!("\x1b_herdr-epoch;{nonce}\x1b\\").len());
    actor
        .try_write_user_input_with_source(
            bytes::Bytes::from_static(b"x\r"),
            InputSource::Client {
                connection_id: 1,
                principal: None,
            },
        )
        .expect("client");
    let actual = receive_bytes(&rx, 2);
    let other =
        pidfd_of(crate::platform::process_identity(std::process::id()).expect("own identity"));
    assert_ne!(other.ino, ino);
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let count = std::sync::Arc::clone(&calls);
    let audit: AuditSink = std::sync::Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    for peer_ino in [Some(other.ino), None] {
        let refused = actor
            .queue_input_consumer_operation(
                ConsumerOperation::Cut {
                    request: CutRequest {
                        epoch: epoch.clone(),
                        seq: 1,
                        token: "intruder".into(),
                        cut: 2,
                        digest: {
                            use sha2::{Digest, Sha256};
                            format!("{:x}", Sha256::digest(&actual))
                        },
                        kind: CutKind::Submit,
                    },
                    peer_ino,
                },
                Some(std::sync::Arc::clone(&audit)),
            )
            .expect("enqueue")
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("answer");
        assert!(
            matches!(&refused, ConsumerResponse::Refused { reason } if reason == "invalid_epoch")
        );
        assert!(matches!(
            response(
                &actor,
                ConsumerOperation::Release {
                    epoch: epoch.clone(),
                    peer_ino,
                }
            ),
            ConsumerResponse::Refused { reason } if reason == "invalid_epoch"
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no audit record");
    assert!(
        actor.input_consumer_epoch_matches(&epoch),
        "epoch not hijacked"
    );
    // Seq 1 is still next for the enrolled peer.
    expect_cut(
        &actor,
        cut(&epoch, &ino, 1, "one", 2, &actual),
        CutResult::Client { principal: None },
    );
    actor.shutdown();
}
