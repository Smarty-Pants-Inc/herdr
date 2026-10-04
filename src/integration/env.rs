use std::io;
use std::path::PathBuf;
#[cfg(all(test, windows))]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(test)]
pub(crate) use super::test_support::{integration_test_env, IntegrationTestEnv};

use portable_pty::CommandBuilder;

pub(crate) const HERDR_PANE_ID_ENV_VAR: &str = "HERDR_PANE_ID";
pub(crate) const HERDR_TAB_ID_ENV_VAR: &str = "HERDR_TAB_ID";
pub(crate) const HERDR_WORKSPACE_ID_ENV_VAR: &str = "HERDR_WORKSPACE_ID";

pub(crate) const PI_CODING_AGENT_DIR_ENV_VAR: &str = "PI_CODING_AGENT_DIR";
pub(crate) const OMP_CONFIG_DIR_ENV_VAR: &str = "PI_CONFIG_DIR";
pub(crate) const CLAUDE_CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const CODEX_HOME_ENV_VAR: &str = "CODEX_HOME";
pub(crate) const KIMI_CODE_HOME_ENV_VAR: &str = "KIMI_CODE_HOME";
pub(crate) const COPILOT_HOME_ENV_VAR: &str = "COPILOT_HOME";
pub(crate) const QODERCLI_CONFIG_DIR_ENV_VAR: &str = "QODER_CONFIG_DIR";
pub(crate) const QWEN_HOME_ENV_VAR: &str = "QWEN_HOME";
pub(crate) const CURSOR_CONFIG_DIR_ENV_VAR: &str = "CURSOR_CONFIG_DIR";
pub(crate) const ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";
pub(crate) const GROK_CONFIG_DIR_ENV_VAR: &str = "GROK_CONFIG_DIR";
/// The grok CLI's own config-home override (documented alongside
/// `$GROK_HOME/config.toml` and `$GROK_HOME/auth.json`).
pub(crate) const GROK_HOME_ENV_VAR: &str = "GROK_HOME";
pub(crate) const HERMES_HOME_ENV_VAR: &str = "HERMES_HOME";

/// Integration path lookup; unscoped tests and production use the process environment.
pub(crate) fn var_os(key: &str) -> Option<std::ffi::OsString> {
    #[cfg(test)]
    if let Some(value) = super::test_support::env_override(key) {
        return value;
    }
    std::env::var_os(key)
}

pub(crate) fn apply_pane_base_env(cmd: &mut CommandBuilder) {
    cmd.env(crate::api::SOCKET_PATH_ENV_VAR, crate::api::socket_path());
    if let Ok(executable) = crate::platform::launch_executable() {
        cmd.env("HERDR_BIN_PATH", executable);
    }
}

pub(crate) fn pi_extension_dir() -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(PI_CODING_AGENT_DIR_ENV_VAR, &[".pi", "agent"])?
            .join("extensions"),
    )
}

pub(crate) fn omp_extension_dir() -> io::Result<PathBuf> {
    if let Some(value) = var_os(PI_CODING_AGENT_DIR_ENV_VAR).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("extensions"));
    }

    let config_dir = var_os(OMP_CONFIG_DIR_ENV_VAR)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    Ok(home_dir()?
        .join(config_dir)
        .join("agent")
        .join("extensions"))
}

pub(crate) fn claude_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CLAUDE_CONFIG_DIR_ENV_VAR, &[".claude"])
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CODEX_HOME_ENV_VAR, &[".codex"])
}

pub(crate) fn kimi_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(KIMI_CODE_HOME_ENV_VAR, &[".kimi-code"])
}

pub(crate) fn copilot_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(COPILOT_HOME_ENV_VAR, &[".copilot"])
}

pub(crate) fn devin_dir() -> io::Result<PathBuf> {
    if let Some(value) = var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("devin"));
    }

    #[cfg(windows)]
    if let Some(value) = var_os("APPDATA").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value).join("devin"));
    }

    Ok(home_dir()?.join(".config").join("devin"))
}

