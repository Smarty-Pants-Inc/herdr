use super::harness::*;

#[test]
fn agent_start_waits_through_unknown_then_rejects_blocked() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut pane_stream, pane_line) = accept_fake_cli_operation(&listener);
        let pane: serde_json::Value = serde_json::from_str(&pane_line).unwrap();
        assert_eq!(pane["method"], "pane.get");
        assert_eq!(pane["params"]["pane_id"], "w1:p1");
        writeln!(
            pane_stream,
            "{}",
            serde_json::json!({
                "id": pane["id"],
                "result": {
                    "type": "pane_info",
                    "pane": { "terminal_id": "term_1" }
                }
            })
        )
        .unwrap();
        pane_stream.flush().unwrap();

        let (mut start_stream, start_line) = accept_fake_cli_operation(&listener);
        let start: serde_json::Value = serde_json::from_str(&start_line).unwrap();
        assert_eq!(start["method"], "agent.start");
        writeln!(
            start_stream,
            "{}",
            serde_json::json!({
                "id": start["id"],
                "result": {
                    "type": "agent_started",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer"
                    },
                    "argv": ["opencode"]
                }
            })
        )
        .unwrap();
        start_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": null,
                        "agent_status": "unknown",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": "opencode",
                        "agent_status": "blocked",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent", "start", "reviewer", "--kind", "opencode", "--pane", "w1:p1",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    assert!(started.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_ready");

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellReadinessOutcome {
    Started,
    Busy,
    TerminalChanged,
}

