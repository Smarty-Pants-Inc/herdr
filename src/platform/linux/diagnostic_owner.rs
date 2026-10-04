use std::io::{self, Read};

pub(crate) fn diagnostic_owner_identity(pid: u32) -> io::Result<Option<String>> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process ID",
        ));
    }
    // Read through one proc inode; a later occupant of the numeric PID cannot
    // substitute its stat file midway through this lookup. Bound all kernel input.
    let file = match std::fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut stat = String::new();
    file.take(8193).read_to_string(&mut stat)?;
    if stat.len() > 8192 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized proc stat",
        ));
    }
    let Some(start) = live_start_time(&stat)? else {
        return Ok(None);
    };
    let mut boot = String::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")?
        .take(129)
        .read_to_string(&mut boot)?;
    let boot = boot.trim();
    if boot.len() != 36
        || !boot
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid kernel boot ID",
        ));
    }
    Ok(Some(format!("linux:{boot}:{start}")))
}

fn live_start_time(stat: &str) -> io::Result<Option<u64>> {
    // comm is parenthesized and may itself contain spaces or closing parentheses.
    // The last ')' precedes field 3; starttime is field 22, offset 19 from there.
    let mut fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields.split_whitespace())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc fields"))?;
    let state = fields
        .next()
        .filter(|state| state.len() == 1 && state.as_bytes()[0].is_ascii_alphabetic())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc state"))?;
    let start = fields
        .nth(18)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing proc starttime"))?;
    // Stopped/traced tasks (T/t) still own their snapshots. Zombies and dead
    // tasks retain their birth field until reaped, but cannot publish again.
    Ok((!matches!(state, "Z" | "X" | "x")).then_some(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_start_time_ignores_parenthesized_command_contents() {
        let fields = (4..=21).map(|_| "0").collect::<Vec<_>>().join(" ");
        for state in ["R", "S", "T", "t"] {
            assert_eq!(
                live_start_time(&format!(
                    "123 (command ) with spaces) {state} {fields} 456 789"
                ))
                .unwrap(),
                Some(456)
            );
        }
        for state in ["Z", "X", "x"] {
            assert_eq!(
                live_start_time(&format!("123 (cmd) {state} {fields} 456")).unwrap(),
                None
            );
        }
        assert!(live_start_time("123 (truncated)").is_err());
        assert!(live_start_time(&format!("123 (cmd) S {fields} invalid")).is_err());
        assert!(live_start_time(&format!("123 (cmd) STOP {fields} 456")).is_err());
    }
}
