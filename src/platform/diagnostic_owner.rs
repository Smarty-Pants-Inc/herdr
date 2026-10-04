//! Kernel process-start identity for private, client-owned diagnostic snapshots.
//! Self-contained dispatcher: integration fixtures include this exact file by path.
//! `None` proves absence; query failures must not authorize reclamation.

#[cfg(target_os = "linux")]
#[path = "linux/diagnostic_owner.rs"]
mod native;
#[cfg(target_os = "macos")]
#[path = "macos/diagnostic_owner.rs"]
mod native;
#[cfg(windows)]
#[path = "windows/diagnostic_owner.rs"]
mod native;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) use native::diagnostic_owner_identity;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn diagnostic_owner_identity(_pid: u32) -> std::io::Result<Option<String>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "kernel diagnostic owner identity is unsupported",
    ))
}

// ponytail: these private diagnostics have never been released. The qualified format
// deliberately does not adopt, authenticate or reclaim earlier experiment files.
pub(crate) const DIAGNOSTIC_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

pub(crate) fn diagnostic_directory(socket: &std::path::Path) -> std::path::PathBuf {
    socket.with_extension("machine-status")
}

pub(crate) fn diagnostic_snapshot_name(id: &str) -> String {
    format!("owner-v2.{id}.json")
}

pub(crate) fn diagnostic_temporary_name(
    id: &str,
    pid: u32,
    owner: &str,
) -> std::io::Result<String> {
    if !valid_id(id) || pid == 0 || owner.is_empty() || owner.len() > 96 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid diagnostic temporary identity",
        ));
    }
    let encoded: String = owner.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("owner-v2.{id}.{pid}.{encoded}.tmp"))
}

pub(crate) enum DiagnosticName {
    Snapshot {
        client_id: String,
    },
    Temporary {
        client_id: String,
        pid: u32,
        owner_identity: String,
    },
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn parse_diagnostic_name(name: &std::ffi::OsStr) -> Option<DiagnosticName> {
    let name = name.to_str()?;
    if name.len() > 255 {
        return None;
    }
    let parts: Vec<_> = name.split('.').collect();
    match parts.as_slice() {
        ["owner-v2", id, "json"] if valid_id(id) => Some(DiagnosticName::Snapshot {
            client_id: (*id).into(),
        }),
        ["owner-v2", id, pid, encoded, "tmp"] if valid_id(id) => {
            let pid = pid.parse::<u32>().ok().filter(|pid| *pid > 0)?;
            if encoded.is_empty() || encoded.len() > 192 || !encoded.len().is_multiple_of(2) {
                return None;
            }
            let bytes = encoded
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    let high = (pair[0] as char).to_digit(16)?;
                    let low = (pair[1] as char).to_digit(16)?;
                    Some((high * 16 + low) as u8)
                })
                .collect::<Option<Vec<_>>>()?;
            Some(DiagnosticName::Temporary {
                client_id: (*id).into(),
                pid,
                owner_identity: String::from_utf8(bytes).ok()?,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_owner_lifecycle_preserves_stopped_but_rejects_unreaped_zombie() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                // Own cleanup even when an assertion fails while the child is stopped.
                let _ = self.0.kill();
                unsafe {
                    libc::kill(self.0.id() as libc::pid_t, libc::SIGCONT);
                }
                let _ = self.0.wait();
            }
        }
        fn wait_for_state(pid: u32, expected: &str) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
                let state = stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap();
                if state == expected {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected kernel state {expected}, observed {state}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let mut child = ChildGuard(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let live = diagnostic_owner_identity(pid).unwrap().unwrap();
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) }, 0);
        wait_for_state(pid, "T");
        assert_eq!(diagnostic_owner_identity(pid).unwrap(), Some(live));
        child.0.kill().unwrap();
        // Do not call wait/try_wait: the zombie must remain in the kernel table.
        wait_for_state(pid, "Z");
        assert_eq!(diagnostic_owner_identity(pid).unwrap(), None);
    }

    #[test]
    fn current_owner_identity_is_stable_and_absent_pid_is_distinct() {
        let first = diagnostic_owner_identity(std::process::id())
            .unwrap()
            .unwrap();
        assert!(!first.is_empty());
        assert_eq!(
            diagnostic_owner_identity(std::process::id()).unwrap(),
            Some(first)
        );
        assert_eq!(diagnostic_owner_identity(i32::MAX as u32).unwrap(), None);
        assert_eq!(
            diagnostic_owner_identity(0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        #[cfg(unix)]
        assert_eq!(
            diagnostic_owner_identity(u32::MAX).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        #[cfg(windows)]
        assert!(!matches!(diagnostic_owner_identity(u32::MAX), Ok(Some(_))));
        // Names encode DWORD/u32 PIDs losslessly; signed pid_t validation belongs
        // in native Unix lookup, never in the cross-platform storage contract.
        let id = "0123456789abcdef0123456789abcdef";
        let name = diagnostic_temporary_name(id, u32::MAX, "birth").unwrap();
        assert!(matches!(
            parse_diagnostic_name(std::ffi::OsStr::new(&name)),
            Some(DiagnosticName::Temporary { pid: u32::MAX, .. })
        ));
    }
}
