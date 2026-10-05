use std::fmt;
use std::io;
use std::sync::Arc;

/// A writer-boundary authorization check for a prompt submission.
///
/// The check is deliberately owned by the I/O actor and evaluated at the last
/// possible moment, rather than when the submission is queued.
#[derive(Clone)]
pub(crate) struct SubmissionGuard {
    check: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
}

impl SubmissionGuard {
    pub(crate) fn new<F>(check: F) -> Self
    where
        F: Fn() -> io::Result<()> + Send + Sync + 'static,
    {
        Self {
            check: Arc::new(check),
        }
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        // A failed authorization check is not a PTY I/O failure: actors must
        // reject only this submission, including when validation itself fails.
        (self.check)().map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err))
    }
}

impl fmt::Debug for SubmissionGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubmissionGuard")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn clones_share_owned_callback_and_debug_does_not_evaluate_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = SubmissionGuard::new({
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        assert!(format!("{guard:?}").contains("SubmissionGuard"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        guard.check().unwrap();
        guard.clone().check().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn validation_failure_is_a_permission_refusal_not_an_actor_io_failure() {
        let guard =
            SubmissionGuard::new(|| Err(io::Error::new(io::ErrorKind::NotFound, "process exited")));
        let error = guard.check().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("process exited"));
    }
}
