//! Connection-local principal resolution; observations are acquired by the platform.
//! Only the Linux platform resolves client principals; other targets report none.
#![cfg(target_os = "linux")]

use crate::pty::input_consumer::Principal;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct MapImage {
    pub uid: u32,
    pub mode: u32,
    pub mtime: u128,
    /// Exact device/inode/ctime/length stamp, supplied by safe platform acquisition.
    pub revision: Option<(u64, u64, i64, i64, u64)>,
    pub json: String,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) struct LoadedMap {
    keys: BTreeMap<String, Principal>,
    nodes: BTreeMap<String, Principal>,
    duplicate_diagnostics: Vec<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct AcceptedPeer {
    pub pid: u32,
    pub start: u64,
    pub server_pid: u32,
    pub linux: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ExecutableIdentity {
    Server,
    Sshd,
    /// A uid-0 process whose exe link the unprivileged server cannot read.
    /// Acceptable as the sshd priv process only because the journal source
    /// then proves it with journald's trusted `_EXE=/usr/sbin/sshd` field.
    RootUnreadable,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Process {
    pub parent: u32,
    pub uid: u32,
    pub start: u64,
    /// Realtime microseconds corresponding to the pinned process start.
    pub started_at: u64,
    pub exe: String,
    pub argv: Vec<String>,
    pub title: String,
    /// Platform verified executable identity, not a basename or argv assertion.
    pub executable: ExecutableIdentity,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JournalQuery {
    pub pid: u32,
    pub uid: u32,
    pub comm: String,
    pub since: u64,
    pub format: String,
}

#[derive(Clone, Debug)]
pub(crate) struct JournalRow {
    pub pid: u32,
    pub uid: u32,
    pub comm: String,
    pub timestamp: u64,
    pub message: String,
}

/// Sources fail closed on unavailable, malformed, unbounded or untrusted data.
/// Journal acquisition additionally verifies boot, executable, transport and
/// monotonic/realtime metadata against the pinned authentication process.
pub(crate) trait Sources {
    fn proc(&mut self, pid: u32) -> Option<Process>;
    fn journal(&mut self, query: JournalQuery) -> Option<Vec<JournalRow>>;
    fn whois(&mut self, ip: &str) -> Option<String>;
}

#[derive(Default)]
pub(crate) struct MapCache {
    last_mtime: Option<u128>,
    last_revision: Option<(u64, u64, i64, i64, u64)>,
    map: LoadedMap,
    #[cfg(test)]
    loads: usize,
}

#[derive(serde::Deserialize)]
struct MapDocument {
    version: u32,
    principals: Vec<MapEntry>,
}

#[derive(serde::Deserialize)]
struct MapEntry {
    smarty_id: String,
    display_name: String,
    ssh_keys: Vec<String>,
    tailscale_nodes: Vec<String>,
}

pub(crate) fn load_map(image: Option<&MapImage>) -> LoadedMap {
    let Some(image) = image else {
        return LoadedMap::default();
    };
    let parsed = (|| {
        if image.uid != 0 || image.mode & 0o022 != 0 || image.json.len() > 65_536 {
            return None;
        }
        let doc: MapDocument = serde_json::from_str(&image.json).ok()?;
        if doc.version != 1 {
            return None;
        }
        let mut map = LoadedMap::default();
        let mut ids = std::collections::BTreeSet::new();
        let mut duplicate_keys = std::collections::BTreeSet::new();
        let mut duplicate_nodes = std::collections::BTreeSet::new();
        for entry in doc.principals {
            let length = entry.display_name.chars().count();
            if !(1..=80).contains(&length)
                || entry
                    .display_name
                    .chars()
                    .any(|c| c.is_control() || "*\\():".contains(c))
                || entry.smarty_id.is_empty()
                || !ids.insert(entry.smarty_id.clone())
                || entry
                    .ssh_keys
                    .iter()
                    .any(|s| !s.starts_with("SHA256:") || s.len() <= 7)
                || entry.tailscale_nodes.iter().any(String::is_empty)
            {
                return None;
            }
            let principal = Principal {
                smarty_id: entry.smarty_id,
                display_name: entry.display_name,
            };
            for (factors, index, duplicates) in [
                (entry.ssh_keys, &mut map.keys, &mut duplicate_keys),
                (entry.tailscale_nodes, &mut map.nodes, &mut duplicate_nodes),
            ] {
                for factor in factors {
                    if index.get(&factor).is_some_and(|p| p != &principal) {
                        duplicates.insert(factor.clone());
                    }
                    index.insert(factor, principal.clone());
                }
            }
        }
        for (index, duplicates) in [
            (&mut map.keys, duplicate_keys),
            (&mut map.nodes, duplicate_nodes),
        ] {
            for factor in duplicates {
                index.remove(&factor);
                let diagnostic = format!("ambiguous principal factor: {factor}");
                tracing::warn!(%diagnostic, "principal map duplicate");
                map.duplicate_diagnostics.push(diagnostic);
            }
        }
        Some(map)
    })();
    parsed.unwrap_or_else(|| {
        tracing::warn!("invalid principal map; new connections remain unmapped");
        LoadedMap::default()
    })
}

pub(crate) fn match_factors(map: &LoadedMap, key: &str, stable_id: &str) -> Option<Principal> {
    let key_principal = map.keys.get(key)?;
    (map.nodes.get(stable_id)? == key_principal).then(|| key_principal.clone())
}

fn accepted_key(message: &str, user: &str) -> Option<(String, String)> {
    let fields: Vec<_> = message.split_whitespace().collect();
    if fields.len() != 11
        || fields[0..3] != ["Accepted", "publickey", "for"]
        || fields[3] != user
        || fields[4] != "from"
        || fields[6] != "port"
        || fields[8] != "ssh2:"
        || fields[7].parse::<u16>().ok().is_none_or(|p| p == 0)
        || fields[9].is_empty()
    {
        return None;
    }
    let fingerprint = fields[10].strip_prefix("SHA256:")?;
    if fingerprint.is_empty()
        || !fingerprint
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        return None;
    }
    let _: std::net::IpAddr = fields[5].parse().ok()?;
    Some((fields[10].into(), fields[5].into()))
}

pub(crate) fn resolve(
    peer: AcceptedPeer,
    map: &LoadedMap,
    sources: &mut impl Sources,
) -> Option<Principal> {
    if !peer.linux || peer.pid == peer.server_pid || map.keys.is_empty() || map.nodes.is_empty() {
        return None;
    }
    let first = sources.proc(peer.pid)?;
    if first.start != peer.start
        || first.executable != ExecutableIdentity::Server
        || first.argv.get(1).map(String::as_str) != Some("remote-client-bridge")
    {
        return None;
    }
    let mut chain = vec![(peer.pid, first)];
    let mut priv_user = None;
    for _ in 0..3 {
        let (pid, child) = chain.last()?;
        let parent_pid = child.parent;
        if parent_pid == 0
            || parent_pid == peer.server_pid
            || chain.iter().any(|(p, _)| *p == parent_pid)
        {
            return None;
        }
        let parent = sources.proc(parent_pid)?;
        if parent.start > child.start || sources.proc(*pid)? != *child {
            return None;
        }
        let user = parent
            .title
            .strip_prefix("sshd: ")
            .and_then(|s| s.strip_suffix(" [priv]"));
        let is_priv = parent.uid == 0
            && matches!(
                parent.executable,
                ExecutableIdentity::Sshd | ExecutableIdentity::RootUnreadable
            )
            && user.is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_whitespace));
        if is_priv {
            priv_user = user.map(str::to_owned);
        }
        chain.push((parent_pid, parent));
        if priv_user.is_some() {
            break;
        }
    }
    let user = priv_user?;
    let (priv_pid, priv_process) = chain.last()?;
    let rows = sources.journal(JournalQuery {
        pid: *priv_pid,
        uid: 0,
        comm: "sshd".into(),
        since: priv_process.started_at,
        format: "json".into(),
    })?;
    let mut accepted = None;
    for row in rows {
        if row.pid != *priv_pid
            || row.uid != 0
            || row.comm != "sshd"
            || row.timestamp < priv_process.started_at
        {
            return None;
        }
        if row.message.starts_with("Accepted publickey ") {
            if accepted.is_some() {
                return None;
            }
            accepted = Some(accepted_key(&row.message, &user)?);
        }
    }
    let (key, ip) = accepted?;
    let address: std::net::IpAddr = ip.parse().ok()?;
    if !tailnet_address(address) {
        return None;
    }
    let whois: serde_json::Value = serde_json::from_str(&sources.whois(&ip)?).ok()?;
    let node = whois.get("Node")?;
    let stable_id = node.get("StableID")?.as_str()?;
    let addresses = node.get("Addresses")?.as_array()?;
    if !addresses
        .iter()
        .filter_map(serde_json::Value::as_str)
        .any(|s| {
            let bare = s.split_once('/').map_or(s, |(ip, _)| ip);
            bare.parse::<std::net::IpAddr>().ok() == Some(address)
        })
    {
        return None;
    }
    // All evidence was acquired by numeric PID. Revalidate the entire pinned
    // chain after the external queries, including executable/title/parent data.
    for (pid, process) in chain {
        if sources.proc(pid)? != process {
            return None;
        }
    }
    match_factors(map, &key, stable_id)
}

pub(crate) fn tailnet_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => u32::from(ip) & 0xffc0_0000 == 0x6440_0000,
        std::net::IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

impl MapCache {
    pub(crate) fn snapshot(&self) -> LoadedMap {
        self.map.clone()
    }

    pub(crate) fn refresh(&mut self, image: Option<&MapImage>) {
        let stamp = image.map(|image| image.mtime);
        let revision = image.and_then(|image| image.revision);
        if stamp != self.last_mtime || revision != self.last_revision {
            self.map = load_map(image);
            self.last_mtime = stamp;
            self.last_revision = revision;
            #[cfg(test)]
            {
                self.loads += 1;
            }
        }
    }

    /// Fixture composition only: the production connection path uses these
    /// same refresh/snapshot/resolve operations, releasing the cache lock first.
    #[cfg(test)]
    fn accept(
        &mut self,
        image: Option<&MapImage>,
        peer: AcceptedPeer,
        sources: &mut impl Sources,
    ) -> Option<Principal> {
        self.refresh(image);
        resolve(peer, &self.map, sources)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    // Every vector exercises the production parser, resolver or cache.
    use super::{
        load_map, match_factors, resolve, AcceptedPeer, ExecutableIdentity, JournalQuery,
        JournalRow, LoadedMap, MapCache, MapImage, Process, Sources,
    };
    use crate::pty::input_consumer::Principal;

    const VALID_MAP: &str = r#"{"version":1,"principals":[{"smarty_id":"paul","display_name":"Paul","ssh_keys":["SHA256:paul"],"tailscale_nodes":["nPaul"]},{"smarty_id":"kate","display_name":"Kate","ssh_keys":["SHA256:kate"],"tailscale_nodes":["nKate"]}]}"#;
    const ACCEPTED: &str =
        "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:paul";
    const WHOIS: &str = r#"{"Node":{"StableID":"nPaul","ID":777,"Name":"untrusted-host-name","Addresses":["100.64.0.7/32"]}}"#;

    fn paul() -> Principal {
        Principal {
            smarty_id: "paul".into(),
            display_name: "Paul".into(),
        }
    }

    fn kate() -> Principal {
        Principal {
            smarty_id: "kate".into(),
            display_name: "Kate".into(),
        }
    }

    // Metadata is injected, not created via chown: ordinary tests need no root.
    fn image(json: &str) -> MapImage {
        MapImage {
            uid: 0,
            mode: 0o644,
            mtime: 1,
            revision: None,
            json: json.into(),
        }
    }

    fn trusted_map() -> LoadedMap {
        LoadedMap {
            keys: [
                ("SHA256:paul".into(), paul()),
                ("SHA256:kate".into(), kate()),
            ]
            .into(),
            nodes: [("nPaul".into(), paul()), ("nKate".into(), kate())].into(),
            duplicate_diagnostics: Vec::new(),
        }
    }

    #[derive(Clone, Debug)]
    struct FakeSources {
        processes: BTreeMap<u32, Process>,
        changed_on_reread: BTreeMap<u32, Process>,
        reads: BTreeMap<u32, usize>,
        rows: Option<Vec<JournalRow>>,
        whois_json: Option<String>,
        proc_calls: Vec<u32>,
        journal_calls: Vec<JournalQuery>,
        whois_calls: Vec<String>,
    }

    impl Sources for FakeSources {
        fn proc(&mut self, pid: u32) -> Option<Process> {
            self.proc_calls.push(pid);
            let reads = self.reads.entry(pid).or_default();
            *reads += 1;
            if *reads > 1 {
                if let Some(changed) = self.changed_on_reread.get(&pid) {
                    return Some(changed.clone());
                }
            }
            self.processes.get(&pid).cloned()
        }
        fn journal(&mut self, query: JournalQuery) -> Option<Vec<JournalRow>> {
            self.journal_calls.push(query);
            self.rows.clone()
        }
        fn whois(&mut self, ip: &str) -> Option<String> {
            self.whois_calls.push(ip.into());
            self.whois_json.clone()
        }
    }

    fn process(parent: u32, uid: u32, exe: &str, argv: &[&str], title: &str) -> Process {
        Process {
            parent,
            uid,
            start: 100,
            started_at: 100,
            executable: match exe {
                "/usr/local/bin/herdr" => ExecutableIdentity::Server,
                "/usr/sbin/sshd" => ExecutableIdentity::Sshd,
                _ => ExecutableIdentity::Other,
            },
            exe: exe.into(),
            argv: argv.iter().map(|s| (*s).into()).collect(),
            title: title.into(),
        }
    }

    fn sources() -> FakeSources {
        FakeSources {
            processes: [
                (
                    40,
                    process(
                        30,
                        1000,
                        "/usr/local/bin/herdr",
                        &["herdr", "remote-client-bridge"],
                        "herdr",
                    ),
                ),
                (
                    30,
                    process(20, 1000, "/usr/sbin/sshd", &["sshd"], "sshd: paul@notty"),
                ),
                (
                    20,
                    process(1, 0, "/usr/sbin/sshd", &["sshd"], "sshd: paul [priv]"),
                ),
            ]
            .into(),
            changed_on_reread: BTreeMap::new(),
            reads: BTreeMap::new(),
            rows: Some(vec![JournalRow {
                pid: 20,
                uid: 0,
                comm: "sshd".into(),
                timestamp: 101,
                message: ACCEPTED.into(),
            }]),
            whois_json: Some(WHOIS.into()),
            proc_calls: Vec::new(),
            journal_calls: Vec::new(),
            whois_calls: Vec::new(),
        }
    }

    fn peer() -> AcceptedPeer {
        AcceptedPeer {
            pid: 40,
            start: 100,
            server_pid: 999,
            linux: true,
        }
    }

    #[test]
    fn version_one_exact_map_shape() {
        assert_eq!(load_map(Some(&image(VALID_MAP))), trusted_map());
    }

    #[test]
    fn reject_wrong_version_missing_fields_and_wrong_types() {
        for json in [
            "{}",
            "null",
            "[]",
            "{",
            r#"{"version":2,"principals":[]}"#,
            r#"{"version":"1","principals":[]}"#,
            r#"{"version":1,"principals":{}}"#,
            r#"{"version":1,"principals":[{"smarty_id":"paul","display_name":"Paul","ssh_keys":[],"tailscale_nodes":"nPaul"}]}"#,
            &VALID_MAP.replace("\"display_name\":\"Paul\",", ""),
            &VALID_MAP.replace("\"smarty_id\":\"paul\",", ""),
            &VALID_MAP.replace("\"ssh_keys\":[\"SHA256:paul\"],", ""),
            &VALID_MAP.replace(",\"tailscale_nodes\":[\"nPaul\"]", ""),
        ] {
            assert_eq!(load_map(Some(&image(json))), LoadedMap::default(), "{json}");
        }
    }

    #[test]
    fn missing_map_is_empty() {
        assert_eq!(load_map(None), LoadedMap::default());
    }

    #[test]
    fn empty_version_one_map_is_valid_empty() {
        assert_eq!(
            load_map(Some(&image(r#"{"version":1,"principals":[]}"#))),
            LoadedMap::default()
        );
    }

    #[test]
    fn root_owned_0644_is_accepted_without_real_root_fixture() {
        let fixture = image(VALID_MAP);
        assert_eq!((fixture.uid, fixture.mode), (0, 0o644));
        assert_eq!(load_map(Some(&fixture)), trusted_map());
    }

    #[test]
    fn reject_nonroot_and_group_or_world_writable_entire_map() {
        for (uid, mode) in [
            (1000, 0o644),
            (0, 0o664),
            (0, 0o646),
            (0, 0o666),
            (0, 0o777),
        ] {
            let mut fixture = image(VALID_MAP);
            fixture.uid = uid;
            fixture.mode = mode;
            assert_eq!(
                load_map(Some(&fixture)),
                LoadedMap::default(),
                "uid={uid}, mode={mode:o}"
            );
        }
    }

    #[test]
    fn ruling_allows_root_owned_0600_not_only_0644() {
        let mut fixture = image(VALID_MAP);
        fixture.mode = 0o600;
        assert_eq!(load_map(Some(&fixture)), trusted_map());
    }

    #[test]
    fn display_names_preserve_safe_unicode_and_spaces() {
        let json = VALID_MAP
            .replace("\"Paul\"", "\"Paul Smith\"")
            .replace("\"Kate\"", "\"ケイト\"");
        let map = load_map(Some(&image(&json)));
        assert_eq!(
            match_factors(&map, "SHA256:paul", "nPaul").map(|p| p.display_name),
            Some("Paul Smith".into())
        );
        assert_eq!(
            match_factors(&map, "SHA256:kate", "nKate").map(|p| p.display_name),
            Some("ケイト".into())
        );
    }

    #[test]
    fn display_names_reject_empty_controls_and_label_delimiters() {
        for encoded in [
            "",
            r"Paul\nKate",
            r"Paul\r",
            r"Paul\u0000",
            r"Paul\u001b",
            "**Paul**",
            "Paul (in Herdr):",
        ] {
            let json = VALID_MAP.replace("\"Paul\"", &format!("\"{encoded}\""));
            assert_eq!(
                load_map(Some(&image(&json))),
                LoadedMap::default(),
                "{encoded:?}"
            );
        }
    }

    #[test]
    fn duplicate_key_across_principals_is_unmapped_and_logged_once_at_load() {
        let json = VALID_MAP.replace("SHA256:kate", "SHA256:paul");
        let map = load_map(Some(&image(&json)));
        assert_eq!(match_factors(&map, "SHA256:paul", "nPaul"), None);
        assert_eq!(match_factors(&map, "SHA256:paul", "nKate"), None);
        assert_eq!(
            map.duplicate_diagnostics
                .iter()
                .filter(|d| d.contains("SHA256:paul"))
                .count(),
            1
        );
        let before = map.duplicate_diagnostics.clone();
        for _ in 0..3 {
            assert_eq!(match_factors(&map, "SHA256:paul", "nPaul"), None);
        }
        assert_eq!(map.duplicate_diagnostics, before);
    }

    #[test]
    fn duplicate_node_across_principals_is_unmapped_and_logged_once_at_load() {
        let map = load_map(Some(&image(&VALID_MAP.replace("nKate", "nPaul"))));
        assert_eq!(match_factors(&map, "SHA256:paul", "nPaul"), None);
        assert_eq!(match_factors(&map, "SHA256:kate", "nPaul"), None);
        assert_eq!(
            map.duplicate_diagnostics
                .iter()
                .filter(|d| d.contains("nPaul"))
                .count(),
            1
        );
    }

    #[test]
    fn both_factors_must_map_to_same_principal() {
        let map = trusted_map();
        assert_eq!(match_factors(&map, "SHA256:paul", "nPaul"), Some(paul()));
        assert_eq!(match_factors(&map, "SHA256:kate", "nKate"), Some(kate()));
        for (key, node) in [
            ("SHA256:paul", "nKate"),
            ("SHA256:paul", "unknown"),
            ("unknown", "nPaul"),
            ("unknown", "unknown"),
        ] {
            assert_eq!(match_factors(&map, key, node), None, "{key}/{node}");
        }
    }

    #[test]
    fn trusted_sources_resolve_and_query_root_journal_since_process_start() {
        let mut f = sources();
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), Some(paul()));
        assert_eq!(
            f.journal_calls,
            vec![JournalQuery {
                pid: 20,
                uid: 0,
                comm: "sshd".into(),
                since: 100,
                format: "json".into()
            }]
        );
        assert_eq!(f.whois_calls, vec!["100.64.0.7"]);
        assert!(f.proc_calls.contains(&40));
    }

    macro_rules! rejected_source {
        ($name:ident, $change:expr) => {
            #[test]
            fn $name() {
                let mut f = sources();
                let mut p = peer();
                ($change)(&mut f, &mut p);
                assert_eq!(resolve(p, &trusted_map(), &mut f), None);
            }
        };
    }

    rejected_source!(
        wrong_exe_even_with_forged_argv,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&40).unwrap().exe = "/usr/bin/python3".into();
            f.processes.get_mut(&40).unwrap().executable = ExecutableIdentity::Other;
        }
    );
    rejected_source!(
        herdr_exe_without_bridge_subcommand,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&40).unwrap().argv = vec!["herdr".into(), "attach".into()];
        }
    );
    rejected_source!(
        missing_peer_proc_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.remove(&40);
        }
    );
    rejected_source!(
        stale_peer_start_pin_is_unmapped,
        |_: &mut FakeSources, p: &mut AcceptedPeer| {
            p.start = 99;
        }
    );
    rejected_source!(
        peer_pid_reuse_during_resolution_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            let mut changed = f.processes[&40].clone();
            changed.start += 1;
            f.changed_on_reread.insert(40, changed);
        }
    );
    rejected_source!(
        priv_pid_reuse_during_resolution_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            let mut changed = f.processes[&20].clone();
            changed.start += 1;
            f.changed_on_reread.insert(20, changed);
        }
    );
    rejected_source!(
        server_ancestor_is_unmapped,
        |_: &mut FakeSources, p: &mut AcceptedPeer| {
            p.server_pid = 30;
        }
    );
    rejected_source!(
        priv_process_not_root_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().uid = 1000;
        }
    );
    rejected_source!(
        priv_title_spoof_wrong_exe_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().exe = "/usr/bin/bash".into();
            f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::Other;
        }
    );
    rejected_source!(
        unreadable_root_priv_without_journal_proof_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::RootUnreadable;
            // The Linux source returns None when any row lacks trusted
            // `_EXE=/usr/sbin/sshd`, `_BOOT_ID` or the other pinned fields.
            f.rows = None;
        }
    );
    rejected_source!(
        unreadable_exe_nonroot_priv_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            let p = f.processes.get_mut(&20).unwrap();
            p.executable = ExecutableIdentity::RootUnreadable;
            p.uid = 1000;
        }
    );
    rejected_source!(
        unreadable_root_priv_two_accepted_records_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::RootUnreadable;
            let row = f.rows.as_ref().unwrap()[0].clone();
            f.rows.as_mut().unwrap().push(row);
        }
    );
    rejected_source!(
        unreadable_root_priv_restarted_during_resolution_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::RootUnreadable;
            let mut changed = f.processes[&20].clone();
            changed.start += 1;
            f.changed_on_reread.insert(20, changed);
        }
    );

    #[test]
    fn unreadable_root_priv_with_one_trusted_journal_record_maps() {
        let mut f = sources();
        f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::RootUnreadable;
        f.processes.get_mut(&20).unwrap().exe = String::new();
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), Some(paul()));
        assert_eq!(f.journal_calls.len(), 1);
        assert_eq!(f.journal_calls[0].pid, 20);
    }

    rejected_source!(
        no_priv_title_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&20).unwrap().title = "sshd: paul@notty".into();
        }
    );
    rejected_source!(
        journal_uid_not_zero_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].uid = 1000;
        }
    );
    rejected_source!(
        journal_wrong_pid_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].pid = 19;
        }
    );
    rejected_source!(
        journal_wrong_comm_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].comm = "bash".into();
        }
    );
    rejected_source!(
        journal_before_priv_start_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].timestamp = 99;
        }
    );
    rejected_source!(
        journal_user_mismatch_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].message = ACCEPTED.replace("for paul", "for kate");
        }
    );
    rejected_source!(
        journal_missing_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows = None;
        }
    );
    rejected_source!(
        zero_accepted_lines_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows = Some(Vec::new());
        }
    );
    rejected_source!(
        control_master_multiple_accepted_lines_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            let row = f.rows.as_ref().unwrap()[0].clone();
            f.rows.as_mut().unwrap().push(row);
        }
    );
    rejected_source!(
        malformed_accepted_line_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].message = "Accepted password for paul".into();
        }
    );
    rejected_source!(
        whois_unavailable_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = None;
        }
    );
    rejected_source!(
        whois_malformed_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some("{".into());
        }
    );
    rejected_source!(
        not_tailnet_whois_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some(r#"{"Node":null}"#.into());
        }
    );
    rejected_source!(
        node_name_or_numeric_id_cannot_replace_stable_id,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some(r#"{"Node":{"ID":"nPaul","Name":"nPaul"}}"#.into());
        }
    );
    rejected_source!(
        whois_node_key_mismatch_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some(WHOIS.replace("nPaul", "nKate"));
        }
    );
    rejected_source!(
        unmapped_node_including_fleet_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some(WHOIS.replace("nPaul", "nFleet"));
        }
    );
    rejected_source!(
        forwarded_key_alone_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.whois_json = Some(WHOIS.replace("nPaul", "nAgentHost"));
        }
    );
    rejected_source!(
        node_alone_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.rows.as_mut().unwrap()[0].message = ACCEPTED.replace("SHA256:paul", "SHA256:unknown");
        }
    );
    rejected_source!(
        non_linux_is_unmapped,
        |_: &mut FakeSources, p: &mut AcceptedPeer| {
            p.linux = false;
        }
    );
    rejected_source!(
        local_attach_without_sshd_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&30).unwrap().parent = 1;
            f.processes.remove(&20);
        }
    );
    rejected_source!(
        ancestor_cycle_is_unmapped,
        |f: &mut FakeSources, _: &mut AcceptedPeer| {
            f.processes.get_mut(&30).unwrap().parent = 40;
        }
    );

    #[test]
    fn third_parent_allowed_fourth_parent_refused() {
        for (extra, expected) in [(1, Some(paul())), (2, None)] {
            let mut f = sources();
            f.processes.get_mut(&30).unwrap().parent = 29;
            f.processes.insert(
                29,
                process(
                    if extra == 1 { 20 } else { 28 },
                    1000,
                    "/bin/sh",
                    &["sh"],
                    "sh",
                ),
            );
            if extra == 2 {
                f.processes
                    .insert(28, process(20, 1000, "/bin/sh", &["sh"], "sh"));
            }
            assert_eq!(resolve(peer(), &trusted_map(), &mut f), expected);
        }
    }

    #[test]
    fn server_is_never_traversed_to_reach_root_sshd() {
        let mut f = sources();
        let mut p = peer();
        p.server_pid = 30;
        assert_eq!(resolve(p, &trusted_map(), &mut f), None);
        assert!(!f.proc_calls.contains(&20));
        assert!(f.journal_calls.is_empty());
        assert!(f.whois_calls.is_empty());
    }

    #[test]
    fn ignores_unrelated_journal_messages_not_additional_acceptances() {
        let mut f = sources();
        let mut row = f.rows.as_ref().unwrap()[0].clone();
        row.message = "pam_unix(sshd:session): session opened for user paul".into();
        f.rows.as_mut().unwrap().push(row);
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), Some(paul()));
    }

    #[test]
    fn stable_id_not_mutable_name_is_the_node_factor() {
        let mut f = sources();
        f.whois_json = Some(WHOIS.replace("untrusted-host-name", "renamed-host"));
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), Some(paul()));
    }

    #[test]
    fn mtime_reload_for_new_connections_existing_binding_retained() {
        let mut cache = MapCache::default();
        let first = image(VALID_MAP);
        let binding = cache.accept(Some(&first), peer(), &mut sources());
        assert_eq!(binding, Some(paul()));
        assert_eq!(cache.loads, 1);
        assert_eq!(cache.last_mtime, Some(1));
        assert_eq!(cache.map, trusted_map());
        assert_eq!(
            cache.accept(Some(&first), peer(), &mut sources()),
            Some(paul())
        );
        assert_eq!(cache.loads, 1, "unchanged mtime must not reparse");
        let mut next = image(&VALID_MAP.replace("\"Paul\"", "\"Paul New\""));
        next.mtime = 2;
        assert_eq!(
            cache
                .accept(Some(&next), peer(), &mut sources())
                .map(|p| p.display_name),
            Some("Paul New".into())
        );
        assert_eq!(cache.loads, 2);
        assert_eq!(
            binding,
            Some(paul()),
            "accepted binding is owned and immutable"
        );
    }

    #[test]
    fn display_name_exact_bounds_and_no_extra_whitespace_policy() {
        for name in [" ".to_string(), "  Paul  ".to_string(), "界".repeat(80)] {
            let json = VALID_MAP.replace("\"Paul\"", &serde_json::to_string(&name).unwrap());
            assert_eq!(
                match_factors(&load_map(Some(&image(&json))), "SHA256:paul", "nPaul")
                    .unwrap()
                    .display_name,
                name
            );
        }
        for name in [
            "界".repeat(81),
            "Paul\\Kate".into(),
            "Paul*".into(),
            "Paul(".into(),
            "Paul)".into(),
            "Paul:".into(),
        ] {
            let json = VALID_MAP.replace("\"Paul\"", &serde_json::to_string(&name).unwrap());
            assert_eq!(load_map(Some(&image(&json))), LoadedMap::default());
        }
        let mut oversized = image(VALID_MAP);
        oversized.json.push_str(&" ".repeat(65_536));
        assert_eq!(load_map(Some(&oversized)), LoadedMap::default());
    }

    #[test]
    fn executable_identity_not_name_or_comm_is_authority() {
        let mut f = sources();
        f.processes.get_mut(&40).unwrap().executable = ExecutableIdentity::Sshd;
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
        let mut f = sources();
        f.processes.get_mut(&20).unwrap().executable = ExecutableIdentity::Server;
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
    }

    #[test]
    fn parent_reparenting_or_younger_generation_is_unmapped() {
        let mut f = sources();
        let mut changed = f.processes[&30].clone();
        changed.parent = 21;
        f.changed_on_reread.insert(30, changed);
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
        let mut f = sources();
        f.processes.get_mut(&20).unwrap().start += 1;
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
    }

    #[test]
    fn whois_must_identify_the_queried_tailnet_address() {
        let mut f = sources();
        f.whois_json = Some(WHOIS.replace("100.64.0.7/32", "100.64.0.8/32"));
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
        let mut f = sources();
        f.rows.as_mut().unwrap()[0].message = ACCEPTED.replace("100.64.0.7", "127.0.0.1");
        assert_eq!(resolve(peer(), &trusted_map(), &mut f), None);
        assert!(f.whois_calls.is_empty());
    }

    #[test]
    fn same_mtime_replacement_or_mode_change_does_not_preserve_cached_authority() {
        let mut cache = MapCache::default();
        let mut first = image(VALID_MAP);
        first.revision = Some((1, 2, 3, 4, 100));
        assert_eq!(
            cache.accept(Some(&first), peer(), &mut sources()),
            Some(paul())
        );
        let mut replacement = image("{");
        replacement.revision = Some((1, 5, 3, 4, 100));
        assert_eq!(
            cache.accept(Some(&replacement), peer(), &mut sources()),
            None
        );
        assert_eq!(cache.loads, 2);
    }

    #[test]
    fn invalid_or_missing_reload_empties_new_bindings_not_existing_ones() {
        for replacement in [
            None,
            Some(image("{")),
            Some(MapImage {
                uid: 1000,
                mode: 0o644,
                mtime: 2,
                revision: None,
                json: VALID_MAP.into(),
            }),
        ] {
            let mut cache = MapCache::default();
            let first = image(VALID_MAP);
            let old = cache.accept(Some(&first), peer(), &mut sources());
            assert_eq!(old, Some(paul()));
            let mut replacement = replacement;
            if let Some(f) = replacement.as_mut() {
                f.mtime = 2;
            }
            assert_eq!(
                cache.accept(replacement.as_ref(), peer(), &mut sources()),
                None
            );
            assert_eq!(
                cache.map,
                LoadedMap::default(),
                "no last-known-good fallback"
            );
            assert_eq!(old, Some(paul()));
        }
    }
}
