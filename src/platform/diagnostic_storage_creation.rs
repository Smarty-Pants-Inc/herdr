//! Native owner-private creation shared by diagnostic publishers and CLI fixtures.
//! Self-contained so integration tests can include the production helpers directly.
#[cfg(unix)]
#[path = "unix_common/diagnostic_creation.rs"]
mod native;
#[cfg(windows)]
#[path = "windows/diagnostic_creation.rs"]
mod native;

pub(crate) use native::*;
