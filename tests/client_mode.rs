//! Integration tests for thin client mode.

#![cfg(unix)]

pub mod support;
#[path = "support/terminal_screen.rs"]
mod terminal_screen;
#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, client_shell_handshake, read_server_message, register_runtime_dir,
    register_spawned_herdr_pid, unregister_spawned_herdr_pid, wait_for_client_shell_bootstrap,
    wait_for_message_variant, wait_for_message_variants, wait_for_socket, wait_until,
    CURRENT_ENDPOINT_PROTOCOL_GENERATION as CURRENT_PROTOCOL, SERVER_MESSAGE_PANE_SURFACE,
    SERVER_MESSAGE_PANE_SURFACE_PATCH, SERVER_MESSAGE_SEMANTIC_NOTIFICATION,
    SERVER_MESSAGE_SERVER_SHUTDOWN,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-client-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    fn close_master(&mut self) {
        drop(self._master.take());
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        self.close_master();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn spawn_client_process(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
) -> SpawnedHerdr {
    spawn_client_process_with_args(config_home, runtime_dir, api_socket_path, &["client"])
}

fn spawn_client_shell_process(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
) -> SpawnedHerdr {
    spawn_client_process_with_args(config_home, runtime_dir, api_socket_path, &["client"])
}

fn spawn_client_process_with_args(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    args: &[&str],
) -> SpawnedHerdr {
    spawn_client_process_with_args_and_env(config_home, runtime_dir, api_socket_path, args, &[])
}

fn spawn_client_process_with_args_and_env(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    spawn_client_process_with_command(
        config_home,
        runtime_dir,
        api_socket_path,
        args,
        extra_env,
        crate::test_command::herdr_pty_command(),
    )
}

fn spawn_client_process_with_command(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    args: &[&str],
    extra_env: &[(&str, &str)],
    mut cmd: portable_pty::CommandBuilder,
) -> SpawnedHerdr {
    register_runtime_dir(runtime_dir);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    cmd.args(args);
    cmd.env("HERDR_DISABLE_SOUND", "1");
    let home = runtime_dir.join("home");
    let tmp = runtime_dir.join("tmp");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    cmd.env("HOME", &home);
    cmd.env("TMPDIR", &tmp);
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

fn spawn_server(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    client_socket_path: &PathBuf,
) -> SpawnedHerdr {
    spawn_server_with_config(
        config_home,
        runtime_dir,
        api_socket_path,
        client_socket_path,
        "onboarding = false\n",
    )
}

fn spawn_server_with_config(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket_path: &PathBuf,
    _client_socket_path: &PathBuf,
    config: &str,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(config_home.join(app_dir_name()).join("config.toml"), config).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = crate::test_command::herdr_pty_command();
    cmd.arg("server");
    let home = runtime_dir.join("home");
    let tmp = runtime_dir.join("tmp");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    cmd.env("HOME", &home);
    cmd.env("TMPDIR", &tmp);
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env(
        "HERDR_TEST_HANDOFF_OWNER_PID",
        std::process::id().to_string(),
    );
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

fn ping_socket(socket_path: &PathBuf) -> String {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");

    let request = r#"{"id":"1","method":"ping","params":{}}"#;
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response.trim().to_string()
}

fn send_json_request(socket_path: &PathBuf, request: &str) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("response should be valid JSON")
}

fn first_pane_id_in_workspace(socket_path: &PathBuf, workspace_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let request = format!(
            r#"{{"id":"pane_list","method":"pane.list","params":{{"workspace_id":"{workspace_id}"}}}}"#
        );
        let panes = send_json_request(socket_path, &request);
        if let Some(pane_id) = panes["result"]["panes"]
            .as_array()
            .and_then(|panes| panes.first())
            .and_then(|pane| pane["pane_id"].as_str())
        {
            return pane_id.to_string();
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("pane.list did not return a pane for workspace {workspace_id} before timeout");
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn client_connects_and_receives_pane_surface() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");
    let (version, error) = client_shell_handshake(&mut stream, CURRENT_PROTOCOL, 54, 23)
        .expect("handshake should succeed");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none(), "{error:?}");
    wait_for_client_shell_bootstrap(&mut stream, Duration::from_secs(10))
        .expect("should receive the shell snapshot and pane surface");

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn direct_attach_initial_mouse_capture_follows_config() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let config_path = config_home.join(app_dir_name()).join("config.toml");

    let spawned_server = spawn_server_with_config(
        &config_home,
        &runtime_dir,
        &api_socket,
        &client_socket,
        "onboarding = false\n[ui]\nmouse_capture = false\n",
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));
    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "create-workspace-for-direct-attach",
            "method": "workspace.create",
            "params": {"cwd": base},
        })
        .to_string(),
    );
    let terminal_id = created["result"]["root_pane"]["terminal_id"]
        .as_str()
        .expect("created terminal id")
        .to_string();

    let mut attach = spawn_client_process_with_args(
        &config_home,
        &runtime_dir,
        &api_socket,
        &["terminal", "attach", &terminal_id],
    );
    let output = spawn_pty_drain(
        attach
            ._master
            .as_ref()
            .expect("direct attach master")
            .try_clone_reader()
            .expect("clone direct attach PTY reader"),
    );
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
            read_output(&output).contains("\x1b[?7l")
        }),
        "direct attach terminal setup should complete; output: {:?}",
        read_output(&output)
    );
    assert!(
        !read_output(&output).contains("\x1b[?1000h"),
        "mouse capture disabled must not enable host mouse reporting; output: {:?}",
        read_output(&output)
    );
    assert!(
        read_output(&output).contains("\x1b[?2004h"),
        "direct attach must enable host bracketed paste; output: {:?}",
        read_output(&output)
    );
    assert!(
        !read_output(&output).contains("\x1b[?u"),
        "direct attach must not query rendered-client keyboard state; output: {:?}",
        read_output(&output)
    );

    let restore_watermark = output_len(&output);
    attach
        ._master
        .as_ref()
        .expect("direct attach master")
        .take_writer()
        .expect("direct attach PTY writer")
        .write_all(b"\x02q")
        .expect("detach direct attach client");
    let restore_output = drain_until_client_exits(&mut attach, &output, restore_watermark);
    assert!(
        restore_output.contains("\x1b[?2004l"),
        "direct attach must disable host bracketed paste on restore; output: {restore_output:?}"
    );
    drop(attach);

    fs::write(
        &config_path,
        "onboarding = false\n[ui]\nmouse_capture = true\n",
    )
    .unwrap();
    let attach = spawn_client_process_with_args(
        &config_home,
        &runtime_dir,
        &api_socket,
        &["terminal", "attach", &terminal_id],
    );
    let output = spawn_pty_drain(
        attach
            ._master
            .as_ref()
            .expect("direct attach master")
            .try_clone_reader()
            .expect("clone direct attach PTY reader"),
    );
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
            read_output(&output).contains("\x1b[?7l")
        }),
        "direct attach terminal setup should complete; output: {:?}",
        read_output(&output)
    );
    assert!(
        read_output(&output).contains("\x1b[?1000h"),
        "mouse capture enabled must retain host mouse reporting; output: {:?}",
        read_output(&output)
    );

    drop(spawned_server);
    cleanup_spawned_herdr(attach, base);
}

#[test]
fn client_sees_headless_startup_config_diagnostic() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    fs::create_dir_all(config_home.join(app_dir)).unwrap();
    fs::write(
        config_home.join(app_dir).join("config.toml"),
        "[keys\nprefix = \"ctrl+a\"\n",
    )
    .unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = crate::test_command::herdr_pty_command();
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", &api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    let spawned = SpawnedHerdr {
        _master: Some(pair.master),
        child,
    };
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let client = spawn_client_shell_process(&config_home, &runtime_dir, &api_socket);
    let output = spawn_pty_drain(
        client
            ._master
            .as_ref()
            .expect("client shell master")
            .try_clone_reader()
            .expect("clone client shell reader"),
    );
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            let output = read_output(&output);
            output.contains("config.toml") && output.contains("herdr config check")
        }),
        "client shell should render startup config diagnostic; output: {:?}",
        read_output(&output)
    );

    drop(spawned);
    cleanup_spawned_herdr(client, base);
}

#[test]
fn server_unreachable_shows_clear_error() {
    // when server is unreachable, the client exits quickly
    // with an actionable connection-failed message.
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let output = crate::test_command::herdr_command()
        .arg("client")
        .env("HERDR_DISABLE_SOUND", "1")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_STATE_HOME", runtime_dir.join("state"))
        .env("HERDR_SOCKET_PATH", &api_socket)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .output()
        .expect("client command should run");

    assert!(
        !output.status.success(),
        "client should fail when no server is running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to connect to server"),
        "stderr should mention connection failure: {stderr}"
    );
    assert!(
        stderr.contains("Is herdr server running?"),
        "stderr should include actionable guidance: {stderr}"
    );
    assert!(
        stderr.contains("Socket path:"),
        "stderr should include attempted socket path: {stderr}"
    );

    cleanup_test_base(&base);
}

#[test]
fn server_crash_after_attach_causes_lost_connection_error() {
    // attach a real thin client connection, kill server unexpectedly,
    // assert clean non-zero client exit plus lost-connection signal.
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    // Attach a real thin client (client subcommand) through PTY so handshake and
    // terminal setup paths are exercised.
    let mut thin_client = spawn_client_process(&config_home, &runtime_dir, &api_socket);

    // Prove attached before kill by waiting for recognizable rendered app content.
    let mut thin_reader = thin_client
        ._master
        .as_ref()
        .expect("thin client master")
        .try_clone_reader()
        .expect("clone client PTY reader");
    let (attached_before_kill, attach_output) = {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut buf = [0u8; 4096];
        let mut seen = false;
        let mut output = String::new();
        while Instant::now() < deadline {
            match thin_reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let out = String::from_utf8_lossy(&buf[..n]);
                    output.push_str(&out);
                    if out.contains("\u{2500}")
                        || out.contains("workspace")
                        || out.contains("pane")
                        || out.contains("terminal")
                    {
                        seen = true;
                        break;
                    }
                    if output.to_lowercase().contains("herdr:") {
                        break;
                    }
                }
                Ok(_) => thread::sleep(Duration::from_millis(30)),
                Err(_) => thread::sleep(Duration::from_millis(30)),
            }
        }
        (seen, output)
    };
    assert!(
        attached_before_kill,
        "thin client must complete attach and receive frame before server crash; output: {attach_output:?}"
    );

    // Kill server unexpectedly.
    if let Some(pid) = spawned.child.process_id() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    spawned.close_master();

    // Client should exit non-zero after connection loss.
    let mut crash_output = String::new();
    let exited = {
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut exited = false;
        while Instant::now() < deadline {
            if thin_client.child.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            // Keep draining client output so the process can progress to exit.
            let mut buf = [0u8; 1024];
            if let Ok(n) = thin_reader.read(&mut buf) {
                if n > 0 {
                    crash_output.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        exited
    };
    assert!(exited, "thin client should exit after server SIGKILL");

    let status = thin_client.child.wait().expect("wait thin client status");
    assert!(
        !status.success(),
        "thin client should exit non-zero after lost server connection"
    );

    // Drain trailing output and require the explicit user-visible lost-connection message.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match thin_reader.read(&mut buf) {
            Ok(n) if n > 0 => crash_output.push_str(&String::from_utf8_lossy(&buf[..n])),
            Ok(_) => break,
            Err(_) => break,
        }
        thread::sleep(Duration::from_millis(30));
    }

    let crash_output_lc = crash_output.to_lowercase();
    assert!(
        crash_output_lc.contains("lost connection to server"),
        "thin client must emit explicit lost-connection message after server crash; output: {crash_output:?}"
    );

    // Ensure server is gone.
    let _ = spawned.child.wait();

    cleanup_test_base(&base);
}

/// Any of the mouse-disable modes emitted by `clear_host_mouse_reporting` on
/// terminal restore. Their presence in the client's PTY output proves the
/// restore path (`TerminalGuard::Drop` → `restore_terminal_state`) ran.
const MOUSE_TEARDOWN_MARKERS: [&str; 2] = ["\u{1b}[?1003l", "\u{1b}[?1000l"];

/// Frames one retried shell command line. The client drops pane input while its endpoint is
/// offline or a surface handoff is pending, so a retried line can reach the shell in part. A
/// partial quoted line leaves the shell at its continuation prompt, where every later retry
/// also fails. Ctrl-C first cancels any partial or continued line; the shell stays alive.
fn retry_shell_line(command: &str) -> Vec<u8> {
    format!("\x03{command}\r").into_bytes()
}

fn output_has_mouse_teardown(output: &str) -> bool {
    MOUSE_TEARDOWN_MARKERS
        .iter()
        .all(|marker| output.contains(marker))
}

/// Shared buffer fed by a background PTY reader thread. Reading on a thread
/// keeps the blocking `Box<dyn Read>` (which has no timeout) off the test's
/// main thread, so a client that never exits fails the deadline instead of
/// hanging the whole test forever.
#[derive(Default)]
struct PtyOutput {
    bytes: Vec<u8>,
    // Keep legacy raw-string watermarks stable even across split UTF-8 reads.
    text: String,
}

type SharedOutput = std::sync::Arc<Mutex<PtyOutput>>;

