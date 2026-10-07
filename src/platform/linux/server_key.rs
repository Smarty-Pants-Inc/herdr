//! Server attestation key for input-consumer answers (smarty-dev#2636, decision 3 B).
//!
//! The installed binary is `root:herdr 2755`; `/etc/herdr/server.key` is `0640 root:herdr`.
//! Only the server reads the key, first thing in `main`: before any config, environment-derived
//! path, plugin or hook. It holds the key in locked memory, then drops the saved and effective
//! gid back to the real gid, so no child ever runs with egid `herdr`. The process stays
//! non-dumpable, so same-uid processes cannot ptrace it or read its memory. Every other mode
//! (CLI, client, bridges) drops the gid without touching the key. Any failure leaves no key:
//! the capability is false and enroll is refused. There is no unsigned fallback.
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::OnceLock;

use ring::signature::Ed25519KeyPair;

const KEY_PATH: &str = "/etc/herdr/server.key";
const KEY_LIMIT: u64 = 4096;

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
}

/// Process entry point. Must run before anything reads configuration or the environment.
pub(crate) fn acquire(args: &[OsString]) {
    if let Some(key) = acquire_with(args, &mut Kernel, load) {
        let _ = KEY.set(key);
    }
}

fn acquire_with<K>(
    args: &[OsString],
    ops: &mut impl GidOps,
    load: impl FnOnce(bool, u32) -> Option<K>,
) -> Option<K> {
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
        eprintln!("herdr: could not drop the setgid group; refusing to run");
        std::process::exit(1);
    }
    // Re-assert after the credential change and verify it; otherwise discard the key
    // (fail closed: capability false, no enrollment).
    key.filter(|_| ops.make_non_dumpable())
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
    load_with(setgid, effective_gid, &mut Nss, read_key)
}

/// Who holds a group, so the loader can prove the key group is the server's alone.
pub(crate) trait GroupDb {
    /// Supplementary members of `gid`, or `None` when the group cannot be resolved.
    fn members(&mut self, gid: u32) -> Option<Vec<String>>;
    /// Whether any account has `gid` as its primary group (`None`: cannot tell).
    fn is_primary_of_any_user(&mut self, gid: u32) -> Option<bool>;
}

/// The key file is `0640 root:herdr`, so every member of `herdr` could read it and forge
/// signatures. Load only when the group has no supplementary members and is nobody's primary
/// group: then only the setgid binary holds it (review #188 P1). Unresolvable means refuse.
fn load_with<K>(
    setgid: bool,
    effective_gid: u32,
    groups: &mut impl GroupDb,
    read: impl FnOnce(&Path, Option<u32>) -> Option<K>,
) -> Option<K> {
    if !setgid
        || groups
            .members(effective_gid)
            .is_none_or(|members| !members.is_empty())
        || groups.is_primary_of_any_user(effective_gid) != Some(false)
    {
        return None;
    }
    read(Path::new(KEY_PATH), Some(effective_gid))
}

/// The system account databases through NSS (files, LDAP, sssd alike).
struct Nss;

impl GroupDb for Nss {
    fn members(&mut self, gid: u32) -> Option<Vec<String>> {
        let mut buf = vec![0 as libc::c_char; 1 << 16];
        let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: getgrgid_r fills `group` using `buf`, both valid for the given length.
        let rc = unsafe {
            libc::getgrgid_r(
                gid,
                group.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        if rc != 0 || found.is_null() {
            return None;
        }
        // SAFETY: on success `group` is initialized and gr_mem is a NULL-terminated array of
        // C strings inside `buf`, which outlives this loop.
        let mut cursor = unsafe { group.assume_init().gr_mem };
        let mut members = Vec::new();
        while !cursor.is_null() && !unsafe { *cursor }.is_null() {
            // SAFETY: each entry is a valid C string (see above).
            let name = unsafe { std::ffi::CStr::from_ptr(*cursor) };
            members.push(name.to_string_lossy().into_owned());
            // SAFETY: stays within the NULL-terminated array.
            cursor = unsafe { cursor.add(1) };
        }
        Some(members)
    }
    fn is_primary_of_any_user(&mut self, gid: u32) -> Option<bool> {
        // The startup thread is single; getpwent's static cursor is not shared yet.
        let mut found = false;
        // SAFETY: setpwent/getpwent/endpwent iterate the password database; each entry is
        // read before the next call.
        unsafe {
            libc::setpwent();
            loop {
                let entry = libc::getpwent();
                if entry.is_null() {
                    break;
                }
                if (*entry).pw_gid == gid {
                    found = true;
                    break;
                }
            }
            libc::endpwent();
        }
        Some(found)
    }
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
        calls: Vec<String>,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                ids: (0, 0, 0),
                drop_ok: true,
                suid_safe: true,
                nodump_ok: true,
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
            if self.drop_ok {
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
        assert_eq!(key, Some(()));
        assert_eq!(seen, Some((true, 990)));
        assert_eq!(fake.calls, ["get", "suid", "set 1000", "get", "nodump"]);
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
            assert_eq!(key, None);
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
        assert_eq!(key, None);
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
        assert_eq!(key, None);
        assert_eq!(fake.calls.last().map(String::as_str), Some("nodump"));
        // Counterpart: the real kernel path succeeds for this unprivileged test process.
        assert!(Kernel.make_non_dumpable());
        // SAFETY: restores the default so later tests in this process are unaffected.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    }

    struct Groups {
        members: Option<Vec<String>>,
        primary: Option<bool>,
    }
    impl GroupDb for Groups {
        fn members(&mut self, _: u32) -> Option<Vec<String>> {
            self.members.clone()
        }
        fn is_primary_of_any_user(&mut self, _: u32) -> Option<bool> {
            self.primary
        }
    }

    #[test]
    fn server_key_group_with_any_member_never_reads_the_key() {
        // Review #188 P1: a member of the key group could read the 0640 key and forge.
        for (members, primary) in [
            (Some(vec!["mallory".to_string()]), Some(false)),
            (Some(vec![]), Some(true)),
            (None, Some(false)),
            (Some(vec![]), None),
        ] {
            let mut groups = Groups { members, primary };
            let key = load_with(true, 990, &mut groups, |_, _| -> Option<()> {
                panic!("must not read the key")
            });
            assert_eq!(key, None);
        }
        // Counterpart: a private group (no members, nobody's primary) reads the real path.
        let mut groups = Groups {
            members: Some(vec![]),
            primary: Some(false),
        };
        let mut read = None;
        let key = load_with(true, 990, &mut groups, |path, gid| {
            read = Some((path.to_owned(), gid));
            Some(())
        });
        assert_eq!(key, Some(()));
        assert_eq!(read, Some((Path::new(KEY_PATH).to_owned(), Some(990))));
        // Not setgid: never reads.
        assert_eq!(load_with(false, 990, &mut groups, |_, _| Some(())), None);
    }

    #[test]
    fn server_key_real_nss_sees_this_users_primary_group() {
        // SAFETY: getgid takes no arguments and cannot fail.
        let gid = unsafe { libc::getgid() };
        assert_eq!(Nss.is_primary_of_any_user(gid), Some(true));
        assert!(Nss.members(gid).is_some());
        assert_eq!(Nss.members(u32::MAX - 7), None);
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
        assert_eq!(key, None);
        assert_eq!(fake.calls, ["get"]);
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
