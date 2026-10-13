use super::harness::*;

#[test]
fn pane_close_only_removes_the_target_tab_when_other_tabs_exist() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let workspace_id = created_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let created_tab = run_cli(
        &socket_path,
        &["tab", "create", "--workspace", &workspace_id],
    );
    assert!(created_tab.status.success());
    let created_tab_json: serde_json::Value = serde_json::from_slice(&created_tab.stdout).unwrap();
    let second_root_pane_id = created_tab_json["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let closed = run_cli(&socket_path, &["pane", "close", &second_root_pane_id]);
    assert!(closed.status.success());
    let closed_json: serde_json::Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed_json["result"]["type"], "ok");

    let workspaces = run_cli(&socket_path, &["workspace", "list"]);
    assert!(workspaces.status.success());
    let workspaces_json: serde_json::Value = serde_json::from_slice(&workspaces.stdout).unwrap();
    assert_eq!(
        workspaces_json["result"]["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        workspaces_json["result"]["workspaces"][0]["workspace_id"],
        workspace_id
    );

    let tabs = run_cli(&socket_path, &["tab", "list", "--workspace", &workspace_id]);
    assert!(tabs.status.success());
    let tabs_json: serde_json::Value = serde_json::from_slice(&tabs.stdout).unwrap();
    assert_eq!(tabs_json["result"]["tabs"].as_array().unwrap().len(), 1);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_close_removes_the_workspace_when_it_closes_the_last_pane() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let root_pane_id = created_json["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let closed = run_cli(&socket_path, &["pane", "close", &root_pane_id]);
    assert!(closed.status.success());
    let closed_json: serde_json::Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed_json["result"]["type"], "ok");

    let workspaces = run_cli(&socket_path, &["workspace", "list"]);
    assert!(workspaces.status.success());
    let workspaces_json: serde_json::Value = serde_json::from_slice(&workspaces.stdout).unwrap();
    assert!(workspaces_json["result"]["workspaces"]
        .as_array()
        .unwrap()
        .is_empty());

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_run_read_and_wait_commands_work() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_cli_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let create = run_cli(
        &socket_path,
        &[
            "pane",
            "run",
            "1-1",
            "echo alpha && echo beta && printf 'ready\\n'",
        ],
    );
    assert!(create.status.success());

    let started = Instant::now();
    let waited = run_cli(
        &socket_path,
        &[
            "pane",
            "wait-output",
            "1-1",
            "--match",
            "ready",
            "--source",
            "recent",
            "--lines",
            "40",
            "--timeout",
            "5000",
        ],
    );
    let elapsed = started.elapsed();
    assert!(
        waited.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&waited.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "already-matching wait took {elapsed:?}"
    );
    let waited_json: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited_json["result"]["type"], "output_matched");

    let read = run_cli(
        &socket_path,
        &["pane", "read", "1-1", "--source", "recent", "--lines", "40"],
    );
    assert!(read.status.success());
    let text = String::from_utf8(read.stdout).unwrap();
    assert!(text.contains("alpha"));
    assert!(text.contains("ready"));

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn wait_output_matches_recent_unwrapped_text() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let token = "WRAP_WAIT_TEST_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789";
    let script = base.join("emit-long-token.sh");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{token}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    let run = run_cli(
        &socket_path,
        &["pane", "run", "1-1", &format!("sh {}", script.display())],
    );
    assert!(run.status.success());

    let waited = run_cli(
        &socket_path,
        &[
            "pane",
            "wait-output",
            "1-1",
            "--match",
            token,
            "--source",
            "recent",
            "--lines",
            "80",
            "--timeout",
            "5000",
        ],
    );
    assert!(
        waited.status.success(),
        "stderr: {} stdout: {}",
        String::from_utf8_lossy(&waited.stderr),
        String::from_utf8_lossy(&waited.stdout)
    );

    let read = run_cli(
        &socket_path,
        &[
            "pane",
            "read",
            "1-1",
            "--source",
            "recent-unwrapped",
            "--lines",
            "80",
        ],
    );
    assert!(read.status.success());
    let text = String::from_utf8(read.stdout).unwrap();
    assert!(text.contains(token));

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_pane_terminates_processes_inside_it() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let split = run_cli(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right"],
    );
    assert!(split.status.success());
    let split_json: serde_json::Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split_json["result"]["pane"]["pane_id"].as_str().unwrap();

    let pid_file = base.join("pane-close.pid");
    let command = format!(
        "python3 -c 'import os,time,pathlib; pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)'",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", pane_id, &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !pid_file.exists() {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(pid_file.exists(), "pid file was not created");

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(3)).unwrap_or_else(|err| {
        panic!("failed to read pane child pid: {err}");
    });
    assert!(process_exists(pid), "child process was not running");

    let closed = run_cli(&socket_path, &["pane", "close", pane_id]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(3)),
        "process {pid} survived pane close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_workspace_terminates_processes_inside_it() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let pid_file = base.join("workspace-close.pid");
    let command = format!(
        "python3 -c 'import os,time,pathlib; pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)'",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", "1-1", &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !pid_file.exists() {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(pid_file.exists(), "pid file was not created");

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(3)).unwrap_or_else(|err| {
        panic!("failed to read pane child pid: {err}");
    });
    assert!(process_exists(pid), "child process was not running");

    let closed = run_cli(&socket_path, &["workspace", "close", "1"]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(3)),
        "process {pid} survived workspace close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn workspace_ids_and_public_pane_ids_are_stable() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let ws1_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let ws1_id = ws1_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let split_12_json = run_cli_json(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right", "--no-focus"],
    );
    assert_eq!(
        split_12_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p2")
    );

    let split_13_json = run_cli_json(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "down", "--no-focus"],
    );
    assert_eq!(
        split_13_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p3")
    );

    let ws2_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/tmp", "--no-focus"],
    );
    let ws2_id = ws2_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(ws2_id, ws1_id);

    let ws2_focus = run_cli(&socket_path, &["workspace", "focus", &ws2_id]);
    assert!(
        ws2_focus.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ws2_focus.stderr)
    );

    let ws2_split_json = run_cli_json(
        &socket_path,
        &["pane", "split", "2-1", "--direction", "right", "--no-focus"],
    );
    assert_eq!(
        ws2_split_json["result"]["pane"]["pane_id"],
        format!("{ws2_id}:p2")
    );

    let ws3_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/", "--no-focus"],
    );
    let ws3_id = ws3_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(ws3_id, ws1_id);
    assert_ne!(ws3_id, ws2_id);

    let close_ws2 = run_cli(&socket_path, &["workspace", "close", &ws2_id]);
    assert!(
        close_ws2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&close_ws2.stderr)
    );

    let workspaces_json = run_cli_json(&socket_path, &["workspace", "list"]);
    let ids: Vec<String> = workspaces_json["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["workspace_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![ws1_id.clone(), ws3_id.clone()]);

    let new_ws_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/var/tmp", "--no-focus"],
    );
    let new_ws_id = new_ws_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_ws_id, ws1_id);
    assert_ne!(new_ws_id, ws2_id);
    assert_ne!(new_ws_id, ws3_id);

    let ws3_panes_json = run_cli_json(&socket_path, &["pane", "list", "--workspace", &ws3_id]);
    assert_eq!(
        ws3_panes_json["result"]["panes"][0]["pane_id"],
        format!("{ws3_id}:p1")
    );

    let close_middle = run_cli(&socket_path, &["pane", "close", &format!("{ws1_id}-2")]);
    assert!(
        close_middle.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&close_middle.stderr)
    );

    let ws1_panes_json = run_cli_json(&socket_path, &["pane", "list", "--workspace", &ws1_id]);
    let pane_ids: Vec<String> = ws1_panes_json["result"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["pane_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        pane_ids,
        vec![format!("{ws1_id}:p1"), format!("{ws1_id}:p3")]
    );

    let closed_lookup = run_cli(&socket_path, &["pane", "get", &format!("{ws1_id}:p2")]);
    assert!(
        !closed_lookup.status.success(),
        "closed pane id should not retarget: {}",
        String::from_utf8_lossy(&closed_lookup.stdout)
    );

    let split_14_json = run_cli_json(
        &socket_path,
        &[
            "pane",
            "split",
            &format!("{ws1_id}:p1"),
            "--direction",
            "right",
            "--no-focus",
        ],
    );
    assert_eq!(
        split_14_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p4")
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_shell_gets_herdr_socket_and_pane_env() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_env_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let env_capture = base.join("pane-env.txt");
    let ran = run_cli(
        &socket_path,
        &[
            "pane",
            "run",
            "1-1",
            &format!(
                "printf '%s\\n%s\\n' \"$HERDR_SOCKET_PATH\" \"$HERDR_PANE_ID\" > {}",
                env_capture.display()
            ),
        ],
    );
    assert!(ran.status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while Instant::now() < deadline {
        if env_capture.exists() {
            text = fs::read_to_string(&env_capture).unwrap();
            if text.contains(&socket_path.display().to_string()) && text.contains(&pane_id) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(env_capture.exists(), "env capture file was not created");
    assert!(
        text.contains(&socket_path.display().to_string()),
        "env file was: {text:?}"
    );
    assert!(text.contains(&pane_id), "env file was: {text:?}");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_resume_reports_transmit_cross_pane_opt_in() {
    for (method, command) in [
        ("pane.report_agent", "report-agent"),
        ("pane.report_agent_session", "report-agent-session"),
    ] {
        for opt_in in [false, true] {
            let base = unique_test_dir();
            fs::create_dir_all(&base).unwrap();
            let socket = base.join("herdr.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    assert!(Instant::now() < deadline, "CLI did not send a report");
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
                        write_fake_pong(
                            &mut stream,
                            &request,
                            "different-build-same-protocol",
                            CURRENT_PROTOCOL,
                        );
                        continue;
                    }
                    writeln!(
                        stream,
                        "{}",
                        serde_json::json!({"id": request["id"], "result": {"type": "ok"}})
                    )
                    .unwrap();
                    return request;
                }
            });
            let mut args = vec![
                "pane",
                command,
                "w1:p1",
                "--source",
                "custom:pi",
                "--agent",
                "pi",
            ];
            if command == "report-agent" {
                args.extend(["--state", "idle"]);
            }
            if opt_in {
                args.push("--allow-cross-pane");
            }
            args.extend(["--", "pi", "--resume", "cli-session"]);
            let output = run_cli(&socket, &args);
            let request = server.join().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["allow_cross_pane"], opt_in);
            assert_eq!(
                request["params"]["resume_argv"],
                serde_json::json!(["pi", "--resume", "cli-session"])
            );
            cleanup_test_base(&base);
        }
    }
}

#[test]
fn pane_agent_reports_accept_options_before_pane() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_agent_report_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // A custom report is released when the pane returns to its idle shell.
    // Hold a real foreground process, and use its connection as the readiness
    // event rather than racing the detector with a report over an empty shell.
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
    assert!(
        started.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let agent_process = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    ready_thread.join().unwrap();

    let state_report = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--state=working",
            "--message=parsing=works",
            &pane_id,
        ],
    );
    assert!(
        state_report.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&state_report.stderr)
    );
    assert!(state_report.stdout.is_empty());
    assert!(state_report.stderr.is_empty());

    let agent = run_cli_json(&socket_path, &["agent", "get", &pane_id]);
    assert_eq!(agent["result"]["agent"]["agent"], "cli-test");
    assert_eq!(agent["result"]["agent"]["agent_status"], "working");

    let session_report = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent-session",
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--seq=1",
            "--agent-session-id=session=1",
            "--session-start-source=startup",
            &pane_id,
        ],
    );
    assert!(
        session_report.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&session_report.stderr)
    );
    assert!(session_report.stdout.is_empty());
    assert!(session_report.stderr.is_empty());

    let resume_report = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--state=idle",
            "--seq=2",
            "--agent-session-id=session=1",
            &pane_id,
            "--",
            "cli-test",
            "--resume",
            "session=1",
        ],
    );
    assert!(
        resume_report.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&resume_report.stderr)
    );
    assert!(resume_report.stdout.is_empty());
    assert!(resume_report.stderr.is_empty());

    // Reports are applied before their replies: no wait/retry is needed for
    // the next lookup. Prompt guards must resolve this custom agent too, then
    // reject its unsupported process type (not claim that it was not found).
    let agent = run_cli_json(&socket_path, &["agent", "get", &pane_id]);
    assert_eq!(agent["result"]["agent"]["agent"], "cli-test");
    assert_eq!(agent["result"]["agent"]["agent_status"], "idle");
    let prompt = run_cli(&socket_path, &["agent", "prompt", &pane_id, "probe"]);
    assert!(!prompt.status.success());
    let error: serde_json::Value = serde_json::from_slice(&prompt.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_ready");

    cleanup_spawned_herdr(herdr, base);
    drop(agent_process);
}