fn spawn_pty_drain(mut reader: Box<dyn Read + Send>) -> SharedOutput {
    let output: SharedOutput = std::sync::Arc::new(Mutex::new(PtyOutput::default()));
    let thread_output = output.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut captured = thread_output.lock().unwrap_or_else(|p| p.into_inner());
                    captured.bytes.extend_from_slice(&buf[..n]);
                    captured.text.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
                Err(_) => break,
            }
        }
    });
    output
}

#[test]
fn screen_capture_preserves_split_utf8_bytes() {
    let reader = std::io::Cursor::new(b"caf\xc3").chain(std::io::Cursor::new(b"\xa9"));
    let output = spawn_pty_drain(Box::new(reader));
    assert!(wait_until(
        Duration::from_secs(2),
        Duration::from_millis(10),
        || {
            let bytes = output.lock().unwrap().bytes.clone();
            bytes == "café".as_bytes() && terminal_screen::text(&bytes, 80, 24).contains("café")
        }
    ));
}

fn read_output(output: &SharedOutput) -> String {
    output
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .text
        .clone()
}

/// Replay the original bytes: differential redraws can retain cells without
/// ever emitting a contiguous raw marker, or erase a marker from the screen.
fn read_screen(output: &SharedOutput) -> String {
    let bytes = output
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .bytes
        .clone();
    terminal_screen::text(&bytes, 80, 24)
}

fn screens_contain_marker(outputs: &[(&str, &SharedOutput)], marker: &str) -> bool {
    outputs
        .iter()
        .all(|(_, output)| read_screen(output).contains(marker))
}

fn screen_marker_diagnostics(outputs: &[(&str, &SharedOutput)], marker: &str) -> String {
    let mut missing = Vec::new();
    let details = outputs
        .iter()
        .map(|(name, output)| {
            let bytes = output
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .bytes
                .clone();
            let screen = terminal_screen::text(&bytes, 80, 24);
            let status = if screen.contains(marker) {
                "visible"
            } else {
                missing.push(*name);
                "MISSING"
            };
            let raw = String::from_utf8_lossy(&bytes);
            let mut start = raw.len().saturating_sub(4096);
            while !raw.is_char_boundary(start) {
                start += 1;
            }
            let tail = &raw[start..];
            format!(
                "{name}: marker {marker:?} {status}; current screen:\n{screen}\nraw capture tail (<=4096 bytes): {tail:?}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("missing clients: {missing:?}\n{details}")
}

#[test]
fn screen_marker_requires_both_current_client_screens() {
    let marker = "PROOF_ALIVE_ANON_ACCEPTED";
    let fragmented = b"PROOF_ALIVE_ALICE_A_AGAIN\x1b[1;13HANON_ACCEPTED\x1b[K";
    let captured = |bytes: &[u8]| {
        std::sync::Arc::new(Mutex::new(PtyOutput {
            bytes: bytes.to_vec(),
            text: String::from_utf8_lossy(bytes).into_owned(),
        }))
    };
    let alice = captured(fragmented);
    let bob = captured(format!("{marker}\r\x1b[2K").as_bytes());
    assert!(!read_output(&alice).contains(marker));
    assert!(read_screen(&alice).contains(marker));
    assert!(read_output(&bob).contains(marker));
    assert!(!read_screen(&bob).contains(marker));
    let outputs = [("Alice", &alice), ("Bob", &bob)];
    assert!(!screens_contain_marker(&outputs, marker));
    let diagnostics = screen_marker_diagnostics(&outputs, marker);
    assert!(diagnostics.contains("missing clients: [\"Bob\"]"));
    assert!(diagnostics.contains(&format!("Bob: marker {marker:?} MISSING")));
    assert!(diagnostics.contains("current screen:"));
    assert!(diagnostics.contains("\\u{1b}[2K"));
    let bob = captured(fragmented);
    assert!(screens_contain_marker(
        &[("Alice", &alice), ("Bob", &bob)],
        marker
    ));
}

#[test]
fn sigwinch_refreshes_host_palette_without_resizing() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let server = spawn_server_with_config(
        &config_home,
        &runtime_dir,
        &api_socket,
        &client_socket,
        "onboarding = false\n[theme]\nname = \"terminal\"\n",
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));
    let client = spawn_client_shell_process(&config_home, &runtime_dir, &api_socket);
    let master = client._master.as_ref().expect("client PTY");
    let output = spawn_pty_drain(master.try_clone_reader().unwrap());
    let mut writer = master.take_writer().unwrap();
    let queries = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\";
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            read_output(&output).contains(queries)
        }),
        "client should query the initial palette: {:?}",
        read_output(&output)
    );

    // Complete the startup query with a light palette. No appearance notification
    // is sent: this models a terminal whose colors are changed directly by OSC.
    let mut light =
        String::from("\x1b]10;rgb:0000/0000/0000\x1b\\\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
    for index in 0..=u8::MAX {
        light.push_str(&format!("\x1b]4;{index};rgb:ffff/ffff/ffff\x1b\\"));
    }
    writer.write_all(light.as_bytes()).unwrap();
    writer.flush().unwrap();
    // Let startup settle and the resize watcher install its signal handler.
    thread::sleep(Duration::from_millis(250));

    for _ in 0..2 {
        let watermark = output_len(&output);
        assert_eq!(
            unsafe { libc::kill(client.child.process_id().unwrap() as i32, libc::SIGWINCH) },
            0
        );
        assert!(
            wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
                let captured = read_output(&output);
                let refreshed = &captured[watermark..];
                refreshed.contains(queries) && refreshed.contains("\x1b]4;15;?\x1b\\")
            }),
            "SIGWINCH should query default colors and the ANSI palette without changing PTY size"
        );
        writer
            .write_all(b"\x1b]10;rgb:eeee/eeee/eeee\x1b\\\x1b]11;rgb:1111/2222/3333\x1b\\")
            .unwrap();
        writer.flush().unwrap();
    }

    drop(writer);
    drop(client);
    cleanup_spawned_herdr(server, base);
}

/// Current captured byte length, used as a watermark so a test can search only
/// the output emitted *after* a trigger. The teardown markers also appear in
/// normal attach-phase output, so matching the whole buffer is meaningless.
fn output_len(output: &SharedOutput) -> usize {
    output.lock().unwrap_or_else(|p| p.into_inner()).text.len()
}

fn rendered_active_workspace(screen: &str, endpoint: &str, pane_marker: &str) -> bool {
    // The federated footer names the actual active endpoint, not keyboard navigation.
    // The marker belongs to its unique workspace pane, never to the sidebar inventory.
    screen.contains(&format!("new · {endpoint}")) && screen.contains(pane_marker)
}

fn sidebar_row_click(screen: &str, label: &str) -> Vec<u8> {
    let sidebar_width = screen
        .lines()
        .find_map(|line| {
            line.chars()
                .position(|character| character == '│')
                .filter(|column| *column > 0)
        })
        .expect("visible sidebar boundary");
    let row = screen
        .lines()
        .position(|line| {
            line.chars()
                .take(sidebar_width)
                .collect::<String>()
                .contains(label)
        })
        .unwrap_or_else(|| panic!("sidebar row {label:?} is not visible: {screen}"))
        + 1;
    format!("\x1b[<0;7;{row}M\x1b[<0;7;{row}m").into_bytes()
}

#[test]
fn rendered_selection_rejects_ready_inventory_with_ignored_click() {
    let steady = "machines                 │\n ▾ Steady               ●│STEADY_ACTIVE_WORKSPACE\n   · steady-ready        │\n ▾ Handoff              ●│\n   · recovered-3         │\n new · Steady        menu│\n";
    assert!(rendered_active_workspace(
        steady,
        "Steady",
        "STEADY_ACTIVE_WORKSPACE"
    ));
    assert!(!rendered_active_workspace(
        steady,
        "Handoff",
        "HANDOFF_ACTIVE_WORKSPACE"
    ));
    let handoff = steady
        .replace("new · Steady", "new · Handoff")
        .replace("STEADY_ACTIVE_WORKSPACE", "HANDOFF_ACTIVE_WORKSPACE");
    assert!(rendered_active_workspace(
        &handoff,
        "Handoff",
        "HANDOFF_ACTIVE_WORKSPACE"
    ));
    assert!(!rendered_active_workspace(
        &handoff,
        "Steady",
        "STEADY_ACTIVE_WORKSPACE"
    ));
    // A footer switch alone is insufficient: require the intended workspace's surface.
    assert!(!rendered_active_workspace(
        &steady.replace("new · Steady", "new · Handoff"),
        "Handoff",
        "HANDOFF_ACTIVE_WORKSPACE"
    ));
}

#[test]
fn sidebar_row_click_ignores_notice_borders() {
    let screen = "┌─────────────────────────┐\n│● Endpoint unavailable   │\n└─────────────────────────┘\n   · local-returned      │\n";
    assert_eq!(
        sidebar_row_click(screen, "local-returned"),
        b"\x1b[<0;7;4M\x1b[<0;7;4m"
    );
}

#[test]
fn sidebar_row_click_tracks_restored_workspace_count() {
    for restored in [false, true] {
        let screen = format!(
            " machines                │\n                         │\n ▾ Local                 │local-returned in pane output\n{}   · local-returned      └─────────────────\n",
            if restored {
                "   · restored            │\n"
            } else {
                ""
            }
        );
        let row = if restored { 5 } else { 4 };
        assert_eq!(
            sidebar_row_click(&screen, "local-returned"),
            format!("\x1b[<0;7;{row}M\x1b[<0;7;{row}m").into_bytes()
        );
    }
}

/// Spawns a server + real thin client under a PTY and waits until the client
/// has attached and rendered a frame. Returns the pieces plus a shared buffer
/// that keeps accumulating PTY output (including teardown) on a background
/// thread.
fn attach_thin_client(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket: &PathBuf,
    client_socket: &PathBuf,
) -> (SpawnedHerdr, SpawnedHerdr, SharedOutput) {
    attach_thin_client_with_config(
        config_home,
        runtime_dir,
        api_socket,
        client_socket,
        "onboarding = false\n",
    )
}

fn attach_thin_client_with_config(
    config_home: &PathBuf,
    runtime_dir: &PathBuf,
    api_socket: &PathBuf,
    client_socket: &PathBuf,
    config: &str,
) -> (SpawnedHerdr, SpawnedHerdr, SharedOutput) {
    let spawned_server =
        spawn_server_with_config(config_home, runtime_dir, api_socket, client_socket, config);
    wait_for_socket(api_socket, Duration::from_secs(10));
    wait_for_socket(client_socket, Duration::from_secs(10));

    let thin_client = spawn_client_process(config_home, runtime_dir, api_socket);
    let reader = thin_client
        ._master
        .as_ref()
        .expect("thin client master")
        .try_clone_reader()
        .expect("clone client PTY reader");
    let output = spawn_pty_drain(reader);

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut attached = false;
    while Instant::now() < deadline {
        let out = read_output(&output);
        if out.contains('\u{2500}')
            || out.contains("workspace")
            || out.contains("pane")
            || out.contains("terminal")
        {
            attached = true;
            break;
        }
        if out.to_lowercase().contains("herdr:") {
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }
    assert!(
        attached,
        "thin client must attach and render a frame; output: {:?}",
        read_output(&output)
    );

    (spawned_server, thin_client, output)
}

#[test]
fn federated_launch_opens_local_directly_while_saved_ssh_is_unavailable() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    for select_remote in [false, true] {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let api_socket = runtime_dir.join("herdr.sock");
        fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
        fs::write(
            config_home.join(app_dir_name()).join("config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        let catalog_dir = runtime_dir
            .join("state")
            .join(app_dir_name())
            .join("client");
        fs::create_dir_all(&catalog_dir).unwrap();
        let profile = "0123456789abcdef0123456789abcdef";
        fs::write(catalog_dir.join("endpoints.json"), serde_json::json!({
            "version": 1, "selected_profile": select_remote.then_some(profile),
            "ssh": [{"id": profile, "label": "Unavailable remote", "target": "test-only", "session": "default", "enabled": true}],
        }).to_string()).unwrap();
        let bin = base.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("ssh"), "#!/bin/sh\nexit 255\n").unwrap();
        fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        // Exercise both auto-start and a subsequent attach to the healthy Local server.
        for args in [&[][..], &["client"][..]] {
            let client = spawn_client_process_with_args_and_env(
                &config_home,
                &runtime_dir,
                &api_socket,
                args,
                &[("PATH", &path)],
            );
            let output =
                spawn_pty_drain(client._master.as_ref().unwrap().try_clone_reader().unwrap());
            wait_for_socket(&api_socket, Duration::from_secs(10));
            assert!(wait_until(
                Duration::from_secs(10),
                Duration::from_millis(20),
                || { read_output(&output).contains("Local") }
            ));
            let mut input = client._master.as_ref().unwrap().take_writer().unwrap();
            // Input is gated until Local's active surface is ready, and that readiness can lag
            // the first rendered frame (the unavailable remote must not extend the wait). Retry
            // the write instead of assuming a single write lands, matching the recovered-Local
            // path below.
            assert!(
                wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
                    if read_output(&output).contains("LOCAL_DIRECT_READY") {
                        return true;
                    }
                    input
                        .write_all(&retry_shell_line("printf 'LOCAL_%s\\n' DIRECT_READY"))
                        .unwrap();
                    false
                }),
                "Local must accept input without waiting for SSH (remote selected: {select_remote}): {}",
                read_output(&output)
            );
            let text = read_output(&output);
            assert!(!text.contains("Local: connecting"), "{text}");
            assert!(!text.contains("Local: reconnecting"), "{text}");
            drop(input);
            drop(client);
        }
        let _ = send_json_request(
            &api_socket,
            r#"{"id":"stop","method":"server.stop","params":{}}"#,
        );
        cleanup_test_base(&base);
    }
}

