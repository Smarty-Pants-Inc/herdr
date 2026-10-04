//! Linux-only connection identity. All failures leave the principal absent.
//!
//! Trust roots: kernel socket credentials + atomic peer pidfd, pinned proc
//! incarnations/parent edges, root-controlled map/executables, root sshd journal
//! metadata, and the root Tailscale daemon. Client names, environment, argv and
//! a fingerprint on their own never constitute identity.

use std::{
    collections::HashSet,
    ffi::CString,
    fs::File,
    io::{self, Read},
    net::IpAddr,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::server::client_identity::{ClientIdentity, Principal};
use serde::Deserialize;
use serde_json::Value;

const MAP_PATH: &str = "/etc/herdr/principals.json";
const JOURNALCTL: &str = "/usr/bin/journalctl";
const TAILSCALE: &str = "/usr/bin/tailscale";
const TAILSCALE_SOCKET: &str = "/run/tailscale/tailscaled.sock";
const SSHD: &str = "/usr/sbin/sshd";
const MAX_PARENTS: usize = 3;
const MAP_LIMIT: usize = 64 * 1024;
const COMMAND_LIMIT: usize = 256 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalMap {
    version: u32,
    principals: Vec<MapEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MapEntry {
    id: String,
    name: String,
    ssh_keys: Vec<String>,
    tailscale_nodes: Vec<String>,
}

impl PrincipalMap {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAP_LIMIT {
            return None;
        }
        let map: Self = serde_json::from_slice(bytes).ok()?;
        if map.version != 1 || map.principals.len() > 128 {
            return None;
        }
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        let mut nodes = HashSet::new();
        for entry in &map.principals {
            if entry.id.is_empty()
                || entry.id.len() > 80
                || !entry
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                || entry.name.trim() != entry.name
                || entry.name.is_empty()
                || entry.name.chars().count() > 80
                || entry
                    .name
                    .chars()
                    .any(|c| c.is_control() || "*[]\\".contains(c))
                || !ids.insert(&entry.id)
                || entry.ssh_keys.len() > 32
                || entry.tailscale_nodes.len() > 32
            {
                return None;
            }
            for key in &entry.ssh_keys {
                if !fingerprint_valid(key) || !keys.insert(key) {
                    return None;
                }
            }
            for node in &entry.tailscale_nodes {
                if node.is_empty()
                    || node.len() > 128
                    || !node
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                    || !nodes.insert(node)
                {
                    return None;
                }
            }
        }
        Some(map)
    }

    fn lookup(&self, fingerprint: &str, node: &str) -> Option<Principal> {
        // Both independent facts must match the SAME entry. No key-only or
        // node-only fallback, including when either list is empty.
        let entry = self.principals.iter().find(|entry| {
            entry.ssh_keys.iter().any(|key| key == fingerprint)
                && entry.tailscale_nodes.iter().any(|id| id == node)
        })?;
        Some(Principal {
            id: entry.id.clone(),
            name: entry.name.clone(),
        })
    }
}

fn fingerprint_valid(value: &str) -> bool {
    value.strip_prefix("SHA256:").is_some_and(|fp| {
        fp.len() == 43
            && fp
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
    })
}

pub(super) fn resolve(stream: &crate::ipc::LocalStream) -> ClientIdentity {
    let crate::ipc::LocalStream::UdSocket(socket) = stream;
    resolve_fd(socket.inner().as_raw_fd())
}

fn resolve_fd(fd: RawFd) -> ClientIdentity {
    let Some(peer) = peer_credentials(fd) else {
        return ClientIdentity::default();
    };
    let principal = resolve_principal(fd, peer);
    ClientIdentity {
        peer_pid: Some(peer.pid),
        uid: Some(peer.uid),
        principal,
    }
}

use super::{
    local_socket_peer_credentials as peer_credentials, LocalSocketPeerCredentials as Peer,
};

/// Atomic custody of the process kernel attached to an API socket.
///
/// This is deliberately based on SO_PEERPIDFD, not pidfd_open(SO_PEERCRED.pid):
/// the latter can attach to a later process after connector death.
pub(crate) struct LocalSocketPeerCustody {
    pid: u32,
    pidfd: OwnedFd,
}

impl LocalSocketPeerCustody {
    pub(crate) fn from_socket(fd: RawFd) -> Option<Self> {
        let peer = peer_credentials(fd)?;
        let pidfd = socket_peer_pidfd(fd)?;
        (pidfd_pid(&pidfd) == Some(peer.pid) && pidfd_alive(&pidfd)).then_some(Self {
            pid: peer.pid,
            pidfd,
        })
    }

    pub(crate) fn is_alive(&self, fd: RawFd) -> bool {
        peer_credentials(fd).is_some_and(|peer| peer.pid == self.pid)
            && pidfd_pid(&self.pidfd) == Some(self.pid)
            && pidfd_alive(&self.pidfd)
    }
}

fn resolve_principal(fd: RawFd, peer: Peer) -> Option<Principal> {
    // Root/pane-root can modify every trust root. No attestation in that case.
    if peer.uid == 0 {
        return None;
    }
    let map = PrincipalMap::parse(&read_limited(
        trusted_file(Path::new(MAP_PATH), false)?,
        MAP_LIMIT,
    )?)?;
    let peer_pidfd = socket_peer_pidfd(fd)?;
    // SO_PEERPIDFD, not pidfd_open(SO_PEERCRED.pid): a dead connector may have
    // passed its socket to a live child, masking hangup while its PID is reused.
    if pidfd_pid(&peer_pidfd)? != peer.pid || !pidfd_alive(&peer_pidfd) {
        return None;
    }
    let mut pins = vec![ProcessPin::open(peer.pid, peer_pidfd)?];
    let mut chain = vec![pins[0].snapshot()?];
    if chain[0].uids != [peer.uid; 4] {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let expected_exe = inode(&trusted_file(&executable, false)?.metadata().ok()?);
    let bridge = peer_bridge(&pins[0], expected_exe)?;
    for _ in 0..MAX_PARENTS {
        let child = chain.last()?;
        let pid = child.ppid;
        if pid <= 1 || pid == std::process::id() || chain.iter().any(|p| p.pid == pid) {
            return None;
        }
        let pin = ProcessPin::open(pid, process_pidfd(pid)?)?;
        let parent = pin.snapshot()?;
        if parent.start > child.start {
            return None;
        }
        pins.push(pin);
        chain.push(parent);
        if chain.last()?.uids == [0; 4] {
            break;
        }
        if chain.last()?.uids != [peer.uid; 4] {
            return None;
        }
    }
    if !chain_valid(&chain, peer, std::process::id(), bridge) {
        return None;
    }
    let privileged = chain.last()?;
    let clock = JournalClock::read()?;
    let user = login_name(peer.uid)?;
    trusted_file(Path::new(SSHD), false)?;
    // Read only pinned, root-controlled LOCAL SYSTEM journal files. Default
    // journal discovery must not admit a user-writable or imported remote file
    // whose prebuilt underscore-prefixed fields merely claim UID 0.
    let journal_files = trusted_system_journals(&clock.machine)?;
    let mut journal_args = vec![
        "--no-pager".into(),
        "--quiet".into(),
        "--output=json".into(),
        "--lines=64".into(),
        format!("--boot={}", clock.boot),
        format!("_PID={}", privileged.pid),
        "_UID=0".into(),
        "_COMM=sshd".into(),
    ];
    // Parent-owned fds stay open while journalctl runs. Its own CLOEXEC fds are
    // unrelated; /proc/<server>/fd names the checked, pinned journal inodes.
    journal_args.extend(journal_files.iter().map(|file| {
        format!(
            "--file=/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        )
    }));
    let journal = run_trusted_command(JOURNALCTL, &journal_args)?;
    let accepted = accepted_auth(&journal, privileged, &chain[0], &user, &clock)?;
    trusted_socket(Path::new(TAILSCALE_SOCKET))?;
    let whois = run_trusted_command(
        TAILSCALE,
        &[
            format!("--socket={TAILSCALE_SOCKET}"),
            "whois".into(),
            "--json".into(),
            accepted.ip.to_string(),
        ],
    )?;
    // Revalidate after all external work: same births, edges, credentials and
    // live handles; neither reparenting nor PID reuse can borrow old evidence.
    let after = pins
        .iter()
        .map(ProcessPin::snapshot)
        .collect::<Option<Vec<_>>>()?;
    if peer_credentials(fd)? != peer {
        return None;
    }
    principal_from_evidence(
        &map,
        RemoteEvidence {
            peer,
            server_pid: std::process::id(),
            before: &chain,
            after: &after,
            bridge_before: bridge,
            bridge_after: peer_bridge(&pins[0], expected_exe)?,
            journal: &journal,
            whois: &whois,
            login: &user,
            clock: &clock,
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Process {
    pid: u32,
    ppid: u32,
    start: u64,
    uids: [u32; 4],
}

fn chain_valid(chain: &[Process], peer: Peer, server_pid: u32, bridge: bool) -> bool {
    bridge
        && (2..=MAX_PARENTS + 1).contains(&chain.len())
        && chain[0].pid == peer.pid
        && chain[0].uids == [peer.uid; 4]
        && peer.uid != 0
        && chain.last().is_some_and(|p| p.uids == [0; 4])
        && chain
            .iter()
            .all(|p| p.pid > 1 && p.pid != server_pid && p.start > 0)
        && chain[..chain.len() - 1]
            .iter()
            .all(|p| p.uids == [peer.uid; 4])
        && chain
            .windows(2)
            .all(|pair| pair[0].ppid == pair[1].pid && pair[1].start <= pair[0].start)
        && chain.iter().map(|p| p.pid).collect::<HashSet<_>>().len() == chain.len()
}

fn evidence_unchanged(before: &[Process], after: &[Process]) -> bool {
    !before.is_empty() && before == after
}

// Private deterministic resolver seam: production can supply it only after
// socket/proc/config/command custody checks; tests inject bounded evidence, never
// a runtime pathname override or an environment-derived claim.
#[derive(Clone, Copy)]
struct RemoteEvidence<'a> {
    peer: Peer,
    server_pid: u32,
    before: &'a [Process],
    after: &'a [Process],
    bridge_before: bool,
    bridge_after: bool,
    journal: &'a [u8],
    whois: &'a [u8],
    login: &'a str,
    clock: &'a JournalClock,
}

fn principal_from_evidence(map: &PrincipalMap, evidence: RemoteEvidence<'_>) -> Option<Principal> {
    if !evidence.bridge_after
        || !chain_valid(
            evidence.before,
            evidence.peer,
            evidence.server_pid,
            evidence.bridge_before,
        )
        || !evidence_unchanged(evidence.before, evidence.after)
    {
        return None;
    }
    let auth = accepted_auth(
        evidence.journal,
        evidence.before.last()?,
        &evidence.before[0],
        evidence.login,
        evidence.clock,
    )?;
    let node = whois_node(evidence.whois, auth.ip)?;
    map.lookup(&auth.fingerprint, &node)
}

struct ProcessPin {
    pid: u32,
    directory: File,
    pidfd: OwnedFd,
}

impl ProcessPin {
    fn open(pid: u32, pidfd: OwnedFd) -> Option<Self> {
        if pid == 0 || pid > i32::MAX as u32 || pidfd_pid(&pidfd)? != pid || !pidfd_alive(&pidfd) {
            return None;
        }
        let path = CString::new(format!("/proc/{pid}")).ok()?;
        // SAFETY: NUL-terminated path and valid flags; owns new fd on success.
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return None;
        }
        // SAFETY: open returned an unowned fd.
        let directory = unsafe { File::from_raw_fd(raw) };
        let pin = Self {
            pid,
            directory,
            pidfd,
        };
        pin.snapshot()?;
        Some(pin)
    }

    fn snapshot(&self) -> Option<Process> {
        if !pidfd_alive(&self.pidfd) {
            return None;
        }
        let stat = read_at(&self.directory, "stat", 8192)?;
        let status = read_at(&self.directory, "status", 16384)?;
        let again = read_at(&self.directory, "stat", 8192)?;
        let (ppid, start) = proc_stat(&stat, self.pid)?;
        if proc_stat(&again, self.pid)? != (ppid, start) || !pidfd_alive(&self.pidfd) {
            return None;
        }
        let text = std::str::from_utf8(&status).ok()?;
        let uids: Vec<u32> = text
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        Some(Process {
            pid: self.pid,
            ppid,
            start,
            uids: uids.try_into().ok()?,
        })
    }
}

fn proc_stat(bytes: &[u8], pid: u32) -> Option<(u32, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.split_once(' ')?.0.parse::<u32>().ok()? != pid {
        return None;
    }
    let (_, tail) = text.rsplit_once(')')?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    if !matches!(*fields.first()?, "R" | "S" | "D" | "T" | "t" | "I" | "P") {
        return None;
    }
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields
        .get(19)?
        .parse::<u64>()
        .ok()
        .filter(|start| *start > 0)?;
    Some((ppid, start))
}

fn peer_bridge(pin: &ProcessPin, expected: (u64, u64)) -> Option<bool> {
    // Opening proc exe follows the kernel magic link, not an untrusted pathname.
    let exe = CString::new("exe").ok()?;
    // SAFETY: directory fd and C string are valid; owns new fd on success.
    let raw = unsafe {
        libc::openat(
            pin.directory.as_raw_fd(),
            exe.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: openat returned an unowned fd.
    let executable = unsafe { File::from_raw_fd(raw) };
    let metadata = executable.metadata().ok()?;
    if !safe_metadata(&metadata, false) || inode(&metadata) != expected {
        return Some(false);
    }
    let cmdline = read_at(&pin.directory, "cmdline", 8192)?;
    // Argv is merely a restrictive check AFTER trusted executable+kernel custody.
    // A name/argv forgery without those facts can never promote identity.
    Some(
        cmdline
            .split(|b| *b == 0)
            .skip(1)
            .any(|arg| arg == b"remote-client-bridge"),
    )
}

pub(super) fn socket_peer_pidfd(fd: RawFd) -> Option<OwnedFd> {
    let mut raw: RawFd = -1;
    let mut len = std::mem::size_of::<RawFd>() as libc::socklen_t;
    // SAFETY: writable exact-size output; successful call allocates a new fd.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw as *mut RawFd).cast(),
            &mut len,
        )
    };
    if status != 0 || raw < 0 {
        return None;
    }
    // SAFETY: successful getsockopt returned an unowned fd (also close on error).
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    (len == std::mem::size_of::<RawFd>() as libc::socklen_t).then_some(owned)
}

fn process_pidfd(pid: u32) -> Option<OwnedFd> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    // SAFETY: pidfd_open with flags 0 returns a new descriptor or error.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if raw < 0 {
        return None;
    }
    // SAFETY: successful pidfd_open returned an unowned fd.
    Some(unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
}

pub(super) fn pidfd_alive(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a live writable pollfd, count 1, no blocking.
    unsafe { libc::poll(&mut poll, 1, 0) == 0 }
}

pub(super) fn pidfd_pid(fd: &OwnedFd) -> Option<u32> {
    let bytes = read_limited(
        File::open(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).ok()?,
        8192,
    )?;
    std::str::from_utf8(&bytes)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

fn read_at(directory: &File, name: &str, limit: usize) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    // SAFETY: valid directory descriptor and NUL-terminated name.
    let raw = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: successful openat returned an unowned fd.
    read_limited(unsafe { File::from_raw_fd(raw) }, limit)
}

fn read_limited(file: File, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn inode(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

fn safe_metadata(meta: &std::fs::Metadata, directory: bool) -> bool {
    meta.uid() == 0
        && meta.mode() & 0o022 == 0
        && if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        }
}

/// Anchor every component to its predecessor. A symlink or writable directory
/// cannot redirect the later open; metadata is from the actual opened inode.
fn trusted_file(path: &Path, directory: bool) -> Option<File> {
    let mut components = path.components();
    if components.next()? != Component::RootDir {
        return None;
    }
    let mut current = File::open("/").ok()?;
    if !safe_metadata(&current.metadata().ok()?, true) {
        return None;
    }
    let names: Vec<_> = components.collect();
    if names.is_empty() {
        return directory.then_some(current);
    }
    for (index, component) in names.iter().enumerate() {
        let Component::Normal(name) = component else {
            return None;
        };
        let is_dir = index + 1 < names.len() || directory;
        let name = CString::new(name.as_bytes()).ok()?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if is_dir { libc::O_DIRECTORY } else { 0 };
        // SAFETY: anchored valid directory fd and C string; new fd on success.
        let raw = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            return None;
        }
        // SAFETY: successful openat returned an unowned fd.
        let next = unsafe { File::from_raw_fd(raw) };
        if !safe_metadata(&next.metadata().ok()?, is_dir) {
            return None;
        }
        current = next;
    }
    Some(current)
}

fn trusted_system_journals(machine: &str) -> Option<Vec<File>> {
    let mut files = Vec::new();
    let mut entries_seen = 0;
    for base in ["/run/log/journal", "/var/log/journal"] {
        let path = Path::new(base).join(machine);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
            Ok(_) => {}
        }
        let directory = trusted_file(&path, true)?;
        // read_dir traverses the pinned directory, not a fresh mutable pathname.
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())).ok()? {
            entries_seen += 1;
            if entries_seen > 512 {
                return None;
            }
            let name = entry.ok()?.file_name();
            let bytes = name.as_bytes();
            if !(bytes == b"system.journal"
                || (bytes.starts_with(b"system@") && bytes.ends_with(b".journal")))
            {
                continue;
            }
            let name = CString::new(bytes).ok()?;
            // SAFETY: valid pinned directory fd and component, new fd on success.
            let raw = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return None;
            }
            // SAFETY: successful openat returned an unowned fd.
            let file = unsafe { File::from_raw_fd(raw) };
            if !safe_metadata(&file.metadata().ok()?, false) {
                return None;
            }
            files.push(file);
            if files.len() > 64 {
                return None;
            }
        }
    }
    (!files.is_empty()).then_some(files)
}

