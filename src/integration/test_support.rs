//! Shared helpers for integration tests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;

use super::env::{
    ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, CLAUDE_CONFIG_DIR_ENV_VAR, CODEX_HOME_ENV_VAR,
    COPILOT_HOME_ENV_VAR, CURSOR_CONFIG_DIR_ENV_VAR, GROK_CONFIG_DIR_ENV_VAR, GROK_HOME_ENV_VAR,
    HERMES_HOME_ENV_VAR, KIMI_CODE_HOME_ENV_VAR, OMP_CONFIG_DIR_ENV_VAR,
    PI_CODING_AGENT_DIR_ENV_VAR, QODERCLI_CONFIG_DIR_ENV_VAR, QWEN_HOME_ENV_VAR,
};

type TestEnvironment = BTreeMap<&'static str, Option<OsString>>;

thread_local! {
    static TEST_ENV: RefCell<Option<TestEnvironment>> = const { RefCell::new(None) };
}

/// A hermetic integration environment on this thread, never the process environment.
/// Keep nested scopes in lexical (LIFO) order; dropping a scope also restores its
/// predecessor during unwinding. The marker prevents moving a guard to another thread.
pub(crate) struct IntegrationTestEnv {
    previous: Option<TestEnvironment>,
    _thread_bound: PhantomData<Rc<()>>,
}

pub(crate) fn integration_test_env() -> IntegrationTestEnv {
    // Explicit absence prevents unrelated tests' XDG or Windows home overrides
    // from influencing fixture defaults. HOME and PATH must be supplied locally.
    let env = [
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
        KIMI_CODE_HOME_ENV_VAR,
        COPILOT_HOME_ENV_VAR,
        QODERCLI_CONFIG_DIR_ENV_VAR,
        QWEN_HOME_ENV_VAR,
        CURSOR_CONFIG_DIR_ENV_VAR,
        ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR,
        GROK_CONFIG_DIR_ENV_VAR,
        GROK_HOME_ENV_VAR,
        HERMES_HOME_ENV_VAR,
    ]
    .into_iter()
    .map(|key| (key, None))
    .collect();
    IntegrationTestEnv {
        previous: TEST_ENV.with(|current| current.replace(Some(env))),
        _thread_bound: PhantomData,
    }
}

impl IntegrationTestEnv {
    pub(crate) fn set(&self, key: &'static str, value: impl AsRef<OsStr>) {
        TEST_ENV.with(|current| {
            current
                .borrow_mut()
                .as_mut()
                .expect("integration environment scope is active")
                .insert(key, Some(value.as_ref().to_owned()));
        });
    }

    pub(crate) fn remove(&self, key: &'static str) {
        TEST_ENV.with(|current| {
            current
                .borrow_mut()
                .as_mut()
                .expect("integration environment scope is active")
                .insert(key, None);
        });
    }
}

impl Drop for IntegrationTestEnv {
    fn drop(&mut self) {
        TEST_ENV.with(|current| current.replace(self.previous.take()));
    }
}

/// Outer None means unscoped/unhandled; inner None explicitly masks the parent key.
pub(super) fn env_override(key: &str) -> Option<Option<OsString>> {
    TEST_ENV.with(|current| current.borrow().as_ref()?.get(key).cloned())
}

/// Match the injected availability lookup and pass overrides to this child only.
/// Resolve bare programs explicitly: Windows may otherwise search the parent's
/// PATH even when a different child PATH is configured. Unscoped tests use the
/// same command builder and inherited environment as production.
pub(super) fn command(program: &str) -> std::io::Result<std::process::Command> {
    let Some(env) = TEST_ENV.with(|current| current.borrow().clone()) else {
        return Ok(crate::noninteractive_process::command(program));
    };
    let path = Path::new(program);
    let resolved = if path.is_absolute() || path.components().count() > 1 {
        path.to_owned()
    } else {
        env.get("PATH")
            .and_then(Option::as_ref)
            .and_then(|paths| {
                std::env::split_paths(paths)
                    .flat_map(|dir| super::registry::command_path_candidates(&dir, program))
                    .find(|path| super::registry::executable_file_exists(path))
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{program} is not on the integration test PATH"),
                )
            })?
    };
    // An empty/relative PATH entry can yield a bare relative filename. Make it
    // absolute as well, without resolving symlinks or searching the parent PATH.
    let resolved = if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()?.join(resolved)
    };
    let mut command = crate::noninteractive_process::command(resolved);
    for (key, value) in env {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    Ok(command)
}

/// Windows refuses file symlinks with `ERROR_PRIVILEGE_NOT_HELD` when the
/// process is neither elevated nor running with Developer Mode enabled.
#[cfg(windows)]
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

#[cfg(windows)]
fn symlink_privilege_denied(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD)
}

/// Create a file symlink, returning `false` when Windows denied the symlink
/// privilege. Callers skip symlink-only assertions on `false` instead of
/// failing an ordinary non-elevated local test run.
#[cfg(unix)]
pub(super) fn symlink_file(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
    true
}

#[cfg(windows)]
pub(super) fn symlink_file(target: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => true,
        Err(error) if symlink_privilege_denied(&error) => {
            eprintln!("skipping symlink test: Windows denied SeCreateSymbolicLinkPrivilege");
            false
        }
        Err(error) => panic!("create symlink {}: {error}", link.display()),
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn command_carries_local_home_path_and_windows_overrides() {
        let env = integration_test_env();
        for key in [
            "HOME",
            "PATH",
            "APPDATA",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
        ] {
            env.set(key, format!("local-{key}"));
        }
        // Explicit relative programs must not turn back into a parent PATH search.
        let child = command("relative/agent").unwrap();
        assert_eq!(
            child.get_program(),
            std::env::current_dir()
                .unwrap()
                .join("relative/agent")
                .as_os_str()
        );
        let overrides = child
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect::<BTreeMap<_, _>>();
        for key in [
            "HOME",
            "PATH",
            "APPDATA",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
        ] {
            assert_eq!(
                overrides.get(OsStr::new(key)),
                Some(&Some(OsString::from(format!("local-{key}")))),
                "{key} must be injected into the child only"
            );
        }
        assert_eq!(overrides.get(OsStr::new("XDG_CONFIG_HOME")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("XDG_STATE_HOME")), Some(&None));
        assert_eq!(overrides.get(OsStr::new("LOCALAPPDATA")), Some(&None));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn symlink_privilege_error_is_detected() {
        assert!(symlink_privilege_denied(
            &std::io::Error::from_raw_os_error(ERROR_PRIVILEGE_NOT_HELD)
        ));
        assert!(!symlink_privilege_denied(
            &std::io::Error::from_raw_os_error(/* ERROR_ACCESS_DENIED */ 5)
        ));
    }
}
