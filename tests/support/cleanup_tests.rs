//! Ownership regressions use only children started and reaped by this test.
use super::*;

pub(super) struct ChildGuard(pub(super) std::process::Child);

impl ChildGuard {
    pub(super) fn sleeping() -> Self {
        Self(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn test_base(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "herdr-cleanup-{label}-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(base.join("runtime")).unwrap();
    fs::write(base.join("runtime/sentinel"), "preserve until verified").unwrap();
    base
}

fn ps_lstart(pid: u32) -> String {
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn scoped_cleanup_preserves_real_process_with_colliding_ps_lstart() {
    // Two distinct births in the same displayed second simulate a recycled PID:
    // retain A's identity but address B's PID. Separate births by kernel ticks.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (first, second, displayed) = loop {
        let first = ChildGuard::sleeping();
        thread::sleep(Duration::from_millis(40));
        let second = ChildGuard::sleeping();
        let displayed = ps_lstart(first.0.id());
        if displayed == ps_lstart(second.0.id()) {
            break (first, second, displayed);
        }
        assert!(
            Instant::now() < deadline,
            "could not reproduce ps collision"
        );
    };
    let first_identity = process_start_identity(first.0.id()).unwrap().unwrap();
    let second_identity = process_start_identity(second.0.id()).unwrap().unwrap();
    eprintln!(
        "ps collision: first={} second={} lstart={displayed:?} identities={first_identity:?}/{second_identity:?}",
        first.0.id(), second.0.id()
    );
    let base = test_base("birth-collision");
    let mut cleanup = ScopedHandoffServer::new(&base);
    cleanup.track_original(first.0.id());
    assert_eq!(cleanup.owners[0].1, first_identity);
    cleanup.owners[0].0 = second.0.id();
    assert!(cleanup.stop_and_cleanup().is_err());
    assert!(
        test_process_running(second.0.id()),
        "cleanup must not signal B using A's colliding ps lstart identity"
    );
    assert!(test_process_running(first.0.id()));
    assert_ne!(
        process_start_identity(first.0.id()).unwrap(),
        Some(second_identity)
    );
    assert!(base.join("runtime/sentinel").exists());
    cleanup.owners.clear();
    cleanup.stop_and_cleanup().unwrap();
    assert!(!base.exists());
}

#[test]
fn scoped_cleanup_terminates_genuine_matched_owner() {
    let mut child = ChildGuard::sleeping();
    let base = test_base("matched-owner");
    let mut cleanup = ScopedHandoffServer::new(&base);
    cleanup.track_original(child.0.id());
    cleanup.stop_and_cleanup().unwrap();
    assert!(!test_process_running(child.0.id()));
    assert!(!child.0.wait().unwrap().success());
    assert!(!base.exists());
}