fn check_agent_start_after_shell_readiness_transition(outcome: ShellReadinessOutcome) {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    let server = thread::spawn(move || {
        let initial_methods = ["pane.get", "agent.start", "pane.get", "pane.process_info"];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut operations = 0;
        let mut starts = 0;
        let mut pane_gets = 0;
        let mut snapshots = 0;
        loop {
            let (mut stream, line) = if operations < initial_methods.len() {
                accept_fake_cli_operation(&listener)
            } else {
                // The buggy CLI exits without another start. Do not block waiting
                // for that request; also accept a fix that skips the pre-retry snapshot.
                if done_rx.try_recv().is_ok() || Instant::now() >= deadline {
                    break;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(err) => panic!("fake server accept failed: {err}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
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
                (stream, line)
            };
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if operations < initial_methods.len() {
                assert_eq!(request["method"], initial_methods[operations]);
            }
            let mut response = serde_json::json!({ "id": request["id"] });
            match request["method"].as_str().unwrap() {
                "pane.get" => {
                    assert_eq!(request["params"]["pane_id"], "w1:p1");
                    pane_gets += 1;
                    let terminal =
                        if outcome == ShellReadinessOutcome::TerminalChanged && pane_gets >= 3 {
                            "term_2"
                        } else {
                            "term_1"
                        };
                    response["result"] = serde_json::json!({
                        "type": "pane_info", "pane": { "terminal_id": terminal }
                    });
                }
                "pane.process_info" => {
                    assert_eq!(request["params"]["pane_id"], "w1:p1");
                    snapshots += 1;
                    let processes = if snapshots == 1 {
                        serde_json::json!([
                            { "pid": 10, "name": "bash", "argv": ["/bin/bash"] },
                            { "pid": 11, "name": "startup-helper" }
                        ])
                    } else {
                        // Non-atomic exec observation: old comm, no argv. This is
                        // not authority to return the previously cached busy error.
                        serde_json::json!([{ "pid": 10, "name": "delayed-shell" }])
                    };
                    response["result"] = serde_json::json!({
                        "type": "pane_process_info",
                        "process_info": {
                            "pane_id": "w1:p1", "shell_pid": 10,
                            "foreground_process_group_id": 10,
                            "foreground_processes": processes
                        }
                    });
                }
                "agent.start" => {
                    assert_eq!(request["params"]["pane_id"], "w1:p1");
                    assert_eq!(request["params"]["name"], "reviewer");
                    assert_eq!(request["params"]["kind"], "pi");
                    starts += 1;
                    assert!(starts <= 2, "unexpected extra launch attempt");
                    if starts == 2 {
                        assert_ne!(outcome, ShellReadinessOutcome::TerminalChanged);
                        assert!(pane_gets >= 3, "retry must recheck the pinned terminal");
                    }
                    if starts == 2 && outcome == ShellReadinessOutcome::Started {
                        response["result"] = serde_json::json!({
                            "type": "agent_started",
                            "agent": { "pane_id": "w1:p1", "terminal_id": "term_1", "name": "reviewer" },
                            "argv": ["pi"]
                        });
                    } else {
                        response["error"] = serde_json::json!({
                            "code": "agent_pane_busy",
                            "message": if starts == 1 { "initial busy" } else { "fresh busy after exec" }
                        });
                    }
                }
                "agent.get" => {
                    assert_eq!(outcome, ShellReadinessOutcome::Started);
                    assert_eq!(starts, 2);
                    assert_eq!(request["params"]["target"], "reviewer");
                    response["result"] = serde_json::json!({
                        "type": "agent_info",
                        "agent": {
                            "pane_id": "w1:p1", "terminal_id": "term_1", "name": "reviewer",
                            "agent": "pi", "agent_status": "idle",
                            "interactive_ready": true, "launch_pending": false
                        }
                    });
                }
                method => panic!("unexpected fake-server operation: {method}"),
            }
            writeln!(stream, "{response}").unwrap();
            stream.flush().unwrap();
            operations += 1;
            if operations == initial_methods.len() {
                listener.set_nonblocking(true).unwrap();
            }
        }
        (starts, snapshots, pane_gets)
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "reviewer",
            "--kind",
            "pi",
            "--pane",
            "w1:p1",
            "--timeout",
            "8000",
        ],
    );
    let _ = done_tx.send(());
    let (starts, snapshots, pane_gets) = server.join().unwrap();
    cleanup_test_base(&base);
    assert!(
        snapshots >= 1,
        "initial busy must observe shell initialization"
    );
    assert!(pane_gets >= 3, "retry must observe the terminal pin again");
    assert_eq!(
        starts,
        if outcome == ShellReadinessOutcome::TerminalChanged {
            1
        } else {
            2
        },
        "{outcome:?}: CLI must let a fresh same-pane start decide; stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    if outcome == ShellReadinessOutcome::Started {
        assert!(
            started.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        assert!(started.stderr.is_empty());
        let response: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
        assert_eq!(response["result"]["type"], "agent_started");
        assert_eq!(response["result"]["agent"]["terminal_id"], "term_1");
        assert_eq!(response["result"]["agent"]["interactive_ready"], true);
    } else {
        assert_eq!(started.status.code(), Some(1));
        assert!(started.stdout.is_empty());
        let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
        assert_eq!(error["error"]["code"], "agent_pane_busy");
        assert_eq!(
            error["error"]["message"],
            if outcome == ShellReadinessOutcome::Busy {
                "fresh busy after exec"
            } else {
                "initial busy"
            }
        );
    }
}

#[test]
fn agent_start_retries_after_transient_process_info_and_respects_fresh_success() {
    check_agent_start_after_shell_readiness_transition(ShellReadinessOutcome::Started);
}

#[test]
fn agent_start_retries_after_transient_process_info_and_respects_fresh_busy() {
    check_agent_start_after_shell_readiness_transition(ShellReadinessOutcome::Busy);
}

#[test]
fn agent_start_does_not_retry_after_the_target_terminal_changes() {
    check_agent_start_after_shell_readiness_transition(ShellReadinessOutcome::TerminalChanged);
}

#[test]
fn prompt_wait_is_sent_as_one_agent_request() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut prompt_stream, prompt_line) = accept_fake_cli_operation(&listener);
        let prompt: serde_json::Value = serde_json::from_str(&prompt_line).unwrap();
        assert_eq!(prompt["method"], "agent.prompt");
        assert_eq!(prompt["params"]["target"], "w1:p1");
        assert_eq!(
            prompt["params"]["wait"]["until"],
            serde_json::json!(["idle"])
        );
        assert!(prompt["params"]["wait"].get("timeout_ms").is_none());
        writeln!(
            prompt_stream,
            "{}",
            serde_json::json!({
                "id": prompt["id"],
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer",
                        "agent": "pi",
                        "agent_status": "idle",
                        "workspace_id": "w1",
                        "tab_id": "w1:t1",
                        "focused": true,
                        "revision": 0
                    }
                }
            })
        )
        .unwrap();
        prompt_stream.flush().unwrap();
    });

    let prompted = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "w1:p1",
            "review this",
            "--wait",
            "--until",
            "idle",
        ],
    );
    assert!(
        prompted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prompted.stderr)
    );
    let prompted: serde_json::Value = serde_json::from_slice(&prompted.stdout).unwrap();
    assert_eq!(prompted["result"]["agent"]["name"], "reviewer");

    server.join().unwrap();
    cleanup_test_base(&base);
}
