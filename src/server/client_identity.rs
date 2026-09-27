//! Which person an attached client is (smarty-dev#1515).
//!
//! Agents run as the same user as the Herdr server. They can edit `authorized_keys`, rc files,
//! the environment and the org map in the user's checkout, and they can use a forwarded
//! ssh-agent. So a client is attributed to a person only from root-owned facts, and only when
//! two independent facts agree with the root-owned map `/etc/herdr/principals.json`:
//!
//! 1. the client is a `herdr remote-client-bridge` whose nearest ancestors reach a root
//!    `sshd: <user> [priv]` process (never through the Herdr server), and that process's
//!    `Accepted publickey … from <ip> … SHA256:<fp>` journald record (trusted `_PID`, `_UID=0`)
//!    names a key fingerprint mapped to the person;
//! 2. `tailscale whois <ip>` names a node and login mapped to the same person.
//!
//! Anything else (a local attach, mosh, an unmapped key or node, a mismatch, a journal miss, a
//! map that is not root-owned) gives no principal. The lookups sit behind [`IdentityHost`] so
//! tests fake them.

use std::path::Path;

use serde::Deserialize;

use crate::platform::{IdentityJournalRecord, IdentityProcess};

pub(crate) const PRINCIPALS_PATH: &str = "/etc/herdr/principals.json";

/// Parents walked from the bridge to the sshd `[priv]` process: `sshd: user@notty`, and at most
/// one shell or wrapper between them.
const MAX_ANCESTORS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrincipalsFile {
    pub version: u32,
    pub principals: Vec<PrincipalEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrincipalEntry {
    /// The display name exactly as in `setup/org.json`.
    pub name: String,
    /// OpenSSH fingerprints, `SHA256:<base64>`.
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    #[serde(default)]
    pub tailscale_nodes: Vec<TailscaleNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TailscaleNode {
    /// MagicDNS name without the trailing dot, or the node's stable ID.
    pub node: String,
    /// The Tailscale login name that owns the node.
    pub login: String,
}

/// Who may own the principals file and its directories.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustPolicy {
    pub owner_uid: u32,
    /// Also check every ancestor directory, so no writable directory can swap the file.
    pub check_ancestors: bool,
}

impl TrustPolicy {
    pub(crate) const ROOT: Self = Self {
        owner_uid: 0,
        check_ancestors: true,
    };
}

/// Reads the principals map, refusing a file (or directory above it) that the trusted owner
/// does not own or that group or others can write.
#[cfg(unix)]
pub(crate) fn load_principals(path: &Path, policy: TrustPolicy) -> Result<PrincipalsFile, String> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    let trusted = |meta: &std::fs::Metadata, what: &Path| {
        if meta.uid() != policy.owner_uid {
            return Err(format!(
                "{} is owned by uid {}, not {}",
                what.display(),
                meta.uid(),
                policy.owner_uid
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{} is group or other writable ({:o})",
                what.display(),
                meta.mode() & 0o7777
            ));
        }
        Ok(())
    };
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|err| format!("{}: {err}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    trusted(&meta, path)?;
    if policy.check_ancestors {
        for dir in path.ancestors().skip(1) {
            if dir.as_os_str().is_empty() {
                break;
            }
            let meta = std::fs::metadata(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
            trusted(&meta, dir)?;
        }
    }
    let mut text = String::new();
    file.take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let map: PrincipalsFile =
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if map.version != 1 {
        return Err(format!(
            "{}: unsupported version {}",
            path.display(),
            map.version
        ));
    }
    Ok(map)
}

#[cfg(not(unix))]
pub(crate) fn load_principals(path: &Path, _policy: TrustPolicy) -> Result<PrincipalsFile, String> {
    Err(format!(
        "{}: client identity is not supported here",
        path.display()
    ))
}

/// The host facts the resolver reads. Production reads `/proc`, journald and tailscaled.
pub(crate) trait IdentityHost {
    fn process(&self, pid: u32) -> Option<IdentityProcess>;
    fn journal_records(&self, pid: u32, since_unix_secs: u64)
        -> Option<Vec<IdentityJournalRecord>>;
    fn tailscale_whois(&self, ip: &str) -> Option<String>;
}

pub(crate) struct SystemIdentityHost;

impl IdentityHost for SystemIdentityHost {
    fn process(&self, pid: u32) -> Option<IdentityProcess> {
        crate::platform::identity_process(pid)
    }

    fn journal_records(
        &self,
        pid: u32,
        since_unix_secs: u64,
    ) -> Option<Vec<IdentityJournalRecord>> {
        crate::platform::identity_journal_records(pid, since_unix_secs)
    }

    fn tailscale_whois(&self, ip: &str) -> Option<String> {
        crate::platform::identity_tailscale_whois(ip)
    }
}

/// The SSH login that carried a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshLogin {
    pub user: String,
    pub ip: String,
    pub fingerprint: String,
}

/// The Tailscale peer behind an IP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TailscalePeer {
    pub node_name: String,
    pub node_stable_id: Option<String>,
    pub login: String,
}

/// Resolves the principal for the client at `peer_pid`, or `None`.
pub(crate) fn resolve_principal(
    host: &dyn IdentityHost,
    map: &PrincipalsFile,
    peer_pid: u32,
    server_pid: u32,
) -> Option<String> {
    let login = ssh_login_for_bridge(host, peer_pid, server_pid)?;
    let whois = host.tailscale_whois(&login.ip)?;
    let peer = parse_tailscale_whois(&whois)?;
    let mut matches = map.principals.iter().filter(|principal| {
        principal
            .ssh_keys
            .iter()
            .any(|key| key == &login.fingerprint)
            && principal.tailscale_nodes.iter().any(|node| {
                node.login == peer.login
                    && (node.node == peer.node_name
                        || peer.node_stable_id.as_deref() == Some(node.node.as_str()))
            })
    });
    let principal = matches.next()?;
    // Two people mapped to the same key and node is a broken map: name nobody.
    if matches.next().is_some() || principal.name.trim().is_empty() {
        return None;
    }
    Some(principal.name.clone())
}

