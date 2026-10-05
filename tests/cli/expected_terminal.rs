//! Exercise the native server and real PTYs, not an accepting mock endpoint.
use super::harness::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

struct GuardServer {
    base: PathBuf,
    socket: PathBuf,
    bin: PathBuf,
    config: String,
    pi_input: PathBuf,
    server: Option<std::process::Child>,
}

impl GuardServer {
    fn new(delay: &str) -> Self {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let bin = base.join("bin");
        fs::create_dir_all(&bin).unwrap();
        // A deterministic Node PTY fixture, NOT installed Pi. The actual OS
        // executable is node and argv retains the recognized package CLI path.
        // Never rewrite process.title: installed Pi does, a separate limitation.
        let pi = bin.join("pi");
        let pi_input = base.join("pi-input");
        let node_cli = base.join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js");
        fs::create_dir_all(node_cli.parent().unwrap()).unwrap();
        fs::write(
            &node_cli,
            format!(
                "const fs=require('fs'); fs.writeFileSync({:?},''); process.stdout.write('\\x1b[?2004h'); process.stdin.on('data', b=>fs.appendFileSync({:?},b));\n",
                pi_input, pi_input
            ),
        )
        .unwrap();
        fs::write(
            &pi,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$PROBE_ORIGIN\" >> '{}'\n'{}' pane report-agent \"$1\" --source custom:expected-terminal-test --agent pi --state idle >/dev/null\nexec /usr/local/bin/node '{}' \"$1\"\n",
                base.join("pi-launch").display(),
                env!("CARGO_BIN_EXE_herdr"),
                node_cli.display(),
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
            pi_input,
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

    fn start_worker(&self, pane: &Value) {
        let started = self.guarded_start(pane, pane["terminal_id"].clone());
        assert_eq!(
            started["result"]["agent"]["terminal_id"], pane["terminal_id"],
            "{started}"
        );
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || self.base.join("pi-launch").exists()
        ));
    }

    fn guarded_prompt(&self, text: &str, expected: &str) -> std::process::Output {
        self.cli(&[
            "agent",
            "prompt",
            "worker",
            text,
            "--expected-terminal",
            expected,
        ])
    }

