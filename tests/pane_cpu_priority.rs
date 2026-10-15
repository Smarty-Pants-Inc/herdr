//! Panes start at normal CPU priority even when the server runs idle (smarty-dev#11237).
//!
//! A real server is started in an idle or batch class that the test sets
//! in-process (no `chrt` or `nice` binary, so the tests never skip); panes are
//! created and inspected through the `herdr` CLI, and the pane shell's
//! scheduling policy and nice value are read from `/proc/<pid>/stat`.

#![cfg(target_os = "linux")]

pub mod support;
#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const SCHED_OTHER: i64 = libc::SCHED_OTHER as i64;
const SCHED_BATCH: i64 = libc::SCHED_BATCH as i64;
const SCHED_IDLE: i64 = libc::SCHED_IDLE as i64;

/// The reduced CPU class a test server starts in.
#[derive(Clone, Copy)]
struct Class {
    policy: libc::c_int,
    nice: libc::c_int,
}

const IDLE_19: Class = Class {
    policy: libc::SCHED_IDLE,
    nice: 19,
};
const BATCH_5: Class = Class {
    policy: libc::SCHED_BATCH,
    nice: 5,
};

/// `class` with its nice raised to this thread's nice when that is higher, as
/// `nice -n` would: an unprivileged child cannot lower the value it inherits.
fn from_here(class: Class) -> Class {
    // SAFETY: errno is thread-local; getpriority reads the calling thread.
    let current = unsafe {
        *libc::__errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS, 0)
    };
    let err = io::Error::last_os_error();
    assert!(
        current != -1 || err.raw_os_error() == Some(0),
        "getpriority: {err}"
    );
    Class {
        nice: class.nice.max(current),
        ..class
    }
}

/// Put the calling (forked, single-threaded) child in `class`.
///
/// Runs between fork and exec, so it calls only async-signal-safe syscalls.
fn enter_class(class: Class) -> io::Result<()> {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: plain syscalls on the calling process with a valid pointer.
    unsafe {
        if libc::sched_setscheduler(0, class.policy, &param) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::setpriority(libc::PRIO_PROCESS, 0, class.nice) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Makes `command` start in `class`; `spawn` fails with the errno if the
/// kernel refuses, so a test that cannot set up its class fails, never skips.
fn in_class(command: &mut Command, class: Class) -> &mut Command {
    let class = from_here(class);
    // SAFETY: the hook calls only async-signal-safe syscalls.
    unsafe { command.pre_exec(move || enter_class(class)) }
}

/// What an unprivileged process in `class` may do to itself on this host.
#[derive(Clone, Copy)]
enum Probe {
    /// Leave the class for SCHED_OTHER.
    LeavePolicy,
    /// Lower its nice value to 0.
    LowerNice,
}

const PROBE_REFUSED: i32 = 86;

/// Whether a child in `class` may do `probe`. Panics if the class itself
/// cannot be set.
fn permitted_in(class: Class, probe: Probe) -> bool {
    let class = from_here(class);
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the hook calls only async-signal-safe syscalls and `_exit`.
    unsafe {
        command.pre_exec(move || {
            enter_class(class)?;
            let param = libc::sched_param { sched_priority: 0 };
            let result = match probe {
                Probe::LeavePolicy => libc::sched_setscheduler(0, libc::SCHED_OTHER, &param),
                Probe::LowerNice => libc::setpriority(libc::PRIO_PROCESS, 0, 0),
            };
            if result != 0 {
                libc::_exit(PROBE_REFUSED);
            }
            Ok(())
        });
    }
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("cannot start a probe child in the test class: {err}"));
    match status.code() {
        Some(0) => true,
        Some(PROBE_REFUSED) => false,
        _ => panic!(
            "probe child ended unexpectedly: {status:?} (signal {:?})",
            status.signal()
        ),
    }
}

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

struct Server {
    child: Child,
    base: PathBuf,
    config_home: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn start(label: &str, class: Class, config: &str) -> Self {
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

        let mut cmd = test_command::herdr_command();
        cmd.arg("server")
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("HERDR_SOCKET_PATH", &socket)
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("SSH_AUTH_SOCK")
            .env("SHELL", "/bin/sh")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = in_class(&mut cmd, class)
            .spawn()
            .unwrap_or_else(|err| panic!("cannot start the server in the test class: {err}"));
        register_spawned_herdr_pid(Some(child.id()));

        let deadline = Instant::now() + Duration::from_secs(15);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "server socket did not appear");
            thread::sleep(Duration::from_millis(25));
        }
        Self {
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
        self.child.id()
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
        let pid = self.child.id();
        // SAFETY: signals only the server child this test spawned.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unregister_spawned_herdr_pid(Some(pid));
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
    // The kernel lets a SCHED_IDLE task leave the class only within RLIMIT_NICE.
    let leave_idle = permitted_in(IDLE_19, Probe::LeavePolicy);
    let lower_nice = permitted_in(IDLE_19, Probe::LowerNice);
    let server = Server::start("idle", IDLE_19, "");

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
    // Leaving SCHED_BATCH and raising a nice value never need privileges.
    let server = Server::start("batch", BATCH_5, "[server]\npane_nice = 19\n");

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
