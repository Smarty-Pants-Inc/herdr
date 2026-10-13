//! Real process and filesystem probes; no identity lookup substitution.
use super::cleanup_tests::{test_base, ChildGuard};
use super::*;

#[test]
fn unreadable_or_missing_importer_record_preserves_live_owner_and_runtime() {
    for case in [
        "missing",
        "directory",
        "empty",
        "truncated",
        "invalid-pid",
        "invalid-utf8",
        "empty-identity",
    ] {
        let child = ChildGuard::sleeping();
        let base = test_base(case);
        let mut cleanup = ScopedHandoffServer::new(&base);
        let listener = std::os::unix::net::UnixListener::bind(&cleanup.socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        cleanup.track_original(child.0.id());
        let _wrapper = cleanup.importer_exe();
        let record = cleanup.importer_records[0].clone();
        match case {
            "missing" => {}
            "directory" => fs::create_dir(&record).unwrap(),
            "empty" => fs::write(&record, "").unwrap(),
            "truncated" => fs::write(&record, child.0.id().to_string()).unwrap(),
            "invalid-pid" => fs::write(&record, "4294967295\nidentity\n").unwrap(),
            "invalid-utf8" => fs::write(&record, [0xff]).unwrap(),
            "empty-identity" => fs::write(&record, format!("{}\n\n", child.0.id())).unwrap(),
            _ => unreachable!(),
        }
        let error = cleanup.stop_and_cleanup().unwrap_err();
        eprintln!(
            "record={case}: {error}; owner={} remains live",
            child.0.id()
        );
        assert!(test_process_running(child.0.id()));
        assert!(base.join("runtime/sentinel").exists());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        cleanup.importer_records.clear();
        cleanup.stop_and_cleanup().unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_accepts_unreaped_zombie_as_terminated() {
    let mut child = ChildGuard::sleeping();
    let base = test_base("zombie");
    let mut cleanup = ScopedHandoffServer::new(&base);
    cleanup.track_original(child.0.id());
    child.0.kill().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while test_process_running(child.0.id()) {
        assert!(
            Instant::now() < deadline,
            "child did not reach zombie state"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Deliberately leave it unreaped while cleanup verifies its termination.
    let stat = fs::read_to_string(format!("/proc/{}/stat", child.0.id())).unwrap();
    assert_eq!(
        stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
        Some("Z")
    );
    cleanup.stop_and_cleanup().unwrap();
    assert!(!base.exists());
    assert!(Path::new(&format!("/proc/{}", child.0.id())).exists());
    eprintln!("unreaped zombie {} permits runtime removal", child.0.id());
}

#[cfg(target_os = "linux")]
#[test]
fn unreadable_native_identity_fails_closed() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "support::importer_exec_tests::unreadable_identity_subprocess",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let receipt = String::from_utf8_lossy(&output.stdout);
    eprintln!("{receipt}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        receipt.contains("1 passed"),
        "helper must execute exactly one test"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "isolated descriptor-limit probe invoked by unreadable_native_identity_fails_closed"]
fn unreadable_identity_subprocess() {
    let child = ChildGuard::sleeping();
    let base = test_base("unreadable-native");
    let mut cleanup = ScopedHandoffServer::new(&base);
    let listener = std::os::unix::net::UnixListener::bind(&cleanup.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    cleanup.track_original(child.0.id());
    // Exhaustion is a genuine native /proc/proc_pidinfo query error, not absence.
    // Isolate the limit in this executable; restore before assertions or Drop.
    let mut original: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
        0
    );
    let exhausted = libc::rlimit {
        rlim_cur: 0,
        rlim_max: original.rlim_max,
    };
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &exhausted) },
        0
    );
    let lookup = process_start_identity(child.0.id());
    let result = cleanup.stop_and_cleanup();
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
        0
    );
    assert_eq!(lookup.unwrap_err().raw_os_error(), Some(libc::EMFILE));
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EMFILE));
    assert!(test_process_running(child.0.id()));
    assert!(base.join("runtime/sentinel").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    eprintln!(
        "native EMFILE: live owner {} and runtime preserved; no socket stop",
        child.0.id()
    );
    cleanup.stop_and_cleanup().unwrap();
}

#[test]
fn importer_wrapper_registers_kernel_birth_before_exec() {
    let base = test_base("importer-exec");
    let mut cleanup = ScopedHandoffServer::new(&base);
    let wrapper = cleanup.importer_exe();
    let mut child = ChildGuard(
        std::process::Command::new(wrapper)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(child.0.wait().unwrap().success());
    let record = fs::read_to_string(&cleanup.importer_records[0]).unwrap();
    let (pid, identity) = record.split_once('\n').unwrap();
    assert_eq!(pid.parse::<u32>().unwrap(), child.0.id());
    #[cfg(target_os = "linux")]
    assert!(identity.starts_with("linux:"));
    #[cfg(target_os = "macos")]
    assert!(identity.starts_with("macos:"));
    eprintln!(
        "pre-exec kernel identity recorded for same PID {}: {}",
        child.0.id(),
        identity.trim()
    );
    cleanup.stop_and_cleanup().unwrap();
    assert!(!base.exists());
}
