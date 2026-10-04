//! CLI contract probes use bounded on-disk diagnostics with a known live fixture PID.
//! Producer/model semantics are exercised by client::machine_status unit tests.

#[path = "support/command.rs"]
pub mod test_command;

#[path = "../src/platform/diagnostic_owner.rs"]
mod diagnostic_owner;
#[path = "../src/platform/diagnostic_storage_creation.rs"]
mod diagnostic_storage_creation;

use std::fs;
use std::io::Write;
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
    owner_identity: String,
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
            owner_identity: diagnostic_owner::diagnostic_owner_identity(std::process::id())
                .unwrap()
                .expect("fixture process must have a kernel identity"),
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
        // HERDR_SOCKET_PATH is the API socket; the existing endpoint contract
        // derives the client socket by inserting -client before .sock.
        diagnostic_owner::diagnostic_directory(&self.root.join("herdr-client.sock"))
    }

    fn snapshot_path(&self, client_id: &str) -> PathBuf {
        let name = diagnostic_owner::diagnostic_snapshot_name(client_id);
        match diagnostic_owner::parse_diagnostic_name(std::ffi::OsStr::new(&name)) {
            Some(diagnostic_owner::DiagnosticName::Snapshot { client_id: parsed }) => {
                assert_eq!(parsed, client_id);
            }
            _ => panic!("production helper generated an unrecognized snapshot name"),
        }
        self.directory().join(name)
    }

    fn write(&self, client_id: &str, updated_at_ms: u64, pid: u32) {
        self.write_bytes(
            client_id,
            serde_json::to_vec(&json!({
                "schema_version":diagnostic_owner::DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION,
                "client_id":client_id,"pid":pid,
                "owner_identity": if pid == std::process::id() {
                    self.owner_identity.clone()
                } else {
                    diagnostic_owner::diagnostic_owner_identity(pid)
                        .unwrap().unwrap_or_else(|| "fixture-dead-owner-start-identity".into())
                },
                "updated_at_ms":updated_at_ms,"version":"fixture-running-version",
                "endpoints":[
                    {"id":MACHINE_ONE,"label":"Reachable","enabled":true,
                     "connected":true,"listed":true,"workspace_count":110,"ready":true},
                    {"id":MACHINE_TWO,"label":"Disabled","enabled":false,
                     "connected":false,"listed":true,"workspace_count":0,"ready":false}
                ]
            }))
            .unwrap(),
        );
    }

    fn write_bytes(&self, client_id: &str, bytes: Vec<u8>) {
        let directory = self.directory();
        match diagnostic_storage_creation::create_private_directory(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("private fixture directory: {error}"),
        }
        let temporary = diagnostic_owner::diagnostic_temporary_name(
            client_id,
            std::process::id(),
            &self.owner_identity,
        )
        .unwrap();
        match diagnostic_owner::parse_diagnostic_name(std::ffi::OsStr::new(&temporary)) {
            Some(diagnostic_owner::DiagnosticName::Temporary {
                client_id: parsed,
                pid,
                owner_identity,
            }) => {
                assert_eq!(parsed, client_id);
                assert_eq!(pid, std::process::id());
                assert_eq!(owner_identity, self.owner_identity);
            }
            _ => panic!("production helper generated an unrecognized temporary name"),
        }
        // Use the publisher's actual native creation primitive, including its
        // private Windows DACL and exclusive, directory-relative Unix open.
        #[cfg(unix)]
        let mut file = {
            let anchor = fs::File::open(&directory).unwrap();
            let name = std::ffi::CString::new(temporary.as_bytes()).unwrap();
            diagnostic_storage_creation::create_private_file_at(&anchor, &name).unwrap()
        };
        #[cfg(windows)]
        let mut file =
            diagnostic_storage_creation::create_private_file(&directory.join(&temporary)).unwrap();
        file.write_all(&bytes).unwrap();
        drop(file);
        fs::rename(directory.join(temporary), self.snapshot_path(client_id)).unwrap();
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
    let path = fixture.snapshot_path(CLIENT_ONE);
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    snapshot["schema_version"] = json!(99);
    fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
    fs::write(&path, b"{").unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
}

