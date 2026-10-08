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
//! Every mode, setgid or not, also refuses to run while the real gid or a supplementary group
//! is the key group (the setgid egid, or any gid named `herdr` in `/etc/group`; herdr#188):
//! those survive the drop and would reach every pane child, which could then read the key
//! file.
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::OnceLock;

use ring::signature::Ed25519KeyPair;

const KEY_PATH: &str = "/etc/herdr/server.key";
const KEY_LIMIT: u64 = 4096;

pub(crate) const SETGID_REFUSAL: &str = "herdr: refusing to run with a set-group-id it cannot drop";
pub(crate) const HERDR_GROUP_REFUSAL: &str =
    "herdr: refusing to run with the herdr group; pane children would inherit it";

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
    /// Contents of `/etc/group`; `Ok(None)` when the file does not exist.
    fn group_file(&mut self) -> std::io::Result<Option<String>>;
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
    fn group_file(&mut self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(GROUP_FILE) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
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
    // herdr#188: a real or supplementary key group survives the drop and reaches every pane
    // child. The key group is the setgid gid, or any gid named `herdr` in /etc/group (an
    // unreadable file refuses; a missing one names none). Checked before the key is touched.
    let mut key_groups = match ops.group_file().map_err(|_| HERDR_GROUP_REFUSAL)? {
        Some(contents) => herdr_group_ids(&contents),
        None => Vec::new(),
    };
    // The setgid-gained gid: whichever of effective and saved differs from the real gid.
    key_groups.extend([effective, saved].into_iter().filter(|&gid| gid != real));
    let groups = ops
        .supplementary_groups()
        .map_err(|_| HERDR_GROUP_REFUSAL)?;
    if groups
        .iter()
        .chain(std::iter::once(&real))
        .any(|gid| key_groups.contains(gid))
    {
        return Err(HERDR_GROUP_REFUSAL);
    }
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
    load_with(setgid, effective_gid, &SYSTEM_ACCOUNTS, read_key)
}

/// The account database files; injectable so tests never touch the real `/etc`.
struct AccountFiles<'a> {
    nsswitch: &'a str,
    group: &'a str,
    passwd: &'a str,
    /// systemd userdb drop-in directories that `nss-systemd` reads.
    userdb: &'a [&'a str],
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
};

/// The key file is `0640 root:herdr`, so every member of `herdr` could read it and forge
/// signatures. Load only when the group is provably the setgid binary's alone (NSS policy B,
/// smarty-dev#6690): NSS resolves `passwd`, `group` and `initgroups` from `files`, optionally
/// followed by `systemd`, so no other source (sss, ldap, nis) can grant membership; the local
/// files give the group no members and nobody as primary group; and no systemd userdb
/// drop-in record names the group as a membership. Anything else refuses.
fn load_with<K>(
    setgid: bool,
    effective_gid: u32,
    accounts: &AccountFiles,
    read: impl FnOnce(&Path, Option<u32>) -> Option<K>,
) -> Option<K> {
    if !setgid {
        return None;
    }
    if let Err(why) = key_group_is_private(accounts, effective_gid) {
        eprintln!("herdr: server key refused: {why}");
        return None;
    }
    read(Path::new(KEY_PATH), Some(effective_gid))
}

fn key_group_is_private(accounts: &AccountFiles, gid: u32) -> Result<(), String> {
    let read =
        |path: &str| std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"));
    nss_is_files_or_systemd(&read(accounts.nsswitch)?)?;
    let name = group_has_no_members(&read(accounts.group)?, gid)?;
    no_primary_holder(&read(accounts.passwd)?, gid)?;
    for dir in accounts.userdb {
        userdb_grants_no_membership(Path::new(dir), &name, gid)?;
    }
    Ok(())
}

