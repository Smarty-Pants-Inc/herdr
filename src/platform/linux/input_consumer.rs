//! Linux-only PTY consumer proof and semantic termios snapshots.
use crate::platform::{InputConsumerSnapshot, ProcessIdentity};
use std::{io, os::fd::RawFd};

fn refused(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}
fn process_fields(peer: ProcessIdentity) -> io::Result<(u32, u32, u32)> {
    if super::process_identity(peer.pid) != Some(peer) {
        return Err(refused("consumer_exited"));
    }
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", peer.pid))?;
    let fields = stat
        .rsplit_once(") ")
        .ok_or_else(|| refused("process_unavailable"))?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let number = |i: usize| -> io::Result<u32> {
        fields
            .get(i)
            .ok_or_else(|| refused("process_unavailable"))?
            .parse::<i64>()
            .map(|n| n as u32)
            .map_err(|_| refused("process_unavailable"))
    };
    let result = (number(2)?, number(3)?, number(4)?);
    if super::process_identity(peer.pid) != Some(peer) {
        return Err(refused("consumer_exited"));
    }
    Ok(result)
}
fn tty_device(fd: RawFd) -> io::Result<u32> {
    let mut index: libc::c_uint = 0;
    // SAFETY: ioctl writes one PTY index to the valid initialized output.
    if unsafe { libc::ioctl(fd, libc::TIOCGPTN, &mut index) } != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(format!("/dev/pts/{index}"))?.rdev();
    let major = libc::major(dev);
    let minor = libc::minor(dev);
    Ok((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
}
fn semantic_termios(fd: RawFd) -> io::Result<Vec<u64>> {
    let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: tcgetattr initializes termios on success; padding is never compared.
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let t = unsafe { t.assume_init() };
    if t.c_lflag & (libc::ICANON | libc::ECHO | libc::IEXTEN) != 0
        || t.c_iflag & (libc::ICRNL | libc::ISTRIP | libc::IXON) != 0
    {
        return Err(refused("termios_not_raw"));
    }
    let mut values = vec![
        t.c_iflag as u64,
        t.c_oflag as u64,
        t.c_cflag as u64,
        t.c_lflag as u64,
        t.c_line as u64,
    ];
    values.extend(t.c_cc.iter().map(|n| *n as u64));
    // SAFETY: accessors borrow the initialized termios.
    values.extend([
        unsafe { libc::cfgetispeed(&t) } as u64,
        unsafe { libc::cfgetospeed(&t) } as u64,
    ]);
    Ok(values)
}
fn identity_snapshot(fd: RawFd, peer: ProcessIdentity) -> io::Result<InputConsumerSnapshot> {
    let (pgid, sid, tty) = process_fields(peer)?;
    let fg = super::foreground_process_group_id_for_tty_fd(fd)
        .ok_or_else(|| refused("foreground_unavailable"))?;
    // SAFETY: tcgetsid only inspects the borrowed master descriptor.
    let pane_sid = unsafe { libc::tcgetsid(fd) };
    if pane_sid <= 0 || sid != pane_sid as u32 || pgid != fg || tty == 0 || tty != tty_device(fd)? {
        return Err(refused("not_foreground_consumer"));
    }
    let leader =
        super::process_identity(pgid).ok_or_else(|| refused("group_leader_unavailable"))?;
    // Open the pidfds first; the full proof below then pins them to the
    // checked generations, since a live pidfd blocks numeric PID reuse.
    let pidfds = match (pidfd_open(peer.pid), pidfd_open(leader.pid)) {
        (Some(p), Some(l)) => Some(std::sync::Arc::new([p, l])),
        _ => None,
    };
    let snapshot = InputConsumerSnapshot {
        peer,
        pgid,
        leader,
        sid,
        tty,
        termios: Vec::new(),
        pidfds,
    };
    if !full_alive(fd, &snapshot)
        || snapshot
            .pidfds
            .as_ref()
            .is_some_and(|p| !p.iter().all(running))
    {
        return Err(refused("consumer_changed"));
    }
    Ok(snapshot)
}
/// Check the same foreground incarnation even when its termios has changed.
/// An already-enrolled incarnation must never acquire a second capability.
pub(crate) fn incarnation(fd: RawFd, peer: ProcessIdentity) -> io::Result<ProcessIdentity> {
    Ok(identity_snapshot(fd, peer)?.leader)
}
pub(crate) fn snapshot(fd: RawFd, peer: ProcessIdentity) -> io::Result<InputConsumerSnapshot> {
    let mut snapshot = identity_snapshot(fd, peer)?;
    snapshot.termios = semantic_termios(fd)?;
    if !full_alive(fd, &snapshot) {
        return Err(refused("consumer_changed"));
    }
    Ok(snapshot)
}
fn pidfd_open(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open takes no pointers; success returns a new owned fd.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    // SAFETY: nothing else owns the descriptor pidfd_open just returned.
    (raw >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as RawFd) })
}
fn running(pidfd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    let mut event = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: zero-time poll borrows one initialized descriptor.
    unsafe { libc::poll(&mut event, 1, 0) == 0 }
}
/// Per-event liveness: called on every actor loop and flush, so it scales
/// with events x enrolled panes. With pidfds it uses only syscalls; the
/// /proc controlling-tty proof still runs at enroll and at every cut
/// (`unchanged`).
pub(crate) fn alive(fd: RawFd, s: &InputConsumerSnapshot) -> bool {
    let Some(pidfds) = s.pidfds.as_deref() else {
        return full_alive(fd, s);
    };
    let pid = s.peer.pid as libc::pid_t;
    // ponytail: a live pidfd pins its PID, so getpgid/getsid between two
    // running() checks observe the enrolled peer, not a reused PID.
    pidfds.iter().all(running)
        // SAFETY: getpgid/getsid/tcgetpgrp/tcgetsid take no pointers.
        && unsafe { libc::getpgid(pid) } == s.pgid as libc::pid_t
        && unsafe { libc::getsid(pid) } == s.sid as libc::pid_t
        && unsafe { libc::tcgetpgrp(fd) } == s.pgid as libc::pid_t
        && unsafe { libc::tcgetsid(fd) } == s.sid as libc::pid_t
        && pidfds.iter().all(running)
}
fn full_alive(fd: RawFd, s: &InputConsumerSnapshot) -> bool {
    process_fields(s.peer).ok()==Some((s.pgid,s.sid,s.tty))
        && super::process_identity(s.pgid)==Some(s.leader)
        && super::foreground_process_group_id_for_tty_fd(fd)==Some(s.pgid)
        // SAFETY: tcgetsid observes the borrowed master.
        && unsafe {libc::tcgetsid(fd)}==s.sid as i32
}
pub(crate) fn unchanged(fd: RawFd, s: &InputConsumerSnapshot) -> bool {
    full_alive(fd, s) && semantic_termios(fd).ok().as_ref() == Some(&s.termios)
}
pub(crate) fn random_bytes(bytes: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}
