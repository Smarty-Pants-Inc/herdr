//! Fail closed on platforms without owner-private diagnostic storage primitives.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::Path;

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private client diagnostic storage is unsupported",
    ))
}

pub(crate) struct PrivateDiagnosticDirectory;
pub(crate) struct DiagnosticDirectoryScan;
impl PrivateDiagnosticDirectory {
    pub(crate) fn open(_path: &Path, _create: bool) -> io::Result<Self> {
        unsupported()
    }
    pub(crate) fn lock(&self) -> io::Result<()> {
        unsupported()
    }
    pub(crate) fn open_file(&self, _name: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(crate) fn create_file(&self, _name: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub(crate) fn replace(&self, _source: &OsStr, _destination: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(crate) fn remove(&self, _name: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub(crate) fn names(&self, _limit: usize) -> io::Result<Vec<OsString>> {
        unsupported()
    }
    pub(crate) fn scan(&self) -> io::Result<DiagnosticDirectoryScan> {
        unsupported()
    }
}
impl DiagnosticDirectoryScan {
    pub(crate) fn matches(&self, _directory: &PrivateDiagnosticDirectory) -> io::Result<bool> {
        unsupported()
    }
    pub(crate) fn next_batch(&mut self, _limit: usize) -> io::Result<(Vec<OsString>, bool)> {
        unsupported()
    }
}
