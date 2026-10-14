//! Real-binary E2E for `[server] systemd_unit` autostart on the remote-host bridge path.
//!
//! The remote host runs `herdr remote-client-bridge` over SSH stdio; when no server is
//! listening it must start one. These tests run the real binary against an isolated
//! fixture (HOME, XDG dirs, `HERDR_CONFIG_PATH`, `HERDR_SOCKET_PATH` all under one unique
//! directory below `std::env::temp_dir()`), with a fake `systemctl` first on the child's
//! PATH. The fake records its argv and, in success mode, launches the real `herdr server`
//! itself, so any daemon on the fixture socket is attributable to exactly one launcher.
//! No live config, socket, systemd user manager or unit is touched.

#![cfg(target_os = "linux")]

#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UNIT: &str = "herdr-e2e.service";
const FAKE_PID_ENV: &str = "LUNIT_FAKE_SYSTEMCTL_PID";

enum Fake {
    Succeeds,
    Fails,
    /// Records its own PID, then `exec sleep` in the same process (no grandchildren).
    Hangs,
}

/// One owned fixture. Drop stops only servers pinned by pidfd whose environment names this
/// fixture's unique socket, then the bridge's own process group while the bridge is still
/// unreaped, then removes only this fixture directory.
///
/// Expected skip: cleanup of the detached (`setsid`) daemon is only safe through a pidfd,
/// so every process fixture requires `pidfd_open` + `pidfd_send_signal` (Linux 5.3+; every
/// fleet Linux host has them). Without them `Fixture::new` prints a `SKIP:` line and returns
/// `None` before creating files or spawning anything, and the test returns early. Rust has
/// no dynamic skip, so such a test reports `ok`; the `SKIP:` line is the honest record that
/// it proved nothing there. There is deliberately no PID/session/group fallback.
struct Fixture {
    dir: PathBuf,
    bridge: Option<Child>,
}

/// Opens a pidfd; injectable only so a test can force the capability preflight to fail.
type PidfdOpener = fn(u32) -> std::io::Result<OwnedFd>;

/// Capability preflight on this test process itself: a pidfd for our own PID plus a
/// signal-0 probe through it (signal 0 delivers nothing). Failure here means the kernel or
/// sandbox lacks pidfd support, unlike a later per-candidate `ESRCH`, which only means that
/// candidate already exited.
fn pidfd_preflight(opener: PidfdOpener) -> std::io::Result<()> {
    let fd = opener(std::process::id())?;
    pidfd_signal(&fd, 0)
}

impl Fixture {
    /// `None` (after a `SKIP:` line) when pidfds are unavailable; see the type docs.
    fn new(systemd_unit: Option<&str>, fake: Fake) -> Option<Self> {
        Self::new_in(&std::env::temp_dir(), systemd_unit, fake, pidfd_open)
    }

