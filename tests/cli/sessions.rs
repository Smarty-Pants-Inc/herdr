use super::harness::*;

#[test]
fn named_sessions_use_separate_servers_and_workspace_state() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");

    wait_for_socket(
        &named_session_socket(&config_home, "alpha"),
        Duration::from_secs(5),
    );
    wait_for_socket(
        &named_session_socket(&config_home, "beta"),
        Duration::from_secs(5),
    );

    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "alpha",
            "workspace",
            "create",
            "--label",
            "alpha-ws",
            "--no-focus",
        ],
    );
    run_named_cli_json(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "beta",
            "workspace",
            "create",
            "--label",
            "beta-ws",
            "--no-focus",
        ],
    );

    let alpha_list = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "alpha", "workspace", "list"],
    );
    let beta_list = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["--session", "beta", "workspace", "list"],
    );

    let alpha_labels: Vec<_> = alpha_list["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workspace| workspace["label"].as_str().unwrap())
        .collect();
    let beta_labels: Vec<_> = beta_list["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workspace| workspace["label"].as_str().unwrap())
        .collect();

    assert_eq!(alpha_labels, vec!["alpha-ws"]);
    assert_eq!(beta_labels, vec!["beta-ws"]);

    let beta_via_explicit_session = run_named_cli_with_socket_override(
        &config_home,
        &runtime_dir,
        &["--session", "beta", "workspace", "list"],
        Some(&named_session_socket(&config_home, "alpha")),
    );
    assert!(
        beta_via_explicit_session.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&beta_via_explicit_session.stderr)
    );
    let beta_via_explicit_session: serde_json::Value =
        serde_json::from_slice(&beta_via_explicit_session.stdout).unwrap();
    let labels_via_explicit: Vec<_> = beta_via_explicit_session["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workspace| workspace["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels_via_explicit, vec!["beta-ws"]);

    let human_sessions = run_named_cli(&config_home, &runtime_dir, &["session", "list"]);
    assert!(human_sessions.status.success());
    let human_sessions = String::from_utf8_lossy(&human_sessions.stdout);
    assert!(human_sessions.contains("name"), "stdout: {human_sessions}");
    assert!(
        human_sessions.contains("status"),
        "stdout: {human_sessions}"
    );
    assert!(human_sessions.contains("alpha"), "stdout: {human_sessions}");
    assert!(
        human_sessions.contains("running"),
        "stdout: {human_sessions}"
    );
    assert!(
        human_sessions.contains("/sessions/beta"),
        "stdout: {human_sessions}"
    );

    let sessions = run_named_cli_json(&config_home, &runtime_dir, &["session", "list", "--json"]);
    let sessions = sessions["sessions"].as_array().unwrap();
    let default_session = sessions
        .iter()
        .find(|session| session["name"] == "default")
        .unwrap();
    let alpha_session = sessions
        .iter()
        .find(|session| session["name"] == "alpha")
        .unwrap();
    let beta_session = sessions
        .iter()
        .find(|session| session["name"] == "beta")
        .unwrap();
    assert_eq!(default_session["default"], true);
    assert_eq!(default_session["running"], false);
    assert_eq!(alpha_session["running"], true);
    assert_eq!(beta_session["running"], true);
    assert!(alpha_session["socket_path"]
        .as_str()
        .unwrap()
        .ends_with("/sessions/alpha/herdr.sock"));
    assert!(beta_session["session_dir"]
        .as_str()
        .unwrap()
        .ends_with("/sessions/beta"));

    let delete_running = run_named_cli(&config_home, &runtime_dir, &["session", "delete", "alpha"]);
    assert_eq!(delete_running.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&delete_running.stderr).contains("stop it before deleting"),
        "stderr: {}",
        String::from_utf8_lossy(&delete_running.stderr)
    );

    let delete_default = run_named_cli(
        &config_home,
        &runtime_dir,
        &["session", "delete", "default"],
    );
    assert_eq!(delete_default.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&delete_default.stderr).contains("default session"),
        "stderr: {}",
        String::from_utf8_lossy(&delete_default.stderr)
    );

    let stopped_alpha = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["session", "stop", "alpha", "--json"],
    );
    assert_eq!(stopped_alpha["stopped"], true);
    assert_eq!(stopped_alpha["session"]["running"], false);

    let deleted_alpha = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["session", "delete", "alpha", "--json"],
    );
    assert_eq!(deleted_alpha["deleted"], true);
    assert!(!config_home
        .join(app_dir_name())
        .join("sessions")
        .join("alpha")
        .exists());

    let _ = run_named_cli(&config_home, &runtime_dir, &["session", "stop", "beta"]);
    drop(alpha);
    drop(beta);
    cleanup_test_base(&base);
}

