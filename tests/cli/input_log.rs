//! Real socket callers must retain their origin in the input log after exiting.
use super::harness::*;

const CASE: &str = "cases::input_log::api_input_log_keeps_exited_pane_and_external_callers";
const FIXTURE_ENV: &str = "HERDR_TEST_INPUT_LOG_FIXTURE";

#[test]
fn api_input_log_keeps_exited_pane_and_external_callers() {
    if let Some(base) = std::env::var_os(FIXTURE_ENV) {
        exercise_callers(Path::new(&base));
        return;
    }

    // Re-exec isolates the real server's state directory and short TMPDIR,
    // without changing the test runner's environment or the user's input log.
    let base = unique_test_dir();
    for dir in ["home", "tmp"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    let mut command = Command::new("/usr/bin/timeout");
    crate::test_command::sanitize_command_env(&mut command);
    let output = command
        .arg("40s")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", CASE, "--nocapture"])
        .env(FIXTURE_ENV, &base)
        .env("HOME", base.join("home"))
        .env("TMPDIR", base.join("tmp"))
        .env("XDG_STATE_HOME", base.join("state"))
        .output()
        .unwrap();
    // On failures too, stop the registered real server before deleting its files.
    cleanup_test_base(&base);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "input-log E2E failed: {:?}\n{}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn shell_quote(value: impl AsRef<std::ffi::OsStr>) -> String {
    format!(
        "'{}'",
        value.as_ref().to_str().unwrap().replace('\'', "'\\''")
    )
}

fn exited_receipt(path: &Path) -> serde_json::Value {
    let mut receipt = None;
    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(25), || {
            receipt = fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            receipt.is_some()
        }),
        "caller did not produce a receipt at {}",
        path.display()
    );
    let receipt = receipt.unwrap();
    assert!(receipt["response"].get("error").is_none(), "{receipt}");
    assert_eq!(receipt["response"]["result"]["type"], "ok", "{receipt}");
    let pid = receipt["pid"].as_u64().unwrap() as u32;
    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(5)),
        "caller {pid} must exit before reading the input log"
    );
    receipt
}