    fn new_in(
        root: &Path,
        systemd_unit: Option<&str>,
        fake: Fake,
        opener: PidfdOpener,
    ) -> Option<Self> {
        // Before any fixture file, server or bridge exists.
        if let Err(err) = pidfd_preflight(opener) {
            eprintln!(
                "SKIP: pidfd_open/pidfd_send_signal unavailable ({err}); managed-unit process \
                 fixtures need pidfd-only cleanup, nothing was created, spawned or signalled"
            );
            return None;
        }
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = root.join(format!(
            "lmu-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        // create_dir (not create_dir_all) fails if the path exists, so the fixture is ours.
        fs::create_dir(&dir).expect("create unique fixture dir");
        for sub in ["bin", "home", "cfg", "state", "data", "cache", "r"] {
            fs::create_dir(dir.join(sub)).unwrap();
        }
        fs::set_permissions(dir.join("r"), fs::Permissions::from_mode(0o700)).unwrap();

        let mut config = String::from("onboarding = false\n");
        if let Some(unit) = systemd_unit {
            config.push_str(&format!("\n[server]\nsystemd_unit = {unit:?}\n"));
        }
        fs::write(dir.join("config.toml"), config).unwrap();

        let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let action = match fake {
            Fake::Fails => {
                "printf 'Failed to start %s: Unit %s not found.\\n' \"$4\" \"$4\" >&2\nexit 5\n"
                    .to_string()
            }
            Fake::Hangs => format!(
                "printf '%s\\n' \"$$\" > {}\nexec sleep 60\n",
                q(&dir.join("hang.pid"))
            ),
            Fake::Succeeds => format!(
                "{FAKE_PID_ENV}=$$\nexport {FAKE_PID_ENV}\nprintf '%s\\n' \"$$\" > {}\n{} server </dev/null >>{} 2>&1 &\nprintf '%s\\n' \"$!\" > {}\nexit 0\n",
                q(&dir.join("fake-systemctl.pid")),
                q(Path::new(env!("CARGO_BIN_EXE_herdr"))),
                q(&dir.join("server.log")),
                q(&dir.join("fake-server.pid")),
            ),
        };
        let script = format!(
            "#!/bin/sh\n{{ for a do printf '%s\\n' \"$a\"; done; echo ---; }} >> {}\n{action}",
            q(&dir.join("systemctl.argv")),
        );
        let fake_path = dir.join("bin/systemctl");
        fs::write(&fake_path, script).unwrap();
        fs::set_permissions(&fake_path, fs::Permissions::from_mode(0o700)).unwrap();

        Some(Self { dir, bridge: None })
    }

    fn api_socket(&self) -> PathBuf {
        self.dir.join("r/herdr.sock")
    }

    fn client_socket(&self) -> PathBuf {
        self.dir.join("r/herdr-client.sock")
    }

    /// A real `herdr` command with this fixture's isolated environment.
    fn command(&self) -> Command {
        let mut command = test_command::herdr_command();
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .current_dir(self.dir.join("home"))
            .env("PATH", path)
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("cfg"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("XDG_RUNTIME_DIR", self.dir.join("r"))
            .env("HERDR_CONFIG_PATH", self.dir.join("config.toml"))
            .env("HERDR_SOCKET_PATH", self.api_socket())
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove(FAKE_PID_ENV);
        command
    }

    /// The remote-host command `herdr --remote` runs over SSH, in its own process group.
    fn spawn_bridge(&mut self) {
        let mut command = self.command();
        command
            .arg("remote-client-bridge")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.bridge = Some(
            command
                .spawn()
                .expect("spawn real herdr remote-client-bridge"),
        );
    }

    fn bridge_pid(&self) -> u32 {
        self.bridge.as_ref().map(Child::id).unwrap()
    }

    /// For flows that must fail: the bridge exits on its own within the deadline.
    fn wait_bridge_exit(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(15);
        let child = self.bridge.as_mut().unwrap();
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "bridge did not exit");
            thread::sleep(Duration::from_millis(25));
        }
        self.bridge.take().unwrap().wait_with_output().unwrap()
    }

    /// Each fake invocation as its argv, in order.
    fn systemctl_calls(&self) -> Vec<Vec<String>> {
        let Ok(log) = fs::read_to_string(self.dir.join("systemctl.argv")) else {
            return Vec::new();
        };
        log.split_terminator("---\n")
            .map(|call| call.lines().map(str::to_owned).collect())
            .collect()
    }

    /// Whether the process now at `pid` carries this fixture's unique socket marker.
    fn has_marker(&self, pid: u32) -> bool {
        let needle = format!("HERDR_SOCKET_PATH={}", self.api_socket().display());
        fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|environ| environ.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()))
    }

    /// Whether the process now at `pid` is a `herdr server` with this fixture's marker.
    fn is_fixture_server(&self, pid: u32) -> bool {
        fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|cmdline| cmdline.split(|b| *b == 0).nth(1) == Some(&b"server"[..]))
            && self.has_marker(pid)
    }

    /// Inventory only: numeric PIDs of this fixture's servers. Never signal these directly.
    fn fixture_servers(&self) -> Vec<u32> {
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| self.is_fixture_server(pid))
            .collect()
    }

    /// Pins a scanned candidate with a pidfd, then re-reads its marker. The pinned process
    /// still being alive after the re-read proves the read was of the pinned process, not a
    /// PID recycled in between; anything else is not ours. Capability was proven by the
    /// preflight, so an open failure here is per candidate (normally `ESRCH`: it already
    /// exited) and the candidate is never signalled.
    fn pin_fixture_server(&self, pid: u32) -> Option<OwnedFd> {
        let fd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => return None,
            Err(err) => {
                eprintln!("fixture cleanup: pid {pid} not pinned ({err}); not signalled");
                return None;
            }
        };
        (self.is_fixture_server(pid) && pidfd_signal(&fd, 0).is_ok()).then_some(fd)
    }

    /// Stops the pinned fixture servers among `candidates`, signalling only through their
    /// pidfds. Returns the candidates it owned.
    fn stop_fixture_servers(&self, candidates: &[u32]) -> Vec<u32> {
        let pinned: Vec<(u32, OwnedFd)> = candidates
            .iter()
            .filter_map(|&pid| Some((pid, self.pin_fixture_server(pid)?)))
            .collect();
        for (_, fd) in &pinned {
            let _ = pidfd_signal(fd, libc::SIGTERM);
        }
        for (_, fd) in &pinned {
            if !pidfd_wait_exit(fd, Duration::from_secs(3)) {
                let _ = pidfd_signal(fd, libc::SIGKILL);
            }
        }
        pinned.into_iter().map(|(pid, _)| pid).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_fixture_servers(&self.fixture_servers());
        if let Some(mut bridge) = self.bridge.take() {
            // try_wait reaps an exited bridge; Ok(None) means it is unreaped, so its PID and
            // therefore its process group ID cannot have been recycled.
            if let Ok(None) = bridge.try_wait() {
                unsafe { libc::killpg(bridge.id() as libc::pid_t, libc::SIGKILL) };
            }
            let _ = bridge.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Pins the process now holding `pid`; the pidfd keeps targeting it after exit or PID reuse.
fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Signals exactly the pinned process (0 probes it). Fails with ESRCH once it is reaped.
fn pidfd_signal(fd: &OwnedFd, signal: i32) -> std::io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the pinned process exits (its pidfd polls readable) within `timeout`.
fn pidfd_wait_exit(fd: &OwnedFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
    rc > 0 && pfd.revents & libc::POLLIN != 0
}

fn is_reaped(fd: &OwnedFd) -> bool {
    matches!(pidfd_signal(fd, 0), Err(err) if err.raw_os_error() == Some(libc::ESRCH))
}

/// Real API round trip; returns the `result.type` and the server PID from SO_PEERCRED.
fn ping(socket: &Path) -> (String, u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(err) if Instant::now() >= deadline => {
                panic!("no server at {}: {err}", socket.display())
            }
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    };
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    assert_eq!(rc, 0, "SO_PEERCRED");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"{\"id\":\"e2e\",\"method\":\"ping\",\"params\":{}}\n")
        .unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    (
        response["result"]["type"].as_str().unwrap_or("").to_owned(),
        cred.pid as u32,
    )
}