/// The SSH login of the root sshd `[priv]` process that owns the bridge at `peer_pid`.
pub(crate) fn ssh_login_for_bridge(
    host: &dyn IdentityHost,
    peer_pid: u32,
    server_pid: u32,
) -> Option<SshLogin> {
    if peer_pid <= 1 || peer_pid == server_pid {
        return None;
    }
    let bridge = host.process(peer_pid)?;
    let is_herdr = bridge
        .exe_name
        .as_deref()
        .is_some_and(|name| name == "herdr" || name.starts_with("herdr-"));
    if !is_herdr
        || !bridge
            .cmdline
            .iter()
            .any(|arg| arg == "remote-client-bridge")
    {
        return None;
    }
    let mut pid = bridge.ppid;
    let mut child_start = bridge.start_ticks;
    for _ in 0..MAX_ANCESTORS {
        if pid <= 1 || pid == server_pid {
            return None;
        }
        let process = host.process(pid)?;
        // A parent never starts after its child; otherwise the pid was reused.
        if process.start_ticks > child_start {
            return None;
        }
        if let Some(user) = sshd_priv_user(&process) {
            let login = accepted_login(host, pid, &process, &user)?;
            // The bridge must still be the process we inspected.
            let again = host.process(peer_pid)?;
            if again.start_ticks != bridge.start_ticks {
                return None;
            }
            return Some(login);
        }
        child_start = process.start_ticks;
        pid = process.ppid;
    }
    None
}

/// The user of a root `sshd: <user> [priv]` monitor process, or `None`.
fn sshd_priv_user(process: &IdentityProcess) -> Option<String> {
    if process.uids.is_empty() || process.uids.iter().any(|uid| *uid != 0) {
        return None;
    }
    if process.comm != "sshd" && process.comm != "sshd-session" {
        return None;
    }
    let title = process.cmdline.join(" ");
    let rest = title
        .strip_prefix("sshd: ")
        .or_else(|| title.strip_prefix("sshd-session: "))?;
    let user = rest.strip_suffix(" [priv]")?;
    (!user.is_empty() && !user.contains(char::is_whitespace)).then(|| user.to_string())
}

fn accepted_login(
    host: &dyn IdentityHost,
    pid: u32,
    process: &IdentityProcess,
    user: &str,
) -> Option<SshLogin> {
    let records = host.journal_records(pid, process.start_unix_secs)?;
    let mut logins = records
        .iter()
        .filter(|record| {
            record.pid == Some(pid)
                && record.uid == Some(0)
                && matches!(record.comm.as_deref(), Some("sshd" | "sshd-session"))
        })
        .filter_map(|record| parse_accepted_publickey(record.message.as_deref()?));
    let login = logins.next()?;
    // One monitor process accepts one login; more than one is not a record we understand.
    if logins.next().is_some() || login.user != user {
        return None;
    }
    Some(login)
}

/// Parses `Accepted publickey for <user> from <ip> port <port> ssh2: <type> SHA256:<fp>`.
pub(crate) fn parse_accepted_publickey(message: &str) -> Option<SshLogin> {
    let rest = message.strip_prefix("Accepted publickey for ")?;
    let words = rest.split(' ').collect::<Vec<_>>();
    // user from ip port N ssh2: TYPE FP [optional suffix such as "ID ... (serial ...) CA ..."]
    if words.len() < 8
        || words[1] != "from"
        || words[3] != "port"
        || words[5] != "ssh2:"
        || words[4].parse::<u16>().is_err()
    {
        return None;
    }
    let ip = words[2];
    if ip.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    let fingerprint = words[7];
    if !fingerprint.starts_with("SHA256:") || fingerprint.len() <= "SHA256:".len() {
        return None;
    }
    Some(SshLogin {
        user: words[0].to_string(),
        ip: ip.to_string(),
        fingerprint: fingerprint.to_string(),
    })
}

/// Parses `tailscale whois --json` output.
pub(crate) fn parse_tailscale_whois(json: &str) -> Option<TailscalePeer> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let node = value.get("Node")?;
    let node_name = node
        .get("Name")?
        .as_str()?
        .trim_end_matches('.')
        .to_string();
    let login = value
        .get("UserProfile")?
        .get("LoginName")?
        .as_str()?
        .to_string();
    if node_name.is_empty() || login.is_empty() {
        return None;
    }
    Some(TailscalePeer {
        node_name,
        node_stable_id: node
            .get("StableID")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        login,
    })
}

/// The principal for a newly accepted client, from the root-owned map; `None` without one.
pub(crate) fn principal_for_peer(peer_pid: Option<u32>) -> Option<String> {
    let peer_pid = peer_pid?;
    let path = Path::new(PRINCIPALS_PATH);
    if !path.exists() {
        return None;
    }
    let map = match load_principals(path, TrustPolicy::ROOT) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(err = %err, "refusing the Herdr principals map");
            return None;
        }
    };
    let principal = resolve_principal(&SystemIdentityHost, &map, peer_pid, std::process::id());
    tracing::info!(
        peer_pid,
        principal = principal.as_deref().unwrap_or("-"),
        "client identity resolved"
    );
    principal
}

#[cfg(test)]
mod tests;
