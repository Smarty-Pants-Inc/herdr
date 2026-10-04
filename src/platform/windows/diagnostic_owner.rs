use std::io;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub(crate) fn diagnostic_owner_identity(pid: u32) -> io::Result<Option<String>> {
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process ID",
        ));
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // Keep the process handle through both queries; PID reuse cannot change its object.
    let result = (|| {
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(handle, &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if exit_code != 259 {
            // STILL_ACTIVE
            return Ok(None);
        }
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit: FILETIME = unsafe { std::mem::zeroed() };
        let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
        let mut user: FILETIME = unsafe { std::mem::zeroed() };
        if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // A terminated process may itself return 259. Its nonzero exit time
        // distinguishes that exit code from a genuinely STILL_ACTIVE process.
        if exit.dwHighDateTime != 0 || exit.dwLowDateTime != 0 {
            return Ok(None);
        }
        let birth = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        // Creation FILETIME is absolute (100 ns since 1601), not boot-relative ticks.
        Ok(Some(format!("windows:{birth}")))
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}