#[test]
fn detached_handoff_importer_is_stopped_before_runtime_removal() {
    let _lock = test_lock();
    for early_failure in [false, true] {
        let base = unique_test_dir();
        let config = base.join("config");
        let runtime = base.join("runtime");
        let api = runtime.join("herdr.sock");
        let mut owned_pids = None;
        let exercise =
            || -> Result<(), &'static str> {
                let mut cleanup = support::ScopedHandoffServer::new(&base);
                let original =
                    spawn_server(&config, &runtime, &api, &runtime.join("herdr-client.sock"));
                wait_for_socket(&api, Duration::from_secs(10));
                cleanup.track_original(original.child.process_id().unwrap());
                let created = send_json_request(&api, &serde_json::json!({
                "id": "cleanup-workload", "method": "workspace.create", "params": {"cwd": base}
            }).to_string());
                let pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
                let process = send_json_request(&api, &serde_json::json!({
                "id": "cleanup-shell", "method": "pane.process_info", "params": {"pane_id": pane}
            }).to_string());
                let shell_pid = process["result"]["process_info"]["shell_pid"]
                    .as_u64()
                    .unwrap() as u32;
                let importer = cleanup.importer_exe();
                let output = crate::test_command::herdr_command()
                    .args([
                        "server",
                        "live-handoff",
                        "--import-exe",
                        importer.to_str().unwrap(),
                    ])
                    .env("HOME", runtime.join("home"))
                    .env("TMPDIR", runtime.join("tmp"))
                    .env("XDG_CONFIG_HOME", &config)
                    .env("XDG_STATE_HOME", runtime.join("state"))
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .env("HERDR_SOCKET_PATH", &api)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let importer_pid = if early_failure {
                    // Simulate failing before the caller can track the CLI result/socket.
                    // Drop must recover ownership from the pre-registered wrapper record.
                    let record = fs::read_to_string(base.join("importer-0.owner")).unwrap();
                    record.lines().next().unwrap().parse().unwrap()
                } else {
                    cleanup.track_importer()
                };
                owned_pids = Some((importer_pid, shell_pid));
                assert!(support::test_process_running(importer_pid));
                drop(original);
                assert!(
                    support::test_process_running(importer_pid),
                    "original child is not the importer owner"
                );
                assert!(
                    runtime.exists(),
                    "runtime must remain until importer termination"
                );
                if early_failure {
                    return Err("simulated failure immediately after CLI handoff");
                }
                cleanup.stop_and_cleanup().unwrap();
                Ok(())
            };
        let mut exercise = exercise;
        assert_eq!(exercise().is_err(), early_failure);
        let (importer_pid, shell_pid) = owned_pids.unwrap();
        assert!(
            !support::test_process_running(importer_pid),
            "detached importer must terminate within test scope"
        );
        assert!(
            wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
                !support::test_process_running(shell_pid)
            }),
            "imported scratch shell must also terminate"
        );
        assert!(
            !base.exists(),
            "remove runtime only after verified termination"
        );
        eprintln!(
            "cleanup early_failure={early_failure} importer={importer_pid} shell={shell_pid} terminated; runtime removed"
        );
    }
}

#[test]
fn importer_stderr_capture_is_capped_and_never_blocks_the_importer() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let artifacts = base.join("artifacts");
    let mut cleanup = support::ScopedHandoffServer::new(&base.join("fixture"));
    cleanup.set_diagnostics(&artifacts);
    // Writes three times the cap to stderr, then proves it ran to completion.
    let noisy = base.join("noisy.sh");
    let done = base.join("done");
    fs::write(
        &noisy,
        "#!/bin/sh\nhead -c 3145728 /dev/zero | tr '\\0' x >&2\nprintf done > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&noisy, fs::Permissions::from_mode(0o700)).unwrap();
    cleanup.set_importer_target(&noisy);
    let wrapper = cleanup.importer_exe();
    let status = std::process::Command::new(&wrapper)
        .arg(&done)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "importer must not fail on a full capture: {status}"
    );
    assert_eq!(fs::read_to_string(&done).unwrap(), "done");
    let captured = fs::metadata(artifacts.join("importer-0.stderr"))
        .unwrap()
        .len();
    assert!(
        captured > 0 && captured <= support::IMPORTER_STDERR_CAP_BYTES,
        "importer stderr capture must be capped: {captured} bytes"
    );
    cleanup.stop_and_cleanup().unwrap();
    let _ = fs::remove_dir_all(&base);
}

