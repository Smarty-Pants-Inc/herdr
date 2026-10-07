//! A smarty-install (setgid / root-owned) herdr refuses self-update before any download
//! or write (smarty-dev#2636, herdr-lead ruling on the updater).
#![cfg(target_os = "linux")]
#[path = "support/command.rs"]
mod test_command;

use std::os::unix::fs::PermissionsExt;

#[test]
fn self_update_refuses_setgid_install_before_download_or_write() {
    let dir = std::env::temp_dir().join(format!("herdr-setgid-upd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("herdr");
    std::fs::copy(env!("CARGO_BIN_EXE_herdr"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o2755)).unwrap();
    let before = std::fs::read(&exe).unwrap();
    let mut command = std::process::Command::new(&exe);
    test_command::sanitize_command_env(&mut command);
    // An unroutable update source: any network attempt would fail differently.
    let out = command
        .arg("update")
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(
        stderr.trim(),
        "this herdr is installed by smarty-install (setgid herdr for the server key); update it with smarty-install herdr <sha>"
    );
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
