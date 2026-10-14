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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UNIT: &str = "herdr-e2e.service";
const FAKE_PID_ENV: &str = "LUNIT_FAKE_SYSTEMCTL_PID";

enum Fake {
    Succeeds,
    Fails,
}

/// One owned fixture. Drop stops only the bridge child and servers whose environment names
/// this fixture's unique socket, then removes only this fixture directory.
struct Fixture {
    dir: PathBuf,
    bridge: Option<Child>,
}

impl Fixture {
    fn new(systemd_unit: Option<&str>, fake: Fake) -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
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
                "printf 'Failed to start %s: Unit %s not found.\\n' \"$3\" \"$3\" >&2\nexit 5\n"
                    .to_string()
            }
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

        Self { dir, bridge: None }
    }

    fn api_socket(&self) -> PathBuf {
        self.dir.join("r/herdr.sock")
    }

    fn client_socket(&self) -> PathBuf {
        self.dir.join("r/herdr-client.sock")
    }

    /// The remote-host command `herdr --remote` runs over SSH, with an isolated environment.
    fn spawn_bridge(&mut self) {
        let mut command = test_command::herdr_command();
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .arg("remote-client-bridge")
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
            .env_remove(FAKE_PID_ENV)
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

    /// `herdr server` processes whose environment names this fixture's unique socket.
    fn fixture_servers(&self) -> Vec<u32> {
        let needle = format!("HERDR_SOCKET_PATH={}", self.api_socket().display());
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let (Ok(cmdline), Ok(environ)) = (
                fs::read(format!("/proc/{pid}/cmdline")),
                fs::read(format!("/proc/{pid}/environ")),
            ) else {
                continue;
            };
            let argv: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            if argv.get(1) == Some(&&b"server"[..])
                && environ.split(|b| *b == 0).any(|kv| kv == needle.as_bytes())
            {
                pids.push(pid);
            }
        }
        pids
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut bridge) = self.bridge.take() {
            let _ = bridge.kill();
            let _ = bridge.wait();
        }
        let pids = self.fixture_servers();
        for &pid in &pids {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.fixture_servers().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        for pid in self.fixture_servers() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
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
    let mut fx = Fixture::new(Some(UNIT), Fake::Succeeds);
    fx.spawn_bridge();

    let (reply, peer_pid) = ping(&fx.api_socket());
    let calls = fx.systemctl_calls();
    println!("systemctl argv: {calls:?}");
    assert_eq!(calls, vec![vec!["--user", "start", UNIT]]);
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
    let mut fx = Fixture::new(Some(UNIT), Fake::Fails);
    fx.spawn_bridge();
    let output = fx.wait_bridge_exit();
    let stderr = stderr_of(&output);
    let calls = fx.systemctl_calls();
    println!(
        "systemctl argv: {calls:?}\nbridge status: {}\nbridge stderr: {stderr}",
        output.status
    );

    assert_eq!(calls, vec![vec!["--user", "start", UNIT]]);
    assert!(!output.status.success());
    // The error names the systemctl action and unit, says it failed, and carries the
    // fake's own failure detail, so a bare mention of the unit is not enough.
    assert!(
        stderr.contains(&format!("systemctl --user start {UNIT}")),
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
    let mut fx = Fixture::new(Some(UNIT), Fake::Succeeds);
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
    let mut fx = Fixture::new(None, Fake::Succeeds);
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
    let mut fx = Fixture::new(Some(bad), Fake::Succeeds);
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
