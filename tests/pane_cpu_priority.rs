//! Panes start at normal CPU priority even when the server runs idle (smarty-dev#11237).
//!
//! A real server is started under `chrt` and `nice`; panes are created and
//! inspected through the `herdr` CLI, and the pane shell's scheduling policy
//! and nice value are read from `/proc/<pid>/stat`.

#![cfg(target_os = "linux")]

pub mod support;
#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const SCHED_OTHER: i64 = 0;
const SCHED_BATCH: i64 = 3;
const SCHED_IDLE: i64 = 5;

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hprio-{label}-{}-{nanos}", std::process::id()))
}

/// Whether `chrt` (util-linux) is installed; prints why the test is skipped if not.
fn chrt_available() -> bool {
    let available = Command::new("chrt")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!(
            "SKIP: chrt (util-linux) is not installed; cannot start a server at idle priority"
        );
    }
    available
}

/// Whether this host lets an unprivileged process under `wrapper` run `probe`.
fn permitted_under(wrapper: &[&str], probe: &str) -> bool {
    Command::new(wrapper[0])
        .args(&wrapper[1..])
        .args(["sh", "-c", probe])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct Server {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    base: PathBuf,
    config_home: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn start(label: &str, wrapper: &[&str], config: &str) -> Self {
        let base = unique_test_dir(label);
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket = runtime_dir.join("herdr.sock");
        fs::create_dir_all(config_home.join("herdr-dev")).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        register_runtime_dir(&runtime_dir);
        fs::write(
            config_home.join("herdr-dev/config.toml"),
            format!("onboarding = false\n{config}"),
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
        let mut cmd = CommandBuilder::new(wrapper[0]);
        test_command::sanitize_pty_command_env(&mut cmd);
        cmd.args(&wrapper[1..]);
        cmd.arg(env!("CARGO_BIN_EXE_herdr"));
        cmd.arg("server");
        cmd.env("XDG_CONFIG_HOME", &config_home);
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
        cmd.env("HERDR_SOCKET_PATH", &socket);
        cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
        cmd.env_remove("SSH_AUTH_SOCK");
        cmd.env("SHELL", "/bin/sh");
        cmd.env_remove("HERDR_ENV");
        // `chrt` and `nice` exec herdr, so the pid is the server's.
        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);

        let deadline = Instant::now() + Duration::from_secs(15);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "server socket did not appear");
            thread::sleep(Duration::from_millis(25));
        }
        Self {
            _master: pair.master,
            child,
            base,
            config_home,
            socket,
        }
    }

    fn cli(&self, args: &[&str]) -> serde_json::Value {
        let output = test_command::herdr_command()
            .args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env_remove("SSH_AUTH_SOCK")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "herdr {args:?} printed non-JSON ({err}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Create a workspace through the CLI and return its pane shell's pid.
    fn create_pane_shell(&self) -> u32 {
        let cwd = self.base.display().to_string();
        let created = self.cli(&["workspace", "create", "--cwd", &cwd, "--no-focus"]);
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no root pane in {created}"))
            .to_string();
        let info = self.cli(&["pane", "process-info", "--pane", &pane_id]);
        info["result"]["process_info"]["shell_pid"]
            .as_u64()
            .unwrap_or_else(|| panic!("no shell pid in {info}")) as u32
    }

    fn pid(&self) -> u32 {
        self.child.process_id().expect("server pid")
    }

    fn log(&self) -> String {
        fs::read_to_string(self.config_home.join("herdr-dev/herdr-server.log")).unwrap_or_default()
    }

    fn wait_for_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = self.log();
            if log.contains(needle) || Instant::now() >= deadline {
                return log;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
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
        }
        unregister_spawned_herdr_pid(pid);
        cleanup_test_base(&self.base);
    }
}

