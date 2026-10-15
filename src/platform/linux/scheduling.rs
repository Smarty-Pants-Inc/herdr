//! CPU scheduling class and nice value for the server and its pane children.
//!
//! Linux children inherit the scheduling policy and nice value of the thread
//! that forked them. A server started under `SCHED_IDLE` or a high nice value
//! would otherwise pass that idle priority to every pane shell (smarty-dev#11237).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tracing::{info, warn};

/// `sched_getscheduler` reports this flag ORed into the policy.
const SCHED_RESET_ON_FORK: libc::c_int = 0x4000_0000;

/// Words the child reports: policy before, policy errno, nice before, nice errno.
const REPORT_WORDS: usize = 4;
const REPORT_BYTES: usize = REPORT_WORDS * std::mem::size_of::<libc::c_int>();
/// The child could not read the value; the matching errno word holds the reason.
const UNKNOWN: libc::c_int = libc::c_int::MIN;

pub(crate) fn policy_name(policy: libc::c_int) -> &'static str {
    match policy {
        libc::SCHED_OTHER => "SCHED_OTHER",
        libc::SCHED_FIFO => "SCHED_FIFO",
        libc::SCHED_RR => "SCHED_RR",
        libc::SCHED_BATCH => "SCHED_BATCH",
        libc::SCHED_IDLE => "SCHED_IDLE",
        6 => "SCHED_DEADLINE",
        _ => "unknown",
    }
}

fn last_errno() -> libc::c_int {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

/// Policy of thread `tid` (0 = the caller), without the reset-on-fork flag.
fn thread_policy(tid: libc::pid_t) -> Result<libc::c_int, libc::c_int> {
    // SAFETY: sched_getscheduler only reads the target's scheduling policy.
    let policy = unsafe { libc::sched_getscheduler(tid) };
    if policy < 0 {
        Err(last_errno())
    } else {
        Ok(policy & !SCHED_RESET_ON_FORK)
    }
}

/// Nice value of thread `tid` (0 = the caller). Linux nice values are per thread.
fn thread_nice(tid: libc::pid_t) -> Result<libc::c_int, libc::c_int> {
    // getpriority can legitimately return -1, so errno is the only failure signal.
    // SAFETY: __errno_location returns the calling thread's errno slot.
    unsafe { *libc::__errno_location() = 0 };
    // SAFETY: getpriority only reads the target's nice value.
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) };
    if nice == -1 {
        let errno = last_errno();
        if errno != 0 {
            return Err(errno);
        }
    }
    Ok(nice)
}

fn set_thread_policy_other(tid: libc::pid_t) -> Result<(), libc::c_int> {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: param is a valid sched_param for SCHED_OTHER.
    if unsafe { libc::sched_setscheduler(tid, libc::SCHED_OTHER, &param) } == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

fn set_thread_nice(tid: libc::pid_t, nice: libc::c_int) -> Result<(), libc::c_int> {
    // SAFETY: setpriority only changes the target's nice value.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice) } == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Runs in the forked pane child before `exec`. Async-signal-safe: plain
/// syscalls and one `write`, no allocation, locking or logging.
fn reset_child_scheduling(target_nice: libc::c_int, report_fd: RawFd) {
    let mut report = [0 as libc::c_int; REPORT_WORDS];
    match thread_policy(0) {
        Ok(policy) => {
            report[0] = policy;
            if policy != libc::SCHED_OTHER {
                if let Err(errno) = set_thread_policy_other(0) {
                    report[1] = errno;
                }
            }
        }
        Err(errno) => {
            report[0] = UNKNOWN;
            report[1] = errno;
        }
    }
    match thread_nice(0) {
        Ok(nice) => {
            report[2] = nice;
            if nice != target_nice {
                if let Err(errno) = set_thread_nice(0, target_nice) {
                    report[3] = errno;
                }
            }
        }
        Err(errno) => {
            report[2] = UNKNOWN;
            report[3] = errno;
        }
    }
    // SAFETY: report is a live array of REPORT_BYTES bytes; a failed write is
    // reported by the parent as a missing report.
    unsafe {
        libc::write(report_fd, report.as_ptr().cast(), REPORT_BYTES);
    }
}

/// Lowest descriptor the report pipe may use; 0-2 are the child's stdio.
const FIRST_NON_STDIO_FD: libc::c_int = 3;

/// Move `fd` to the lowest free descriptor >= 3 (close-on-exec) and close the
/// original. File status flags such as `O_NONBLOCK` live on the shared open
/// file description, so the duplicate keeps them.
fn move_above_stdio(fd: RawFd) -> Result<OwnedFd, libc::c_int> {
    if fd >= FIRST_NON_STDIO_FD {
        // SAFETY: the caller owns fd and transfers it here.
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    // SAFETY: F_DUPFD_CLOEXEC only duplicates the caller-owned fd.
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, FIRST_NON_STDIO_FD) };
    let result = if moved < 0 {
        Err(last_errno())
    } else {
        // SAFETY: fcntl returned a fresh descriptor owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(moved) })
    };
    // SAFETY: the caller owns fd; it is closed exactly once here.
    unsafe { libc::close(fd) };
    result
}

/// A close-on-exec report pipe whose ends are never fd 0, 1 or 2, with a
/// non-blocking write end. If Herdr started with a standard descriptor closed,
/// `pipe2` could return 0-2, which the PTY spawn's stdio setup replaces before
/// the hook runs. Allocation-free, so it is also safe after `fork`.
fn report_pipe() -> Result<(OwnedFd, OwnedFd), libc::c_int> {
    let mut fds = [-1 as libc::c_int; 2];
    // SAFETY: fds has room for the two descriptors pipe2 returns.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(last_errno());
    }
    // Move the reader first while the writer still occupies its own slot.
    let reader = match move_above_stdio(fds[0]) {
        Ok(reader) => reader,
        Err(errno) => {
            // SAFETY: pipe2 returned fds[1]; it is closed exactly once here.
            unsafe { libc::close(fds[1]) };
            return Err(errno);
        }
    };
    let writer = move_above_stdio(fds[1])?;
    Ok((reader, writer))
}

