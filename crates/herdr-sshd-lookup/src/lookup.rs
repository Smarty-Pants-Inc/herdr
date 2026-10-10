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
/// `/proc/<pid>/net/unix` on a busy host; a longer table refuses.
const UNIX_TABLE_LIMIT: usize = 16 << 20;
/// A bridge holds a handful of descriptors; a longer table refuses.
const FD_LIMIT: usize = 4_096;
/// `__SO_ACCEPTCON` in the `Flags` column of `/proc/net/unix`: listening.
const SO_ACCEPTCON: u32 = 0x0001_0000;
/// The subcommand the bridge runs as (`herdr remote-client-bridge`), as the
/// server's own check reads it.
const BRIDGE_SUBCOMMAND: &str = "remote-client-bridge";
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
    /// Any other executable, by device and inode.
    Other(u64, u64),
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
    /// Socket inodes among `/proc/<pid>/fd`; None when unreadable or too many.
    fn sockets(&mut self, pid: u32) -> Option<Vec<u64>>;
    /// Bounded `/proc/<pid>/net/unix`: the Unix sockets of its network namespace.
    fn unix_table(&mut self, pid: u32) -> Option<Vec<u8>>;
    /// `kernel.yama.ptrace_scope`; None without Yama.
    fn ptrace_scope(&mut self) -> Option<u32>;
}

/// The caller's connection (fd 3): its socket inode and its peer pid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Connection {
    pub peer: u32,
    pub inode: u64,
}

#[derive(Debug, PartialEq)]
struct UnixRow {
    inode: u64,
    listening: bool,
    /// Bound path; abstract names start with `@`. None when unnamed.
    path: Option<String>,
}

/// Rows of `/proc/<pid>/net/unix`. A bound path may hold a newline, which
/// splits its row; the row's own fields stay on the first line, so a line
/// that does not parse is skipped, never trusted.
fn unix_rows(table: &[u8]) -> Vec<UnixRow> {
    String::from_utf8_lossy(table)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.splitn(8, ' ').collect();
            if fields.len() < 7 || !fields[0].ends_with(':') {
                return None;
            }
            let flags = u32::from_str_radix(fields[3], 16).ok()?;
            u32::from_str_radix(fields[4], 16).ok()?;
            u32::from_str_radix(fields[5], 16).ok()?;
            Some(UnixRow {
                inode: fields[6].parse().ok()?,
                listening: flags & SO_ACCEPTCON != 0,
                path: fields
                    .get(7)
                    .filter(|p| !p.is_empty())
                    .map(|p| (*p).to_owned()),
            })
        })
        .collect()
}

/// The fd-3 peer is a bridge that this caller accepted: the peer runs the
/// bridge subcommand and holds no listening socket, and the caller holds
/// both this accepted socket and the listener it was accepted from.
fn accepted_bridge(
    host: &mut impl Host,
    caller: u32,
    peer: &Snapshot,
    connection: Connection,
) -> Option<()> {
    let argv: Vec<_> = peer.cmdline.split(|b| *b == 0).collect();
    if argv.get(1).copied() != Some(BRIDGE_SUBCOMMAND.as_bytes()) {
        return None;
    }
    let peer_sockets = host.sockets(connection.peer)?;
    let peer_rows = unix_rows(&host.unix_table(connection.peer)?);
    if peer_rows
        .iter()
        .any(|row| row.listening && peer_sockets.contains(&row.inode))
    {
        return None;
    }
    let caller_sockets = host.sockets(caller)?;
    if !caller_sockets.contains(&connection.inode) {
        return None;
    }
    let caller_rows = unix_rows(&host.unix_table(caller)?);
    // An accepted socket carries its listener's address; the connecting
    // side is unnamed.
    let path = caller_rows
        .iter()
        .find(|row| row.inode == connection.inode && !row.listening)?
        .path
        .as_ref()?;
    caller_rows
        .iter()
        .any(|row| {
            row.listening && row.path.as_ref() == Some(path) && caller_sockets.contains(&row.inode)
        })
        .then_some(())
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

/// The inode of the socket at `fd`, as `/proc/<pid>/fd` and `/proc/net/unix` name it.
fn socket_inode(fd: RawFd) -> Option<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat fills the zeroed structure on success.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: fstat succeeded.
    let stat = unsafe { stat.assume_init() };
    (stat.st_mode & libc::S_IFMT == libc::S_IFSOCK).then_some(stat.st_ino)
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
        && !matches!(process.exe, Exe::Other(..))
}

