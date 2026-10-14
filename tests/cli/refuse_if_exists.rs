//! smarty-code#1707: a floor-claim file that appears after the sender's final
//! check but before Herdr writes the key. Real server, real CLI, real PTY. The
//! PTY writer is held by a full PTY input queue (the pane reads nothing until
//! released), not by a test hook, so the guarded key waits in the actor's FIFO.
use super::harness::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

/// Far larger than a Linux PTY input buffer, yet under the 1 MiB request
/// limit: the actor's write stays pending.
const FILLER_BYTES: usize = 256 * 1024;
const LONG_GUARDED_TEXT_BYTES: usize = 96 * 1024;

struct HeldPane {
    base: PathBuf,
    socket: PathBuf,
    pane_id: String,
    hold: PathBuf,
    resume: PathBuf,
    server: Option<std::process::Child>,
}

impl HeldPane {
    fn start() -> Self {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let hold = base.join("hold");
        fs::create_dir_all(&hold).unwrap();
        let release = hold.join("release");
        let resume = hold.join("resume");
        for fifo_path in [&release, &resume] {
            let fifo = std::ffi::CString::new(fifo_path.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        }
        // After the initial release, this reader records input exactly as the
        // old `cat` did. Controlled cases can stop it after an exact byte count,
        // use TIOCINQ as evidence that bytes are unread in the PTY, then resume.
        let reader = base.join("held-reader.py");
        fs::write(
            &reader,
            r#"import fcntl, json, os, pathlib, select, struct, sys, termios, time
hold = pathlib.Path(sys.argv[1])
filler = int(sys.argv[2])
captured = open(hold / "captured", "wb", buffering=0)

def fail(message):
    (hold / "reader-error").write_text(message)
    raise SystemExit(2)

def copy_exact(count):
    deadline = time.monotonic() + 30
    remaining = count
    while remaining:
        timeout = deadline - time.monotonic()
        if timeout <= 0:
            fail("timed out waiting for {} more PTY bytes".format(remaining))
        ready, _, _ = select.select([0], [], [], min(timeout, 1.0))
        if not ready:
            continue
        chunk = os.read(0, min(remaining, 65536))
        if not chunk:
            fail("PTY closed with {} bytes still expected".format(remaining))
        captured.write(chunk)
        remaining -= len(chunk)

copy_exact(filler)
config_path = hold / "reader-config.json"
if config_path.exists():
    config = json.loads(config_path.read_text())
    copy_exact(int(config["prefix_bytes"]))
    minimum = int(config["min_pending_bytes"])
    deadline = time.monotonic() + 30
    while True:
        pending = struct.unpack("i", fcntl.ioctl(0, termios.FIONREAD, struct.pack("i", 0)))[0]
        if pending >= minimum:
            break
        if time.monotonic() >= deadline:
            fail("timed out waiting for {} unread PTY bytes; observed {}".format(minimum, pending))
        time.sleep(0.002)
    (hold / "reader-observed").write_text(str(pending))
    with open(hold / "resume", "rb", buffering=0) as gate:
        if not gate.read(1):
            fail("reader resume FIFO closed without a byte")
    copy_exact(int(config["after_resume_bytes"]))
    (hold / "reader-done").write_text("done")

while True:
    chunk = os.read(0, 65536)
    if not chunk:
        break
    captured.write(chunk)
"#,
        )
        .unwrap();
        // The pane reads no input until the test writes the release FIFO, then
        // records every byte it receives, unmodified.
        let shell = base.join("held-shell");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nstty raw -echo\n: > '{hold}/ready'\nread -r _ < '{hold}/release'\nexec /usr/bin/python3 -u '{reader}' '{hold}' {filler}\n",
                hold = hold.display(),
                reader = reader.display(),
                filler = FILLER_BYTES,
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        let config = base.join("config").join(app_dir_name());
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
                shell.to_str().unwrap()
            ),
        )
        .unwrap();
        register_runtime_dir(&base.join("runtime"));
        let socket = base.join("runtime/herdr.sock");
        let mut fixture = Self {
            base,
            socket,
            pane_id: String::new(),
            hold,
            resume,
            server: None,
        };
        let server = fixture
            .command()
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        register_spawned_herdr_pid(Some(server.id()));
        fixture.server = Some(server);
        wait_for_socket(&fixture.socket, Duration::from_secs(5));
        let created = parse_cli_json_output(
            &["workspace", "create"],
            fixture
                .command()
                .args([
                    "workspace",
                    "create",
                    "--cwd",
                    fixture.base.to_str().unwrap(),
                ])
                .output()
                .unwrap(),
        );
        fixture.pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
                fixture.hold.join("ready").exists()
            }),
            "held pane never started"
        );
        fixture
    }

    fn command(&self) -> Command {
        let mut command = crate::test_command::herdr_command();
        for (key, name) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let path = self.base.join(name);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            command.env(key, path);
        }
        command
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("SHELL", "/bin/sh");
        command
    }

    /// Queue more input than the PTY accepts. The pane reads nothing, so the
    /// actor's write blocks and every later write waits behind it.
    fn hold_writer(&self) {
        let response = send_request(
            &self.socket,
            &json!({"id": "filler", "method": "pane.send_text",
                "params": {"pane_id": self.pane_id, "text": "x".repeat(FILLER_BYTES)}})
            .to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
    }

    /// Wait until the server logged the request: it passed every request-time
    /// check and was queued, while the writer is still held.
    /// A sender that exits first was refused before queuing (for example by
    /// a CLI without the flag): report its output instead of timing out.
    fn wait_for_accepted(&self, method: &str, count: usize, sender: &mut std::process::Child) {
        let log = self
            .base
            .join("state")
            .join(app_dir_name())
            .join("api-input.jsonl");
        let accepted = wait_until(Duration::from_secs(10), Duration::from_millis(10), || {
            if sender.try_wait().unwrap().is_some() {
                return true;
            }
            fs::read_to_string(&log).is_ok_and(|raw| {
                raw.lines()
                    .filter(|line| line.contains(&format!("\"method\":\"{method}\"")))
                    .count()
                    >= count
            })
        });
        if let Some(status) = sender.try_wait().unwrap() {
            let mut stderr = String::new();
            std::io::Read::read_to_string(sender.stderr.as_mut().unwrap(), &mut stderr).unwrap();
            panic!("sender exited before its write was queued: {status}\nstderr: {stderr}");
        }
        assert!(accepted, "server never accepted {method}");
    }

    fn release_writer(&self) {
        let mut fifo = fs::OpenOptions::new()
            .write(true)
            .open(self.hold.join("release"))
            .unwrap();
        fifo.write_all(b"go\n").unwrap();
    }

    fn configure_controlled_reader(
        &self,
        prefix_bytes: usize,
        min_pending_bytes: usize,
        after_resume_bytes: usize,
    ) {
        fs::write(
            self.hold.join("reader-config.json"),
            json!({
                "prefix_bytes": prefix_bytes,
                "min_pending_bytes": min_pending_bytes,
                "after_resume_bytes": after_resume_bytes,
            })
            .to_string(),
        )
        .unwrap();
    }

    /// Number of unread bytes observed by TIOCINQ after the configured prefix
    /// has been consumed. This is PTY evidence, not a timing-based guess.
    fn observed_pending_bytes(&self) -> usize {
        let observed = self.hold.join("reader-observed");
        let error = self.hold.join("reader-error");
        let mut pending = None;
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(10), || {
                // The reader creates the file before writing its count. Wait
                // for the complete value, not just directory-entry visibility.
                pending = fs::read_to_string(&observed)
                    .ok()
                    .and_then(|raw| raw.parse::<usize>().ok());
                pending.is_some() || error.exists()
            }),
            "held PTY reader never observed its configured byte boundary"
        );
        assert!(
            !error.exists(),
            "held PTY reader failed: {}",
            fs::read_to_string(error).unwrap_or_default()
        );
        pending.expect("complete unread PTY byte count")
    }

    fn resume_reader(&self) {
        let mut fifo = fs::OpenOptions::new()
            .write(true)
            .open(&self.resume)
            .unwrap();
        fifo.write_all(b"go\n").unwrap();
    }

    /// Exact bytes captured after the filler by a controlled reader. Waiting
    /// for its completion marker ensures the whole expected span was observed.
    fn controlled_bytes_after_filler(&self, expected_len: usize) -> Vec<u8> {
        let done = self.hold.join("reader-done");
        let error = self.hold.join("reader-error");
        assert!(
            wait_until(Duration::from_secs(20), Duration::from_millis(10), || {
                done.exists() || error.exists()
            }),
            "held PTY reader did not finish its exact byte span"
        );
        assert!(
            !error.exists(),
            "held PTY reader failed: {}",
            fs::read_to_string(error).unwrap_or_default()
        );
        let bytes = fs::read(self.hold.join("captured")).unwrap();
        assert_eq!(
            bytes.len(),
            FILLER_BYTES + expected_len,
            "controlled reader captured unexpected byte count"
        );
        assert!(bytes[..FILLER_BYTES].iter().all(|byte| *byte == b'x'));
        bytes[FILLER_BYTES..].to_vec()
    }

    /// Bytes the pane received after the filler.
    fn received_after_filler(&self, expected_len: usize) -> Vec<u8> {
        let captured = self.hold.join("captured");
        assert!(
            wait_until(Duration::from_secs(20), Duration::from_millis(20), || {
                fs::metadata(&captured)
                    .is_ok_and(|meta| meta.len() as usize >= FILLER_BYTES + expected_len)
            }),
            "pane did not receive the expected bytes"
        );
        let bytes = fs::read(&captured).unwrap();
        assert!(bytes[..FILLER_BYTES].iter().all(|byte| *byte == b'x'));
        bytes[FILLER_BYTES..].to_vec()
    }
}

