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
    fn set_non_dumpable(&mut self);
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
    fn set_non_dumpable(&mut self) {
        // SAFETY: prctl with integer arguments only.
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
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
    // The key is read before the drop: afterwards the process can no longer open it.
    let key = if is_server_invocation(args) {
        load(setgid, effective)
    } else {
        None
    };
    if setgid && (!ops.setresgid(real) || ops.getresgid() != (real, real, real)) {
        // Never continue with a group the drop could not shed.
        eprintln!("herdr: could not drop the setgid group; refusing to run");
        std::process::exit(1);
    }
    if key.is_some() {
        // Re-assert after the credential change.
        ops.set_non_dumpable();
    }
    key
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
    if !setgid {
        return None;
    }
    read_key(Path::new(KEY_PATH), Some(effective_gid))
}

/// `group` is the required owning group in production (`None` only for the test seam, which
/// requires an owner-only file instead).
fn read_key(path: &Path, group: Option<u32>) -> Option<LockedKey> {
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
    let mut pem = Vec::with_capacity(KEY_LIMIT as usize);
    let read = file.by_ref().take(KEY_LIMIT + 1).read_to_end(&mut pem);
    let key = read.ok().and_then(|_| parse_pem(&pem));
    wipe(&mut pem);
    key
}

fn parse_pem(pem: &[u8]) -> Option<LockedKey> {
    use base64::Engine as _;
    let text = std::str::from_utf8(pem).ok()?;
    let body = text
        .trim()
        .strip_prefix("-----BEGIN PRIVATE KEY-----")?
        .strip_suffix("-----END PRIVATE KEY-----")?;
    let mut compact: Vec<u8> = body.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(&compact);
    wipe(&mut compact);
    let mut der = der.ok()?;
    let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der);
    wipe(&mut der);
    let boxed = Box::new(pair.ok()?);
    let size = std::mem::size_of::<Ed25519KeyPair>();
    // SAFETY: mlock pins the pages of this live allocation, which is never freed.
    if unsafe { libc::mlock((&*boxed as *const Ed25519KeyPair).cast(), size) } != 0 {
        return None;
    }
    Some(LockedKey(boxed))
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

    #[derive(Default)]
    struct Fake {
        ids: (u32, u32, u32),
        drop_ok: bool,
        calls: Vec<String>,
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
        fn set_non_dumpable(&mut self) {
            self.calls.push("nodump".into());
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
        assert_eq!(fake.calls, ["get", "set 1000", "get", "nodump"]);
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