pub(crate) fn droid_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".factory"))
}

pub(crate) fn config_dir_from_env_or_home(
    env_var: &str,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = var_os(env_var).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    let mut path = home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(crate) fn expand_tilde_path(path: PathBuf) -> io::Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path);
    };

    if raw == "~" {
        return home_dir();
    }

    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| raw.strip_prefix('~'))
    {
        return Ok(home_dir()?.join(rest));
    }

    Ok(path)
}

pub(crate) fn opencode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/opencode"))
}

pub(crate) fn opencode_state_dir() -> io::Result<PathBuf> {
    if let Some(value) = var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/state/opencode"))
}

pub(crate) fn kilo_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/kilo"))
}

pub(crate) fn hermes_dir() -> io::Result<PathBuf> {
    if let Some(value) = var_os(HERMES_HOME_ENV_VAR).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    #[cfg(windows)]
    {
        let explicit_home = var_os("HOME").filter(|value| !value.is_empty());
        let profile = var_os("USERPROFILE").filter(|value| !value.is_empty());
        if let Some(home) = explicit_home.filter(|home| profile.as_ref() != Some(home)) {
            return Ok(PathBuf::from(home).join(".hermes"));
        }
        if let Some(local_app_data) = var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(local_app_data).join("hermes"));
        }
    }

    Ok(home_dir()?.join(".hermes"))
}

pub(crate) fn hermes_plugin_dir() -> io::Result<PathBuf> {
    Ok(hermes_dir()?
        .join("plugins")
        .join(super::HERMES_PLUGIN_INSTALL_NAME))
}

pub(crate) fn qodercli_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QODERCLI_CONFIG_DIR_ENV_VAR, &[".qoder"])
}

pub(crate) fn qwen_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QWEN_HOME_ENV_VAR, &[".qwen"])
}

pub(crate) fn letta_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".letta"))
}

pub(crate) fn cursor_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CURSOR_CONFIG_DIR_ENV_VAR, &[".cursor"])
}

pub(crate) fn mastracode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".mastracode"))
}

pub(crate) fn antigravity_cli_dir() -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &[".gemini", "config"])
}

pub(crate) fn grok_dir() -> io::Result<PathBuf> {
    // GROK_CONFIG_DIR is a herdr-level override only (primarily a test
    // seam); the grok CLI does not honor it, so it stays first and explicit.
    if let Some(value) = var_os(GROK_CONFIG_DIR_ENV_VAR).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(GROK_HOME_ENV_VAR, &[".grok"])
}

pub(crate) fn home_dir() -> io::Result<PathBuf> {
    if let Some(home) = var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(profile) = var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
            var_os("HOMEPATH").filter(|value| !value.is_empty()),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Ok(home);
        }
    }

    Err(io::Error::other(
        "home directory is not set; cannot locate home directory",
    ))
}

