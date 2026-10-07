//! A smarty-install (setgid / root-owned) herdr refuses self-update before any download
//! or write (smarty-dev#2636, herdr-lead ruling on the updater).
#![cfg(target_os = "linux")]
#[path = "support/command.rs"]
mod test_command;

use std::os::unix::fs::PermissionsExt;

const REFUSAL: &str = "this herdr is installed by smarty-install (setgid herdr for the server key); update it with smarty-install herdr <sha>";

fn setgid_copy(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("herdr-setgid-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("herdr");
    std::fs::copy(env!("CARGO_BIN_EXE_herdr"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o2755)).unwrap();
    (dir, exe)
}

fn isolate(command: &mut std::process::Command, dir: &std::path::Path) {
    test_command::sanitize_command_env(command);
    command
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("HERDR_CONFIG_PATH", dir.join("config/herdr/config.toml"))
        .env("HTTPS_PROXY", "http://127.0.0.1:9");
}

#[test]
fn self_update_refuses_deleted_running_setgid_binary() {
    // Review r5 F1: the running inode decides, even after its path is unlinked.
    let (dir, exe) = setgid_copy("deleted");
    let script = "import os,sys\nfd=os.open(sys.argv[1],os.O_RDONLY)\nos.unlink(sys.argv[1])\nos.execv('/proc/self/fd/%d'%fd,['herdr','update'])";
    let mut command = std::process::Command::new("python3");
    command.arg("-c").arg(script).arg(&exe);
    isolate(&mut command, &dir);
    let out = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.trim(), REFUSAL);
    assert!(!exe.exists());
    assert!(!dir.join("config").exists(), "no config written");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn channel_set_refuses_setgid_install_before_touching_config() {
    // Review r5 F2: exact message, non-zero exit, config untouched.
    for channel in ["stable", "preview"] {
        let (dir, exe) = setgid_copy(channel);
        let mut command = std::process::Command::new(&exe);
        command.args(["channel", "set", channel]);
        isolate(&mut command, &dir);
        let out = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert_eq!(stderr.trim(), REFUSAL);
        assert!(
            !dir.join("config").exists(),
            "no config written for {channel}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn self_update_refuses_setgid_install_before_download_or_write() {
    let dir = std::env::temp_dir().join(format!("herdr-setgid-upd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("herdr");
    std::fs::copy(env!("CARGO_BIN_EXE_herdr"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o2755)).unwrap();
    let before = std::fs::read(&exe).unwrap();
    let mut command = std::process::Command::new(&exe);
    // An unroutable update source: any network attempt would fail differently.
    isolate(&mut command, &dir);
    let out = command.arg("update").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(stderr.trim(), REFUSAL);
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(std::fs::read(&exe).unwrap(), before, "binary untouched");
    assert_eq!(
        std::fs::metadata(&exe).unwrap().permissions().mode() & 0o7777,
        0o2755
    );
    assert!(
        entries
            .iter()
            .all(|n| n == "herdr" || n == "config" || n == "state"),
        "{entries:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
