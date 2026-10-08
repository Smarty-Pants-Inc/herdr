//! Linux trust acquisition for connection principals. No environment evidence.

use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::platform::ProcessIdentity;
use crate::pty::input_consumer::Principal;
use crate::server::client_identity::{
    self, AcceptedPeer, ExecutableIdentity, JournalQuery, JournalRow, MapCache, MapImage, Process,
    Sources,
};

const MAP_PATH: &str = "/etc/herdr/principals.json";
const MAP_LIMIT: usize = 65_536;
const OUTPUT_LIMIT: usize = 262_144;
const COMMAND_TIMEOUT: Duration = Duration::from_millis(800);
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

fn micros_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_micros()
        .try_into()
        .ok()
}

struct LinuxSources {
    boot_id: String,
    boot_micros: u64,
    ticks: u64,
    server_exe: Metadata,
}

impl LinuxSources {
    fn new() -> Option<Self> {
        let boot_id = String::from_utf8(bounded_read("/proc/sys/kernel/random/boot_id", 128)?)
            .ok()?
            .trim()
            .replace('-', "");
        let stat = String::from_utf8(bounded_read("/proc/stat", 262_144)?).ok()?;
        let boot_seconds: u64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .parse()
            .ok()?;
        // SAFETY: sysconf has no pointer arguments.
        let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
        if ticks == 0 {
            return None;
        }
        Some(Self {
            boot_id,
            boot_micros: boot_seconds.checked_mul(1_000_000)?,
            ticks,
            server_exe: std::fs::metadata("/proc/self/exe").ok()?,
        })
    }

    fn monotonic_start(&self, start: u64) -> Option<u64> {
        start.checked_mul(1_000_000)?.checked_div(self.ticks)
    }
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
        // (EACCES). Such a root process is only `RootUnreadable`; the resolver
        // then requires journald's trusted `_EXE` for it (ruling r3 A).
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
            started_at: self
                .boot_micros
                .checked_add(self.monotonic_start(identity.start_time)?)?,
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

    fn journal(&mut self, query: JournalQuery) -> Option<Vec<JournalRow>> {
        let process = self.proc(query.pid)?;
        if process.uid != 0
            || !matches!(
                process.executable,
                ExecutableIdentity::Sshd | ExecutableIdentity::RootUnreadable
            )
            || query.uid != 0
            || query.comm != "sshd"
            || query.format != "json"
            || query.since != process.started_at
        {
            return None;
        }
        let args = vec![
            format!("_PID={}", query.pid),
            "_UID=0".into(),
            "_COMM=sshd".into(),
            "--since".into(),
            format!(
                "@{}.{:06}",
                query.since / 1_000_000,
                query.since % 1_000_000
            ),
            "-o".into(),
            "json".into(),
            "--no-pager".into(),
        ];
        let bytes = trusted_command(Path::new("/usr/bin/journalctl"), &args)?;
        let text = String::from_utf8(bytes).ok()?;
        let now = micros_now()?;
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes the valid output pointer on success.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: successful clock_gettime initialized the structure.
        let time = unsafe { time.assume_init() };
        let monotonic_now = u64::try_from(time.tv_sec)
            .ok()?
            .checked_mul(1_000_000)?
            .checked_add(u64::try_from(time.tv_nsec).ok()? / 1_000)?;
        let mut rows = Vec::new();
        for line in text.lines().filter(|s| !s.is_empty()) {
            rows.push(parse_journal_row(
                line,
                &query,
                &self.boot_id,
                self.monotonic_start(process.start)?,
                now,
                monotonic_now,
            )?);
        }
        if self.proc(query.pid)? != process {
            return None;
        }
        Some(rows)
    }

    fn whois(&mut self, ip: &str) -> Option<String> {
        if !client_identity::tailnet_address(ip.parse().ok()?) {
            return None;
        }
        String::from_utf8(trusted_command(
            Path::new("/usr/bin/tailscale"),
            &["whois".into(), "--json".into(), ip.into()],
        )?)
        .ok()
    }
}

