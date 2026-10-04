//! Handle-relative, owner-private diagnostic storage. An opened directory anchors all
//! operations, so replacing its pathname cannot redirect a later open/rename/unlink.
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

#[path = "../diagnostic_storage_creation.rs"]
mod diagnostic_storage_creation;

pub(crate) struct PrivateDiagnosticDirectory(File);

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "diagnostic storage must be owner-private and not a symlink",
    )
}

fn name_cstring(name: &OsStr) -> io::Result<CString> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid diagnostic filename",
        ));
    }
    CString::new(name.as_bytes()).map_err(io::Error::other)
}

fn validate_owner(metadata: &std::fs::Metadata) -> io::Result<()> {
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "diagnostic storage is not owned by this user",
        ));
    }
    Ok(())
}

fn validate_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    validate_owner(&metadata)?;
    if !metadata.is_file() || metadata.mode() & 0o7777 != 0o600 || metadata.nlink() > 1 {
        return Err(denied());
    }
    if metadata.nlink() == 0 {
        // The opened, authenticated inode may have been atomically replaced.
        // Never confuse unsafe objects with this retryable publication race.
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "diagnostic snapshot was replaced during open",
        ));
    }
    Ok(())
}

impl PrivateDiagnosticDirectory {
    pub(crate) fn open(path: &Path, create: bool) -> io::Result<Self> {
        if create {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match diagnostic_storage_creation::create_private_directory(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        validate_owner(&metadata)?;
        if !metadata.is_dir() || metadata.mode() & 0o7777 != 0o700 {
            return Err(denied());
        }
        Ok(Self(file))
    }

    /// Nonblocking: the foreground loop must never wait for another publisher.
    /// The descriptor's Drop releases this advisory lock, including on crashes.
    pub(crate) fn lock(&self) -> io::Result<()> {
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn open_file(&self, name: &OsStr) -> io::Result<File> {
        self.open_file_with(name, |_, _| Ok(()))
    }

    // A per-call seam permits exact open -> replace -> validate tests, with no
    // global hook or timing dependency. Production passes a no-op closure.
    fn open_file_with(
        &self,
        name: &OsStr,
        mut before_validate: impl FnMut(&File, usize) -> io::Result<()>,
    ) -> io::Result<File> {
        let name = name_cstring(name)?;
        for attempt in 0..3 {
            // O_NONBLOCK prevents a substituted FIFO from hanging before fstat validation.
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openat returned an owned descriptor.
            let file = unsafe { File::from_raw_fd(fd) };
            before_validate(&file, attempt)?;
            match validate_file(&file) {
                Ok(()) => return Ok(file),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "diagnostic snapshot replacement retry limit reached",
        ))
    }

    pub(crate) fn create_file(&self, name: &OsStr) -> io::Result<File> {
        let name = name_cstring(name)?;
        let file = diagnostic_storage_creation::create_private_file_at(&self.0, &name)?;
        validate_file(&file)?;
        Ok(file)
    }

    pub(crate) fn replace(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        let source = name_cstring(source)?;
        let destination = name_cstring(destination)?;
        if unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                source.as_ptr(),
                self.0.as_raw_fd(),
                destination.as_ptr(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn remove(&self, name: &OsStr) -> io::Result<()> {
        // Validate the opened object before unlinking. Other users cannot exchange an
        // entry in this 0700 directory; same-user malicious mutation is outside this boundary.
        self.open_file(name)?;
        let name = name_cstring(name)?;
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn names(&self, limit: usize) -> io::Result<Vec<OsString>> {
        let (names, complete) = self.scan()?.next_batch(limit)?;
        if !complete {
            return Err(io::Error::other(
                "too many client machine diagnostic entries to scan",
            ));
        }
        Ok(names)
    }

    pub(crate) fn scan(&self) -> io::Result<DiagnosticDirectoryScan> {
        // Open a new description, not dup: directory offsets must not be shared across scans.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        let stream = unsafe { libc::fdopendir(file.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // fdopendir owns this descriptor after success; Scan::drop releases it.
        let _ = file.into_raw_fd();
        Ok(DiagnosticDirectoryScan {
            stream,
            identity: (metadata.dev(), metadata.ino()),
            pending: None,
        })
    }
}

pub(crate) struct DiagnosticDirectoryScan {
    stream: *mut libc::DIR,
    identity: (u64, u64),
    pending: Option<OsString>,
}

// SAFETY: the stream has one owner and readdir is only called through &mut self.
// Moving ownership to another thread does not share access to the DIR object.
unsafe impl Send for DiagnosticDirectoryScan {}

impl Drop for DiagnosticDirectoryScan {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.stream) };
    }
}

impl DiagnosticDirectoryScan {
    pub(crate) fn matches(&self, directory: &PrivateDiagnosticDirectory) -> io::Result<bool> {
        let metadata = directory.0.metadata()?;
        Ok(self.identity == (metadata.dev(), metadata.ino()))
    }

    pub(crate) fn next_batch(&mut self, limit: usize) -> io::Result<(Vec<OsString>, bool)> {
        let mut names = Vec::new();
        if let Some(name) = self.pending.take() {
            names.push(name);
        }
        loop {
            // readdir's null return is EOF or an error. Reset errno for an exact distinction.
            unsafe { *errno() = 0 };
            let entry = unsafe { libc::readdir(self.stream) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error);
                }
                return Ok((names, true));
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let name = OsString::from_vec(name.to_vec());
            if names.len() == limit {
                self.pending = Some(name);
                return Ok((names, false));
            }
            names.push(name);
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> *mut libc::c_int {
    libc::__errno_location()
}
#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    libc::__error()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
unsafe fn errno() -> *mut libc::c_int {
    // Other Unix targets use the BSD libc contract.
    libc::__error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn diagnostic_directory_handle_cannot_be_redirected_after_open() {
        let root = std::env::temp_dir().join(format!(
            "herdr-diagnostic-anchor-{}",
            crate::client::endpoint::ProfileId::generate()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let original = root.join("original");
        let directory = PrivateDiagnosticDirectory::open(&original, true).unwrap();
        std::fs::rename(&original, root.join("moved")).unwrap();
        symlink(&root, &original).unwrap();
        directory.create_file(OsStr::new("probe")).unwrap();
        assert!(root.join("moved/probe").exists());
        assert!(!root.join("probe").exists());
        assert!(PrivateDiagnosticDirectory::open(&original, false).is_err());
        std::fs::set_permissions(
            root.join("moved/probe"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            directory
                .open_file(OsStr::new("probe"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_open_refuses_symlinks_hardlinks_and_fifos_without_blocking() {
        let root = std::env::temp_dir().join(format!(
            "herdr-diagnostic-objects-{}",
            crate::client::endpoint::ProfileId::generate()
        ));
        let directory = PrivateDiagnosticDirectory::open(&root, true).unwrap();
        directory.create_file(OsStr::new("regular")).unwrap();
        symlink(root.join("regular"), root.join("symlink")).unwrap();
        assert!(directory.open_file(OsStr::new("symlink")).is_err());
        std::fs::hard_link(root.join("regular"), root.join("hardlink")).unwrap();
        assert!(directory.open_file(OsStr::new("hardlink")).is_err());
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(
            directory
                .open_file(OsStr::new("fifo"))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_atomic_replace_reopens_authenticated_unlinked_snapshot() {
        use std::io::{Read, Write};
        let root = std::env::temp_dir().join(format!(
            "herdr-diagnostic-replace-{}",
            crate::client::endpoint::ProfileId::generate()
        ));
        let directory = PrivateDiagnosticDirectory::open(&root, true).unwrap();
        directory
            .create_file(OsStr::new("snapshot"))
            .unwrap()
            .write_all(b"old")
            .unwrap();
        let mut opens = 0;
        let mut file = directory
            .open_file_with(OsStr::new("snapshot"), |old, attempt| {
                opens += 1;
                if attempt == 0 {
                    directory
                        .create_file(OsStr::new("next"))?
                        .write_all(b"new")?;
                    directory.replace(OsStr::new("next"), OsStr::new("snapshot"))?;
                    assert_eq!(old.metadata()?.nlink(), 0);
                }
                Ok(())
            })
            .unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "new");
        assert_eq!(opens, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_atomic_replace_retry_exhaustion_is_bounded_and_unavailable() {
        let root = std::env::temp_dir().join(format!(
            "herdr-diagnostic-exhaustion-{}",
            crate::client::endpoint::ProfileId::generate()
        ));
        let directory = PrivateDiagnosticDirectory::open(&root, true).unwrap();
        directory.create_file(OsStr::new("snapshot")).unwrap();
        let mut opens = 0;
        let error = directory
            .open_file_with(OsStr::new("snapshot"), |old, _| {
                opens += 1;
                directory.create_file(OsStr::new("next"))?;
                directory.replace(OsStr::new("next"), OsStr::new("snapshot"))?;
                assert_eq!(old.metadata()?.nlink(), 0);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(opens, 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_atomic_replace_does_not_retry_unsafe_mode_or_multiple_links() {
        let root = std::env::temp_dir().join(format!(
            "herdr-diagnostic-unsafe-replace-{}",
            crate::client::endpoint::ProfileId::generate()
        ));
        let directory = PrivateDiagnosticDirectory::open(&root, true).unwrap();
        directory.create_file(OsStr::new("snapshot")).unwrap();
        let mut opens = 0;
        let error = directory
            .open_file_with(OsStr::new("snapshot"), |old, _| {
                opens += 1;
                directory.create_file(OsStr::new("next"))?;
                directory.replace(OsStr::new("next"), OsStr::new("snapshot"))?;
                assert_eq!(old.metadata()?.nlink(), 0);
                old.set_permissions(std::fs::Permissions::from_mode(0o644))?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(opens, 1, "unsafe unlinked inode must not consume retries");

        std::fs::hard_link(root.join("snapshot"), root.join("hardlink")).unwrap();
        opens = 0;
        let error = directory
            .open_file_with(OsStr::new("snapshot"), |_, _| {
                opens += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(opens, 1, "multiple links are not a replacement transient");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_opened_file_rejects_foreign_owner_and_nonregular_objects() {
        let foreign = File::open("/etc/passwd").unwrap();
        if foreign.metadata().unwrap().uid() != unsafe { libc::geteuid() } {
            assert_eq!(
                validate_owner(&foreign.metadata().unwrap())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            let error = validate_file(&foreign).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                error.to_string().contains("not owned"),
                "must reject the opened foreign owner, not merely its mode"
            );
        }
        assert_eq!(
            validate_file(&File::open("/").unwrap()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
