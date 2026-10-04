//! Process environment reads with thread-local overrides in unit tests.
//!
//! Production reads and writes retain the standard-library behavior. Test
//! writes require an active scope and never change the process environment.

use std::env::VarError;
use std::ffi::{OsStr, OsString};

pub(crate) fn var(key: &str) -> Result<String, VarError> {
    #[cfg(test)]
    {
        var_os(key)
            .ok_or(VarError::NotPresent)?
            .into_string()
            .map_err(VarError::NotUnicode)
    }
    #[cfg(not(test))]
    std::env::var(key)
}

pub(crate) fn var_os(key: &str) -> Option<OsString> {
    #[cfg(test)]
    if let Some(value) = TEST_ENV.with(|scopes| {
        scopes
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| scope.get(OsStr::new(key)).cloned())
    }) {
        return value;
    }
    std::env::var_os(key)
}

pub(crate) fn set_var(key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
    #[cfg(test)]
    override_var(key.as_ref(), Some(value.as_ref().to_os_string()));
    #[cfg(not(test))]
    std::env::set_var(key, value);
}

pub(crate) fn remove_var(key: impl AsRef<OsStr>) {
    #[cfg(test)]
    override_var(key.as_ref(), None);
    #[cfg(not(test))]
    std::env::remove_var(key);
}

#[cfg(test)]
type Overrides = std::collections::HashMap<OsString, Option<OsString>>;

#[cfg(test)]
thread_local! {
    static TEST_ENV: std::cell::RefCell<Vec<Overrides>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn override_var(key: &OsStr, value: Option<OsString>) {
    TEST_ENV.with(|scopes| {
        scopes
            .borrow_mut()
            .last_mut()
            .expect("environment writes in tests require an active test_env scope")
            .insert(key.to_os_string(), value);
    });
}

/// An empty override scope inheriting unhandled keys from enclosing scopes,
/// then the process. Removals mask inherited values. Threads do not inherit it.
/// Guards must be dropped in reverse order; the marker makes them non-Send.
#[cfg(test)]
#[must_use]
pub(crate) struct TestEnv {
    depth: usize,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
pub(crate) fn test_env() -> TestEnv {
    let depth = TEST_ENV.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        scopes.push(Overrides::new());
        scopes.len()
    });
    TestEnv {
        depth,
        _thread_bound: std::marker::PhantomData,
    }
}

#[cfg(test)]
impl TestEnv {
    pub(crate) fn set(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        self.assert_current();
        set_var(key, value);
    }

    pub(crate) fn remove(&self, key: impl AsRef<OsStr>) {
        self.assert_current();
        remove_var(key);
    }

    fn assert_current(&self) {
        TEST_ENV.with(|scopes| {
            assert_eq!(
                scopes.borrow().len(),
                self.depth,
                "test_env scopes require LIFO use"
            );
        });
    }
}

#[cfg(test)]
impl Drop for TestEnv {
    fn drop(&mut self) {
        self.assert_current();
        TEST_ENV.with(|scopes| {
            scopes.borrow_mut().pop();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "HERDR_TEST_ENV_SCOPE_REGRESSION";

    #[test]
    fn nested_scopes_restore_on_unwind_and_leave_process_unchanged() {
        let original = std::env::var_os(KEY);
        let inherited = std::env::var_os("PATH");
        {
            let outer = test_env();
            assert_eq!(var_os(KEY), original);
            assert_eq!(var_os("PATH"), inherited);
            outer.set(OsString::from(KEY), OsString::from("outer"));
            let result = std::panic::catch_unwind(|| {
                let inner = test_env();
                assert_eq!(var(KEY).as_deref(), Ok("outer"));
                inner.remove(KEY);
                assert_eq!(var(KEY), Err(VarError::NotPresent));
                inner.remove("PATH");
                assert_eq!(var_os("PATH"), None);
                inner.set(KEY, "inner");
                assert_eq!(var(KEY).as_deref(), Ok("inner"));
                assert_eq!(std::env::var_os(KEY), original);
                assert_eq!(std::env::var_os("PATH"), inherited);
                panic!("exercise environment scope cleanup");
            });
            assert!(result.is_err());
            assert_eq!(var(KEY).as_deref(), Ok("outer"));
            assert_eq!(var_os("PATH"), inherited);
            outer.remove(KEY);
            assert_eq!(var(KEY), Err(VarError::NotPresent));
        }
        assert_eq!(var_os(KEY), original);
        assert_eq!(std::env::var_os(KEY), original);
        assert_eq!(std::env::var_os("PATH"), inherited);
    }

    #[test]
    fn overlapping_threads_have_independent_non_inherited_scopes() {
        let original = std::env::var_os(KEY);
        let parent = test_env();
        parent.set(KEY, "parent");
        let (a_ready, a_wait) = std::sync::mpsc::channel();
        let (b_ready, b_wait) = std::sync::mpsc::channel();
        std::thread::scope(|threads| {
            for (value, ready, wait) in [("a", a_ready, b_wait), ("b", b_ready, a_wait)] {
                let original = original.clone();
                threads.spawn(move || {
                    assert_eq!(var_os(KEY), original);
                    let env = test_env();
                    env.set(KEY, value);
                    ready.send(()).unwrap();
                    wait.recv_timeout(std::time::Duration::from_secs(30))
                        .expect("both scopes must overlap before observations");
                    assert_eq!(var(KEY).as_deref(), Ok(value));
                    assert_eq!(std::env::var_os(KEY), original);
                    ready.send(()).unwrap();
                    wait.recv_timeout(std::time::Duration::from_secs(30))
                        .expect("keep peer scope alive until observations finish");
                    drop(env);
                    assert_eq!(var_os(KEY), original);
                });
            }
        });
        assert_eq!(var(KEY).as_deref(), Ok("parent"));
        assert_eq!(std::env::var_os(KEY), original);
    }

    #[test]
    fn mutation_wrappers_require_a_scope() {
        assert!(std::panic::catch_unwind(|| set_var(KEY, "unguarded")).is_err());
        assert!(std::panic::catch_unwind(|| remove_var(KEY)).is_err());
        let _env = test_env();
        set_var(KEY, "guarded");
        assert_eq!(var(KEY).as_deref(), Ok("guarded"));
        remove_var(KEY);
        assert_eq!(var(KEY), Err(VarError::NotPresent));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_values_preserve_var_error() {
        use std::os::unix::ffi::OsStringExt;

        let env = test_env();
        let value = OsString::from_vec(vec![0xff]);
        env.set(KEY, &value);
        assert_eq!(var_os(KEY), Some(value.clone()));
        assert_eq!(var(KEY), Err(VarError::NotUnicode(value)));
    }
}