/// The parent's side of one pane spawn's scheduling reset.
pub(crate) struct PaneScheduling {
    target_nice: libc::c_int,
    reader: Option<OwnedFd>,
    writer: Option<OwnedFd>,
}

/// Arrange for the pane child to run at `SCHED_OTHER` and `target_nice`.
///
/// The builder must be spawned at most once, before the returned value is
/// reported or dropped: the child writes its outcome to a pipe the value owns.
pub(crate) fn prepare_pane_scheduling(
    command: &mut portable_pty::CommandBuilder,
    target_nice: i32,
) -> PaneScheduling {
    let (reader, writer) = match report_pipe() {
        Ok((reader, writer)) => (Some(reader), Some(writer)),
        Err(errno) => {
            warn!(
                errno,
                err = %io::Error::from_raw_os_error(errno),
                "could not create pane scheduling report pipe"
            );
            (None, None)
        }
    };
    let report_fd = writer.as_ref().map_or(-1, AsRawFd::as_raw_fd);
    // SAFETY: reset_child_scheduling is async-signal-safe.
    let hook = unsafe {
        portable_pty::cmdbuilder::PreExecHook::new(move || {
            reset_child_scheduling(target_nice, report_fd)
        })
    };
    command.pre_exec_hook(Some(hook));
    PaneScheduling {
        target_nice,
        reader,
        writer,
    }
}

impl PaneScheduling {
    /// Log, once for this pane, what the child could not reset.
    pub(crate) fn report(mut self, pane_id: u32, pid: Option<u32>) {
        drop(self.writer.take());
        let Some(reader) = self.reader.take() else {
            return;
        };
        let mut report = [0 as libc::c_int; REPORT_WORDS];
        // SAFETY: report is a writable buffer of REPORT_BYTES bytes.
        let read =
            unsafe { libc::read(reader.as_raw_fd(), report.as_mut_ptr().cast(), REPORT_BYTES) };
        if read != REPORT_BYTES as isize {
            // Spawn returns only after exec, so the child already wrote its report.
            warn!(
                pane_id,
                pid, "pane child did not report its CPU scheduling; priority not verified"
            );
            return;
        }
        let [policy_before, policy_errno, nice_before, nice_errno] = report;
        log_pane_outcome(
            pane_id,
            pid,
            self.target_nice,
            (policy_before, policy_errno),
            (nice_before, nice_errno),
        );
    }
}

fn log_pane_outcome(
    pane_id: u32,
    pid: Option<u32>,
    target_nice: libc::c_int,
    (policy_before, policy_errno): (libc::c_int, libc::c_int),
    (nice_before, nice_errno): (libc::c_int, libc::c_int),
) {
    let policy = (policy_before != UNKNOWN).then(|| policy_name(policy_before));
    if policy_errno != 0 {
        warn!(
            pane_id,
            pid,
            policy,
            errno = policy_errno,
            err = %io::Error::from_raw_os_error(policy_errno),
            "pane shell kept the server's CPU scheduling policy; could not reset it to SCHED_OTHER"
        );
    } else if policy_before != libc::SCHED_OTHER {
        info!(
            pane_id,
            pid, policy, "pane shell scheduling policy reset to SCHED_OTHER"
        );
    }
    let nice = (nice_before != UNKNOWN).then_some(nice_before);
    if nice_errno != 0 {
        warn!(
            pane_id,
            pid,
            nice,
            target_nice,
            errno = nice_errno,
            err = %io::Error::from_raw_os_error(nice_errno),
            "pane shell kept the server's nice value; could not set the configured server.pane_nice"
        );
    } else if nice_before != target_nice {
        info!(
            pane_id,
            pid, nice, target_nice, "pane shell nice value reset"
        );
    }
}

