//! `herdr-sshd-lookup 3<&CONNECTION`: print the accepted public key of the sshd
//! login that the connection's peer descends from.
//!
//! Installed `root:systemd-journal` mode 2755 (docs/next/sshd-lookup.md). It
//! prints one JSON line only after every check passes; otherwise it prints
//! nothing and exits nonzero.

/// The server's own bridge argv rule, the same file (smarty-dev#13408 F2).
#[cfg(target_os = "linux")]
#[path = "../../../src/server/bridge_argv.rs"]
mod bridge_argv;
#[cfg(target_os = "linux")]
mod lookup;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    if let Some(line) = lookup::run() {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        if stdout
            .write_all(line.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok()
        {
            return std::process::ExitCode::SUCCESS;
        }
    }
    std::process::ExitCode::FAILURE
}