/// (ppid, session id) from /proc/<pid>/stat.
fn ppid_and_sid(pid: u32) -> (u32, u32) {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
    (fields[1].parse().unwrap(), fields[3].parse().unwrap())
}

/// The value of `key` in /proc/<pid>/environ, if present.
fn environ_value(pid: u32, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    fs::read(format!("/proc/{pid}/environ"))
        .unwrap()
        .split(|b| *b == 0)
        .find_map(|kv| {
            kv.strip_prefix(prefix.as_bytes())
                .map(|v| String::from_utf8_lossy(v).into_owned())
        })
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn configured_unit_is_started_through_systemctl_user_and_bridge_attaches() {
    let Some(mut fx) = Fixture::new(Some(UNIT), Fake::Succeeds) else {
        return;
    };
    fx.spawn_bridge();

    let (reply, peer_pid) = ping(&fx.api_socket());
    let calls = fx.systemctl_calls();
    println!("systemctl argv: {calls:?}");
    assert_eq!(calls, vec![vec!["--user", "--no-block", "start", UNIT]]);
    assert_eq!(reply, "pong");

    let fake_server = read_pid(&fx.dir.join("fake-server.pid"));
    let fake_systemctl = read_pid(&fx.dir.join("fake-systemctl.pid"));
    let marker = environ_value(fake_server, FAKE_PID_ENV);
    let (ppid, sid) = ppid_and_sid(fake_server);
    let (_, bridge_sid) = ppid_and_sid(fx.bridge_pid());
    let servers = fx.fixture_servers();
    println!(
        "pong from pid {peer_pid}; fake-launched pid {fake_server} (ppid {ppid}, sid {sid}); \
         launched by fake systemctl pid {fake_systemctl} (server env marker {marker:?}); \
         bridge pid {} sid {bridge_sid}; fixture servers {servers:?}",
        fx.bridge_pid()
    );
    // The socket's owner is the process the fake launched, the only server for this fixture.
    assert_eq!(peer_pid, fake_server);
    assert_eq!(servers, vec![fake_server], "no detached fallback server");
    // It carries the fake's marker and is not a setsid'd detached daemon of the bridge.
    assert_eq!(marker, Some(fake_systemctl.to_string()));
    assert_eq!(sid, bridge_sid, "server inherited the fake's session");
    assert_ne!(sid, fake_server, "server is not a detached session leader");

    // The API can answer before the client listener is bound; wait for the
    // same readiness the real bridge waits for before checking its attachment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(fx.client_socket()).is_err() {
        assert!(
            Instant::now() < deadline,
            "client socket did not become ready"
        );
        thread::sleep(Duration::from_millis(25));
    }
    // The bridge is still attached (forwarding stdio), not exited with an error.
    let bridge = fx.bridge.as_mut().unwrap();
    assert!(
        bridge.try_wait().unwrap().is_none(),
        "bridge stays attached"
    );
    assert!(fx.client_socket().exists());
}