fn parse_journal_row(
    line: &str,
    query: &JournalQuery,
    boot: &str,
    monotonic_start: u64,
    now: u64,
    monotonic_now: u64,
) -> Option<JournalRow> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let string = |key| value.get(key)?.as_str();
    let pid = string("_PID")?.parse().ok()?;
    let uid = string("_UID")?.parse().ok()?;
    let comm = string("_COMM")?;
    let timestamp = string("__REALTIME_TIMESTAMP")?.parse().ok()?;
    let monotonic: u64 = string("__MONOTONIC_TIMESTAMP")?.parse().ok()?;
    if pid != query.pid
        || uid != 0
        || comm != "sshd"
        || string("_BOOT_ID")? != boot
        || string("_EXE")? != "/usr/sbin/sshd"
        || string("_TRANSPORT")? != "syslog"
        || timestamp < query.since
        || timestamp > now
        || monotonic < monotonic_start
        || monotonic > monotonic_now
    {
        return None;
    }
    Some(JournalRow {
        pid,
        uid,
        comm: comm.into(),
        timestamp,
        message: string("MESSAGE")?.into(),
    })
}

/// Execute an already-open root-protected binary, with a clean environment, no
/// shell, bounded output and wall deadline. A hung/unavailable helper gives None.
fn trusted_command(path: &Path, args: &[String]) -> Option<Vec<u8>> {
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
        let deadline = Instant::now() + COMMAND_TIMEOUT;
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
    fn journal_trusted_metadata_not_environment_fields() {
        let query = JournalQuery {
            pid: 20,
            uid: 0,
            comm: "sshd".into(),
            since: 100,
            format: "json".into(),
        };
        let row = serde_json::json!({"_PID":"20", "_UID":"0", "_COMM":"sshd", "_EXE":"/usr/sbin/sshd",
            "_TRANSPORT":"syslog", "_BOOT_ID":"boot", "__REALTIME_TIMESTAMP":"101", "__MONOTONIC_TIMESTAMP":"80",
            "MESSAGE":"Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:paul"});
        assert!(parse_journal_row(&row.to_string(), &query, "boot", 80, 102, 81).is_some());
        for (key, bad) in [
            ("_PID", "21"),
            ("_UID", "1000"),
            ("_COMM", "bash"),
            ("_EXE", "/tmp/sshd"),
            ("_TRANSPORT", "stdout"),
            ("_BOOT_ID", "other"),
            ("__REALTIME_TIMESTAMP", "99"),
            ("__REALTIME_TIMESTAMP", "103"),
            ("__MONOTONIC_TIMESTAMP", "79"),
            ("__MONOTONIC_TIMESTAMP", "82"),
        ] {
            let mut candidate = row.clone();
            candidate[key] = bad.into();
            assert!(
                parse_journal_row(&candidate.to_string(), &query, "boot", 80, 102, 81).is_none(),
                "{key}"
            );
        }
        let mut candidate = row.clone();
        candidate.as_object_mut().unwrap().remove("_UID");
        candidate["UID"] = "0".into();
        assert!(parse_journal_row(&candidate.to_string(), &query, "boot", 80, 102, 81).is_none());
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
        assert!(process.started_at <= micros_now().unwrap());
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
                &["%s".into(), "literal;$(touch nope)".into()]
            ),
            Some(b"literal;$(touch nope)".to_vec())
        );
        assert!(trusted_command(Path::new("/usr/bin/sleep"), &["2".into()]).is_none());
        assert!(trusted_command(Path::new("/tmp/journalctl"), &[]).is_none());
        assert_eq!(
            trusted_command(Path::new("/usr/bin/env"), &[]),
            Some(b"LANG=C\nLC_ALL=C\n".to_vec())
        );
        assert!(trusted_command(
            Path::new("/usr/bin/head"),
            &[
                "-c".into(),
                (OUTPUT_LIMIT + 1).to_string(),
                "/dev/zero".into()
            ]
        )
        .is_none());
        assert!(trusted_command(Path::new("/usr/bin/false"), &[]).is_none());
    }
}
