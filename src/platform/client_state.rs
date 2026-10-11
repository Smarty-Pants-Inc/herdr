use std::path::Path;

#[cfg(not(windows))]
pub(crate) fn create_private_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::create_remote_ssh_config_file(path)
}

#[cfg(windows)]
pub(crate) fn create_private_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::windows::create_remote_ssh_config_file(path)
}

#[cfg(not(windows))]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    super::windows::replace_file(source, destination)
}

#[cfg(not(windows))]
pub(crate) fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    // replace_file uses MOVEFILE_WRITE_THROUGH on Windows.
    Ok(())
}

/// Opens `path` for reading and appending as an owner-only log. It creates the file with mode
/// 0600, does not follow a final symlink, refuses anything but a regular file owned by this
/// user, and restricts an existing file that others can read or write to 0600.
#[cfg(unix)]
pub(crate) fn open_private_append_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other("the log is not a regular file"));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("the log is not owned by this user"));
    }
    if metadata.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Whether `path` still names the open `file`, checked without following a final symlink:
/// `Some(true)` only if the entry is a regular file owned by this user with the handle's
/// (dev, ino); `Some(false)` if it names anything else or nothing.
#[cfg(unix)]
pub(crate) fn path_names_open_file(path: &Path, file: &std::fs::File) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let (Ok(live), Ok(held)) = (std::fs::symlink_metadata(path), file.metadata()) else {
        return Some(false);
    };
    Some(
        live.file_type().is_file()
            && live.uid() == unsafe { libc::geteuid() }
            && (live.dev(), live.ino()) == (held.dev(), held.ino()),
    )
}

/// `None`: identity is not checked here, so callers never rename the entry. On Windows the
/// private log handle denies delete sharing, so its entry cannot be renamed or replaced while
/// it is open, and log rotation is a documented no-op (records append in place).
#[cfg(not(unix))]
pub(crate) fn path_names_open_file(_path: &Path, _file: &std::fs::File) -> Option<bool> {
    None
}

/// Opens `path` as a private log; see `windows::open_private_log_file`. The handle is opened
/// for writing, not appending, so callers must write at the end of the file.
#[cfg(windows)]
pub(crate) fn open_private_append_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::windows::open_private_log_file(path)
}

/// Opens `path` for reading and appending. No private-file support on this platform.
#[cfg(not(any(unix, windows)))]
pub(crate) fn open_private_append_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)
}