#[test]
fn failed_unit_start_reports_unit_and_launches_nothing() {
    let Some(mut fx) = Fixture::new(Some(UNIT), Fake::Fails) else {
        return;
    };
    fx.spawn_bridge();
    let output = fx.wait_bridge_exit();
    let stderr = stderr_of(&output);
    let calls = fx.systemctl_calls();
    println!(
        "systemctl argv: {calls:?}\nbridge status: {}\nbridge stderr: {stderr}",
        output.status
    );

    assert_eq!(calls, vec![vec!["--user", "--no-block", "start", UNIT]]);
    assert!(!output.status.success());
    // The error names the systemctl action and unit, says it failed, and carries the
    // fake's own failure detail, so a bare mention of the unit is not enough.
    assert!(
        stderr.contains(&format!("systemctl --user --no-block start {UNIT}")),
        "error must name the systemctl action and unit: {stderr}"
    );
    assert!(
        stderr.contains("failed"),
        "error must report failure: {stderr}"
    );
    assert!(
        stderr.contains(&format!("Unit {UNIT} not found")),
        "error must carry systemctl's failure detail: {stderr}"
    );
    assert!(
        fx.fixture_servers().is_empty(),
        "no detached fallback server"
    );
    assert!(!fx.api_socket().exists() && !fx.client_socket().exists());
}

#[test]
fn invalid_config_with_unit_fails_closed_instead_of_detached_defaults() {
    // A managed unit plus an unrelated wrong-typed field: Config::load rejects the whole
    // file and falls back to defaults (no unit), which must not become a detached launch.
    let Some(mut fx) = Fixture::new(Some(UNIT), Fake::Succeeds) else {
        return;
    };
    let config_path = fx.dir.join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[update]\nversion_check = \"yes\"\n");
    fs::write(&config_path, config).unwrap();
    fx.spawn_bridge();
    let output = fx.wait_bridge_exit();
    let stderr = stderr_of(&output);
    println!(
        "systemctl argv: {:?}\nbridge status: {}\nbridge stderr: {stderr}",
        fx.systemctl_calls(),
        output.status
    );

    assert!(!output.status.success());
    assert!(
        stderr.contains("config")
            && (stderr.contains(&config_path.display().to_string())
                || stderr.contains("version_check")),
        "error must identify the invalid config: {stderr}"
    );
    assert!(fx.systemctl_calls().is_empty(), "systemctl must not run");
    assert!(!fx.dir.join("fake-server.pid").exists());
    assert!(
        fx.fixture_servers().is_empty(),
        "no detached fallback server"
    );
    assert!(!fx.api_socket().exists() && !fx.client_socket().exists());
}