/// A real server whose one pane runs a process titled `pi`, already detected as Pi. Shared by
/// the smarty-dev#10398 report E2Es below.
struct DetectedPiPane {
    herdr: SpawnedHerdr,
    base: PathBuf,
    socket_path: PathBuf,
    pane_id: String,
}

impl DetectedPiPane {
    fn spawn() -> Self {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket_path = runtime_dir.join("herdr.sock");
        let bin_dir = base.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        // A process titled `pi`, so the server detects the pane as Pi.
        let fake_pi = bin_dir.join("pi");
        fs::copy("/bin/sleep", &fake_pi).unwrap();

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
        let command = format!("{} 120", fake_pi.display());
        let started = run_cli(&socket_path, &["pane", "run", &pane_id, &command]);
        assert!(
            started.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        let pi = Self {
            herdr,
            base,
            socket_path,
            pane_id,
        };
        let mut last = serde_json::Value::Null;
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
                last = pi.pane_get();
                last["agent"] == "pi"
            }),
            "pi was never detected: {last}"
        );
        pi
    }

    fn pane_get(&self) -> serde_json::Value {
        run_cli_json(&self.socket_path, &["pane", "get", &self.pane_id])["result"]["pane"].clone()
    }

    fn cleanup(self) {
        cleanup_spawned_herdr(self.herdr, self.base);
    }
}

