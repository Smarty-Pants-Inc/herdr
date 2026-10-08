//! Linux trust acquisition for connection principals. No environment evidence.

use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::platform::ProcessIdentity;
use crate::pty::input_consumer::Principal;
use crate::server::client_identity::{
    self, AcceptedPeer, ExecutableIdentity, MapCache, MapImage, Process, Sources, SshdLogin,
};

const MAP_PATH: &str = "/etc/herdr/principals.json";
const MAP_LIMIT: usize = 65_536;
const OUTPUT_LIMIT: usize = 262_144;
const COMMAND_TIMEOUT: Duration = Duration::from_millis(800);
/// Installed `root:systemd-journal` mode 2755 (docs/next/sshd-lookup.md). The
/// server itself is not in the journal group.
const SSHD_LOOKUP: &str = "/usr/local/libexec/herdr-sshd-lookup";
/// Above the helper's own 1.2 s journal deadline.
const SSHD_LOOKUP_TIMEOUT: Duration = Duration::from_millis(2_000);
static MAP: OnceLock<Mutex<MapCache>> = OnceLock::new();

fn cache() -> &'static Mutex<MapCache> {
    MAP.get_or_init(|| Mutex::new(MapCache::default()))
}

pub(crate) fn initialize_client_principals() {
    if let Ok(mut cache) = cache().lock() {
        let image = read_map(Path::new(MAP_PATH));
        cache.refresh(image.as_ref());
    }
}

pub(crate) fn resolve_client_principal(peer: Option<ProcessIdentity>) -> Option<Principal> {
    // Check/reload even for an unlabelled connection; invalid candidates never
    // leave stale authority for later connects. External commands run unlocked.
    let map = {
        let mut cache = cache().lock().ok()?;
        // Serialize acquisition/publication so an older concurrent read cannot
        // overwrite a newer cached candidate. Bounded local reads only.
        let image = read_map(Path::new(MAP_PATH));
        cache.refresh(image.as_ref());
        cache.snapshot()
    };
    let peer = peer?;
    client_identity::resolve(
        AcceptedPeer {
            pid: peer.pid,
            start: peer.start_time,
            server_pid: std::process::id(),
            linux: true,
        },
        &map,
        &mut LinuxSources::new()?,
    )
}

fn protected(metadata: &Metadata) -> bool {
    metadata.uid() == 0 && metadata.mode() & 0o022 == 0
}

/// Open every component relative to its already-open protected parent. Refuse
/// symlinks, nonregular leaves, writable/nonroot directories and special files.
fn protected_open(path: &Path) -> Option<File> {
    if !path.is_absolute() {
        return None;
    }
    let mut directory = File::open("/").ok()?;
    if !protected(&directory.metadata().ok()?) {
        return None;
    }
    let parts: Vec<_> = path.components().collect();
    for (index, part) in parts.iter().enumerate().skip(1) {
        let Component::Normal(name) = part else {
            return None;
        };
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_bytes()).ok()?;
        let leaf = index + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if leaf { 0 } else { libc::O_DIRECTORY };
        // SAFETY: valid directory descriptor and NUL-terminated component.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return None;
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().ok()?;
        if !protected(&metadata)
            || if leaf {
                !metadata.is_file()
            } else {
                !metadata.is_dir()
            }
        {
            return None;
        }
        directory = file;
    }
    directory.metadata().ok()?.is_file().then_some(directory)
}

