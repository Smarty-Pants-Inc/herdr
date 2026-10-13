//! pidfs identity of a local socket peer for the input consumer (smarty-dev#2636 E′,
//! design "Round 2 answers" items 4 and 6). No pid or start-time fallback: any
//! missing kernel feature refuses with `peer_identity_unsupported`.
use crate::platform::PeerPidfd;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, OnceLock};

/// `PID_FS_MAGIC` (include/uapi/linux/magic.h).
const PID_FS_MAGIC: u64 = 0x5049_4446;
/// `_IO(PIDFS_IOCTL_MAGIC, 6)` (include/uapi/linux/pidfd.h).
const PIDFD_GET_PID_NAMESPACE: libc::c_ulong = 0xFF06;
/// `PIDFD_INFO_PID` request bit.
const PIDFD_INFO_PID: u64 = 1;

/// `struct pidfd_info`, `PIDFD_INFO_SIZE_VER0` (64 bytes, Linux 6.13).
#[repr(C)]
#[derive(Default)]
struct PidfdInfo {
    mask: u64,
    cgroupid: u64,
    pid: u32,
    tgid: u32,
    ppid: u32,
    ruid: u32,
    rgid: u32,
    euid: u32,
    egid: u32,
    suid: u32,
    sgid: u32,
    fsuid: u32,
    fsgid: u32,
    exit_code: i32,
}
const _: () = assert!(std::mem::size_of::<PidfdInfo>() == 64);
/// `_IOWR(PIDFS_IOCTL_MAGIC, 11, struct pidfd_info)`.
const PIDFD_GET_INFO: libc::c_ulong =
    (3 << 30) | ((std::mem::size_of::<PidfdInfo>() as libc::c_ulong) << 16) | (0xFF << 8) | 11;

pub(crate) const UNSUPPORTED: &str = "peer_identity_unsupported";

fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, UNSUPPORTED)
}

#[cfg(test)]
thread_local! {
    /// H8 test hook: make the next `SO_PEERPIDFD` on this thread fail with this errno.
    pub(crate) static FORCE_PEERPIDFD_ERRNO: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
}

