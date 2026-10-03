//! CLI contract probes use bounded on-disk diagnostics with a known live fixture PID.
//! Producer/model semantics are exercised by client::machine_status unit tests.

#[path = "support/command.rs"]
pub mod test_command;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const CLIENT_ONE: &str = "11111111111111111111111111111111";
const CLIENT_TWO: &str = "22222222222222222222222222222222";
const MACHINE_ONE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MACHINE_TWO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "herdr-status-client-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            socket: root.join("herdr.sock"),
            root,
        }
    }

    fn command(&self) -> Command {
        let mut command = test_command::herdr_command();
        command
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("APPDATA", self.root.join("config"))
            .env("LOCALAPPDATA", self.root.join("state"))
            .env("HERDR_SOCKET_PATH", &self.socket);
        command
    }

    fn directory(&self) -> PathBuf {
        self.root.join("herdr-client.machine-status")
    }

    fn write(&self, client_id: &str, updated_at_ms: u64, pid: u32) {
        fs::create_dir_all(self.directory()).unwrap();
        fs::write(
            self.directory().join(format!("{client_id}.json")),
            serde_json::to_vec(&json!({
                "schema_version":1,"client_id":client_id,"pid":pid,
                "updated_at_ms":updated_at_ms,"version":"fixture-running-version",
                "endpoints":[
                    {"id":MACHINE_ONE,"label":"Reachable","enabled":true,
                     "connected":true,"listed":true,"workspace_count":110,"ready":true},
                    {"id":MACHINE_TWO,"label":"Disabled","enabled":false,
                     "connected":false,"listed":true,"workspace_count":0,"ready":false}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn status(&self, extra: &[&str]) -> Value {
        let output = self
            .command()
            .args(["status", "client", "--json"])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn assert_unusable(status: &Value, reason: &str) {
    assert_eq!(status["readout"]["schema_version"], 1);
    assert_eq!(status["readout"]["status"], reason);
    assert_eq!(status["readout"]["available"], false);
    assert_eq!(status["readout"]["fresh"], false);
    assert_eq!(status["endpoints"], json!([]));
}

#[test]
fn status_client_machines_fresh_readout_preserves_binary_contract_and_each_saved_machine() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    let status = fixture.status(&[]);
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["remote_host_bridge"], true);
    assert!(status["endpoint_capabilities"].is_array());
    assert_eq!(status["running"], true);
    assert_eq!(status["readout"]["available"], true);
    assert_eq!(status["readout"]["fresh"], true);
    assert_eq!(status["readout"]["status"], "fresh");
    assert_eq!(status["readout"]["client_id"], CLIENT_ONE);
    assert_eq!(status["readout"]["pid"], std::process::id());
    assert_eq!(status["readout"]["version"], "fixture-running-version");
    assert_eq!(status["readout"]["max_age_ms"], 5000);
    assert_eq!(status["endpoints"].as_array().unwrap().len(), 2);
    assert_eq!(status["endpoints"][0]["id"], MACHINE_ONE);
    assert_eq!(status["endpoints"][0]["connected"], true);
    assert_eq!(status["endpoints"][0]["workspace_count"], 110);
    assert_eq!(status["endpoints"][1]["enabled"], false);
    assert_eq!(status["endpoints"][1]["listed"], true);
    assert_eq!(status["endpoints"][1]["workspace_count"], 0);
}

#[test]
fn status_client_machines_missing_old_or_dead_client_is_unavailable_not_healthy_empty() {
    let fixture = Fixture::new();
    let absent = fixture.status(&[]);
    assert_unusable(&absent, "unavailable");
    assert_eq!(absent["running"], false);
    // A terminated child proves PID liveness rather than relying on an arbitrary unused PID.
    let mut child = fixture.command().args(["version"]).spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    fixture.write(CLIENT_ONE, now_ms(), pid);
    let dead = fixture.status(&[]);
    assert_unusable(&dead, "unavailable");
    assert_eq!(dead["running"], false);
}

#[test]
fn status_client_machines_stale_or_future_readout_cannot_pass() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms() - 60_000, std::process::id());
    let stale = fixture.status(&[]);
    assert_unusable(&stale, "stale");
    assert_eq!(stale["running"], true);
    fixture.write(CLIENT_ONE, now_ms() + 60_000, std::process::id());
    assert_unusable(&fixture.status(&[]), "stale");
}

#[test]
fn status_client_machines_multiple_clients_require_exact_selection_even_when_one_is_stale() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    fixture.write(CLIENT_TWO, now_ms() - 60_000, std::process::id());
    let ambiguous = fixture.status(&[]);
    assert_unusable(&ambiguous, "ambiguous");
    assert_eq!(
        ambiguous["readout"]["candidates"].as_array().unwrap().len(),
        2
    );
    let selected = fixture.status(&["--client-id", CLIENT_ONE]);
    assert_eq!(selected["readout"]["status"], "fresh");
    assert_eq!(selected["readout"]["client_id"], CLIENT_ONE);
    assert_unusable(&fixture.status(&["--client-id", CLIENT_TWO]), "stale");
    assert_unusable(
        &fixture.status(&["--client-id", MACHINE_ONE]),
        "unavailable",
    );
}

#[test]
fn status_client_machines_corrupt_or_unsupported_readout_fails_closed() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    let path = fixture.directory().join(format!("{CLIENT_ONE}.json"));
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    snapshot["schema_version"] = json!(99);
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
    fs::write(&path, b"{").unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
}

#[test]
fn status_client_machines_selection_and_scope_errors_do_not_pick_arbitrary_clients() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    // An explicit session takes precedence over inherited socket scope.
    let output = fixture
        .command()
        .args(["--session", "different", "status", "client", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_unusable(
        &serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        "unavailable",
    );
    for args in [
        vec!["status", "client", "--client-id"],
        vec!["status", "client", "--client-id", "../../bad"],
        vec![
            "status",
            "client",
            "--json",
            "--client-id",
            CLIENT_ONE,
            "--client-id",
            CLIENT_TWO,
        ],
    ] {
        let output = fixture.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}