#[test]
fn status_client_machines_reused_pid_cannot_authenticate_abandoned_owner() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    fixture.write(CLIENT_TWO, now_ms() - 60_000, std::process::id());
    let path = fixture.snapshot_path(CLIENT_TWO);
    let mut abandoned: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    abandoned["owner_identity"] = json!("fixture-old-boot-and-process-start-identity");
    fixture.write_bytes(CLIENT_TWO, serde_json::to_vec(&abandoned).unwrap());
    let status = fixture.status(&[]);
    assert_eq!(status["readout"]["status"], "fresh");
    assert_eq!(status["readout"]["client_id"], CLIENT_ONE);
    assert_eq!(status["readout"]["candidates"].as_array().unwrap().len(), 1);
    assert_unusable(&fixture.status(&["--client-id", CLIENT_TWO]), "unavailable");
}

#[test]
fn status_client_machines_unrecognized_legacy_files_are_ignored_and_untouched() {
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    let old_snapshot = fixture.directory().join(format!("{CLIENT_TWO}.json"));
    let old_partial = fixture.directory().join(format!("{CLIENT_TWO}.tmp"));
    // The unqualified format was never released. Do not migrate, authenticate,
    // or even open foreign/unrecognized names, regardless of their content/mode.
    fs::write(&old_snapshot, b"{ invalid old JSON").unwrap();
    fs::write(&old_partial, b"partial old temporary").unwrap();
    let status = fixture.status(&[]);
    assert_eq!(status["readout"]["status"], "fresh");
    assert_eq!(status["readout"]["client_id"], CLIENT_ONE);
    assert_eq!(status["readout"]["candidates"].as_array().unwrap().len(), 1);
    assert_unusable(&fixture.status(&["--client-id", CLIENT_TWO]), "unavailable");
    assert_eq!(fs::read(old_snapshot).unwrap(), b"{ invalid old JSON");
    assert_eq!(fs::read(old_partial).unwrap(), b"partial old temporary");
}

#[cfg(unix)]
#[test]
fn status_client_machines_unsafe_directory_file_and_hardlinks_fail_closed() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    let path = fixture.snapshot_path(CLIENT_ONE);
    assert_eq!(
        fs::metadata(fixture.directory())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    fs::set_permissions(fixture.directory(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
    fs::set_permissions(fixture.directory(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, fixture.directory().join("linked")).unwrap();
    assert_unusable(&fixture.status(&[]), "invalid");
    fs::remove_file(fixture.directory().join("linked")).unwrap();
    assert_eq!(fixture.status(&[])["readout"]["status"], "fresh");
}

#[cfg(windows)]
#[test]
fn status_client_machines_permissive_directory_dacl_fails_closed() {
    use interprocess::os::windows::security_descriptor::{
        AsSecurityDescriptorExt as _, SecurityDescriptor,
    };
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Security::{
            SetKernelObjectSecurity, DACL_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
    };
    let fixture = Fixture::new();
    fixture.write(CLIENT_ONE, now_ms(), std::process::id());
    assert_eq!(fixture.status(&[])["readout"]["status"], "fresh");
    // Unlike inherited ACLs (which vary by host), an explicit Everyone grant
    // deterministically demonstrates that healthy JSON does not bypass DACL checks.
    let sddl = widestring::U16CString::from_str("D:P(A;OICI;GA;;;WD)").unwrap();
    let descriptor = SecurityDescriptor::deserialize(&sddl).unwrap();
    let mut attributes = SECURITY_ATTRIBUTES::default();
    descriptor.write_to_security_attributes(&mut attributes);
    let directory = fs::OpenOptions::new()
        .access_mode(0x00040000) // WRITE_DAC
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(fixture.directory())
        .unwrap();
    assert_ne!(
        unsafe {
            SetKernelObjectSecurity(
                directory.as_raw_handle(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                attributes.lpSecurityDescriptor,
            )
        },
        0
    );
    drop(directory);
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
