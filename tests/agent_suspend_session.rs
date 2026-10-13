//! smarty-dev#7577: a suspended agent (ctrl-z) keeps its reporter-bound session
//! ID. After `fg` the same live reporter accepts a guarded
//! `herdr agent prompt --expected-session <id>` with no re-report, and a
//! same-reporter `/reload` re-report re-confirms the same ID. A reporter that
//! exits, or a different agent in front, still clears the binding.
//!
//! Regression pin for the inherited detection path that read ctrl-z as an
//! exit: with the pane shell in front and no agent probed, the detector used
//! to report `ProcessExit`, which cleared the binding and recorded the session
//! as stale. The background-job hold (`src/pane/background_agent.rs`) now
//! keeps a stopped job's identity while its process is alive.
//!
//! Everything here runs against a real disposable server through the real CLI.
//! The reporter is a real child process in the pane: a Python script named `pi`
//! (so the server identifies it as Pi) that sends `pane.report_agent_session`
//! itself over the API socket, so the server records it as the live reporter.
#![cfg(target_os = "linux")]

pub mod support;
#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, wait_for_socket,
};

const WAIT: Duration = Duration::from_secs(15);

struct Server {
    base: PathBuf,
    api_socket: PathBuf,
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        unregister_spawned_herdr_pid(pid);
        cleanup_test_base(&self.base);
    }
}