#[cfg(target_os = "linux")]
fn capture_threads() -> usize {
    fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .filter(|task| {
            fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|comm| comm.trim() == "importer-stderr")
        })
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn unused_importer_stderr_capture_is_joined_by_cleanup() {
    let base = unique_test_dir();
    let before = capture_threads();
    let mut cleanup = support::ScopedHandoffServer::new(&base.join("fixture"));
    cleanup.set_diagnostics(&base.join("artifacts"));
    // The CLI fails before it spawns the importer: the wrapper never runs.
    let wrapper = cleanup.importer_exe();
    // The thread names itself after it starts; wait for that, boundedly.
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
            capture_threads() > before
        }),
        "drainer waits for the wrapper"
    );
    // The owner record is published before any stderr setup, so teardown
    // between the two can still find and stop the wrapper's PID.
    let script = fs::read_to_string(&wrapper).unwrap();
    let published = script.find(".owner'\n").expect("owner record publish");
    let capture = script.find("exec 2<>").expect("stderr capture");
    assert!(published < capture, "{script}");
    cleanup.stop_and_cleanup().unwrap();
    assert!(
        wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            capture_threads() == before
        }),
        "cleanup must unblock and join a drainer whose wrapper never ran"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn importer_stderr_capture_setup_failure_is_not_silent() {
    let base = unique_test_dir();
    let fixture = base.join("fixture");
    let mut cleanup = support::ScopedHandoffServer::new(&fixture);
    cleanup.set_diagnostics(&base.join("artifacts"));
    // An existing name makes mkfifo fail.
    fs::create_dir_all(&fixture).unwrap();
    fs::write(fixture.join("importer-0.stderr.fifo"), "").unwrap();
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup.importer_exe()));
    assert!(
        built.is_err(),
        "a wrapper without bounded stderr capture must not be produced"
    );
    cleanup.stop_and_cleanup().unwrap();
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn handoff_failure_bundle_retains_importer_failure_and_socket_owners() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let fixture = base.join("fixture");
    let artifacts = base.join("artifacts");
    let config = fixture.join("config");
    let runtime = fixture.join("runtime");
    let api = runtime.join("herdr.sock");
    let mut cleanup = support::ScopedHandoffServer::new(&fixture);
    // A passing body leaves no artifacts behind.
    let passing = support::HandoffFailureBundle::new(&artifacts, "passing");
    let passing_dir = passing.dir();
    drop(passing);
    assert!(
        !passing_dir.exists(),
        "passing run must not retain a bundle"
    );

    let original = spawn_server(&config, &runtime, &api, &runtime.join("herdr-client.sock"));
    wait_for_socket(&api, Duration::from_secs(10));
    let original_pid = original.child.process_id().unwrap();
    cleanup.track_original(original_pid);
    // A live pane makes the handoff carry a real PTY descriptor.
    let created = send_json_request(
        &api,
        &serde_json::json!({"id": "w", "method": "workspace.create", "params": {"cwd": base}})
            .to_string(),
    );
    assert!(created.get("error").is_none(), "{created}");
    // The importer reports `restored`, then exits with an error: the source sees the
    // incident's `handoff stream closed while reading line` and rolls back.
    cleanup.set_importer_env("HERDR_TEST_HANDOFF_IMPORT_FAIL", "after_restored");
    let mut bundle_dir = None;
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut bundle = support::HandoffFailureBundle::new(&artifacts, "failing");
        bundle_dir = Some(bundle.dir());
        bundle.watch("handoff", &mut cleanup, Some(original_pid));
        bundle.copy_logs_from(&config.join(app_dir_name()));
        let importer = cleanup.importer_exe();
        bundle.note("cli live-handoff start");
        let output = crate::test_command::herdr_command()
            .args([
                "server",
                "live-handoff",
                "--import-exe",
                importer.to_str().unwrap(),
            ])
            .env("HOME", runtime.join("home"))
            .env("TMPDIR", runtime.join("tmp"))
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", runtime.join("state"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("HERDR_SOCKET_PATH", &api)
            .output()
            .unwrap();
        bundle.note(&format!("cli live-handoff exit={}", output.status));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }));
    assert!(
        failed.is_err(),
        "the injected importer failure must fail the handoff"
    );
    let dir = bundle_dir.unwrap();
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    // Snapshotted by the panic hook while the source still serves; cleanup then
    // stops the source and joins the stderr drainer, completing the capture.
    let attribution = read("attribution.txt");
    drop(original);
    cleanup.stop_and_cleanup().unwrap();
    let stderr = read("handoff/importer-0.stderr");
    let server_log = read("logs-0/herdr-server.log");
    // The importer prints its returned error only after its stream closes; the
    // source's rollback may SIGKILL it first. Either outcome must be on record.
    assert!(
        dir.join("handoff/importer-0.stderr").is_file()
            && (stderr.contains("test handoff import failure after restored")
                || server_log.contains("status=signal: 9")),
        "importer stderr must keep its error unless rollback killed it first: {stderr:?}\n{server_log}"
    );
    let start = read("handoff/importer-0.start");
    assert!(
        start.starts_with("pid=") && start.contains("uptime="),
        "{start}"
    );
    let owner = read("handoff/importer-0.owner");
    assert!(
        start.contains(&format!("pid={}", owner.lines().next().unwrap_or("?"))),
        "{owner}"
    );
    let receipts = read("receipts.txt");
    assert!(receipts.contains("cli live-handoff exit="), "{receipts}");
    assert!(
        server_log.contains("reaped during rollback"),
        "source log must carry the importer's observed exit: {server_log}"
    );
    assert!(
        attribution.contains("intended_importers=[(") && attribution.contains("importer pid="),
        "{attribution}"
    );
    // Snapshotted by the panic hook before it kills this thread's servers: the
    // source has restored its public listeners and still holds them.
    assert!(
        attribution.contains(&format!("{} socket dev=", api.display())),
        "{attribution}"
    );
    if cfg!(target_os = "linux") {
        for socket in [&api, &runtime.join("herdr-client.sock")] {
            assert!(
                attribution.lines().any(|line| line.starts_with("  LISTEN")
                    && line.contains(&format!(
                        " {} pid={original_pid} role=original source server",
                        socket.display()
                    ))),
                "public listener must be attributed to the source: {attribution}"
            );
        }
        assert!(
            attribution.contains(&format!("  pid={original_pid} ")),
            "{attribution}"
        );
    }
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn federated_saved_machines_recover_snapshots_after_live_handoff() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config = base.join("config");
    let runtime = base.join("runtime");
    let api = runtime.join("herdr.sock");
    let steady_base = base.join("steady");
    let handoff_base = base.join("handoff");
    let steady_config = steady_base.join("config");
    let steady_runtime = steady_base.join("runtime");
    let steady_api = steady_runtime.join("herdr.sock");
    let handoff_config = handoff_base.join("config");
    let handoff_runtime = handoff_base.join("runtime");
    let handoff_api = handoff_runtime.join("herdr.sock");
    // Declared before processes so unwinding drops clients/original children first,
    // then stops detached importers before removing their exact private runtime.
    let mut steady_cleanup = support::ScopedHandoffServer::new(&steady_base);
    let mut handoff_cleanup = support::ScopedHandoffServer::new(&handoff_base);
    let steady = spawn_server(
        &steady_config,
        &steady_runtime,
        &steady_api,
        &steady_runtime.join("herdr-client.sock"),
    );
    let handoff = spawn_server(
        &handoff_config,
        &handoff_runtime,
        &handoff_api,
        &handoff_runtime.join("herdr-client.sock"),
    );
    wait_for_socket(&steady_api, Duration::from_secs(10));
    wait_for_socket(&handoff_api, Duration::from_secs(10));
    steady_cleanup.track_original(steady.child.process_id().unwrap());
    handoff_cleanup.track_original(handoff.child.process_id().unwrap());
    // The support panic hook snapshots it before killing this thread's servers.
    let mut failure_bundle = support::HandoffFailureBundle::new(
        &support::failure_artifact_root(),
        "federated-live-handoff",
    );
    failure_bundle.watch("handoff", &mut handoff_cleanup, handoff.child.process_id());
    failure_bundle.watch("steady", &mut steady_cleanup, steady.child.process_id());
    for dir in [&handoff_config, &steady_config, &config] {
        failure_bundle.copy_logs_from(&dir.join(app_dir_name()));
    }
    let create = |socket: &PathBuf, label: &str| {
        let response = send_json_request(
            socket,
            &serde_json::json!({
                "id": "create", "method": "workspace.create",
                "params": {"cwd": base, "focus": false, "label": label}
            })
            .to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
        response["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let steady_workspace = create(&steady_api, "steady-ready");
    let first = create(&handoff_api, "handoff-ready");
    let steady_pane = first_pane_id_in_workspace(&steady_api, &steady_workspace);
    let handoff_pane = first_pane_id_in_workspace(&handoff_api, &first);
    // Only the active workspace's pane surface is rendered; inventories alone cannot
    // distinguish an ignored workspace click from a successful selection.
    send_pane_shell_command(
        &steady_api,
        &steady_pane,
        "printf 'STEADY_ACTIVE_WORKSPACE\\n'",
    );
    send_pane_shell_command(
        &handoff_api,
        &handoff_pane,
        "printf 'HANDOFF_ACTIVE_WORKSPACE\\n'",
    );
    let snapshot_data = || {
        let mut stream = UnixStream::connect(handoff_runtime.join("herdr-client.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, error) = client_shell_handshake(&mut stream, CURRENT_PROTOCOL, 54, 23).unwrap();
        assert!(error.is_none(), "{error:?}");
        // Observe the negotiated endpoint codec, not the unrelated JSON API snapshot.
        for _ in 0..8 {
            let (variant, payload) = read_server_message(&mut stream).unwrap();
            if variant == support::SERVER_MESSAGE_ENDPOINT_CONTROL {
                let ((kind, data), consumed): ((String, String), usize) =
                    bincode::serde::decode_from_slice(&payload, bincode::config::standard())
                        .unwrap();
                assert_eq!(consumed, payload.len());
                if kind == "shell.snapshot.v1" {
                    return data;
                }
            }
        }
        panic!("negotiated shell.snapshot.v1 did not arrive");
    };
    fs::create_dir_all(config.join(app_dir_name())).unwrap();
    fs::write(
        config.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();
    let catalog_dir = runtime.join("state").join(app_dir_name()).join("client");
    fs::create_dir_all(&catalog_dir).unwrap();
    fs::write(catalog_dir.join("endpoints.json"), serde_json::json!({
        "version": 1, "selected_profile": "0123456789abcdef0123456789abcdef",
        "ssh": [
            {"id":"0123456789abcdef0123456789abcdef", "label":"Steady", "target":"steady-test", "session":"default", "enabled":true},
            {"id":"fedcba9876543210fedcba9876543210", "label":"Handoff", "target":"handoff-test", "session":"default", "enabled":true}
        ]
    }).to_string()).unwrap();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_herdr"), bin.join("herdr")).unwrap();
    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    fs::write(bin.join("ssh"), format!(
        "#!/bin/sh\nfor arg do case \"$arg\" in steady-test) root={};; handoff-test) root={};; esac; last=\"$arg\"; done\nexport HOME=\"$root/runtime/home\" TMPDIR=\"$root/runtime/tmp\" XDG_CONFIG_HOME=\"$root/config\" XDG_STATE_HOME=\"$root/runtime/state\" XDG_RUNTIME_DIR=\"$root/runtime\" HERDR_SOCKET_PATH=\"$root/runtime/herdr.sock\"\nunset HERDR_CLIENT_SOCKET_PATH HERDR_SESSION\nexec /bin/sh -c \"$last\"\n",
        quote(&steady_base), quote(&handoff_base)
    )).unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let cli = |config: &PathBuf, runtime: &PathBuf, socket: &PathBuf, args: &[&str]| {
        let result = crate::test_command::herdr_command()
            .args(args)
            .env("HOME", runtime.join("home"))
            .env("TMPDIR", runtime.join("tmp"))
            .env("XDG_CONFIG_HOME", config)
            .env("XDG_STATE_HOME", runtime.join("state"))
            .env("XDG_RUNTIME_DIR", runtime)
            .env("HERDR_SOCKET_PATH", socket)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "CLI {args:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        result.stdout
    };
    let status = || -> Value {
        serde_json::from_slice(&cli(
            &config,
            &runtime,
            &api,
            &["status", "client", "--json"],
        ))
        .unwrap()
    };
    // Opt-in comparison against the exact incident-era client. Discovery/bridges and handoff
    // still use the current test server binary. Older clients have no runtime readout.
    let external_client = std::env::var_os("HERDR_RECONNECT_CLIENT_EXE");
    // Opt in to keeping Steady selected for every handoff, including large snapshots.
    // The default still clicks Handoff before the 100/110-workspace cases.
    let inactive_large = std::env::var("HERDR_RECONNECT_INACTIVE_LARGE").as_deref() == Ok("1");
    eprintln!("handoff inactive_large={inactive_large}");
    let machines_ready = |expected: usize| {
        if external_client.is_some() {
            return [&steady_api, &handoff_api]
                .iter()
                .enumerate()
                .all(|(index, socket)| {
                    let response = send_json_request(
                        socket,
                        r#"{"id":"count","method":"workspace.list","params":{}}"#,
                    );
                    response["result"]["workspaces"]
                        .as_array()
                        .is_some_and(|workspaces| {
                            workspaces.len() == if index == 0 { 1 } else { expected }
                        })
                });
        }
        let status = status();
        status["readout"]["schema_version"] == 1
            && status["readout"]["fresh"] == true
            && status["running"] == true
            && status["endpoints"].as_array().is_some_and(|machines| {
                machines.len() == 2
                    && machines.iter().all(|machine| {
                        machine["connected"] == true
                            && machine["listed"] == true
                            && machine["ready"] == true
                            && machine["workspace_count"]
                                == if machine["label"] == "Handoff" {
                                    expected
                                } else {
                                    1
                                }
                    })
            })
    };
    let command = if let Some(executable) = &external_client {
        assert!(
            std::path::Path::new(executable).is_file(),
            "external scratch client must exist"
        );
        eprintln!(
            "external scratch client executable={}",
            std::path::Path::new(executable).display()
        );
        let mut command = portable_pty::CommandBuilder::new(executable);
        crate::test_command::sanitize_pty_command_env(&mut command);
        command
    } else {
        crate::test_command::herdr_pty_command()
    };
    let client = spawn_client_process_with_command(
        &config,
        &runtime,
        &api,
        &["client"],
        &[("PATH", &path)],
        command,
    );
    let output = spawn_pty_drain(client._master.as_ref().unwrap().try_clone_reader().unwrap());
    let screen = || {
        terminal_screen::text(
            &output.lock().unwrap_or_else(|p| p.into_inner()).bytes,
            80,
            24,
        )
    };
    assert!(
        wait_until(
            Duration::from_secs(15),
            Duration::from_millis(20),
            || screen().contains("steady-ready") && screen().contains("handoff-ready")
        ),
        "both saved machines must bootstrap: {}",
        screen()
    );
    let client_log = || {
        fs::read_to_string(config.join(app_dir_name()).join("herdr-client.log")).unwrap_or_default()
    };
    let save_evidence = |phase: &str| {
        if let Some(dir) = std::env::var_os("HERDR_RECONNECT_EVIDENCE_DIR") {
            let dir = PathBuf::from(dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("{phase}.pty")),
                &output.lock().unwrap_or_else(|p| p.into_inner()).bytes,
            )
            .unwrap();
            fs::write(dir.join(format!("{phase}.screen.txt")), screen()).unwrap();
            fs::write(dir.join(format!("{phase}.client.log")), client_log()).unwrap();
            fs::write(
                dir.join(format!("{phase}.status.json")),
                serde_json::to_vec_pretty(&status()).unwrap(),
            )
            .unwrap();
        }
    };
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
            rendered_active_workspace(&screen(), "Steady", "STEADY_ACTIVE_WORKSPACE")
        }),
        "bootstrap must render Steady's active workspace: {}",
        screen()
    );
    save_evidence("bootstrap");
    let mut input = client._master.as_ref().unwrap().take_writer().unwrap();
    let mut count = 1;
    for workspaces in [3, 100, 110] {
        // By default cover inactive-small and selected-large; opt-in covers inactive-large.
        if workspaces == 100 && !inactive_large {
            input
                .write_all(&sidebar_row_click(&screen(), "recovered-3"))
                .unwrap();
            assert!(
                wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
                    rendered_active_workspace(&screen(), "Handoff", "HANDOFF_ACTIVE_WORKSPACE")
                        && machines_ready(count)
                }),
                "selected handoff workspace must actually render after the click: {}",
                screen()
            );
        }
        while count < workspaces {
            create(&handoff_api, &format!("workspace-{count:03}"));
            count += 1;
        }
        let label = format!("recovered-{workspaces}");
        let response = send_json_request(&handoff_api, &serde_json::json!({"id":"rename", "method":"workspace.rename", "params":{"workspace_id":first,"label":label}}).to_string());
        assert!(response.get("error").is_none(), "{response}");
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
                screen().contains(&label)
            }),
            "unique pre-handoff frame label must be installed: {}",
            screen()
        );
        let natural_snapshot = snapshot_data();
        eprintln!(
            "negotiated shell.snapshot.v1 workspaces={workspaces} natural_bytes={}",
            natural_snapshot.len()
        );
        if workspaces >= 100 && natural_snapshot.len() < 200_000 {
            // Persisted, bounded display-only labels qualify the 200 KB bootstrap size.
            // Ephemeral workspace tokens are deliberately not carried through live handoff.
            // Keep the real workspace/pane counts unchanged and every codec frozen.
            let natural: Value = serde_json::from_str(&natural_snapshot).unwrap();
            for pane in natural["panes"].as_array().unwrap() {
                let response = send_json_request(&handoff_api, &serde_json::json!({
                    "id":"padding", "method":"pane.rename",
                    "params":{"pane_id":pane["pane_id"],"label":format!("qualification-{}", "x".repeat(1500))}
                }).to_string());
                assert!(response.get("error").is_none(), "{response}");
            }
        }
        let snapshot = snapshot_data();
        let snapshot_value: Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(
            snapshot_value["workspaces"].as_array().unwrap().len(),
            workspaces
        );
        if workspaces >= 100 {
            assert!(
                snapshot.len() >= 200_000,
                "large snapshot must cross the incident's buffered size: {}",
                snapshot.len()
            );
        }
        eprintln!(
            "negotiated shell.snapshot.v1 workspaces={workspaces} qualified_bytes={}",
            snapshot.len()
        );
        if let Some(dir) = std::env::var_os("HERDR_RECONNECT_EVIDENCE_DIR") {
            fs::write(
                PathBuf::from(dir).join(format!("before-{workspaces}.snapshot.json")),
                &snapshot,
            )
            .unwrap();
        }
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
                machines_ready(workspaces)
            }),
            "both machine counts must be current before handoff: {}",
            status()
        );
        let (active_endpoint, active_marker) = if workspaces >= 100 && !inactive_large {
            ("Handoff", "HANDOFF_ACTIVE_WORKSPACE")
        } else {
            ("Steady", "STEADY_ACTIVE_WORKSPACE")
        };
        assert!(
            rendered_active_workspace(&screen(), active_endpoint, active_marker),
            "before {workspaces}: expected active {active_endpoint} workspace: {}",
            screen()
        );
        save_evidence(&format!("before-{workspaces}"));
        let log_watermark = client_log().len();
        let importer_exe = handoff_cleanup.importer_exe();
        failure_bundle.note(&format!(
            "cli live-handoff start workspaces={workspaces} importer={}",
            importer_exe.display()
        ));
        let handoff_result = cli(
            &handoff_config,
            &handoff_runtime,
            &handoff_api,
            &[
                "server",
                "live-handoff",
                "--import-exe",
                importer_exe.to_str().unwrap(),
            ],
        );
        let importer_pid = handoff_cleanup.track_importer();
        failure_bundle.note(&format!(
            "cli live-handoff succeeded workspaces={workspaces} importer_pid={importer_pid}"
        ));
        eprintln!(
            "actual CLI live-handoff workspaces={workspaces} binary={} result={}",
            env!("CARGO_BIN_EXE_herdr"),
            String::from_utf8_lossy(&handoff_result)
        );
        let completed_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        // No input, observer handshake, rename, metadata report or other server mutation may
        // wake rendering here. A new handshake and a post-handoff current-generation readout
        // distinguish recovery from the identical retained pre-handoff frame/count.
        // Older clients have no readout: observe only PTY/log files in this window, never
        // poll their servers. Their post-boot count is checked by snapshot_data AFTER it.
        let recovered = wait_until(Duration::from_secs(20), Duration::from_millis(100), || {
            let log = client_log();
            let tail = log.get(log_watermark..).unwrap_or_default();
            let text = screen();
            let machine_online = text
                .lines()
                .any(|line| line.contains("Handoff") && line.contains('●'));
            let fresh_generation = external_client.is_some()
                || status()["readout"]["updated_at_ms"]
                    .as_u64()
                    .is_some_and(|timestamp| timestamp >= completed_at_ms);
            tail.contains("endpoint transport failed")
                && tail.contains("endpoint handshake succeeded")
                && text.contains(&label)
                && text.contains("Steady")
                && rendered_active_workspace(&text, active_endpoint, active_marker)
                && machine_online
                && !text.contains("reconnecting")
                && fresh_generation
                && (external_client.is_some() || machines_ready(workspaces))
        });
        save_evidence(&format!("no-input-after-{workspaces}"));
        if !recovered {
            // Diagnostic counterexample only AFTER the no-input acceptance window fails.
            // A host focus event can request repaint without sending text into a pane.
            input.write_all(b"\x1b[I").unwrap();
            thread::sleep(Duration::from_millis(500));
            save_evidence(&format!("diagnostic-focus-after-{workspaces}"));
            eprintln!(
                "no-input recovery failed; diagnostic focus frame:\n{}",
                screen()
            );
        }
        let snapshot = snapshot_data();
        let snapshot_value: Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(
            snapshot_value["workspaces"].as_array().unwrap().len(),
            workspaces,
            "post-handoff bootstrap must retain the workspace count"
        );
        if workspaces >= 100 {
            assert!(
                snapshot.len() >= 200_000,
                "post-handoff bootstrap must retain the qualified size: {}",
                snapshot.len()
            );
        }
        eprintln!(
            "post-handoff negotiated shell.snapshot.v1 workspaces={workspaces} bytes={}",
            snapshot.len()
        );
        if let Some(dir) = std::env::var_os("HERDR_RECONNECT_EVIDENCE_DIR") {
            fs::write(
                PathBuf::from(dir).join(format!("after-{workspaces}.snapshot.json")),
                &snapshot,
            )
            .unwrap();
        }
        assert!(
            rendered_active_workspace(&screen(), active_endpoint, active_marker),
            "after {workspaces}: expected retained active {active_endpoint} workspace: {}",
            screen()
        );
        save_evidence(&format!("after-{workspaces}"));
        eprintln!(
            "handoff workspaces={workspaces} recovered={recovered} readout={}\n{}",
            status(),
            screen()
        );
        assert!(
            recovered,
            "handoff must restore the saved machine snapshot with {workspaces} workspaces"
        );
    }
    drop(input);
    drop(client);
    drop(handoff);
    drop(steady);
    handoff_cleanup.stop_and_cleanup().unwrap();
    steady_cleanup.stop_and_cleanup().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn federated_client_starts_without_local_and_survives_its_restart() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let remote_config = base.join("remote-config");
    let remote_runtime = base.join("remote-runtime");
    let remote_api = remote_runtime.join("herdr.sock");
    let remote_client = remote_runtime.join("herdr-client.sock");
    let mut remote_server =
        spawn_server(&remote_config, &remote_runtime, &remote_api, &remote_client);
    wait_for_socket(&remote_api, Duration::from_secs(10));
    wait_for_socket(&remote_client, Duration::from_secs(10));
    let created = send_json_request(
        &remote_api,
        &serde_json::json!({
            "id": "remote-workspace", "method": "workspace.create",
            "params": {"cwd": base, "focus": true, "label": "remote-ready"},
        })
        .to_string(),
    );
    let remote_pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    send_pane_shell_command(&remote_api, remote_pane, "printf 'REMOTE_INITIAL_FRAME\\n'");

    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();
    let catalog_dir = runtime_dir
        .join("state")
        .join(app_dir_name())
        .join("client");
    fs::create_dir_all(&catalog_dir).unwrap();
    let profile = "0123456789abcdef0123456789abcdef";
    fs::write(catalog_dir.join("endpoints.json"), serde_json::json!({
        "version": 1, "selected_profile": profile,
        "ssh": [{"id": profile, "label": "Test remote", "target": "test-only", "session": "default", "enabled": true}],
    }).to_string()).unwrap();

    // The SSH executable is private to this client. Discovery and the stdio bridge run the real
    // binary against a second disposable local server, never the developer's saved hosts.
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(base.join("home")).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_herdr"), bin.join("herdr")).unwrap();
    let quote =
        |path: &std::path::Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let ssh_commands = base.join("ssh-commands");
    let bridge_pid = base.join("bridge-pid");
    fs::write(bin.join("ssh"), format!(
        "#!/bin/sh\nexport HOME={} XDG_CONFIG_HOME={} XDG_RUNTIME_DIR={} HERDR_SOCKET_PATH={}\nunset HERDR_CLIENT_SOCKET_PATH HERDR_SESSION\nfor arg do last=\"$arg\"; done\nprintf '%s\\n' \"$last\" >> {}\ncase \"$last\" in *remote-client-bridge*) printf '%s\\n' \"$$\" > {};; esac\nexec /bin/sh -c \"$last\"\n",
        quote(&base.join("home")), quote(&remote_config), quote(&remote_runtime), quote(&remote_api), quote(&ssh_commands), quote(&bridge_pid),
    )).unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut client = spawn_client_process_with_args_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &["client"],
        &[("PATH", &path)],
    );
    let output = spawn_pty_drain(client._master.as_ref().unwrap().try_clone_reader().unwrap());
    let screen_text = || {
        let bytes = output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bytes
            .clone();
        terminal_screen::text(&bytes, 80, 24)
    };
    assert!(
        wait_until(Duration::from_secs(12), Duration::from_millis(20), || {
            screen_text().contains("REMOTE_INITIAL_FRAME")
        }),
        "remote must be usable before Local exists: {}",
        read_output(&output)
    );
    assert!(
        fs::read_to_string(&ssh_commands)
            .unwrap()
            .contains("remote-client-bridge --idle-timeout-v1"),
        "saved machine discovery must opt into the advertised bridge idle timeout"
    );

    let mut input = client._master.as_ref().unwrap().take_writer().unwrap();
    send_pane_shell_command(&remote_api, remote_pane, "reconnect_survivor=ALIVE");
    for cycle in 1..=3 {
        let pid: libc::pid_t = fs::read_to_string(&bridge_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let marker = format!("REMOTE_RECONNECTED_{cycle}");
        send_pane_shell_command(&remote_api, remote_pane, &format!("printf '{marker}\\n'"));
        assert!(
            wait_until(Duration::from_secs(15), Duration::from_millis(20), || {
                screen_text().contains(&marker)
            }),
            "remote reconnect {cycle} must restore the visible screen without switching machines"
        );
        assert!(
            wait_until(Duration::from_secs(8), Duration::from_millis(100), || {
                if screen_text().contains(&format!("REMOTE_ALIVE_INPUT_{cycle}")) {
                    return true;
                }
                input
                    .write_all(&retry_shell_line(&format!(
                        "printf 'REMOTE_%s_INPUT_{cycle}\\n' \"$reconnect_survivor\""
                    )))
                    .unwrap();
                false
            }),
            "remote reconnect {cycle} must restore visible input and preserve the shell: {}",
            screen_text()
        );
    }

    let mut local = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "local-workspace", "method": "workspace.create",
            "params": {"cwd": base, "focus": true, "label": "local-online"},
        })
        .to_string(),
    );
    assert_eq!(created["result"]["type"], "workspace_created");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(20),
        || screen_text().contains("local-online")
    ));

    local.child.kill().unwrap();
    local.close_master();
    drop(local);
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            if screen_text().contains("REMOTE_SURVIVED") {
                return true;
            }
            input
                .write_all(&retry_shell_line("printf 'REMOTE_%s\\n' SURVIVED"))
                .unwrap();
            false
        }),
        "Local loss must not interrupt remote input or output: {}",
        screen_text()
    );
    assert!(client.child.try_wait().unwrap().is_none());

    let restarted = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "local-returned", "method": "workspace.create",
            "params": {"cwd": base, "focus": true, "label": "local-returned"},
        })
        .to_string(),
    );
    assert_eq!(created["result"]["type"], "workspace_created");
    assert!(
        wait_until(Duration::from_secs(12), Duration::from_millis(20), || {
            screen_text().contains("local-returned")
        }),
        "Local must reconnect with fresh metadata"
    );
    input
        .write_all(b"printf 'REMOTE_%s\\n' STILL_SELECTED\r")
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            screen_text().contains("REMOTE_STILL_SELECTED")
        }),
        "Local recovery must not steal selection: {}",
        screen_text()
    );
    let local_pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    send_pane_shell_command(
        &api_socket,
        local_pane,
        "printf 'LOCAL_WHILE_REMOTE_STALLED\\n'",
    );
    {
        struct ResumeBridge(libc::pid_t);
        impl Drop for ResumeBridge {
            fn drop(&mut self) {
                unsafe { libc::kill(self.0, libc::SIGCONT) };
            }
        }
        let bridge: libc::pid_t = fs::read_to_string(&bridge_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(bridge, libc::SIGSTOP) }, 0);
        let _resume_bridge = ResumeBridge(bridge);
        input
            .write_all(&sidebar_row_click(&screen_text(), "local-returned"))
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
                screen_text().contains("LOCAL_WHILE_REMOTE_STALLED")
            }),
            "one Local selection must not wait for the remote bridge: {}",
            screen_text()
        );
        assert!(
            wait_until(Duration::from_secs(3), Duration::from_millis(100), || {
                if screen_text().contains("LOCAL_INPUT_WHILE_REMOTE_STALLED") {
                    return true;
                }
                input
                    .write_all(&retry_shell_line(
                        "printf 'LOCAL_%s\\n' INPUT_WHILE_REMOTE_STALLED",
                    ))
                    .unwrap();
                false
            }),
            "Local input must become usable while the remote bridge remains stopped: {}",
            screen_text()
        );
    }
    input
        .write_all(&sidebar_row_click(&screen_text(), "remote-ready"))
        .unwrap();
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(20),
        || screen_text().contains("REMOTE_STILL_SELECTED")
    ));

    let watermark = output_len(&output);
    remote_server.child.kill().unwrap();
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
            read_output(&output)[watermark..].contains("reconnecting")
        }),
        "the selected remote must be marked disconnected"
    );
    let text = read_output(&output);
    assert!(
        text.rfind("\x1b[?1000h") > text.rfind("\x1b[?1000l"),
        "losing the selected remote must keep host mouse reporting enabled"
    );

    send_pane_shell_command(
        &api_socket,
        local_pane,
        "printf 'LOCAL_RECOVERED_SURFACE\\n'",
    );
    // Select the fresh workspace below Local's restored workspace.
    // A fast shutdown may leave no saved workspace, so locate the actual row.
    input
        .write_all(&sidebar_row_click(&screen_text(), "local-returned"))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
            screen_text().contains("LOCAL_RECOVERED_SURFACE")
        }),
        "recovered Local must be selectable: {}",
        screen_text()
    );
    // A coherent frame precedes the final host-effects fence; input stays gated until then.
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(100), || {
            if screen_text().contains("LOCAL_INPUT_RECOVERED") {
                return true;
            }
            input
                .write_all(&retry_shell_line("printf 'LOCAL_%s\\n' INPUT_RECOVERED"))
                .unwrap();
            false
        }),
        "recovered Local must accept input: {}",
        screen_text()
    );
    drop(input);
    drop(client);
    drop(restarted);
    drop(remote_server);
    cleanup_test_base(&base);
}