fn trusted_socket(path: &Path) -> Option<()> {
    use std::os::unix::fs::FileTypeExt;
    let parent = trusted_file(path.parent()?, true)?;
    let name = CString::new(path.file_name()?.as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: valid dir/name and writable stat output; NOFOLLOW rejects symlinks.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return None;
    }
    // SAFETY: fstatat initialized stat.
    let stat = unsafe { stat.assume_init() };
    if stat.st_uid != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return None;
    }
    // Socket write permissions allow connecting, not replacing it; the trusted
    // parent controls pathname custody. No requirement to remove daemon 0666.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_socket() || metadata.uid() != 0 || metadata.ino() != stat.st_ino {
        return None;
    }
    // A root-owned socket inode alone is not a root-owned daemon: an installer
    // could have chowned a socket served by a non-root process. Verify the live
    // kernel peer too. Use nonblocking connect so a full backlog fails closed.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return None;
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    // SAFETY: socket creates a new descriptor with fixed family/type/flags.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: successful socket returned an unowned fd.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: initialized, NUL-terminated sockaddr_un and live socket fd.
    if unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    } != 0
    {
        return None;
    }
    let peer = peer_credentials(socket.as_raw_fd())?;
    if peer.uid != 0 {
        return None;
    }
    let pin = ProcessPin::open(peer.pid, socket_peer_pidfd(socket.as_raw_fd())?)?;
    (pin.snapshot()?.uids == [0; 4]).then_some(())
}

