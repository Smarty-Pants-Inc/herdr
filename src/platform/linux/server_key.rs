//! Server attestation key for input-consumer answers (smarty-dev#2636, decision 3 B).
//!
//! The installed binary is `root:herdr 2755`; `/etc/herdr/server.key` is `0640 root:herdr`.
//! Only the server reads the key, first thing in `main`: before any config, environment-derived
//! path, plugin or hook. It holds the key in locked memory, then drops the saved and effective
//! gid back to the real gid, so no child ever runs with egid `herdr`. The process stays
//! non-dumpable, so same-uid processes cannot ptrace it or read its memory. Every other mode
//! (CLI, client, bridges) drops the gid without touching the key. Any failure leaves no key:
//! the capability is false and enroll is refused. There is no unsigned fallback.
//!
//! The key is provisioned only by Paul's reviewed root block (smarty-dev#2636), which runs the
//! same account checks as `key_group_is_private` before it writes the key. This runtime check
//! is defense in depth: it refuses a key whose group has since gained a member.
//!
//! Every mode, setgid or not, also refuses to run when, after the verified drop, the real gid
//! or a supplementary group is a key gid (herdr#188, same rule): any gid named `herdr` in
//! `/etc/group`, the group owner of `/etc/herdr/server.key` or `server.pub` if present (by
//! `lstat`; a symlink refuses, EACCES counts as no reachable key), or the starting effective
//! or saved gid when it differs from the real gid. Those survive the drop and would reach
//! every pane child, which could then read the key file. A missing or unreadable
//! `/etc/group` refuses too. The key itself is read before the drop (only the setgid group
//! can open it); a refusal exits before any child exists, so the key never reaches one.
//!
//! Load rules, in order (the root block, smarty-dev#2636, runs rules 2 to 6 too):
//! 1. the process is a setgid `herdr server` invocation and `fs.suid_dumpable` is 0 or 2;
//! 2. the key gid is not one of the process's supplementary groups (`getgroups`): an
//!    inherited supplementary `herdr` survives `setresgid` and proves a member exists;
//! 3. `nsswitch.conf` maps `passwd`, `group` (and `initgroups`, if listed) to exactly
//!    `files` or `files systemd`;
//! 4. `/etc/group` has exactly one record with the key gid (no alias name), no other record
//!    with its name, and no members; `/etc/passwd` gives nobody it as primary group. With
//!    one name, every name-based check below covers every name of the gid;
//! 5. no userdb drop-in (`/etc/userdb`, `/run/userdb`, `/run/host/userdb`, `/usr/lib/userdb`)
//!    names the group as a membership or primary group, or defines the gid under another
//!    name;
//! 6. when NSS lists `systemd`, every Varlink userdb service in `/run/systemd/userdb` (homed,
//!    machined, DynamicUser, third-party) answers `GetMemberships(groupName)` and
//!    `GetGroupRecord(gid)` with `NoRecordFound` (no alias or second member list), and
//!    enumerates its users (`GetUserRecord`, no name) with no record whose `gid` is the key
//!    gid or whose `memberOf` names the group (an empty enumeration is fine). Only
//!    `io.systemd.Machine` may refuse enumeration (`EnumerationNotSupported`). All within 2 s total; any
//!    membership, other error, timeout or bad reply refuses. `io.systemd.Multiplexer` (an
//!    aggregate of the others) and `io.systemd.NameServiceSwitch` (a re-export of NSS) are
//!    skipped;
//! 7. the key file is a regular file (no symlink), `root:<key gid>`, no group-write or other
//!    bits, at most 4096 bytes, in a root-owned directory without group or other write;
//! 8. after the gid drop the process is verified non-dumpable and untraced.
//!
//! Any failure, unreadable file or unparseable record refuses.
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ring::signature::Ed25519KeyPair;

const KEY_PATH: &str = "/etc/herdr/server.key";
const PUB_PATH: &str = "/etc/herdr/server.pub";
const KEY_LIMIT: u64 = 4096;

pub(crate) const SETGID_REFUSAL: &str = "herdr: refusing to run with a set-group-id it cannot drop";
pub(crate) const HERDR_GROUP_REFUSAL: &str =
    "herdr: refusing to run with a group that may read the herdr server key";

const GROUP_FILE: &str = "/etc/group";
const HERDR_GROUP: &str = "herdr";

struct LockedKey(Box<Ed25519KeyPair>);

static KEY: OnceLock<LockedKey> = OnceLock::new();

/// Credential operations, injectable so tests can check the order without root.
pub(crate) trait GidOps {
    fn getresgid(&mut self) -> (u32, u32, u32);
    fn setresgid(&mut self, gid: u32) -> bool;
    /// The gid drop resets dumpability to `fs.suid_dumpable`. Only `1` (same-user ptrace
    /// and /proc/<pid>/mem) would expose the key in the instant before the re-assert.
    fn suid_dumpable_safe(&mut self) -> bool;
    /// Sets the process non-dumpable and proves it: `PR_GET_DUMPABLE` is 0 and no tracer is
    /// attached. False means the key must be discarded.
    fn make_non_dumpable(&mut self) -> bool;
    fn supplementary_groups(&mut self) -> std::io::Result<Vec<u32>>;
    /// Contents of `/etc/group`; a missing file is an error.
    fn group_file(&mut self) -> std::io::Result<String>;
    /// Group owners of the key files that exist (see `key_file_gids`).
    fn key_file_groups(&mut self) -> std::io::Result<Vec<u32>>;
}

struct Kernel;

impl GidOps for Kernel {
    fn getresgid(&mut self) -> (u32, u32, u32) {
        let (mut r, mut e, mut s) = (0, 0, 0);
        // SAFETY: getresgid writes three gids into valid output pointers.
        if unsafe { libc::getresgid(&mut r, &mut e, &mut s) } != 0 {
            // Unknown credentials: treat as setgid so the caller drops to the real gid.
            // SAFETY: getgid takes no arguments and cannot fail.
            let real = unsafe { libc::getgid() };
            return (real, u32::MAX, u32::MAX);
        }
        (r, e, s)
    }
    fn setresgid(&mut self, gid: u32) -> bool {
        // SAFETY: setresgid takes plain integers.
        unsafe { libc::setresgid(gid, gid, gid) == 0 }
    }
    fn suid_dumpable_safe(&mut self) -> bool {
        std::fs::read_to_string("/proc/sys/fs/suid_dumpable")
            .is_ok_and(|v| matches!(v.trim(), "0" | "2"))
    }
    fn make_non_dumpable(&mut self) -> bool {
        // SAFETY: prctl with integer arguments only.
        let set = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0;
        // SAFETY: as above; PR_GET_DUMPABLE returns the current mode.
        let mode = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
        let untraced = std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
            status
                .lines()
                .find_map(|l| l.strip_prefix("TracerPid:"))
                .is_some_and(|pid| pid.trim() == "0")
        });
        set && mode == 0 && untraced
    }
    fn supplementary_groups(&mut self) -> std::io::Result<Vec<u32>> {
        process_groups()
    }
    fn group_file(&mut self) -> std::io::Result<String> {
        std::fs::read_to_string(GROUP_FILE)
    }
    fn key_file_groups(&mut self) -> std::io::Result<Vec<u32>> {
        key_file_gids(&[Path::new(KEY_PATH), Path::new(PUB_PATH)])
    }
}

/// The group owner of each path that exists, by `lstat` (never followed). A missing path adds
/// nothing; a symlink or any other error fails.
// ponytail: EACCES also adds nothing, as in herdr#188: the check runs after the gid drop, so
// neither this process nor its children (same credentials) can reach that key.
fn key_file_gids(paths: &[&Path]) -> std::io::Result<Vec<u32>> {
    let mut gids = Vec::new();
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(std::io::Error::other(format!(
                    "{} is a symlink",
                    path.display()
                )));
            }
            Ok(meta) => gids.push(meta.gid()),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(gids)
}

/// The process's supplementary group list. On Linux it never includes the egid gained from
/// the setgid bit, so the key gid in it can only be inherited.
fn process_groups() -> std::io::Result<Vec<u32>> {
    loop {
        // SAFETY: a zero size with a null buffer only returns the group count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let len = usize::try_from(count).map_err(|_| std::io::Error::last_os_error())?;
        let mut groups = vec![0; len];
        // SAFETY: the buffer holds exactly `count` gid_t entries.
        let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if let Ok(filled) = usize::try_from(filled) {
            groups.truncate(filled);
            return Ok(groups);
        }
        let err = std::io::Error::last_os_error();
        // The list grew between the two calls; ask again.
        if err.raw_os_error() != Some(libc::EINVAL) {
            return Err(err);
        }
    }
}

/// The gids named `herdr` in `/etc/group` content. Read directly, not through NSS.
fn herdr_group_ids(group_file: &str) -> Vec<u32> {
    group_file
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let gid = fields.nth(1)?;
            (name == HERDR_GROUP).then(|| gid.trim().parse().ok())?
        })
        .collect()
}

/// Process entry point. Must run before anything reads configuration or the environment, or
/// opens a file, socket or child. Exits nonzero when the process cannot run safely.
pub(crate) fn acquire(args: &[OsString]) {
    match acquire_with(args, &mut Kernel, load) {
        Ok(key) => {
            if let Some(key) = key {
                let _ = KEY.set(key);
            }
        }
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(1);
        }
    }
}

