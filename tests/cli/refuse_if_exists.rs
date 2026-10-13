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

struct HeldPane {
    base: PathBuf,
    socket: PathBuf,
    pane_id: String,
    hold: PathBuf,
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
        let fifo = std::ffi::CString::new(release.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // The pane reads no input until the test writes the release FIFO, then
        // records every byte it receives, unmodified.
        let shell = base.join("held-shell");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nstty raw -echo\n: > '{hold}/ready'\nread -r _ < '{hold}/release'\nexec cat > '{hold}/captured'\n",
                hold = hold.display()
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