fn run_trusted_command(path: &str, args: &[String]) -> Option<Vec<u8>> {
    let executable = trusted_file(Path::new(path), false)?;
    if executable.metadata().ok()?.mode() & 0o111 == 0 {
        return None;
    }
    // Execute the checked inode, not a fresh path lookup. Linux opens the proc
    // magic link before applying CLOEXEC. No shell and no inherited loader env.
    let mut child = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let result = (|| {
        let mut stdout = child.stdout.take()?;
        // SAFETY: fcntl gets/sets flags on this owned pipe descriptor.
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return None;
        }
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut output = Vec::new();
        let mut status = None;
        loop {
            let mut buffer = [0u8; 8192];
            match stdout.read(&mut buffer) {
                Ok(0) if status.is_some() => {
                    return status
                        .filter(|s: &std::process::ExitStatus| s.success())
                        .map(|_| output)
                }
                Ok(n) => {
                    output.extend_from_slice(&buffer[..n]);
                    if output.len() > COMMAND_LIMIT {
                        return None;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
            if status.is_none() {
                status = child.try_wait().ok()?;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    // Timeouts/output caps must not leave child processes or pipe readers behind.
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn login_name(uid: u32) -> Option<String> {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0u8; 16384];
    // SAFETY: caller-owned passwd/storage/result buffers for getpwuid_r.
    if unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    } != 0
        || result.is_null()
    {
        return None;
    }
    // SAFETY: successful getpwuid_r initialized record; pw_name points into buffer.
    let name = unsafe { std::ffi::CStr::from_ptr(record.assume_init().pw_name) }
        .to_str()
        .ok()?;
    (!name.is_empty() && !name.bytes().any(|b| b.is_ascii_whitespace())).then(|| name.to_string())
}

struct JournalClock {
    boot: String,
    machine: String,
    ticks_per_second: u64,
}

impl JournalClock {
    fn read() -> Option<Self> {
        let boot = read_limited(File::open("/proc/sys/kernel/random/boot_id").ok()?, 64)?;
        let boot = std::str::from_utf8(&boot).ok()?.trim().replace('-', "");
        if boot.len() != 32 || !boot.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let machine = read_limited(trusted_file(Path::new("/etc/machine-id"), false)?, 64)?;
        let machine = std::str::from_utf8(&machine).ok()?.trim().to_string();
        if machine.len() != 32 || !machine.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        // proc starttime uses BOOTTIME; journal uses MONOTONIC. After any system
        // suspend their origins diverge. Refuse rather than guess a historical
        // conversion and admit a previous occupant of the root sshd PID.
        let read_clock = |clock| {
            let mut ts = std::mem::MaybeUninit::<libc::timespec>::zeroed();
            // SAFETY: writable timespec for a supported kernel clock.
            if unsafe { libc::clock_gettime(clock, ts.as_mut_ptr()) } != 0 {
                return None;
            }
            // SAFETY: clock_gettime initialized ts.
            let ts = unsafe { ts.assume_init() };
            Some(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128)
        };
        if (read_clock(libc::CLOCK_BOOTTIME)? - read_clock(libc::CLOCK_MONOTONIC)?).abs()
            > 10_000_000
        {
            return None;
        }
        // SAFETY: sysconf with a constant selector has no memory arguments.
        let ticks_per_second = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
            .ok()
            .filter(|hz| *hz > 0)?;
        Some(Self {
            boot,
            machine,
            ticks_per_second,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AcceptedAuth {
    ip: IpAddr,
    fingerprint: String,
}

fn accepted_auth(
    bytes: &[u8],
    privileged: &Process,
    peer: &Process,
    user: &str,
    clock: &JournalClock,
) -> Option<AcceptedAuth> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut accepted = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).ok()?;
        let message = record.get("MESSAGE")?.as_str()?;
        if !message.starts_with("Accepted publickey ") {
            continue;
        }
        // Underscore-prefixed fields are supplied by journald, not by the sender.
        // Verify them even though journalctl also filters them. Never trust a
        // flattened text line or SYSLOG_PID/identifier instead of these fields.
        if record.get("_UID")?.as_str()? != "0"
            || record.get("_PID")?.as_str()?.parse::<u32>().ok()? != privileged.pid
            || record.get("_COMM")?.as_str()? != "sshd"
            || record.get("_EXE")?.as_str()? != SSHD
            || record.get("_BOOT_ID")?.as_str()? != clock.boot
            || record.get("_MACHINE_ID")?.as_str()? != clock.machine
        {
            return None;
        }
        let timestamp = record
            .get("__MONOTONIC_TIMESTAMP")?
            .as_str()?
            .parse::<u64>()
            .ok()?;
        // proc births are tick-truncated: refuse events in the privileged
        // process's first tick. Otherwise an earlier root sshd reusing the same
        // numeric PID within that tick could lend its Accepted event. This can
        // reject very fast authentication, but never borrows a prior occupant.
        // The upper bound includes one tick for the peer's truncated birth.
        let stamp = u128::from(timestamp) * u128::from(clock.ticks_per_second);
        if stamp < (u128::from(privileged.start) + 1) * 1_000_000
            || stamp >= (u128::from(peer.start) + 1) * 1_000_000
        {
            return None;
        }
        let auth = parse_accepted(message, user)?;
        if accepted.replace(auth).is_some() {
            return None;
        }
    }
    accepted
}

fn parse_accepted(message: &str, user: &str) -> Option<AcceptedAuth> {
    let words: Vec<_> = message.split_whitespace().collect();
    if words.len() != 11
        || words[..4] != ["Accepted", "publickey", "for", user]
        || words[4] != "from"
        || words[6] != "port"
        || words[8] != "ssh2:"
        || words[7]
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .is_none()
        || !matches!(
            words[9],
            "ED25519" | "RSA" | "ECDSA" | "ECDSA-SK" | "ED25519-SK"
        )
        || !fingerprint_valid(words[10])
    {
        return None;
    }
    let ip = words[5].parse::<IpAddr>().ok()?;
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    Some(AcceptedAuth {
        ip,
        fingerprint: words[10].to_string(),
    })
}

fn whois_node(bytes: &[u8], ip: IpAddr) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let node = value.get("Node")?.as_object()?;
    let stable = node.get("StableID")?.as_str()?;
    if stable.is_empty() || stable.len() > 128 {
        return None;
    }
    if !node.get("Addresses")?.as_array()?.iter().any(|address| {
        address
            .as_str()
            .and_then(|value| value.split_once('/'))
            .and_then(|(address, prefix)| {
                address
                    .parse::<IpAddr>()
                    .ok()
                    .map(|address| (address, prefix))
            })
            .is_some_and(|(address, prefix)| {
                address == ip && prefix == if ip.is_ipv4() { "32" } else { "128" }
            })
    }) {
        return None;
    }
    // Authorize only the opaque stable ID, not its mutable friendly name.
    Some(stable.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    fn map() -> PrincipalMap {
        PrincipalMap::parse(format!(r#"{{"version":1,"principals":[{{"id":"paul","name":"Paul","sshKeys":["{KEY}"],"tailscaleNodes":["nProof"]}}]}}"#).as_bytes()).unwrap()
    }
    fn processes() -> Vec<Process> {
        vec![
            Process {
                pid: 500,
                ppid: 400,
                start: 300,
                uids: [1000; 4],
            },
            Process {
                pid: 400,
                ppid: 300,
                start: 200,
                uids: [1000; 4],
            },
            Process {
                pid: 300,
                ppid: 2,
                start: 100,
                uids: [0; 4],
            },
        ]
    }
    fn journal() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"_UID":"0", "_PID":"300", "_COMM":"sshd", "_EXE":SSHD,
            "_BOOT_ID":"boot", "_MACHINE_ID":"machine", "__MONOTONIC_TIMESTAMP":"2500000",
            "MESSAGE":format!("Accepted publickey for paul from 100.64.0.9 port 34567 ssh2: ED25519 {KEY}")})).unwrap()
    }
    fn clock() -> JournalClock {
        JournalClock {
            boot: "boot".into(),
            machine: "machine".into(),
            ticks_per_second: 100,
        }
    }

    #[test]
    fn authentic_journal_and_node_require_same_mapped_principal() {
        let chain = processes();
        assert!(chain_valid(
            &chain,
            Peer {
                pid: 500,
                uid: 1000
            },
            999,
            true
        ));
        let auth = accepted_auth(&journal(), &chain[2], &chain[0], "paul", &clock())
            .expect("authentic root sshd event");
        let node = whois_node(br#"{"Node":{"StableID":"nProof","Addresses":["100.64.0.9/32"],"Name":"not-authority"}}"#, auth.ip).unwrap();
        assert_eq!(
            map().lookup(&auth.fingerprint, &node),
            Some(Principal {
                id: "paul".into(),
                name: "Paul".into()
            })
        );
    }

    // Test-only alternate map path: production always uses MAP_PATH and has no
    // configurable pathname. The real descriptor/ownership gate still runs.
    fn principal_from_test_file(path: &Path, evidence: RemoteEvidence<'_>) -> Option<Principal> {
        let map = PrincipalMap::parse(&read_limited(trusted_file(path, false)?, MAP_LIMIT)?)?;
        principal_from_evidence(&map, evidence)
    }

    #[test]
    fn complete_resolver_seam_promotes_only_stable_authentic_custody() {
        let before = processes();
        let journal = journal();
        let clock = clock();
        let whois = br#"{"Node":{"StableID":"nProof","Addresses":["100.64.0.9/32"]}}"#;
        let evidence = RemoteEvidence {
            peer: Peer {
                pid: 500,
                uid: 1000,
            },
            server_pid: 999,
            before: &before,
            after: &before,
            bridge_before: true,
            bridge_after: true,
            journal: &journal,
            whois,
            login: "paul",
            clock: &clock,
        };
        assert_eq!(
            principal_from_evidence(&map(), evidence),
            Some(Principal {
                id: "paul".into(),
                name: "Paul".into()
            })
        );
        let mut changed = before.clone();
        changed[0].start += 1;
        let mut forged_record: Value = serde_json::from_slice(&journal).unwrap();
        forged_record["_UID"] = "1000".into();
        let forged_journal = serde_json::to_vec(&forged_record).unwrap();
        let wrong_key = String::from_utf8(journal.clone())
            .unwrap()
            .replace(KEY, "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
        for mutation in 0..10 {
            let mut bad = evidence;
            match mutation {
                0 => bad.bridge_before = false,
                1 => bad.bridge_after = false,
                2 => bad.after = &changed,
                3 => bad.server_pid = 400,
                4 => bad.peer.uid = 0,
                5 => bad.journal = &forged_journal,
                6 => bad.whois = br#"{"Node":{"StableID":"other","Addresses":["100.64.0.9/32"]}}"#,
                7 => bad.login = "kate",
                8 => bad.after = &[],
                9 => bad.journal = wrong_key.as_bytes(),
                _ => unreachable!(),
            }
            assert_eq!(
                principal_from_evidence(&map(), bad),
                None,
                "mutation {mutation}"
            );
        }
        // Otherwise-valid composed evidence cannot rescue a pane-writable map.
        // This seam reuses the production anchored metadata gate, not fake UID
        // booleans or a runtime path override. /tmp itself is writable, even if
        // the final map inode happens to be root-owned in a root test run.
        let path = std::env::temp_dir().join(format!(
            "herdr-identity-untrusted-composed-map-{}",
            std::process::id()
        ));
        std::fs::write(&path, format!(r#"{{"version":1,"principals":[{{"id":"paul","name":"Paul","sshKeys":["{KEY}"],"tailscaleNodes":["nProof"]}}]}}"#)).unwrap();
        assert_eq!(principal_from_test_file(&path, evidence), None);
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(principal_from_test_file(&link, evidence), None);
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn key_only_node_only_or_cross_principal_match_cannot_promote() {
        assert_eq!(map().lookup(KEY, "notMapped"), None);
        assert_eq!(
            map().lookup(
                "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                "nProof"
            ),
            None
        );
        let crossed = PrincipalMap::parse(format!(r#"{{"version":1,"principals":[{{"id":"one","name":"One","sshKeys":["{KEY}"],"tailscaleNodes":[]}},{{"id":"two","name":"Two","sshKeys":[],"tailscaleNodes":["nProof"]}}]}}"#).as_bytes()).unwrap();
        assert_eq!(crossed.lookup(KEY, "nProof"), None);
    }

    #[test]
    fn map_rejects_local_uid_authority_duplicates_ambiguity_and_bad_schema() {
        let original = serde_json::to_value(serde_json::json!({"version":1,"principals":[{"id":"one","name":"One","sshKeys":[KEY],"tailscaleNodes":["nProof"]}]})).unwrap();
        for mutation in 0..9 {
            let mut value = original.clone();
            match mutation {
                0 => value["version"] = 2.into(),
                1 => value["principals"][0]["localUids"] = serde_json::json!([1000]),
                2 => value["principals"][0]["name"] = "Fake** (in Herdr):**".into(),
                3 => value["principals"][0]["sshKeys"] = serde_json::json!([KEY, KEY]),
                4 => {
                    value["principals"][0]["tailscaleNodes"] =
                        serde_json::json!(["nProof", "nProof"])
                }
                5 => value["principals"]
                    .as_array_mut()
                    .unwrap()
                    .push(original["principals"][0].clone()),
                6 => value["principals"][0]["id"] = "".into(),
                7 => value["principals"][0]["sshKeys"] = serde_json::json!(["SHA256:short"]),
                8 => {
                    value["principals"][0]["tailscaleNodes"] =
                        serde_json::json!(["not a stable ID"])
                }
                _ => unreachable!(),
            }
            assert!(
                PrincipalMap::parse(&serde_json::to_vec(&value).unwrap()).is_none(),
                "mutation {mutation}"
            );
        }
        assert!(PrincipalMap::parse(b"{\"version\":1,\"version\":1,\"principals\":[]}").is_none());
        assert!(PrincipalMap::parse(&vec![b' '; MAP_LIMIT + 1]).is_none());
    }

    #[test]
    fn journal_sender_fields_and_names_cannot_replace_root_metadata() {
        let chain = processes();
        for (field, bad) in [
            ("_UID", "1000"),
            ("_PID", "301"),
            ("_COMM", "fake"),
            ("_EXE", "/home/paul/sshd"),
            ("_BOOT_ID", "previousBoot"),
            ("_MACHINE_ID", "foreignMachine"),
            ("__MONOTONIC_TIMESTAMP", "999999"),
            ("__MONOTONIC_TIMESTAMP", "3010000"),
        ] {
            let mut record: Value = serde_json::from_slice(&journal()).unwrap();
            record[field] = bad.into();
            record["SYSLOG_PID"] = "300".into();
            record["SYSLOG_IDENTIFIER"] = "sshd".into();
            assert!(
                accepted_auth(
                    &serde_json::to_vec(&record).unwrap(),
                    &chain[2],
                    &chain[0],
                    "paul",
                    &clock()
                )
                .is_none(),
                "{field}={bad}"
            );
        }
        let mut record: Value = serde_json::from_slice(&journal()).unwrap();
        record.as_object_mut().unwrap().remove("_UID");
        assert!(accepted_auth(
            &serde_json::to_vec(&record).unwrap(),
            &chain[2],
            &chain[0],
            "paul",
            &clock()
        )
        .is_none());
    }

    #[test]
    fn stale_ambiguous_or_different_login_journal_event_is_refused() {
        let chain = processes();
        let mut twice = journal();
        twice.push(b'\n');
        twice.extend_from_slice(&journal());
        assert!(accepted_auth(&twice, &chain[2], &chain[0], "paul", &clock()).is_none());
        assert!(accepted_auth(&journal(), &chain[2], &chain[0], "kate", &clock()).is_none());
        let mut reused = chain[2].clone();
        reused.start = 260;
        assert!(accepted_auth(&journal(), &reused, &chain[0], "paul", &clock()).is_none());
        // Same-tick PID reuse: journal is exactly in the new process's truncated
        // birth tick. A >= starttime-only check would wrongly accept this.
        reused.start = 250;
        assert!(accepted_auth(&journal(), &reused, &chain[0], "paul", &clock()).is_none());
        for message in [
            format!("Accepted password for paul from 100.64.0.9 port 22 ssh2: ED25519 {KEY}"),
            format!("Accepted publickey for paul from 100.64.0.9 port 0 ssh2: ED25519 {KEY}"),
            format!("Accepted publickey for paul from 127.0.0.1 port 22 ssh2: ED25519 {KEY}"),
            format!("Accepted publickey for paul from 100.64.0.9 port 22 ssh2: ED25519 {KEY} injected"),
            format!("env SSH_CONNECTION=100.64.0.9; Accepted publickey for paul from 100.64.0.9 port 22 ssh2: ED25519 {KEY}"),
        ] { assert!(parse_accepted(&message, "paul").is_none()); }
    }

    #[test]
    fn node_display_names_wrong_ip_and_unstable_ids_are_not_authority() {
        let ip = "100.64.0.9".parse().unwrap();
        for bytes in [
            r#"{"Node":{"Name":"nProof","Addresses":["100.64.0.9/32"]}}"#,
            r#"{"Node":{"ID":"nProof","Addresses":["100.64.0.9/32"]}}"#,
            r#"{"Node":{"StableID":"nProof","Addresses":["100.64.0.10/32"]}}"#,
            r#"{"Node":{"StableID":"nProof","Addresses":["100.64.0.9/128"]}}"#,
            r#"{"Node":{"StableID":"nProof","Addresses":["100.64.0.0/24"]}}"#,
            r#"{"Node":{"StableID":"","Addresses":["100.64.0.9/32"]}}"#,
        ] {
            assert_eq!(whois_node(bytes.as_bytes(), ip), None);
        }
        assert_eq!(
            whois_node(
                br#"{"Node":{"StableID":"nProof","Addresses":["fd7a:115c:a1e0::9/128"]}}"#,
                "fd7a:115c:a1e0::9".parse().unwrap()
            ),
            Some("nProof".into())
        );
    }

    #[test]
    fn chain_refuses_server_ancestor_name_spoof_wrong_uid_cycles_and_reused_parent() {
        let chain = processes();
        let peer = Peer {
            pid: 500,
            uid: 1000,
        };
        assert!(!chain_valid(&chain, peer, 400, true));
        assert!(!chain_valid(&chain, peer, 999, false)); // forged argv/name without trusted executable
        assert!(!chain_valid(
            &chain,
            Peer {
                pid: 501,
                uid: 1000
            },
            999,
            true
        ));
        assert!(!chain_valid(
            &chain,
            Peer {
                pid: 500,
                uid: 2000
            },
            999,
            true
        ));
        for mutation in 0..5 {
            let mut bad = chain.clone();
            match mutation {
                0 => bad[1].start = 301, // reused ancestor born after its supposed child
                1 => bad[1].uids[1] = 0, // unexplained credential change
                2 => bad[2].uids = [1000; 4], // no root custody
                3 => bad[1].pid = 500,   // cycle / missing edge
                4 => bad[0].ppid = 300,  // skipped, unauthenticated parent
                _ => unreachable!(),
            }
            assert!(!chain_valid(&bad, peer, 999, true), "mutation {mutation}");
        }
    }

    #[test]
    fn final_revalidation_detects_reuse_reparent_credentials_and_disappearance() {
        let chain = processes();
        assert!(evidence_unchanged(&chain, &chain));
        for mutation in 0..4 {
            let mut after = chain.clone();
            match mutation {
                0 => after[0].start += 1,
                1 => after[1].ppid = 2,
                2 => after[2].uids[1] = 1000,
                3 => {
                    after.pop();
                }
                _ => unreachable!(),
            }
            assert!(!evidence_unchanged(&chain, &after));
        }
        assert!(!evidence_unchanged(&[], &[]));
    }

    #[test]
    fn proc_parser_uses_kernel_birth_and_edges_not_parenthesized_names() {
        let fields = (0..17).map(|_| "0").collect::<Vec<_>>().join(" ");
        let stat = format!("500 (sshd ) [priv] forged) S 400 {fields} 300 0");
        assert_eq!(proc_stat(stat.as_bytes(), 500), Some((400, 300)));
        assert_eq!(proc_stat(stat.as_bytes(), 501), None);
        assert_eq!(proc_stat(stat.replace(") S", ") Z").as_bytes(), 500), None);
        assert_eq!(proc_stat(b"500 (truncated) S 400", 500), None);
    }

    #[test]
    fn actual_root_files_are_trusted_but_writable_ancestors_and_symlinks_are_not() {
        assert!(trusted_file(Path::new(JOURNALCTL), false).is_some());
        assert!(trusted_file(Path::new("/etc/passwd"), false).is_some());
        assert!(trusted_file(Path::new("/tmp"), true).is_none()); // root-owned, world writable
        assert!(trusted_file(Path::new("/dev/stdin"), false).is_none()); // kernel symlink, no-follow
        assert!(trusted_file(Path::new("etc/passwd"), false).is_none());
        assert!(trusted_file(Path::new("/etc/../etc/passwd"), false).is_none());
        let file =
            std::env::temp_dir().join(format!("herdr-identity-user-map-{}", std::process::id()));
        std::fs::write(&file, b"{}").unwrap();
        assert!(trusted_file(&file, false).is_none());
        let link = file.with_extension("link");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
        assert!(trusted_file(&link, false).is_none());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn real_socket_credentials_atomic_handle_and_proc_incarnation_agree() {
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = peer_credentials(server.as_raw_fd()).unwrap();
        assert_eq!(peer.pid, std::process::id());
        // SAFETY: getuid has no memory arguments.
        assert_eq!(peer.uid, unsafe { libc::getuid() });
        let fd = socket_peer_pidfd(server.as_raw_fd())
            .expect("Linux >=6.5 exposes atomic socket peer handles");
        assert_eq!(pidfd_pid(&fd), Some(peer.pid));
        let pin = ProcessPin::open(peer.pid, fd).unwrap();
        let before = pin.snapshot().unwrap();
        assert_eq!(pin.snapshot(), Some(before));
        assert!(peer_bridge(&pin, (0, 0)).is_none_or(|bridge| !bridge));
        assert_eq!(
            resolve_fd(server.as_raw_fd()).principal,
            None,
            "local sockets never map shared UID"
        );
        drop(client);
    }

    #[test]
    fn dead_connector_with_inherited_socket_cannot_borrow_a_reused_pid() {
        use std::io::{Read as _, Write as _};
        let path =
            std::env::temp_dir().join(format!("herdr-identity-peer-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (dst, src) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
            *dst = *src as libc::c_char;
        }
        // Create the socket before fork but CONNECT in the child: credentials
        // name the child; the parent retains the same connected open file.
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0);
        let retained = unsafe { OwnedFd::from_raw_fd(raw) };
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // Only async-signal-safe syscalls after fork, no Rust allocation.
            unsafe {
                if libc::connect(
                    raw,
                    (&addr as *const libc::sockaddr_un).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                ) != 0
                {
                    libc::_exit(1);
                }
                // Even a process name containing a fake UID/status line cannot
                // replace the kernel credentials in status or SO_PEERCRED.
                libc::prctl(libc::PR_SET_NAME, c"sshd\nUid:\t0\n)".as_ptr(), 0, 0, 0);
                let mut byte = 1u8;
                libc::write(raw, (&byte as *const u8).cast(), 1);
                libc::read(raw, (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
        }
        let (mut stream, _) = listener.accept().unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        let peer = peer_credentials(stream.as_raw_fd()).unwrap();
        assert_eq!(peer.pid, child as u32);
        let pin =
            ProcessPin::open(peer.pid, socket_peer_pidfd(stream.as_raw_fd()).unwrap()).unwrap();
        assert_eq!(
            pin.snapshot().unwrap().uids,
            [peer.uid; 4],
            "forged process/status names do not change kernel UID"
        );
        stream.write_all(&[1]).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(!pidfd_alive(&pin.pidfd));
        assert!(
            pin.snapshot().is_none(),
            "pinned proc directory cannot become a later occupant"
        );
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLRDHUP,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 0) },
            0,
            "retained endpoint masks connector death"
        );
        assert!(socket_peer_pidfd(stream.as_raw_fd()).is_none_or(|fd| !pidfd_alive(&fd)));
        assert_eq!(resolve_fd(stream.as_raw_fd()).principal, None);
        drop(retained);
        drop(stream);
        drop(listener);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn trusted_command_uses_fixed_environment_and_bounded_receipts() {
        let output = run_trusted_command("/usr/bin/printenv", &["PATH".into()]).unwrap();
        assert_eq!(output, b"/usr/bin:/usr/sbin\n");
        assert!(run_trusted_command("/usr/bin/false", &[]).is_none());
        assert!(run_trusted_command("/usr/bin/sleep", &["3".into()]).is_none());
        assert!(run_trusted_command("/usr/bin/seq", &["1".into(), "1000000".into()]).is_none());
    }
}