fn acquire_with<K>(
    args: &[OsString],
    ops: &mut impl GidOps,
    load: impl FnOnce(bool, u32) -> Option<K>,
) -> Result<Option<K>, &'static str> {
    let (real, effective, saved) = ops.getresgid();
    let setgid = effective != real || saved != real;
    // The key is read before the drop: afterwards the process can no longer open it. A
    // setgid exec starts non-dumpable; refuse the key if the drop could make it ptraceable.
    let key = if is_server_invocation(args) && (!setgid || ops.suid_dumpable_safe()) {
        load(setgid, effective)
    } else {
        None
    };
    if setgid && (!ops.setresgid(real) || ops.getresgid() != (real, real, real)) {
        // Never continue with a group the drop could not shed.
        return Err(SETGID_REFUSAL);
    }
    // herdr#188, after the verified drop: a real or supplementary key gid survives the drop
    // and would reach every pane child. Key gids: the starting effective and saved gid when
    // they differ from the real gid, every gid named `herdr` in /etc/group, and the group
    // owner of each /etc/herdr key file that exists (so a supplementary gid /etc/group does
    // not map still refuses when a key file carries it). getgroups failure, a missing or
    // unreadable /etc/group, or a key-file symlink or lstat error refuses. A key read above
    // never outlives a refusal: the process exits before any child exists.
    let groups = ops
        .supplementary_groups()
        .map_err(|_| HERDR_GROUP_REFUSAL)?;
    let group_file = ops.group_file().map_err(|_| HERDR_GROUP_REFUSAL)?;
    let mut key_gids: Vec<u32> = [effective, saved]
        .into_iter()
        .filter(|&gid| gid != real)
        .collect();
    key_gids.extend(herdr_group_ids(&group_file));
    key_gids.extend(ops.key_file_groups().map_err(|_| HERDR_GROUP_REFUSAL)?);
    if groups
        .iter()
        .chain(std::iter::once(&real))
        .any(|gid| key_gids.contains(gid))
    {
        return Err(HERDR_GROUP_REFUSAL);
    }
    // Re-assert after the credential change and verify it; otherwise discard the key
    // (fail closed: capability false, no enrollment).
    Ok(key.filter(|_| ops.make_non_dumpable()))
}

/// `herdr [--session S] server` and its live-handoff import forms. Nothing else loads the key.
fn is_server_invocation(args: &[OsString]) -> bool {
    let mut rest = args.iter().skip(1).map(|a| a.to_str());
    let mut next = rest.next();
    loop {
        match next {
            Some(Some("--session")) => {
                rest.next();
                next = rest.next();
            }
            Some(Some(arg)) if arg.starts_with("--session=") => next = rest.next(),
            _ => break,
        }
    }
    next == Some(Some("server"))
        && matches!(
            rest.next(),
            None | Some(Some("--handoff-import" | "--import-from-running"))
        )
}

fn load(setgid: bool, effective_gid: u32) -> Option<LockedKey> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("HERDR_TEST_SERVER_KEY_PATH") {
        // Test-only trust root; release builds read only KEY_PATH.
        return read_key(Path::new(&path), None);
    }
    load_with(
        setgid,
        effective_gid,
        process_groups().map_err(|e| format!("getgroups failed: {e}")),
        &SYSTEM_ACCOUNTS,
        read_key,
    )
}

fn not_supplementary(groups: Result<Vec<u32>, String>, gid: u32) -> Result<(), String> {
    if groups?.contains(&gid) {
        return Err(format!(
            "gid {gid} is a supplementary group of this process, so the group has a member; \
             remove that account from the group"
        ));
    }
    Ok(())
}

/// The account database files; injectable so tests never touch the real `/etc`.
struct AccountFiles<'a> {
    nsswitch: &'a str,
    group: &'a str,
    passwd: &'a str,
    /// systemd userdb drop-in directories that `nss-systemd` reads.
    userdb: &'a [&'a str],
    /// The Varlink userdb service sockets that `nss-systemd` queries.
    userdb_services: &'a str,
    /// Total deadline for all service queries.
    userdb_deadline: Duration,
}

const SYSTEM_ACCOUNTS: AccountFiles<'static> = AccountFiles {
    nsswitch: "/etc/nsswitch.conf",
    group: "/etc/group",
    passwd: "/etc/passwd",
    userdb: &[
        "/etc/userdb",
        "/run/userdb",
        "/run/host/userdb",
        "/usr/lib/userdb",
    ],
    userdb_services: "/run/systemd/userdb",
    userdb_deadline: Duration::from_secs(2),
};

/// The key file is `0640 root:herdr`, so every member of `herdr` could read it and forge
/// signatures. Load only when the group is provably the setgid binary's alone (NSS policy B,
/// smarty-dev#6690): NSS resolves `passwd`, `group` and `initgroups` from `files`, optionally
/// followed by `systemd`, so no other source (sss, ldap, nis) can grant membership; the local
/// files give the group no members and nobody as primary group; no systemd userdb drop-in
/// record or Varlink service names the group as a membership; and the process does not
/// carry the group as a supplementary group. Anything else refuses.
fn load_with<K>(
    setgid: bool,
    effective_gid: u32,
    groups: Result<Vec<u32>, String>,
    accounts: &AccountFiles,
    read: impl FnOnce(&Path, Option<u32>) -> Option<K>,
) -> Option<K> {
    if !setgid {
        return None;
    }
    let checks = not_supplementary(groups, effective_gid)
        .and_then(|()| key_group_is_private(accounts, effective_gid));
    if let Err(why) = checks {
        eprintln!("herdr: server key refused: {why}");
        return None;
    }
    read(Path::new(KEY_PATH), Some(effective_gid))
}

fn key_group_is_private(accounts: &AccountFiles, gid: u32) -> Result<(), String> {
    let read =
        |path: &str| std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"));
    let systemd = nss_is_files_or_systemd(&read(accounts.nsswitch)?)?;
    let name = group_has_no_members(&read(accounts.group)?, gid)?;
    no_primary_holder(&read(accounts.passwd)?, gid)?;
    for dir in accounts.userdb {
        userdb_grants_no_membership(Path::new(dir), &name, gid)?;
    }
    if systemd {
        userdb_services_report_no_membership(
            Path::new(accounts.userdb_services),
            &name,
            gid,
            accounts.userdb_deadline,
        )?;
    }
    Ok(())
}

/// `passwd` and `group` must be listed exactly as `files` or `files systemd`; `initgroups`
/// (supplementary groups at login) too when listed. Any other source, action term or
/// duplicate refuses. Returns whether any of them lists `systemd`, which is safe only with
/// the userdb drop-in scan and the Varlink service query below.
fn nss_is_files_or_systemd(conf: &str) -> Result<bool, String> {
    let (mut passwd, mut group, mut systemd) = (false, false, false);
    let mut seen = Vec::new();
    for raw in conf.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((db, sources)) = line.split_once(':') else {
            return Err(format!("nsswitch.conf: cannot parse `{line}`"));
        };
        let db = db.trim().to_ascii_lowercase();
        if !matches!(db.as_str(), "passwd" | "group" | "initgroups") {
            continue;
        }
        if seen.contains(&db) {
            return Err(format!("nsswitch.conf lists `{db}` twice"));
        }
        let sources = sources.split_whitespace().collect::<Vec<_>>();
        if sources != ["files"] && sources != ["files", "systemd"] {
            return Err(format!(
                "nsswitch.conf maps `{db}` to `{}`; the server key needs `{db}: files` or `{db}: files systemd`",
                sources.join(" ")
            ));
        }
        passwd |= db == "passwd";
        group |= db == "group";
        systemd |= sources.len() == 2;
        seen.push(db);
    }
    if !(passwd && group) {
        return Err("nsswitch.conf must map both `passwd` and `group`".into());
    }
    Ok(systemd)
}

const VARLINK_REPLY_LIMIT: usize = 256 * 1024;

const NO_RECORD: &str = "io.systemd.UserDatabase.NoRecordFound";
const NO_ENUMERATION: &str = "io.systemd.UserDatabase.EnumerationNotSupported";

/// The only service allowed to refuse user enumeration (`EnumerationNotSupported`, observed
/// on systemd 255). machined synthesizes a user and a same-id group for each container
/// user mapped into the host's transient range, so the `NoRecordFound` for
/// `GetGroupRecord(gid)` that every service must give shows that no machined user has the
/// key gid as primary group.
const ENUMERATION_EXEMPT: &str = "io.systemd.Machine";

