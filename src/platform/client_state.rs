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
