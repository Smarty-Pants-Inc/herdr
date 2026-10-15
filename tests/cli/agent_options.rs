use super::harness::*;

#[test]
fn agent_wait_accepts_repeated_until_and_exits_when_one_status_matches() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin_dir = base.join("bin");

    fs::create_dir_all(&bin_dir).unwrap();
    let fake_pi = bin_dir.join("pi");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf 'starting\\n'\nsleep 4\nprintf 'Working...\\n'\nsleep 1\nprintf '\\033[2J\\033[Hdone\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake_pi).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_pi, perms).unwrap();
    }

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path_override = format!("{}:{}", bin_dir.display(), inherited_path);
    let herdr = spawn_herdr_with_path(
        &config_home,
        &runtime_dir,
        &socket_path,
        Some(Path::new(&path_override)),
    );

    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_cli_2","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let start_pi = run_cli(&socket_path, &["pane", "run", &pane_id, "pi"]);
    assert!(start_pi.status.success());
    assert!(wait_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        || run_cli(&socket_path, &["agent", "get", &pane_id])
            .status
            .success()
    ));

    let waited = run_cli(
        &socket_path,
        &[
            "agent",
            "wait",
            &pane_id,
            "--until",
            "blocked",
            "--until",
            "idle",
            "--timeout",
            "10000",
        ],
    );
    assert!(
        waited.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited_json: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited_json["result"]["agent"]["agent_status"], "idle");
    assert_eq!(waited_json["result"]["agent"]["agent"], "pi");

    cleanup_spawned_herdr(herdr, base);
}

/// Real CLI against a scripted socket: answer version pings, reply to the first
/// other request with `reply`, and return that request.
fn agent_prompt_against_scripted_server(
    args: &[&str],
    reply: serde_json::Value,
) -> (std::process::Output, serde_json::Value) {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket = base.join("herdr.sock");
    let server = scripted_server(&socket, reply);
    let output = run_cli(&socket, args);
    let request = server.join().unwrap();
    cleanup_test_base(&base);
    (output, request)
}

fn scripted_server(
    socket: &Path,
    reply: serde_json::Value,
) -> thread::JoinHandle<serde_json::Value> {
    scripted_server_with_capabilities(
        socket,
        reply,
        serde_json::json!({"live_handoff": true, "detached_server_daemon": true}),
    )
}

/// Like `scripted_server`, but the pong advertises `capabilities`.
fn scripted_server_with_capabilities(
    socket: &Path,
    reply: serde_json::Value,
    capabilities: serde_json::Value,
) -> thread::JoinHandle<serde_json::Value> {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "CLI did not send a request");
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(err) => panic!("accept: {err}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if request["method"] == "ping" {
                let pong = serde_json::json!({"id": request["id"], "result": {
                    "type": "pong",
                    "version": "different-build-same-protocol",
                    "protocol": CURRENT_PROTOCOL,
                    "capabilities": capabilities,
                }});
                writeln!(stream, "{pong}").unwrap();
                stream.flush().unwrap();
                continue;
            }
            let mut reply = reply;
            reply["id"] = request["id"].clone();
            writeln!(stream, "{reply}").unwrap();
            return request;
        }
    })
}

// smarty-dev#9677 review trace 1: an opt-in right after TARGET followed by an
// option is the literal TEXT, exactly as before; --wait stays an option.
#[test]
fn agent_prompt_flag_text_before_an_option_stays_literal() {
    let (output, request) = agent_prompt_against_scripted_server(
        &["agent", "prompt", "w1:p2", "--allow-cross-pane", "--wait"],
        serde_json::json!({"result": {"type": "agent_prompted"}}),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["method"], "agent.prompt");
    assert_eq!(request["params"]["target"], "w1:p2");
    assert_eq!(request["params"]["text"], "--allow-cross-pane");
    assert!(request["params"]["wait"].is_object(), "{request}");
    assert_ne!(request["params"]["allow_cross_pane"], true);
}

// smarty-dev#9677 review trace 3, as on master: without an opt-in the first two
// argv are the literal TARGET and TEXT, so `--session=work` there is the TEXT
// and selects no session, and `--wait --timeout 1000` stay options. A session
// after TEXT is still applied.
#[test]
fn agent_prompt_global_in_the_text_slot_is_literal_text_as_on_master() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let session_dir = config_home.join(app_dir).join("sessions").join("work");
    fs::create_dir_all(&session_dir).unwrap();
    let default_socket = base.join("herdr.sock");
    let session_socket = session_dir.join("herdr.sock");
    let prompted = serde_json::json!({"result": {"type": "agent_prompted"}});
    for (args, socket, text) in [
        (
            &[
                "agent",
                "prompt",
                "w1:p1",
                "--session=work",
                "--wait",
                "--timeout",
                "1000",
            ][..],
            &default_socket,
            "--session=work",
        ),
        (
            &[
                "agent",
                "prompt",
                "w1:p1",
                "hi",
                "--session=work",
                "--wait",
                "--timeout",
                "1000",
            ][..],
            &session_socket,
            "hi",
        ),
    ] {
        let server = scripted_server(socket, prompted.clone());
        let mut command = crate::test_command::herdr_command();
        command
            .args(args)
            .env("HERDR_SOCKET_PATH", &default_socket)
            .env("XDG_CONFIG_HOME", &config_home);
        let output = command.output().unwrap();
        let request = server.join().unwrap();
        fs::remove_file(socket).unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(request["method"], "agent.prompt", "{args:?}");
        assert_eq!(request["params"]["target"], "w1:p1", "{args:?}");
        assert_eq!(request["params"]["text"], text, "{args:?}");
        assert_eq!(
            request["params"]["wait"]["timeout_ms"], 1000,
            "{args:?}: {request}"
        );
        assert_ne!(request["params"]["allow_cross_pane"], true, "{args:?}");
    }
    cleanup_test_base(&base);
}

