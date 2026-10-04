use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt as _, SecurityDescriptor,
};
use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use widestring::U16CString;
use windows_sys::Win32::{
    Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)").map_err(io::Error::other)?;
    let descriptor = SecurityDescriptor::deserialize(&sddl)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let path = extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned an owned handle; File closes it exactly once.
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    let sddl = U16CString::from_str("D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;OW)")
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let descriptor = SecurityDescriptor::deserialize(&sddl)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let path = extended_length_path(path)?;
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn extended_length_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt as _;
    let path = std::path::absolute(path)?;
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut extended = if wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
        || wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16])
    {
        wide
    } else if wide.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        "\\\\?\\UNC\\"
            .encode_utf16()
            .chain(wide.into_iter().skip(2))
            .collect()
    } else {
        "\\\\?\\".encode_utf16().chain(wide).collect()
    };
    extended.push(0);
    Ok(extended)
}