/// smarty-dev#10398 end to end: a real server, a real `pi` foreground process and the real
/// `herdr pane report-agent-session` CLI. A Pi `/reload` anchors an unanchored pane, a
/// same-path report without an id keeps the id, a reload for another session never replaces
/// the anchor, and an explicit `new` session still does.
#[cfg(target_os = "linux")]
#[test]
fn pi_reload_cli_reports_anchor_the_pane_and_keep_the_id_end_to_end() {
    let pi = DetectedPiPane::spawn();
    let (base, socket_path, pane_id) = (&pi.base, &pi.socket_path, &pi.pane_id);
    let detected = pi.pane_get();
    assert_eq!(
        detected["agent_session"],
        serde_json::Value::Null,
        "{detected}"
    );
    assert_eq!(
        detected["agent_session_id"],
        serde_json::Value::Null,
        "{detected}"
    );

    let path_a = base.join("pi-reload-a.jsonl").to_str().unwrap().to_string();
    let path_b = base.join("pi-reload-b.jsonl").to_str().unwrap().to_string();
    // Returns `pane get`'s (session path, session id) right after the report: reports are
    // applied before the reply, so no wait is needed.
    let report = |seq: u64, start: Option<&str>, path: &str, id: Option<&str>| {
        let seq = format!("--seq={seq}");
        let path = format!("--agent-session-path={path}");
        let start = start.map(|start| format!("--session-start-source={start}"));
        let id = id.map(|id| format!("--agent-session-id={id}"));
        let mut args = vec![
            "pane",
            "report-agent-session",
            "--source=herdr:pi",
            "--agent=pi",
            seq.as_str(),
            path.as_str(),
        ];
        args.extend(start.as_deref());
        args.extend(id.as_deref());
        args.push(pane_id.as_str());
        let output = run_cli(socket_path, &args);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pane = pi.pane_get();
        assert_eq!(pane["agent"], "pi", "{pane}");
        (
            pane["agent_session"]["value"].clone(),
            pane["agent_session_id"].clone(),
        )
    };
    let anchored_a = (serde_json::json!(path_a), serde_json::json!("pi-id-a"));

    assert_eq!(
        report(1, Some("reload"), &path_a, Some("pi-id-a")),
        anchored_a,
        "a reload anchors the unanchored pane"
    );
    assert_eq!(
        report(2, None, &path_a, None),
        anchored_a,
        "an id-less same-path report keeps the id"
    );
    assert_eq!(
        report(3, Some("reload"), &path_b, Some("pi-id-b")),
        anchored_a,
        "a reload for another session never replaces the anchor"
    );
    assert_eq!(
        report(4, Some("new"), &path_b, Some("pi-id-b")),
        (serde_json::json!(path_b), serde_json::json!("pi-id-b")),
        "an explicit new session still replaces it"
    );

    pi.cleanup();
}

