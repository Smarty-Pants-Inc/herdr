use std::collections::HashMap;

use super::*;

const SERVER: u32 = 500;
const BRIDGE: u32 = 900;
const NOTTY: u32 = 800;
const PRIV: u32 = 700;
const KATE_KEY: &str = "SHA256:katekatekatekatekatekatekatekatekatekatekat";
const KATE_IP: &str = "100.64.0.7";

fn process(ppid: u32, uid: u32, comm: &str, cmdline: &[&str], start: u64) -> IdentityProcess {
    IdentityProcess {
        ppid,
        uids: vec![uid; 4],
        comm: comm.to_string(),
        cmdline: cmdline.iter().map(|arg| arg.to_string()).collect(),
        exe_name: None,
        start_unix_secs: 1_700_000_000 + start,
        start_ticks: start * 100,
    }
}

fn bridge(ppid: u32) -> IdentityProcess {
    IdentityProcess {
        exe_name: Some("herdr".into()),
        ..process(
            ppid,
            1000,
            "herdr",
            &[
                "/home/u/.local/bin/herdr",
                "--session",
                "s",
                "remote-client-bridge",
            ],
            30,
        )
    }
}

fn accepted(fingerprint: &str, ip: &str) -> IdentityJournalRecord {
    IdentityJournalRecord {
        message: Some(format!(
            "Accepted publickey for paul from {ip} port 51234 ssh2: ED25519 {fingerprint}"
        )),
        pid: Some(PRIV),
        uid: Some(0),
        comm: Some("sshd".into()),
    }
}

fn whois(node: &str, login: &str) -> String {
    serde_json::json!({
        "Node": { "Name": format!("{node}."), "StableID": "nKATE123" },
        "UserProfile": { "LoginName": login },
    })
    .to_string()
}

struct FakeHost {
    processes: HashMap<u32, IdentityProcess>,
    journal: Vec<IdentityJournalRecord>,
    whois: HashMap<String, String>,
}

impl FakeHost {
    /// Kate's Mac over SSH on Tailscale: bridge -> `sshd: paul@notty` -> root `sshd: paul [priv]`.
    fn kate() -> Self {
        let mut processes = HashMap::new();
        processes.insert(BRIDGE, bridge(NOTTY));
        processes.insert(
            NOTTY,
            process(PRIV, 1000, "sshd", &["sshd: paul@notty"], 20),
        );
        processes.insert(PRIV, process(1, 0, "sshd", &["sshd: paul [priv]"], 10));
        Self {
            processes,
            journal: vec![accepted(KATE_KEY, KATE_IP)],
            whois: HashMap::from([(
                KATE_IP.to_string(),
                whois("kates-mac.tail0000.ts.net", "kate@example.com"),
            )]),
        }
    }
}

impl IdentityHost for FakeHost {
    fn process(&self, pid: u32) -> Option<IdentityProcess> {
        self.processes.get(&pid).cloned()
    }

    fn journal_records(&self, pid: u32, _since: u64) -> Option<Vec<IdentityJournalRecord>> {
        Some(
            self.journal
                .iter()
                .filter(|record| record.pid == Some(pid))
                .cloned()
                .collect(),
        )
    }

    fn tailscale_whois(&self, ip: &str) -> Option<String> {
        self.whois.get(ip).cloned()
    }
}

fn map() -> PrincipalsFile {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "principals": [
            {
                "name": "Kate",
                "sshKeys": [KATE_KEY],
                "tailscaleNodes": [{ "node": "kates-mac.tail0000.ts.net", "login": "kate@example.com" }],
            },
            {
                "name": "Paul",
                "sshKeys": ["SHA256:paulpaulpaulpaulpaulpaulpaulpaulpaulpaulpau"],
                "tailscaleNodes": [{ "node": "pauls-mac.tail0000.ts.net", "login": "paul@example.com" }],
            },
        ],
    }))
    .expect("principals map")
}