impl Server {
    fn start() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Short: the client socket path must fit a sockaddr_un.
        let base = std::env::temp_dir().join(format!("has-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let api_socket = runtime_dir.join("herdr.sock");
        fs::create_dir_all(config_home.join("herdr")).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            config_home.join("herdr/config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = test_command::herdr_pty_command();
        cmd.arg("server");
        cmd.env("XDG_CONFIG_HOME", &config_home);
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
        cmd.env("HERDR_SOCKET_PATH", &api_socket);
        cmd.env(
            "HERDR_CLIENT_SOCKET_PATH",
            runtime_dir.join("herdr-client.sock"),
        );
        // An interactive POSIX shell with job control: ctrl-z and `fg`.
        cmd.env("SHELL", "/bin/sh");
        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        register_runtime_dir(&runtime_dir);
        wait_for_socket(&api_socket, WAIT);
        Self {
            base,
            api_socket,
            _master: pair.master,
            child,
        }
    }

    fn request(&self, request: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(&self.api_socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream.write_all(request.to_string().as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("{err}: {line:?}"))
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        test_command::herdr_command()
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.base)
            .env("HERDR_SOCKET_PATH", &self.api_socket)
            .output()
            .unwrap()
    }

    fn new_pane(&self) -> String {
        let created = self.request(serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }));
        created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("workspace.create: {created}"))
            .to_string()
    }

    fn type_line(&self, pane_id: &str, text: &str) {
        self.send(pane_id, text, &["Enter"]);
    }

    fn send(&self, pane_id: &str, text: &str, keys: &[&str]) {
        let response = self.request(serde_json::json!({
            "id": "test:pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": text, "keys": keys}
        }));
        assert!(response.get("result").is_some(), "send_input: {response}");
    }

    fn pane(&self, pane_id: &str) -> serde_json::Value {
        let output = self.cli(&["pane", "get", pane_id]);
        assert!(
            output.status.success(),
            "pane get: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["result"]["pane"]
            .clone()
    }

    fn wait_pane(
        &self,
        pane_id: &str,
        what: &str,
        ready: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let pane = self.pane(pane_id);
            if ready(&pane) {
                return pane;
            }
            assert!(Instant::now() < deadline, "{what}: {pane}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// `herdr agent prompt <pane> <text> --expected-session <id>`.
    fn guarded_prompt(&self, pane_id: &str, text: &str, session_id: &str) -> std::process::Output {
        self.cli(&[
            "agent",
            "prompt",
            pane_id,
            text,
            "--expected-session",
            session_id,
        ])
    }

    fn server_log(&self) -> String {
        fs::read_to_string(self.base.join("config/herdr-dev/herdr-server.log")).unwrap_or_default()
    }
}

/// The fake Pi: reports its session over the API socket (so it is the bound
/// reporter), appends each terminal line to `prompts`, and on `RELOAD` re-reports
/// the same session ID the way Pi's `/reload` does.
struct FakePi {
    session_id: String,
    pid: u32,
    prompts: PathBuf,
    reported: PathBuf,
}

const FAKE_PI: &str = r#"import json, os, socket, sys
sock, pane, path, sid, reported, prompts, pidfile = sys.argv[1:8]
seq = 0
def call(method, params):
    global seq
    seq += 1
    params = dict(params, pane_id=pane, source="herdr:pi", agent="pi", seq=seq,
        agent_session_path=path, agent_session_id=sid)
    s = socket.socket(socket.AF_UNIX)
    s.connect(sock)
    s.sendall((json.dumps({"id": "pi:" + method, "method": method, "params": params}) + "\n").encode())
    reply = s.makefile().readline()
    s.close()
    return reply
def report(start):
    # Pi's session_start: report the session, then publish the state, which
    # gives the pane full lifecycle hook authority like the real extension.
    reply = call("pane.report_agent_session", {"session_start_source": start})
    call("pane.report_agent", {"state": "idle"})
    with open(reported, "a") as f:
        f.write(start + " " + reply)
with open(pidfile, "w") as f:
    f.write(str(os.getpid()) + "\n")
report("startup")
for line in sys.stdin:
    if line.strip() == "RELOAD":
        report("reload")
        continue
    with open(prompts, "a") as f:
        f.write(line)
"#;

fn start_fake_pi(server: &Server, pane_id: &str, name: &str) -> FakePi {
    let dir = server.base.join(name);
    fs::create_dir_all(&dir).unwrap();
    let session_id = format!("{name}-session-id");
    let prompts = dir.join("prompts");
    let reported = dir.join("reported");
    let pidfile = dir.join("pid");
    // Named `pi` so the server identifies the process (`python3 <script named pi>`).
    let script = dir.join("pi");
    fs::write(&script, FAKE_PI).unwrap();
    server.type_line(
        pane_id,
        &format!(
            "python3 {} {} {} {} {} {} {} {}",
            script.display(),
            server.api_socket.display(),
            pane_id,
            dir.join("session.jsonl").display(),
            session_id,
            reported.display(),
            prompts.display(),
            pidfile.display(),
        ),
    );
    let reply = wait_for_file(&reported, "startup ", WAIT);
    assert!(
        reply.contains("\"result\""),
        "startup report failed: {reply}"
    );
    let pid = wait_for_file(&pidfile, "\n", WAIT).trim().parse().unwrap();
    server.wait_pane(pane_id, "the startup report binds the id", |pane| {
        pane["agent"] == "pi" && pane["agent_session_id"] == session_id.as_str()
    });
    FakePi {
        session_id,
        pid,
        prompts,
        reported,
    }
}

fn wait_for_file(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never contained {needle:?}: {text:?}",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// `/proc/<pid>/stat` state and whether the process group is the terminal's
/// foreground group (tpgid == pgrp).
fn job_state(pid: u32) -> Option<(char, bool)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let pgrp: i64 = fields.get(2)?.parse().ok()?;
    let tpgid: i64 = fields.get(5)?.parse().ok()?;
    Some((state, pgrp == tpgid))
}

fn wait_job(pid: u32, what: &str, ready: impl Fn(Option<(char, bool)>) -> bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = job_state(pid);
        if ready(state) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {state:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// ctrl-z: the shell stops the agent's foreground job and takes the terminal.
fn suspend(server: &Server, pane_id: &str, pi: &FakePi) {
    server.send(pane_id, "", &["ctrl+z"]);
    wait_job(
        pi.pid,
        "the reporter is stopped behind the shell",
        |state| state == Some(('T', false)),
    );
}

/// `fg`: the same process is continued and is the foreground job again.
fn resume(server: &Server, pane_id: &str, pi: &FakePi) {
    server.type_line(pane_id, "fg");
    wait_job(
        pi.pid,
        "the reporter is back in the foreground",
        |state| matches!(state, Some((state, true)) if state != 'T'),
    );
}

/// Waits until the server's foreground probe has observed the agent job move
/// `count` times to (`true`) or from (`false`) the background. Each test server
/// has a single pane, so the log line is unambiguous.
fn wait_for_job_observed(server: &Server, in_background: bool, count: usize) {
    let needle =
        format!("agent job foreground changed pane=1 agent=Some(Pi) in_background={in_background}");
    let deadline = Instant::now() + WAIT;
    loop {
        let log = server.server_log();
        if log.matches(&needle).count() >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the server never logged {needle:?} x{count}: {}",
            log_tail(&log)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Refused before any input: the bound session is unknown, or (once the agent
/// itself is gone from the pane) the agent target no longer exists.
fn assert_refused(output: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the guard accepted: {stderr}");
    assert!(
        stderr.contains("agent_session_unknown") || stderr.contains("agent_not_found"),
        "{stderr}"
    );
}

fn assert_delivered(server: &Server, pane_id: &str, pi: &FakePi, text: &str) {
    let output = server.guarded_prompt(pane_id, text, &pi.session_id);
    assert!(
        output.status.success(),
        "guarded prompt {text:?}: {}\npane: {}\nlog tail: {}",
        String::from_utf8_lossy(&output.stderr),
        server.pane(pane_id),
        log_tail(&server.server_log()),
    );
    wait_for_file(&pi.prompts, text, WAIT);
}

fn log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}

#[test]
fn suspended_reporter_keeps_its_session_binding_after_fg() {
    let server = Server::start();
    let pane_id = server.new_pane();
    let pi = start_fake_pi(&server, &pane_id, "suspended");
    assert_delivered(&server, &pane_id, &pi, "before-suspend");

    suspend(&server, &pane_id, &pi);
    wait_for_job_observed(&server, true, 1);
    // The shell, not the agent, owns the terminal: the guard refuses.
    let while_stopped = server.guarded_prompt(&pane_id, "while-stopped", &pi.session_id);
    assert!(
        !while_stopped.status.success(),
        "a guarded prompt reached a stopped agent"
    );
    // The stopped job is alive, so its binding is still recorded.
    let pane = server.pane(&pane_id);
    assert_eq!(pane["agent_session_id"], pi.session_id.as_str(), "{pane}");

    resume(&server, &pane_id, &pi);
    wait_for_job_observed(&server, false, 1);
    // The same live reporter, with no new report: the same ID is accepted.
    assert_delivered(&server, &pane_id, &pi, "after-fg");
    assert!(
        !fs::read_to_string(&pi.prompts)
            .unwrap()
            .contains("while-stopped"),
        "the refused prompt was typed somewhere"
    );

    // Pi's /reload re-reports the same ID from the same process: it re-confirms
    // the binding instead of hitting the stale-session gate.
    server.type_line(&pane_id, "RELOAD");
    let reported = wait_for_file(&pi.reported, "reload ", WAIT);
    assert!(
        reported
            .lines()
            .any(|line| line.starts_with("reload ") && line.contains("\"result\"")),
        "{reported}"
    );
    server.wait_pane(&pane_id, "the reload keeps the id", |pane| {
        pane["agent_session_id"] == pi.session_id.as_str()
    });
    assert_delivered(&server, &pane_id, &pi, "after-reload");
}

#[test]
fn reporter_killed_while_suspended_clears_its_session_binding() {
    let server = Server::start();
    let pane_id = server.new_pane();
    let pi = start_fake_pi(&server, &pane_id, "killed");

    suspend(&server, &pane_id, &pi);
    wait_for_job_observed(&server, true, 1);
    let pane = server.pane(&pane_id);
    assert_eq!(pane["agent_session_id"], pi.session_id.as_str(), "{pane}");
    // SAFETY: plain kill(2) on the test's own child process.
    assert_eq!(
        unsafe { libc::kill(pi.pid as libc::pid_t, libc::SIGKILL) },
        0
    );
    wait_job(pi.pid, "the reporter is gone", |state| {
        state.is_none_or(|(state, _)| state == 'Z' || state == 'X')
    });
    server.wait_pane(&pane_id, "a dead reporter's binding is cleared", |pane| {
        pane["agent_session_id"].is_null()
    });
    assert_refused(&server.guarded_prompt(&pane_id, "after-exit", &pi.session_id));
}

#[test]
fn reporter_exit_in_front_clears_its_session_binding() {
    let server = Server::start();
    let pane_id = server.new_pane();
    let pi = start_fake_pi(&server, &pane_id, "exited");
    suspend(&server, &pane_id, &pi);
    wait_for_job_observed(&server, true, 1);
    resume(&server, &pane_id, &pi);
    wait_for_job_observed(&server, false, 1);

    // ctrl-d: end of input, the reporter exits normally in the foreground.
    server.send(&pane_id, "", &["ctrl+d"]);
    wait_job(pi.pid, "the reporter exited", |state| {
        state.is_none_or(|(state, _)| state == 'Z' || state == 'X')
    });
    server.wait_pane(
        &pane_id,
        "an exited reporter's binding is cleared",
        |pane| pane["agent_session_id"].is_null(),
    );
    assert_refused(&server.guarded_prompt(&pane_id, "after-exit", &pi.session_id));
}

#[test]
fn different_agent_in_front_of_a_suspended_reporter_clears_its_binding() {
    let server = Server::start();
    let pane_id = server.new_pane();
    let pi = start_fake_pi(&server, &pane_id, "displaced");

    suspend(&server, &pane_id, &pi);
    wait_for_job_observed(&server, true, 1);
    // A different agent (a process named `claude`) takes the foreground while
    // the reporter is still alive and stopped.
    let claude = server.base.join("displaced/claude");
    fs::write(&claude, "import sys\nfor _ in sys.stdin:\n    pass\n").unwrap();
    server.type_line(&pane_id, &format!("python3 {}", claude.display()));
    server.wait_pane(&pane_id, "the different agent is detected", |pane| {
        pane["agent"] == "claude"
    });
    let pane = server.wait_pane(&pane_id, "the binding is cleared", |pane| {
        pane["agent_session_id"].is_null()
    });
    assert_eq!(
        job_state(pi.pid).map(|(state, _)| state),
        Some('T'),
        "{pane}"
    );
    assert_refused(&server.guarded_prompt(&pane_id, "to-claude", &pi.session_id));
    assert!(!fs::read_to_string(&pi.prompts)
        .unwrap_or_default()
        .contains("to-claude"));
}