fn exercise_callers(base: &Path) {
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let caller_pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", caller_pane, "--direction", "right"],
    );
    let target_pane = split["result"]["pane"]["pane_id"].as_str().unwrap();
    let python = fs::canonicalize("/usr/bin/python3").unwrap();
    let caller_script = base.join("caller.py");
    fs::write(&caller_script, r#"import ctypes, json, os, pathlib, socket, sys
sock, pane, receipt_path, text = sys.argv[1:5]
if sys.argv[5:]:
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(15, ctypes.c_char_p(sys.argv[5].encode()), 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_NAME failed")
origin = {"pid": os.getpid(), "ppid": os.getppid(), "exe": pathlib.Path(os.readlink("/proc/self/exe")).name, "comm": pathlib.Path("/proc/self/comm").read_text().strip()}
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(8)
    connection.connect(sock)
    request = {"id": "input-log-e2e", "method": "pane.send_text", "params": {"pane_id": pane, "text": text, "allow_cross_pane": True}}
    connection.sendall((json.dumps(request) + "\n").encode())
    origin["response"] = json.loads(connection.makefile().readline())
pathlib.Path(receipt_path).write_text(json.dumps(origin))
"#).unwrap();

    // Reuse the agents.rs pattern: the program running inside a real pane
    // reports semantic agent state through the CLI, then names itself sender.
    let pane_receipt = base.join("pane-receipt.json");
    let pane_script = base.join("pane-caller.sh");
    let herdr_bin = shell_quote(env!("CARGO_BIN_EXE_herdr"));
    fs::write(&pane_script, format!(
        "#!/bin/sh\nset -eu\nexport HERDR_AGENT=pi\n{herdr_bin} pane report-agent \"$HERDR_PANE_ID\" --source custom:input-log-e2e --agent pi --state idle >/dev/null\n{herdr_bin} agent rename \"$HERDR_PANE_ID\" sender >/dev/null\n{} {} {} {} {} pane-origin-secret\n",
        shell_quote(&python), shell_quote(&caller_script), shell_quote(&socket_path),
        shell_quote(target_pane), shell_quote(&pane_receipt),
    )).unwrap();
    let launched = run_cli(
        &socket_path,
        &[
            "pane",
            "run",
            caller_pane,
            &format!("/bin/sh {}", shell_quote(&pane_script)),
        ],
    );
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let pane_origin = exited_receipt(&pane_receipt);

    // Check actual scope creation, not is-system-running: a degraded user
    // manager is usable. Fall back only if systemd-run cannot create a scope.
    let systemd_available = Command::new("/usr/bin/timeout")
        .args([
            "5s",
            "/usr/bin/systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .is_ok_and(|output| output.status.success());
    // Clear the unfinished input from the normal callers, then execute a
    // harmless command. Its standalone output cannot be satisfied by PTY echo.
    let renamed_text = "\u{15}printf '%s_%s\\n' DELIVERED_REVIEW 1\n";
    let mut external_origins = Vec::new();
    for (label, text, comm) in [
        ("external", "external-origin-secret", None),
        ("renamed", renamed_text, Some("renamed-worker")),
    ] {
        let unit = format!("herdr-input-log-{}-{label}", std::process::id());
        let external_receipt = base.join(format!("{label}-receipt.json"));
        let mut external = Command::new("/usr/bin/timeout");
        crate::test_command::sanitize_command_env(&mut external);
        external.arg("12s");
        if systemd_available {
            external
                .args(["/usr/bin/systemd-run", "--user", "--scope", "--quiet"])
                .arg(format!("--unit={unit}"))
                .arg("--");
        } else {
            // --fork detaches even if the runner is a process group leader;
            // --wait reaps the caller before reading its durable log record.
            external.args(["/usr/bin/setsid", "--fork", "--wait"]);
        }
        external
            .arg(&python)
            .arg(&caller_script)
            .arg(&socket_path)
            .arg(target_pane)
            .arg(&external_receipt)
            .arg(text);
        if let Some(comm) = comm {
            external.arg(comm);
        }
        let output = external.stdin(Stdio::null()).output().unwrap();
        assert!(
            output.status.success(),
            "{label} caller failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let origin = exited_receipt(&external_receipt);
        if let Some(comm) = comm {
            assert_eq!(origin["comm"], comm);
            assert_ne!(origin["exe"], origin["comm"], "{origin}");
        }
        external_origins.push((origin, text.len(), unit));
    }
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            let output = run_cli(
                &socket_path,
                &["pane", "read", target_pane, "--source", "recent"],
            );
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim() == "DELIVERED_REVIEW_1")
        }),
        "renamed caller's original input must reach the shell, not just return OK"
    );

    // All three socket owners are gone now. Do not reconstruct their metadata from
    // /proc while reading: the durable records must already hold the evidence.
    let log_path = base
        .join("state")
        .join(app_dir_name())
        .join("api-input.jsonl");
    let raw = fs::read_to_string(&log_path).unwrap();
    let records: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let record_for = |origin: &serde_json::Value| {
        let matching: Vec<_> = records
            .iter()
            .filter(|line| {
                line["method"] == "pane.send_text" && line["caller"]["pid"] == origin["pid"]
            })
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected one durable record for {origin}; got {records:?}"
        );
        matching[0]
    };
    let pane_record = record_for(&pane_origin);
    eprintln!("pane caller actual api-input.jsonl: {pane_record}");
    assert!(
        !raw.contains("origin-secret") && !raw.contains("DELIVERED_REVIEW"),
        "input text leaked into the log"
    );
    assert_eq!(pane_record["caller"]["pane"], caller_pane);
    assert_eq!(pane_record["caller"]["agent"], "sender");
    for (origin, _, unit) in &external_origins {
        let record = record_for(origin);
        eprintln!("external caller actual api-input.jsonl (systemd={systemd_available}): {record}");
        assert!(record["caller"]["pane"].is_null(), "{record}");
        assert!(record["caller"]["agent"].is_null(), "{record}");
        assert!(record["caller"]["session"].is_null(), "{record}");
        if systemd_available {
            assert_eq!(record["caller"]["unit"], format!("{unit}.scope"));
        }
    }
    for (origin, bytes) in std::iter::once((&pane_origin, "pane-origin-secret".len())).chain(
        external_origins
            .iter()
            .map(|(origin, bytes, _)| (origin, *bytes)),
    ) {
        let record = record_for(origin);
        assert_eq!(record["target_pane"], target_pane);
        assert_eq!(record["bytes"], bytes);
        assert_eq!(
            record["caller"]["exe"], origin["exe"],
            "caller.exe must be the actual executable basename, not mutable comm, captured before exit"
        );
        assert_eq!(
            record["caller"]["ppid"], origin["ppid"],
            "caller.ppid must be captured before the caller exits"
        );
    }
    drop(herdr);
}