impl Drop for HeldPane {
    fn drop(&mut self) {
        cleanup_test_base(&self.base);
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
            unregister_spawned_herdr_pid(Some(server.id()));
        }
    }
}

fn error_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stderr).unwrap_or_else(|err| {
        panic!(
            "expected a JSON error ({err}); status {:?}\nstderr: {}\nstdout: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// RED (today, and still without the flag): the claim appears after the
/// sender's final check but before Herdr writes, and the Escape lands.
#[test]
fn unguarded_escape_lands_when_the_claim_appears_after_the_final_check() {
    let pane = HeldPane::start();
    let claim = pane.base.join("floor-claim");
    pane.hold_writer();
    assert!(!claim.exists(), "sender's final check: no claim yet");
    let sent = pane
        .command()
        .args(["pane", "send-keys", &pane.pane_id, "esc"])
        .output()
        .unwrap();
    // The unguarded send returns once the key is queued behind the held write.
    assert!(sent.status.success(), "{sent:?}");
    fs::write(&claim, "floor claimed").unwrap();
    pane.release_writer();
    assert_eq!(pane.received_after_filler(1), b"\x1b");
}

/// GREEN: with --refuse-if-exists, Herdr checks the path at write time,
/// refuses the key with guard_path_present and writes 0 bytes; a later
/// unguarded write (the page message) still arrives.
#[test]
fn refuse_if_exists_drops_the_key_when_the_claim_appears_before_the_write() {
    let pane = HeldPane::start();
    let claim = pane.base.join("floor-claim");
    pane.hold_writer();
    assert!(!claim.exists(), "sender's final check: no claim yet");
    let mut guarded = pane
        .command()
        .args([
            "pane",
            "send-keys",
            &pane.pane_id,
            "--refuse-if-exists",
            claim.to_str().unwrap(),
            "esc",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The guarded request is accepted and queued behind the held write; it is
    // not checked at request time, so the claim may appear only now.
    pane.wait_for_accepted("pane.send_keys", 1, &mut guarded);
    fs::write(&claim, "floor claimed").unwrap();
    pane.release_writer();
    let refused = guarded.wait_with_output().unwrap();
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let error = error_json(&refused);
    assert_eq!(error["error"]["code"], "guard_path_present", "{error}");
    assert_eq!(error["error"]["partial_text_consumed"], false, "{error}");

    let page = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, "PAGE"])
        .output()
        .unwrap();
    assert!(page.status.success(), "{page:?}");
    // Exactly the page message follows the filler: no byte of the Escape.
    assert_eq!(pane.received_after_filler(4), b"PAGE");
}

/// A claim present before the guarded request refuses before writing any of it.
/// The PTY reader proves BEFORE is already unread in the kernel when the guard
/// is reached; the exact capture proves that refusal did not flush unrelated
/// queued input. AFTER is queued after the refusal and must also survive.
#[test]
fn refuse_if_exists_before_write_preserves_unrelated_unread_pty_input() {
    const BEFORE: &str = "BEFORE:";
    const GUARDED: &str = "MUST_NOT_BE_WRITTEN";
    const AFTER: &str = ":AFTER";

    let pane = HeldPane::start();
    let claim = pane.base.join("floor-claim");
    pane.hold_writer();

    let before = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, BEFORE])
        .output()
        .unwrap();
    assert!(before.status.success(), "{before:?}");
    pane.configure_controlled_reader(0, BEFORE.len(), BEFORE.len() + AFTER.len());
    fs::write(&claim, "floor claimed").unwrap();

    let mut guarded = pane
        .command()
        .args([
            "pane",
            "send-text",
            &pane.pane_id,
            "--refuse-if-exists",
            claim.to_str().unwrap(),
            GUARDED,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pane.wait_for_accepted("pane.send_text", 3, &mut guarded);
    pane.release_writer();

    let unread = pane.observed_pending_bytes();
    assert_eq!(unread, BEFORE.len(), "only BEFORE is unread at refusal");
    let refused = guarded.wait_with_output().unwrap();
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let error = error_json(&refused);
    assert_eq!(error["error"]["code"], "guard_path_present", "{error}");
    assert_eq!(error["error"]["partial_text_consumed"], false, "{error}");
    assert_eq!(error["error"]["flush_failed"], false, "{error}");

    let after = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, AFTER])
        .output()
        .unwrap();
    assert!(after.status.success(), "{after:?}");
    pane.resume_reader();
    let received = pane.controlled_bytes_after_filler(BEFORE.len() + AFTER.len());
    let mut expected = BEFORE.as_bytes().to_vec();
    expected.extend_from_slice(AFTER.as_bytes());
    assert_eq!(received, expected);
    println!(
        "E2E refuse_if_exists pre-write: unread_before={unread}; partial_text_consumed=false; flush_failed=false; captured_after_filler={:?}",
        String::from_utf8_lossy(&received)
    );
}

/// The reader consumes the known filler and BEFORE prefix, then reports the
/// exact unread PTY queue length while the long guarded request is in flight.
/// The claim is created only after that evidence, so a pending sender plus
/// queued guarded bytes prove a mid-write race without a timing guess.
#[test]
fn refuse_if_exists_mid_write_delivers_full_text_delayed_enter_and_unrelated_input() {
    const BEFORE: &str = "BEFORE:";
    const AFTER: &str = ":AFTER";

    let pane = HeldPane::start();
    let claim = pane.base.join("floor-claim");
    pane.hold_writer();

    let before = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, BEFORE])
        .output()
        .unwrap();
    assert!(before.status.success(), "{before:?}");

    // Stay below Linux's per-argument exec limit, while remaining far larger
    // than a PTY input queue so the CLI cannot finish before the reader resumes.
    let guarded_text = "g".repeat(LONG_GUARDED_TEXT_BYTES);
    let mut guarded = pane
        .command()
        .args([
            "pane",
            "send-text",
            &pane.pane_id,
            "--refuse-if-exists",
            claim.to_str().unwrap(),
            &guarded_text,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pane.wait_for_accepted("pane.send_text", 3, &mut guarded);

    // These unrelated writes are queued behind the guarded text. The Enter is
    // intentionally delayed by the blocked PTY write, not folded into text.
    let enter = pane
        .command()
        .args(["pane", "send-keys", &pane.pane_id, "enter"])
        .output()
        .unwrap();
    assert!(enter.status.success(), "{enter:?}");
    let after = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, AFTER])
        .output()
        .unwrap();
    assert!(after.status.success(), "{after:?}");

    pane.configure_controlled_reader(BEFORE.len(), 1, guarded_text.len() + 1 + AFTER.len());
    pane.release_writer();
    let pending = pane.observed_pending_bytes();
    assert!(pending > 0, "guarded bytes reached the unread PTY queue");
    assert!(
        pending < guarded_text.len(),
        "only a prefix of the {}-byte request is pending in the PTY, observed {pending}",
        guarded_text.len()
    );
    assert!(
        guarded.try_wait().unwrap().is_none(),
        "guarded CLI must still be waiting on the partial PTY write"
    );

    fs::write(&claim, "floor claimed mid-write").unwrap();
    assert!(claim.exists());
    println!(
        "E2E refuse_if_exists mid-write: claim=present; unread_guarded_bytes={pending}/{}; sender_pending=true",
        guarded_text.len()
    );
    pane.resume_reader();

    let sent = guarded.wait_with_output().unwrap();
    assert!(sent.status.success(), "{sent:?}");
    let expected_len = BEFORE.len() + guarded_text.len() + 1 + AFTER.len();
    let received = pane.controlled_bytes_after_filler(expected_len);
    let mut expected = BEFORE.as_bytes().to_vec();
    expected.extend_from_slice(guarded_text.as_bytes());
    expected.push(b'\r');
    expected.extend_from_slice(AFTER.as_bytes());
    assert_eq!(received, expected);
    println!(
        "E2E refuse_if_exists mid-write complete: captured={} bytes; before={:?}; guarded={:?} bytes (prefix={:?}, suffix={:?}); delayed_enter=\\r; after={:?}; exact=true",
        received.len(),
        String::from_utf8_lossy(&received[..BEFORE.len()]),
        guarded_text.len(),
        String::from_utf8_lossy(&received[BEFORE.len()..BEFORE.len() + 16]),
        String::from_utf8_lossy(
            &received[BEFORE.len() + guarded_text.len() - 16..BEFORE.len() + guarded_text.len()]
        ),
        String::from_utf8_lossy(&received[received.len() - AFTER.len()..]),
    );
}