/// Legacy serialization for Windows sound/platform subprocess tests only.
/// Integration tests use the lock-free `integration_test_env` scope instead.
#[cfg(all(test, windows))]
pub(crate) fn integration_env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_test_env_masks_parent_path_and_config_defaults() {
        let _env = integration_test_env();
        for key in [
            "HOME",
            "PATH",
            "APPDATA",
            "LOCALAPPDATA",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            PI_CODING_AGENT_DIR_ENV_VAR,
            OMP_CONFIG_DIR_ENV_VAR,
            CLAUDE_CONFIG_DIR_ENV_VAR,
            CODEX_HOME_ENV_VAR,
            HERMES_HOME_ENV_VAR,
        ] {
            assert_eq!(var_os(key), None, "{key} must not fall back to the parent");
        }
        assert!(home_dir().is_err());
    }

    #[test]
    fn integration_test_env_restores_nested_scope_after_unwind() {
        let parent_home = std::env::var_os("HOME");
        let parent_path = std::env::var_os("PATH");
        {
            let outer = integration_test_env();
            let home = std::env::temp_dir().join("herdr-outer-home");
            outer.set("HOME", &home);
            outer.set("PATH", "outer-path");
            outer.set("XDG_STATE_HOME", "outer-state");
            let result = std::panic::catch_unwind(|| {
                let inner = integration_test_env();
                inner.set("HOME", "inner-home");
                inner.set("PATH", "inner-path");
                inner.set("XDG_STATE_HOME", "inner-state");
                panic!("test fixture failed");
            });
            assert!(result.is_err());
            assert_eq!(home_dir().unwrap(), home);
            assert_eq!(var_os("PATH"), Some("outer-path".into()));
            assert_eq!(
                opencode_state_dir().unwrap(),
                PathBuf::from("outer-state/opencode")
            );
        }
        assert_eq!(std::env::var_os("HOME"), parent_home);
        assert_eq!(std::env::var_os("PATH"), parent_path);
        assert_eq!(var_os("HOME"), parent_home);
        assert_eq!(var_os("PATH"), parent_path);
        // A failed fixture cannot poison a later scope or leave its HOME behind.
        let fresh = integration_test_env();
        assert!(home_dir().is_err());
        fresh.set("HOME", "fresh-home");
        assert_eq!(home_dir().unwrap(), PathBuf::from("fresh-home"));
    }

    #[test]
    fn integration_test_env_isolates_overlapping_threads() {
        let parent_home = std::env::var_os("HOME");
        let parent_path = std::env::var_os("PATH");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            let handles = ["a", "b"].map(|label| {
                let barrier = &barrier;
                threads.spawn(move || {
                    let env = integration_test_env();
                    let home = std::env::temp_dir().join(format!("herdr-isolated-{label}"));
                    let state = home.join("state");
                    let config = home.join("config");
                    env.set("HOME", &home);
                    env.set("PATH", home.join("bin"));
                    env.set("XDG_STATE_HOME", &state);
                    env.set("XDG_CONFIG_HOME", &config);
                    barrier.wait();
                    assert_eq!(home_dir().unwrap(), home);
                    assert_eq!(opencode_state_dir().unwrap(), state.join("opencode"));
                    assert_eq!(devin_dir().unwrap(), config.join("devin"));
                    assert_eq!(var_os("PATH"), Some(home.join("bin").into_os_string()));
                })
            });
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(std::env::var_os("HOME"), parent_home);
        assert_eq!(std::env::var_os("PATH"), parent_path);
    }

    #[cfg(windows)]
    #[test]
    fn integration_test_env_covers_windows_home_fallbacks() {
        let env = integration_test_env();
        env.set("HOMEDRIVE", "C:");
        env.set("HOMEPATH", r"\Users\integration");
        assert_eq!(home_dir().unwrap(), PathBuf::from(r"C:\Users\integration"));
        env.set("USERPROFILE", r"C:\Users\profile");
        assert_eq!(home_dir().unwrap(), PathBuf::from(r"C:\Users\profile"));
        env.set("HOME", r"C:\fixture-home");
        assert_eq!(home_dir().unwrap(), PathBuf::from(r"C:\fixture-home"));
        env.set("HOME", "");
        assert_eq!(home_dir().unwrap(), PathBuf::from(r"C:\Users\profile"));
        env.remove("USERPROFILE");
        env.remove("HOMEPATH");
        assert!(home_dir().is_err());
    }

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let env = integration_test_env();
        let home = std::env::temp_dir().join("herdr-opencode-state-home");
        env.set("HOME", &home);
        assert_eq!(
            opencode_state_dir().unwrap(),
            home.join(".local/state/opencode")
        );
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let env = integration_test_env();
        let xdg = std::env::temp_dir().join("herdr-xdg-state");
        env.set("XDG_STATE_HOME", &xdg);
        assert_eq!(opencode_state_dir().unwrap(), xdg.join("opencode"));
    }
}