// smarty-dev#9677 review trace 2: globals around a leading opt-in are applied
// (--remote then refuses a subcommand), and TARGET is never shifted onto them.
#[test]
fn agent_prompt_leading_opt_in_applies_surrounding_globals() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let missing_socket = base.join("no-server.sock");
    let cli = |args: &[&str]| {
        let output = run_cli(&missing_socket, args);
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    for args in [
        &[
            "agent",
            "prompt",
            "--allow-cross-pane",
            "--session=work",
            "w1:p2",
            "--remote=host",
        ][..],
        &[
            "agent",
            "prompt",
            "--allow-cross-pane",
            "--session=work",
            "w1:p2",
            "--remote=host",
            "hi",
        ][..],
    ] {
        let (code, stderr) = cli(args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains("--remote can only be used with the default launch command"),
            "{args:?}: {stderr}"
        );
    }
    // Without --remote and without TEXT: a clean usage error, nothing sent.
    let (code, stderr) = cli(&[
        "agent",
        "prompt",
        "--allow-cross-pane",
        "--session=work",
        "w1:p2",
    ]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("agent prompt requires text"), "{stderr}");

    // With TEXT the prompt goes to the named session's socket, with the opt-in.
    let config_home = base.join("config");
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let session_dir = config_home.join(app_dir).join("sessions").join("work");
    fs::create_dir_all(&session_dir).unwrap();
    let session_socket = session_dir.join("herdr.sock");
    for args in [
        &[
            "agent",
            "prompt",
            "--allow-cross-pane",
            "--session=work",
            "w1:p2",
            "hi",
        ][..],
        &[
            "agent",
            "prompt",
            "--allow-cross-pane",
            "--session",
            "work",
            "w1:p2",
            "hi",
        ][..],
        &[
            "agent",
            "prompt",
            "w1:p2",
            "--allow-cross-pane",
            "--session=work",
            "hi",
        ][..],
    ] {
        let server = scripted_server(
            &session_socket,
            serde_json::json!({"result": {"type": "agent_prompted"}}),
        );
        let mut command = crate::test_command::herdr_command();
        command
            .args(args)
            .env("HERDR_SOCKET_PATH", &missing_socket)
            .env("XDG_CONFIG_HOME", &config_home);
        let output = command.output().unwrap();
        let request = server.join().unwrap();
        fs::remove_file(&session_socket).unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(request["method"], "agent.prompt", "{args:?}");
        assert_eq!(request["params"]["target"], "w1:p2", "{args:?}");
        assert_eq!(request["params"]["text"], "hi", "{args:?}");
        assert_eq!(request["params"]["allow_cross_pane"], true, "{args:?}");
    }
    cleanup_test_base(&base);
}

// smarty-dev#9677: --allow-cross-pane is accepted before TARGET, between TARGET
// and TEXT, and after TEXT, and each sends TARGET/TEXT with the opt-in.
#[test]
fn agent_prompt_accepts_cross_pane_opt_in_anywhere() {
    let prompted = serde_json::json!({"result": {"type": "agent_prompted"}});
    for args in [
        &[
            "agent",
            "prompt",
            "--allow-cross-pane",
            "w1:p2",
            "hello there",
        ][..],
        &[
            "agent",
            "prompt",
            "w1:p2",
            "--allow-cross-pane",
            "hello there",
        ][..],
        &[
            "agent",
            "prompt",
            "w1:p2",
            "hello there",
            "--allow-cross-pane",
        ][..],
    ] {
        let (output, request) = agent_prompt_against_scripted_server(args, prompted.clone());
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(request["method"], "agent.prompt", "{args:?}");
        assert_eq!(request["params"]["target"], "w1:p2", "{args:?}");
        assert_eq!(request["params"]["text"], "hello there", "{args:?}");
        assert_eq!(request["params"]["allow_cross_pane"], true, "{args:?}");
    }
}