#[test]
fn dead_server_cli_reports_one_session_aware_json_line() {
    fn assert_server_not_running(
        output: std::process::Output,
        socket_path: &Path,
        attach_command: &str,
    ) {
        assert_eq!(
            output.status.code(),
            Some(1),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "server errors belong on stderr");

        let stderr = String::from_utf8(output.stderr).unwrap();
        let lines: Vec<_> = stderr.lines().collect();
        assert_eq!(lines.len(), 1, "expected exactly one JSON line: {stderr:?}");

        let response: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(response["id"], "cli:workspace:create");
        assert_eq!(response["error"]["code"], "server_not_running");
        assert_eq!(
            response["error"]["message"],
            format!(
                "no herdr server is running at {}; run `{attach_command}` to start or attach it",
                socket_path.display()
            )
        );
    }

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);

    let named_socket = named_session_socket(&config_home, "foo");
    let missing = run_named_cli(
        &config_home,
        &runtime_dir,
        &["--session", "foo", "workspace", "create"],
    );
    assert_server_not_running(missing, &named_socket, "herdr session attach foo");

    let stale_socket = runtime_dir.join("stale.sock");
    drop(UnixListener::bind(&stale_socket).unwrap());
    let stale = crate::test_command::herdr_command()
        .args(["workspace", "create"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HERDR_SOCKET_PATH", &stale_socket)
        .env("HERDR_SESSION", "unrelated")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .output()
        .unwrap();
    assert_server_not_running(stale, &stale_socket, "herdr");

    cleanup_test_base(&base);
}

#[test]
fn integration_commands_run_locally_when_server_is_missing() {
    let base = unique_test_dir();
    let home_dir = base.join("home");
    let extensions_dir = home_dir.join(".pi/agent/extensions");
    fs::create_dir_all(&extensions_dir).unwrap();

    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    let missing_socket = runtime_dir.join("missing.sock");

    let expected_extension = extensions_dir.join("herdr-agent-state.ts");
    assert!(
        !expected_extension.exists(),
        "test setup should start without extension file"
    );

    let workspace_list = crate::test_command::herdr_command()
        .args(["workspace", "list"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();
    assert_eq!(workspace_list.status.code(), Some(1));

    let integration_install = crate::test_command::herdr_command()
        .args(["integration", "install", "pi"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();
    assert_eq!(integration_install.status.code(), Some(0));
    assert!(
        expected_extension.exists(),
        "integration install should write local files without a server"
    );

    let integration_status = crate::test_command::herdr_command()
        .args(["integration", "status"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();
    assert_eq!(integration_status.status.code(), Some(0));
    let status_stdout = String::from_utf8_lossy(&integration_status.stdout);
    assert!(status_stdout.contains("pi: current (v10)"));
    assert!(status_stdout.contains("claude: not installed"));

    let integration_uninstall = crate::test_command::herdr_command()
        .args(["integration", "uninstall", "pi"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();
    assert_eq!(integration_uninstall.status.code(), Some(0));
    assert!(
        !expected_extension.exists(),
        "integration uninstall should remove local files without a server"
    );

    cleanup_test_base(&base);
}

#[test]
fn integration_status_outdated_only_prints_action_for_legacy_install() {
    let base = unique_test_dir();
    let home_dir = base.join("home");
    let extensions_dir = home_dir.join(".pi/agent/extensions");
    fs::create_dir_all(&extensions_dir).unwrap();
    fs::write(
        extensions_dir.join("herdr-agent-state.ts"),
        "// legacy herdr integration\n",
    )
    .unwrap();

    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    let missing_socket = runtime_dir.join("missing.sock");

    let output = crate::test_command::herdr_command()
        .args(["integration", "status", "--outdated-only"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("installed herdr integrations need updating"));
    assert!(stderr.contains("herdr integration install pi"));

    cleanup_test_base(&base);
}

#[test]
fn integration_status_rejects_unknown_flags() {
    let base = unique_test_dir();
    let home_dir = base.join("home");
    fs::create_dir_all(&home_dir).unwrap();
    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    let missing_socket = runtime_dir.join("missing.sock");

    let output = crate::test_command::herdr_command()
        .args(["integration", "status", "--wat"])
        .env("HERDR_SOCKET_PATH", &missing_socket)
        .env("HOME", &home_dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));

    cleanup_test_base(&base);
}

#[test]
fn status_commands_report_client_and_server_versions() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let full = run_cli(&socket_path, &["status"]);
    assert!(
        full.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    let full_stdout = String::from_utf8_lossy(&full.stdout);
    assert!(full_stdout.contains("client:\n"), "stdout: {full_stdout}");
    assert!(
        full_stdout.contains(&format!("  version: {}", env!("CARGO_PKG_VERSION"))),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains("  protocol: 22"),
        "stdout: {full_stdout}"
    );
    assert!(full_stdout.contains("server:\n"), "stdout: {full_stdout}");
    assert!(
        full_stdout.contains("  status: running"),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains("  private_protocol_compatible: yes"),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains("  endpoint_compatible: yes"),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains("  restart_needed: no"),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains("  server_binary_stale: no"),
        "stdout: {full_stdout}"
    );
    assert!(
        full_stdout.contains(&socket_path.display().to_string()),
        "stdout: {full_stdout}"
    );

    let server = run_cli(&socket_path, &["status", "server"]);
    assert!(server.status.success());
    let server_stdout = String::from_utf8_lossy(&server.stdout);
    assert!(
        server_stdout.contains("status: running"),
        "stdout: {server_stdout}"
    );
    assert!(
        server_stdout.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))),
        "stdout: {server_stdout}"
    );
    assert!(
        server_stdout.contains("private_protocol: 22"),
        "stdout: {server_stdout}"
    );

    let client = run_cli(&socket_path, &["status", "client"]);
    assert!(client.status.success());
    let client_stdout = String::from_utf8_lossy(&client.stdout);
    assert!(
        client_stdout.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))),
        "stdout: {client_stdout}"
    );
    assert!(
        client_stdout.contains("protocol: 22"),
        "stdout: {client_stdout}"
    );
    assert!(
        client_stdout.contains("endpoint_protocol_generation: 1"),
        "stdout: {client_stdout}"
    );
    assert!(
        client_stdout.contains("binary: "),
        "stdout: {client_stdout}"
    );

    let full_json = run_cli_json(&socket_path, &["status", "--json"]);
    assert_eq!(full_json["client"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(full_json["client"]["protocol"], 22);
    assert_eq!(full_json["client"]["endpoint_protocol_generation"], 1);
    assert_eq!(full_json["client"]["remote_host_bridge"], true);
    assert_eq!(full_json["server"]["status"], "running");
    assert_eq!(full_json["server"]["running"], true);
    assert_eq!(full_json["server"]["compatible"], true);
    assert_eq!(full_json["server"]["endpoint_compatible"], true);
    assert_eq!(
        full_json["server"]["socket"],
        socket_path.display().to_string()
    );
    assert_eq!(full_json["server"]["restart_needed"], false);
    assert_eq!(full_json["server"]["server_binary_stale"], false);
    assert_eq!(full_json["update"]["restart_needed"], false);
    assert_eq!(full_json["update"]["server_binary_stale"], false);

    let server_json = run_cli_json(&socket_path, &["status", "server", "--json"]);
    assert_eq!(server_json["status"], "running");
    assert_eq!(server_json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(server_json["protocol"], 22);
    assert_eq!(server_json["compatible"], true);
    assert_eq!(server_json["endpoint_compatible"], true);

    let client_json = run_cli_json(&socket_path, &["status", "client", "--json"]);
    assert_eq!(client_json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(client_json["protocol"], 22);
    assert_eq!(client_json["endpoint_protocol_generation"], 1);
    assert_eq!(client_json["remote_host_bridge"], true);
    assert!(client_json["binary"]
        .as_str()
        .is_some_and(|path| !path.is_empty()));

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn status_reports_not_running_when_server_socket_is_missing() {
    let base = unique_test_dir();
    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    let socket_path = runtime_dir.join("missing.sock");

    let status = run_cli(&socket_path, &["status"]);
    assert!(status.status.success());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("  status: not running"), "stdout: {stdout}");
    assert!(stdout.contains("  restart_needed: no"), "stdout: {stdout}");
    assert!(
        stdout.contains("  server_binary_stale: no"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(&socket_path.display().to_string()),
        "stdout: {stdout}"
    );

    let status_json = run_cli_json(&socket_path, &["status", "--json"]);
    assert_eq!(status_json["server"]["status"], "not_running");
    assert_eq!(status_json["server"]["running"], false);
    assert_eq!(
        status_json["server"]["socket"],
        socket_path.display().to_string()
    );
    assert_eq!(status_json["server"]["restart_needed"], false);
    assert_eq!(status_json["server"]["server_binary_stale"], false);
    assert_eq!(status_json["update"]["restart_needed"], false);
    assert_eq!(status_json["update"]["server_binary_stale"], false);

    cleanup_test_base(&base);
}

#[test]
fn server_stop_command_shuts_down_running_server() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(
        stopped.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        stopped.stdout.is_empty(),
        "server stop should not print stdout: {}",
        String::from_utf8_lossy(&stopped.stdout)
    );
    assert!(
        !socket_path.exists() || UnixStream::connect(&socket_path).is_err(),
        "api socket should be removed or stale before server stop returns"
    );
    assert!(
        !client_socket.exists() || UnixStream::connect(&client_socket).is_err(),
        "client socket should be removed or stale before server stop returns"
    );

    let pid = herdr.child.process_id();
    let exit_status = herdr.child.wait().unwrap();
    unregister_spawned_herdr_pid(pid);
    assert!(exit_status.success(), "server stop should exit cleanly");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn server_stop_then_restart_restores_pane_history() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let marker = "PERSISTED_HISTORY_AFTER_STOP";

    let mut herdr = spawn_herdr_with_pane_history(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &[
            "workspace",
            "create",
            "--cwd",
            base.to_str().expect("test path should be utf-8"),
            "--label",
            "history-restart",
        ],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace create should return root pane id")
        .to_string();
    let sent = run_cli(
        &socket_path,
        &["pane", "send-text", &pane_id, &format!("echo {marker}\n")],
    );
    assert!(
        sent.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            pane_read_recent_contains(&socket_path, &pane_id, marker)
        }),
        "pane should contain marker before server stop"
    );

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(
        stopped.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );

    let pid = herdr.child.process_id();
    let exit_status = herdr.child.wait().unwrap();
    unregister_spawned_herdr_pid(pid);
    assert!(exit_status.success(), "server stop should exit cleanly");
    drop(herdr);

    let restarted = spawn_herdr_with_pane_history(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let workspaces = run_cli_json(&socket_path, &["workspace", "list"]);
    let workspace_id = workspaces["result"]["workspaces"]
        .as_array()
        .expect("workspace.list should return workspaces")
        .iter()
        .find(|workspace| workspace["label"] == "history-restart")
        .and_then(|workspace| workspace["workspace_id"].as_str())
        .expect("restored workspace should exist")
        .to_string();
    let panes = run_cli_json(
        &socket_path,
        &["pane", "list", "--workspace", &workspace_id],
    );
    let restored_pane_id = panes["result"]["panes"]
        .as_array()
        .expect("pane.list should return panes")
        .first()
        .and_then(|pane| pane["pane_id"].as_str())
        .expect("restored pane should exist")
        .to_string();

    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(25), || {
            pane_read_recent_contains(&socket_path, &restored_pane_id, marker)
        }),
        "restarted server should restore saved pane history"
    );

    cleanup_spawned_herdr(restarted, base);
}

#[cfg(target_os = "linux")]
#[test]
fn cold_restore_replays_only_explicitly_authorized_argv_panes() {
    exercise_cold_restore_argv(false, false);
}

#[cfg(target_os = "linux")]
#[test]
fn cold_restore_untrusted_snapshot_cannot_authorize_or_launder_ordinary_argv() {
    exercise_cold_restore_argv(true, false);
}

#[cfg(target_os = "linux")]
#[test]
fn cold_restore_failed_argv_redacts_restart_log_and_preserves_successful_control() {
    exercise_cold_restore_argv(false, true);
}

#[cfg(target_os = "linux")]
fn exercise_cold_restore_argv(untrusted_snapshot: bool, unavailable_executable: bool) {
    use std::os::unix::fs::PermissionsExt;

    // Keep every child and all persistence inside this test's private root.
    struct PrivateRoot(PathBuf);
    impl Drop for PrivateRoot {
        fn drop(&mut self) {
            cleanup_test_base(&self.0);
        }
    }
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            unregister_spawned_herdr_pid(Some(self.0.id()));
        }
    }
    fn command(base: &Path) -> Command {
        let mut command = crate::test_command::herdr_command();
        command
            .env_clear()
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_STATE_HOME", base.join("state"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("XDG_CACHE_HOME", base.join("cache"))
            .env("XDG_RUNTIME_DIR", base.join("runtime"))
            .env("HERDR_SESSION", "default")
            .env("HERDR_SOCKET_PATH", base.join("runtime/herdr.sock"))
            .env(
                "HERDR_CLIENT_SOCKET_PATH",
                base.join("runtime/herdr-client.sock"),
            )
            .env("SHELL", "/bin/sh")
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .current_dir(base);
        command
    }
    fn start(base: &Path) -> Server {
        let child = command(base)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        register_spawned_herdr_pid(Some(child.id()));
        let server = Server(child);
        wait_for_socket(&base.join("runtime/herdr.sock"), Duration::from_secs(10));
        wait_for_socket(
            &base.join("runtime/herdr-client.sock"),
            Duration::from_secs(10),
        );
        // Socket binding precedes restore; an App read is the restore barrier.
        request(base, "workspace.list", serde_json::json!({}));
        server
    }
    fn stop(base: &Path, server: &mut Server) {
        let output = command(base).args(["server", "stop"]).output().unwrap();
        assert!(
            output.status.success(),
            "stop failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(25), || {
                server.0.try_wait().unwrap().is_some()
            }),
            "cold stop must terminate the original server"
        );
        assert!(server.0.wait().unwrap().success());
        unregister_spawned_herdr_pid(Some(server.0.id()));
    }
    fn request(base: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut stream = UnixStream::connect(base.join("runtime/herdr.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let id = format!("cold-restore:{method}");
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": id, "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id, "response: {response}");
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }
    fn launches(cwd: &Path) -> Vec<(usize, u32)> {
        fs::read_to_string(cwd.join("launches"))
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                assert_eq!(fields.len(), 2, "invalid complete launch record: {line:?}");
                (fields[0].parse().unwrap(), fields[1].parse().unwrap())
            })
            .collect()
    }
    fn acknowledge(base: &Path, pane_id: &str, cwd: &Path, marker: &str) {
        request(
            base,
            "pane.send_input",
            serde_json::json!({"pane_id": pane_id, "text": marker, "keys": ["Enter"]}),
        );
        assert!(
            wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
                fs::read_to_string(cwd.join("received"))
                    .is_ok_and(|text| text.lines().any(|line| line == marker))
            }),
            "pane must acknowledge fresh input {marker:?} in a file, not restored screen history"
        );
    }
    fn restored_pane(base: &Path, workspace: &str, label: &str) -> String {
        let tabs = request(
            base,
            "tab.list",
            serde_json::json!({"workspace_id": workspace}),
        );
        let tab_id = tabs["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["label"] == label)
            .and_then(|tab| tab["tab_id"].as_str())
            .unwrap_or_else(|| panic!("missing restored tab {label}: {tabs}"));
        let layout = request(base, "layout.export", serde_json::json!({"tab_id": tab_id}));
        layout["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn snapshot_pane<'a>(snapshot: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
        let tab = snapshot["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|ws| ws["tabs"].as_array().unwrap())
            .find(|tab| tab["custom_name"] == label)
            .unwrap_or_else(|| panic!("snapshot missing tab {label}: {snapshot}"));
        let panes = tab["panes"].as_object().unwrap();
        assert_eq!(panes.len(), 1);
        panes.values().next().unwrap()
    }

    let root = PrivateRoot(unique_test_dir());
    let base = &root.0;
    for dir in [
        "home",
        "config",
        "state",
        "data",
        "cache",
        "runtime",
        "eligible",
        "ordinary",
        "unavailable",
    ] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(base.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
    register_runtime_dir(&base.join("runtime"));
    let app_dir = base.join("config").join(app_dir_name());
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("config.toml"),
        "onboarding = false\n[experimental]\npane_history = false\n",
    )
    .unwrap();
    let script = base.join("ARGV0_CONTROL_SENTINEL.sh");
    assert!(script.is_absolute());
    fs::write(&script, "#!/bin/sh\nset -eu\nprintf '%s %s\\n' \"$1\" \"$RECIPE_ENV_SENTINEL_KEY\" > recipe_arguments\nn=0\nif [ -f count ]; then read -r n < count; fi\nn=$((n + 1))\nprintf '%s\\n' \"$n\" > count\nprintf '%s %s\\n' \"$n\" \"$$\" >> launches\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> received\n  printf 'got:%s\\n' \"$line\"\ndone\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let argv = serde_json::json!([script, "ARGV1_SENTINEL"]);
    let unavailable_script = base.join("ARGV0_FAILURE_SENTINEL.sh");
    fs::copy(&script, &unavailable_script).unwrap();
    let unavailable_argv = serde_json::json!([unavailable_script, "ARGV1_SENTINEL"]);
    let eligible_cwd = base.join("eligible");
    let ordinary_cwd = base.join("ordinary");
    let unavailable_cwd = base.join("unavailable");
    let mut server = start(base);
    let created = request(
        base,
        "workspace.create",
        serde_json::json!({
            "cwd": base, "label": "cold-argv", "focus": false,
            "env": {"RECIPE_ENV_SENTINEL_KEY": "WORKSPACE_ENV_SENTINEL_VALUE"}
        }),
    );
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut initial_panes = Vec::new();
    let mut layouts = vec![("ordinary", &ordinary_cwd, "layout.apply")];
    if !untrusted_snapshot {
        layouts.insert(0, ("eligible", &eligible_cwd, "layout.apply_restorable"));
    }
    if unavailable_executable {
        layouts.push(("unavailable", &unavailable_cwd, "layout.apply_restorable"));
    }
    for (label, cwd, method) in layouts {
        let command = if label == "unavailable" {
            &unavailable_argv
        } else {
            &argv
        };
        // Same arguments (except the failed executable), no leaf env or replacement target.
        let applied = request(
            base,
            method,
            serde_json::json!({
                "workspace_id": workspace, "tab_label": label, "focus": false,
                "root": {"type": "pane", "cwd": cwd, "command": command}
            }),
        );
        let pane_id = applied["layout"]["root"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            wait_until(
                Duration::from_secs(5),
                Duration::from_millis(25),
                || launches(cwd).len() == 1
            ),
            "{label} must start once"
        );
        let records = launches(cwd);
        assert_eq!(records[0].0, 1);
        assert!(process_exists(records[0].1));
        acknowledge(base, &pane_id, cwd, &format!("before-cold-{label}"));
        assert_eq!(
            fs::read_to_string(cwd.join("recipe_arguments")).unwrap(),
            "ARGV1_SENTINEL WORKSPACE_ENV_SENTINEL_VALUE\n",
            "sentinels must reach the real argv process, not just exist in test inputs"
        );
        initial_panes.push((pane_id, records[0].1));
    }
    stop(base, &mut server);
    drop(server);
    for (_, pid) in &initial_panes {
        assert!(
            wait_for_pid_exit(*pid, Duration::from_secs(5)),
            "old pane process {pid} must exit before restart"
        );
    }
    let snapshot_path = app_dir.join("session.json");
    let mut first_snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    assert_eq!(
        snapshot_pane(&first_snapshot, "ordinary")["launch_argv"],
        argv
    );
    assert!(
        !snapshot_pane(&first_snapshot, "ordinary")["cold_restore_argv"]
            .as_bool()
            .unwrap_or(false)
    );

    if untrusted_snapshot {
        // Ordinary layout.apply never grants replay. Add a forged grant while
        // stopped, then make the same-owner snapshot writable by other users.
        let tab = first_snapshot["workspaces"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .flat_map(|ws| ws["tabs"].as_array_mut().unwrap())
            .find(|tab| tab["custom_name"] == "ordinary")
            .unwrap();
        let pane = tab["panes"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        pane["cold_restore_argv"] = serde_json::json!(true);
        fs::write(&snapshot_path, serde_json::to_vec(&first_snapshot).unwrap()).unwrap();
        fs::set_permissions(&snapshot_path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            fs::metadata(&snapshot_path).unwrap().permissions().mode() & 0o777,
            0o666
        );
        assert_eq!(
            snapshot_pane(&first_snapshot, "ordinary")["cold_restore_argv"],
            true
        );

        // A second cold cycle checks anti-laundering: a private autosave must
        // not turn the rejected grant into trusted replay authorization.
        for cycle in 1..=2 {
            let mut restarted = start(base);
            let ordinary = restored_pane(base, &workspace, "ordinary");
            assert_eq!(ordinary, initial_panes[0].0);
            let ready = format!("ordinary-shell-ready-{cycle}");
            request(
                base,
                "pane.send_input",
                serde_json::json!({"pane_id": ordinary, "text": format!("printf 'shell-ready\\n' > {ready}"), "keys": ["Enter"]}),
            );
            assert!(
                wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
                    fs::read_to_string(ordinary_cwd.join(&ready))
                        .is_ok_and(|text| text == "shell-ready\n")
                }),
                "cold cycle {cycle}: rejected argv must restore as a functioning plain shell"
            );
            assert_eq!(
                launches(&ordinary_cwd).len(),
                1,
                "cold cycle {cycle} must not replay argv"
            );
            if cycle == 1 {
                let log = fs::read_to_string(app_dir.join("herdr-server.log")).unwrap();
                assert!(
                    log.lines().any(|line| {
                        let line = line.to_ascii_lowercase();
                        line.contains("argv")
                            && (line.contains("untrusted") || line.contains("not trusted"))
                    }),
                    "untrusted snapshot must log argv trust refusal: {log}"
                );
            }
            stop(base, &mut restarted);
            drop(restarted);
            let saved: serde_json::Value =
                serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
            assert!(
                !snapshot_pane(&saved, "ordinary")["cold_restore_argv"]
                    .as_bool()
                    .unwrap_or(false),
                "cold cycle {cycle}: autosave must strip the rejected authorization"
            );
            assert_eq!(
                fs::metadata(&snapshot_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(launches(&ordinary_cwd).len(), 1);
        }
        return;
    }

    assert_eq!(
        snapshot_pane(&first_snapshot, "eligible")["launch_argv"],
        argv
    );
    assert_eq!(
        snapshot_pane(&first_snapshot, "eligible")["cold_restore_argv"],
        true
    );
    if unavailable_executable {
        assert_eq!(
            snapshot_pane(&first_snapshot, "unavailable")["launch_argv"],
            unavailable_argv
        );
        assert_eq!(
            snapshot_pane(&first_snapshot, "unavailable")["cold_restore_argv"],
            true
        );
        assert_eq!(
            fs::metadata(&snapshot_path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the failed replay must come from a genuinely eligible trusted snapshot"
        );
        fs::remove_file(&unavailable_script).unwrap();
    }
    fs::remove_file(eligible_cwd.join("recipe_arguments")).unwrap();
    let log_path = app_dir.join("herdr-server.log");
    let log_offset = fs::metadata(&log_path).unwrap().len() as usize;
    let mut restarted = start(base);
    let workspaces = request(base, "workspace.list", serde_json::json!({}));
    let workspace = workspaces["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["label"] == "cold-argv")
        .and_then(|ws| ws["workspace_id"].as_str())
        .unwrap();
    let eligible = restored_pane(base, workspace, "eligible");
    let ordinary = restored_pane(base, workspace, "ordinary");
    assert_eq!(eligible, initial_panes[0].0);
    assert_eq!(ordinary, initial_panes[1].0);
    assert!(wait_until(Duration::from_secs(8), Duration::from_millis(25), || launches(&eligible_cwd).len() == 2),
        "eligible argv must execute a SECOND startup after a real cold stop/restart; launches={:?}, ordinary={:?}", launches(&eligible_cwd), launches(&ordinary_cwd));
    let records = launches(&eligible_cwd);
    assert_eq!(
        records.iter().map(|record| record.0).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_ne!(records[1].1, initial_panes[0].1);
    assert!(process_exists(records[1].1));
    acknowledge(base, &eligible, &eligible_cwd, "after-cold-eligible");
    assert_eq!(
        fs::read_to_string(eligible_cwd.join("recipe_arguments")).unwrap(),
        "ARGV1_SENTINEL WORKSPACE_ENV_SENTINEL_VALUE\n"
    );
    // A fresh file proves the ordinary pane restored as a functioning shell.
    request(
        base,
        "pane.send_input",
        serde_json::json!({"pane_id": ordinary, "text": "printf 'shell-ready\\n' > ordinary-shell-ready", "keys": ["Enter"]}),
    );
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            fs::read_to_string(ordinary_cwd.join("ordinary-shell-ready"))
                .is_ok_and(|text| text == "shell-ready\n")
        }),
        "ordinary argv must become a shell, not restart the script"
    );
    assert_eq!(launches(&ordinary_cwd).len(), 1);
    assert_eq!(launches(&eligible_cwd).len(), 2);
    if unavailable_executable {
        let unavailable = restored_pane(base, workspace, "unavailable");
        assert_eq!(unavailable, initial_panes[2].0);
        let pane = request(
            base,
            "pane.get",
            serde_json::json!({"pane_id": unavailable}),
        );
        let restore_error = pane["pane"]["restore_error"].as_str().unwrap();
        assert_eq!(restore_error, "Could not start the saved argv command. Check its executable and saved directory, then restart this session.");
        assert_eq!(launches(&unavailable_cwd).len(), 1);
    }
    stop(base, &mut restarted);
    drop(restarted);
    assert!(wait_for_pid_exit(records[1].1, Duration::from_secs(5)));
    let second_snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(app_dir.join("session.json")).unwrap()).unwrap();
    // Restore must retain the mark on terminal state so a later capture keeps it.
    assert_eq!(
        snapshot_pane(&second_snapshot, "eligible")["cold_restore_argv"],
        true
    );
    assert_eq!(
        snapshot_pane(&second_snapshot, "eligible")["launch_argv"],
        argv
    );
    assert!(
        !snapshot_pane(&second_snapshot, "ordinary")["cold_restore_argv"]
            .as_bool()
            .unwrap_or(false)
    );
    assert_eq!(launches(&eligible_cwd).len(), 2);
    assert_eq!(launches(&ordinary_cwd).len(), 1);
    // Inspect only bytes appended by this restart, never earlier creation logs.
    let log = fs::read(&log_path).unwrap();
    assert!(
        log.len() >= log_offset,
        "restart log must remain append-only"
    );
    let restart_log = std::str::from_utf8(&log[log_offset..]).unwrap();
    let replay_lines: Vec<_> = restart_log
        .lines()
        .filter(|line| line.contains("replayed cold restore argv"))
        .collect();
    assert_eq!(replay_lines.len(), 1, "{restart_log}");
    for sentinel in [
        "ARGV0_CONTROL_SENTINEL",
        "ARGV0_FAILURE_SENTINEL",
        "ARGV1_SENTINEL",
        "RECIPE_ENV_SENTINEL_KEY",
        "WORKSPACE_ENV_SENTINEL_VALUE",
    ] {
        assert!(
            !restart_log.contains(sentinel),
            "recipe leaked into restart log: {sentinel}: {restart_log}"
        );
    }
    if unavailable_executable {
        assert!(
            restart_log.contains("failed to spawn argv command pane"),
            "{restart_log}"
        );
        assert!(
            restart_log.contains("failed to replay cold restore argv"),
            "{restart_log}"
        );
        assert_eq!(launches(&unavailable_cwd).len(), 1);
        assert_eq!(
            snapshot_pane(&second_snapshot, "unavailable")["cold_restore_argv"],
            true
        );
        assert_eq!(
            snapshot_pane(&second_snapshot, "unavailable")["launch_argv"],
            unavailable_argv
        );
    }
}

#[test]
fn unloaded_session_survives_autosave_and_shutdown_when_recovery_is_blocked() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let data_dir = config_home.join(app_dir_name());
    fs::create_dir_all(&data_dir).unwrap();
    let session_path = data_dir.join("session.json");
    let original = b"{unreadable layout";
    fs::write(&session_path, original).unwrap();
    fs::write(data_dir.join("session-backups"), b"blocks recovery").unwrap();

    let mut herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(25),
        || {
            fs::read_to_string(data_dir.join("herdr-server.log"))
                .is_ok_and(|log| log.contains("event=\"persist.save\""))
        }
    ));
    assert_eq!(fs::read(&session_path).unwrap(), original);

    assert!(run_cli(&socket_path, &["server", "stop"]).status.success());
    let pid = herdr.child.process_id();
    assert!(herdr.child.wait().unwrap().success());
    unregister_spawned_herdr_pid(pid);
    assert_eq!(fs::read(&session_path).unwrap(), original);
    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn session_appearing_after_startup_is_preserved_before_autosave() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let data_dir = config_home.join(app_dir_name());
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    // The API socket binds before restore; a read-only App request waits for it.
    let ready = run_cli_json(&socket_path, &["workspace", "list"]);
    assert_eq!(ready["result"]["workspaces"], serde_json::json!([]));
    // The server has already evaluated restore, but has not created any layout.
    let original = include_bytes!("../fixtures/session/current-herdr-session.json");
    fs::write(data_dir.join("session.json"), original).unwrap();
    run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(25),
        || {
            fs::read_to_string(data_dir.join("herdr-server.log"))
                .is_ok_and(|log| log.contains("event=\"persist.save\""))
        }
    ));
    let backups: Vec<_> = fs::read_dir(data_dir.join("session-backups"))
        .expect("late session must be preserved before autosave")
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(backups, vec![original.to_vec()]);
    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn server_start_restores_legacy_session_through_api_identity() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let data_dir = config_home.join(app_dir_name());
    let pion_cwd = base.join("legacy-pion");
    let herdr_cwd = base.join("legacy-herdr");

    fs::create_dir_all(&pion_cwd).unwrap();
    fs::create_dir_all(&herdr_cwd).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let pion_cwd = pion_cwd.to_str().expect("test cwd should be UTF-8");
    let herdr_cwd = herdr_cwd.to_str().expect("test cwd should be UTF-8");
    let legacy_session = include_str!("../fixtures/session/legacy-pre-tabs-v2.json")
        .replace("/tmp/pion", pion_cwd)
        .replace("/tmp/herdr", herdr_cwd);
    fs::write(data_dir.join("session.json"), legacy_session).unwrap();

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let workspaces = run_cli_json(&socket_path, &["workspace", "list"]);
    let restored_workspace = workspaces["result"]["workspaces"]
        .as_array()
        .expect("workspace.list should return workspaces")
        .iter()
        .find(|workspace| workspace["label"] == "legacy")
        .expect("legacy workspace should restore");
    let workspace_id = restored_workspace["workspace_id"]
        .as_str()
        .expect("restored workspace should have public id")
        .to_string();
    assert_eq!(restored_workspace["pane_count"], 2);
    assert_eq!(restored_workspace["tab_count"], 1);
    assert_eq!(
        restored_workspace["active_tab_id"],
        format!("{workspace_id}:t1")
    );

    let panes = run_cli_json(
        &socket_path,
        &["pane", "list", "--workspace", &workspace_id],
    );
    let panes = panes["result"]["panes"]
        .as_array()
        .expect("pane.list should return panes");
    assert_eq!(panes.len(), 2);
    let root_pane_id = format!("{workspace_id}:p1");
    let focused_pane_id = format!("{workspace_id}:p2");
    assert!(panes.iter().any(|pane| {
        pane["pane_id"] == root_pane_id
            && pane["tab_id"] == format!("{workspace_id}:t1")
            && pane["cwd"] == pion_cwd
            && pane["focused"] == false
    }));
    assert!(panes.iter().any(|pane| {
        pane["pane_id"] == focused_pane_id
            && pane["tab_id"] == format!("{workspace_id}:t1")
            && pane["cwd"] == herdr_cwd
            && pane["focused"] == true
    }));

    let reported = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            &focused_pane_id,
            "--source",
            "test",
            "--agent",
            "pi",
            "--state",
            "working",
        ],
    );
    assert!(
        reported.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&reported.stderr)
    );

    let agents = run_cli_json(&socket_path, &["agent", "list"]);
    let agents = agents["result"]["agents"]
        .as_array()
        .expect("agent.list should return agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["pane_id"], focused_pane_id);
    assert_eq!(agents[0]["workspace_id"], workspace_id);
    assert_eq!(agents[0]["agent"], "pi");
    assert_eq!(agents[0]["agent_status"], "working");

    cleanup_spawned_herdr(herdr, base);
}