/// smarty-dev#10398 end to end: a real server, a real `pi` foreground process,
/// a real `events.subscribe` stream, and the real `herdr pane report-agent-session`
/// CLI. The event must carry the actual id `pane get` shows and fire when only it
/// changes. A pre-v11 Pi extension re-reports its path without an id after `/reload`;
/// that report keeps the id (the id belongs to the session file), so it emits nothing,
/// and the id clears only together with the path.
#[test]
fn agent_session_changed_cli_reports_carry_the_actual_id_end_to_end() {
    let pi = DetectedPiPane::spawn();
    let (base, socket_path, pane_id) = (&pi.base, &pi.socket_path, &pi.pane_id);

    // The same socket path event subscribers use.
    let mut stream = UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": "session-id-events",
            "method": "events.subscribe",
            "params": {"subscriptions": [
                {"type": "pane.agent_session_changed", "pane_id": pane_id}
            ]}
        })
    )
    .unwrap();
    let mut events = BufReader::new(stream);
    let mut next_event = || {
        let mut line = String::new();
        let count = events.read_line(&mut line).unwrap();
        assert_ne!(count, 0, "event socket closed");
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let started = next_event();
    assert_eq!(
        started["result"]["type"], "subscription_started",
        "{started}"
    );

    let session_path = base.join("pi-a.jsonl");
    let path = session_path.to_str().unwrap().to_string();
    let report = |seq: u64, id: Option<&str>, path: &str| {
        let seq = format!("--seq={seq}");
        let path = format!("--agent-session-path={path}");
        let mut args = vec![
            "pane",
            "report-agent-session",
            "--source=herdr:pi",
            "--agent=pi",
            seq.as_str(),
            path.as_str(),
            "--session-start-source=startup",
        ];
        let id = id.map(|id| format!("--agent-session-id={id}"));
        if let Some(id) = id.as_deref() {
            args.push(id);
        }
        args.push(pane_id.as_str());
        let output = run_cli(socket_path, &args);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Reports are applied before the reply: `pane get` must already agree.
        pi.pane_get()["agent_session_id"].clone()
    };

    let null = serde_json::Value::Null;
    let id_a = serde_json::json!("pi-id-a");
    assert_eq!(report(1, None, &path), null, "path only");
    assert_eq!(
        report(2, Some("pi-id-a"), &path),
        id_a,
        "same path with an id"
    );
    assert_eq!(report(3, Some("pi-id-a"), &path), id_a, "repeat");
    // The sticky id (#247): an id-less re-report of the same path keeps the id, so
    // neither report changes the session and neither emits.
    assert_eq!(report(4, None, &path), id_a, "path only after /reload");
    assert_eq!(report(5, None, &path), id_a, "repeat");
    // A sentinel with a new path: events are ordered, so whatever arrives
    // before it is everything the five reports emitted. A different path still
    // clears the id, so the sentinel's own event carries that clear.
    let sentinel = base.join("pi-sentinel.jsonl");
    let sentinel = sentinel.to_str().unwrap().to_string();
    assert_eq!(report(6, None, &sentinel), null, "sentinel");

    let mut received = Vec::new();
    loop {
        let event = next_event();
        assert_eq!(event["event"], "pane.agent_session_changed", "{event}");
        let data = event["data"].clone();
        assert_eq!(data["pane_id"], *pane_id, "{event}");
        assert_eq!(data["agent"], "pi", "{event}");
        let is_sentinel = data["session_id"] == sentinel;
        received.push(data);
        if is_sentinel {
            break;
        }
    }
    let fields = |data: &serde_json::Value| {
        (
            data["session_id"].clone(),
            data["previous_session_id"].clone(),
            data["agent_session_id"].clone(),
            data["previous_agent_session_id"].clone(),
        )
    };
    let path_value = serde_json::json!(path);
    assert_eq!(
        received.iter().map(fields).collect::<Vec<_>>(),
        vec![
            (path_value.clone(), null.clone(), null.clone(), null.clone()),
            (
                path_value.clone(),
                path_value.clone(),
                id_a.clone(),
                null.clone()
            ),
            (
                serde_json::json!(sentinel),
                path_value.clone(),
                null.clone(),
                id_a.clone()
            ),
        ],
        "exactly two events before the sentinel: path only and the id set; the \
         same-path id-less reports keep the id and emit nothing"
    );
    for data in &received {
        // Present and explicitly null, not merely absent.
        let object = data.as_object().unwrap();
        assert!(object.contains_key("agent_session_id"), "{data}");
        assert!(object.contains_key("previous_agent_session_id"), "{data}");
    }

    pi.cleanup();
}

