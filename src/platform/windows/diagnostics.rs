//! Private diagnostics on Windows. No-delete-sharing directory/ancestor handles pin
//! the pathname; opened objects are checked for owner, DACL and reparse points.
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetKernelObjectSecurity, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, TokenOwner, TokenUser, WinLocalSystemSid, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID, TOKEN_OWNER,
        TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, LockFileEx, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OPEN_EXISTING,
    },
    System::{
        Threading::{GetCurrentProcess, OpenProcessToken},
        IO::OVERLAPPED,
    },
};

pub(crate) fn diagnostic_process_exists(pid: u32) -> bool {
    pid != 0 && super::process_exists(pid)
}

pub(crate) struct PrivateDiagnosticDirectory {
    path: PathBuf,
    _ancestors: Vec<File>,
    lock: RefCell<Option<File>>,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "diagnostic storage must be owner-private and not a reparse point",
    )
}

fn open_handle(path: &Path, writable: bool) -> io::Result<File> {
    let path = super::extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | if writable { GENERIC_WRITE } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned an owned handle.
    let file = unsafe { File::from_raw_handle(handle) };
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(denied());
    }
    Ok(file)
}

fn validate_private(file: &File, directory: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(denied());
    }
    if !directory {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.nNumberOfLinks != 1 {
            return Err(denied());
        }
    }
    let mut needed = 0;
    let flags = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    unsafe { GetKernelObjectSecurity(file.as_raw_handle(), flags, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut descriptor = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            flags,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut owner: PSID = null_mut();
    let mut defaulted = 0;
    let mut dacl: *mut ACL = null_mut();
    let mut present = 0;
    if unsafe {
        GetSecurityDescriptorOwner(descriptor.as_mut_ptr().cast(), &mut owner, &mut defaulted)
    } == 0
        || unsafe {
            GetSecurityDescriptorDacl(
                descriptor.as_mut_ptr().cast(),
                &mut present,
                &mut dacl,
                &mut defaulted,
            )
        } == 0
        || owner.is_null()
        || present == 0
        || dacl.is_null()
    {
        return Err(denied());
    }
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = super::token_information(token, TokenUser);
    let default_owner = super::token_information(token, TokenOwner);
    unsafe { CloseHandle(token) };
    let (user, default_owner) = (user?, default_owner?);
    let user_sid = unsafe { (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let default_owner_sid = unsafe { (*(default_owner.as_ptr().cast::<TOKEN_OWNER>())).Owner };
    if unsafe { EqualSid(owner, user_sid) } == 0
        && unsafe { EqualSid(owner, default_owner_sid) } == 0
    {
        return Err(denied());
    }
    let mut system = [0u64; 9];
    let mut length = std::mem::size_of_val(&system) as u32;
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            null_mut(),
            system.as_mut_ptr().cast(),
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // S-1-3-4 (OWNER RIGHTS), used by existing private-file/directory helpers.
    let mut owner_rights: [u32; 3] = [0x00000101, 0x03000000, 4];
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, u32::from(index), &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let kind = unsafe { (*(ace.cast::<ACE_HEADER>())).AceType };
        if kind == 1 {
            continue;
        } // A deny ACE never grants access.
        if kind != 0 {
            return Err(denied());
        } // Fail closed on other grant forms.
        let sid = unsafe { (&raw mut (*(ace.cast::<ACCESS_ALLOWED_ACE>())).SidStart).cast() };
        if unsafe { EqualSid(sid, user_sid) } == 0
            && unsafe { EqualSid(sid, system.as_mut_ptr().cast()) } == 0
            && unsafe { EqualSid(sid, owner_rights.as_mut_ptr().cast()) } == 0
        {
            return Err(denied());
        }
    }
    Ok(())
}

impl PrivateDiagnosticDirectory {
    pub(crate) fn open(path: &Path, create: bool) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        if create {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match super::create_remote_private_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let mut ancestors = Vec::new();
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            ancestors.push(open_handle(&prefix, false)?);
        }
        let leaf = ancestors.last().ok_or_else(denied)?;
        validate_private(leaf, true)?;
        Ok(Self {
            path,
            _ancestors: ancestors,
            lock: RefCell::new(None),
        })
    }

    fn path(&self, name: &OsStr) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
            || name.to_string_lossy().contains(':')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid diagnostic filename",
            ));
        }
        Ok(self.path.join(name))
    }

    pub(crate) fn lock(&self) -> io::Result<()> {
        let path = self.path(OsStr::new(".publish-lock"))?;
        match super::create_config_temporary(&path, true) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let file = open_handle(&path, true)?;
        validate_private(&file, false)?;
        let mut overlapped = OVERLAPPED::default();
        if unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        *self.lock.borrow_mut() = Some(file);
        Ok(())
    }

    pub(crate) fn open_file(&self, name: &OsStr) -> io::Result<File> {
        let file = open_handle(&self.path(name)?, false)?;
        validate_private(&file, false)?;
        Ok(file)
    }
    pub(crate) fn create_file(&self, name: &OsStr) -> io::Result<File> {
        let file = super::create_config_temporary(&self.path(name)?, true)?;
        validate_private(&file, false)?;
        Ok(file)
    }
    pub(crate) fn replace(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        super::replace_file(&self.path(source)?, &self.path(destination)?)
    }
    pub(crate) fn remove(&self, name: &OsStr) -> io::Result<()> {
        self.open_file(name)?;
        std::fs::remove_file(self.path(name)?)
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
        let anchors = self
            ._ancestors
            .iter()
            .map(File::try_clone)
            .collect::<io::Result<Vec<_>>>()?;
        let identity = identity(self._ancestors.last().ok_or_else(denied)?)?;
        Ok(DiagnosticDirectoryScan {
            _anchors: anchors,
            identity,
            entries: std::fs::read_dir(&self.path)?,
            pending: None,
        })
    }
}

fn identity(file: &File) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

pub(crate) struct DiagnosticDirectoryScan {
    _anchors: Vec<File>,
    identity: (u32, u32, u32),
    entries: std::fs::ReadDir,
    pending: Option<OsString>,
}

impl DiagnosticDirectoryScan {
    pub(crate) fn matches(&self, directory: &PrivateDiagnosticDirectory) -> io::Result<bool> {
        Ok(self.identity == identity(directory._ancestors.last().ok_or_else(denied)?)?)
    }
    pub(crate) fn next_batch(&mut self, limit: usize) -> io::Result<(Vec<OsString>, bool)> {
        let mut names = Vec::new();
        if let Some(name) = self.pending.take() {
            names.push(name);
        }
        for entry in self.entries.by_ref() {
            let name = entry?.file_name();
            if name == ".publish-lock" {
                continue;
            }
            if names.len() == limit {
                self.pending = Some(name);
                return Ok((names, false));
            }
            names.push(name);
        }
        Ok((names, true))
    }
}