#[test]
fn without_configured_unit_launch_stays_detached_and_skips_systemctl() {
    let Some(mut fx) = Fixture::new(None, Fake::Succeeds) else {
        return;
    };
    fx.spawn_bridge();

    let (reply, peer_pid) = ping(&fx.api_socket());
    let servers = fx.fixture_servers();
    let (ppid, sid) = ppid_and_sid(peer_pid);
    println!(
        "systemctl argv: {:?}; pong from pid {peer_pid} (ppid {ppid}, sid {sid}); \
         bridge pid {}; fixture servers {servers:?}",
        fx.systemctl_calls(),
        fx.bridge_pid()
    );
    assert_eq!(reply, "pong");
    assert!(fx.systemctl_calls().is_empty(), "systemctl must not run");
    assert!(!fx.dir.join("fake-server.pid").exists());
    assert_eq!(servers, vec![peer_pid]);
    // The existing detached launch: setsid'd session leader without the fake's marker.
    assert_eq!(sid, peer_pid, "detached daemon leads its own session");
    assert_eq!(environ_value(peer_pid, FAKE_PID_ENV), None);
}

#[test]
fn invalid_unit_name_is_rejected_before_any_launch() {
    let bad = "../herdr.service";
    let Some(mut fx) = Fixture::new(Some(bad), Fake::Succeeds) else {
        return;
    };
    fx.spawn_bridge();
    let output = fx.wait_bridge_exit();
    let stderr = stderr_of(&output);
    println!(
        "systemctl argv: {:?}\nbridge status: {}\nbridge stderr: {stderr}",
        fx.systemctl_calls(),
        output.status
    );

    assert!(!output.status.success());
    assert!(
        stderr.contains(bad) || stderr.contains("systemd_unit"),
        "error must identify the rejected setting: {stderr}"
    );
    assert!(fx.systemctl_calls().is_empty(), "systemctl must not run");
    assert!(
        fx.fixture_servers().is_empty(),
        "no detached fallback server"
    );
    assert!(!fx.api_socket().exists() && !fx.client_socket().exists());
}