fn resolve(host: &FakeHost) -> Option<String> {
    resolve_principal(host, &map(), BRIDGE, SERVER)
}

#[test]
fn a_verified_client_gets_its_principal() {
    assert_eq!(resolve(&FakeHost::kate()).as_deref(), Some("Kate"));
}

#[test]
fn the_node_may_be_mapped_by_stable_id() {
    let mut map = map();
    map.principals[0].tailscale_nodes[0].node = "nKATE123".into();
    assert_eq!(
        resolve_principal(&FakeHost::kate(), &map, BRIDGE, SERVER).as_deref(),
        Some("Kate")
    );
}

#[test]
fn a_key_match_with_a_whois_mismatch_gets_none() {
    // Kate's key, forwarded by an agent, from another machine on the tailnet.
    let mut host = FakeHost::kate();
    host.whois.insert(
        KATE_IP.into(),
        whois("dev1.tail0000.ts.net", "kate@example.com"),
    );
    assert_eq!(resolve(&host), None);
    // Right node name, wrong login.
    host.whois.insert(
        KATE_IP.into(),
        whois("kates-mac.tail0000.ts.net", "mallory@example.com"),
    );
    assert_eq!(resolve(&host), None);
    // Paul's node with Kate's key: both are mapped, but not to one person.
    host.whois.insert(
        KATE_IP.into(),
        whois("pauls-mac.tail0000.ts.net", "paul@example.com"),
    );
    assert_eq!(resolve(&host), None);
    // No whois answer (not a tailnet IP).
    host.whois.clear();
    assert_eq!(resolve(&host), None);
}

#[test]
fn a_node_match_with_an_unmapped_key_gets_none() {
    let mut host = FakeHost::kate();
    host.journal = vec![accepted(
        "SHA256:othrothrothrothrothrothrothrothrothrothroth",
        KATE_IP,
    )];
    assert_eq!(resolve(&host), None);
}

#[test]
fn an_unknown_client_gets_none() {
    // A local attach (no sshd ancestor, e.g. mosh-server -> shell -> herdr).
    let mut host = FakeHost::kate();
    host.processes.insert(BRIDGE, bridge(600));
    host.processes
        .insert(600, process(1, 1000, "mosh-server", &["mosh-server"], 5));
    assert_eq!(resolve(&host), None);

    // Not a remote-client-bridge.
    let mut host = FakeHost::kate();
    host.processes.get_mut(&BRIDGE).expect("bridge").cmdline = vec!["herdr".into()];
    assert_eq!(resolve(&host), None);

    // A bridge-named process that is not the herdr executable.
    let mut host = FakeHost::kate();
    host.processes.get_mut(&BRIDGE).expect("bridge").exe_name = Some("python3".into());
    assert_eq!(resolve(&host), None);

    // A journal miss.
    let mut host = FakeHost::kate();
    host.journal.clear();
    assert_eq!(resolve(&host), None);

    // No peer at all.
    assert_eq!(
        resolve_principal(&FakeHost::kate(), &map(), 0, SERVER),
        None
    );
}

