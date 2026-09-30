use std::fs;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "support/command.rs"]
pub mod test_command;

#[test]
fn cli_user_dir_creation_seeds_legacy_config_before_printing_config_dir() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "herdr-plugin-config-dir-{}-{nanos}",
        std::process::id()
    ));
    let app_dir_name = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let home = base.join("home");
    let config_home = base.join("config");
    let state_home = base.join("state");
    let plugin_id = "example.legacy-config";
    let plugins_dir = config_home.join(app_dir_name).join("plugins");
    let config_dir = plugins_dir.join("config").join(plugin_id);
    let state_dir = state_home
        .join(app_dir_name)
        .join("plugins")
        .join(plugin_id);
    let legacy_dir = plugins_dir.join(plugin_id);
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&legacy_dir).unwrap();
    fs::write(legacy_dir.join(".env"), "TOKEN=legacy\n").unwrap();
    assert!(!config_dir.exists());
    assert!(!state_dir.exists());

    // Run the real CLI, keeping its process-wide SIGPIPE handling out of the
    // test runner. The shared helper scrubs all inherited HERDR_* markers.
    let mut child = crate::test_command::herdr_command()
        .args(["plugin", "config-dir", plugin_id])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut printed_path = String::new();
    let read_result = stdout.read_line(&mut printed_path);
    // Snapshot the migration as soon as the path is emitted, not just after
    // process exit. Reap the child before asserting any captured result.
    let migrated_env = fs::read_to_string(config_dir.join(".env"));
    let stable_dirs_exist = config_dir.is_dir() && state_dir.is_dir();
    let output = child.wait_with_output().unwrap();
    fs::remove_dir_all(&base).unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(read_result.unwrap() > 0);
    assert_eq!(printed_path, format!("{}\n", config_dir.display()));
    assert_eq!(migrated_env.unwrap(), "TOKEN=legacy\n");
    assert!(
        stable_dirs_exist,
        "config and state directories must exist before output"
    );
}