// Merge guard with master: a leading or after-TARGET opt-in combines with the
// session/pane/status expectations and wait options (spaced and `=` forms),
// all of which reach the wire on the checked method.
#[test]
fn agent_prompt_opt_in_combines_with_expectations_and_wait_options() {
    let capabilities = serde_json::json!({
        "live_handoff": false,
        "expected_agent_session_guard": true,
        "expected_agent_status_guard": true,
    });
    for (args, method, status) in [
        (
            &[
                "agent",
                "prompt",
                "--allow-cross-pane",
                "w1:p2",
                "hi",
                "--expected-session",
                "session-a",
                "--expected-pane=w1:p2",
                "--expected-status",
                "idle",
                "--wait",
                "--until=done",
                "--timeout",
                "1500",
            ][..],
            "agent.prompt_status_checked",
            Some("idle"),
        ),
        (
            &[
                "agent",
                "prompt",
                "w1:p2",
                "--allow-cross-pane",
                "hi",
                "--expected-session=session-a",
                "--expected-pane",
                "w1:p2",
                "--wait",
                "--until",
                "done",
                "--timeout=1500",
            ][..],
            "agent.prompt_session_checked",
            None,
        ),
    ] {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        let socket = base.join("herdr.sock");
        let server = scripted_server_with_capabilities(
            &socket,
            serde_json::json!({"result": {"type": "agent_prompted"}}),
            capabilities.clone(),
        );
        let output = run_cli(&socket, args);
        let request = server.join().unwrap();
        cleanup_test_base(&base);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let params = &request["params"];
        assert_eq!(request["method"], method, "{args:?}: {request}");
        assert_eq!(params["target"], "w1:p2", "{args:?}");
        assert_eq!(params["text"], "hi", "{args:?}");
        assert_eq!(params["allow_cross_pane"], true, "{args:?}");
        assert_eq!(params["expected_agent_session_id"], "session-a", "{args:?}");
        assert_eq!(params["expected_pane_id"], "w1:p2", "{args:?}");
        match status {
            Some(status) => assert_eq!(params["expected_agent_status"], status, "{args:?}"),
            None => assert!(params.get("expected_agent_status").is_none(), "{args:?}"),
        }
        assert_eq!(
            params["wait"],
            serde_json::json!({"until": ["done"], "timeout_ms": 1500}),
            "{args:?}"
        );
    }
}

// Removing the flag would leave no TEXT, so it stays the literal prompt text.
#[test]
fn agent_prompt_flag_shaped_text_stays_literal_without_opt_in() {
    let (output, request) = agent_prompt_against_scripted_server(
        &["agent", "prompt", "w1:p2", "--allow-cross-pane"],
        serde_json::json!({"result": {"type": "agent_prompted"}}),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["method"], "agent.prompt");
    assert_eq!(request["params"]["target"], "w1:p2");
    assert_eq!(request["params"]["text"], "--allow-cross-pane");
    // A false opt-in is omitted on the wire.
    assert_ne!(request["params"]["allow_cross_pane"], true);
}

// Against the real server (#265): the input-origin guard is advisory, so an
// unattributable non-plugin caller is not refused as input_origin_unknown, with
// or without the opt-in in any argv position, nor with flag-shaped literal TEXT.
// This custom agent then answers agent_not_ready loudly (non-zero exit, the
// error on stderr, nothing on stdout).
#[test]
fn agent_prompt_input_origin_unknown_is_advisory_with_or_without_compatibility_flag() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Hold a real foreground process so the custom report is not released
    // when the pane returns to its idle shell.
    let ready_path = base.join("agent-ready.sock");
    let listener = UnixListener::bind(&ready_path).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let ready_thread = thread::spawn(move || {
        ready_tx.send(listener.accept().unwrap().0).unwrap();
    });
    let command = format!(
        "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"{}\"); s.recv(1)'",
        ready_path.display()
    );
    let started = run_cli(&socket_path, &["pane", "run", &pane_id, &command]);
    assert!(started.status.success());
    let agent_process = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    ready_thread.join().unwrap();
    let reported = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            &pane_id,
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--state=idle",
        ],
    );
    assert!(
        reported.status.success(),
        "{}",
        String::from_utf8_lossy(&reported.stderr)
    );

    let unattributable = |args: &[&str]| {
        let mut command = crate::test_command::herdr_command();
        command
            .args(args)
            .env("HERDR_SOCKET_PATH", &socket_path)
            .env("HERDR_ENV", "0");
        command.output().unwrap()
    };
    let error_code = |output: &std::process::Output| {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        error["error"]["code"].as_str().unwrap().to_owned()
    };

    // No opt-in and flag-shaped literal TEXT get the same answer as the opt-in.
    for args in [
        &["agent", "prompt", &pane_id, "probe"][..],
        &["agent", "prompt", &pane_id, "--allow-cross-pane"][..],
        &["agent", "prompt", "--allow-cross-pane", &pane_id, "probe"][..],
        &["agent", "prompt", &pane_id, "--allow-cross-pane", "probe"][..],
        &["agent", "prompt", &pane_id, "probe", "--allow-cross-pane"][..],
    ] {
        assert_eq!(
            error_code(&unattributable(args)),
            "agent_not_ready",
            "{args:?}"
        );
    }

    cleanup_spawned_herdr(herdr, base);
    drop(agent_process);
}