    fn observed_worker_terminal(&self) -> String {
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || {
                let observed = self.request("agent.get", json!({"target": "worker"}));
                observed["result"]["agent"]["agent"] == "pi"
                    && observed["result"]["agent"]["name"] == "worker"
                    && observed["result"]["agent"]["interactive_ready"] == true
            }
        ));
        let observed = self.request("agent.get", json!({"target": "worker"}));
        assert!(observed.get("result").is_some(), "{observed}");
        observed["result"]["agent"]["terminal_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn kill_observed_foreground_group(&self, pane: &Value) {
        let info = self.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
        let pgid = info["result"]["process_info"]["foreground_process_group_id"]
            .as_i64()
            .expect("foreground process group id");
        assert!(pgid > 0, "{info}");
        assert_eq!(
            unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGTERM) },
            0
        );
    }

    fn prompt_error(output: std::process::Output) -> Value {
        assert_eq!(output.status.code(), Some(1));
        serde_json::from_slice(&output.stderr).unwrap_or_else(|err| {
            panic!(
                "prompt did not return JSON error: {err}; stderr={}; stdout={}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )
        })
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
fn guarded_agent_prompt_refuses_after_foreground_loss_without_shell_effects() {
    let server = GuardServer::new("0");
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    let poison = server.base.join("foreground-loss-prompt-reached-shell");
    server.kill_observed_foreground_group(&pane);

    let response = GuardServer::prompt_error(server.guarded_prompt(
        &format!("printf poisoned > '{}'", poison.display()),
        &expected,
    ));
    assert_eq!(response["error"]["code"], "agent_not_ready", "{response}");
    assert!(!poison.exists(), "guarded prompt reached the shell");
    assert!(
        fs::read(&server.pi_input).unwrap_or_default().is_empty(),
        "guarded prompt reached the old Pi"
    );
}

// Linux exposes the exact interprocess-lock waiter and the kernel's terminal
// foreground group, so this interval is synchronized without sleep guesses.
#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_foreground_change_during_input_log_lock() {
    use std::os::unix::fs::MetadataExt;

    let server = GuardServer::new("0");
    let prompts = server.base.join("shell-prompts");
    // Count each actual shell prompt, including one caused by a bare Enter.
    // This observes input without replacing the shell's native controlling PTY.
    fs::write(
        server.bin.join("delayed-shell"),
        format!(
            "#!/bin/sh\nPS1='$(printf x >> \"{}\")$ '\nexport PS1\nexec /bin/sh\n",
            prompts.display()
        ),
    )
    .unwrap();
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || server.pi_input.exists()
    ));
    let info = server.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
    let pgid = info["result"]["process_info"]["foreground_process_group_id"]
        .as_i64()
        .unwrap();
    let shell_pid = info["result"]["process_info"]["shell_pid"]
        .as_u64()
        .unwrap();
    assert!(pgid > 0 && pgid as u64 != shell_pid, "{info}");
    let server_pid = server.server.as_ref().unwrap().id().to_string();
    let log_path = server
        .base
        .join("state")
        .join(app_dir_name())
        .join("api-input.jsonl");
    let log_before = fs::read(&log_path).unwrap();
    let input_before = fs::read(&server.pi_input).unwrap();
    assert!(input_before.is_empty());
    let effect = server.base.join("guarded-shell-effect");
    let token = "native_guard_unsent_assignment";
    // A comment terminates bracketed-paste suffixes on shells that do not
    // interpret them. The downstream barrier uses the same ordinary shell form.
    let text = format!(
        ":; NATIVE_GUARD_UNSENT={token}; printf escaped > '{}'; #",
        effect.display()
    );
    let (response, prompts_at_resume) = thread::scope(|scope| {
        // Lock ownership is inside the scope: an assertion panic drops it
        // before scoped threads join, so a failed proof cannot deadlock cleanup.
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&log_path)
            .unwrap();
        lock.lock().unwrap();
        let metadata = lock.metadata().unwrap();
        let identity = format!(
            "{:02x}:{:02x}:{}",
            libc::major(metadata.dev()),
            libc::minor(metadata.dev()),
            metadata.ino()
        );
        let is_blocked = || {
            fs::read_to_string("/proc/locks")
                .unwrap()
                .lines()
                .any(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    fields.contains(&"->")
                        && fields.contains(&"FLOCK")
                        && fields.contains(&"WRITE")
                        && fields.contains(&server_pid.as_str())
                        && fields.contains(&identity.as_str())
                })
        };
        let writer = scope.spawn(|| native_guarded_request(&server.socket, &text, &expected));
        assert!(
            wait_until(
                Duration::from_secs(3),
                Duration::from_millis(25),
                is_blocked
            ),
            "server {server_pid} never waited on exact input log inode {identity}"
        );
        assert_eq!(fs::read(&log_path).unwrap(), log_before);
        assert!(!writer.is_finished());
        let prompts_before = fs::read(&prompts).unwrap().len();
        assert_eq!(
            unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGTERM) },
            0
        );
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || {
                // /proc stat field 8 is tpgid. Read it without querying the
                // App, which deliberately remains blocked on the input log.
                let stat = fs::read_to_string(format!("/proc/{shell_pid}/stat"));
                let tpgid = stat.ok().and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().nth(5))
                        .and_then(|field| field.parse::<u64>().ok())
                });
                tpgid == Some(shell_pid)
                    && fs::read(&prompts).is_ok_and(|receipts| receipts.len() == prompts_before + 1)
            }
        ));
        assert!(
            is_blocked(),
            "receiver resumed before foreground transition"
        );
        assert!(!writer.is_finished());
        assert_eq!(fs::read(&server.pi_input).unwrap(), input_before);
        let prompts_at_resume = fs::read(&prompts).unwrap().len();
        eprintln!(
            "native lock barrier: server={server_pid}, inode={identity}; shell tpgid={shell_pid}; terminal={expected}"
        );
        drop(lock);
        (writer.join().unwrap(), prompts_at_resume)
    });

    let same = server.request("pane.get", json!({"pane_id": pane["pane_id"]}));
    assert_eq!(same["result"]["pane"]["terminal_id"], expected);
    let drained = server.base.join("guarded-downstream-barrier");
    let barrier = format!(
        ":; printf '%s' \"${{NATIVE_GUARD_UNSENT-unset}}\" > '{}'; #",
        drained.display()
    );
    assert!(server
        .cli(&["pane", "run", server.pane_id(&pane), &barrier])
        .status
        .success());
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || drained.exists() && fs::read(&prompts).is_ok_and(|p| p.len() > prompts_at_resume)
    ));
    let screen = server.request(
        "pane.read",
        json!({"pane_id": pane["pane_id"], "source": "recent", "format": "text"}),
    );
    let log = fs::read_to_string(&log_path).unwrap();
    let records: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record["method"] == "agent.prompt_guarded")
        .collect();
    // The durable record describes an attempted input, not a success receipt.
    assert_eq!(records.len(), 1, "{log}");
    assert_eq!(records[0]["target_terminal"], expected);
    assert_eq!(records[0]["bytes"], text.len());
    assert!(!log.contains(token));
    eprintln!(
        "native downstream proof: response={response}; capture={:?}; shell sentinel={:?}; prompt receipts={}",
        fs::read(&server.pi_input).unwrap(),
        fs::read_to_string(&drained).unwrap(),
        fs::read(&prompts).unwrap().len() - prompts_at_resume
    );
    assert!(
        response.get("result").is_none(),
        "must not report success: {response}"
    );
    assert!(
        matches!(
            response["error"]["code"].as_str(),
            Some("agent_not_ready" | "agent_prompt_failed")
        ),
        "{response}"
    );
    assert!(!effect.exists(), "guarded input executed in the shell");
    assert_eq!(fs::read_to_string(&drained).unwrap(), "unset");
    assert_eq!(fs::read(&server.pi_input).unwrap(), input_before);
    assert!(!screen["result"]["read"]["text"]
        .as_str()
        .unwrap()
        .contains(token));
    assert_eq!(
        fs::read(&prompts).unwrap().len(),
        prompts_at_resume + 1,
        "only the downstream barrier may supply an Enter to the shell"
    );
}