/// A guard path that cannot be checked (its parent is mode 000, so lstat
/// fails with EACCES) refuses with guard_path_unverifiable and 0 bytes: the
/// guard fails closed. Root bypasses directory permissions, so this is
/// skipped as root.
#[test]
fn refuse_if_exists_fails_closed_when_the_path_cannot_be_checked() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root bypasses directory permissions (no EACCES)");
        return;
    }
    let pane = HeldPane::start();
    let locked = pane.base.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let claim = locked.join("floor-claim");
    pane.hold_writer();
    let mut guarded = pane
        .command()
        .args([
            "pane",
            "send-keys",
            &pane.pane_id,
            "--refuse-if-exists",
            claim.to_str().unwrap(),
            "esc",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pane.wait_for_accepted("pane.send_keys", 1, &mut guarded);
    pane.release_writer();
    let refused = guarded.wait_with_output().unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let error = error_json(&refused);
    assert_eq!(error["error"]["code"], "guard_path_unverifiable", "{error}");
    assert_eq!(error["error"]["partial_text_consumed"], false, "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("os error 13"),
        "{error}"
    );
    let page = pane
        .command()
        .args(["pane", "send-text", &pane.pane_id, "PAGE"])
        .output()
        .unwrap();
    assert!(page.status.success(), "{page:?}");
    assert_eq!(pane.received_after_filler(4), b"PAGE");
}

/// Absent at write time: the guarded text is written as usual.
#[test]
fn refuse_if_exists_writes_when_the_path_is_still_absent() {
    let pane = HeldPane::start();
    let claim = pane.base.join("floor-claim");
    pane.hold_writer();
    let mut guarded = pane
        .command()
        .args([
            "pane",
            "send-text",
            &pane.pane_id,
            "--refuse-if-exists",
            claim.to_str().unwrap(),
            "hello",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    pane.wait_for_accepted("pane.send_text", 2, &mut guarded);
    pane.release_writer();
    let sent = guarded.wait_with_output().unwrap();
    assert!(sent.status.success(), "{sent:?}");
    assert_eq!(pane.received_after_filler(5), b"hello");
}