#[test]
fn the_trust_chain_rejects_forgeable_links() {
    // A user-owned sshd calling itself [priv].
    let mut host = FakeHost::kate();
    host.processes.get_mut(&PRIV).expect("priv").uids = vec![1000; 4];
    assert_eq!(resolve(&host), None);

    // A journal record not written by root.
    let mut host = FakeHost::kate();
    host.journal[0].uid = Some(1000);
    assert_eq!(resolve(&host), None);

    // A journal record by another command.
    let mut host = FakeHost::kate();
    host.journal[0].comm = Some("logger".into());
    assert_eq!(resolve(&host), None);

    // The chain passes through the Herdr server (an agent in a pane).
    let mut host = FakeHost::kate();
    host.processes.insert(BRIDGE, bridge(SERVER));
    host.processes.insert(
        SERVER,
        process(PRIV, 1000, "herdr", &["herdr", "server"], 15),
    );
    assert_eq!(resolve(&host), None);

    // A reused pid: the "parent" started after the bridge.
    let mut host = FakeHost::kate();
    host.processes.get_mut(&NOTTY).expect("notty").start_ticks = 99_999;
    assert_eq!(resolve(&host), None);

    // Too far away: more than three ancestors to the monitor.
    let mut host = FakeHost::kate();
    host.processes.insert(BRIDGE, bridge(803));
    host.processes
        .insert(803, process(802, 1000, "sh", &["sh"], 25));
    host.processes
        .insert(802, process(801, 1000, "sh", &["sh"], 24));
    host.processes
        .insert(801, process(NOTTY, 1000, "sh", &["sh"], 23));
    assert_eq!(resolve(&host), None);

    // Two accepted logins for one monitor process.
    let mut host = FakeHost::kate();
    host.journal.push(accepted(KATE_KEY, "100.64.0.8"));
    assert_eq!(resolve(&host), None);

    // The accepted user differs from the monitor's user.
    let mut host = FakeHost::kate();
    host.journal[0].message = Some(format!(
        "Accepted publickey for root from {KATE_IP} port 1 ssh2: ED25519 {KATE_KEY}"
    ));
    assert_eq!(resolve(&host), None);
}

#[test]
fn parses_sshd_accepted_lines() {
    let login = parse_accepted_publickey(&format!(
        "Accepted publickey for paul from {KATE_IP} port 58097 ssh2: ED25519 {KATE_KEY}"
    ))
    .expect("login");
    assert_eq!(login.user, "paul");
    assert_eq!(login.ip, KATE_IP);
    assert_eq!(login.fingerprint, KATE_KEY);
    assert!(
        parse_accepted_publickey("Accepted password for paul from 1.2.3.4 port 1 ssh2").is_none()
    );
    assert!(parse_accepted_publickey(
        "Accepted publickey for paul from host.example port 1 ssh2: ED25519 SHA256:x"
    )
    .is_none());
}

#[cfg(unix)]
mod principals_file {
    use std::os::unix::fs::PermissionsExt as _;

    use super::super::*;

    fn write(dir: &std::path::Path, mode: u32) -> std::path::PathBuf {
        let path = dir.join("principals.json");
        std::fs::write(
            &path,
            r#"{"version":1,"principals":[{"name":"Kate","sshKeys":["SHA256:k"],"tailscaleNodes":[{"node":"n","login":"l"}]}]}"#,
        )
        .expect("write map");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-principals-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[test]
    fn a_map_not_owned_by_root_is_refused() {
        if current_uid() == 0 {
            return;
        }
        let dir = TempDir::new();
        let path = write(dir.path(), 0o644);
        let err = load_principals(&path, TrustPolicy::ROOT).expect_err("refused");
        assert!(err.contains("not 0"), "{err}");
    }

    #[test]
    fn a_group_or_other_writable_map_is_refused() {
        let dir = TempDir::new();
        let policy = TrustPolicy {
            owner_uid: current_uid(),
            check_ancestors: false,
        };
        for mode in [0o664, 0o646] {
            let path = write(dir.path(), mode);
            let err = load_principals(&path, policy).expect_err("refused");
            assert!(err.contains("writable"), "{err}");
        }
    }

    #[test]
    fn a_writable_directory_above_the_map_is_refused() {
        let dir = TempDir::new();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod dir");
        let path = write(dir.path(), 0o644);
        let policy = TrustPolicy {
            owner_uid: current_uid(),
            check_ancestors: true,
        };
        assert!(load_principals(&path, policy).is_err());
    }

    #[test]
    fn a_trusted_map_loads() {
        let dir = TempDir::new();
        let path = write(dir.path(), 0o644);
        let policy = TrustPolicy {
            owner_uid: current_uid(),
            check_ancestors: false,
        };
        let map = load_principals(&path, policy).expect("loads");
        assert_eq!(map.principals[0].name, "Kate");
    }
}
