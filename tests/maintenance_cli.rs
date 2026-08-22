#![cfg(unix)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use support::CURRENT_PROTOCOL;

const CAPABILITY: &str = "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhc";

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hmc-{}-{nanos}", std::process::id()))
}

fn accept_request(listener: &UnixListener) -> (UnixStream, serde_json::Value) {
    let (stream, _) = listener.accept().expect("accept CLI request");
    let mut line = String::new();
    BufReader::new(stream.try_clone().expect("clone CLI stream"))
        .read_line(&mut line)
        .expect("read CLI request");
    (
        stream,
        serde_json::from_str(&line).expect("parse CLI request JSON"),
    )
}

fn reply_pong(stream: &mut UnixStream, request: &serde_json::Value) {
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "pong",
                "version": "test",
                "protocol": CURRENT_PROTOCOL,
                "capabilities": {
                    "live_handoff": true,
                    "detached_server_daemon": true,
                },
            },
        })
    )
    .expect("reply to CLI ping");
    stream.flush().expect("flush CLI ping response");
}

fn accept_maintenance_request(listener: &UnixListener) -> (UnixStream, serde_json::Value) {
    loop {
        let (mut stream, request) = accept_request(listener);
        if request["method"] != "ping" {
            return (stream, request);
        }
        reply_pong(&mut stream, &request);
    }
}

fn reply_maintenance(stream: &mut UnixStream, request: &serde_json::Value) {
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "omp_maintenance",
                "maintenance": {
                    "schema": "herdr.omp_maintenance.v1",
                    "held": true,
                    "route_count": 0,
                    "routes": [],
                },
            },
        })
    )
    .expect("reply to maintenance request");
    stream.flush().expect("flush maintenance response");
}

fn process_listing(pid: u32, include_environment: bool) -> String {
    let flag = if include_environment { "eww" } else { "-ww" };
    let output = Command::new("ps")
        .args([flag, "-p", &pid.to_string(), "-o", "command="])
        .output()
        .expect("inspect CLI process command");
    assert!(
        output.status.success(),
        "ps failed to inspect the CLI process"
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn spawn_maintenance_cli(socket_path: &Path, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(args)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn maintenance CLI")
}

fn run_private_capability_action(
    listener: &UnixListener,
    socket_path: &Path,
    args: &[&str],
    expected_method: &str,
    expected_proof: Option<(&str, &str)>,
    trailing_newline: bool,
) -> Output {
    let mut child = spawn_maintenance_cli(socket_path, args);
    let mut stdin = child.stdin.take().expect("open CLI stdin");
    stdin
        .write_all(CAPABILITY.as_bytes())
        .expect("write private capability");
    if trailing_newline {
        stdin.write_all(b"\n").expect("write capability newline");
    }
    drop(stdin);

    let (mut stream, request) = accept_maintenance_request(listener);
    assert_eq!(request["method"], expected_method);
    assert!(
        request["params"]["operation_id"] == CAPABILITY,
        "the CLI must preserve the private capability exactly"
    );
    match expected_proof {
        Some((session, pane_id)) => {
            assert_eq!(request["params"]["session"], session);
            assert_eq!(request["params"]["pane_id"], pane_id);
        }
        None => {
            assert!(request["params"].get("session").is_none());
            assert!(request["params"].get("pane_id").is_none());
        }
    }

    for listing in [
        process_listing(child.id(), false),
        process_listing(child.id(), true),
    ] {
        assert!(
            !listing.contains(CAPABILITY),
            "the private capability must not appear in the CLI argv or environment"
        );
    }

    reply_maintenance(&mut stream, &request);
    let output = child.wait_with_output().expect("wait for maintenance CLI");
    assert!(
        output.status.success(),
        "private maintenance CLI invocation should succeed"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(CAPABILITY),
        "the private capability must not appear in CLI stdout"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(CAPABILITY),
        "the private capability must not appear in CLI stderr"
    );
    output
}

#[test]
fn maintenance_cli_reads_private_stdin_capabilities_without_process_exposure() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).expect("create test directory");
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake Herdr socket");

    for (args, method, proof, trailing_newline) in [
        (
            [
                "server",
                "omp-maintenance",
                "acquire",
                "--operation-id-stdin",
                "--json",
            ]
            .as_slice(),
            "server.omp_maintenance.acquire",
            None,
            false,
        ),
        (
            [
                "server",
                "omp-maintenance",
                "permit",
                "--operation-id-stdin",
                "--proof-session",
                "default",
                "--proof-pane",
                "w1:p1",
                "--json",
            ]
            .as_slice(),
            "server.omp_maintenance.permit",
            Some(("default", "w1:p1")),
            true,
        ),
        (
            [
                "server",
                "omp-maintenance",
                "release",
                "--operation-id-stdin",
                "--json",
            ]
            .as_slice(),
            "server.omp_maintenance.release",
            None,
            false,
        ),
        (
            [
                "server",
                "omp-maintenance",
                "release",
                "--operation-id-stdin",
                "--json",
            ]
            .as_slice(),
            "server.omp_maintenance.release",
            None,
            false,
        ),
    ] {
        let _ = run_private_capability_action(
            &listener,
            &socket_path,
            args,
            method,
            proof,
            trailing_newline,
        );
    }

    drop(listener);
    fs::remove_dir_all(base).expect("remove test directory");
}

#[test]
fn maintenance_cli_rejects_legacy_or_ambiguous_capability_flags_without_echoing_them() {
    for args in [
        vec![
            "server".to_string(),
            "omp-maintenance".to_string(),
            "acquire".to_string(),
            format!("--operation-id={CAPABILITY}"),
        ],
        vec![
            "server".to_string(),
            "omp-maintenance".to_string(),
            "acquire".to_string(),
            "--operation-id-stdin".to_string(),
            format!("--operation-id={CAPABILITY}"),
        ],
        vec![
            "server".to_string(),
            "omp-maintenance".to_string(),
            "status".to_string(),
            "--operation-id-stdin".to_string(),
        ],
        vec![
            "server".to_string(),
            "omp-maintenance".to_string(),
            "inspect".to_string(),
            "--operation-id-stdin".to_string(),
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("run rejected maintenance CLI invocation");
        assert_eq!(output.status.code(), Some(2));
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(CAPABILITY),
            "rejected invocation must not echo the private capability to stdout"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(CAPABILITY),
            "rejected invocation must not echo the private capability to stderr"
        );
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args([
            "server",
            "omp-maintenance",
            "acquire",
            "--operation-id-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn malformed private capability invocation");
    let mut stdin = child.stdin.take().expect("open malformed capability stdin");
    stdin
        .write_all(format!("{CAPABILITY}\n\n").as_bytes())
        .expect("write malformed private capability");
    drop(stdin);
    let output = child
        .wait_with_output()
        .expect("wait for malformed private capability invocation");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(CAPABILITY),
        "invalid stdin must not echo the private capability to stdout"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(CAPABILITY),
        "invalid stdin must not echo the private capability to stderr"
    );
}