#[test]
fn pane_read_rejects_invalid_value_with_usage_error() {
    // Invalid option values fail as CLI usage errors before any server
    // contact: exit 2 with the plain parser message, not the old
    // `Error: Custom { ... }` io::Error wrapper from main.
    let socket_path = Path::new("/tmp/herdr-cli-invalid-values-no-server.sock");

    let read = run_cli(socket_path, &["pane", "read", "w1:p1", "--source", "bogus"]);
    assert_eq!(read.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&read.stderr);
    assert!(stderr.contains("invalid read source: bogus"));
    assert!(!stderr.contains("Error: Custom"));
}

/// smarty-dev#8152: clearing a pane's input poison is an operator action. A real
/// `herdr` CLI run inside a pane is refused for another pane and for its own pane
/// (plain shell pane: operator_origin_required; reported agent pane targeting
/// another pane: the cross-pane policy's cross_pane_input_denied). The same CLI
/// run outside every pane is the operator and succeeds. Drain-before-clear on a
/// poisoned pane is proven natively in pty::actor guard_tests, because no CLI
/// path can poison pane input (it needs a failed TCIFLUSH during guarded loss).
#[cfg(target_os = "linux")]
#[test]
fn clear_input_poison_is_refused_inside_panes_and_accepted_from_an_operator() {
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
    let source = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &source, "--direction", "right"],
    );
    let target = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Runs the real CLI from the source pane's shell and returns its JSON reply.
    let mut run = 0;
    let mut clear_from_source_pane = |pane: &str| -> serde_json::Value {
        run += 1;
        let out = base.join(format!("clear-{run}.json"));
        let done = base.join(format!("clear-{run}.done"));
        let command = format!(
            "'{}' pane clear-input-poison {pane} 2> '{}'; echo $? > '{}'",
            env!("CARGO_BIN_EXE_herdr"),
            out.display(),
            done.display()
        );
        assert!(run_cli(&socket_path, &["pane", "run", &source, &command])
            .status
            .success());
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(25), || {
                fs::read_to_string(&done).is_ok_and(|rc| rc.ends_with('\n'))
            }),
            "in-pane CLI did not finish"
        );
        assert_eq!(fs::read_to_string(&done).unwrap().trim(), "1");
        // The CLI prints its warnings, then the error response, on stderr.
        let stderr = fs::read_to_string(&out).unwrap();
        serde_json::from_str(stderr.lines().last().unwrap_or_default()).unwrap()
    };

    // A plain shell pane is pane-attributed: refused for another pane and its own.
    for pane in [target.clone(), source.clone()] {
        let refused = clear_from_source_pane(&pane);
        assert_eq!(
            refused["error"]["code"], "operator_origin_required",
            "{refused}"
        );
    }

    // Once the source pane is an agent, the cross-pane policy refuses it first.
    assert!(run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            &source,
            "--source",
            "custom:pi",
            "--agent",
            "pi",
            "--state",
            "idle",
        ],
    )
    .status
    .success());
    let refused = clear_from_source_pane(&target);
    assert_eq!(
        refused["error"]["code"], "cross_pane_input_denied",
        "{refused}"
    );
    let refused = clear_from_source_pane(&source);
    assert_eq!(
        refused["error"]["code"], "operator_origin_required",
        "{refused}"
    );

    // The operator: the same CLI outside every pane.
    // Strip every inherited HERDR_* so the run is outside every pane even when
    // the suite itself runs inside a Herdr pane.
    let mut operator = crate::test_command::herdr_command();
    strip_herdr_env(&mut operator);
    let cleared = operator
        .args(["pane", "clear-input-poison", &target])
        .env("HERDR_SOCKET_PATH", &socket_path)
        .output()
        .unwrap();
    assert!(
        cleared.status.success(),
        "{}",
        String::from_utf8_lossy(&cleared.stderr)
    );
    let cleared: serde_json::Value = serde_json::from_slice(&cleared.stdout).unwrap();
    assert_eq!(cleared["result"]["type"], "ok", "{cleared}");
    assert!(cleared["result"]["message"]
        .as_str()
        .unwrap()
        .contains("TCIFLUSH"));

    cleanup_spawned_herdr(herdr, base);
}