#[cfg(target_os = "linux")]
fn native_guarded_request(socket: &Path, text: &str, expected: &str) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"id": "native-writer-guard", "method": "agent.prompt_guarded",
            "params": {"target": "worker", "text": text, "expected_terminal": expected}})
    )
    .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(target_os = "linux")]
fn native_shell_foreground(shell_pid: u64) -> bool {
    fs::read_to_string(format!("/proc/{shell_pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(5))
                .and_then(|field| field.parse::<u64>().ok())
        })
        == Some(shell_pid)
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_foreground_change_while_native_writer_queued() {
    let server = GuardServer::new("0");
    let prompts = server.base.join("shell-prompts");
    fs::write(
        server.bin.join("delayed-shell"),
        format!(
            "#!/bin/sh\nPS1='$(/usr/bin/stty sane -echo; printf x >> \"{}\")$ '\nexport PS1\nexec /bin/sh\n",
            prompts.display()
        ),
    )
    .unwrap();
    let probe = server
        .base
        .join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js");
    fs::write(
        &probe,
        format!(
            "const fs=require('fs'); fs.writeFileSync({:?},''); fs.writeFileSync({:?},'blocked'); setInterval(()=>{{}},1000);\n",
            server.pi_input,
            server.base.join("reader-blocked"),
        ),
    )
    .unwrap();
    // Keep a live, foreground native reader behind a readiness barrier. SIGSTOP
    // would return the interactive shell to foreground before the guarded request.
    fs::write(
        server.bin.join("pi"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PROBE_ORIGIN\" >> '{}'\n/usr/bin/stty raw -echo\n'{}' pane report-agent \"$1\" --source custom:expected-terminal-test --agent pi --state idle >/dev/null\nprintf '\\033[?2004h'\nexec /usr/local/bin/node '{}' \"$1\"\n",
            server.base.join("pi-launch").display(),
            env!("CARGO_BIN_EXE_herdr"),
            probe.display(),
        ),
    )
    .unwrap();
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || { server.pi_input.exists() }
    ));
    let info = server.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
    let pgid = info["result"]["process_info"]["foreground_process_group_id"]
        .as_i64()
        .unwrap();
    let shell_pid = info["result"]["process_info"]["shell_pid"]
        .as_u64()
        .unwrap();
    assert!(pgid > 0 && pgid as u64 != shell_pid, "{info}");
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || server.base.join("reader-blocked").exists(),
    ));
    // More than any native PTY input buffer, but well below the API frame limit.
    // Legacy data is deliberately only spaces: never assignment text or Enter.
    let filler = server.request(
        "pane.send_text",
        json!({
            "pane_id": pane["pane_id"], "text": " ".repeat(512 * 1024)
        }),
    );
    assert!(filler.get("result").is_some(), "{filler}");
    // A deliberately paused native reader has no UI signals. Preserve the
    // same explicit lifecycle authority a real Pi integration supplies.
    let report = server.cli(&[
        "pane",
        "report-agent",
        server.pane_id(&pane),
        "--source",
        "herdr:pi",
        "--agent",
        "pi",
        "--state",
        "idle",
    ]);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let observed = server.request("agent.get", json!({"target": "worker"}));
    let foreground = server.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
    assert_eq!(
        observed["result"]["agent"]["terminal_id"], expected,
        "{observed}"
    );
    assert_eq!(
        foreground["result"]["process_info"]["foreground_process_group_id"], pgid,
        "{foreground}"
    );
    eprintln!("queued native input observation: {observed}; foreground: {foreground}");
    let log_path = server
        .base
        .join("state")
        .join(app_dir_name())
        .join("api-input.jsonl");
    let token = "native_guard_queued_unsent";
    let effect = server.base.join("queued-shell-effect");
    let text = format!(
        ":; NATIVE_GUARD_UNSENT={token}; printf escaped > '{}'; #",
        effect.display()
    );
    let response = thread::scope(|scope| {
        let writer = scope.spawn(|| native_guarded_request(&server.socket, &text, &expected));
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || {
                writer.is_finished()
                    || fs::read_to_string(&log_path).unwrap().lines().any(|line| {
                        serde_json::from_str::<Value>(line).unwrap()["method"]
                            == "agent.prompt_guarded"
                    })
            }
        ));
        if writer.is_finished() {
            panic!(
                "queued request refused before writer barrier: {}",
                writer.join().unwrap()
            );
        }
        // The log proves this handler started. A later App-dispatched lookup
        // proves it finished enqueueing; the socket completion is still pending.
        let barrier = server.request("pane.get", json!({"pane_id": pane["pane_id"]}));
        assert_eq!(barrier["result"]["pane"]["terminal_id"], expected);
        assert!(
            !writer.is_finished(),
            "submission completed while its reader was paused"
        );
        assert!(fs::read(&server.pi_input).unwrap().is_empty());
        assert_eq!(
            unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL) },
            0
        );
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || { native_shell_foreground(shell_pid) }
        ));
        eprintln!("native queued barrier: paused reader pgid={pgid}; App handler enqueued; same terminal={expected}; shell tpgid={shell_pid}");
        writer.join().unwrap()
    });
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || { fs::read(&prompts).is_ok_and(|p| p.len() >= 3) }
    ));
    let prompts_at_resume = fs::read(&prompts).unwrap().len();
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], "agent_not_ready", "{response}");
    assert!(!effect.exists());
    // Killing a raw reader does not restore its terminal modes. Restore only
    // this fixture's owned PTY, then drain the deliberately harmless backlog.
    let tty = format!("/proc/{shell_pid}/fd/0");
    assert!(Command::new("/usr/bin/stty")
        .args(["-F", &tty, "sane"])
        .status()
        .unwrap()
        .success());
    assert!(server
        .cli(&["pane", "send-keys", server.pane_id(&pane), "enter"])
        .status
        .success());
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || fs::read(&prompts).is_ok_and(|p| p.len() > prompts_at_resume),
    ));
    let prompts_after_drain = fs::read(&prompts).unwrap().len();
    let drained = server.base.join("queued-downstream-barrier");
    let barrier = format!(
        ":; printf '%s' \"${{NATIVE_GUARD_UNSENT-unset}}\" > '{}'; #",
        drained.display()
    );
    assert!(server
        .cli(&["pane", "run", server.pane_id(&pane), &barrier])
        .status
        .success());
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || { drained.exists() && fs::read(&prompts).is_ok_and(|p| p.len() > prompts_after_drain) }
    ));
    eprintln!(
        "native queued proof: response={response}; capture={:?}; downstream sentinel={:?}",
        fs::read(&server.pi_input).unwrap(),
        fs::read_to_string(&drained).unwrap()
    );
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], "agent_not_ready", "{response}");
    assert!(!effect.exists());
    assert_eq!(fs::read_to_string(&drained).unwrap(), "unset");
    assert!(fs::read(&server.pi_input).unwrap().is_empty());
    assert_eq!(prompts_after_drain, prompts_at_resume + 1);
    assert_eq!(fs::read(&prompts).unwrap().len(), prompts_at_resume + 2);
    assert!(!fs::read_to_string(&log_path).unwrap().contains(token));
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_delayed_enter_after_native_probe_consumes_text() {
    let server = GuardServer::new("0");
    let prompts = server.base.join("shell-prompts");
    fs::write(
        server.bin.join("delayed-shell"),
        format!(
            "#!/bin/sh\nPS1='$(printf x >> \"{}\")$ '\nexport PS1\nexec /bin/sh\n",
            prompts.display()
        ),
    )
    .unwrap();
    let text = "--assignment\nλ 日本語\nsecond line";
    let payload = format!("\x1b[200~{text}\x1b[201~").into_bytes();
    let ready = server.base.join("raw-reader-ready");
    let probe = server
        .base
        .join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js");
    fs::write(&probe, format!(
        "const fs=require('fs'); process.stdin.setRawMode(true); fs.writeFileSync({:?},'ready'); process.stdout.write('\\x1b[?2004h'); let data=Buffer.alloc(0); process.stdin.on('data', b=>{{ data=Buffer.concat([data,b]); if(data.length>={}){{ fs.writeFileSync({:?},data); require('child_process').spawnSync('/usr/bin/stty',['sane'],{{stdio:[0,1,2]}}); process.stdout.write('\\x1b[?2004l'); process.exit(0); }} }});\n",
        ready, payload.len(), server.pi_input
    )).unwrap();
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || ready.exists()
    ));
    let info = server.request("pane.process_info", json!({"pane_id": pane["pane_id"]}));
    let shell_pid = info["result"]["process_info"]["shell_pid"]
        .as_u64()
        .unwrap();
    let prompts_before = fs::read(&prompts).unwrap().len();
    // The native reader exits itself only after consuming the entire text
    // boundary. No parent sleep/kill guesses the deferred-Enter interval.
    let response = native_guarded_request(&server.socket, text, &expected);
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || {
            native_shell_foreground(shell_pid)
                && fs::read(&prompts).is_ok_and(|p| p.len() > prompts_before)
        }
    ));
    let same = server.request("pane.get", json!({"pane_id": pane["pane_id"]}));
    assert_eq!(same["result"]["pane"]["terminal_id"], expected);
    eprintln!("native delayed Enter proof: response={response}; exact text capture={:?}; shell prompt delta={}", fs::read(&server.pi_input).unwrap(), fs::read(&prompts).unwrap().len() - prompts_before);
    assert_eq!(fs::read(&server.pi_input).unwrap(), payload);
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], "agent_not_ready", "{response}");
    assert_eq!(
        fs::read(&prompts).unwrap().len(),
        prompts_before + 1,
        "no blank Enter may follow the probe exit prompt"
    );
    // A downstream shell command also fences presentation/input after refusal.
    server.barrier(&pane);
    assert_eq!(fs::read(&prompts).unwrap().len(), prompts_before + 2);
    assert_eq!(fs::read(&server.pi_input).unwrap(), payload);
}

