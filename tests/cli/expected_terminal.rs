//! Exercise the native server and real PTYs, not an accepting mock endpoint.
use super::harness::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

struct GuardServer {
    base: PathBuf,
    socket: PathBuf,
    bin: PathBuf,
    config: String,
    server: Option<std::process::Child>,
}

impl GuardServer {
    fn new(delay: &str) -> Self {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let bin = base.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let pi = bin.join("pi");
        fs::write(
            &pi,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$PROBE_ORIGIN\" >> '{}'\nexport HERDR_AGENT=pi\n'{}' pane report-agent \"$1\" --source custom:expected-terminal-test --agent pi --state idle >/dev/null\nwhile IFS= read -r prompt; do :; done\n",
                base.join("pi-launch").display(),
                env!("CARGO_BIN_EXE_herdr"),
            ),
        )
        .unwrap();
        fs::set_permissions(pi, fs::Permissions::from_mode(0o755)).unwrap();
        let shell = bin.join("delayed-shell");
        fs::write(
            &shell,
            format!("#!/bin/sh\n/bin/sleep {delay}\nexec /bin/sh\n"),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!(
            "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
            shell.to_str().unwrap()
        );
        let socket = base.join("runtime/herdr.sock");
        let mut fixture = Self {
            base,
            socket,
            bin,
            config,
            server: None,
        };
        fixture.spawn();
        fixture
    }

    fn command(&self) -> Command {
        // The shared constructor removes all HERDR_* and both Pi profile keys
        // before these private paths are applied. Never change the runner env.
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

    fn cli(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn cli_json(&self, args: &[&str]) -> Value {
        parse_cli_json_output(args, self.cli(args))
    }

    fn spawn(&mut self) {
        let config = self.base.join("config").join(app_dir_name());
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("config.toml"), &self.config).unwrap();
        register_runtime_dir(&self.base.join("runtime"));
        let mut command = self.command();
        command
            .arg("server")
            .env("PATH", &self.bin)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().unwrap();
        register_spawned_herdr_pid(Some(child.id()));
        self.server = Some(child);
        wait_for_socket(&self.socket, Duration::from_secs(5));
    }

    fn request(&self, method: &str, params: Value) -> Value {
        send_request(
            &self.socket,
            &json!({"id": "expected-terminal", "method": method, "params": params}).to_string(),
        )
    }

    fn pane(&self, origin: &str) -> Value {
        let env = format!("PROBE_ORIGIN={origin}");
        self.cli_json(&[
            "workspace",
            "create",
            "--cwd",
            self.base.to_str().unwrap(),
            "--env",
            &env,
        ])["result"]["root_pane"]
            .clone()
    }

    fn pane_id<'a>(&self, pane: &'a Value) -> &'a str {
        pane["pane_id"].as_str().unwrap()
    }

    fn prepare_params(&self, pane: &Value) -> Value {
        json!({
            "pane_id": pane["pane_id"],
            "text": format!("printf '%s\\n' \"$PROBE_ORIGIN\" > '{}'", self.base.join("preparation").display()),
            "keys": ["Enter"],
        })
    }

    fn start_params(&self, pane: &Value) -> Value {
        json!({"pane_id": pane["pane_id"], "name": "worker", "kind": "pi",
            "args": [pane["pane_id"]], "timeout_ms": 8000})
    }

    fn guarded_prepare(&self, pane: &Value, expected: Value) -> Value {
        let mut params = self.prepare_params(pane);
        params["expected_terminal"] = expected;
        self.request("pane.send_input", params)
    }

    fn guarded_start(&self, pane: &Value, expected: Value) -> Value {
        let mut params = self.start_params(pane);
        params["expected_terminal"] = expected;
        self.request("agent.start", params)
    }

    fn barrier(&self, pane: &Value) {
        let marker = self.base.join(format!(
            "barrier-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let command = format!("printf ready > '{}'", marker.display());
        assert!(self
            .cli(&["pane", "run", self.pane_id(pane), &command])
            .status
            .success());
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(25),
            || marker.exists()
        ));
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(25),
            || {
                let info = self.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
                info["result"]["process_info"]["foreground_processes"]
                    .as_array()
                    .is_some_and(|processes| {
                        !processes.is_empty()
                            && processes
                                .iter()
                                .all(|p| matches!(p["name"].as_str(), Some("sh" | "dash" | "bash")))
                    })
            }
        ));
    }

    fn stop(&mut self) {
        if self.server.is_some() {
            let list = self.request("workspace.list", json!({}));
            if let Some(workspaces) = list["result"]["workspaces"].as_array() {
                for workspace in workspaces {
                    let _ = self.cli(&[
                        "workspace",
                        "close",
                        workspace["workspace_id"].as_str().unwrap(),
                    ]);
                }
            }
            self.reap();
        }
    }

    fn reap(&mut self) {
        if let Some(mut child) = self.server.take() {
            let pid = child.id();
            // Graceful shutdown flushes the empty session after workspace.close
            // and reaps the server's PTYs before the same endpoint is recreated.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            let exited = wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
                child.try_wait().ok().flatten().is_some()
            });
            if !exited {
                let _ = child.kill();
            }
            let _ = child.wait();
            unregister_spawned_herdr_pid(Some(pid));
        }
    }

    fn replace(&mut self, original: &Value) -> Value {
        // Public pane numbers are monotonic within a server, and pane.swap swaps
        // layout cells only. A real server replacement at the same endpoint is
        // the deterministic way to reuse a public slot without test-only APIs.
        self.stop();
        self.spawn();
        let replacement = self.pane("replacement");
        self.barrier(&replacement);
        assert_eq!(replacement["pane_id"], original["pane_id"]);
        assert_ne!(replacement["terminal_id"], original["terminal_id"]);
        replacement
    }

    fn no_effects(&self, replacement: &Value, preparation_absent: bool) {
        // A real shell barrier proves all input before this point was consumed.
        self.barrier(replacement);
        assert!(!self.base.join("pi-launch").exists());
        if preparation_absent {
            assert!(!self.base.join("preparation").exists());
        }
        let named = self.request("agent.get", json!({"target": "worker"}));
        assert_eq!(named["error"]["code"], "agent_not_found", "{named}");
        let listed = self.request("agent.list", json!({}));
        assert!(listed["result"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["name"] != "worker"));
    }
}