/// Passes over `/proc/self/task` before giving up on a stable thread list.
const THREAD_ENUMERATION_PASSES: usize = 3;

/// At server start, warn loudly when the server itself runs at reduced CPU
/// priority, then reset every thread where the kernel permits it.
///
/// Contract: the server calls this from `run_server` before it builds its
/// async runtime, thread pools, PTY readers or handoff/import workers, so the
/// process is normally single-threaded (the main thread) here. Threads the
/// standard library or logging may have started are covered by re-reading
/// `/proc/self/task` until a pass finds no new thread. Threads created later
/// inherit the already reset class from their creator.
pub(crate) fn check_server_scheduling(start: &'static str) {
    let policy = thread_policy(0);
    let nice = thread_nice(0);
    let reduced_policy = matches!(policy, Ok(libc::SCHED_IDLE | libc::SCHED_BATCH));
    let raised_nice = matches!(nice, Ok(value) if value > 0);
    if !reduced_policy && !raised_nice {
        return;
    }
    let policy_label = policy.map_or("unknown", policy_name);
    let nice_value = nice.ok();
    warn!(
        start,
        policy = policy_label,
        nice = nice_value,
        "herdr server is running at reduced CPU priority and every pane would inherit it. \
         Cause: the process that launched this server (a systemd unit's Nice= or \
         CPUSchedulingPolicy=, a nice/chrt/ionice or smarty-reap wrapper, or the previous \
         server for a live handoff). Fix: remove Nice=/CPUSchedulingPolicy= from the unit or \
         start herdr without the wrapper, then restart the server"
    );

    let mut failures: Vec<(&'static str, libc::c_int)> = Vec::new();
    let mut reset_one = |tid: libc::pid_t| {
        // ESRCH: the thread exited after it was listed; nothing left to reset.
        if reduced_policy {
            match set_thread_policy_other(tid) {
                Ok(()) | Err(libc::ESRCH) => {}
                Err(errno) => failures.push(("SCHED_OTHER", errno)),
            }
        }
        if raised_nice {
            match set_thread_nice(tid, 0) {
                Ok(()) | Err(libc::ESRCH) => {}
                Err(errno) => failures.push(("nice 0", errno)),
            }
        }
    };
    let coverage = reset_listed_threads(server_thread_ids, &mut reset_one);
    if coverage == ThreadCoverage::Unlisted {
        reset_one(0);
    }
    failures.sort_unstable();
    failures.dedup();
    match coverage {
        ThreadCoverage::Complete => {}
        ThreadCoverage::Unlisted => warn!(
            start,
            "could not enumerate server threads; reset only the main thread"
        ),
        ThreadCoverage::Unstable => warn!(
            start,
            passes = THREAD_ENUMERATION_PASSES,
            "server threads kept appearing while resetting CPU priority; newer threads may keep the reduced priority"
        ),
    }
    if failures.is_empty() && coverage == ThreadCoverage::Complete {
        info!(
            start,
            "herdr server CPU priority reset to SCHED_OTHER at nice 0"
        );
        return;
    }
    for (target, errno) in failures {
        warn!(
            start,
            target,
            errno,
            err = %io::Error::from_raw_os_error(errno),
            "herdr server could not reset its own CPU priority; the kernel refused it (RLIMIT_NICE or missing CAP_SYS_NICE)"
        );
    }
}

/// How much of the server's thread list the reset covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadCoverage {
    /// A full pass found no thread that had not been reset.
    Complete,
    /// The first listing was unreadable or empty; nothing was reset.
    Unlisted,
    /// Listed threads were reset, but no pass came back without a new thread.
    Unstable,
}

/// Reset every listed thread, re-listing until a pass shows no new thread or
/// [`THREAD_ENUMERATION_PASSES`] passes have run.
fn reset_listed_threads(
    mut list: impl FnMut() -> Option<Vec<libc::pid_t>>,
    mut reset: impl FnMut(libc::pid_t),
) -> ThreadCoverage {
    let mut done: Vec<libc::pid_t> = Vec::new();
    for _ in 0..THREAD_ENUMERATION_PASSES {
        let Some(ids) = list() else {
            return if done.is_empty() {
                ThreadCoverage::Unlisted
            } else {
                ThreadCoverage::Unstable
            };
        };
        let mut new = false;
        for tid in ids {
            if !done.contains(&tid) {
                new = true;
                reset(tid);
                done.push(tid);
            }
        }
        if !new {
            return ThreadCoverage::Complete;
        }
    }
    ThreadCoverage::Unstable
}