#[test]
fn hung_unit_start_times_out_and_kills_and_reaps_systemctl() {
    let Some(mut fx) = Fixture::new(Some(UNIT), Fake::Hangs) else {
        return;
    };
    let spawned = Instant::now();
    fx.spawn_bridge();

    // The fake records its own PID, then becomes `sleep 60` in place.
    let pid_file = fx.dir.join("hang.pid");
    let hang_pid = loop {
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            spawned.elapsed() < Duration::from_secs(10),
            "fake never ran"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let pinned_at = Instant::now();
    let fd = pidfd_open(hang_pid).expect("pin hung fake systemctl");
    assert!(
        fx.has_marker(hang_pid) && pidfd_signal(&fd, 0).is_ok(),
        "pinned process is this fixture's live fake systemctl"
    );

    let exited = pidfd_wait_exit(&fd, Duration::from_secs(10));
    let exit_after = pinned_at.elapsed();
    if !exited {
        // Owned, pinned and marker-checked: stop it through the pidfd only.
        let _ = pidfd_signal(&fd, libc::SIGKILL);
    }
    let bridge_state = fs::read_to_string(format!("/proc/{}/stat", fx.bridge_pid()));
    let reaped_deadline = Instant::now() + Duration::from_secs(2);
    while !is_reaped(&fd) && Instant::now() < reaped_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    let reaped = is_reaped(&fd);
    let output = fx.wait_bridge_exit();
    let total = spawned.elapsed();
    let stderr = stderr_of(&output);
    println!(
        "systemctl argv: {:?}\nhung pid {hang_pid}: exited {exited} after {exit_after:?} \
         from pin, reaped {reaped}; bridge stat when reaped-check began: {bridge_state:?}\n\
         bridge status {} after {total:?}\nbridge stderr: {stderr}",
        fx.systemctl_calls(),
        output.status
    );

    assert_eq!(
        fx.systemctl_calls(),
        vec![vec!["--user", "--no-block", "start", UNIT]]
    );
    assert!(exited, "hung systemctl was not killed");
    assert!(reaped, "killed systemctl was not reaped");
    // The helper's deadline is 5s; the ceiling leaves room for a loaded host.
    assert!(
        exit_after >= Duration::from_secs(4) && total < Duration::from_secs(10),
        "timeout should fire around 5s: killed after {exit_after:?}, bridge exit after {total:?}"
    );
    assert!(!output.status.success());
    assert!(
        stderr.contains(&format!("systemctl --user --no-block start {UNIT}")),
        "error must name the systemctl action and unit: {stderr}"
    );
    let lower = stderr.to_lowercase();
    assert!(
        lower.contains("within 5s") || lower.contains("timed out") || lower.contains("timeout"),
        "error must report the timeout: {stderr}"
    );
    assert!(!fx.dir.join("fake-server.pid").exists());
    assert!(
        fx.fixture_servers().is_empty(),
        "no detached fallback server"
    );
    assert!(!fx.api_socket().exists() && !fx.client_socket().exists());
}

#[test]
fn cleanup_stops_pinned_fixture_server_and_spares_foreign_candidate() {
    // Owned: the real detached daemon of the no-unit flow (its own session, not in the
    // bridge's group, so only a pidfd can stop it safely).
    let Some(mut fx) = Fixture::new(None, Fake::Succeeds) else {
        return;
    };
    fx.spawn_bridge();
    let (reply, server) = ping(&fx.api_socket());
    assert_eq!(reply, "pong");
    assert_eq!(fx.fixture_servers(), vec![server]);

    // Foreign: a live real `herdr server` with the same argv shape but another fixture's
    // marker, owned by this test as a direct child.
    let Some(other) = Fixture::new(None, Fake::Succeeds) else {
        return;
    };
    let mut foreign = other
        .command()
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreign herdr server");
    let (_, foreign_peer) = ping(&other.api_socket());
    assert_eq!(foreign_peer, foreign.id());

    let observer = pidfd_open(server).expect("observe owned server");
    assert!(
        fx.pin_fixture_server(foreign.id()).is_none(),
        "a candidate whose marker is not ours must not be pinned"
    );
    let stopped = fx.stop_fixture_servers(&[foreign.id(), server]);
    let server_exited = pidfd_wait_exit(&observer, Duration::from_secs(1));
    let foreign_alive = foreign.try_wait().unwrap().is_none();
    let (foreign_reply, _) = ping(&other.api_socket());
    println!(
        "owned server {server}, foreign server {}; stopped {stopped:?}; owned exited \
         {server_exited}; foreign alive {foreign_alive}, replies {foreign_reply:?}",
        foreign.id()
    );
    // Owned child, still unreaped: safe to kill by its handle.
    let _ = foreign.kill();
    let _ = foreign.wait();

    assert_eq!(
        stopped,
        vec![server],
        "only the pinned owned server is stopped"
    );
    assert!(server_exited, "owned fixture server was terminated");
    assert!(foreign_alive, "foreign process was signalled");
    assert_eq!(foreign_reply, "pong");
    assert!(fx.fixture_servers().is_empty());
}

#[test]
fn unavailable_pidfd_preflight_skips_before_creating_or_spawning_anything() {
    static OPENS: AtomicU32 = AtomicU32::new(0);
    fn unavailable(_pid: u32) -> std::io::Result<OwnedFd> {
        OPENS.fetch_add(1, Ordering::Relaxed);
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }
    // A private empty root: any fixture file the skipped path created would land here.
    let root = std::env::temp_dir().join(format!(
        "lmu-preflight-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir(&root).expect("create unique preflight root");

    let fixture = Fixture::new_in(&root, None, Fake::Succeeds, unavailable);
    let created: Vec<_> = fs::read_dir(&root)
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    let _ = fs::remove_dir(&root);

    assert!(fixture.is_none(), "unsupported pidfd must yield no fixture");
    // The opener was the only probe: one call, and an Err means no pidfd existed to signal.
    // Nothing can have been spawned: spawning needs the fixture this returned None for.
    assert_eq!(OPENS.load(Ordering::Relaxed), 1);
    assert!(
        created.is_empty(),
        "skip created fixture files: {created:?}"
    );
}
