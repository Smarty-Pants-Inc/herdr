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

const ROOT_PREFIX: &str = "herdr-omp-";
#[cfg(unix)]
const OWNER_METADATA_FILE: &str = ".herdr-owner.json";

#[cfg(unix)]
#[derive(serde::Deserialize, serde::Serialize)]
struct OwnerMetadata {
    schema: u8,
    pid: u32,
    process_start_identity: Option<u64>,
}

/// Ephemeral HOME and OMP state owned by one hidden guest process tree.
pub(crate) struct OmpGuestStateDir {
    root: PathBuf,
}

impl OmpGuestStateDir {
    pub(crate) fn new() -> io::Result<Self> {
        Self::new_in(&std::env::temp_dir())
    }

    fn new_in(temp_dir: &Path) -> io::Result<Self> {
        reclaim_stale_roots(temp_dir);
        for _ in 0..8 {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let root = temp_dir.join(format!("{ROOT_PREFIX}{}-{suffix}", std::process::id()));
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    let initialized = (|| {
                        set_private_permissions(&root)?;
                        write_owner_metadata(&root)?;
                        let agent_dir = root.join(".omp/agent");
                        std::fs::create_dir_all(&agent_dir)?;
                        set_private_permissions(&root.join(".omp"))?;
                        set_private_permissions(&agent_dir)
                    })();
                    if let Err(error) = initialized {
                        let _ = std::fs::remove_dir_all(&root);
                        return Err(error);
                    }
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

#[cfg(unix)]
fn write_owner_metadata(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let owner = OwnerMetadata {
        schema: 1,
        pid: std::process::id(),
        process_start_identity: crate::platform::process_start_identity(std::process::id()),
    };
    let path = root.join(OWNER_METADATA_FILE);
    let bytes = serde_json::to_vec(&owner).map_err(|error| io::Error::other(error.to_string()))?;
    std::fs::write(&path, bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_owner_metadata(_root: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reclaim_stale_roots(temp_dir: &Path) {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    let current_uid = unsafe { libc::geteuid() };
    for entry in entries.flatten() {
        let Some(pid) = guest_root_pid(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_dir()
            || metadata.uid() != current_uid
            || metadata.mode() & 0o777 != 0o700
        {
            continue;
        }
        let owner_path = path.join(OWNER_METADATA_FILE);
        let Ok(owner_file) = std::fs::symlink_metadata(&owner_path) else {
            continue;
        };
        if !owner_file.file_type().is_file()
            || owner_file.uid() != current_uid
            || owner_file.mode() & 0o077 != 0
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(owner_path) else {
            continue;
        };
        let Ok(owner) = serde_json::from_slice::<OwnerMetadata>(&bytes) else {
            continue;
        };
        if owner.schema == 1 && owner.pid == pid && owner_process_is_inactive(&owner) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn guest_root_pid(name: &std::ffi::OsStr) -> Option<u32> {
    let value = name.to_str()?.strip_prefix(ROOT_PREFIX)?;
    let (pid, suffix) = value.split_once('-')?;
    if suffix.len() != 32
        || !suffix
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    pid.parse().ok()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn owner_process_is_inactive(owner: &OwnerMetadata) -> bool {
    owner_process_is_inactive_with(
        owner,
        crate::platform::process_exists,
        crate::platform::process_start_identity,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn owner_process_is_inactive_with(
    owner: &OwnerMetadata,
    process_exists: impl FnOnce(u32) -> bool,
    process_start_identity: impl FnOnce(u32) -> Option<u64>,
) -> bool {
    if let (Some(expected), Some(actual)) = (
        owner.process_start_identity,
        process_start_identity(owner.pid),
    ) {
        return expected != actual;
    }
    !process_exists(owner.pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reclaim_stale_roots(_temp_dir: &Path) {}

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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unique_test_base(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "herdr-omp-state-test-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&base).unwrap();
        base
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn wait_for_child_marker(marker: &Path, child: &mut std::process::Child) -> PathBuf {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(value) = std::fs::read_to_string(marker) {
                if !value.trim().is_empty() {
                    return PathBuf::from(value);
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("guest root owner exited before publishing its root: {status}");
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for guest root owner");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn isolated_state_is_private_and_removed_on_drop() {
        let state = OmpGuestStateDir::new().unwrap();
        let root = state.root().to_path_buf();
        assert!(root.join(".omp/agent").is_dir());
        #[cfg(unix)]
        assert!(root.join(OWNER_METADATA_FILE).is_file());
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn abrupt_owner_death_root_is_reclaimed_on_restart() {
        const BASE_ENV: &str = "HERDR_TEST_STALE_OMP_ROOT_BASE";
        const MARKER_ENV: &str = "HERDR_TEST_STALE_OMP_ROOT_MARKER";
        if let (Some(base), Some(marker)) =
            (std::env::var_os(BASE_ENV), std::env::var_os(MARKER_ENV))
        {
            let state = OmpGuestStateDir::new_in(Path::new(&base)).unwrap();
            std::fs::write(marker, state.root().as_os_str().as_encoded_bytes()).unwrap();
            unsafe { libc::_exit(0) }
        }

        let base = unique_test_base("abrupt");
        let marker = base.join("root-path");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "omp_guest_state::tests::abrupt_owner_death_root_is_reclaimed_on_restart",
                "--nocapture",
            ])
            .env(BASE_ENV, &base)
            .env(MARKER_ENV, &marker)
            .status()
            .unwrap();
        assert!(status.success());
        let stale_root = PathBuf::from(std::fs::read_to_string(&marker).unwrap());
        assert!(stale_root.is_dir());

        let restarted = OmpGuestStateDir::new_in(&base).unwrap();
        assert!(!stale_root.exists());
        drop(restarted);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn concurrent_live_owner_root_is_preserved() {
        const BASE_ENV: &str = "HERDR_TEST_LIVE_OMP_ROOT_BASE";
        const MARKER_ENV: &str = "HERDR_TEST_LIVE_OMP_ROOT_MARKER";
        const RELEASE_ENV: &str = "HERDR_TEST_LIVE_OMP_ROOT_RELEASE";
        if let (Some(base), Some(marker), Some(release)) = (
            std::env::var_os(BASE_ENV),
            std::env::var_os(MARKER_ENV),
            std::env::var_os(RELEASE_ENV),
        ) {
            let state = OmpGuestStateDir::new_in(Path::new(&base)).unwrap();
            std::fs::write(marker, state.root().as_os_str().as_encoded_bytes()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !Path::new(&release).exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(Path::new(&release).exists());
            return;
        }

        let base = unique_test_base("live");
        let marker = base.join("root-path");
        let release = base.join("release");
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "omp_guest_state::tests::concurrent_live_owner_root_is_preserved",
                "--nocapture",
            ])
            .env(BASE_ENV, &base)
            .env(MARKER_ENV, &marker)
            .env(RELEASE_ENV, &release)
            .spawn()
            .unwrap();
        let live_root = wait_for_child_marker(&marker, &mut owner);
        assert!(live_root.is_dir());

        let concurrent = OmpGuestStateDir::new_in(&base).unwrap();
        assert!(live_root.is_dir());
        drop(concurrent);

        std::fs::write(&release, b"release").unwrap();
        assert!(owner.wait().unwrap().success());
        assert!(!live_root.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn owner_launch_identity_distinguishes_pid_reuse() {
        let owner = OwnerMetadata {
            schema: 1,
            pid: 42,
            process_start_identity: Some(7),
        };
        assert!(!owner_process_is_inactive_with(
            &owner,
            |_| true,
            |_| Some(7)
        ));
        assert!(owner_process_is_inactive_with(
            &owner,
            |_| true,
            |_| Some(8)
        ));
        assert!(!owner_process_is_inactive_with(&owner, |_| true, |_| None));
        assert!(owner_process_is_inactive_with(&owner, |_| false, |_| None));
    }
}