/// Every current thread of this process, or `None` when /proc/self/task is
/// unreadable or lists nothing.
fn server_thread_ids() -> Option<Vec<libc::pid_t>> {
    let ids: Vec<libc::pid_t> = std::fs::read_dir("/proc/self/task")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    (!ids.is_empty()).then_some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd_flags(fd: RawFd) -> (libc::c_int, libc::c_int) {
        // SAFETY: F_GETFD/F_GETFL only read descriptor flags.
        unsafe {
            (
                libc::fcntl(fd, libc::F_GETFD),
                libc::fcntl(fd, libc::F_GETFL),
            )
        }
    }

    #[test]
    fn report_pipe_never_uses_stdio_fds_even_when_they_are_closed() {
        // Close 0-2 only in a forked child; the test process keeps its stdio.
        // The child runs only syscalls and report_pipe, which does not allocate.
        // SAFETY: fork; the child calls only async-signal-safe functions and _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());
        if pid == 0 {
            let code = (|| {
                for fd in 0..3 {
                    // SAFETY: closing the child's own stdio descriptors.
                    unsafe { libc::close(fd) };
                }
                let Ok((reader, writer)) = report_pipe() else {
                    return 10;
                };
                let (r, w) = (reader.as_raw_fd(), writer.as_raw_fd());
                if r < 3 || w < 3 {
                    return 11;
                }
                for fd in 0..3 {
                    // The originals must be closed again, not left on 0-2.
                    // SAFETY: F_GETFD only probes whether fd is open.
                    if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
                        return 12;
                    }
                }
                let (r_fd, r_fl) = fd_flags(r);
                let (w_fd, w_fl) = fd_flags(w);
                if r_fd & libc::FD_CLOEXEC == 0 || w_fd & libc::FD_CLOEXEC == 0 {
                    return 13;
                }
                if w_fl & libc::O_NONBLOCK == 0 || r_fl & libc::O_NONBLOCK == 0 {
                    return 14;
                }
                let byte = [7u8];
                let mut got = [0u8];
                // SAFETY: one-byte buffers on the live pipe ends.
                let ok = unsafe {
                    libc::write(w, byte.as_ptr().cast(), 1) == 1
                        && libc::read(r, got.as_mut_ptr().cast(), 1) == 1
                };
                if !ok || got != byte {
                    return 15;
                }
                0
            })();
            // SAFETY: leave the forked child without running test-harness code.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: pid is our child; status is a valid out pointer.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status), "child status {status}");
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }

    #[test]
    fn report_pipe_keeps_fds_above_stdio_in_a_normal_process() {
        let (reader, writer) = report_pipe().expect("pipe");
        assert!(reader.as_raw_fd() >= 3 && writer.as_raw_fd() >= 3);
        assert_ne!(fd_flags(writer.as_raw_fd()).1 & libc::O_NONBLOCK, 0);
    }

    fn run(listings: Vec<Option<Vec<libc::pid_t>>>) -> (ThreadCoverage, Vec<libc::pid_t>) {
        let mut listings = listings.into_iter();
        let mut reset = Vec::new();
        let coverage = reset_listed_threads(|| listings.next().flatten(), |tid| reset.push(tid));
        (coverage, reset)
    }

    #[test]
    fn thread_reset_complete_after_a_pass_with_no_new_thread() {
        assert_eq!(
            run(vec![
                Some(vec![1, 2]),
                Some(vec![2, 1, 3]),
                Some(vec![1, 3])
            ]),
            (ThreadCoverage::Complete, vec![1, 2, 3])
        );
    }

    #[test]
    fn thread_reset_unlisted_when_first_listing_fails() {
        assert_eq!(run(vec![None]), (ThreadCoverage::Unlisted, vec![]));
    }

    #[test]
    fn thread_reset_unstable_when_threads_keep_appearing_or_listing_breaks() {
        assert_eq!(
            run(vec![Some(vec![1]), Some(vec![1, 2]), Some(vec![1, 2, 3])]),
            (ThreadCoverage::Unstable, vec![1, 2, 3])
        );
        assert_eq!(
            run(vec![Some(vec![1]), None]),
            (ThreadCoverage::Unstable, vec![1])
        );
    }

    #[test]
    fn server_thread_ids_lists_the_calling_thread() {
        // SAFETY: gettid has no preconditions.
        let tid = unsafe { libc::gettid() };
        assert!(server_thread_ids().expect("listing").contains(&tid));
    }
}
