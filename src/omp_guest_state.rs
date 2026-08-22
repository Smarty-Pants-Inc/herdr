use std::io;
use std::path::{Path, PathBuf};

const CLEARED_ENV: &[&str] = &[
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "OMP_PROFILE",
    "PI_PROFILE",
    "PI_CODING_AGENT_SESSION_DIR",
    "PI_CONFIG_FILES",
    "OMP_WORKTREE_DIR",
    "OMP_AUTORESEARCH_DB_DIR",
    "OMP_GITHUB_CACHE_DB",
    "OMP_AUTH_BROKER_SNAPSHOT_CACHE",
];

/// Ephemeral HOME and OMP state owned by one hidden guest process tree.
pub(crate) struct OmpGuestStateDir {
    root: PathBuf,
}

impl OmpGuestStateDir {
    pub(crate) fn new() -> io::Result<Self> {
        for _ in 0..8 {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let root =
                std::env::temp_dir().join(format!("herdr-omp-{}-{suffix}", std::process::id()));
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    set_private_permissions(&root)?;
                    let agent_dir = root.join(".omp/agent");
                    std::fs::create_dir_all(&agent_dir)?;
                    set_private_permissions(&root.join(".omp"))?;
                    set_private_permissions(&agent_dir)?;
                    return Ok(Self { root });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate isolated OMP guest state",
        ))
    }

    pub(crate) fn apply_to_command(&self, command: &mut std::process::Command) {
        for name in CLEARED_ENV {
            command.env_remove(name);
        }
        command
            .env("HOME", &self.root)
            .env("PI_CONFIG_DIR", ".omp")
            .env("PI_CODING_AGENT_DIR", self.agent_dir());
    }

    pub(crate) fn apply_to_pane_env(
        &self,
        mut launch_env: crate::pane::PaneLaunchEnv,
    ) -> crate::pane::PaneLaunchEnv {
        for name in CLEARED_ENV {
            launch_env = launch_env.without_env(*name);
        }
        launch_env
            .with_extra("HOME", self.root.to_string_lossy())
            .with_extra("PI_CONFIG_DIR", ".omp")
            .with_extra("PI_CODING_AGENT_DIR", self.agent_dir().to_string_lossy())
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn agent_dir(&self) -> PathBuf {
        self.root.join(".omp/agent")
    }
}

impl Drop for OmpGuestStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_state_is_private_and_removed_on_drop() {
        let state = OmpGuestStateDir::new().unwrap();
        let root = state.root().to_path_buf();
        assert!(root.join(".omp/agent").is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(state);
        assert!(!root.exists());
    }

    #[test]
    fn guest_command_clears_global_state_and_uses_private_home() {
        let state = OmpGuestStateDir::new().unwrap();
        let mut command = std::process::Command::new("omp");
        for name in CLEARED_ENV {
            command.env(name, "global");
        }
        state.apply_to_command(&mut command);

        let value = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .map(|(_, value)| value.map(std::ffi::OsStr::to_owned))
        };
        assert_eq!(
            value("HOME"),
            Some(Some(state.root().as_os_str().to_owned()))
        );
        assert_eq!(
            value("PI_CONFIG_DIR"),
            Some(Some(std::ffi::OsString::from(".omp")))
        );
        assert_eq!(
            value("PI_CODING_AGENT_DIR"),
            Some(Some(state.root().join(".omp/agent").into_os_string()))
        );
        for name in CLEARED_ENV {
            assert_eq!(value(name), Some(None), "{name}");
        }
    }
}
