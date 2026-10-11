use super::*;

#[test]
fn channel_native_foreground_refuses_stopped_dead_reused_and_foreign_peers() {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("real pane PTY");
    let mut command = portable_pty::CommandBuilder::new("sh");
    command.args(["-c", "read line"]);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("real foreground root");
    let pid = child.process_id().expect("root PID");
    let identity = process_identity(pid).expect("exact root");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !registered_process_is_foreground(identity, identity) {
        assert!(
            std::time::Instant::now() < deadline,
            "root never became foreground"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let foreign = process_identity(std::process::id()).expect("outside process");
    assert!(!registered_process_is_foreground(identity, foreign));
    let reused = super::super::ProcessIdentity {
        start_time: identity.start_time + 1,
        ..identity
    };
    assert!(!registered_process_is_foreground(identity, reused));
    assert_eq!(
        registered_process_liveness(reused),
        super::super::ProcessLiveness::Dead
    );
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) }, 0);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("stopped stat");
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        if rest.starts_with('T') {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "root never stopped");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A stopped process still owns its pin but cannot register/receive input.
    assert_eq!(
        registered_process_liveness(identity),
        super::super::ProcessLiveness::Alive
    );
    assert!(!registered_process_is_foreground(identity, identity));
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) }, 0);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        registered_process_liveness(identity),
        super::super::ProcessLiveness::Dead
    );
    assert!(!registered_process_is_foreground(identity, identity));
}

#[test]
fn channel_zombie_liveness_is_confirmed_death_not_takeover_guess() {
    let mut child = std::process::Command::new("sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    let identity = process_identity(child.id()).expect("live child");
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while registered_process_liveness(identity) != super::super::ProcessLiveness::Dead {
        assert!(
            std::time::Instant::now() < deadline,
            "exit was not observed"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child.wait().expect("reap");
}
