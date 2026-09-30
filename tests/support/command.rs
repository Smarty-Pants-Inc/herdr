//! Child-only environment isolation for integration-test Herdr processes.
//!
//! Always construct the command with these helpers before applying test-specific
//! overrides. Agent profile overrides are also removed so integration commands
//! use the test's HOME rather than changing the invoking agent's live profile.
//! Do not mutate the test runner's environment: tests run in parallel.

use std::ffi::{OsStr, OsString};
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
    sanitize_pty_command_env(&mut command);
    command
}

/// Sanitize the complete PTY snapshot before applying test-specific overrides.
pub fn sanitize_pty_command_env(command: &mut CommandBuilder) {
    let keys: Vec<_> = command
        .iter_full_env()
        .map(|(key, _)| key.to_os_string())
        .collect();
    for (key, value) in isolated_env(keys) {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        }
    }
}

fn is_isolated_env_key(key: &OsStr, windows: bool) -> bool {
    let bytes = key.as_encoded_bytes();
    if windows {
        bytes
            .get(..b"HERDR_".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"HERDR_"))
            || bytes.eq_ignore_ascii_case(b"PI_CODING_AGENT_DIR")
            || bytes.eq_ignore_ascii_case(b"PI_CONFIG_DIR")
    } else {
        bytes.starts_with(b"HERDR_") || bytes == b"PI_CODING_AGENT_DIR" || bytes == b"PI_CONFIG_DIR"
    }
}