/// `SO_PEERPIDFD` on a connected Unix socket, validated as a pidfs pidfd.
pub(crate) fn from_socket(socket: RawFd) -> io::Result<PeerPidfd> {
    #[cfg(test)]
    if let Some(errno) = FORCE_PEERPIDFD_ERRNO.with(|f| f.take()) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    let mut raw: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes one descriptor into the sized integer buffer.
    let status = unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut raw as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if raw < 0 || len != std::mem::size_of::<libc::c_int>() as libc::socklen_t {
        return Err(unsupported());
    }
    // SAFETY: successful SO_PEERPIDFD returns a new descriptor nothing else owns.
    validate(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Accept only a pidfs pidfd on a 64-bit build: before Linux 6.9 pidfds are anon
/// inodes that all share one inode number, so comparing them would accept anyone.
pub(crate) fn validate(pidfd: OwnedFd) -> io::Result<PeerPidfd> {
    if !cfg!(target_pointer_width = "64") {
        return Err(unsupported());
    }
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: fstatfs fills the initialized-size buffer for a borrowed descriptor.
    if unsafe { libc::fstatfs(pidfd.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(unsupported());
    }
    // SAFETY: fstatfs succeeded, so the buffer is initialized.
    #[allow(clippy::unnecessary_cast)] // f_type's width differs by architecture.
    if unsafe { fs.assume_init() }.f_type as u64 != PID_FS_MAGIC {
        return Err(unsupported());
    }
    let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: fstat fills the buffer for a borrowed descriptor.
    if unsafe { libc::fstat(pidfd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(unsupported());
    }
    // SAFETY: fstat succeeded, so the buffer is initialized.
    let ino = unsafe { st.assume_init() }.st_ino;
    let mut info = PidfdInfo {
        mask: PIDFD_INFO_PID,
        ..PidfdInfo::default()
    };
    // SAFETY: PIDFD_GET_INFO reads and writes exactly one sized pidfd_info.
    if unsafe { libc::ioctl(pidfd.as_raw_fd(), PIDFD_GET_INFO as _, &mut info) } != 0
        || info.mask & PIDFD_INFO_PID == 0
        || info.tgid == 0
    {
        return Err(unsupported());
    }
    Ok(PeerPidfd {
        ino,
        pid: info.tgid,
        fd: Arc::new(pidfd),
    })
}

/// The pidfd's process has not exited (a zero-time poll is not readable).
pub(crate) fn running(pidfd: &PeerPidfd) -> bool {
    let mut event = libc::pollfd {
        fd: pidfd.fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: zero-time poll borrows one initialized descriptor.
    unsafe { libc::poll(&mut event, 1, 0) == 0 }
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes no pointers; success returns a new owned fd.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: nothing else owns the descriptor pidfd_open just returned.
    Ok(unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
}

/// nsfs inode of this server's active pid namespace, the one `PIDFD_GET_INFO`
/// reports pids in, read through `PIDFD_GET_PID_NAMESPACE` on our own pidfd.
/// `None` (refuse) when that fails or `/proc/self/ns/pid` names another
/// namespace, because the admission proof reads `/proc/<pid>`.
pub(crate) fn server_pid_namespace() -> Option<u64> {
    static NAMESPACE: OnceLock<Option<u64>> = OnceLock::new();
    *NAMESPACE.get_or_init(|| {
        use std::os::unix::fs::MetadataExt;
        let own = pidfd_open(std::process::id()).ok()?;
        // SAFETY: the ioctl takes no argument and returns a new nsfs descriptor.
        let raw = unsafe { libc::ioctl(own.as_raw_fd(), PIDFD_GET_PID_NAMESPACE as _, 0) };
        if raw < 0 {
            return None;
        }
        // SAFETY: the ioctl returned a new descriptor nothing else owns.
        let ns = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        let ino = ns.metadata().ok()?.ino();
        (std::fs::metadata("/proc/self/ns/pid").ok()?.ino() == ino).then_some(ino)
    })
}

/// The peer's `/proc` identity, read while its pidfd shows it alive so the
/// number cannot be reused under the read.
pub(crate) fn identity(pidfd: &PeerPidfd) -> io::Result<crate::platform::ProcessIdentity> {
    if server_pid_namespace().is_none() {
        return Err(unsupported());
    }
    let exited = || io::Error::new(io::ErrorKind::PermissionDenied, "consumer_exited");
    if !running(pidfd) {
        return Err(exited());
    }
    let identity = super::process_identity(pidfd.pid).ok_or_else(exited)?;
    if !running(pidfd) {
        return Err(exited());
    }
    Ok(identity)
}

/// A validated pidfd for a live pid (tests stand in for a socket peer).
#[cfg(test)]
pub(crate) fn open_for_test(pid: u32) -> io::Result<PeerPidfd> {
    validate(pidfd_open(pid)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn input_consumer_socket_peer_pidfd_names_this_process() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let peer = from_socket(a.as_raw_fd()).expect("pidfs peer");
        assert_eq!(peer.pid, std::process::id());
        // Every pidfd for one process shares its pidfs inode.
        assert_eq!(
            open_for_test(std::process::id()).expect("own").ino,
            peer.ino
        );
        assert!(running(&peer));
        assert_eq!(identity(&peer).expect("identity").pid, std::process::id());
    }

    #[test]
    fn input_consumer_server_pid_namespace_matches_proc_self() {
        // H9: the signed pidns is the namespace PIDFD_GET_INFO reports in.
        let ns = server_pid_namespace().expect("pidns");
        assert_eq!(
            std::fs::metadata("/proc/self/ns/pid").expect("ns").ino(),
            ns
        );
    }

    #[test]
    fn input_consumer_peer_pidfd_fails_closed_without_pidfs() {
        // H8: SO_PEERPIDFD missing, then a descriptor that is not a pidfs pidfd.
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        FORCE_PEERPIDFD_ERRNO.with(|f| f.set(Some(libc::ENOPROTOOPT)));
        assert!(from_socket(a.as_raw_fd()).is_err());
        let file = std::fs::File::open("/proc/self/stat").expect("file");
        let err = validate(OwnedFd::from(file)).expect_err("not pidfs");
        assert_eq!(err.to_string(), UNSUPPORTED);
    }

    #[test]
    fn input_consumer_peer_pidfd_of_exited_process_is_not_running() {
        let mut child = std::process::Command::new("true").spawn().expect("child");
        let peer = open_for_test(child.id()).expect("pidfd");
        child.wait().expect("reaped");
        assert!(!running(&peer));
        assert!(identity(&peer).is_err());
    }
}