/// `nss-systemd` asks every Varlink service socket in `/run/systemd/userdb` for records:
/// homed, machined, DynamicUser and any third-party service (through `io.systemd.Multiplexer`
/// when userdbd runs, otherwise socket by socket). This asks each socket directly, as
/// `nss-systemd` does without the multiplexer, so it needs neither userdbd nor `userdbctl`.
/// The multiplexer only aggregates the other sockets and `io.systemd.NameServiceSwitch`
/// re-exports NSS (files, checked above), so both are skipped. Every other entry must answer
/// `GetMemberships(groupName)` and `GetGroupRecord(gid)` with `NoRecordFound`, and enumerate
/// its users (`GetUserRecord` without a name) with no record whose `gid` is the key gid or
/// whose `memberOf` names the group; an empty enumeration (`NoRecordFound`) is fine. Only
/// `ENUMERATION_EXEMPT` may answer `EnumerationNotSupported`. A membership, a group record, any other error, a non-socket
/// entry, a connect or read error, a reply over 256 KiB, malformed JSON or the 2 s total
/// deadline refuses. A missing directory means no services exist.
fn userdb_services_report_no_membership(
    dir: &Path,
    group: &str,
    gid: u32,
    deadline: Duration,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut services = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: non-UTF-8 service name", dir.display()))?;
        if !matches!(
            name.as_str(),
            "io.systemd.Multiplexer" | "io.systemd.NameServiceSwitch"
        ) {
            services.push((name, entry.path()));
        }
    }
    // ponytail: one thread bounds every blocking step (connect included) by the deadline;
    // on timeout it is abandoned with its sockets, which have their own timeouts.
    let (tx, rx) = std::sync::mpsc::channel();
    let group = group.to_owned();
    let end = Instant::now() + deadline;
    std::thread::Builder::new()
        .name("herdr-userdb".into())
        .spawn(move || {
            let result = services
                .iter()
                .try_for_each(|(name, path)| query_service(name, path, &group, gid, end));
            let _ = tx.send(result);
        })
        .map_err(|e| format!("cannot query userdb services: {e}"))?;
    rx.recv_timeout(deadline)
        .unwrap_or_else(|_| Err("userdb services did not answer within the deadline".into()))
}

fn query_service(
    service: &str,
    path: &Path,
    group: &str,
    gid: u32,
    end: Instant,
) -> Result<(), String> {
    let fail = |what: String| format!("userdb service {service}: {what}");
    let memberships = varlink_more(
        service,
        path,
        "GetMemberships",
        serde_json::json!({ "groupName": group }),
        end,
        |reply| {
            let user = reply
                .get("userName")
                .map_or_else(|| "an account".to_owned(), ToString::to_string);
            Err(format!("reports {user} as a member of group {group}"))
        },
    )
    .map_err(fail)?;
    if memberships.as_deref() != Some(NO_RECORD) {
        return Err(fail(format!("GetMemberships: error {memberships:?}")));
    }
    // Any service group record with the key gid is a second name (or a second member list)
    // for it, which the name-based checks would miss: refuse it.
    let groups = varlink_more(
        service,
        path,
        "GetGroupRecord",
        serde_json::json!({ "gid": gid }),
        end,
        |reply| {
            let name = reply
                .pointer("/record/groupName")
                .map_or_else(|| "?".to_owned(), ToString::to_string);
            Err(format!("has a group record {name} with gid {gid}"))
        },
    )
    .map_err(fail)?;
    if groups.as_deref() != Some(NO_RECORD) {
        return Err(fail(format!("GetGroupRecord: error {groups:?}")));
    }
    let users = varlink_more(
        service,
        path,
        "GetUserRecord",
        serde_json::json!({}),
        end,
        |reply| match reply.get("record") {
            Some(record) if user_record_names_group(record, group, gid) => Err(format!(
                "user record {} has group {group} (gid {gid}) as primary group or membership",
                record
                    .get("userName")
                    .map_or_else(|| "?".to_owned(), ToString::to_string)
            )),
            Some(serde_json::Value::Object(_)) => Ok(()),
            _ => Err("malformed user record".into()),
        },
    )
    .map_err(fail)?;
    match users.as_deref() {
        None | Some(NO_RECORD) => Ok(()),
        // The gid query above already answered NoRecordFound.
        Some(NO_ENUMERATION) if service == ENUMERATION_EXEMPT => Ok(()),
        Some(other) => Err(fail(format!("user enumeration refused: {other}"))),
    }
}

/// Calls `io.systemd.UserDatabase.<method>` with `more` and passes each reply's parameters
/// to `each`. Returns `None` when the replies ended normally (the last one without
/// `continues`), or the error name when the service answered with an error.
fn varlink_more(
    service: &str,
    path: &Path,
    method: &str,
    mut parameters: serde_json::Value,
    end: Instant,
    mut each: impl FnMut(&serde_json::Value) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let remaining = || {
        end.checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "timed out".to_owned())
    };
    let mut stream = UnixStream::connect(path).map_err(|e| format!("connect: {e}"))?;
    parameters["service"] = service.into();
    let request = serde_json::json!({
        "method": format!("io.systemd.UserDatabase.{method}"),
        "parameters": parameters,
        "more": true,
    });
    let mut bytes = request.to_string().into_bytes();
    bytes.push(0);
    stream
        .set_write_timeout(Some(remaining()?))
        .and_then(|()| stream.write_all(&bytes))
        .map_err(|e| format!("write: {e}"))?;
    let (mut reply, mut total) = (Vec::new(), 0usize);
    let mut chunk = [0u8; 8192];
    loop {
        while let Some(end_of_message) = reply.iter().position(|&b| b == 0) {
            let message: Vec<u8> = reply.drain(..=end_of_message).collect();
            let message: serde_json::Value = serde_json::from_slice(&message[..end_of_message])
                .map_err(|_| format!("{method}: malformed reply"))?;
            if let Some(error) = message.get("error") {
                return Ok(Some(
                    error.as_str().unwrap_or("<non-string error>").to_owned(),
                ));
            }
            each(
                message
                    .get("parameters")
                    .unwrap_or(&serde_json::Value::Null),
            )
            .map_err(|e| format!("{method}: {e}"))?;
            if message.get("continues") != Some(&serde_json::Value::Bool(true)) {
                return Ok(None);
            }
        }
        stream
            .set_read_timeout(Some(remaining()?))
            .map_err(|e| format!("{method}: read: {e}"))?;
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Err(format!("{method}: closed without a reply")),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{method}: read: {e}")),
        };
        reply.extend_from_slice(&chunk[..n]);
        total += n;
        if total > VARLINK_REPLY_LIMIT {
            return Err(format!("{method}: reply too large"));
        }
    }
}

/// A user record (drop-in or service) whose `gid` is the key gid or whose `memberOf` names
/// the group, anywhere in the document (`perMachine` and other sections included). A
/// non-array `memberOf` counts as naming it (fail closed).
fn user_record_names_group(record: &serde_json::Value, name: &str, gid: u32) -> bool {
    let is_key_group = key_group_matcher(name, gid);
    json_values(record, "memberOf")
        .any(|v| v.as_array().is_none_or(|a| a.iter().any(&is_key_group)))
        || json_values(record, "gid").any(&is_key_group)
}

fn key_group_matcher(name: &str, gid: u32) -> impl Fn(&serde_json::Value) -> bool + '_ {
    let gid_text = gid.to_string();
    move |value| match value {
        serde_json::Value::String(s) => s == name || *s == gid_text,
        serde_json::Value::Number(n) => n.as_u64() == Some(u64::from(gid)),
        _ => false,
    }
}

/// The records of an `/etc/group` or `/etc/passwd` file, split into exactly `fields` fields.
/// NIS `+`/`-` entries and malformed lines are errors, never skipped.
fn records<'a>(
    text: &'a str,
    name: &'a str,
    fields: usize,
) -> impl Iterator<Item = Result<Vec<&'a str>, String>> + 'a {
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.is_empty() && !line.starts_with('#')
        })
        .map(move |line| {
            let parts = line.split(':').collect::<Vec<_>>();
            if line.starts_with(['+', '-']) || parts.len() != fields {
                return Err(format!("{name}: cannot parse `{line}`"));
            }
            Ok(parts)
        })
}

fn field_id(value: &str, name: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("{name}: bad id `{value}`"))
}

/// Returns the key group's name. Exactly one `/etc/group` record has this gid, no other record
/// has its name, and it is memberless. A second name (alias) for the gid would let a
/// membership by that name grant the gid past the name-based checks.
fn group_has_no_members(text: &str, gid: u32) -> Result<String, String> {
    let mut found: Option<String> = None;
    let mut names = Vec::new();
    for record in records(text, "/etc/group", 4) {
        let fields = record?;
        names.push(fields[0]);
        if field_id(fields[2], "/etc/group")? != gid {
            continue;
        }
        if let Some(first) = &found {
            return Err(format!(
                "gid {gid} has more than one name in /etc/group (`{first}`, `{}`); it must have one",
                fields[0]
            ));
        }
        found = Some(fields[0].to_owned());
        if !fields[3].trim().is_empty() {
            return Err(format!(
                "group {} (gid {gid}) has members `{}`; it must have none",
                fields[0], fields[3]
            ));
        }
    }
    let name = found.ok_or_else(|| format!("gid {gid} is not a local group in /etc/group"))?;
    if names.iter().filter(|n| **n == name).count() > 1 {
        return Err(format!(
            "group name {name} appears more than once in /etc/group"
        ));
    }
    Ok(name)
}

const USERDB_RECORD_LIMIT: u64 = 1 << 20;

