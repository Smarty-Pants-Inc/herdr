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

/// The length of the private log at `path` if it is a regular file owned by this user, read
/// without following a final symlink. `None` when it is missing, a link, another kind of file,
/// foreign, or unreadable: size-based rotation never moves what the private open would refuse.
#[cfg(unix)]
pub(crate) fn private_log_len(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    (metadata.file_type().is_file() && metadata.uid() == euid).then_some(metadata.len())
}

/// See the Unix version. Windows metadata from a path cannot name the owner, so after the
/// no-follow type check this probes through the private open, which refuses reparse points,
/// non-files and foreign owners. The probe handle is closed on return, before any rename.
#[cfg(windows)]
pub(crate) fn private_log_len(path: &Path) -> Option<u64> {
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let file = super::windows::open_private_log_file(path).ok()?;
    file.metadata().ok().map(|metadata| metadata.len())
}

/// See the Unix version. No ownership support on this platform.
#[cfg(not(any(unix, windows)))]
pub(crate) fn private_log_len(path: &Path) -> Option<u64> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.file_type().is_file().then_some(metadata.len())
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