#[cfg(target_os = "linux")]
/// Removes every `HERDR_*` variable inherited from the test runner, so an
/// operator leg never carries pane markers of an enclosing Herdr session.
fn strip_herdr_env(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(key);
        }
    }
}

const CLIENT_CHILD_SOCKET: &str = "CLEAR_POISON_CLIENT_CHILD_SOCKET";
const CLIENT_CHILD_PANE: &str = "CLEAR_POISON_CLIENT_CHILD_PANE";
const CLIENT_CHILD_OUT: &str = "CLEAR_POISON_CLIENT_CHILD_OUT";

/// Opens a real client-protocol connection (endpoint hello, boot id from the
/// snapshot) and sends `pane.clear_input_poison` on the endpoint command lane.
#[cfg(target_os = "linux")]
fn clear_input_poison_over_client_protocol(
    client_socket: &Path,
    pane: &str,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(client_socket).map_err(|err| err.to_string())?;
    crate::support::client_shell_handshake(
        &mut stream,
        crate::support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        80,
        24,
    )?;
    let boot_id =
        crate::support::wait_for_client_shell_boot_id(&mut stream, Duration::from_secs(10))?;
    crate::support::client_shell_endpoint_request(
        &mut stream,
        &boot_id,
        &serde_json::json!({
            "id": "clear",
            "method": "pane.clear_input_poison",
            "params": {"pane_id": pane},
        }),
        Duration::from_secs(10),
    )
}

