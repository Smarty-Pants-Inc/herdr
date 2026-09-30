//! Child-only environment isolation for integration-test Herdr processes.
//!
//! Always construct the command with these helpers before applying test-specific
//! overrides. Agent profile overrides are also removed so integration commands
//! use the test's HOME rather than changing the invoking agent's live profile.
//! Do not mutate the test runner's environment: tests run in parallel.

use std::ffi::OsString;
use std::process::Command;

use portable_pty::CommandBuilder;

/// Start a Herdr CLI/server command with no inherited Herdr runtime context.
pub fn herdr_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    sanitize_command_env(&mut command);
    command
}

/// Also usable for shell hooks that invoke Herdr indirectly.
pub fn sanitize_command_env(command: &mut Command) {
    for (key, value) in isolated_env(std::env::vars_os().map(|(key, _)| key)) {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
}

/// portable-pty snapshots the inherited environment at construction time.
pub fn herdr_pty_command() -> CommandBuilder {
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    for (key, value) in isolated_env(std::env::vars_os().map(|(key, _)| key)) {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        }
    }
    command
}

fn isolated_env(
    keys: impl IntoIterator<Item = OsString>,
) -> impl Iterator<Item = (OsString, Option<OsString>)> {
    keys.into_iter()
        .filter(|key| {
            key.as_encoded_bytes().starts_with(b"HERDR_")
                || key == "PI_CODING_AGENT_DIR"
                || key == "PI_CONFIG_DIR"
        })
        .map(|key| (key, None))
        .chain(std::iter::once((
            OsString::from("HERDR_SESSION"),
            Some(OsString::from("default")),
        )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn isolation_covers_unknown_herdr_keys_and_preserves_other_environment() {
        let keys = [
            "HERDR_SESSION",
            "HERDR_SOCKET_PATH",
            "HERDR_CONFIG_PATH",
            "HERDR_TEST_FUTURE_SETTING",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
            "HOME",
            "XDG_RUNTIME_DIR",
            "NOT_HERDR_ENV",
        ];
        let overrides: Vec<_> = isolated_env(keys.map(OsString::from)).collect();
        assert_eq!(overrides.len(), 7);
        for (key, value) in &overrides[..6] {
            assert!(
                key.as_encoded_bytes().starts_with(b"HERDR_")
                    || key == "PI_CODING_AGENT_DIR"
                    || key == "PI_CONFIG_DIR"
            );
            assert_eq!(value, &None);
        }
        let default_session = (
            OsString::from("HERDR_SESSION"),
            Some(OsString::from("default")),
        );
        assert_eq!(overrides.last(), Some(&default_session));
        assert_eq!(isolated_env([]).collect::<Vec<_>>(), vec![default_session]);
    }

    #[test]
    fn both_constructors_isolate_inherited_context_and_allow_explicit_overrides() {
        let mut cli = herdr_command();
        let mut pty = herdr_pty_command();
        for (key, _) in std::env::vars_os() {
            if (key.as_encoded_bytes().starts_with(b"HERDR_") && key != "HERDR_SESSION")
                || key == "PI_CODING_AGENT_DIR"
                || key == "PI_CONFIG_DIR"
            {
                assert!(cli
                    .get_envs()
                    .any(|(name, value)| name == key.as_os_str() && value.is_none()));
                assert_eq!(pty.get_env(&key), None);
            }
        }
        assert!(cli.get_envs().any(|(key, value)| {
            key == "HERDR_SESSION" && value == Some(OsStr::new("default"))
        }));
        assert_eq!(pty.get_env("HERDR_SESSION"), Some(OsStr::new("default")));

        for (key, value) in [
            ("HERDR_SESSION", "named-session"),
            ("HERDR_SOCKET_PATH", "test.sock"),
            ("HERDR_ENV", "1"),
            ("HERDR_TEST_HANDOFF_IMPORT_FAIL", "hang_before_ready"),
            ("PI_CODING_AGENT_DIR", "test-agent-profile"),
            ("PI_CONFIG_DIR", "test-omp-profile"),
        ] {
            cli.env(key, value);
            pty.env(key, value);
            assert!(cli
                .get_envs()
                .any(|(name, actual)| { name == key && actual == Some(OsStr::new(value)) }));
            assert_eq!(pty.get_env(key), Some(OsStr::new(value)));
        }
    }

    #[test]
    fn constructors_are_checked_with_a_polluted_parent_environment() {
        // Re-exec only the constructor assertions, not Herdr or this test. This
        // exercises real inheritance without unsafe global set_var calls.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_command::tests::both_constructors_isolate_inherited_context_and_allow_explicit_overrides",
                "--nocapture",
            ])
            .env("HERDR_SESSION", "inherited-session")
            .env("HERDR_SOCKET_PATH", "inherited.sock")
            .env("HERDR_CLIENT_SOCKET_PATH", "inherited-client.sock")
            .env("HERDR_CONFIG_PATH", "inherited-config.toml")
            .env("HERDR_PANE_ID", "inherited-pane")
            .env("HERDR_ENV", "1")
            .env("HERDR_TEST_FUTURE_SETTING", "inherited-hook")
            .env("PI_CODING_AGENT_DIR", "inherited-agent-profile")
            .env("PI_CONFIG_DIR", "inherited-omp-profile")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[cfg(unix)]
    #[test]
    fn isolation_handles_non_unicode_herdr_keys() {
        use std::os::unix::ffi::OsStringExt;
        let key = OsString::from_vec(b"HERDR_\xff".to_vec());
        let overrides: Vec<_> = isolated_env([key.clone()]).collect();
        assert_eq!(overrides[0], (key, None));
    }
}
