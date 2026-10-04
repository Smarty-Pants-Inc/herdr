use std::io;

pub(crate) fn diagnostic_owner_identity(pid: u32) -> io::Result<Option<String>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process ID",
        ));
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if read != size {
        let error = io::Error::last_os_error();
        if read <= 0 && error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(if read > 0 {
            io::Error::new(io::ErrorKind::InvalidData, "short process birth query")
        } else {
            error
        });
    }
    if info.pbi_status == libc::SZOMB {
        return Ok(None);
    }
    // BSD start timeval is absolute wall-clock birth, rather than uptime ticks:
    // unlike Linux starttime it already distinguishes process starts across boots.
    Ok(Some(format!(
        "macos:{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}