/// Child half of the test below: re-executed from this test binary inside a
/// Herdr pane. A no-op unless that test supplied its environment.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "re-executed inside a pane by clear_input_poison_client_lane_is_refused_inside_a_pane"]
fn clear_input_poison_client_protocol_child() {
    let Some(out) = std::env::var_os(CLIENT_CHILD_OUT) else {
        return;
    };
    let socket = std::env::var_os(CLIENT_CHILD_SOCKET).expect("client socket");
    let pane = std::env::var(CLIENT_CHILD_PANE).expect("pane");
    let result = match clear_input_poison_over_client_protocol(Path::new(&socket), &pane) {
        Ok(response) => response.to_string(),
        Err(error) => serde_json::json!({ "child_error": error }).to_string(),
    };
    let out = PathBuf::from(out);
    let partial = out.with_extension("partial");
    fs::write(&partial, result).unwrap();
    fs::rename(partial, out).unwrap();
}

/// smarty-dev#8152 round 2: the client-shell lane is not operator proof by
/// itself. A same-user process inside a pane can open the owner-readable
/// client socket, complete a valid endpoint hello, read the boot id from the
/// snapshot and send the allowlisted `pane.clear_input_poison`; it must be
/// refused. The CLI has no command that speaks the client protocol (and the
/// real `herdr` client refuses to start nested in a pane), so this is not a CLI
/// E2E: it drives the real server through a real client-protocol connection
/// from a child process (this test binary, re-executed) spawned inside a pane,
/// and from this test process outside every pane, which is the operator.
#[cfg(target_os = "linux")]
#[test]
fn clear_input_poison_client_lane_is_refused_inside_a_pane() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let source = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &source, "--direction", "right"],
    );
    let target = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // In-pane client: refused for another pane and for its own pane.
    let exe = std::env::current_exe().unwrap();
    for (run, pane) in [target.clone(), source.clone()].into_iter().enumerate() {
        let out = base.join(format!("client-clear-{run}.json"));
        let log = base.join(format!("client-clear-{run}.log"));
        let command = format!(
            "{CLIENT_CHILD_SOCKET}='{}' {CLIENT_CHILD_PANE}='{pane}' {CLIENT_CHILD_OUT}='{}' '{}' --exact cases::panes::clear_input_poison_client_protocol_child --include-ignored --nocapture > '{}' 2>&1",
            client_socket.display(),
            out.display(),
            exe.display(),
            log.display()
        );
        assert!(run_cli(&socket_path, &["pane", "run", &source, &command])
            .status
            .success());
        assert!(
            wait_until(Duration::from_secs(30), Duration::from_millis(50), || out
                .exists()),
            "in-pane client did not finish: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        let refused: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            refused["error"]["code"], "operator_origin_required",
            "in-pane client targeting {pane}: {refused}"
        );
    }

    // The operator: a client run outside every pane still clears it. It runs
    // in a re-executed child with every inherited HERDR_* removed, so the
    // result does not depend on whether the runner itself is inside a pane.
    let out = base.join("client-clear-operator.json");
    let log = base.join("client-clear-operator.log");
    let mut operator = std::process::Command::new(&exe);
    strip_herdr_env(&mut operator);
    let status = operator
        .args([
            "--exact",
            "cases::panes::clear_input_poison_client_protocol_child",
            "--include-ignored",
            "--nocapture",
        ])
        .env(CLIENT_CHILD_SOCKET, &client_socket)
        .env(CLIENT_CHILD_PANE, &target)
        .env(CLIENT_CHILD_OUT, &out)
        .stdout(fs::File::create(&log).unwrap())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success() && out.exists(),
        "operator client failed: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    let cleared: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(cleared["result"]["type"], "ok", "{cleared}");
    assert!(cleared["result"]["message"]
        .as_str()
        .unwrap()
        .contains("TCIFLUSH"));

    cleanup_spawned_herdr(herdr, base);
}
