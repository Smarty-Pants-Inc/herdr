//! Remove inherited server CPU pinning before a pane program can fork.
//!
//! Sysfs is authoritative: PID 1 and the server may both have narrowed masks.
//! The kernel still intersects this request with the child's cgroup cpuset.

use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use portable_pty::{Child, CommandBuilder, SlavePty};

const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
const WORD_BITS: usize = libc::c_ulong::BITS as usize;
type PreExecHook = Box<dyn FnMut() -> io::Result<()> + Send + Sync>;

pub(crate) fn spawn_pane_child(
    slave: &dyn SlavePty,
    command: CommandBuilder,
    pane_id: u32,
) -> io::Result<Box<dyn Child + Send + Sync>> {
    // All I/O, parsing, allocation, and descriptor setup happen before fork.
    let prepared = (|| {
        let mask = parse_online_cpus(&std::fs::read_to_string(ONLINE_CPUS)?)?;
        let (reader, writer) = report_pipe()?;
        Ok::<_, io::Error>((reader, affinity_hook(mask, writer)))
    })();
    let (reader, hook) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => {
            tracing::warn!(pane = pane_id, %err, "could not prepare pane CPU affinity; retaining inherited affinity");
            return slave
                .spawn_command(command)
                .map_err(|err| io::Error::other(err.to_string()));
        }
    };

    // SAFETY: affinity_hook only borrows its preallocated mask and performs
    // sched_setaffinity, errno access, and raw writes. It never allocates, locks,
    // logs, unwinds, or drops its captured resources in the child.
    let result = unsafe { slave.spawn_command_with_pre_exec(command, hook) };

    // The hook's parent-side writer has been dropped on return from spawn. Do
    // not wait for EOF: this nonblocking read cannot hang even if a PTY backend
    // retains a writer or exec fails before the hook. One record smaller than
    // PIPE_BUF is written atomically before descriptor cleanup closes the end.
    let mut report = [0; std::mem::size_of::<libc::c_int>()];
    match File::from(reader).read_exact(&mut report) {
        Ok(()) => {
            let errno = libc::c_int::from_ne_bytes(report);
            if errno != 0 {
                let err = io::Error::from_raw_os_error(errno);
                tracing::warn!(pane = pane_id, errno, %err, "could not reset pane CPU affinity; retaining inherited affinity");
            }
        }
        Err(err) if result.is_ok() => {
            tracing::warn!(pane = pane_id, %err, "pane CPU affinity result was unavailable");
        }
        Err(_) => {} // Earlier terminal setup failures already fail the spawn.
    }
    result.map_err(|err| io::Error::other(err.to_string()))
}

fn cpu_range(text: &str) -> io::Result<(usize, usize)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid online CPU list");
    let number = |text: &str| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        text.parse::<usize>().map_err(|_| invalid())
    };
    let (first, last) = match text.split_once('-') {
        Some((first, last)) => (number(first)?, number(last)?),
        None => {
            let cpu = number(text)?;
            (cpu, cpu)
        }
    };
    if first > last {
        return Err(invalid());
    }
    Ok((first, last))
}

fn parse_online_cpus(text: &str) -> io::Result<Vec<libc::c_ulong>> {
    let text = text.trim();
    let mut highest = 0;
    for range in text.split(',') {
        highest = highest.max(cpu_range(range)?.1);
    }
    let words = (highest / WORD_BITS)
        .checked_add(1)
        .ok_or_else(|| io::Error::other("online CPU mask size overflow"))?;
    // Native unsigned-long words provide the alignment and bit ordering the
    // kernel expects, without CPU_SETSIZE or another fixed CPU-index ceiling.
    let mut mask = Vec::<libc::c_ulong>::new();
    mask.try_reserve_exact(words).map_err(io::Error::other)?;
    mask.resize(words, 0);
    for range in text.split(',') {
        let (first, last) = cpu_range(range)?;
        for cpu in first..=last {
            mask[cpu / WORD_BITS] |= 1 << (cpu % WORD_BITS);
        }
    }
    Ok(mask)
}