fn isolated_env(
    keys: impl IntoIterator<Item = OsString>,
) -> impl Iterator<Item = (OsString, Option<OsString>)> {
    keys.into_iter()
        .filter(|key| is_isolated_env_key(key, cfg!(windows)))
        .map(|key| (key, None))
        .chain(std::iter::once((
            OsString::from("HERDR_SESSION"),
            Some(OsString::from("default")),
        )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_matches_platform_case_policy_and_exact_profile_keys() {
        for (key, unix, windows) in [
            ("HERDR_", true, true),
            ("HERDR_FUTURE_SETTING", true, true),
            ("hErDr_SOCKET_PATH", false, true),
            ("herdr_session", false, true),
            ("PI_CODING_AGENT_DIR", true, true),
            ("pI_cOdInG_aGeNt_DiR", false, true),
            ("PI_CONFIG_DIR", true, true),
            ("pi_config_dir", false, true),
            ("PI_CONFIG_DIR_EXTRA", false, false),
            ("PI_CODING_AGENT_DIRECTORY", false, false),
            ("Pİ_CONFIG_DIR", false, false),
            ("HERDR", false, false),
            ("NOT_HERDR_ENV", false, false),
            ("HOME", false, false),
            ("", false, false),
        ] {
            assert_eq!(is_isolated_env_key(OsStr::new(key), false), unix, "{key}");
            assert_eq!(is_isolated_env_key(OsStr::new(key), true), windows, "{key}");
        }
    }

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
            assert!(is_isolated_env_key(key, cfg!(windows)));
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
        for (key, value) in std::env::vars_os() {
            let is_session = if cfg!(windows) {
                key.as_encoded_bytes()
                    .eq_ignore_ascii_case(b"HERDR_SESSION")
            } else {
                key == "HERDR_SESSION"
            };
            if is_isolated_env_key(&key, cfg!(windows)) && !is_session {
                assert!(cli
                    .get_envs()
                    .any(|(name, value)| name == key.as_os_str() && value.is_none()));
                assert_eq!(pty.get_env(&key), None);
            } else if !is_isolated_env_key(&key, cfg!(windows)) {
                assert!(!cli.get_envs().any(|(name, _)| name == key.as_os_str()));
                // Windows registry entries may legitimately override ordinary
                // parent values in portable-pty's base snapshot.
                if !cfg!(windows) {
                    assert_eq!(pty.get_env(&key), Some(value.as_os_str()));
                }
            }
        }
        assert!(cli.get_envs().any(|(key, value)| {
            key == "HERDR_SESSION" && value == Some(OsStr::new("default"))
        }));
        assert_eq!(pty.get_env("HERDR_SESSION"), Some(OsStr::new("default")));

        #[cfg(windows)]
        for effective_key in [
            "HERDR_MIXED_CASE_SETTING",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
        ] {
            if std::env::var_os(effective_key).is_some() {
                assert!(cli.get_envs().any(|(key, value)| {
                    key.as_encoded_bytes()
                        .eq_ignore_ascii_case(effective_key.as_bytes())
                        && value.is_none()
                }));
                assert_eq!(pty.get_env(effective_key), None);
            }
        }

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
            .env("hErDr_MIXED_CASE_SETTING", "inherited-mixed-herdr")
            .env("pI_cOdInG_aGeNt_DiR", "inherited-mixed-agent-profile")
            .env("pI_cOnFiG_dIr", "inherited-mixed-omp-profile")
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

    #[test]
    fn pty_sanitizer_clears_builder_only_context_and_allows_explicit_overrides() {
        let mut command = CommandBuilder::new("herdr-test");
        let keys = [
            "HERDR_SESSION",
            "HERDR_BUILDER_ONLY_SETTING",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
            "hErDr_BUILDER_ONLY_SETTING",
            "pI_cOdInG_aGeNt_DiR",
            "pI_cOnFiG_dIr",
        ];
        for key in keys {
            command.env(key, "builder-only");
        }
        command.env("UNRELATED_BUILDER_ONLY_SETTING", "keep");
        sanitize_pty_command_env(&mut command);
        for key in keys {
            if key == "HERDR_SESSION" {
                assert_eq!(command.get_env(key), Some(OsStr::new("default")));
            } else if is_isolated_env_key(OsStr::new(key), cfg!(windows)) {
                assert_eq!(command.get_env(key), None, "{key}");
            } else {
                assert_eq!(command.get_env(key), Some(OsStr::new("builder-only")));
            }
        }
        assert_eq!(
            command.get_env("UNRELATED_BUILDER_ONLY_SETTING"),
            Some(OsStr::new("keep"))
        );
        for key in keys {
            command.env(key, "explicit-override");
            assert_eq!(command.get_env(key), Some(OsStr::new("explicit-override")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn isolation_handles_non_unicode_herdr_keys() {
        use std::os::unix::ffi::OsStringExt;
        let key = OsString::from_vec(b"HERDR_\xff".to_vec());
        let overrides: Vec<_> = isolated_env([key.clone()]).collect();
        assert_eq!(overrides[0], (key, None));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn pty_sanitizer_handles_builder_only_non_unicode_keys_and_values() {
        #[cfg(unix)]
        let (key, value) = {
            use std::os::unix::ffi::OsStringExt;
            (
                OsString::from_vec(b"HERDR_\xff".to_vec()),
                OsString::from_vec(b"profile-\xff".to_vec()),
            )
        };
        #[cfg(windows)]
        let (key, value) = {
            use std::os::windows::ffi::OsStringExt;
            let mut key: Vec<_> = "hErDr_".encode_utf16().collect();
            key.push(0xd800);
            (OsString::from_wide(&key), OsString::from_wide(&[0xd800]))
        };
        let mut command = CommandBuilder::new("herdr-test");
        command.env(&key, &value);
        command.env("HERDR_BUILDER_ONLY_SETTING", &value);
        command.env("PI_CODING_AGENT_DIR", &value);
        command.env("PI_CONFIG_DIR", &value);
        command.env("UNRELATED_BUILDER_ONLY_SETTING", &value);
        sanitize_pty_command_env(&mut command);
        for key in [
            key.as_os_str(),
            OsStr::new("HERDR_BUILDER_ONLY_SETTING"),
            OsStr::new("PI_CODING_AGENT_DIR"),
            OsStr::new("PI_CONFIG_DIR"),
        ] {
            assert_eq!(command.get_env(key), None);
        }
        assert_eq!(
            command.get_env("UNRELATED_BUILDER_ONLY_SETTING"),
            Some(value.as_os_str())
        );
    }
}