#[test]
fn client_shell_detaches_restores_and_freshly_reattaches_to_current_state() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "client-shell-lifecycle-workspace",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true, "label": "shell-lifecycle"},
        })
        .to_string(),
    );
    assert_eq!(created["result"]["type"], "workspace_created", "{created}");
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id")
        .to_string();
    send_pane_shell_command(&api_socket, &pane_id, "printf 'SHELL_LIFECYCLE_INITIAL\\n'");

    let mut client_a = spawn_client_shell_process(&config_home, &runtime_dir, &api_socket);
    let output_a = spawn_pty_drain(
        client_a
            ._master
            .as_ref()
            .expect("first client shell PTY")
            .try_clone_reader()
            .expect("clone first client shell reader"),
    );
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            let output = read_output(&output_a);
            output.contains("shell-lifecycle") && output.contains("SHELL_LIFECYCLE_INITIAL")
        }),
        "client shell should compose one coherent snapshot and pane surface; output: {:?}",
        read_output(&output_a)
    );

    let detach_watermark = output_len(&output_a);
    client_a
        ._master
        .as_ref()
        .expect("first client shell PTY")
        .take_writer()
        .expect("first client shell writer")
        .write_all(b"\x02q")
        .expect("detach first client shell");
    let detach_output = drain_until_client_exits(&mut client_a, &output_a, detach_watermark);
    assert!(
        output_has_mouse_teardown(&detach_output),
        "client shell should restore the host terminal after detach; output: {detach_output:?}"
    );
    assert!(
        ping_socket(&api_socket).contains("pong"),
        "server should remain alive after client shell detach"
    );
    drop(client_a);

    send_pane_shell_command(
        &api_socket,
        &pane_id,
        "printf 'SHELL_LIFECYCLE_DETACHED\\n'",
    );
    let mut client_b = spawn_client_shell_process(&config_home, &runtime_dir, &api_socket);
    let output_b = spawn_pty_drain(
        client_b
            ._master
            .as_ref()
            .expect("reattached client shell PTY")
            .try_clone_reader()
            .expect("clone reattached client shell reader"),
    );
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            let output = read_output(&output_b);
            output.contains("shell-lifecycle") && output.contains("SHELL_LIFECYCLE_DETACHED")
        }),
        "fresh client shell should receive current state and detached-period output; server exit: {:?}; output: {:?}",
        server.child.try_wait(),
        read_output(&output_b)
    );

    let disconnect_watermark = output_len(&output_b);
    if let Some(pid) = server.child.process_id() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    server.close_master();
    let disconnect_output =
        drain_until_client_exits(&mut client_b, &output_b, disconnect_watermark);
    assert!(
        output_has_mouse_teardown(&disconnect_output),
        "client shell should restore the host terminal after endpoint loss; output: {disconnect_output:?}"
    );
    assert!(
        disconnect_output
            .to_lowercase()
            .contains("lost connection to server"),
        "client shell should explain endpoint loss; output: {disconnect_output:?}"
    );

    drop(server);
    cleanup_spawned_herdr(client_b, base);
}

fn captured_window_titles(output: &SharedOutput) -> Vec<String> {
    read_output(output)
        .split("\x1b]0;")
        .skip(1)
        .filter_map(|suffix| {
            suffix
                .split_once('\x07')
                .map(|(title, _)| title.to_string())
        })
        .collect()
}

