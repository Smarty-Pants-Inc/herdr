//! Linux facts for attributing an attached client to a person (smarty-dev#1515).
//!
//! Every fact here must come from something the Herdr server's own user cannot forge: the
//! kernel's `/proc`, journald's trusted fields and tailscaled. The commands run by absolute path
//! with a cleared environment, because agents running as the same user control `PATH` and rc files.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::platform::{IdentityJournalRecord, IdentityProcess};

const JOURNALCTL: &str = "/usr/bin/journalctl";
const TAILSCALE: &[&str] = &["/usr/bin/tailscale", "/usr/sbin/tailscale"];
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 1 << 20;

pub(crate) fn identity_process(pid: u32) -> Option<IdentityProcess> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses; the fields after it follow the last ')'.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let fields = stat.get(close + 2..)?.split(' ').collect::<Vec<_>>();
    // Field 4 (ppid) is index 1 after state; field 22 (starttime) is index 19.
    let ppid = fields.get(1)?.parse().ok()?;
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uids = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .filter_map(|uid| uid.parse::<u32>().ok())
        .collect::<Vec<_>>();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let exe_name = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    Some(IdentityProcess {
        ppid,
        uids,
        comm,
        cmdline,
        exe_name,
        start_unix_secs: start_unix_secs(start_ticks)?,
        start_ticks,
    })
}

fn start_unix_secs(start_ticks: u64) -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<u64>()
        .ok()?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let ticks = u64::try_from(ticks).ok().filter(|ticks| *ticks > 0)?;
    Some(btime + start_ticks / ticks)
}

/// Journal records the root process `pid` wrote in this boot since `since_unix_secs`.
pub(crate) fn identity_journal_records(
    pid: u32,
    since_unix_secs: u64,
) -> Option<Vec<IdentityJournalRecord>> {
    let output = run(
        JOURNALCTL,
        &[
            "--no-pager",
            "--quiet",
            "--boot",
            "--output=json",
            &format!("--since=@{}", since_unix_secs.saturating_sub(1)),
            &format!("_PID={pid}"),
            "_UID=0",
        ],
    )?;
    Some(
        output
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .map(|record| IdentityJournalRecord {
                message: journal_field(&record, "MESSAGE"),
                pid: journal_field(&record, "_PID").and_then(|pid| pid.parse().ok()),
                uid: journal_field(&record, "_UID").and_then(|uid| uid.parse().ok()),
                comm: journal_field(&record, "_COMM"),
            })
            .collect(),
    )
}

fn journal_field(record: &serde_json::Value, name: &str) -> Option<String> {
    match record.get(name)? {
        serde_json::Value::String(value) => Some(value.clone()),
        // journald encodes non-UTF-8 values as byte arrays; they never match.
        _ => None,
    }
}

pub(crate) fn identity_tailscale_whois(ip: &str) -> Option<String> {
    let program = TAILSCALE
        .iter()
        .find(|path| std::path::Path::new(path).is_file())?;
    run(program, &["whois", "--json", ip])
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout)
            .take(MAX_OUTPUT as u64)
            .read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let output = reader.join().ok()?;
    status
        .success()
        .then(|| String::from_utf8(output).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_this_process_from_proc() {
        let process = super::identity_process(std::process::id()).expect("own process");
        let uid = unsafe { libc::getuid() };
        assert_eq!(process.uids.first(), Some(&uid));
        assert_eq!(process.ppid, std::os::unix::process::parent_id());
        assert!(!process.cmdline.is_empty());
        assert!(process.exe_name.is_some());
        let parent = super::identity_process(process.ppid).expect("parent process");
        assert!(parent.start_ticks <= process.start_ticks);
        assert!(process.start_unix_secs > 1_600_000_000);
    }
}