/// `(policy, nice)` of a process's main thread from `/proc/<pid>/stat`.
fn policy_and_nice(pid: u32) -> (i64, i64) {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // Fields after the parenthesised comm start at field 3 (state).
    let rest = &stat[stat.rfind(')').expect("comm") + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| fields[n - 3].parse::<i64>().unwrap();
    (field(41), field(19))
}

fn pane_lines<'a>(log: &'a str, pid: u32, message: &str) -> Vec<&'a str> {
    log.lines()
        .filter(|line| line.contains(message) && line.contains(&format!(" pid={pid} ")))
        .collect()
}

#[test]
fn pane_under_idle_server_starts_at_normal_priority_or_logs_the_refusal() {
    let _lock = test_lock();
    if !chrt_available() {
        return;
    }
    let wrapper = ["chrt", "--idle", "0", "nice", "-n", "19"];
    // The kernel lets a SCHED_IDLE task leave the class only within RLIMIT_NICE.
    let leave_idle = permitted_under(&wrapper, "chrt --other -p 0 $$");
    let lower_nice = permitted_under(&wrapper, "renice --priority 0 -p $$");
    let server = Server::start("idle", &wrapper, "");

    let startup = server.wait_for_log("reduced CPU priority");
    assert!(
        startup.contains("herdr server is running at reduced CPU priority")
            && startup.contains("policy=\"SCHED_IDLE\"")
            && startup.contains("CPUSchedulingPolicy="),
        "missing startup warning:\n{startup}"
    );

    let shell = server.create_pane_shell();
    let (policy, nice) = policy_and_nice(shell);
    let log = server.wait_for_log(&format!("pid={shell}"));
    let policy_refusals = pane_lines(&log, shell, "kept the server's CPU scheduling policy");
    let nice_refusals = pane_lines(&log, shell, "kept the server's nice value");

    if leave_idle {
        assert_eq!(policy, SCHED_OTHER, "pane shell policy");
        assert!(policy_refusals.is_empty(), "{policy_refusals:?}");
    } else {
        assert_eq!(
            policy, SCHED_IDLE,
            "the kernel refused, so the shell stays idle"
        );
        assert_eq!(policy_refusals.len(), 1, "one refusal per pane:\n{log}");
        assert!(
            policy_refusals[0].contains("errno=1"),
            "{}",
            policy_refusals[0]
        );
    }
    if lower_nice {
        assert_eq!(nice, 0, "pane shell nice");
        assert!(nice_refusals.is_empty(), "{nice_refusals:?}");
    } else {
        assert_eq!(nice, 19, "the kernel refused, so the shell keeps nice 19");
        assert_eq!(nice_refusals.len(), 1, "one refusal per pane:\n{log}");
        assert!(
            nice_refusals[0].contains("errno=13"),
            "{}",
            nice_refusals[0]
        );
    }
}

#[test]
fn pane_under_batch_server_starts_at_sched_other_and_configured_nice() {
    let _lock = test_lock();
    if !chrt_available() {
        return;
    }
    // Leaving SCHED_BATCH and raising a nice value never need privileges.
    let wrapper = ["chrt", "--batch", "0", "nice", "-n", "5"];
    let server = Server::start("batch", &wrapper, "[server]\npane_nice = 19\n");

    let startup = server.wait_for_log("reduced CPU priority");
    assert!(
        startup.contains("herdr server is running at reduced CPU priority")
            && startup.contains("policy=\"SCHED_BATCH\""),
        "missing startup warning:\n{startup}"
    );
    // The server could leave SCHED_BATCH itself.
    assert_ne!(
        policy_and_nice(server.pid()).0,
        SCHED_BATCH,
        "server policy"
    );

    let shell = server.create_pane_shell();
    assert_eq!(
        policy_and_nice(shell),
        (SCHED_OTHER, 19),
        "pane shell (policy, nice)"
    );
    let log = server.wait_for_log(&format!("pid={shell}"));
    assert!(
        pane_lines(&log, shell, "kept the server's").is_empty(),
        "unexpected refusal:\n{log}"
    );
}