fn report_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut raw = [-1; 2];
    // SAFETY: pipe2 initializes exactly two descriptors on success.
    if unsafe { libc::pipe2(raw.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: these new descriptors have no other owner.
    let mut ends = raw.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    // Command's stdio setup must not overwrite either end if the server was
    // launched with a standard descriptor closed.
    for end in &mut ends {
        if end.as_raw_fd() < 3 {
            // SAFETY: fcntl duplicates this live descriptor above stdio.
            let fd = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl returned a new descriptor owned only here.
            *end = unsafe { OwnedFd::from_raw_fd(fd) };
        }
    }
    let [reader, writer] = ends;
    Ok((reader, writer))
}

fn affinity_hook(mask: Vec<libc::c_ulong>, writer: OwnedFd) -> PreExecHook {
    Box::new(move || {
        // SAFETY: the parent prepared an aligned, nonempty mask covering every
        // online CPU. The syscall borrows it without modifying the allocation.
        let status = unsafe {
            libc::sched_setaffinity(
                0,
                mask.len() * std::mem::size_of::<libc::c_ulong>(),
                mask.as_ptr().cast(),
            )
        };
        let errno: libc::c_int = if status == 0 {
            0
        } else {
            // SAFETY: Linux errno is thread-local; read it before another syscall.
            unsafe { *libc::__errno_location() }
        };
        loop {
            // SAFETY: writer stays owned by this closure; errno is an initialized
            // fixed-size record. The parent keeps the reader open through spawn,
            // so this cannot raise SIGPIPE. The nonblocking pipe cannot stall.
            let written = unsafe {
                libc::write(
                    writer.as_raw_fd(),
                    (&errno as *const libc::c_int).cast(),
                    std::mem::size_of_val(&errno),
                )
            };
            // Retry only an interrupted write, without creating an io::Error.
            if written >= 0 || unsafe { *libc::__errno_location() } != libc::EINTR {
                break;
            }
        }
        // Affinity refusal must not abort exec or change pane lifecycle policy.
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_cpu_lists_are_rejected() {
        // Decision C — keep: malformed sysfs is a fail-soft path a pane E2E
        // cannot reach cheaply without replacing host kernel-owned files.
        for text in [
            "", " ", ",", "0,", "0,,1", "-1", "+1", "1-", "2-1", "0-1-2", "0 1", "0-3:2",
        ] {
            assert!(parse_online_cpus(text).is_err(), "accepted {text:?}");
        }
        assert!(parse_online_cpus(&format!("{}0", usize::MAX)).is_err());
    }

    #[test]
    fn sparse_high_cpu_indices_are_not_narrowed() {
        // Decision C — keep: a sparse >CPU_SETSIZE topology cannot be arranged
        // cheaply by the pane E2E on an ordinary host.
        let mask = parse_online_cpus("0-2,7,1024,8193-8194\n").unwrap();
        assert_eq!(mask.len(), 8194 / WORD_BITS + 1);
        let cpus: Vec<_> = mask
            .iter()
            .enumerate()
            .flat_map(|(word, bits)| {
                (0..WORD_BITS)
                    .filter(move |&bit| *bits & (1 << bit) != 0)
                    .map(move |bit| word * WORD_BITS + bit)
            })
            .collect();
        assert_eq!(cpus, [0, 1, 2, 7, 1024, 8193, 8194]);
    }

    #[test]
    fn affinity_refusal_reports_errno_and_allows_exec() {
        // Decision C — keep: refusal is cheap to inject here with an all-zero
        // mask, but pane E2E cannot modify sysfs or kernel policy. This proves
        // exec continues after a real kernel refusal without changing parent affinity.
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let (reader, writer) = report_pipe().unwrap();
        // SAFETY: affinity_hook uses only its preallocated mask and raw syscalls.
        // Keep the reader alive through spawn so the hook cannot raise SIGPIPE.
        let mut child = unsafe {
            pair.slave.spawn_command_with_pre_exec(
                CommandBuilder::new("/bin/true"),
                affinity_hook(vec![0], writer),
            )
        }
        .unwrap();
        // /bin/true needs no PTY input or output draining before it exits.
        assert!(child.wait().unwrap().success());
        let mut report = [0; std::mem::size_of::<libc::c_int>()];
        // The report pipe is nonblocking; read one record rather than waiting for EOF.
        File::from(reader).read_exact(&mut report).unwrap();
        assert_ne!(libc::c_int::from_ne_bytes(report), 0);
    }
}