fn wait_for_window_title(output: &SharedOutput, expected_suffix: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(title) = captured_window_titles(output)
            .into_iter()
            .find(|title| title.ends_with(expected_suffix))
        {
            return title;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "outer window title ending in {expected_suffix:?} was not emitted; titles: {:?}; output: {:?}",
        captured_window_titles(output),
        read_output(output)
    );
}

fn wait_for_pane_terminal_title(socket_path: &PathBuf, pane_id: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let request = serde_json::json!({
            "id": "window-title-pane-get",
            "method": "pane.get",
            "params": {"pane_id": pane_id},
        });
        let response = send_json_request(socket_path, &request.to_string());
        if response["result"]["pane"]["terminal_title"].as_str() == Some(expected) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("pane {pane_id} did not report terminal title {expected:?}");
}

fn send_pane_shell_command(socket_path: &PathBuf, pane_id: &str, command: &str) {
    let request = serde_json::json!({
        "id": "window-title-command",
        "method": "pane.send_input",
        "params": {
            "pane_id": pane_id,
            "text": command,
            "keys": ["Enter"],
        }
    });
    let response = send_json_request(socket_path, &request.to_string());
    assert_eq!(response["result"]["type"], "ok", "{response}");
}

#[test]
fn configured_window_title_tracks_all_tokens_and_focused_osc_only() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let (server, client, output) = attach_thin_client_with_config(
        &config_home,
        &runtime_dir,
        &api_socket,
        &client_socket,
        "onboarding = false\n[ui]\nwindow_title = \"H={hostname}|W={workspace}|T={tab}|P={pane}|O={terminal_title}\"\n",
    );

    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "create-workspace",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true},
        })
        .to_string(),
    );
    assert_eq!(created["result"]["type"], "workspace_created", "{created}");
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace id")
        .to_string();
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("pane id")
        .to_string();
    let tab_id = created["result"]["tab"]["tab_id"]
        .as_str()
        .expect("tab id")
        .to_string();

    for request in [
        serde_json::json!({
            "id": "rename-workspace",
            "method": "workspace.rename",
            "params": {"workspace_id": workspace_id, "label": "space-a"},
        }),
        serde_json::json!({
            "id": "rename-tab",
            "method": "tab.rename",
            "params": {"tab_id": tab_id, "label": "tab-a"},
        }),
        serde_json::json!({
            "id": "rename-pane",
            "method": "pane.rename",
            "params": {"pane_id": pane_id, "label": "pane-a"},
        }),
    ] {
        let response = send_json_request(&api_socket, &request.to_string());
        assert!(response.get("result").is_some(), "{response}");
    }

    let renamed = wait_for_window_title(&output, "|W=space-a|T=tab-a|P=pane-a|O=");
    assert!(renamed.starts_with("H="));
    assert!(
        !renamed.starts_with("H=|"),
        "hostname token was empty: {renamed}"
    );

    send_pane_shell_command(&api_socket, &pane_id, r"printf '\033]0;building\007'");
    wait_for_window_title(&output, "|W=space-a|T=tab-a|P=pane-a|O=building");

    let second_tab = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "second-tab",
            "method": "tab.create",
            "params": {"workspace_id": workspace_id, "focus": true},
        })
        .to_string(),
    );
    assert_eq!(second_tab["result"]["type"], "tab_created", "{second_tab}");
    let second_pane_id = second_tab["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("second pane id")
        .to_string();
    wait_for_window_title(&output, "|W=space-a|T=2|P=|O=");
    let titles_before_hidden_update = captured_window_titles(&output).len();
    send_pane_shell_command(&api_socket, &pane_id, r"printf '\033]0;hidden update\007'");
    // Intentionally consume the AppState title through a read-only request
    // before the queued source is handled.
    wait_for_pane_terminal_title(&api_socket, &pane_id, "hidden update");
    send_pane_shell_command(
        &api_socket,
        &second_pane_id,
        r"printf '\033]0;foreground marker\007'",
    );
    wait_for_window_title(&output, "|W=space-a|T=2|P=|O=foreground marker");
    assert!(
        captured_window_titles(&output)[titles_before_hidden_update..]
            .iter()
            .all(|title| !title.ends_with("|O=hidden update")),
        "a hidden pane title reached the outer terminal"
    );

    let focused = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "focus-first-tab",
            "method": "tab.focus",
            "params": {"tab_id": tab_id},
        })
        .to_string(),
    );
    assert_eq!(focused["result"]["tab"]["focused"], true, "{focused}");
    wait_for_window_title(&output, "|W=space-a|T=tab-a|P=pane-a|O=hidden update");

    drop(server);
    cleanup_spawned_herdr(client, base);
}

/// Polls until the client exits, then returns only the output captured after
/// the `since` byte watermark. Panics if the client does not exit within the
/// deadline.
fn drain_until_client_exits(
    thin_client: &mut SpawnedHerdr,
    output: &SharedOutput,
    since: usize,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut exited = false;
    while Instant::now() < deadline {
        if thin_client.child.try_wait().ok().flatten().is_some() {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Give the reader thread a beat to flush trailing teardown bytes.
    thread::sleep(Duration::from_millis(100));
    let full = read_output(output);
    assert!(exited, "thin client should exit; output: {full:?}");
    full.get(since..).unwrap_or_default().to_string()
}

/// Attaches a thin client, runs `trigger` to force an exit, and asserts the
/// client emits the mouse teardown after that point. The teardown markers also
/// appear in normal attach output, so only bytes emitted after the trigger
/// (past the watermark) count.
fn assert_client_restores_terminal(trigger: impl FnOnce(&mut SpawnedHerdr, &mut SpawnedHerdr)) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let (mut spawned_server, mut thin_client, pty_output) =
        attach_thin_client(&config_home, &runtime_dir, &api_socket, &client_socket);

    let since = output_len(&pty_output);
    trigger(&mut spawned_server, &mut thin_client);

    let output = drain_until_client_exits(&mut thin_client, &pty_output, since);
    assert!(
        output_has_mouse_teardown(&output),
        "client must emit mouse teardown after trigger; output after trigger: {output:?}"
    );

    // SpawnedHerdr::Drop kills and reaps both processes with a bounded wait.
    drop(spawned_server);
    cleanup_spawned_herdr(thin_client, base);
}

/// The `--remote` ssh-death path: killing the bridge closes the socket, the
/// client sees EOF and unwinds normally, so the terminal is restored. This is
/// the path that does NOT deliver a signal to the client. Guards against a
/// regression that would leave mouse reporting on after an ssh disconnect.
#[test]
fn client_restores_terminal_on_server_eof() {
    assert_client_restores_terminal(|server, _client| {
        // Kill the server unexpectedly; the client socket closes and the
        // client reader hits EOF, mirroring the ssh bridge dying under
        // `herdr --remote`.
        if let Some(pid) = server.child.process_id() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        server.close_master();
    });
}

/// A direct SIGHUP/SIGTERM with a writable terminal follows the graceful quit
/// path and emits the terminal teardown. Actual terminal-window closure also
/// makes the PTY unwritable and is covered separately below.
#[test]
fn client_restores_terminal_on_sighup() {
    assert_client_restores_terminal(|_server, client| {
        let pid = client.child.process_id().expect("thin client pid") as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
    });
}

fn read_until_client_attaches(client: &SpawnedHerdr) -> String {
    let master = client._master.as_ref().expect("thin client master");
    let fd = master.as_raw_fd().expect("thin client PTY file descriptor");
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert_ne!(flags, -1, "read thin client PTY flags");
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1,
        "make thin client PTY nonblocking"
    );

    let mut reader = master.try_clone_reader().expect("clone client PTY reader");
    let mut output = String::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let mut buf = [0u8; 4096];
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("read thin client PTY: {err}"),
        }
        if output.contains('\u{2500}')
            || output.contains("workspace")
            || output.contains("pane")
            || output.contains("terminal")
        {
            return output;
        }
    }
    panic!("thin client must attach and render a frame; output: {output:?}");
}

#[test]
fn client_exits_cleanly_when_terminal_and_transport_hang_up() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned_server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut thin_client = spawn_client_process(&config_home, &runtime_dir, &api_socket);
    read_until_client_attaches(&thin_client);

    // Freeze the client so the dead terminal and transport EOF are both
    // observable when it resumes, making the `--remote` shutdown race deterministic.
    let client_pid = thin_client.child.process_id().expect("thin client pid") as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(client_pid, libc::SIGSTOP) },
        0,
        "stop thin client"
    );
    let server_pid = spawned_server.child.process_id().expect("server pid") as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(server_pid, libc::SIGKILL) },
        0,
        "kill server transport"
    );
    spawned_server.close_master();
    thin_client.close_master();
    assert_eq!(
        unsafe { libc::kill(client_pid, libc::SIGCONT) },
        0,
        "resume thin client"
    );

    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = thin_client.child.try_wait().expect("poll thin client") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };

    drop(spawned_server);
    cleanup_spawned_herdr(thin_client, base);

    let status = status.expect("thin client should exit after terminal and transport hang up");
    assert!(
        status.success(),
        "thin client should exit cleanly after terminal and transport hang up, got {status}"
    );
}

#[test]
fn client_exits_cleanly_when_terminal_hangs_up() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned_server = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut thin_client = spawn_client_process(&config_home, &runtime_dir, &api_socket);
    let attached_output = read_until_client_attaches(&thin_client);

    // Closing the final PTY master models the outer terminal disappearing: the
    // foreground client receives SIGHUP and writes to stdout/stderr fail.
    thin_client.close_master();
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = thin_client.child.try_wait().expect("poll thin client") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let server_response = ping_socket(&api_socket);

    drop(spawned_server);
    cleanup_spawned_herdr(thin_client, base);

    let status = status.unwrap_or_else(|| {
        panic!("thin client did not exit after PTY hangup; attach output: {attached_output:?}")
    });
    assert!(
        status.success(),
        "thin client should exit cleanly after PTY hangup, got {status}; attach output: {attached_output:?}"
    );
    assert!(
        server_response.contains("pong"),
        "server should survive client PTY hangup: {server_response}"
    );
}