/// `nss-systemd` adds memberships from userdb drop-ins: `<user>:<group>.membership` files,
/// `memberOf` in `*.user` records and `members` in `*.group` records (also inside
/// `perMachine` and other sections, so the whole document is searched). Any record that
/// names the key group (by name or gid) as a membership, or a user whose primary gid is the
/// key gid, refuses. A missing directory is fine; any other read or parse error refuses.
fn userdb_grants_no_membership(dir: &Path, name: &str, gid: u32) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let gid_text = gid.to_string();
    let is_key_group = key_group_matcher(name, gid);
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let path = entry.path();
        let file = entry.file_name();
        let Some(file) = file.to_str() else {
            return Err(format!("{}: non-UTF-8 file name", path.display()));
        };
        if let Some(stem) = file.strip_suffix(".membership") {
            // The membership is the file name itself; the content is not consulted by systemd.
            let Some((_, group)) = stem.split_once(':') else {
                return Err(format!("{}: cannot parse membership name", path.display()));
            };
            if group == name || group == gid_text {
                return Err(format!("{} names group {name}", path.display()));
            }
            continue;
        }
        let kind = if file.ends_with(".user") {
            "user"
        } else if file.ends_with(".group") {
            "group"
        } else {
            continue;
        };
        let record = read_userdb_record(&path)?;
        let names_key_group = match kind {
            "user" => user_record_names_group(&record, name, gid),
            _ => {
                let has_key_gid = json_values(&record, "gid").any(&is_key_group);
                // A record with the key gid under another name is an alias for it.
                let alias = has_key_gid
                    && json_values(&record, "groupName").any(|v| v.as_str() != Some(name));
                let is_key = has_key_gid || json_values(&record, "groupName").any(&is_key_group);
                alias
                    || is_key
                        && json_values(&record, "members")
                            .any(|v| v.as_array().is_none_or(|a| !a.is_empty()))
            }
        };
        if names_key_group {
            return Err(format!(
                "{} grants membership in group {name} (gid {gid})",
                path.display()
            ));
        }
    }
    Ok(())
}

fn read_userdb_record(path: &Path) -> Result<serde_json::Value, String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(USERDB_RECORD_LIMIT + 1).read_to_string(&mut text))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.len() as u64 > USERDB_RECORD_LIMIT {
        return Err(format!("{}: record too large", path.display()));
    }
    match serde_json::from_str(&text) {
        Ok(record @ serde_json::Value::Object(_)) => Ok(record),
        _ => Err(format!("{}: not a JSON object record", path.display())),
    }
}

/// Every value stored under `key` anywhere in the document.
fn json_values<'a>(
    value: &'a serde_json::Value,
    key: &'a str,
) -> Box<dyn Iterator<Item = &'a serde_json::Value> + 'a> {
    match value {
        serde_json::Value::Object(map) => Box::new(map.iter().flat_map(move |(k, v)| {
            let own = (k == key).then_some(v);
            own.into_iter().chain(json_values(v, key))
        })),
        serde_json::Value::Array(items) => {
            Box::new(items.iter().flat_map(move |v| json_values(v, key)))
        }
        _ => Box::new(std::iter::empty()),
    }
}

fn no_primary_holder(text: &str, gid: u32) -> Result<(), String> {
    for record in records(text, "/etc/passwd", 7) {
        let fields = record?;
        field_id(fields[2], "/etc/passwd")?;
        if field_id(fields[3], "/etc/passwd")? == gid {
            return Err(format!(
                "account {} has gid {gid} as its primary group",
                fields[0]
            ));
        }
    }
    Ok(())
}

/// A heap buffer pinned in RAM before any secret byte is written to it; wiped on drop.
struct LockedBuf(Vec<u8>);

impl LockedBuf {
    fn new(len: usize) -> Option<Self> {
        let buf = vec![0u8; len];
        // SAFETY: mlock pins the pages of this live, initialized allocation.
        (unsafe { libc::mlock(buf.as_ptr().cast(), len) } == 0).then_some(Self(buf))
    }
}

impl Drop for LockedBuf {
    fn drop(&mut self) {
        wipe(&mut self.0);
        // SAFETY: unlocks the range locked in `new`; the allocation is still live.
        unsafe { libc::munlock(self.0.as_ptr().cast(), self.0.len()) };
    }
}

impl Drop for LockedKey {
    fn drop(&mut self) {
        let size = std::mem::size_of::<Ed25519KeyPair>();
        let bytes = (&mut *self.0 as *mut Ed25519KeyPair).cast::<u8>();
        for i in 0..size {
            // SAFETY: the key pair is plain data with no drop glue that reads its bytes;
            // zeroing it before the Box frees it only erases the secret.
            unsafe { std::ptr::write_volatile(bytes.add(i), 0) };
        }
    }
}

/// Reads and parses on a thread whose whole stack is locked first, into locked buffers,
/// so no copy of the key (file bytes, base64, DER, parsed scalar) can reach swap.
fn read_key(path: &Path, group: Option<u32>) -> Option<LockedKey> {
    let path = path.to_owned();
    std::thread::Builder::new()
        .name("herdr-server-key".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            lock_own_stack()
                .then(|| read_key_locked(&path, group))
                .flatten()
        })
        .ok()?
        .join()
        .ok()
        .flatten()
}

fn lock_own_stack() -> bool {
    let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: pthread_getattr_np initializes attr for the calling thread on success.
    if unsafe { libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) } != 0 {
        return false;
    }
    let (mut base, mut size) = (std::ptr::null_mut(), 0usize);
    // SAFETY: attr was initialized above; outputs are valid pointers.
    let got = unsafe { libc::pthread_attr_getstack(attr.as_ptr(), &mut base, &mut size) } == 0;
    // SAFETY: destroys the attr initialized above.
    unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) };
    // SAFETY: locks this thread's own mapped stack range.
    got && unsafe { libc::mlock(base, size) } == 0
}