// Linux supplies independent executable, argv, generation and tpgid evidence.
// This package-path Node helper is NOT installed Pi and never rewrites title.
#[cfg(target_os = "linux")]
fn native_launcher_identity(pid: u32) -> (u32, u32, u64) {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    (
        fields[2].parse().unwrap(),
        fields[5].parse().unwrap(),
        fields[19].parse().unwrap(),
    )
}

#[cfg(target_os = "linux")]
fn native_surviving_launcher_case(mode: &str) {
    use std::os::unix::fs::MetadataExt;
    let server = GuardServer::new("0");
    let text = "--assignment\nλ 日本語\nsecond line";
    let payload = format!("\x1b[200~{text}\x1b[201~").into_bytes();
    let probe = server
        .base
        .join("node_modules/@earendil-works/pi-coding-agent/dist/cli.js");
    let ready = server.base.join("node-ready");
    let body = if mode == "delayed" {
        format!("let data=Buffer.alloc(0); process.stdin.on('data', b=>{{data=Buffer.concat([data,b]); if(data.length>={}){{fs.writeFileSync({:?},data); process.stdout.write('\\x1b[?2004l'); process.exit(0);}}}});", payload.len(), server.pi_input)
    } else {
        format!(
            "process.stdin.on('data', b=>fs.appendFileSync({:?},b));",
            server.pi_input
        )
    };
    fs::write(&probe, format!("const fs=require('fs'); process.stdin.setRawMode(true); fs.writeFileSync({:?},''); process.stdout.write('\\x1b[?2004h'); fs.writeFileSync({ready:?},String(process.pid)); {body}\n", server.pi_input)).unwrap();
    let follow = server.base.join("innocuous-follow.py");
    let follow_ready = server.base.join("follow-ready");
    let follow_input = server.base.join("follow-input");
    let follow_done = server.base.join("follow-done");
    let fence = "\x1fR6_FENCE\x1f";
    // The unrecognized raw reader records ALL bytes, including a bare Enter.
    // TCSANOW never flushes queued input. An explicit raw fence proves drainage
    // without guessing a quiet interval or erasing residual text/Enter.
    fs::write(&follow, format!("import os,pathlib,tty,termios\ntty.setraw(0,termios.TCSANOW)\npathlib.Path({follow_ready:?}).write_text(str(os.getpid()))\ndata=b''\nwhile not data.endswith(b'\\x1fR6_FENCE\\x1f'):\n data+=os.read(0,4096)\n pathlib.Path({follow_input:?}).write_bytes(data)\npathlib.Path({follow_done:?}).write_text('done')\nwhile True: os.read(0,4096)\n")).unwrap();
    fs::write(server.bin.join("pi"), format!(
        "#!/bin/sh\nprintf '%s\\n' \"$PROBE_ORIGIN\" >> '{}'\n'{}' pane report-agent \"$1\" --source custom:expected-terminal-test --agent pi --state idle >/dev/null\nprintf '%s' \"$$\" > '{}'\n/usr/local/bin/node '{}' \"$1\" < /dev/tty &\nchild=$!\nprintf '%s' \"$child\" > '{}'\nwait \"$child\"\nprintf '\\033[?2004l'\n/usr/bin/python3 '{}'\n: surviving_nonexec_leader\n",
        server.base.join("pi-launch").display(), env!("CARGO_BIN_EXE_herdr"), server.base.join("leader-pid").display(), probe.display(), server.base.join("child-pid").display(), follow.display()
    )).unwrap();
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(10),
        || ready.exists()
    ));
    // Apply lifecycle readiness only after the actual child published its raw
    // input barrier; a pre-child launcher report can race startup detection.
    let report = server.cli(&[
        "pane",
        "report-agent",
        server.pane_id(&pane),
        "--source",
        "herdr:pi",
        "--agent",
        "pi",
        "--state",
        "idle",
    ]);
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let expected = server.observed_worker_terminal();
    let child: u32 = fs::read_to_string(&ready).unwrap().parse().unwrap();
    let leader: u32 = fs::read_to_string(server.base.join("leader-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        fs::read_to_string(server.base.join("child-pid"))
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        child
    );
    assert_eq!(
        fs::read_link(format!("/proc/{child}/exe")).unwrap(),
        fs::canonicalize("/usr/local/bin/node").unwrap()
    );
    let argv = fs::read(format!("/proc/{child}/cmdline")).unwrap();
    assert!(
        argv.split(|b| *b == 0)
            .any(|arg| arg == probe.as_os_str().as_encoded_bytes()),
        "explicit recognized CLI argv: {argv:?}"
    );
    assert_eq!(
        fs::read_link(format!("/proc/{leader}/exe")).unwrap(),
        fs::canonicalize("/bin/sh").unwrap()
    );
    let identity = native_launcher_identity(leader);
    assert_eq!((identity.0, identity.1), (leader, leader));
    assert_eq!(native_launcher_identity(child).0, leader);
    let info = server.request("pane.process_info", json!({"pane_id":pane["pane_id"]}));
    assert_eq!(
        info["result"]["process_info"]["foreground_process_group_id"],
        leader
    );
    eprintln!("native same-group {mode}: actual node={child}; exe/CLI argv verified; leader={leader}; pgrp/tpgid/start={identity:?}; terminal={expected}");
    let kill_child = || {
        // Never kill the group: the shell launcher MUST survive unchanged.
        assert_eq!(
            unsafe { libc::kill(child as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert!(wait_until(
            Duration::from_secs(3),
            Duration::from_millis(10),
            || follow_ready.exists()
        ));
        assert!(
            !Path::new(&format!("/proc/{child}")).exists(),
            "actual child reaped"
        );
        assert_eq!(native_launcher_identity(leader), identity);
    };
    let log_path = server
        .base
        .join("state")
        .join(app_dir_name())
        .join("api-input.jsonl");
    let log_before = fs::read(&log_path).unwrap();
    let response = if mode == "lock" {
        thread::scope(|scope| {
            // Drop lock before scoped request joins, including during unwinding.
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&log_path)
                .unwrap();
            lock.lock().unwrap();
            let meta = lock.metadata().unwrap();
            let inode = format!(
                "{:02x}:{:02x}:{}",
                libc::major(meta.dev()),
                libc::minor(meta.dev()),
                meta.ino()
            );
            let server_pid = server.server.as_ref().unwrap().id().to_string();
            let blocked = || {
                fs::read_to_string("/proc/locks")
                    .unwrap()
                    .lines()
                    .any(|line| {
                        let fields: Vec<_> = line.split_whitespace().collect();
                        ["->", "FLOCK", "WRITE", &server_pid, &inode]
                            .iter()
                            .all(|field| fields.contains(field))
                    })
            };
            let writer = scope.spawn(|| native_guarded_request(&server.socket, text, &expected));
            assert!(
                wait_until(Duration::from_secs(3), Duration::from_millis(10), blocked),
                "exact log inode waiter"
            );
            assert_eq!(fs::read(&log_path).unwrap(), log_before);
            assert!(!writer.is_finished());
            kill_child();
            assert!(
                blocked() && !writer.is_finished(),
                "child exited while receiver still suspended"
            );
            assert!(fs::read(&server.pi_input).unwrap().is_empty());
            drop(lock);
            writer.join().unwrap()
        })
    } else {
        if mode == "gone" {
            kill_child();
        }
        // Reader exits itself on exact text, no parent sleep or guessed kill.
        native_guarded_request(&server.socket, text, &expected)
    };
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(10),
        || follow_ready.exists()
    ));
    assert_eq!(
        native_launcher_identity(leader),
        identity,
        "launcher PID/start/group/tpgid unchanged"
    );
    let same = server.request("pane.get", json!({"pane_id":pane["pane_id"]}));
    assert_eq!(same["result"]["pane"]["terminal_id"], expected);
    let sent = server.request(
        "pane.send_text",
        json!({"pane_id":pane["pane_id"], "text":fence}),
    );
    assert!(sent.get("result").is_some(), "{sent}");
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(10),
        || follow_done.exists()
    ));
    let capture = fs::read(&server.pi_input).unwrap();
    let downstream = fs::read(&follow_input).unwrap();
    eprintln!("native same-group {mode} proof: response={response}; actual-node={capture:?}; follow-on={downstream:?}; explicit-fence={:?}; unchanged leader={identity:?}", fence.as_bytes());
    assert_eq!(
        capture,
        if mode == "delayed" {
            payload
        } else {
            Vec::new()
        }
    );
    assert!(
        response.get("result").is_none(),
        "no successful acknowledgement: {response}"
    );
    assert!(
        matches!(
            response["error"]["code"].as_str(),
            Some("agent_not_ready" | "agent_prompt_failed")
        ),
        "{response}"
    );
    assert_eq!(
        downstream,
        fence.as_bytes(),
        "NO guarded text, Enter or residual; only explicit raw downstream fence"
    );
    if mode == "gone" {
        let attempts = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["method"] == "agent.prompt_guarded"
                })
                .count()
        };
        assert_eq!(
            attempts(&fs::read(&log_path).unwrap()),
            attempts(&log_before),
            "launcher-only refuses before logging effects"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_surviving_launcher_during_input_log_lock() {
    native_surviving_launcher_case("lock");
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_surviving_launcher_delayed_enter() {
    native_surviving_launcher_case("delayed");
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_agent_prompt_refuses_launcher_only_after_actual_node_exits() {
    native_surviving_launcher_case("gone");
}

#[test]
fn guarded_agent_prompt_refuses_blocked_agent_without_input() {
    let server = GuardServer::new("0");
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    let poison = server.base.join("blocked-prompt-reached-agent");
    let blocked = server.cli(&[
        "pane",
        "report-agent",
        server.pane_id(&pane),
        "--source",
        "custom:expected-terminal-test",
        "--agent",
        "pi",
        "--state",
        "blocked",
    ]);
    assert!(
        blocked.status.success(),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );

    let response = GuardServer::prompt_error(server.guarded_prompt(
        &format!("printf poisoned > '{}'", poison.display()),
        &expected,
    ));
    assert_eq!(response["error"]["code"], "agent_blocked", "{response}");
    assert!(!poison.exists(), "blocked prompt reached the shell");
    assert!(
        fs::read(&server.pi_input).unwrap_or_default().is_empty(),
        "blocked prompt reached Pi"
    );
}

#[test]
fn guarded_agent_prompt_refuses_replaced_terminal_after_agent_observation() {
    let mut server = GuardServer::new("0");
    let original = server.pane("original");
    server.barrier(&original);
    server.start_worker(&original);
    let expected = server.observed_worker_terminal();
    let poison = server.base.join("replacement-prompt-reached-shell");
    let replacement = server.replace(&original);
    assert_ne!(replacement["terminal_id"], expected);

    let response = GuardServer::prompt_error(server.guarded_prompt(
        &format!("printf poisoned > '{}'", poison.display()),
        &expected,
    ));
    assert!(
        matches!(
            response["error"]["code"].as_str(),
            Some("terminal_identity_mismatch" | "agent_not_found")
        ),
        "{response}"
    );
    assert!(!poison.exists(), "prompt reached the replacement shell");
    assert!(
        fs::read(&server.pi_input).unwrap_or_default().is_empty(),
        "prompt reached the old Pi"
    );
}

#[test]
fn guarded_agent_prompt_accepts_same_terminal_pi_input_once_and_intact() {
    let server = GuardServer::new("0");
    let pane = server.pane("original");
    server.barrier(&pane);
    server.start_worker(&pane);
    let expected = server.observed_worker_terminal();
    let text = "--assignment\nλ 日本語\nsecond line";

    let output = server.guarded_prompt(text, &expected);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected_bytes = format!("\x1b[200~{text}\x1b[201~\n").into_bytes();
    assert!(
        wait_until(
            Duration::from_secs(3),
            Duration::from_millis(25),
            || fs::read(&server.pi_input).ok().as_deref() == Some(expected_bytes.as_slice())
        ),
        "captured={:?}, expected={expected_bytes:?}",
        fs::read(&server.pi_input).ok()
    );
    assert_eq!(fs::read(&server.pi_input).unwrap(), expected_bytes);
}

#[test]
fn expected_terminal_capability_and_cli_flags_refuse_then_accept_exact_identity() {
    let server = GuardServer::new("0");
    let capabilities = server.request("ping", json!({}))["result"]["capabilities"].clone();
    assert_eq!(capabilities["expected_terminal_guard"], true);
    assert_eq!(capabilities["expected_terminal_agent_prompt_guard"], true);
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
