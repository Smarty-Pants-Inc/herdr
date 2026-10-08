//! Checks and journal parsing. Every fact comes from /proc and from journald's
//! trusted fields, read by this process. No environment or configuration input.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::IpAddr;
use std::os::fd::{AsRawFd, RawFd};
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
/// The caller passes its accepted connection from the bridge here.
const CONNECTION_FD: RawFd = 3;
/// The server's chain walk allows at most three parents from the bridge to the priv.
const MAX_PARENTS: usize = 3;
/// Every journal field `accepted_record` reads. journald always adds the
/// last three to JSON output (journalctl(1), systemd 236+); naming them keeps
/// the query independent of that default.
const JOURNAL_FIELDS: &str = "MESSAGE,_PID,_UID,_COMM,_EXE,_TRANSPORT,\
_BOOT_ID,__REALTIME_TIMESTAMP,__MONOTONIC_TIMESTAMP";
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

fn socket_option(fd: RawFd, option: libc::c_int) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into the integer.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    (status == 0 && len as usize == std::mem::size_of::<libc::c_int>()).then_some(value)
}

/// The kernel-attested pid at the other end of `fd`, only when `fd` is a
/// connected AF_UNIX stream socket. `getpeername` refuses a listening socket,
/// whose SO_PEERCRED would name its own creator, and an unconnected one.
pub(crate) fn peer_pid(fd: RawFd) -> Option<u32> {
    if socket_option(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
        || socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
    {
        return None;
    }
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let mut len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: getpeername writes at most `len` bytes into the zeroed address.
    if unsafe { libc::getpeername(fd, address.as_mut_ptr().cast(), &mut len) } != 0 {
        return None;
    }
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into the zeroed ucred.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if status != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return None;
    }
    // SAFETY: SO_PEERCRED filled the zero-initialized structure.
    let pid = unsafe { credentials.assume_init() }.pid;
    u32::try_from(pid).ok().filter(|pid| *pid > 1)
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
    Some(Snapshot {
        parent,
        start,
        comm,
        uids: uids.try_into().ok()?,
        cmdline: host.read(pid, "cmdline")?,
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
        format!("--output-fields={JOURNAL_FIELDS}"),
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

fn is_priv(process: &Snapshot) -> bool {
    priv_user(&process.cmdline).is_some()
        && process.comm == "sshd"
        && process.uids == [0; 4]
        && process.exe != Exe::Other
}

/// `peer` is the kernel-attested pid at the other end of the caller's
/// connection (`peer_pid`). The answer names the sshd login that the peer
/// descends from, within the server's three-parent rule.
pub(crate) fn lookup(host: &mut impl Host, peer: u32) -> Option<Login> {
    let caller_pid = host.caller();
    if caller_pid <= 1 || peer <= 1 || peer == caller_pid {
        return None;
    }
    let caller = snapshot(host, caller_pid)?;
    let mut chain = vec![(peer, snapshot(host, peer)?)];
    // Like the server's walk: the caller is never traversed, no cycles, and
    // no parent is younger than its child.
    for _ in 0..MAX_PARENTS {
        let (_, child) = chain.last()?;
        let parent_pid = child.parent;
        if parent_pid <= 1
            || parent_pid == caller_pid
            || chain.iter().any(|(pid, _)| *pid == parent_pid)
        {
            return None;
        }
        let parent = snapshot(host, parent_pid)?;
        if parent.start > child.start {
            return None;
        }
        let found = is_priv(&parent);
        chain.push((parent_pid, parent));
        if found {
            break;
        }
    }
    let (pid, login) = chain.last().cloned()?;
    if !is_priv(&login) {
        return None;
    }
    let user = priv_user(&login.cmdline)?.to_owned();
    let listener_pid = login.parent;
    if listener_pid <= 1 || listener_pid == caller_pid {
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
    // Every fact was read by number. A changed process (a reused peer pid
    // included: its start time differs) refuses the answer.
    if host.caller() != caller_pid
        || snapshot(host, caller_pid)? != caller
        || snapshot(host, listener_pid)? != listener
    {
        return None;
    }
    for (pid, process) in &chain {
        if snapshot(host, *pid)? != *process {
            return None;
        }
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
    // Inherited descriptors must not reach journalctl. Fd 3 is the caller's
    // connection: read its peer, then close it too.
    // SAFETY: close_range has no pointer arguments.
    if unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) } != 0 {
        return None;
    }
    let peer = peer_pid(CONNECTION_FD);
    // SAFETY: fd 3 is not used again.
    if unsafe { libc::close(CONNECTION_FD) } != 0 {
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
    let mut host = System {
        caller_gid: real,
        journal_gid: effective,
    };
    // The contract takes no arguments: the peer is the only input.
    let login = peer
        .filter(|_| std::env::args_os().len() == 1)
        .and_then(|peer| lookup(&mut host, peer));
    if !set_gids(real, real, real) {
        return None;
    }
    login.map(|login| login.line())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::fd::AsRawFd;

    const ACCEPTED: &str =
        "Accepted publickey for paul from 100.64.0.7 port 1234 ssh2: ED25519 SHA256:paulKey+/=";
    const BOOT: &str = "0123456789abcdef0123456789abcdef";
    /// The Herdr server that runs the helper.
    const CALLER: u32 = 900;
    /// `herdr remote-client-bridge`, at the other end of fd 3.
    const BRIDGE: u32 = 40;
    const NOTTY: u32 = 30;
    const PRIV: u32 = 20;
    const LISTENER: u32 = 10;
    /// 500 s after boot at 100 ticks per second.
    const PRIV_START: u64 = 50_000;
    const BOOT_MICROS: u64 = 1_000_000_000;
    const START_MONO: u64 = 500_000_000;
    const START_REAL: u64 = BOOT_MICROS + START_MONO;
    /// A real row from this kind of query on systemd 255 (ryzen2, user
    /// journal: the priv's root rows need the journal group), with its
    /// identity fields rewritten to the fixture's login.
    const REAL_ROW: &str = r#"{"MESSAGE":"Disconnected from user paul 100.78.221.70 port 45732","__CURSOR":"s=ddf8c2b7f40c4315bf30bf6fd8ce41c0;i=83e198;b=8d13f1eae46342f58531ea73c9813542;m=aff907dcfb;t=65d4e8f4059eb;x=1e5edca72a63987d","__SEQNUM":"8642968","_PID":"3212144","__MONOTONIC_TIMESTAMP":"755797318907","__REALTIME_TIMESTAMP":"1791441852455403","_BOOT_ID":"8d13f1eae46342f58531ea73c9813542","_EXE":"/usr/sbin/sshd","_COMM":"sshd","__SEQNUM_ID":"ddf8c2b7f40c4315bf30bf6fd8ce41c0","_TRANSPORT":"syslog","_UID":"1000"}"#;

    #[derive(Clone)]
    struct Fixture {
        files: BTreeMap<(u32, &'static str), Vec<u8>>,
        exes: BTreeMap<u32, Exe>,
        caller: u32,
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

    fn add(
        files: &mut BTreeMap<(u32, &'static str), Vec<u8>>,
        (pid, comm, parent, start, uid): (u32, &str, u32, u64, u32),
        cmdline: &[u8],
        cgroup: &str,
    ) {
        files.insert((pid, "stat"), stat(pid, comm, parent, start));
        files.insert((pid, "status"), status(uid));
        files.insert((pid, "cmdline"), cmdline.to_vec());
        files.insert((pid, "cgroup"), cgroup.as_bytes().to_vec());
    }

    const SESSION: &str = "0::/user.slice/user-1000.slice/session-7.scope\n";

    fn fixture() -> Fixture {
        let mut files = BTreeMap::new();
        add(
            &mut files,
            (CALLER, "herdr", 700, 60_000, 1000),
            b"herdr\0server\0",
            "0::/user.slice/user-1000.slice/session-4.scope\n",
        );
        add(
            &mut files,
            (BRIDGE, "herdr", NOTTY, PRIV_START + 20, 1000),
            b"herdr\0remote-client-bridge\0",
            SESSION,
        );
        add(
            &mut files,
            (NOTTY, "sshd", PRIV, PRIV_START + 10, 1000),
            b"sshd: paul@notty\0",
            SESSION,
        );
        add(
            &mut files,
            (PRIV, "sshd", LISTENER, PRIV_START, 0),
            b"sshd: paul [priv]\0\0\0\0",
            SESSION,
        );
        add(
            &mut files,
            (LISTENER, "sshd", 1, 100, 0),
            b"sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups\0",
            "0::/system.slice/ssh.service\n",
        );
        Fixture {
            files,
            exes: [
                (CALLER, Exe::Other),
                (BRIDGE, Exe::Other),
                (NOTTY, Exe::Sshd),
                (PRIV, Exe::Unreadable),
                (LISTENER, Exe::Unreadable),
            ]
            .into(),
            caller: CALLER,
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
    fn good_chain_prints_one_line_from_one_bounded_query() {
        let mut f = fixture();
        let login = lookup(&mut f, BRIDGE).expect("login");
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

    /// P1-a: the query must name every field the parser reads. A row that
    /// holds only the requested fields (plus the cursor) still proves the login.
    #[test]
    fn journal_query_requests_every_field_the_record_parser_reads() {
        let args = journal_args(PRIV, 1);
        let requested: Vec<&str> = args
            .iter()
            .find_map(|a| a.strip_prefix("--output-fields="))
            .expect("output fields")
            .split(',')
            .collect();
        let read = [
            "MESSAGE",
            "_PID",
            "_UID",
            "_COMM",
            "_EXE",
            "_TRANSPORT",
            "_BOOT_ID",
            "__REALTIME_TIMESTAMP",
            "__MONOTONIC_TIMESTAMP",
        ];
        for field in read {
            assert!(requested.contains(&field), "{field} is not requested");
        }
        let clock = fixture().clock().unwrap();
        let project = |message: &str| {
            let full = row(message);
            let mut kept = serde_json::Map::new();
            for (key, value) in full.as_object().unwrap() {
                if requested.contains(&key.as_str()) || key == "__CURSOR" {
                    kept.insert(key.clone(), value.clone());
                }
            }
            serde_json::Value::Object(kept)
        };
        let output = journal(&[project("Connection from x"), project(ACCEPTED)]).unwrap();
        assert_eq!(
            accepted_record(&output, PRIV, "paul", &clock, START_MONO, START_REAL),
            Some(("SHA256:paulKey+/=".into(), "100.64.0.7".parse().unwrap()))
        );
        // Each read field is load-bearing: without it the row refuses.
        for field in read {
            let mut row = project(ACCEPTED);
            row.as_object_mut().unwrap().remove(field);
            let output = journal(&[row]).unwrap();
            assert_eq!(
                accepted_record(&output, PRIV, "paul", &clock, START_MONO, START_REAL),
                None,
                "{field}"
            );
        }
    }

    /// The real JSON shape (string timestamps, undashed boot id, extra
    /// fields) parses; only the identity values are the fixture's.
    #[test]
    fn real_journal_row_shape_is_accepted() {
        let mut real: serde_json::Value = serde_json::from_str(REAL_ROW).unwrap();
        let boot = real["_BOOT_ID"].as_str().unwrap().to_owned();
        let realtime: u64 = real["__REALTIME_TIMESTAMP"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let monotonic: u64 = real["__MONOTONIC_TIMESTAMP"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        real["_PID"] = "20".into();
        real["_UID"] = "0".into();
        real["MESSAGE"] = ACCEPTED.into();
        let clock = Clock {
            boot_id: boot,
            boot_micros: realtime - monotonic,
            ticks: 100,
            now: realtime + 1,
            monotonic_now: monotonic + 1,
        };
        let output = journal(&[real]).unwrap();
        assert_eq!(
            accepted_record(&output, PRIV, "paul", &clock, monotonic - 1, realtime - 1)
                .map(|(fp, _)| fp),
            Some("SHA256:paulKey+/=".into())
        );
    }

    #[test]
    fn readable_sshd_exe_is_accepted_like_the_journal_proof() {
        let mut f = fixture();
        f.exes.insert(PRIV, Exe::Sshd);
        f.exes.insert(LISTENER, Exe::Sshd);
        assert_eq!(lookup(&mut f, BRIDGE), Some(paul()));
    }

    fn refused(change: impl FnOnce(&mut Fixture)) {
        let mut f = fixture();
        change(&mut f);
        assert_eq!(lookup(&mut f, BRIDGE), None);
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
    fn priv_checks_refuse_dead_spoofed_or_orphaned_logins() {
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
        // The caller is not alive, is init, or is part of the login chain.
        refused(|f| f.caller = 1);
        refused(|f| f.files.retain(|(pid, _), _| *pid != CALLER));
        refused(|f| f.caller = LISTENER);
        // The journal is unavailable.
        refused(|f| f.journal = None);
    }

    /// A peer that is not within three parents of a root sshd priv learns
    /// nothing: the agent's own socket, a local process, or a title spoof.
    #[test]
    fn peer_that_does_not_descend_from_a_priv_refuses() {
        // A local process: bridge -> shell -> herdr server -> systemd user.
        refused(|f| {
            add(
                &mut f.files,
                (BRIDGE, "herdr", 50, PRIV_START + 20, 1000),
                b"herdr\0remote-client-bridge\0",
                SESSION,
            );
            add(&mut f.files, (50, "bash", 51, 10, 1000), b"bash\0", SESSION);
            add(
                &mut f.files,
                (51, "herdr", 52, 5, 1000),
                b"herdr\0",
                SESSION,
            );
            add(
                &mut f.files,
                (52, "systemd", 1, 1, 1000),
                b"systemd\0",
                SESSION,
            );
            f.exes
                .extend([(50, Exe::Other), (51, Exe::Other), (52, Exe::Other)]);
        });
        // The walk reaches init before a priv.
        refused(|f| {
            add(
                &mut f.files,
                (NOTTY, "sshd", 1, PRIV_START + 10, 1000),
                b"sshd: paul@notty\0",
                SESSION,
            );
        });
        // A fourth parent is out of reach (the server's rule).
        refused(|f| {
            add(
                &mut f.files,
                (BRIDGE, "herdr", 41, PRIV_START + 20, 1000),
                b"herdr\0remote-client-bridge\0",
                SESSION,
            );
            add(
                &mut f.files,
                (41, "sh", 42, PRIV_START + 15, 1000),
                b"sh\0",
                SESSION,
            );
            add(
                &mut f.files,
                (42, "sh", NOTTY, PRIV_START + 12, 1000),
                b"sh\0",
                SESSION,
            );
            f.exes.extend([(41, Exe::Other), (42, Exe::Other)]);
        });
        // The walk ends on its third parent: a non-root process titled like
        // the priv there is not one.
        refused(|f| {
            add(
                &mut f.files,
                (BRIDGE, "herdr", 41, PRIV_START + 20, 1000),
                b"herdr\0remote-client-bridge\0",
                SESSION,
            );
            add(
                &mut f.files,
                (41, "sh", NOTTY, PRIV_START + 15, 1000),
                b"sh\0",
                SESSION,
            );
            f.exes.insert(41, Exe::Other);
            f.files.insert((PRIV, "status"), status(1000));
        });
        // A user process titled like a priv is not one; nor is the peer itself.
        refused(|f| {
            add(
                &mut f.files,
                (NOTTY, "sshd", 1, PRIV_START + 10, 1000),
                b"sshd: paul [priv]\0",
                SESSION,
            );
        });
        let mut f = fixture();
        assert_eq!(lookup(&mut f, PRIV), None);
        // The caller is the peer (its own socketpair) or sits in the chain.
        refused(|f| f.caller = BRIDGE);
        refused(|f| f.caller = NOTTY);
        // A parent younger than its child, and a cycle.
        refused(|f| {
            f.files
                .insert((NOTTY, "stat"), stat(NOTTY, "sshd", PRIV, PRIV_START + 30));
        });
        refused(|f| {
            f.files.insert(
                (NOTTY, "stat"),
                stat(NOTTY, "sshd", BRIDGE, PRIV_START + 10),
            );
        });
        // The peer is gone.
        refused(|f| f.files.retain(|(pid, _), _| *pid != BRIDGE));
    }

    #[test]
    fn third_parent_allowed() {
        let mut f = fixture();
        add(
            &mut f.files,
            (BRIDGE, "herdr", 41, PRIV_START + 20, 1000),
            b"herdr\0remote-client-bridge\0",
            SESSION,
        );
        add(
            &mut f.files,
            (41, "sh", NOTTY, PRIV_START + 15, 1000),
            b"sh\0",
            SESSION,
        );
        f.exes.insert(41, Exe::Other);
        assert_eq!(lookup(&mut f, BRIDGE), Some(paul()));
    }

    #[test]
    fn process_change_during_lookup_refuses_the_answer() {
        // Peer pid reuse: the bridge pid now names a new process.
        refused(|f| {
            f.after_journal = vec![(
                (BRIDGE, "stat"),
                stat(BRIDGE, "herdr", NOTTY, PRIV_START + 21),
            )]
        });
        refused(|f| {
            f.after_journal = vec![((NOTTY, "stat"), stat(NOTTY, "sshd", PRIV, PRIV_START + 11))]
        });
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
            f.files
                .insert((NOTTY, "stat"), stat(NOTTY, "sshd", PRIV, PRIV_START + 110));
            f.files.insert(
                (BRIDGE, "stat"),
                stat(BRIDGE, "herdr", NOTTY, PRIV_START + 120),
            );
        });
    }

    #[test]
    fn only_a_connected_unix_stream_socket_names_a_peer() {
        use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
        // Connected stream pair: the kernel names this process.
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(a.as_raw_fd()), Some(std::process::id()));
        // A connected stream accepted from a listener, like the server's.
        let dir = std::env::temp_dir().join(format!("herdr-peer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert_eq!(peer_pid(accepted.as_raw_fd()), Some(std::process::id()));
        // A listening socket reports its own creator: refused.
        assert_eq!(peer_pid(listener.as_raw_fd()), None);
        // Not a socket, a closed fd, a datagram pair, an unconnected socket.
        let file = File::open("/proc/self/stat").unwrap();
        assert_eq!(peer_pid(file.as_raw_fd()), None);
        let mut pipe = [0; 2];
        // SAFETY: valid two-element output array.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        assert_eq!(peer_pid(pipe[0]), None);
        // SAFETY: closing the two descriptors created above.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        assert_eq!(peer_pid(-1), None);
        let (d, _e) = UnixDatagram::pair().unwrap();
        assert_eq!(peer_pid(d.as_raw_fd()), None);
        // SAFETY: socket has no pointer arguments.
        let unconnected = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(unconnected >= 0);
        assert_eq!(peer_pid(unconnected), None);
        // SAFETY: closing the descriptor created above.
        unsafe { libc::close(unconnected) };
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_client = std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
        assert_eq!(peer_pid(tcp_client.as_raw_fd()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn real_host_refuses_a_peer_that_is_not_an_sshd_login() {
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