/// `passwd` and `group` must be listed exactly as `files` or `files systemd`; `initgroups`
/// (supplementary groups at login) too when listed. Any other source, action term or
/// duplicate refuses. `systemd` is safe only with the userdb scan below.
fn nss_is_files_or_systemd(conf: &str) -> Result<(), String> {
    let (mut passwd, mut group) = (false, false);
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
        seen.push(db);
    }
    if !(passwd && group) {
        return Err("nsswitch.conf must map both `passwd` and `group`".into());
    }
    Ok(())
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

/// Returns the key group's name. Every `/etc/group` record with this gid must be memberless.
fn group_has_no_members(text: &str, gid: u32) -> Result<String, String> {
    let mut found = None;
    for record in records(text, "/etc/group", 4) {
        let fields = record?;
        if field_id(fields[2], "/etc/group")? != gid {
            continue;
        }
        found.get_or_insert_with(|| fields[0].to_owned());
        if !fields[3].trim().is_empty() {
            return Err(format!(
                "group {} (gid {gid}) has members `{}`; it must have none",
                fields[0], fields[3]
            ));
        }
    }
    found.ok_or_else(|| format!("gid {gid} is not a local group in /etc/group"))
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
    let is_key_group = |value: &serde_json::Value| match value {
        serde_json::Value::String(s) => s == name || s == &gid_text,
        serde_json::Value::Number(n) => n.as_u64() == Some(u64::from(gid)),
        _ => false,
    };
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
            "user" => {
                json_values(&record, "memberOf")
                    .any(|v| v.as_array().is_none_or(|a| a.iter().any(is_key_group)))
                    || json_values(&record, "gid").any(is_key_group)
            }
            _ => {
                let is_key = json_values(&record, "groupName").any(is_key_group)
                    || json_values(&record, "gid").any(is_key_group);
                is_key
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
        /// `None`: /etc/group is unreadable; `Some(None)`: it does not exist.
        group_file: Option<Option<String>>,
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
                group_file: Some(Some("root:x:0:\nherdr:x:990:\npaul:x:1000:\n".into())),
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
        fn group_file(&mut self) -> std::io::Result<Option<String>> {
            self.calls.push("file".into());
            self.group_file
                .clone()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EACCES))
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
            ["get", "file", "groups", "suid", "set 1000", "get", "nodump"]
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
        let accounts = AccountFiles {
            nsswitch: &paths[0],
            group: &paths[1],
            passwd: &paths[2],
            userdb: &userdb_dirs,
        };
        load_with(true, 990, &accounts, |path, gid| {
            assert_eq!(path, Path::new(KEY_PATH));
            Some(gid)
        })
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
            load_with(false, 990, &SYSTEM_ACCOUNTS, |_, _| -> Option<()> {
                panic!("must not read the key")
            }),
            None
        );
        // Unreadable files refuse.
        let missing = "/nonexistent/herdr-accounts";
        let accounts = AccountFiles {
            nsswitch: missing,
            group: missing,
            passwd: missing,
            userdb: &[],
        };
        assert_eq!(load_with(true, 990, &accounts, |_, _| Some(())), None);
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
        assert_eq!(fake.calls, ["get", "file", "groups"]);
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
        // gid reach every pane child, so no mode runs; the key is never touched.
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
                let refused = acquire_with(&args(argv), &mut fake, |_, _| -> Option<()> {
                    panic!("must not read the key")
                });
                assert_eq!(refused, Err(HERDR_GROUP_REFUSAL), "{ids:?} {groups:?}");
            }
        }
        // The setgid egid is the key group even when /etc/group does not name it herdr.
        let mut fake = Fake {
            ids: (1000, 977, 977),
            groups: Some(vec![1000, 977]),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        // getgroups failure or an unreadable /etc/group refuses.
        let mut fake = Fake {
            groups: None,
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
        let mut fake = Fake {
            group_file: None,
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Err(HERDR_GROUP_REFUSAL));
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
        // No /etc/group names no herdr group.
        let mut fake = Fake {
            ids: (1000, 1000, 1000),
            groups: Some(vec![1000, 990]),
            group_file: Some(None),
            ..Fake::default()
        };
        assert_eq!(run(&mut fake), Ok(None));
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