impl Drop for GuardServer {
    fn drop(&mut self) {
        // Do not query a broken server while unwinding. Child ownership and the
        // registered private runtime cleanup still owns our exact processes.
        if !thread::panicking() {
            self.stop();
        }
        self.reap();
        cleanup_test_base(&self.base);
    }
}

fn assert_mismatch(response: Value) {
    assert_eq!(
        response["error"]["code"], "terminal_identity_mismatch",
        "{response}"
    );
}

#[test]
fn expected_terminal_refuses_replacement_before_preparation_and_first_start() {
    let mut server = GuardServer::new("0");
    let original = server.pane("original");
    server.barrier(&original);
    let replacement = server.replace(&original);
    assert_mismatch(server.guarded_prepare(&original, original["terminal_id"].clone()));
    assert_mismatch(server.guarded_start(&original, original["terminal_id"].clone()));
    server.no_effects(&replacement, true);
}

#[test]
fn expected_terminal_refuses_available_replacement_after_busy_original() {
    let mut server = GuardServer::new("0");
    let original = server.pane("original");
    server.barrier(&original);
    assert!(server
        .cli(&["pane", "run", server.pane_id(&original), "/bin/sleep 20"])
        .status
        .success());
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || {
            let info = server.request("pane.process_info", json!({"pane_id": original["pane_id"]}));
            info["result"]["process_info"]["foreground_processes"]
                .as_array()
                .is_some_and(|p| p.iter().any(|p| p["name"] == "sleep"))
        }
    ));
    let busy = server.guarded_start(&original, original["terminal_id"].clone());
    assert_eq!(busy["error"]["code"], "agent_pane_busy", "{busy}");
    let replacement = server.replace(&original);
    assert_mismatch(server.guarded_prepare(&original, original["terminal_id"].clone()));
    assert_mismatch(server.guarded_start(&original, original["terminal_id"].clone()));
    server.no_effects(&replacement, true);
}

#[test]
fn expected_terminal_closes_last_observation_to_start_gap() {
    let mut server = GuardServer::new("0");
    let original = server.pane("original");
    server.barrier(&original);
    let prepared = server.guarded_prepare(&original, original["terminal_id"].clone());
    assert!(prepared.get("result").is_some(), "{prepared}");
    server.barrier(&original);
    assert_eq!(
        fs::read_to_string(server.base.join("preparation")).unwrap(),
        "original\n"
    );
    let observation = server.request("pane.get", json!({"pane_id": original["pane_id"]}));
    let expected = observation["result"]["pane"]["terminal_id"].clone();
    let replacement = server.replace(&original);
    assert_mismatch(server.guarded_start(&original, expected));
    server.no_effects(&replacement, false);
    assert_eq!(
        fs::read_to_string(server.base.join("preparation")).unwrap(),
        "original\n"
    );
}