#[test]
fn client_receives_pane_surface_after_pane_output() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");
    let (version, error) = client_shell_handshake(&mut stream, CURRENT_PROTOCOL, 54, 23)
        .expect("handshake should succeed");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none(), "{error:?}");
    wait_for_client_shell_bootstrap(&mut stream, Duration::from_secs(10))
        .expect("initial client shell bootstrap");

    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "create-output-workspace",
            "method": "workspace.create",
            "params": {"label": "output", "focus": true}
        })
        .to_string(),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id");
    assert!(wait_for_message_variant(
        &mut stream,
        Duration::from_secs(5),
        SERVER_MESSAGE_PANE_SURFACE,
    )
    .expect("wait for created workspace surface"));

    let sent = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "send-output",
            "method": "pane.send_text",
            "params": {"pane_id": pane_id, "text": "printf 'test-output\\n'\\n"}
        })
        .to_string(),
    );
    assert!(sent.get("error").is_none(), "{sent}");
    assert!(
        wait_for_message_variants(
            &mut stream,
            Duration::from_secs(5),
            &[
                SERVER_MESSAGE_PANE_SURFACE,
                SERVER_MESSAGE_PANE_SURFACE_PATCH,
            ],
        )
        .expect("wait for post-output pane surface"),
        "should receive a pane surface update after pane output"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn unavailable_restored_pane_keeps_saved_cwd_in_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let data_dir = config_home.join(app_dir_name());
    let missing_cwd = base.join("missing-cwd-for-test");
    let missing_cwd = missing_cwd.to_str().expect("test cwd should be UTF-8");
    fs::create_dir_all(&data_dir).unwrap();
    let session = serde_json::json!({
        "version": 2,
        "workspaces": [{
            "custom_name": "missing-cwd",
            "layout": { "Pane": 0 },
            "panes": { "0": { "cwd": missing_cwd } },
            "zoomed": false,
            "focused": 0,
            "root_pane": 0
        }],
        "active": 0,
        "selected": 0
    });
    fs::write(
        data_dir.join("session.json"),
        serde_json::to_vec_pretty(&session).unwrap(),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let workspaces = send_json_request(
        &api_socket,
        r#"{"id":"workspace_list","method":"workspace.list","params":{}}"#,
    );
    let restored_workspace = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["label"] == "missing-cwd")
        .expect("server should restore workspace with missing pane cwd");
    let workspace_id = restored_workspace["workspace_id"]
        .as_str()
        .expect("restored workspace should have public id");
    let pane_id = first_pane_id_in_workspace(&api_socket, workspace_id);
    let pane = send_json_request(
        &api_socket,
        &format!(r#"{{"id":"pane_get","method":"pane.get","params":{{"pane_id":"{pane_id}"}}}}"#),
    );
    assert_eq!(pane["result"]["pane"]["workspace_id"], workspace_id);
    let cwd = pane["result"]["pane"]["cwd"]
        .as_str()
        .expect("restored pane should retain saved cwd");
    assert_eq!(cwd, missing_cwd);
    assert!(pane["result"]["pane"]["restore_error"]
        .as_str()
        .is_some_and(|error| error.contains("directory")));

    let client_shell = spawn_client_shell_process(&config_home, &runtime_dir, &api_socket);
    let output = spawn_pty_drain(
        client_shell
            ._master
            .as_ref()
            .expect("restored client shell PTY")
            .try_clone_reader()
            .expect("clone restored client shell reader"),
    );
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            let screen = read_output(&output);
            screen.contains("missing-cwd") && screen.contains("unavailable")
        }),
        "client shell should render the unavailable pane; output: {:?}",
        read_output(&output)
    );
    drop(client_shell);
    let stopped = send_json_request(
        &api_socket,
        r#"{"id":"stop","method":"server.stop","params":{}}"#,
    );
    assert!(stopped.get("error").is_none(), "{stopped}");
    let mut spawned = spawned;
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(20),
        || { spawned.child.try_wait().unwrap().is_some() }
    ));
    drop(spawned);

    fs::create_dir(missing_cwd).unwrap();
    let restarted = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let recovered = send_json_request(
        &api_socket,
        &format!(r#"{{"id":"recovered","method":"pane.get","params":{{"pane_id":"{pane_id}"}}}}"#),
    );
    assert_eq!(
        std::fs::canonicalize(recovered["result"]["pane"]["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(missing_cwd).unwrap()
    );
    assert!(recovered["result"]["pane"]["restore_error"].is_null());
    let sent = send_json_request(
        &api_socket,
        &serde_json::json!({"id": "type", "method": "pane.send_text", "params": {
            "pane_id": pane_id, "text": "printf 'RESTORE_RETRY_OK\\n'\n"
        }})
        .to_string(),
    );
    assert!(sent.get("error").is_none(), "{sent}");
    cleanup_spawned_herdr(restarted, base);
}

#[test]
fn server_stop_wakes_idle_server_without_client_or_pty_events() {
    let _lock = test_lock();
    for wait_for_app in [false, true] {
        assert_server_stop_exits_without_other_events(wait_for_app);
    }
}

fn assert_server_stop_exits_without_other_events(wait_for_app: bool) {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    if wait_for_app {
        wait_for_socket(&client_socket, Duration::from_secs(10));
        // Synchronize with App startup without creating a pane or a client that
        // could accidentally wake the event loop after the stop flag is set.
        let workspaces = send_json_request(
            &api_socket,
            r#"{"id":"ready","method":"workspace.list","params":{}}"#,
        );
        assert_eq!(workspaces["result"]["workspaces"], serde_json::json!([]));
    }
    // The other case stops as soon as the API binds, including startup before
    // the App begins receiving requests or registers client readiness.
    let stopped = send_json_request(
        &api_socket,
        r#"{"id":"stop","method":"server.stop","params":{}}"#,
    );
    assert!(stopped.get("error").is_none(), "{stopped}");

    // The immediate API acknowledgement alone does not prove shutdown: the
    // real server process must complete cleanup with no further socket traffic.
    let mut exit_status = None;
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
            exit_status = spawned.child.try_wait().unwrap();
            exit_status.is_some()
        }),
        "idle server did not exit after acknowledging server.stop"
    );
    assert!(exit_status.unwrap().success());
    assert!(!api_socket.exists(), "API socket must be cleaned up");
    assert!(!client_socket.exists(), "client socket must be cleaned up");
    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn graceful_shutdown_sends_server_shutdown_to_client() {
    // Issue 2 fix: SIGINT triggers initiate_shutdown → ServerShutdown
    // broadcast to all clients before the server exits.
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");
    let (version, error) = client_shell_handshake(&mut stream, CURRENT_PROTOCOL, 54, 23)
        .expect("handshake should succeed");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none(), "{error:?}");
    wait_for_client_shell_bootstrap(&mut stream, Duration::from_secs(5))
        .expect("client shell bootstrap");

    // Send SIGINT to the server process to trigger graceful shutdown.
    if let Some(pid) = spawned.child.process_id() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGINT);
        }
    }

    // The client should receive a ServerShutdown message
    // before the connection is closed, not just an abrupt EOF.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let result = read_server_message(&mut stream);
    match result {
        Ok((variant, _payload)) => {
            assert_eq!(
                variant, SERVER_MESSAGE_SERVER_SHUTDOWN,
                "expected ServerShutdown, got variant {variant}"
            );
        }
        Err(e) => {
            panic!("expected ServerShutdown message before connection close, got error: {e}");
        }
    }

    // Wait for the server to exit.
    spawned.close_master();
    let _ = spawned.child.wait();

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn client_receives_notify_on_agent_state_change() {
    // Notification events (sound/toast) are forwarded as
    // ServerMessage::Notify to connected clients when an agent state change
    // is triggered via the API (pane.report_agent).
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // Enable toast and sound in config so the server produces notifications.
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n[ui.toast]\nenabled = true\n[ui.sound]\nenabled = true\n",
    )
    .unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);

    // Spawn the server directly (not using spawn_server helper because it
    // overwrites the config file with a minimal one).
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = crate::test_command::herdr_pty_command();
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", &api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    let spawned = SpawnedHerdr {
        _master: Some(pair.master),
        child,
    };
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));

    let mut stream = UnixStream::connect(&client_socket).expect("should connect");
    let (version, error) = client_shell_handshake(&mut stream, CURRENT_PROTOCOL, 54, 23)
        .expect("handshake should succeed");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none(), "{error:?}");
    wait_for_client_shell_bootstrap(&mut stream, Duration::from_secs(5))
        .expect("client shell bootstrap");

    // Create a workspace via the API.
    let mut ws_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let request = r#"{"id":"1","method":"workspace.create","params":{}}"#;
    writeln!(ws_stream, "{}", request).unwrap();
    let mut reader = BufReader::new(ws_stream);
    let mut ws_response = String::new();
    reader.read_line(&mut ws_response).unwrap();

    // Extract the workspace ID and pane ID from the response.
    let ws_id = ws_response
        .split('"')
        .find(|s| s.starts_with("w_"))
        .unwrap_or("w_1")
        .to_string();

    // Get pane list to find a pane ID.
    let mut pane_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let pane_request =
        format!(r#"{{"id":"2","method":"pane.list","params":{{"workspace_id":"{ws_id}"}}}}"#);
    writeln!(pane_stream, "{}", pane_request).unwrap();
    let mut pane_reader = BufReader::new(pane_stream);
    let mut pane_response = String::new();
    pane_reader.read_line(&mut pane_response).unwrap();

    // Extract first pane ID (format: p_<ws>_<pane>).
    let pane_id = pane_response
        .split('"')
        .find(|s| s.starts_with("p_"))
        .unwrap_or("p_1_1")
        .to_string();

    // Report agent as Blocked via the API — this should trigger a
    // ServerMessage::Notify with kind=Sound (Request sound).
    let mut report_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let report_request = format!(
        r#"{{"id":"3","method":"pane.report_agent","params":{{"pane_id":"{pane_id}","agent":"pi","state":"blocked","source":"test"}}}}"#
    );
    writeln!(report_stream, "{}", report_request).unwrap();
    let mut report_reader = BufReader::new(report_stream);
    let mut report_response = String::new();
    report_reader.read_line(&mut report_response).unwrap();

    // Read messages from the client stream and look for the semantic notification.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut found_notify = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match read_server_message(&mut stream) {
            Ok((variant, _payload)) => {
                if variant == SERVER_MESSAGE_SEMANTIC_NOTIFICATION {
                    found_notify = true;
                    break;
                }
                // Snapshot and pane-surface messages may arrive first.
            }
            Err(_) => {
                break;
            }
        }
    }

    assert!(
        found_notify,
        "client should receive a semantic notification after pane.report_agent"
    );

    // Now report Idle from Working — this should trigger a Done sound
    // if the pane is in a background workspace.
    // First, create a second workspace to make the first one "background".
    let mut ws2_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let ws2_request = r#"{"id":"4","method":"workspace.create","params":{}}"#;
    writeln!(ws2_stream, "{}", ws2_request).unwrap();
    let mut ws2_reader = BufReader::new(ws2_stream);
    let mut ws2_response = String::new();
    ws2_reader.read_line(&mut ws2_response).unwrap();

    // Focus the new workspace (making the first one background).
    let ws2_id = ws2_response
        .split('"')
        .find(|s| s.starts_with("w_"))
        .unwrap_or("w_2")
        .to_string();
    let mut focus_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let focus_request = format!(
        r#"{{"id":"5","method":"workspace.focus","params":{{"workspace_id":"{ws2_id}"}}}}"#
    );
    writeln!(focus_stream, "{}", focus_request).unwrap();
    let mut focus_reader = BufReader::new(focus_stream);
    let mut focus_response = String::new();
    focus_reader.read_line(&mut focus_response).unwrap();

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
            ping_socket(&api_socket).contains("pong")
        }),
        "server should stay responsive after workspace focus"
    );

    // Report agent as Working first, then Idle — this transition in a
    // background workspace should trigger a Done sound notification.
    let mut work_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let work_request = format!(
        r#"{{"id":"6","method":"pane.report_agent","params":{{"pane_id":"{pane_id}","agent":"pi","state":"working","source":"test"}}}}"#
    );
    writeln!(work_stream, "{}", work_request).unwrap();
    let mut work_reader = BufReader::new(work_stream);
    let mut work_response = String::new();
    work_reader.read_line(&mut work_response).unwrap();

    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
            ping_socket(&api_socket).contains("pong")
        }),
        "server should stay responsive after working state report"
    );

    let mut idle_stream = UnixStream::connect(&api_socket).expect("connect to API");
    let idle_request = format!(
        r#"{{"id":"7","method":"pane.report_agent","params":{{"pane_id":"{pane_id}","agent":"pi","state":"idle","source":"test"}}}}"#
    );
    writeln!(idle_stream, "{}", idle_request).unwrap();
    let mut idle_reader = BufReader::new(idle_stream);
    let mut idle_response = String::new();
    idle_reader.read_line(&mut idle_response).unwrap();

    // Read messages and look for the done semantic notification.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut found_done_notify = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match read_server_message(&mut stream) {
            Ok((variant, _payload)) => {
                if variant == SERVER_MESSAGE_SEMANTIC_NOTIFICATION {
                    found_done_notify = true;
                    break;
                }
                // Snapshot and pane-surface messages may arrive first.
            }
            Err(e) => {
                eprintln!("read error while looking for done notification: {e}");
                break;
            }
        }
    }

    assert!(
        found_done_notify,
        "client should receive a semantic notification when a background pane transitions Working→Idle"
    );

    cleanup_spawned_herdr(spawned, base);
}

