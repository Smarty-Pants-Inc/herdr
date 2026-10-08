//! Checks and journal parsing. Every fact comes from /proc and from journald's
//! trusted fields, read by this process. No environment or configuration input.

use std::ffi::OsString;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::IpAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SSHD: &str = "/usr/sbin/sshd";
const JOURNALCTL: &str = "/usr/bin/journalctl";
/// sshd logs the acceptance within LoginGraceTime (default 120 s) of the fork.
const LOGIN_WINDOW_SECONDS: u64 = 600;
/// One login writes a few records. A full page may be truncated: refuse it.
const ROW_LIMIT: usize = 64;
const READ_LIMIT: usize = 16_384;
const OUTPUT_LIMIT: usize = 262_144;
/// Below the server's helper deadline, so the server never waits on a stuck journal.
const JOURNAL_TIMEOUT: Duration = Duration::from_millis(1_200);
/// Only root (systemd) places a process in these cgroups.
const LISTENER_CGROUPS: [&str; 2] = [
    "0::/system.slice/ssh.service",
    "0::/system.slice/sshd.service",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Exe {
    /// `/proc/<pid>/exe` is the inode of `/usr/sbin/sshd`.
    Sshd,
    /// EACCES: a root process, unreadable without root. Other evidence decides.
    Unreadable,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    parent: u32,
    start: u64,
    comm: String,
    uids: [u32; 4],
    cmdline: Vec<u8>,
    /// Audit login uid, set once by pam_loginuid; None when unset.
    loginuid: Option<u32>,
    cgroup: String,
    exe: Exe,
}

pub(crate) struct Clock {
    /// 32 hex digits, as journald writes `_BOOT_ID`.
    boot_id: String,
    boot_micros: u64,
    ticks: u64,
    now: u64,
    monotonic_now: u64,
}

pub(crate) trait Host {
    /// Bounded read of `/proc/<pid>/<leaf>`.
    fn read(&mut self, pid: u32, leaf: &str) -> Option<Vec<u8>>;
    /// None when the process is gone.
    fn exe(&mut self, pid: u32) -> Option<Exe>;
    /// The process that started this helper (`getppid`).
    fn caller(&mut self) -> u32;
    /// This helper's real uid, inherited from the caller.
    fn uid(&mut self) -> u32;
    fn clock(&mut self) -> Option<Clock>;
    /// Fixed `journalctl` arguments; None on failure, timeout or excess output.
    fn journal(&mut self, args: &[String]) -> Option<Vec<u8>>;
}

#[derive(Debug, PartialEq)]
pub(crate) struct Login {
    pub pid: u32,
    pub user: String,
    pub fingerprint: String,
    pub source_ip: IpAddr,
}

impl Login {
    /// User and fingerprint are restricted to characters JSON does not escape.
    pub(crate) fn line(&self) -> String {
        format!(
            "{{\"pid\":{},\"user\":\"{}\",\"fingerprint\":\"{}\",\"source_ip\":\"{}\"}}\n",
            self.pid, self.user, self.fingerprint, self.source_ip
        )
    }
}

/// Exactly one canonical decimal pid above 1.
pub(crate) fn parse_pid(args: &[OsString]) -> Option<u32> {
    let [arg] = args else {
        return None;
    };
    let arg = arg.to_str()?;
    if arg.is_empty()
        || arg.len() > 10
        || arg.starts_with('0')
        || !arg.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let pid: u32 = arg.parse().ok()?;
    (pid > 1 && pid <= i32::MAX as u32).then_some(pid)
}

fn snapshot(host: &mut impl Host, pid: u32) -> Option<Snapshot> {
    let stat = String::from_utf8(host.read(pid, "stat")?).ok()?;
    let (head, _) = stat.split_once(" (")?;
    let close = stat.rfind(") ")?;
    if head != pid.to_string() {
        return None;
    }
    let comm = stat.get(head.len() + 2..close)?.to_owned();
    let fields: Vec<_> = stat.get(close + 2..)?.split_whitespace().collect();
    let parent = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    let status = String::from_utf8(host.read(pid, "status")?).ok()?;
    let uids: Vec<u32> = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let loginuid = host
        .read(pid, "loginuid")
        .and_then(|bytes| String::from_utf8(bytes).ok()?.trim().parse::<u32>().ok())
        .filter(|uid| *uid != u32::MAX);
    Some(Snapshot {
        parent,
        start,
        comm,
        uids: uids.try_into().ok()?,
        cmdline: host.read(pid, "cmdline")?,
        loginuid,
        cgroup: String::from_utf8(host.read(pid, "cgroup")?).ok()?,
        exe: host.exe(pid)?,
    })
}

fn valid_user(user: &str) -> bool {
    (1..=32).contains(&user.len())
        && !user.starts_with('-')
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The title must be exactly `sshd: <user> [priv]`; sshd pads it with NULs.
fn priv_user(cmdline: &[u8]) -> Option<&str> {
    let end = cmdline.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    let user = std::str::from_utf8(&cmdline[..end])
        .ok()?
        .strip_prefix("sshd: ")?
        .strip_suffix(" [priv]")?;
    valid_user(user).then_some(user)
}

fn journal_args(pid: u32, since: u64) -> Vec<String> {
    vec![
        "--no-pager".into(),
        "--quiet".into(),
        "--boot".into(),
        "--output=json".into(),
        "--output-fields=MESSAGE,_PID,_UID,_COMM,_EXE,_TRANSPORT".into(),
        format!("--lines={ROW_LIMIT}"),
        format!("--since=@{since}"),
        format!("--until=@{}", since + LOGIN_WINDOW_SECONDS),
        format!("_PID={pid}"),
        "_COMM=sshd".into(),
        "_UID=0".into(),
    ]
}

/// Parse `Accepted publickey for <user> from <ip> port <p> ssh2: <type> SHA256:<fp>`.
fn accepted_key(message: &str, user: &str) -> Option<(String, IpAddr)> {
    let fields: Vec<_> = message.split(' ').collect();
    if fields.len() != 11
        || fields[0..3] != ["Accepted", "publickey", "for"]
        || fields[3] != user
        || fields[4] != "from"
        || fields[6] != "port"
        || fields[8] != "ssh2:"
        || fields[7].parse::<u16>().ok().is_none_or(|p| p == 0)
        || fields[7].starts_with(['0', '+'])
        || fields[9].is_empty()
        || !fields[9]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    let digest = fields[10].strip_prefix("SHA256:")?;
    if digest.is_empty()
        || digest.len() > 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
    {
        return None;
    }
    Some((fields[10].into(), fields[5].parse().ok()?))
}

/// Every row must carry this login's trusted metadata; exactly one acceptance.
fn accepted_record(
    output: &[u8],
    pid: u32,
    user: &str,
    clock: &Clock,
    start_monotonic: u64,
    start_realtime: u64,
) -> Option<(String, IpAddr)> {
    let lines: Vec<_> = std::str::from_utf8(output)
        .ok()?
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() >= ROW_LIMIT {
        return None;
    }
    let pid = pid.to_string();
    let mut found = None;
    for line in lines {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        let field = |key| row.get(key)?.as_str();
        let realtime: u64 = field("__REALTIME_TIMESTAMP")?.parse().ok()?;
        let monotonic: u64 = field("__MONOTONIC_TIMESTAMP")?.parse().ok()?;
        if field("_PID")? != pid
            || field("_UID")? != "0"
            || field("_COMM")? != "sshd"
            || field("_EXE")? != SSHD
            || field("_TRANSPORT")? != "syslog"
            || field("_BOOT_ID")? != clock.boot_id
            || realtime < start_realtime
            || realtime > clock.now
            || monotonic < start_monotonic
            || monotonic > clock.monotonic_now
        {
            return None;
        }
        // A binary (non-string) MESSAGE refuses the whole login.
        let message = field("MESSAGE")?;
        if message.starts_with("Accepted ") {
            if found.is_some() {
                return None;
            }
            found = Some(accepted_key(message, user)?);
        }
    }
    found
}

pub(crate) fn lookup(host: &mut impl Host, pid: u32) -> Option<Login> {
    let uid = host.uid();
    let caller_pid = host.caller();
    if caller_pid <= 1 || caller_pid == pid {
        return None;
    }
    let caller = snapshot(host, caller_pid)?;
    let login = snapshot(host, pid)?;
    let user = priv_user(&login.cmdline)?.to_owned();
    let listener_pid = login.parent;
    // The caller must run as the user this login authenticated: another
    // user's process cannot learn about it.
    if caller.uids != [uid; 4]
        || login.loginuid != Some(uid)
        || login.comm != "sshd"
        || login.uids != [0; 4]
        || login.exe == Exe::Other
        || listener_pid <= 1
        || listener_pid == caller_pid
    {
        return None;
    }
    let listener = snapshot(host, listener_pid)?;
    if listener.parent != 1
        || listener.comm != "sshd"
        || listener.uids != [0; 4]
        || listener.exe == Exe::Other
        || !LISTENER_CGROUPS.contains(&listener.cgroup.trim_end())
        || listener.start > login.start
    {
        return None;
    }
    let clock = host.clock()?;
    let start_monotonic = login
        .start
        .checked_mul(1_000_000)?
        .checked_div(clock.ticks)?;
    let start_realtime = clock.boot_micros.checked_add(start_monotonic)?;
    let output = host.journal(&journal_args(pid, start_realtime / 1_000_000))?;
    let (fingerprint, source_ip) =
        accepted_record(&output, pid, &user, &clock, start_monotonic, start_realtime)?;
    // Every fact was read by number. A changed process refuses the answer.
    if host.caller() != caller_pid
        || snapshot(host, caller_pid)? != caller
        || snapshot(host, pid)? != login
        || snapshot(host, listener_pid)? != listener
    {
        return None;
    }
    Some(Login {
        pid,
        user,
        fingerprint,
        source_ip,
    })
}

fn bounded_read(path: &str, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

/// The installed helper: real /proc, real journal, and the group switch.
struct System {
    caller_gid: libc::gid_t,
    journal_gid: libc::gid_t,
}

fn set_gids(real: libc::gid_t, effective: libc::gid_t, saved: libc::gid_t) -> bool {
    // SAFETY: setresgid has no pointer arguments.
    unsafe { libc::setresgid(real, effective, saved) == 0 }
}

impl Host for System {
    fn read(&mut self, pid: u32, leaf: &str) -> Option<Vec<u8>> {
        bounded_read(&format!("/proc/{pid}/{leaf}"), READ_LIMIT)
    }

    fn exe(&mut self, pid: u32) -> Option<Exe> {
        match File::open(format!("/proc/{pid}/exe")) {
            Ok(file) => {
                let exe = file.metadata().ok()?;
                let sshd = std::fs::metadata(SSHD).ok()?;
                Some(if (exe.dev(), exe.ino()) == (sshd.dev(), sshd.ino()) {
                    Exe::Sshd
                } else {
                    Exe::Other
                })
            }
            Err(e) if e.kind() == ErrorKind::PermissionDenied => Some(Exe::Unreadable),
            Err(_) => None,
        }
    }

    fn caller(&mut self) -> u32 {
        // SAFETY: getppid cannot fail.
        unsafe { libc::getppid() as u32 }
    }

    fn uid(&mut self) -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn clock(&mut self) -> Option<Clock> {
        let boot_id = String::from_utf8(bounded_read("/proc/sys/kernel/random/boot_id", 128)?)
            .ok()?
            .trim()
            .replace('-', "");
        if boot_id.len() != 32 || !boot_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let stat = String::from_utf8(bounded_read("/proc/stat", 262_144)?).ok()?;
        let boot_seconds: u64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .parse()
            .ok()?;
        // SAFETY: sysconf has no pointer arguments.
        let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes the valid output pointer on success.
        if ticks == 0
            || unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0
        {
            return None;
        }
        // SAFETY: successful clock_gettime initialized the structure.
        let time = unsafe { time.assume_init() };
        Some(Clock {
            boot_id,
            boot_micros: boot_seconds.checked_mul(1_000_000)?,
            ticks,
            now: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_micros()
                .try_into()
                .ok()?,
            monotonic_now: u64::try_from(time.tv_sec)
                .ok()?
                .checked_mul(1_000_000)?
                .checked_add(u64::try_from(time.tv_nsec).ok()? / 1_000)?,
        })
    }

    fn journal(&mut self, args: &[String]) -> Option<Vec<u8>> {
        // The journal group is effective only while journalctl is spawned.
        if !set_gids(libc::gid_t::MAX, self.journal_gid, libc::gid_t::MAX) {
            return None;
        }
        let spawned = Command::new(JOURNALCTL)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        // Drop the group for good; the child already holds what it needs.
        let dropped = set_gids(self.caller_gid, self.caller_gid, self.caller_gid);
        let mut child = spawned.ok()?;
        let output = dropped.then(|| bounded_output(&mut child)).flatten();
        if output.is_none() {
            let _ = child.kill();
        }
        let status = child.wait().ok()?;
        output.filter(|_| status.success())
    }
}

/// Read stdout to EOF within the deadline and the output limit.
fn bounded_output(child: &mut std::process::Child) -> Option<Vec<u8>> {
    let mut stdout = child.stdout.take()?;
    // SAFETY: stdout is a live pipe descriptor; existing flags are preserved.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0
        || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return None;
    }
    let deadline = Instant::now() + JOURNAL_TIMEOUT;
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    while Instant::now() < deadline {
        match stdout.read(&mut buffer) {
            Ok(0) => return Some(output),
            Ok(n) if output.len() + n > OUTPUT_LIMIT => return None,
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    None
}

/// Entry point for the installed binary.
pub(crate) fn run() -> Option<String> {
    // Inherited descriptors must not reach journalctl.
    // SAFETY: close_range has no pointer arguments.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } != 0 {
        return None;
    }
    // SAFETY: integer-only prctl.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
        return None;
    }
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    // SAFETY: three valid output pointers.
    if unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) } != 0 {
        return None;
    }
    // The /proc checks need no group: keep the journal group only as saved id.
    if !set_gids(libc::gid_t::MAX, real, libc::gid_t::MAX) {
        return None;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut host = System {
        caller_gid: real,
        journal_gid: effective,
    };
    let login = parse_pid(&args).and_then(|pid| lookup(&mut host, pid));
    if !set_gids(real, real, real) {
        return None;
    }
    login.map(|login| login.line())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const ACCEPTED: &str =
        "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:paulKey+/=";
    const BOOT: &str = "0123456789abcdef0123456789abcdef";
    const CALLER: u32 = 900;
    const PRIV: u32 = 20;
    const LISTENER: u32 = 10;
    /// 500 s after boot at 100 ticks per second.
    const PRIV_START: u64 = 50_000;
    const BOOT_MICROS: u64 = 1_000_000_000;
    const START_MONO: u64 = 500_000_000;
    const START_REAL: u64 = BOOT_MICROS + START_MONO;

    #[derive(Clone)]
    struct Fixture {
        files: BTreeMap<(u32, &'static str), Vec<u8>>,
        exes: BTreeMap<u32, Exe>,
        caller: u32,
        uid: u32,
        journal: Option<Vec<u8>>,
        journal_calls: Vec<Vec<String>>,
        /// Applied when the journal is read: models pid reuse mid-lookup.
        after_journal: Vec<((u32, &'static str), Vec<u8>)>,
    }

    impl Host for Fixture {
        fn read(&mut self, pid: u32, leaf: &str) -> Option<Vec<u8>> {
            self.files
                .iter()
                .find(|((p, l), _)| *p == pid && *l == leaf)
                .map(|(_, v)| v.clone())
        }
        fn exe(&mut self, pid: u32) -> Option<Exe> {
            self.exes.get(&pid).copied()
        }
        fn caller(&mut self) -> u32 {
            self.caller
        }
        fn uid(&mut self) -> u32 {
            self.uid
        }
        fn clock(&mut self) -> Option<Clock> {
            Some(Clock {
                boot_id: BOOT.into(),
                boot_micros: BOOT_MICROS,
                ticks: 100,
                now: START_REAL + 3_600_000_000,
                monotonic_now: START_MONO + 3_600_000_000,
            })
        }
        fn journal(&mut self, args: &[String]) -> Option<Vec<u8>> {
            self.journal_calls.push(args.to_vec());
            for (key, value) in std::mem::take(&mut self.after_journal) {
                self.files.insert(key, value);
            }
            self.journal.clone()
        }
    }

    fn stat(pid: u32, comm: &str, parent: u32, start: u64) -> Vec<u8> {
        format!(
            "{pid} ({comm}) S {parent} {} {start} 0 0\n",
            ["0"; 17].join(" ")
        )
        .into_bytes()
    }

    fn status(uid: u32) -> Vec<u8> {
        format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t0\t0\t0\t0\n").into_bytes()
    }

    fn row(message: &str) -> serde_json::Value {
        serde_json::json!({"_PID":"20", "_UID":"0", "_COMM":"sshd", "_EXE":"/usr/sbin/sshd",
            "_TRANSPORT":"syslog", "_BOOT_ID":BOOT, "__REALTIME_TIMESTAMP":(START_REAL + 1).to_string(),
            "__MONOTONIC_TIMESTAMP":(START_MONO + 1).to_string(), "__CURSOR":"s=1",
            "MESSAGE":message})
    }

    fn journal(rows: &[serde_json::Value]) -> Option<Vec<u8>> {
        Some(
            rows.iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>()
                .into_bytes(),
        )
    }

    fn fixture() -> Fixture {
        let mut files = BTreeMap::new();
        let mut add =
            |pid, comm: &str, parent, start, uid, cmdline: &[u8], login: &str, cg: &str| {
                files.insert((pid, "stat"), stat(pid, comm, parent, start));
                files.insert((pid, "status"), status(uid));
                files.insert((pid, "cmdline"), cmdline.to_vec());
                files.insert((pid, "loginuid"), login.as_bytes().to_vec());
                files.insert((pid, "cgroup"), cg.as_bytes().to_vec());
            };
        add(
            CALLER,
            "herdr",
            700,
            60_000,
            1000,
            b"herdr\0server\0",
            "1000",
            "0::/user.slice/user-1000.slice/session-4.scope\n",
        );
        add(
            PRIV,
            "sshd",
            LISTENER,
            PRIV_START,
            0,
            b"sshd: paul [priv]\0\0\0\0",
            "1000",
            "0::/user.slice/user-1000.slice/session-7.scope\n",
        );
        add(
            LISTENER,
            "sshd",
            1,
            100,
            0,
            b"sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups\0",
            "4294967295",
            "0::/system.slice/ssh.service\n",
        );
        Fixture {
            files,
            exes: [
                (CALLER, Exe::Other),
                (PRIV, Exe::Unreadable),
                (LISTENER, Exe::Unreadable),
            ]
            .into(),
            caller: CALLER,
            uid: 1000,
            journal: journal(&[
                row("Connection from 100.64.0.7 port 1234 on 100.64.0.1 port 22"),
                row(ACCEPTED),
                row(
                    "pam_unix(sshd:session): session opened for user paul(uid=1000) by paul(uid=0)",
                ),
            ]),
            journal_calls: Vec::new(),
            after_journal: Vec::new(),
        }
    }

    fn paul() -> Login {
        Login {
            pid: PRIV,
            user: "paul".into(),
            fingerprint: "SHA256:paulKey+/=".into(),
            source_ip: "100.64.0.7".parse().unwrap(),
        }
    }

    #[test]
    fn good_login_prints_one_line_from_one_bounded_query() {
        let mut f = fixture();
        let login = lookup(&mut f, PRIV).expect("login");
        assert_eq!(login, paul());
        assert_eq!(
            login.line(),
            "{\"pid\":20,\"user\":\"paul\",\"fingerprint\":\"SHA256:paulKey+/=\",\"source_ip\":\"100.64.0.7\"}\n"
        );
        let parsed: serde_json::Value = serde_json::from_str(&login.line()).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 4);
        assert_eq!(
            f.journal_calls,
            vec![journal_args(PRIV, START_REAL / 1_000_000)]
        );
        let args = &f.journal_calls[0];
        for expected in [
            "_PID=20",
            "_COMM=sshd",
            "--lines=64",
            "--since=@1500",
            "--until=@2100",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected}");
        }
    }

    #[test]
    fn readable_sshd_exe_is_accepted_like_the_journal_proof() {
        let mut f = fixture();
        f.exes.insert(PRIV, Exe::Sshd);
        f.exes.insert(LISTENER, Exe::Sshd);
        assert_eq!(lookup(&mut f, PRIV), Some(paul()));
    }

    fn refused(change: impl FnOnce(&mut Fixture)) {
        let mut f = fixture();
        change(&mut f);
        assert_eq!(lookup(&mut f, PRIV), None);
    }

    #[test]
    fn record_parser_refuses_wrong_user_two_matches_and_malformed() {
        // Wrong user: the acceptance names another account than the title.
        refused(|f| f.journal = journal(&[row(&ACCEPTED.replace("for paul", "for kate"))]));
        // Two matches, also when the second is another method.
        refused(|f| f.journal = journal(&[row(ACCEPTED), row(ACCEPTED)]));
        refused(|f| {
            f.journal = journal(&[
                row(ACCEPTED),
                row("Accepted password for paul from 100.64.0.7 port 1234 ssh2"),
            ])
        });
        // No match.
        refused(|f| f.journal = journal(&[row("Connection closed")]));
        refused(|f| f.journal = Some(Vec::new()));
        for message in [
            "Accepted password for paul from 100.64.0.7 port 1234 ssh2",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 MD5:aa",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:a\"b",
            "Accepted publickey for paul from 100.64.0.7 port 0 ssh2: ED25519 SHA256:k",
            "Accepted publickey for paul from 100.64.0.7 port 01234 ssh2: ED25519 SHA256:k",
            "Accepted publickey for paul from host.example port 1234 ssh2: ED25519 SHA256:k",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:k extra",
            "Accepted publickey for paul  from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:k",
            "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519-CERT SHA256:k ID x (serial 1) CA ED25519 SHA256:ca",
        ] {
            refused(|f| f.journal = journal(&[row(message)]));
        }
        // Not JSON, binary MESSAGE, or a page that may be truncated.
        refused(|f| f.journal = Some(b"{\n".to_vec()));
        refused(|f| {
            let mut binary = row(ACCEPTED);
            binary["MESSAGE"] = serde_json::json!([65, 66]);
            f.journal = journal(&[row(ACCEPTED), binary]);
        });
        refused(|f| {
            let mut rows = vec![row(ACCEPTED)];
            rows.resize(ROW_LIMIT, row("noise"));
            f.journal = journal(&rows);
        });
    }

    #[test]
    fn record_parser_requires_trusted_metadata_of_this_login() {
        let real = |d: i64| ((START_REAL as i64) + d).to_string();
        let mono = |d: i64| ((START_MONO as i64) + d).to_string();
        for (key, bad) in [
            ("_PID", "21".to_string()),
            ("_UID", "1000".into()),
            ("_COMM", "bash".into()),
            ("_EXE", "/tmp/sshd".into()),
            ("_TRANSPORT", "stdout".into()),
            ("_BOOT_ID", "ffffffffffffffffffffffffffffffff".into()),
            ("__REALTIME_TIMESTAMP", real(-1)),
            ("__REALTIME_TIMESTAMP", real(3_600_000_001)),
            ("__MONOTONIC_TIMESTAMP", mono(-1)),
            ("__MONOTONIC_TIMESTAMP", mono(3_600_000_001)),
        ] {
            let mut candidate = row(ACCEPTED);
            candidate[key] = bad.clone().into();
            refused(|f| f.journal = journal(&[candidate]));
            // An unrelated row with bad metadata also refuses the login.
            let mut noise = row("noise");
            noise[key] = bad.into();
            refused(|f| f.journal = journal(&[row(ACCEPTED), noise]));
        }
        let mut missing = row(ACCEPTED);
        missing.as_object_mut().unwrap().remove("_UID");
        missing["UID"] = "0".into();
        refused(|f| f.journal = journal(&[missing]));
    }

    #[test]
    fn pid_checks_refuse_dead_spoofed_or_foreign_logins() {
        // Not alive.
        refused(|f| f.files.retain(|(pid, _), _| *pid != PRIV));
        refused(|f| {
            f.exes.remove(&PRIV);
        });
        // Title is not exactly `sshd: <user> [priv]`.
        for title in [
            &b"sshd: paul@notty\0"[..],
            b"sshd: paul [priv] \0",
            b"sshd: paul [priv]\0x\0",
            b"sshd:  paul [priv]\0",
            b"sshd: pa ul [priv]\0",
            b"sshd: -paul [priv]\0",
            b"sshd:  [priv]\0",
            b"/usr/sbin/sshd: paul [priv]\0",
        ] {
            refused(|f| {
                f.files.insert((PRIV, "cmdline"), title.to_vec());
            });
        }
        // Priv process is not root sshd.
        refused(|f| {
            f.files.insert((PRIV, "status"), status(1000));
        });
        refused(|f| {
            f.exes.insert(PRIV, Exe::Other);
        });
        refused(|f| {
            f.files
                .insert((PRIV, "stat"), stat(PRIV, "bash", LISTENER, PRIV_START));
        });
        // Parent is not the system sshd listener.
        refused(|f| {
            f.files.insert((LISTENER, "status"), status(1000));
        });
        refused(|f| {
            f.exes.insert(LISTENER, Exe::Other);
        });
        refused(|f| {
            f.files.insert(
                (LISTENER, "cgroup"),
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/ssh.service\n"
                    .to_vec(),
            );
        });
        refused(|f| {
            f.files
                .insert((LISTENER, "stat"), stat(LISTENER, "sshd", 600, 100));
        });
        refused(|f| {
            f.files.insert(
                (LISTENER, "stat"),
                stat(LISTENER, "sshd", 1, PRIV_START + 1),
            );
        });
        // Orphaned priv (listener restarted): parent is init.
        refused(|f| {
            f.files
                .insert((PRIV, "stat"), stat(PRIV, "sshd", 1, PRIV_START));
        });
        // Another user's login, an unset login uid, or a caller not running
        // wholly as this helper's real uid.
        refused(|f| {
            f.files.insert((PRIV, "loginuid"), b"1001".to_vec());
        });
        refused(|f| {
            f.files.insert((PRIV, "loginuid"), b"4294967295".to_vec());
        });
        refused(|f| f.uid = 1001);
        refused(|f| {
            f.files
                .insert((CALLER, "status"), b"Uid:\t1000\t0\t1000\t1000\n".to_vec());
        });
        refused(|f| f.caller = 1);
        refused(|f| f.caller = PRIV);
        refused(|f| f.files.retain(|(pid, _), _| *pid != CALLER));
        // The journal is unavailable.
        refused(|f| f.journal = None);
    }

    #[test]
    fn process_change_during_lookup_refuses_the_answer() {
        refused(|f| {
            f.after_journal = vec![((PRIV, "stat"), stat(PRIV, "sshd", LISTENER, PRIV_START + 1))]
        });
        refused(|f| f.after_journal = vec![((PRIV, "cmdline"), b"sshd: kate [priv]\0".to_vec())]);
        refused(|f| f.after_journal = vec![((CALLER, "stat"), stat(CALLER, "herdr", 700, 60_001))]);
        refused(|f| f.after_journal = vec![((LISTENER, "stat"), stat(LISTENER, "sshd", 1, 101))]);
        // A younger priv generation: its rows predate the pinned start.
        refused(|f| {
            f.files.insert(
                (PRIV, "stat"),
                stat(PRIV, "sshd", LISTENER, PRIV_START + 100),
            );
        });
    }

    #[test]
    fn exactly_one_canonical_decimal_pid() {
        let parse = |args: &[&str]| parse_pid(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(parse(&["20"]), Some(20));
        assert_eq!(parse(&["2147483647"]), Some(i32::MAX as u32));
        for bad in [
            &[][..],
            &["20", "21"],
            &[""],
            &["0"],
            &["1"],
            &["020"],
            &["+20"],
            &["-20"],
            &["20 "],
            &[" 20"],
            &["2147483648"],
            &["99999999999"],
            &["0x14"],
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(parse_pid(&[OsString::from_vec(vec![b'2', 0xff])]), None);
    }

    #[test]
    fn real_host_refuses_a_process_that_is_not_an_sshd_login() {
        let mut host = System {
            caller_gid: 0,
            journal_gid: 0,
        };
        assert!(host.clock().is_some());
        assert_eq!(lookup(&mut host, std::process::id()), None);
        assert_eq!(lookup(&mut host, u32::MAX >> 1), None);
        assert!(snapshot(&mut host, std::process::id()).is_some());
        // An unprivileged test cannot read pid 1's executable.
        // SAFETY: getuid cannot fail.
        if unsafe { libc::getuid() } != 0 {
            assert_eq!(host.exe(1), Some(Exe::Unreadable));
        }
    }
}