#[test]
fn expected_terminal_native_json_rejects_invalid_present_guards_without_effects() {
    let server = GuardServer::new("0");
    let pane = server.pane("original");
    server.barrier(&pane);
    let other = server.pane("other");
    for expected in [
        other["terminal_id"].clone(),
        json!("term_nonexistent"),
        json!(""),
        json!("malformed"),
    ] {
        assert_mismatch(server.guarded_prepare(&pane, expected.clone()));
        assert_mismatch(server.guarded_start(&pane, expected));
    }
    for expected in [Value::Null, json!(42), json!(false), json!([]), json!({})] {
        for response in [
            server.guarded_prepare(&pane, expected.clone()),
            server.guarded_start(&pane, expected),
        ] {
            assert!(
                response.get("error").is_some(),
                "explicit null/type must not become absent: {response}"
            );
        }
    }
    let mut missing = pane.clone();
    missing["pane_id"] = json!("w999:p999");
    assert_mismatch(server.guarded_prepare(&missing, pane["terminal_id"].clone()));
    assert_mismatch(server.guarded_start(&missing, pane["terminal_id"].clone()));
    server.no_effects(&pane, true);
}

#[test]
fn expected_terminal_capability_and_cli_flags_refuse_then_accept_exact_identity() {
    let server = GuardServer::new("0");
    assert_eq!(
        server.request("ping", json!({}))["result"]["capabilities"]["expected_terminal_guard"],
        true
    );
    let pane = server.pane("original");
    server.barrier(&pane);
    let other = server.pane("other");
    let wrong = other["terminal_id"].as_str().unwrap();
    let right = pane["terminal_id"].as_str().unwrap();
    let preparation = format!(
        "printf cli > '{}'",
        server.base.join("preparation").display()
    );
    for args in [
        vec![
            "pane",
            "run",
            server.pane_id(&pane),
            "--expected-terminal",
            wrong,
            &preparation,
        ],
        vec![
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            server.pane_id(&pane),
            "--expected-terminal",
            wrong,
            "--timeout",
            "8000",
        ],
    ] {
        let output = server.cli(&args);
        assert_eq!(output.status.code(), Some(1));
        assert_mismatch(serde_json::from_slice(&output.stderr).unwrap());
    }
    server.no_effects(&pane, true);
    assert!(server
        .cli(&[
            "pane",
            "run",
            server.pane_id(&pane),
            "--expected-terminal",
            right,
            &preparation
        ])
        .status
        .success());
    server.barrier(&pane);
    assert_eq!(
        fs::read_to_string(server.base.join("preparation")).unwrap(),
        "cli"
    );
    let started = server.cli_json(&[
        "agent",
        "start",
        "worker",
        "--kind",
        "pi",
        "--pane",
        server.pane_id(&pane),
        "--expected-terminal",
        right,
        "--timeout",
        "8000",
        "--",
        server.pane_id(&pane),
    ]);
    assert_eq!(
        started["result"]["agent"]["terminal_id"],
        pane["terminal_id"]
    );
    assert_eq!(started["result"]["agent"]["interactive_ready"], true);
    assert_eq!(
        fs::read_to_string(server.base.join("pi-launch")).unwrap(),
        "original\n"
    );
}

#[test]
fn expected_terminal_absent_remains_legacy_compatible_for_both_native_methods() {
    let server = GuardServer::new("0");
    let pane = server.pane("original");
    server.barrier(&pane);
    let preparation = server.request("pane.send_input", server.prepare_params(&pane));
    assert!(preparation.get("result").is_some(), "{preparation}");
    server.barrier(&pane);
    assert_eq!(
        fs::read_to_string(server.base.join("preparation")).unwrap(),
        "original\n"
    );
    let started = server.request("agent.start", server.start_params(&pane));
    assert_eq!(
        started["result"]["agent"]["terminal_id"], pane["terminal_id"],
        "{started}"
    );
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || server.base.join("pi-launch").exists()
    ));
    let named = server.request("agent.get", json!({"target": "worker"}));
    assert_eq!(named["result"]["agent"]["terminal_id"], pane["terminal_id"]);
}

#[test]
fn expected_terminal_cli_retries_same_terminal_transient_busy_successfully() {
    let server = GuardServer::new("0.8");
    let pane = server.pane("original");
    let busy = server.guarded_start(&pane, pane["terminal_id"].clone());
    assert_eq!(busy["error"]["code"], "agent_pane_busy", "{busy}");
    let started = server.cli_json(&[
        "agent",
        "start",
        "worker",
        "--kind",
        "pi",
        "--pane",
        server.pane_id(&pane),
        "--expected-terminal",
        pane["terminal_id"].as_str().unwrap(),
        "--timeout",
        "8000",
        "--",
        server.pane_id(&pane),
    ]);
    assert_eq!(
        started["result"]["agent"]["terminal_id"],
        pane["terminal_id"]
    );
    assert_eq!(started["result"]["agent"]["interactive_ready"], true);
    assert_eq!(
        fs::read_to_string(server.base.join("pi-launch")).unwrap(),
        "original\n"
    );
}