/// Sender provenance must originate from real TUI input, not a test-authored hello
/// or an API-supplied user name. Keep both named binary clients attached throughout.
#[test]
fn sender_identity_two_real_clients_last_input() {
    use std::ffi::OsStr;

    // The generic server launcher cannot override HOME/TMPDIR/session. Keep this
    // stricter launcher local: reuse its command sanitizer and process ownership
    // guards without changing other integration tests' launch behavior.
    fn spawn_owned(
        base: &PathBuf,
        config_home: &std::path::Path,
        runtime_dir: &std::path::Path,
        api_socket: &PathBuf,
        client_socket: &PathBuf,
        role: &str,
    ) -> (SpawnedHerdr, SharedOutput) {
        assert!(
            api_socket.starts_with(base),
            "API socket must be under child TMPDIR"
        );
        assert!(
            client_socket.starts_with(base),
            "client socket must be under child TMPDIR"
        );
        let mut cmd = crate::test_command::herdr_pty_command();
        for (key, _) in cmd.iter_full_env() {
            let bytes = key.as_encoded_bytes();
            assert!(
                !bytes.starts_with(b"HERDR_") || key == OsStr::new("HERDR_SESSION"),
                "inherited Herdr context survived sanitization: {key:?}"
            );
        }
        assert_eq!(cmd.get_env("HERDR_SESSION"), Some(OsStr::new("default")));
        assert_eq!(cmd.get_env("PI_CODING_AGENT_DIR"), None);
        assert_eq!(cmd.get_env("PI_CONFIG_DIR"), None);

        cmd.arg(role);
        cmd.cwd(base);
        for (key, path) in [
            ("HOME", base.join("home")),
            ("TMPDIR", base.clone()),
            ("XDG_CONFIG_HOME", config_home.to_path_buf()),
            ("XDG_RUNTIME_DIR", runtime_dir.to_path_buf()),
            ("XDG_STATE_HOME", runtime_dir.join("state")),
            ("XDG_DATA_HOME", base.join("data")),
            ("XDG_CACHE_HOME", base.join("cache")),
        ] {
            fs::create_dir_all(&path).unwrap();
            cmd.env(key, &path);
            assert_eq!(cmd.get_env(key), Some(path.as_os_str()));
        }
        cmd.env("SHELL", "/bin/sh");
        // Prevent the invoking shell's startup hooks from escaping private HOME.
        cmd.env_remove("ENV");
        cmd.env_remove("BASH_ENV");
        cmd.env("HERDR_SESSION", "proof-3937");
        cmd.env("HERDR_SOCKET_PATH", api_socket);
        cmd.env("HERDR_CLIENT_SOCKET_PATH", client_socket);
        cmd.env("HERDR_DISABLE_SOUND", "1");
        for (key, value) in cmd.iter_full_env() {
            if key.as_encoded_bytes().starts_with(b"HERDR_") {
                let expected = match key.to_str().expect("owned Herdr key is UTF-8") {
                    "HERDR_SESSION" => OsStr::new("proof-3937"),
                    "HERDR_SOCKET_PATH" => api_socket.as_os_str(),
                    "HERDR_CLIENT_SOCKET_PATH" => client_socket.as_os_str(),
                    "HERDR_DISABLE_SOUND" => OsStr::new("1"),
                    _ => panic!("unexpected Herdr environment key: {key:?}"),
                };
                assert_eq!(value, expected, "owned environment {key:?}");
            }
        }
        register_runtime_dir(runtime_dir);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);
        let output = spawn_pty_drain(pair.master.try_clone_reader().unwrap());
        let process = SpawnedHerdr {
            _master: Some(pair.master),
            child,
        };
        println!("3937 process role={role} pid={:?} config={} session=proof-3937 HOME={} TMPDIR={} api={} client={} inherited_context=cleared",
            process.child.process_id(), config_home.display(), base.join("home").display(),
            base.display(), api_socket.display(), client_socket.display());
        (process, output)
    }

    fn request(socket: &PathBuf, id: &str, method: &str, params: Value) -> Value {
        // Unlike the older generic request helper, a missing API response must
        // fail a bounded proof rather than hang the entire integration binary.
        let mut stream = UnixStream::connect(socket).expect("connect to owned API socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": id, "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("bounded API response");
        let response: Value = serde_json::from_str(&line).expect("JSON API response");
        assert_eq!(response["id"], id, "{response}");
        response
    }

    fn last_input(socket: &PathBuf, pane: &str, alias: bool) -> Value {
        let params = if alias {
            serde_json::json!({"pane_id": pane})
        } else {
            serde_json::json!({"pane": pane})
        };
        let response = request(socket, "last-input", "pane.last_input", params);
        assert!(response.get("error").is_none(), "{response}");
        let result = response["result"]
            .as_object()
            .expect("last input result object");
        assert_eq!(result.len(), 2, "{response}");
        assert_eq!(result["type"], "pane_last_input", "{response}");
        assert!(
            result.contains_key("last_input"),
            "explicit null is required: {response}"
        );
        let input = result["last_input"].clone();
        if !input.is_null() {
            let record = input.as_object().expect("last input record");
            assert_eq!(record.len(), 3, "{response}");
            assert!(record.contains_key("user"), "{response}");
            assert!(
                input["user"].is_null() || input["user"].is_string(),
                "{response}"
            );
            assert!(input["client_id"].as_u64().is_some(), "{response}");
            assert!(input["at"].as_u64().is_some(), "{response}");
        }
        input
    }

    fn epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn type_and_observe(
        socket: &PathBuf,
        pane: &str,
        writer: &mut dyn Write,
        outputs: &[(&str, &SharedOutput)],
        command: &str,
        marker: &str,
        user: &str,
    ) -> Value {
        let started = epoch_ms();
        let mut observed = Value::Null;
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
                observed = last_input(socket, pane, false);
                if observed["user"].as_str() == Some(user)
                    && observed["at"].as_u64().is_some_and(|at| at >= started)
                    && screens_contain_marker(outputs, marker)
                {
                    return true;
                }
                // Readiness can lag the first frame; retry through the real client's
                // PTY, never through pane.send_input or a fabricated endpoint hello.
                writer.write_all(&retry_shell_line(command)).unwrap();
                writer.flush().unwrap();
                false
            }),
            "real {user} input must reach {pane}; last_input={observed}; {}",
            screen_marker_diagnostics(outputs, marker)
        );
        assert!(
            observed["at"].as_u64().unwrap() <= epoch_ms(),
            "epoch milliseconds: {observed}"
        );
        println!(
            "3937 typed marker={marker} pane={pane} result={}",
            serde_json::json!({"type": "pane_last_input", "last_input": observed})
        );
        observed
    }

    struct OwnedScratch(PathBuf);
    impl Drop for OwnedScratch {
        fn drop(&mut self) {
            cleanup_test_base(&self.0);
        }
    }

    let _lock = test_lock();
    // mktemp creates an exclusive owner-only directory; never use a live profile
    // or trust an inherited TMPDIR to identify a safe scratch session. Use a short
    // /tmp root rather than nesting below Main's possibly long TMPDIR: Unix socket
    // paths must fit sockaddr_un (108 bytes on Linux). Child TMPDIR is this root,
    // so both sockets remain below its owned TMPDIR, with cleanup bound to Drop.
    let temp = std::process::Command::new("mktemp")
        .args(["-d", "/tmp/herdr-3937-proof.XXXXXX"])
        .output()
        .expect("create owned scratch directory");
    assert!(temp.status.success(), "mktemp failed: {:?}", temp.stderr);
    let scratch = OwnedScratch(PathBuf::from(
        String::from_utf8(temp.stdout).unwrap().trim(),
    ));
    let base = &scratch.0;
    let runtime = base.join("runtime");
    let api = runtime.join("herdr.sock");
    let client_socket = runtime.join("herdr-client.sock");
    let server_config = base.join("server-config");
    let alice_config = base.join("alice-config");
    let bob_config = base.join("bob-config");
    for (home, config) in [
        (&server_config, "onboarding = false\n[identity]\nname = \"server-not-sender\"\n[ui]\nwindow_title = \"PROOF:{workspace}\"\n"),
        (&alice_config, "onboarding = false\n[identity]\nname = \"alice\"\n[ui]\nwindow_title = \"PROOF:{workspace}\"\n"),
        (&bob_config, "onboarding = false\n[identity]\nname = \"bob\"\n[ui]\nwindow_title = \"PROOF:{workspace}\"\n"),
    ] {
        fs::create_dir_all(home.join(app_dir_name())).unwrap();
        fs::write(home.join(app_dir_name()).join("config.toml"), config).unwrap();
    }
    let (mut server, _server_output) = spawn_owned(
        base,
        &server_config,
        &runtime,
        &api,
        &client_socket,
        "server",
    );
    wait_for_socket(&api, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));
    let created = request(
        &api,
        "create-proof",
        "workspace.create",
        serde_json::json!({"cwd": base, "focus": true, "label": "sender-proof"}),
    );
    assert_eq!(created["result"]["type"], "workspace_created", "{created}");
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(last_input(&api, &pane, false), Value::Null);
    assert_eq!(last_input(&api, &pane, true), Value::Null);
    println!(
        "3937 before-input pane={pane} result={{\"type\":\"pane_last_input\",\"last_input\":null}}"
    );

    for params in [
        serde_json::json!({}),
        serde_json::json!({"pane": "p_999999_999999"}),
    ] {
        let response = request(&api, "invalid-last-input", "pane.last_input", params);
        assert!(response.get("error").is_some(), "{response}");
        assert!(response.get("result").is_none(), "{response}");
        println!("3937 invalid-query response={response}");
    }

    let (mut alice, output_a) = spawn_owned(
        base,
        &alice_config,
        &runtime,
        &api,
        &client_socket,
        "client",
    );
    let (mut bob, output_b) =
        spawn_owned(base, &bob_config, &runtime, &api, &client_socket, "client");
    let outputs = [("Alice", &output_a), ("Bob", &output_b)];
    for (name, output) in &outputs {
        let named_output = [(*name, *output)];
        assert!(
            wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
                screens_contain_marker(&named_output, "sender-proof")
            }),
            "{name} must render the sender-proof workspace before input; {}",
            screen_marker_diagnostics(&named_output, "sender-proof")
        );
    }
    assert_eq!(
        last_input(&api, &pane, false),
        Value::Null,
        "attach/render/focus are not pane input"
    );
    let mut input_a = alice._master.as_ref().unwrap().take_writer().unwrap();
    let mut input_b = bob._master.as_ref().unwrap().take_writer().unwrap();
    let a1 = type_and_observe(
        &api,
        &pane,
        &mut *input_a,
        &outputs,
        "proof_survivor=ALIVE; printf 'PROOF_%s\\n' ALICE_A",
        "PROOF_ALICE_A",
        "alice",
    );
    let b1 = type_and_observe(
        &api,
        &pane,
        &mut *input_b,
        &outputs,
        "printf 'PROOF_%s_%s\\n' \"$proof_survivor\" BOB_B",
        "PROOF_ALIVE_BOB_B",
        "bob",
    );
    let a2 = type_and_observe(
        &api,
        &pane,
        &mut *input_a,
        &outputs,
        "printf 'PROOF_%s_%s\\n' \"$proof_survivor\" ALICE_A_AGAIN",
        "PROOF_ALIVE_ALICE_A_AGAIN",
        "alice",
    );
    assert_ne!(
        a1["client_id"], b1["client_id"],
        "two attached clients have distinct IDs"
    );
    assert_eq!(a1["client_id"], a2["client_id"], "Alice keeps her ID");
    assert!(a1["at"].as_u64().unwrap() <= b1["at"].as_u64().unwrap());
    assert!(b1["at"].as_u64().unwrap() <= a2["at"].as_u64().unwrap());
    assert_eq!(
        last_input(&api, &pane, true),
        a2,
        "alias/read does not change provenance"
    );

    let rejected = request(
        &api,
        "reject-unknown-send",
        "pane.send_text",
        serde_json::json!({"pane_id": "p_999999_999999", "text": "must-not-arrive"}),
    );
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert_eq!(
        last_input(&api, &pane, false),
        a2,
        "rejected anonymous input is not accepted input"
    );
    println!("3937 rejected-send response={rejected} preserved={a2}");

    // A rejected send with text/valid keys must not attribute or enqueue a partial
    // prefix. Poisoning the persistent shell variable also makes the later real
    // typed markers a direct counterexample if rejected bytes leaked into the PTY.
    let rejected_guard = request(
        &api,
        "reject-terminal-guard",
        "pane.send_input_guarded",
        serde_json::json!({
            "pane_id": pane,
            "expected_terminal": "term_3937_unknown",
            "text": "proof_survivor=POISONED_GUARD",
            "keys": ["Enter"],
        }),
    );
    assert_eq!(
        rejected_guard["error"]["code"], "terminal_identity_mismatch",
        "{rejected_guard}"
    );
    assert!(rejected_guard.get("result").is_none(), "{rejected_guard}");
    assert_eq!(
        last_input(&api, &pane, false),
        a2,
        "failed terminal guard preserves sender"
    );
    println!("3937 rejected-guard response={rejected_guard} preserved={a2}");
    let rejected_partial = request(
        &api,
        "reject-valid-prefix-invalid-key",
        "pane.send_input_guarded",
        serde_json::json!({
            "pane_id": pane,
            "expected_terminal": created["result"]["root_pane"]["terminal_id"],
            "text": "proof_survivor=POISONED_PARTIAL",
            "keys": ["Enter", "SenderProofInvalidKey"],
        }),
    );
    assert_eq!(
        rejected_partial["error"]["code"], "invalid_key",
        "{rejected_partial}"
    );
    assert!(
        rejected_partial.get("result").is_none(),
        "{rejected_partial}"
    );
    assert_eq!(
        last_input(&api, &pane, false),
        a2,
        "invalid later key cannot accept a valid prefix or clear sender"
    );
    println!("3937 rejected-valid-prefix response={rejected_partial} preserved={a2}");

    // Accepted anonymous input clears attribution without forgetting the attached
    // clients' identities or replacing the shell whose state they share.
    let anonymous = request(
        &api,
        "anonymous-send",
        "pane.send_text",
        serde_json::json!({"pane_id": pane, "text": "printf 'PROOF_%s_%s\\n' \"$proof_survivor\" ANON_ACCEPTED\n"}),
    );
    assert_eq!(anonymous["result"]["type"], "ok", "{anonymous}");
    assert_eq!(
        last_input(&api, &pane, false),
        Value::Null,
        "accepted anonymous API input clears the last sender"
    );
    assert_eq!(last_input(&api, &pane, true), Value::Null);
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(20), || {
            screens_contain_marker(&outputs, "PROOF_ALIVE_ANON_ACCEPTED")
        }),
        "anonymous bytes must reach the preserved shell and both attached clients; {}",
        screen_marker_diagnostics(&outputs, "PROOF_ALIVE_ANON_ACCEPTED")
    );
    println!("3937 anonymous response={anonymous} pane={pane} result={{\"type\":\"pane_last_input\",\"last_input\":null}}");
    assert_eq!(
        last_input(&api, &pane, false),
        Value::Null,
        "output/read/render do not restore attribution"
    );

    let restored_a = type_and_observe(
        &api,
        &pane,
        &mut *input_a,
        &outputs,
        "printf 'PROOF_%s_%s\\n' \"$proof_survivor\" ALICE_RESTORED",
        "PROOF_ALIVE_ALICE_RESTORED",
        "alice",
    );
    assert_eq!(restored_a["client_id"], a1["client_id"]);
    let restored_b = type_and_observe(
        &api,
        &pane,
        &mut *input_b,
        &outputs,
        "printf 'PROOF_%s_%s\\n' \"$proof_survivor\" BOB_RESTORED",
        "PROOF_ALIVE_BOB_RESTORED",
        "bob",
    );
    assert_eq!(restored_b["client_id"], b1["client_id"]);
    assert!(
        alice.child.try_wait().unwrap().is_none(),
        "Alice remains attached"
    );
    assert!(
        bob.child.try_wait().unwrap().is_none(),
        "Bob remains attached"
    );
    assert!(
        server.child.try_wait().unwrap().is_none(),
        "server survives anonymous input"
    );
    println!("3937 PASS two real binary TUI clients; A->B->A; anonymous clears attribution; restored Alice/Bob IDs; no Pi proof claimed");
    drop(input_a);
    drop(input_b);
    drop(alice);
    drop(bob);
    drop(server);
    drop(scratch);
}