/// `group` is the required owning group in production (`None` only for the test seam, which
/// requires an owner-only file instead).
fn read_key_locked(path: &Path, group: Option<u32>) -> Option<LockedKey> {
    if group.is_some() {
        let dir = std::fs::metadata(path.parent()?).ok()?;
        if dir.uid() != 0 || dir.mode() & 0o022 != 0 {
            return None;
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let mode_ok = match group {
        Some(gid) => meta.uid() == 0 && meta.gid() == gid && meta.mode() & 0o027 == 0,
        None => meta.mode() & 0o077 == 0,
    };
    if !meta.is_file() || !mode_ok || meta.len() > KEY_LIMIT {
        return None;
    }
    let mut pem = LockedBuf::new(KEY_LIMIT as usize + 1)?;
    let mut len = 0;
    loop {
        match file.read(&mut pem.0[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
        if len > KEY_LIMIT as usize {
            return None;
        }
    }
    parse_pem(&pem.0[..len])
}

fn parse_pem(pem: &[u8]) -> Option<LockedKey> {
    use base64::Engine as _;
    const BEGIN: &[u8] = b"-----BEGIN PRIVATE KEY-----";
    const END: &[u8] = b"-----END PRIVATE KEY-----";
    let text = pem.trim_ascii();
    let body = text.strip_prefix(BEGIN)?.strip_suffix(END)?;
    let mut compact = LockedBuf::new(body.len())?;
    let mut used = 0;
    for &b in body.iter().filter(|b| !b.is_ascii_whitespace()) {
        compact.0[used] = b;
        used += 1;
    }
    let mut der = LockedBuf::new(used.div_ceil(4) * 3 + 3)?;
    let der_len = base64::engine::general_purpose::STANDARD
        .decode_slice(&compact.0[..used], &mut der.0)
        .ok()?;
    let mut slot = Box::<Ed25519KeyPair>::new_uninit();
    let size = std::mem::size_of::<Ed25519KeyPair>();
    // SAFETY: pins the slot before the parsed key is written into it; never freed while live.
    if unsafe { libc::mlock(slot.as_ptr().cast(), size) } != 0 {
        return None;
    }
    let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der.0[..der_len]).ok()?;
    slot.write(pair);
    // SAFETY: written just above.
    Some(LockedKey(unsafe { slot.assume_init() }))
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: volatile write to an owned, initialized byte; keeps the wipe from being elided.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

pub(crate) fn available() -> bool {
    KEY.get().is_some()
}

/// Called only to sign an enroll answer (see `app::api::input_consumer`).
pub(crate) fn sign(message: &[u8]) -> Option<Vec<u8>> {
    Some(KEY.get()?.0.sign(message).as_ref().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        ids: (u32, u32, u32),
        drop_ok: bool,
        suid_safe: bool,
        nodump_ok: bool,
        /// `setresgid` reports success but changes nothing.
        drop_ignored: bool,
        /// `None`: getgroups fails.
        groups: Option<Vec<u32>>,
        /// `None`: /etc/group is missing or unreadable.
        group_file: Option<String>,
        /// Key file group owners; `None`: a key file is a symlink or cannot be lstat'ed.
        key_files: Option<Vec<u32>>,
        calls: Vec<String>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                ids: (0, 0, 0),
                drop_ok: true,
                suid_safe: true,
                nodump_ok: true,
                drop_ignored: false,
                groups: Some(vec![1000]),
                group_file: Some("root:x:0:\nherdr:x:990:\npaul:x:1000:\n".into()),
                key_files: Some(Vec::new()),
                calls: Vec::new(),
            }
        }
    }
    impl GidOps for Fake {
        fn getresgid(&mut self) -> (u32, u32, u32) {
            self.calls.push("get".into());
            self.ids
        }
        fn setresgid(&mut self, gid: u32) -> bool {
            self.calls.push(format!("set {gid}"));
            if self.drop_ok && !self.drop_ignored {
                self.ids = (gid, gid, gid);
            }
            self.drop_ok
        }
        fn suid_dumpable_safe(&mut self) -> bool {
            self.calls.push("suid".into());
            self.suid_safe
        }
        fn make_non_dumpable(&mut self) -> bool {
            self.calls.push("nodump".into());
            self.nodump_ok
        }
        fn supplementary_groups(&mut self) -> std::io::Result<Vec<u32>> {
            self.calls.push("groups".into());
            self.groups
                .clone()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))
        }
        fn group_file(&mut self) -> std::io::Result<String> {
            self.calls.push("file".into());
            self.group_file
                .clone()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))
        }
        fn key_file_groups(&mut self) -> std::io::Result<Vec<u32>> {
            self.calls.push("keys".into());
            self.key_files
                .clone()
                .ok_or_else(|| std::io::Error::other("symlink"))
        }
    }
    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn server_key_is_read_before_the_gid_drop_and_dumpable_is_reasserted() {
        let mut fake = Fake {
            ids: (1000, 990, 990),
            drop_ok: true,
            ..Fake::default()
        };
        let mut seen = None;
        let key = acquire_with(&args(&["herdr", "server"]), &mut fake, |setgid, egid| {
            seen = Some((setgid, egid));
            Some(())
        });
        assert_eq!(key, Ok(Some(())));
        assert_eq!(seen, Some((true, 990)));
        assert_eq!(
            fake.calls,
            ["get", "suid", "set 1000", "get", "groups", "file", "keys", "nodump"]
        );
        assert_eq!(fake.ids, (1000, 1000, 1000));
    }

    #[test]
    fn server_key_client_and_cli_modes_never_load_and_still_drop() {
        for argv in [
            &["herdr"][..],
            &["herdr", "client"],
            &["herdr", "server", "stop"],
            &["herdr", "remote-client-bridge"],
            &["herdr", "pane", "run", "server"],
            &["herdr", "--session"],
        ] {
            let mut fake = Fake {
                ids: (1000, 990, 990),
                drop_ok: true,
                ..Fake::default()
            };
            let key = acquire_with(&args(argv), &mut fake, |_, _| -> Option<()> {
                panic!("{argv:?} must not load the key")
            });
            assert_eq!(key, Ok(None));
            assert_eq!(fake.ids, (1000, 1000, 1000), "{argv:?}");
            assert!(!fake.calls.contains(&"nodump".to_string()));
        }
    }

    #[test]
    fn server_key_unsafe_suid_dumpable_never_loads_but_still_drops() {
        let mut fake = Fake {
            ids: (1000, 990, 990),
            suid_safe: false,
            ..Fake::default()
        };
        let key = acquire_with(
            &args(&["herdr", "server"]),
            &mut fake,
            |_, _| -> Option<()> {
                panic!("must not read the key when the drop could make it ptraceable")
            },
        );
        assert_eq!(key, Ok(None));
        assert_eq!(fake.ids, (1000, 1000, 1000));
    }

    #[test]
    fn server_key_unverified_non_dumpable_discards_the_key() {
        // Review r3 F1: a refused or unverified PR_SET_DUMPABLE keeps no key.
        let mut fake = Fake {
            ids: (1000, 990, 990),
            nodump_ok: false,
            ..Fake::default()
        };
        let key = acquire_with(&args(&["herdr", "server"]), &mut fake, |_, _| Some(()));
        assert_eq!(key, Ok(None));
        assert_eq!(fake.calls.last().map(String::as_str), Some("nodump"));
        // Counterpart: the real kernel path succeeds for this unprivileged test process.
        assert!(Kernel.make_non_dumpable());
        // SAFETY: restores the default so later tests in this process are unaffected.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    }

    const CLEAN_NSS: &str = "# comment\npasswd:  files\ngroup:   files # local\nhosts: files dns\n";
    const CLEAN_GROUP: &str = "root:x:0:\nherdr:x:990:\nusers:x:100:alice,bob\n";
    const CLEAN_PASSWD: &str =
        "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:100::/home/alice:/bin/sh\n";

    /// Runs `load_with` against injected account files; `Some(gid)` means the key was read.
    fn load_from(nss: &str, group: &str, passwd: &str) -> Option<Option<u32>> {
        load_from_userdb(nss, group, passwd, &[])
    }

    /// As `load_from`, with `(file name, content)` drop-ins in one userdb directory. A second,
    /// missing userdb directory is always listed too: absence must not refuse.
    fn load_from_userdb(
        nss: &str,
        group: &str,
        passwd: &str,
        userdb: &[(&str, &str)],
    ) -> Option<Option<u32>> {
        load_case(nss, group, passwd, userdb, &[], Ok(Vec::new())).0
    }

    /// A fake Varlink userdb service socket.
    enum Service {
        /// Records each request and writes these bytes, whatever the method.
        Answer(&'static str),
        /// `NoRecordFound` for memberships; these replies for `GetUserRecord` and
        /// `GetGroupRecord`.
        Methods(&'static str, &'static str),
        /// Never accepts: connect succeeds into the backlog, the reply never comes.
        Hang,
        /// A regular file where a socket should be.
        NotSocket,
    }

    /// As `load_from_userdb`, plus fake services in the Varlink socket directory and an
    /// injected supplementary group list. Returns the load result and the requests received.
    fn load_case(
        nss: &str,
        group: &str,
        passwd: &str,
        userdb: &[(&str, &str)],
        services: &[(&str, Service)],
        groups: Result<Vec<u32>, String>,
    ) -> (Option<Option<u32>>, Vec<String>) {
        let dir = std::env::temp_dir().join(format!(
            "herdr-accounts-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let dir = TempDir(dir);
        let paths = ["nsswitch.conf", "group", "passwd"].map(|name| {
            dir.0
                .join(name)
                .to_str()
                .expect("utf-8 temp path")
                .to_owned()
        });
        for (path, text) in paths.iter().zip([nss, group, passwd]) {
            std::fs::write(path, text).expect("write");
        }
        let dropins = dir.0.join("userdb");
        std::fs::create_dir_all(&dropins).expect("userdb dir");
        for (name, text) in userdb {
            std::fs::write(dropins.join(name), text).expect("write drop-in");
        }
        let missing = dir.0.join("no-such-userdb");
        let userdb_dirs = [&dropins, &missing].map(|d| d.to_str().expect("utf-8").to_owned());
        let userdb_dirs = [userdb_dirs[0].as_str(), userdb_dirs[1].as_str()];
        let sockets = dir.0.join("svc");
        std::fs::create_dir_all(&sockets).expect("services dir");
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut idle = Vec::new();
        for (name, service) in services {
            let path = sockets.join(name);
            match service {
                Service::NotSocket => std::fs::write(&path, "").expect("write"),
                Service::Hang => {
                    idle.push(std::os::unix::net::UnixListener::bind(&path).expect("bind"))
                }
                Service::Answer(_) | Service::Methods(..) => {
                    let (memberships, users, groups): (&'static str, &'static str, &'static str) =
                        match service {
                            Service::Answer(reply) => (reply, reply, reply),
                            Service::Methods(users, groups) => (NO_RECORD_Z, users, groups),
                            _ => unreachable!(),
                        };
                    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
                    let requests = requests.clone();
                    // Serves until the test process exits (nextest runs one test per process).
                    std::thread::spawn(move || {
                        while let Ok((mut conn, _)) = listener.accept() {
                            let mut request = Vec::new();
                            let mut byte = [0u8; 1];
                            while conn.read_exact(&mut byte).is_ok() && byte[0] != 0 {
                                request.push(byte[0]);
                            }
                            let text = String::from_utf8(request).expect("utf-8 request");
                            let reply = if text.contains("GetMemberships") {
                                memberships
                            } else if text.contains("GetUserRecord") {
                                users
                            } else {
                                groups
                            };
                            requests.lock().expect("lock").push(text);
                            let _ = conn.write_all(reply.as_bytes());
                        }
                    });
                }
            }
        }
        let accounts = AccountFiles {
            nsswitch: &paths[0],
            group: &paths[1],
            passwd: &paths[2],
            userdb: &userdb_dirs,
            userdb_services: sockets.to_str().expect("utf-8"),
            userdb_deadline: Duration::from_millis(300),
        };
        let loaded = load_with(true, 990, groups, &accounts, |path, gid| {
            assert_eq!(path, Path::new(KEY_PATH));
            Some(gid)
        });
        drop(idle);
        let requests = requests.lock().expect("lock").clone();
        (loaded, requests)
    }

    #[test]
    fn server_key_userdb_service_membership_is_refused() {
        // Review r2 P1-a: homed, DynamicUser or a third-party Varlink service can grant
        // membership that no drop-in shows. Any reported membership refuses.
        let member = concat!(
            r#"{"parameters":{"userName":"mallory","groupName":"herdr"},"continues":true}"#,
            "\0",
            r#"{"parameters":{"userName":"eve","groupName":"herdr"}}"#,
            "\0"
        );
        let services = [
            ("io.systemd.Machine", Service::Answer(NO_RECORD_Z)),
            ("com.example.Users", Service::Answer(member)),
        ];
        let (loaded, requests) = load_case(
            SYSTEMD_NSS,
            CLEAN_GROUP,
            CLEAN_PASSWD,
            &[],
            &services,
            Ok(Vec::new()),
        );
        assert_eq!(loaded, None);
        let request = requests
            .iter()
            .find(|r| r.contains("com.example.Users"))
            .expect("the member service was asked");
        let request: serde_json::Value = serde_json::from_str(request).expect("json");
        assert_eq!(
            request,
            serde_json::json!({
                "method": "io.systemd.UserDatabase.GetMemberships",
                "parameters": {"groupName": "herdr", "service": "com.example.Users"},
                "more": true,
            })
        );
    }

    #[test]
    fn server_key_userdb_service_failure_or_timeout_refuses() {
        for service in [
            Service::Hang,
            Service::NotSocket,
            Service::Answer(""),
            Service::Answer("not json\0"),
            Service::Answer(r#"{"error":"io.systemd.UserDatabase.NoRecordFound"}"#),
            Service::Answer("{\"error\":\"org.varlink.service.MethodNotFound\"}\0"),
            Service::Answer("{\"error\":\"io.systemd.UserDatabase.ServiceNotAvailable\"}\0"),
        ] {
            let services = [
                ("io.systemd.DynamicUser", Service::Answer(NO_RECORD_Z)),
                ("com.example.Bad", service),
            ];
            let started = Instant::now();
            let (loaded, _) = load_case(
                SYSTEMD_NSS,
                CLEAN_GROUP,
                CLEAN_PASSWD,
                &[],
                &services,
                Ok(Vec::new()),
            );
            assert_eq!(loaded, None);
            assert!(started.elapsed() < Duration::from_secs(2), "deadline holds");
        }
        // An oversized reply refuses without reading it all.
        let big: &'static str = Box::leak("x".repeat(VARLINK_REPLY_LIMIT + 1).into_boxed_str());
        let services = [("com.example.Big", Service::Answer(big))];
        let (loaded, _) = load_case(
            SYSTEMD_NSS,
            CLEAN_GROUP,
            CLEAN_PASSWD,
            &[],
            &services,
            Ok(Vec::new()),
        );
        assert_eq!(loaded, None);
    }

    const NO_RECORD_Z: &str =
        "{\"error\":\"io.systemd.UserDatabase.NoRecordFound\",\"parameters\":{}}\0";
    const NO_ENUM_Z: &str =
        "{\"error\":\"io.systemd.UserDatabase.EnumerationNotSupported\",\"parameters\":{}}\0";
    /// DynamicUser-style enumeration of two unrelated users, as observed on ryzen2.
    const OTHER_USERS: &str = concat!(
        r#"{"parameters":{"record":{"userName":"svc-a","uid":65498,"gid":65498,"service":"x"}},"continues":true}"#,
        "\0",
        r#"{"parameters":{"record":{"userName":"svc-b","uid":65499,"gid":65499,"memberOf":["users"]}}}"#,
        "\0"
    );

    fn systemd_case(services: &[(&str, Service)]) -> (Option<Option<u32>>, Vec<String>) {
        load_case(
            SYSTEMD_NSS,
            CLEAN_GROUP,
            CLEAN_PASSWD,
            &[],
            services,
            Ok(Vec::new()),
        )
    }

    #[test]
    fn server_key_userdb_service_primary_holder_or_member_of_is_refused() {
        // Security verdict 6053925456: a Varlink provider can report a user whose primary gid
        // is herdr, or whose memberOf names it, without any GetMemberships answer.
        for users in [
            concat!(
                r#"{"parameters":{"record":{"userName":"svc-a","uid":65498,"gid":65498}},"continues":true}"#,
                "\0",
                r#"{"parameters":{"record":{"userName":"mallory","uid":1001,"gid":990}}}"#,
                "\0"
            ),
            "{\"parameters\":{\"record\":{\"userName\":\"mallory\",\"memberOf\":[\"herdr\"]}}}\0",
            "{\"parameters\":{\"record\":{\"userName\":\"m\",\"perMachine\":[{\"gid\":990}]}}}\0",
            "{\"parameters\":{\"record\":{\"userName\":\"mallory\",\"memberOf\":\"herdr\"}}}\0",
            // A reply without a record object is malformed.
            "{\"parameters\":{}}\0",
            // A stream that says it continues but ends is cut short.
            "{\"parameters\":{\"record\":{\"userName\":\"a\"}},\"continues\":true}\0",
        ] {
            let services = [("com.example.Users", Service::Methods(users, NO_RECORD_Z))];
            assert_eq!(systemd_case(&services).0, None, "{users}");
        }
        // Counterpart: unrelated users enumerate and load.
        let services = [(
            "com.example.Users",
            Service::Methods(OTHER_USERS, NO_RECORD_Z),
        )];
        let (loaded, requests) = systemd_case(&services);
        assert_eq!(loaded, Some(Some(990)));
        let request: serde_json::Value = serde_json::from_str(&requests[2]).expect("json");
        assert_eq!(
            request,
            serde_json::json!({
                "method": "io.systemd.UserDatabase.GetUserRecord",
                "parameters": {"service": "com.example.Users"},
                "more": true,
            })
        );
    }

    #[test]
    fn server_key_userdb_enumeration_refusal_is_exempt_only_for_machined() {
        let method_not_found = "{\"error\":\"org.varlink.service.MethodNotFound\"}\0";
        let group_990 = "{\"parameters\":{\"record\":{\"groupName\":\"vg\",\"gid\":990}}}\0";
        // Any other service refusing enumeration refuses, whatever the refusal.
        for (name, users) in [
            ("io.systemd.DynamicUser", NO_ENUM_Z),
            ("io.systemd.Home", NO_ENUM_Z),
            ("com.example.Users", NO_ENUM_Z),
            ("com.example.Users", method_not_found),
            ("io.systemd.Machine", method_not_found),
        ] {
            let services = [(name, Service::Methods(users, NO_RECORD_Z))];
            assert_eq!(systemd_case(&services).0, None, "{name} {users}");
        }
        // machined refusing enumeration still needs NoRecordFound for the key gid's group.
        for groups in [group_990, method_not_found, NO_ENUM_Z] {
            let services = [("io.systemd.Machine", Service::Methods(NO_ENUM_Z, groups))];
            assert_eq!(systemd_case(&services).0, None, "{groups}");
        }
        let services = [(
            "io.systemd.Machine",
            Service::Methods(NO_ENUM_Z, NO_RECORD_Z),
        )];
        let (loaded, requests) = systemd_case(&services);
        assert_eq!(loaded, Some(Some(990)));
        let request: serde_json::Value = serde_json::from_str(&requests[1]).expect("json");
        assert_eq!(
            request["parameters"],
            serde_json::json!({"gid": 990, "service": "io.systemd.Machine"})
        );
    }

    #[test]
    fn server_key_key_gid_alias_names_are_refused() {
        // Security round 3: a second name for the key gid would let a membership by that name
        // grant the gid past every name-based check. Every alias source refuses.
        for group in [
            "herdr:x:990:\nhgrp:x:990:\n",
            "hgrp:x:990:\nherdr:x:990:\n",
            "herdr:x:990:\nherdr:x:991:\n",
        ] {
            assert_eq!(load_from(CLEAN_NSS, group, CLEAN_PASSWD), None, "{group:?}");
        }
        // A userdb drop-in group record with the key gid under another name, alone or with a
        // membership or memberOf by that alias name.
        let alias = r#"{"groupName":"hgrp","gid":990}"#;
        for files in [
            &[("hgrp.group", alias)][..],
            &[("hgrp.group", alias), ("mallory:hgrp.membership", "")],
            &[
                ("hgrp.group", alias),
                (
                    "mallory.user",
                    r#"{"userName":"mallory","memberOf":["hgrp"]}"#,
                ),
            ],
            &[(
                "hgrp.group",
                r#"{"groupName":"hgrp","perMachine":[{"gid":990}]}"#,
            )],
        ] {
            for nss in [CLEAN_NSS, SYSTEMD_NSS] {
                let refused = load_from_userdb(nss, CLEAN_GROUP, CLEAN_PASSWD, files);
                assert_eq!(refused, None, "{files:?}");
            }
        }
        // A Varlink service group record for the key gid under another name (or any name).
        for record in [
            "{\"parameters\":{\"record\":{\"groupName\":\"hgrp\",\"gid\":990}}}\0",
            "{\"parameters\":{\"record\":{\"groupName\":\"herdr\",\"gid\":990,\"members\":[\"m\"]}}}\0",
        ] {
            for name in ["com.example.Users", "io.systemd.DynamicUser"] {
                let services = [(name, Service::Methods(OTHER_USERS, record))];
                assert_eq!(systemd_case(&services).0, None, "{name} {record}");
            }
        }
        // Counterparts: one name, other groups' records and a memberless same-name drop-in load.
        let files = [
            (
                "herdr.group",
                r#"{"groupName":"herdr","gid":990,"members":[]}"#,
            ),
            ("users.group", r#"{"groupName":"users","gid":100}"#),
        ];
        let loaded = load_from_userdb(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD, &files);
        assert_eq!(loaded, Some(Some(990)));
    }

    #[test]
    fn server_key_userdb_services_without_memberships_load() {
        // The services observed on ryzen2: DynamicUser enumerates unrelated users, machined
        // refuses enumeration and knows no gid 990. The multiplexer and the NSS re-export are
        // skipped (they would hang here if queried). An empty enumeration also loads.
        let services = [
            (
                "io.systemd.DynamicUser",
                Service::Methods(OTHER_USERS, NO_RECORD_Z),
            ),
            (
                "io.systemd.Machine",
                Service::Methods(NO_ENUM_Z, NO_RECORD_Z),
            ),
            ("com.example.Empty", Service::Answer(NO_RECORD_Z)),
            ("io.systemd.Multiplexer", Service::Hang),
            ("io.systemd.NameServiceSwitch", Service::Hang),
        ];
        let (loaded, requests) = load_case(
            SYSTEMD_NSS,
            CLEAN_GROUP,
            CLEAN_PASSWD,
            &[],
            &services,
            Ok(Vec::new()),
        );
        assert_eq!(loaded, Some(Some(990)));
        assert_eq!(requests.len(), 9);
        // No services at all, or no socket directory, also load.
        assert_eq!(
            load_from(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD),
            Some(Some(990))
        );
        assert_eq!(
            userdb_services_report_no_membership(
                Path::new("/nonexistent/herdr-userdb"),
                "herdr",
                990,
                Duration::from_millis(300)
            ),
            Ok(())
        );
        // Files-only NSS never asks the services: a hanging one does not matter.
        let services = [("com.example.Hang", Service::Hang)];
        let (loaded, _) = load_case(
            CLEAN_NSS,
            CLEAN_GROUP,
            CLEAN_PASSWD,
            &[],
            &services,
            Ok(Vec::new()),
        );
        assert_eq!(loaded, Some(Some(990)));
    }

    #[test]
    fn server_key_supplementary_key_gid_refuses() {
        // Review r2 P1-b: a supplementary herdr survives setresgid; it also proves a member.
        for groups in [
            Ok(vec![990]),
            Ok(vec![4, 24, 990, 1000]),
            Err("getgroups failed".into()),
        ] {
            let (loaded, _) = load_case(
                CLEAN_NSS,
                CLEAN_GROUP,
                CLEAN_PASSWD,
                &[],
                &[],
                groups.clone(),
            );
            assert_eq!(loaded, None, "{groups:?}");
        }
        for groups in [vec![], vec![4, 24, 100, 1000]] {
            let (loaded, _) = load_case(CLEAN_NSS, CLEAN_GROUP, CLEAN_PASSWD, &[], &[], Ok(groups));
            assert_eq!(loaded, Some(Some(990)));
        }
        // The real list for this test process is readable.
        assert!(process_groups().is_ok());
    }

    #[test]
    fn server_key_non_systemd_extra_nss_source_is_refused() {
        // NSS policy B (smarty-dev#6690): only `files` or `files systemd`. A member known only
        // to sss/ldap/nis is invisible to the local files and the userdb scan, so any other
        // source, order or action term for passwd, group or initgroups refuses the key.
        for nss in [
            "passwd: files\ngroup: files sss\n",
            "passwd: files\ngroup: sss files\n",
            "passwd: files\ngroup: files ldap\n",
            "passwd: files sss\ngroup: files\n",
            "passwd: files ldap\ngroup: files\n",
            "passwd: files systemd sss\ngroup: files systemd\n",
            "passwd: files systemd\ngroup: files systemd ldap\n",
            "passwd: files systemd\ngroup: systemd files\n",
            "passwd: systemd\ngroup: files\n",
            "passwd: files systemd\ngroup: files [NOTFOUND=return] systemd\n",
            "passwd: files systemd\ngroup: files systemd\ninitgroups: files systemd sss\n",
            "passwd: files systemd\ngroup: files systemd\ngroup: files systemd\n",
            "passwd: files systemd\ngroup: files systemd systemd\n",
            "passwd: compat\ngroup: compat\n",
            "passwd: files\ngroup: files [SUCCESS=merge] sss\n",
            "passwd: files\ngroup: files\ninitgroups: files sss\n",
            "passwd: files\ngroup: files\ngroup: files ldap\n",
            "passwd: files\n",
            "group: files\n",
            "",
            "passwd files\ngroup: files\n",
        ] {
            assert_eq!(load_from(nss, CLEAN_GROUP, CLEAN_PASSWD), None, "{nss:?}");
        }
    }

    #[test]
    fn server_key_files_systemd_nss_is_accepted() {
        for nss in [
            "passwd: files systemd\ngroup: files systemd\n",
            "passwd: files systemd\ngroup: files\n",
            "passwd:\tfiles\tsystemd # comment\ngroup: files   systemd\n",
            "passwd: files systemd\ngroup: files systemd\ninitgroups: files systemd\n",
            "passwd: files systemd\ngroup: files systemd\nshadow: files systemd\nhosts: files dns\n",
        ] {
            assert_eq!(
                load_from(nss, CLEAN_GROUP, CLEAN_PASSWD),
                Some(Some(990)),
                "{nss:?}"
            );
        }
    }

    const SYSTEMD_NSS: &str = "passwd: files systemd\ngroup: files systemd\n";

    #[test]
    fn server_key_userdb_membership_is_refused() {
        for nss in [SYSTEMD_NSS, CLEAN_NSS] {
            for name in ["mallory:herdr.membership", "mallory:990.membership"] {
                let refused = load_from_userdb(nss, CLEAN_GROUP, CLEAN_PASSWD, &[(name, "")]);
                assert_eq!(refused, None, "{name}");
            }
            // A herdr group record with members, found by name or by gid.
            for record in [
                r#"{"groupName":"herdr","gid":990,"members":["mallory"]}"#,
                r#"{"groupName":"alias","gid":990,"members":["mallory"]}"#,
                r#"{"groupName":"herdr","perMachine":[{"members":["mallory"]}]}"#,
            ] {
                let files = [("herdr.group", record)];
                let refused = load_from_userdb(nss, CLEAN_GROUP, CLEAN_PASSWD, &files);
                assert_eq!(refused, None, "{record}");
            }
            // Counterparts: other groups' memberships and a memberless herdr record load.
            let files = [
                ("alice:users.membership", ""),
                (
                    "users.group",
                    r#"{"groupName":"users","gid":100,"members":["alice"]}"#,
                ),
                (
                    "herdr.group",
                    r#"{"groupName":"herdr","gid":990,"members":[]}"#,
                ),
            ];
            let loaded = load_from_userdb(nss, CLEAN_GROUP, CLEAN_PASSWD, &files);
            assert_eq!(loaded, Some(Some(990)));
        }
    }

    #[test]
    fn server_key_userdb_user_member_of_herdr_is_refused() {
        for record in [
            r#"{"userName":"mallory","uid":1001,"memberOf":["wheel","herdr"]}"#,
            r#"{"userName":"mallory","uid":1001,"memberOf":["990"]}"#,
            r#"{"userName":"mallory","perMachine":[{"matchHostname":"h","memberOf":["herdr"]}]}"#,
            r#"{"userName":"mallory","uid":1001,"gid":990}"#,
            r#"{"userName":"mallory","uid":1001,"memberOf":"herdr"}"#,
        ] {
            let files = [("mallory.user", record)];
            let refused = load_from_userdb(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD, &files);
            assert_eq!(refused, None, "{record}");
        }
        let files = [(
            "alice.user",
            r#"{"userName":"alice","uid":1000,"gid":100,"memberOf":["wheel","users"]}"#,
        )];
        let loaded = load_from_userdb(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD, &files);
        assert_eq!(loaded, Some(Some(990)));
    }

    #[test]
    fn server_key_unreadable_or_unparseable_userdb_records_refuse() {
        for (name, text) in [
            ("bad.user", "not json"),
            ("bad.group", "[]"),
            ("bad.user", ""),
            ("nocolon.membership", ""),
        ] {
            let refused = load_from_userdb(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD, &[(name, text)]);
            assert_eq!(refused, None, "{name}: {text:?}");
        }
        // Unrelated files in a userdb directory are ignored, as systemd ignores them.
        let files = [("README", "not json"), ("x.conf", "")];
        let loaded = load_from_userdb(SYSTEMD_NSS, CLEAN_GROUP, CLEAN_PASSWD, &files);
        assert_eq!(loaded, Some(Some(990)));
        // A dangling record symlink and a non-directory userdb path refuse.
        let dir = std::env::temp_dir().join(format!("herdr-userdb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let dir = TempDir(dir);
        std::os::unix::fs::symlink(dir.0.join("gone"), dir.0.join("ghost.user")).expect("link");
        assert!(userdb_grants_no_membership(&dir.0, "herdr", 990).is_err());
        let file = dir.0.join("plain");
        std::fs::write(&file, "").expect("write");
        assert!(userdb_grants_no_membership(&file, "herdr", 990).is_err());
    }

    #[test]
    fn server_key_local_group_members_and_primary_holders_are_refused() {
        for (group, passwd) in [
            ("herdr:x:990:mallory\n", CLEAN_PASSWD),
            ("herdr:x:990:\nalias:x:990:mallory\n", CLEAN_PASSWD),
            (CLEAN_GROUP, "mallory:x:1001:990::/home/m:/bin/sh\n"),
            // Absent group, NIS compat entries and malformed records fail closed.
            ("root:x:0:\n", CLEAN_PASSWD),
            ("herdr:x:990:\n+:::\n", CLEAN_PASSWD),
            (CLEAN_GROUP, "root:x:0:0:root:/root:/bin/sh\n+::::::\n"),
            ("herdr:x:990\n", CLEAN_PASSWD),
            ("herdr:x:nine:\n", CLEAN_PASSWD),
            (CLEAN_GROUP, "alice:x:1000:100:/home/alice:/bin/sh\n"),
            (CLEAN_GROUP, "alice:x:1000:x::/home/alice:/bin/sh\n"),
        ] {
            assert_eq!(
                load_from(CLEAN_NSS, group, passwd),
                None,
                "{group:?} {passwd:?}"
            );
        }
    }

    #[test]
    fn server_key_clean_files_only_accounts_read_the_key() {
        assert_eq!(
            load_from(CLEAN_NSS, CLEAN_GROUP, CLEAN_PASSWD),
            Some(Some(990))
        );
        // initgroups listed as files only is fine too.
        let nss = "passwd: files\ngroup: files\ninitgroups: files\n";
        assert_eq!(load_from(nss, CLEAN_GROUP, CLEAN_PASSWD), Some(Some(990)));
        // Not setgid: never reads.
        assert_eq!(
            load_with(
                false,
                990,
                Ok(Vec::new()),
                &SYSTEM_ACCOUNTS,
                |_, _| -> Option<()> { panic!("must not read the key") }
            ),
            None
        );
        // Unreadable files refuse.
        let missing = "/nonexistent/herdr-accounts";
        let accounts = AccountFiles {
            nsswitch: missing,
            group: missing,
            passwd: missing,
            userdb: &[],
            userdb_services: missing,
            userdb_deadline: Duration::from_millis(300),
        };
        assert_eq!(
            load_with(true, 990, Ok(Vec::new()), &accounts, |_, _| Some(())),
            None
        );
    }

    #[test]
    fn server_key_server_invocations_are_exact() {
        for argv in [
            &["herdr", "server"][..],
            &["herdr", "--session", "work", "server"],
            &["herdr", "--session=work", "server"],
            &["herdr", "server", "--handoff-import", "/s", "t"],
            &[
                "herdr",
                "server",
                "--import-from-running",
                "--expect-source-pid",
                "1",
            ],
        ] {
            assert!(is_server_invocation(&args(argv)), "{argv:?}");
        }
    }

    #[test]
    fn server_key_not_setgid_production_gets_no_key_and_no_drop() {
        let mut fake = Fake {
            ids: (1000, 1000, 1000),
            drop_ok: true,
            ..Fake::default()
        };
        let key = acquire_with(&args(&["herdr", "server"]), &mut fake, |setgid, _| {
            (setgid).then_some(())
        });
        assert_eq!(key, Ok(None));
        assert_eq!(fake.calls, ["get", "groups", "file", "keys"]);
    }

    // Ported from herdr#188 group_privilege: every mode drops an inherited setgid, verifies
    // the drop, and refuses to run while the process holds the herdr group.
    fn run(fake: &mut Fake) -> Result<Option<()>, &'static str> {
        acquire_with(&args(&["herdr", "client"]), fake, |_, _| -> Option<()> {
            panic!("client mode must not load the key")
        })
    }

    #[test]
    fn server_key_group_a_saved_gid_alone_is_dropped() {
        let mut fake = Fake {
            ids: (1000, 1000, 990),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Ok(None));
        assert_eq!(fake.ids, (1000, 1000, 1000));
    }

    #[test]
    fn server_key_group_a_failed_or_unverified_drop_refuses() {
        for (drop_ok, drop_ignored) in [(false, false), (true, true)] {
            for argv in [&["herdr", "client"][..], &["herdr", "server"]] {
                let mut fake = Fake {
                    ids: (1000, 990, 990),
                    drop_ok,
                    drop_ignored,
                    ..Fake::default()
                };
                let refused = acquire_with(&args(argv), &mut fake, |_, _| Some(()));
                assert_eq!(refused, Err(SETGID_REFUSAL), "{argv:?}");
                assert!(!fake.calls.contains(&"nodump".to_string()));
            }
        }
    }

    #[test]
    fn server_key_group_herdr_membership_refuses_every_mode() {
        // Supplementary herdr (by /etc/group name or as the setgid egid) and a real herdr
        // gid reach every pane child, so no mode runs. The check follows the verified drop;
        // a key read before it is discarded with the refusal (the process exits).
        for (ids, groups) in [
            ((1000, 1000, 1000), vec![1000, 990]),
            ((1000, 990, 990), vec![1000, 990]),
            ((990, 990, 990), vec![990]),
        ] {
            for argv in [&["herdr", "client"][..], &["herdr", "server"]] {
                let mut fake = Fake {
                    ids,
                    groups: Some(groups.clone()),
                    ..Fake::default()
                };
                let refused = acquire_with(&args(argv), &mut fake, |_, _| Some(()));
                assert_eq!(refused, Err(HERDR_GROUP_REFUSAL), "{ids:?} {groups:?}");
                assert!(!fake.calls.contains(&"nodump".to_string()));
            }
        }
        // The setgid egid is the key group even when /etc/group does not name it herdr.
        let mut fake = Fake {
            ids: (1000, 977, 977),
            groups: Some(vec![1000, 977]),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        // getgroups failure refuses.
        let mut fake = Fake {
            groups: None,
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
    }

    #[test]
    fn server_key_group_key_file_group_and_unknown_key_groups_refuse() {
        // #188 security: a supplementary gid kept from an earlier login or NSS, which
        // /etc/group does not map, still refuses when a key file carries it.
        for argv in [&["herdr", "client"][..], &["herdr", "server"]] {
            let mut fake = Fake {
                ids: (1000, 1000, 1000),
                groups: Some(vec![1000, 4242]),
                key_files: Some(vec![4242, 4242]),
                ..Fake::default()
            };
            let refused = acquire_with(&args(argv), &mut fake, |_, _| Some(()));
            assert_eq!(refused, Err(HERDR_GROUP_REFUSAL), "{argv:?}");
        }
        // As the real gid too.
        let mut fake = Fake {
            ids: (4242, 4242, 4242),
            groups: Some(vec![4242]),
            key_files: Some(vec![4242]),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        // Missing or unreadable /etc/group, or a key file symlink, refuses.
        let mut fake = Fake {
            group_file: None,
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        let mut fake = Fake {
            key_files: None,
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        // Counterpart: a key file whose group the process does not hold runs.
        let mut fake = Fake {
            ids: (1000, 1000, 1000),
            groups: Some(vec![1000, 100]),
            key_files: Some(vec![4242]),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Ok(None));
    }

    #[test]
    fn server_key_group_key_file_gids_lstat_and_refuse_symlinks() {
        let dir = std::env::temp_dir().join(format!("herdr-keygid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let dir = TempDir(dir);
        let key = dir.0.join("server.key");
        std::fs::write(&key, "k").expect("write");
        let gid = std::fs::metadata(&key).expect("meta").gid();
        let missing = dir.0.join("server.pub");
        assert_eq!(key_file_gids(&[&key, &missing]).expect("lstat"), vec![gid]);
        assert_eq!(
            key_file_gids(&[&missing]).expect("lstat"),
            Vec::<u32>::new()
        );
        // A symlink refuses, even one to a valid file, and is never followed.
        let link = dir.0.join("link.pub");
        std::os::unix::fs::symlink(&key, &link).expect("symlink");
        assert!(key_file_gids(&[&key, &link]).is_err());
        let dangling = dir.0.join("dangling.key");
        std::os::unix::fs::symlink(dir.0.join("gone"), &dangling).expect("symlink");
        assert!(key_file_gids(&[&dangling]).is_err());
        // EACCES adds nothing: these credentials cannot reach that key (#188 semantics).
        // A non-root test only; root bypasses the directory mode.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let closed = dir.0.join("closed");
            std::fs::create_dir(&closed).expect("dir");
            std::fs::write(closed.join("server.key"), "k").expect("write");
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
                .expect("mode");
            let hidden = key_file_gids(&[&closed.join("server.key")]);
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700))
                .expect("mode");
            assert_eq!(hidden.expect("EACCES is no key"), Vec::<u32>::new());
        }
    }

    #[test]
    fn server_key_group_a_normal_process_is_unchanged() {
        let mut fake = Fake {
            ids: (1000, 1000, 1000),
            groups: Some(vec![1000, 100]),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Ok(None));
        assert!(!fake.calls.iter().any(|c| c.starts_with("set")));
    }

    #[test]
    fn server_key_group_herdr_group_ids_match_the_exact_name_only() {
        let file = "herdr-dev:x:5:\n#herdr:x:6:\nherdr:x:977:a,b\nbad\nherdr:x:nope:\n";
        assert_eq!(herdr_group_ids(file), vec![977]);
    }

    #[test]
    fn server_key_group_the_live_process_ids_are_readable() {
        let (real, ..) = Kernel.getresgid();
        // SAFETY: getgid has no preconditions.
        assert_eq!(real, unsafe { libc::getgid() });
        Kernel.supplementary_groups().expect("getgroups");
    }

    #[test]
    fn server_key_file_checks_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-key-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let dir = TempDir(dir);
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("key");
        use base64::Engine as _;
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref())
        );
        let path = dir.0.join("server.key");
        std::fs::write(&path, &pem).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(read_key(&path, None).is_some(), "owner-only test key loads");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("mode");
        assert!(
            read_key(&path, None).is_none(),
            "group-readable test key refused"
        );
        // Production rules: root owner, herdr group, root directory. A user file never passes.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("mode");
        let gid = std::fs::metadata(&path).expect("meta").gid();
        assert!(
            read_key(&path, Some(gid)).is_none(),
            "non-root owner refused"
        );
        let link = dir.0.join("link.key");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(read_key(&link, None).is_none(), "symlink refused");
        std::fs::write(&path, "not a key").expect("write");
        assert!(read_key(&path, None).is_none(), "garbage refused");
        assert!(read_key(Path::new("/nonexistent/server.key"), None).is_none());
    }
}