/// `connection.peer` is the kernel-attested pid at the other end of the
/// caller's connection (`peer_pid`). The answer names the sshd login that the
/// peer descends from, within the server's three-parent rule.
pub(crate) fn lookup(host: &mut impl Host, connection: Connection) -> Option<Login> {
    let peer = connection.peer;
    let caller_pid = host.caller();
    if caller_pid <= 1 || peer <= 1 || peer == caller_pid {
        return None;
    }
    // Without Yama (or with scope 0) another process of the same user could
    // ptrace a bridge and make it connect anywhere.
    if host.ptrace_scope()? < 1 {
        return None;
    }
    let caller = snapshot(host, caller_pid)?;
    let mut chain = vec![(peer, snapshot(host, peer)?)];
    // The peer runs the caller's executable (both the installed herdr) as a
    // bridge, and the caller accepted it on its own listener. A bridge never
    // listens, so a process holds a socket whose peer is a bridge only when
    // that bridge connected to it. The end re-read repeats the executable and
    // argv checks, since the snapshots hold them.
    if !matches!(caller.exe, Exe::Other(..)) || chain[0].1.exe != caller.exe {
        return None;
    }
    accepted_bridge(host, caller_pid, &chain[0].1, connection)?;
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
        || matches!(listener.exe, Exe::Other(..))
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
                    Exe::Other(exe.dev(), exe.ino())
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

    fn sockets(&mut self, pid: u32) -> Option<Vec<u64>> {
        let mut inodes = Vec::new();
        let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
        for (index, entry) in entries.enumerate() {
            if index >= FD_LIMIT {
                return None;
            }
            match std::fs::read_link(entry.ok()?.path()) {
                Ok(target) => inodes.extend(target.to_str().and_then(|target| {
                    target
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .parse::<u64>()
                        .ok()
                })),
                // Closed while listing.
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
        Some(inodes)
    }

    fn unix_table(&mut self, pid: u32) -> Option<Vec<u8>> {
        bounded_read(&format!("/proc/{pid}/net/unix"), UNIX_TABLE_LIMIT)
    }

    fn ptrace_scope(&mut self) -> Option<u32> {
        String::from_utf8(bounded_read("/proc/sys/kernel/yama/ptrace_scope", 16)?)
            .ok()?
            .trim()
            .parse()
            .ok()
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
    let peer = peer_pid(CONNECTION_FD).and_then(|peer| {
        Some(Connection {
            peer,
            inode: socket_inode(CONNECTION_FD)?,
        })
    });
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
    /// The installed herdr binary: the server (caller) and the bridge.
    const HERDR: Exe = Exe::Other(1, 100);
    const SHELL: Exe = Exe::Other(1, 200);
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
        after_journal_exes: Vec<(u32, Exe)>,
        sockets: BTreeMap<u32, Vec<u64>>,
        tables: BTreeMap<u32, String>,
        ptrace_scope: Option<u32>,
    }

    /// The server's listener, its accepted socket (fd 3) and the bridge's end.
    const LISTEN_INO: u64 = 500;
    const ACCEPTED_INO: u64 = 501;
    const CLIENT_INO: u64 = 502;
    const SERVER_SOCKET: &str = "/run/user/1000/herdr/herdr.sock";

    fn unix_row(inode: u64, listening: bool, path: &str) -> String {
        let (flags, state) = if listening {
            ("00010000", "01")
        } else {
            ("00000000", "03")
        };
        format!("0000000000000000: 00000002 00000000 {flags} 0001 {state} {inode} {path}\n")
            .replace(" \n", "\n")
    }

    fn unix_table(rows: &[String]) -> String {
        format!(
            "Num       RefCount Protocol Flags    Type St Inode Path\n{}",
            rows.concat()
        )
    }

    fn connection() -> Connection {
        Connection {
            peer: BRIDGE,
            inode: ACCEPTED_INO,
        }
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
        fn sockets(&mut self, pid: u32) -> Option<Vec<u64>> {
            self.sockets.get(&pid).cloned()
        }
        fn unix_table(&mut self, pid: u32) -> Option<Vec<u8>> {
            self.tables.get(&pid).map(|t| t.clone().into_bytes())
        }
        fn ptrace_scope(&mut self) -> Option<u32> {
            self.ptrace_scope
        }
        fn journal(&mut self, args: &[String]) -> Option<Vec<u8>> {
            self.journal_calls.push(args.to_vec());
            for (key, value) in std::mem::take(&mut self.after_journal) {
                self.files.insert(key, value);
            }
            self.exes
                .extend(std::mem::take(&mut self.after_journal_exes));
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
                (CALLER, HERDR),
                (BRIDGE, HERDR),
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
            after_journal_exes: Vec::new(),
            sockets: [
                (CALLER, vec![LISTEN_INO, ACCEPTED_INO]),
                (BRIDGE, vec![CLIENT_INO]),
            ]
            .into(),
            tables: {
                let table = unix_table(&[
                    unix_row(LISTEN_INO, true, SERVER_SOCKET),
                    unix_row(ACCEPTED_INO, false, SERVER_SOCKET),
                    unix_row(CLIENT_INO, false, ""),
                ]);
                [(CALLER, table.clone()), (BRIDGE, table)].into()
            },
            ptrace_scope: Some(1),
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
        let login = lookup(&mut f, connection()).expect("login");
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
        assert_eq!(lookup(&mut f, connection()), Some(paul()));
    }

    fn refused(change: impl FnOnce(&mut Fixture)) {
        let mut f = fixture();
        change(&mut f);
        assert_eq!(lookup(&mut f, connection()), None);
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
            f.exes.insert(PRIV, SHELL);
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
            f.exes.insert(LISTENER, SHELL);
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
            f.exes.extend([(50, SHELL), (51, SHELL), (52, SHELL)]);
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
            f.exes.extend([(41, SHELL), (42, SHELL)]);
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
            f.exes.insert(41, SHELL);
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
        assert_eq!(
            lookup(
                &mut f,
                Connection {
                    peer: PRIV,
                    ..connection()
                }
            ),
            None
        );
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

    /// The peer must run the caller's executable inode (the installed herdr),
    /// read before the walk and again at the end.
    #[test]
    fn peer_must_run_the_callers_executable() {
        // Same inode: the good chain passes (also with a readable sshd exe).
        let mut f = fixture();
        assert_eq!(f.exes[&BRIDGE], f.exes[&CALLER]);
        assert_eq!(lookup(&mut f, connection()), Some(paul()));
        // Another executable: same inode on another device, another inode.
        refused(|f| {
            f.exes.insert(BRIDGE, Exe::Other(2, 100));
        });
        refused(|f| {
            f.exes.insert(BRIDGE, SHELL);
        });
        // Unreadable (another user's process) or sshd: refused, also for the caller.
        refused(|f| {
            f.exes.insert(BRIDGE, Exe::Unreadable);
        });
        refused(|f| {
            f.exes.insert(BRIDGE, Exe::Sshd);
        });
        refused(|f| {
            f.exes.insert(CALLER, Exe::Unreadable);
            f.exes.insert(BRIDGE, Exe::Unreadable);
        });
        refused(|f| {
            f.exes.insert(CALLER, Exe::Sshd);
            f.exes.insert(BRIDGE, Exe::Sshd);
        });
        refused(|f| {
            f.exes.remove(&BRIDGE);
        });
        // Changed between the reads, for the peer or the caller.
        refused(|f| f.after_journal_exes = vec![(BRIDGE, SHELL)]);
        refused(|f| f.after_journal_exes = vec![(CALLER, SHELL)]);
        refused(|f| f.after_journal_exes = vec![(BRIDGE, SHELL), (CALLER, SHELL)]);
    }

    /// Round 3: the peer is a bridge this caller accepted, on a host where
    /// another process of the same user cannot ptrace it.
    #[test]
    fn peer_must_be_a_bridge_accepted_by_the_caller() {
        // The bridge accepted by the caller passes.
        assert_eq!(lookup(&mut fixture(), connection()), Some(paul()));
        // The peer is a server: it listens, or runs another subcommand.
        refused(|f| {
            f.sockets.get_mut(&BRIDGE).unwrap().push(LISTEN_INO);
        });
        refused(|f| {
            let table = unix_table(&[
                unix_row(LISTEN_INO, true, SERVER_SOCKET),
                unix_row(ACCEPTED_INO, false, SERVER_SOCKET),
                unix_row(CLIENT_INO, false, ""),
                unix_row(600, true, "@herdr-b"),
            ]);
            f.tables.insert(BRIDGE, table);
            f.sockets.get_mut(&BRIDGE).unwrap().push(600);
        });
        for argv in [
            &b"herdr\0server\0"[..],
            b"herdr\0",
            b"herdr\0--session\0b\0remote-client-bridge\0",
            b"herdr\0remote-client-bridge-x\0",
        ] {
            refused(|f| {
                f.files.insert((BRIDGE, "cmdline"), argv.to_vec());
            });
        }
        // Changed argv between the reads.
        refused(|f| f.after_journal = vec![((BRIDGE, "cmdline"), b"herdr\0server\0".to_vec())]);
        // The peer's descriptors or table are unreadable.
        refused(|f| {
            f.sockets.remove(&BRIDGE);
        });
        refused(|f| {
            f.tables.remove(&BRIDGE);
        });
        // The caller did not accept this connection: it does not hold it, it
        // holds the connecting (unnamed) side, or it holds no listener with
        // the accepted socket's path.
        refused(|f| {
            f.sockets.insert(CALLER, vec![LISTEN_INO]);
        });
        let mut f = fixture();
        f.sockets.insert(CALLER, vec![LISTEN_INO, CLIENT_INO]);
        let client_side = Connection {
            peer: BRIDGE,
            inode: CLIENT_INO,
        };
        assert_eq!(lookup(&mut f, client_side), None);
        // The listener itself is not an accepted connection.
        let listener = Connection {
            peer: BRIDGE,
            inode: LISTEN_INO,
        };
        assert_eq!(lookup(&mut fixture(), listener), None);
        refused(|f| {
            f.sockets.insert(CALLER, vec![ACCEPTED_INO]);
        });
        refused(|f| {
            let table = unix_table(&[
                unix_row(LISTEN_INO, true, "/run/user/1000/other.sock"),
                unix_row(ACCEPTED_INO, false, SERVER_SOCKET),
            ]);
            f.tables.insert(CALLER, table);
        });
        refused(|f| {
            let table = unix_table(&[
                unix_row(LISTEN_INO, true, SERVER_SOCKET),
                unix_row(ACCEPTED_INO, false, ""),
            ]);
            f.tables.insert(CALLER, table);
        });
        refused(|f| {
            f.tables.remove(&CALLER);
        });
        // Same-user ptrace is open, or Yama is absent.
        refused(|f| f.ptrace_scope = Some(0));
        refused(|f| f.ptrace_scope = None);
        for scope in [2, 3] {
            let mut f = fixture();
            f.ptrace_scope = Some(scope);
            assert_eq!(lookup(&mut f, connection()), Some(paul()));
        }
    }

    #[test]
    fn unix_table_rows_parse_and_split_paths_are_skipped() {
        let table = unix_table(&[
            unix_row(500, true, "/run/a b.sock"),
            unix_row(501, false, "@abstract"),
            unix_row(502, false, ""),
            // A path with a newline: its tail is not a row.
            "0000000000000000: 00000002 00000000 00010000 0001 01 503 /tmp/x\nfake 00010000\n"
                .into(),
        ]);
        let rows = unix_rows(table.as_bytes());
        assert_eq!(
            rows,
            vec![
                UnixRow {
                    inode: 500,
                    listening: true,
                    path: Some("/run/a b.sock".into())
                },
                UnixRow {
                    inode: 501,
                    listening: false,
                    path: Some("@abstract".into())
                },
                UnixRow {
                    inode: 502,
                    listening: false,
                    path: None
                },
                UnixRow {
                    inode: 503,
                    listening: true,
                    path: Some("/tmp/x".into())
                },
            ]
        );
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
        f.exes.insert(41, SHELL);
        assert_eq!(lookup(&mut f, connection()), Some(paul()));
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
        let own = |peer| Connection {
            peer,
            inode: ACCEPTED_INO,
        };
        assert_eq!(lookup(&mut host, own(std::process::id())), None);
        assert_eq!(lookup(&mut host, own(u32::MAX >> 1)), None);
        // The real tables name a listener, its accepted socket (with the
        // listener's path) and the unnamed connecting side.
        use std::os::unix::net::{UnixListener, UnixStream};
        let dir = std::env::temp_dir().join(format!("herdr-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let inode = |fd: RawFd| socket_inode(fd).unwrap();
        let (l, c, a) = (
            inode(listener.as_raw_fd()),
            inode(client.as_raw_fd()),
            inode(accepted.as_raw_fd()),
        );
        let sockets = host.sockets(std::process::id()).unwrap();
        assert!([l, c, a].iter().all(|i| sockets.contains(i)));
        let rows = unix_rows(&host.unix_table(std::process::id()).unwrap());
        let row = |i| rows.iter().find(|r| r.inode == i).unwrap();
        let named = Some(path.to_str().unwrap().to_owned());
        assert_eq!((row(l).listening, &row(l).path), (true, &named));
        assert_eq!((row(a).listening, &row(a).path), (false, &named));
        assert_eq!((row(c).listening, &row(c).path), (false, &None));
        assert_eq!(
            socket_inode(File::open("/proc/self/stat").unwrap().as_raw_fd()),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
        if std::path::Path::new("/proc/sys/kernel/yama/ptrace_scope").exists() {
            assert!(host.ptrace_scope().is_some());
        }
        assert!(snapshot(&mut host, std::process::id()).is_some());
        // A same-user process's executable reads as its device and inode.
        let own = std::fs::metadata("/proc/self/exe").unwrap();
        assert_eq!(
            host.exe(std::process::id()),
            Some(Exe::Other(own.dev(), own.ino()))
        );
        // An unprivileged test cannot read a root pid 1's executable (a CI
        // container's pid 1 may run as the test's own user).
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        if uid != 0 && snapshot(&mut host, 1).is_some_and(|init| init.uids == [0; 4]) {
            assert_eq!(host.exe(1), Some(Exe::Unreadable));
        }
    }
}