fn stamp(m: &Metadata) -> (u64, u64, u32, u32, u64, i64, i64, i64, i64) {
    (
        m.dev(),
        m.ino(),
        m.uid(),
        m.mode(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

fn read_map(path: &Path) -> Option<MapImage> {
    static INVALID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let result = (|| {
        let mut file = protected_open(path)?;
        let before = file.metadata().ok()?;
        if before.len() > MAP_LIMIT as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAP_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAP_LIMIT || stamp(&before) != stamp(&file.metadata().ok()?) {
            return None;
        }
        // Reopen the protected path after reading: replacement, symlink changes
        // or metadata changes while reading refuse this candidate entirely.
        if stamp(&before) != stamp(&protected_open(path)?.metadata().ok()?) {
            return None;
        }
        // Compare exact metadata, not a lossy hash: copied mtime, replacement
        // and chmod must not leave cached authority for a new connection.
        Some(MapImage {
            uid: before.uid(),
            mode: before.mode(),
            mtime: (u128::from(before.mtime() as u64) << 64)
                | u128::from(before.mtime_nsec() as u64),
            revision: Some((
                before.dev(),
                before.ino(),
                before.ctime(),
                before.ctime_nsec(),
                before.len(),
            )),
            json: String::from_utf8(bytes).ok()?,
        })
    })();
    if result.is_none() {
        // Once per transition into an unavailable/unsafe candidate, not every
        // local attach. A later successful candidate resets the diagnostic.
        if !INVALID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("principal map unavailable or unsafe; connections remain unmapped");
        }
    } else {
        INVALID.store(false, std::sync::atomic::Ordering::Relaxed);
        // Parser failures and duplicate factors are logged by the metadata-keyed cache.
    }
    result
}

fn bounded_read(path: impl AsRef<Path>, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

struct LinuxSources {
    server_exe: Metadata,
}

impl LinuxSources {
    fn new() -> Option<Self> {
        Some(Self {
            server_exe: std::fs::metadata("/proc/self/exe").ok()?,
        })
    }
}

/// Ask the setgid helper about one sshd priv pid. A missing, failing, hung or
/// malformed helper leaves the connection unmapped.
fn sshd_login_via(helper: &Path, pid: u32) -> Option<SshdLogin> {
    sshd_login_from(helper, &[pid.to_string()], pid)
}

fn sshd_login_from(helper: &Path, args: &[String], pid: u32) -> Option<SshdLogin> {
    let output = trusted_command(helper, args, SSHD_LOOKUP_TIMEOUT)?;
    client_identity::parse_sshd_login(&output).filter(|login| login.pid == pid)
}

impl Sources for LinuxSources {
    fn proc(&mut self, pid: u32) -> Option<Process> {
        let identity = super::process_identity(pid)?;
        let stat = String::from_utf8(bounded_read(format!("/proc/{pid}/stat"), 8192)?).ok()?;
        let rest = stat.get(stat.rfind(')')? + 2..)?;
        let fields: Vec<_> = rest.split_whitespace().collect();
        let parent = fields.get(1)?.parse().ok()?;
        if fields.get(19)?.parse::<u64>().ok()? != identity.start_time {
            return None;
        }
        let status =
            String::from_utf8(bounded_read(format!("/proc/{pid}/status"), 16_384)?).ok()?;
        let uids: Vec<u32> = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if uids.len() != 4 {
            return None;
        }
        let uid = uids[1];
        let all_root = uids.iter().all(|uid| *uid == 0);
        // An unprivileged server cannot read a root process's exe link
        // (EACCES). Such a root process is only `RootUnreadable`; the sshd
        // lookup helper then requires journald's trusted `_EXE` (ruling r3 A).
        let exe_metadata = std::fs::read_link(format!("/proc/{pid}/exe")).and_then(|path| {
            let metadata = File::open(format!("/proc/{pid}/exe"))?.metadata()?;
            Ok((path, metadata))
        });
        let (exe, executable) = match exe_metadata {
            Ok((path, metadata)) => (path.to_str()?.to_owned(), Some(metadata)),
            Err(e) if e.kind() == ErrorKind::PermissionDenied && all_root => (String::new(), None),
            Err(_) => return None,
        };
        let cmdline = bounded_read(format!("/proc/{pid}/cmdline"), 8192)?;
        let argv: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8(s.to_vec()))
            .collect::<Result<_, _>>()
            .ok()?;
        let title = argv.first()?.clone();
        let same_server = executable.as_ref().is_some_and(|executable| {
            (executable.dev(), executable.ino()) == (self.server_exe.dev(), self.server_exe.ino())
        });
        let genuine_sshd = (|| {
            let executable = executable.as_ref()?;
            if !all_root {
                return None;
            }
            let sshd = protected_open(Path::new("/usr/sbin/sshd"))?
                .metadata()
                .ok()?;
            Some((sshd.dev(), sshd.ino()) == (executable.dev(), executable.ino()))
        })()
        .unwrap_or(false);
        if super::process_identity(pid) != Some(identity) {
            return None;
        }
        Some(Process {
            parent,
            uid,
            start: identity.start_time,
            exe,
            argv,
            title,
            executable: if same_server {
                ExecutableIdentity::Server
            } else if genuine_sshd {
                ExecutableIdentity::Sshd
            } else if executable.is_none() {
                ExecutableIdentity::RootUnreadable
            } else {
                ExecutableIdentity::Other
            },
        })
    }

    fn sshd_login(&mut self, pid: u32) -> Option<SshdLogin> {
        sshd_login_via(Path::new(SSHD_LOOKUP), pid)
    }

    fn whois(&mut self, ip: &str) -> Option<String> {
        if !client_identity::tailnet_address(ip.parse().ok()?) {
            return None;
        }
        String::from_utf8(trusted_command(
            Path::new("/usr/bin/tailscale"),
            &["whois".into(), "--json".into(), ip.into()],
            COMMAND_TIMEOUT,
        )?)
        .ok()
    }
}

/// Execute an already-open root-protected binary, with a clean environment, no
/// shell, bounded output and wall deadline. A hung/unavailable helper gives None.
fn trusted_command(path: &Path, args: &[String], timeout: Duration) -> Option<Vec<u8>> {
    let executable = protected_open(path)?;
    let mut child = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
        .args(args)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let outcome = (|| {
        let mut stdout = child.stdout.take()?;
        // SAFETY: stdout is a live pipe descriptor; preserving existing flags.
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return None;
        }
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut eof = false;
        loop {
            // Check the deadline even while a helper continuously fills stdout.
            if Instant::now() >= deadline {
                return None;
            }
            let mut buffer = [0; 4096];
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if output.len() + n > OUTPUT_LIMIT {
                        return None;
                    }
                    output.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
            if let Some(status) = child.try_wait().ok()? {
                if !status.success() {
                    return None;
                }
                if eof {
                    return Some(output);
                }
            }
            if eof {
                std::thread::sleep(Duration::from_millis(5));
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    })();
    if outcome.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_path_refuses_when_the_sshd_lookup_helper_is_missing_or_fails() {
        let line =
            r#"{"pid":20,"user":"paul","fingerprint":"SHA256:paul","source_ip":"100.64.0.7"}"#;
        let printf = |text: &str| sshd_login_via_printf(text, 20);
        // The installed path is absent on test hosts: the production source refuses.
        if !Path::new(SSHD_LOOKUP).exists() {
            assert_eq!(LinuxSources::new().unwrap().sshd_login(20), None);
        }
        assert_eq!(
            sshd_login_via(
                Path::new("/usr/local/libexec/herdr-sshd-lookup-missing"),
                20
            ),
            None
        );
        // Untrusted location, failure exit, hang, and the empty fail-closed answer.
        assert_eq!(
            sshd_login_via(Path::new("/tmp/herdr-sshd-lookup"), 20),
            None
        );
        assert_eq!(sshd_login_via(Path::new("/usr/bin/false"), 20), None);
        assert_eq!(sshd_login_via(Path::new("/usr/bin/true"), 20), None);
        // A well-formed answer is used only for the pid that was asked about.
        assert_eq!(
            printf(&format!("{line}\n")).map(|login| login.fingerprint),
            Some("SHA256:paul".into())
        );
        assert_eq!(sshd_login_via_printf(&format!("{line}\n"), 21), None);
        assert_eq!(printf(&format!("{line}\n{line}\n")), None);
        assert_eq!(printf(line), None);
        assert_eq!(printf("garbage\n"), None);
    }

    /// `/usr/bin/printf` stands in for a helper that printed `text`.
    fn sshd_login_via_printf(text: &str, pid: u32) -> Option<SshdLogin> {
        sshd_login_from(
            Path::new("/usr/bin/printf"),
            &["%s".into(), text.into()],
            pid,
        )
    }

    #[test]
    fn production_entrypoint_unavailable_or_local_peer_is_unmapped() {
        initialize_client_principals();
        assert_eq!(resolve_client_principal(None), None);
        assert_eq!(
            resolve_client_principal(super::super::process_identity(std::process::id())),
            None
        );
    }

    #[test]
    fn real_proc_snapshot_preserves_pinned_generation_and_executable() {
        let mut sources = LinuxSources::new().expect("Linux procfs");
        let process = sources.proc(std::process::id()).expect("self snapshot");
        assert_eq!(process.executable, ExecutableIdentity::Server);
        assert_eq!(
            process.start,
            super::super::process_identity(std::process::id())
                .unwrap()
                .start_time
        );
        assert!(sources.proc(u32::MAX).is_none());
    }

    #[test]
    fn real_unreadable_root_exe_is_root_unreadable_not_refused_or_sshd() {
        // pid 1 is root; an unprivileged test process cannot read its exe link.
        let denied = std::fs::read_link("/proc/1/exe")
            .is_err_and(|e| e.kind() == ErrorKind::PermissionDenied);
        let mut sources = LinuxSources::new().expect("Linux procfs");
        let process = sources.proc(1).expect("pid 1 snapshot");
        assert_eq!(process.uid, 0);
        if denied {
            assert_eq!(process.executable, ExecutableIdentity::RootUnreadable);
            assert!(process.exe.is_empty());
        } else {
            assert_ne!(process.executable, ExecutableIdentity::RootUnreadable);
        }
    }

    #[test]
    fn safe_map_reader_refuses_unprotected_parent_symlink_and_special_file() {
        // /tmp is deliberately writable even if a leaf happens to be root-owned.
        assert!(read_map(Path::new("/tmp/principals.json")).is_none());
        assert!(protected_open(Path::new("/etc/../etc/passwd")).is_none());
        assert!(protected_open(Path::new("/dev/null")).is_none());
        assert!(protected_open(Path::new("/proc/self/exe")).is_none());
        assert!(protected_open(Path::new("/etc/passwd")).is_some());
    }

    #[test]
    fn trusted_command_uses_open_descriptor_and_fixed_arguments() {
        assert_eq!(
            trusted_command(
                Path::new("/usr/bin/printf"),
                &["%s".into(), "literal;$(touch nope)".into()],
                COMMAND_TIMEOUT
            ),
            Some(b"literal;$(touch nope)".to_vec())
        );
        assert!(
            trusted_command(Path::new("/usr/bin/sleep"), &["2".into()], COMMAND_TIMEOUT).is_none()
        );
        assert!(trusted_command(Path::new("/tmp/journalctl"), &[], COMMAND_TIMEOUT).is_none());
        assert_eq!(
            trusted_command(Path::new("/usr/bin/env"), &[], COMMAND_TIMEOUT),
            Some(b"LANG=C\nLC_ALL=C\n".to_vec())
        );
        assert!(trusted_command(
            Path::new("/usr/bin/head"),
            &[
                "-c".into(),
                (OUTPUT_LIMIT + 1).to_string(),
                "/dev/zero".into()
            ],
            COMMAND_TIMEOUT
        )
        .is_none());
        assert!(trusted_command(Path::new("/usr/bin/false"), &[], COMMAND_TIMEOUT).is_none());
    }
}
