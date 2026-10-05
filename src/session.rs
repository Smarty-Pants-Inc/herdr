use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

use crate::ipc::LocalStream;

pub const SESSION_ENV_VAR: &str = "HERDR_SESSION";
pub const DEFAULT_SESSION_NAME: &str = "default";

const MAX_SESSION_NAME_LEN: usize = 64;
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_WAIT_POLL: Duration = Duration::from_millis(25);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

#[cfg(not(test))]
static EXPLICIT_SESSION_REQUESTED: AtomicBool = AtomicBool::new(false);

// Test session selection follows the thread-local environment scope instead
// of allowing one test's CLI selection to affect another test's socket lookup.
#[cfg(test)]
thread_local! {
    static EXPLICIT_SESSION_REQUESTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn set_explicit_session_requested(requested: bool) {
    #[cfg(not(test))]
    EXPLICIT_SESSION_REQUESTED.store(requested, Ordering::Relaxed);
    #[cfg(test)]
    EXPLICIT_SESSION_REQUESTED.with(|flag| flag.set(requested));
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub socket_path: String,
    pub session_dir: String,
}

/// The prompt leaf parser owns exactly two literal argv values: TARGET and TEXT.
/// Locate those slots before either global extractor can remove flag-shaped data.
/// Only skip options these extractors remove; all other command paths keep their
/// existing full-argv scans, and options after the two slots remain global.
pub(crate) fn agent_prompt_payload_slots(args: &[String]) -> std::ops::Range<usize> {
    let mut index = 1;
    for command in ["agent", "prompt"] {
        while let Some(arg) = args.get(index) {
            match arg.as_str() {
                "--session" | "--remote" | "--remote-keybindings" => index += 2,
                "--handoff" => index += 1,
                value
                    if value.starts_with("--session=")
                        || value.starts_with("--remote=")
                        || value.starts_with("--remote-keybindings=") =>
                {
                    index += 1;
                }
                _ => break,
            }
        }
        if args.get(index).map(String::as_str) != Some(command) {
            return 0..0;
        }
        index += 1;
    }
    index..index + 2
}

pub fn configure_from_args(args: &[String]) -> Result<Vec<String>, String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    if args.get(1).map(String::as_str) == Some("session")
        && args.get(2).map(String::as_str) == Some("attach")
    {
        if matches!(
            args.get(3).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            return Ok(args.to_vec());
        }
        let Some(name) = args.get(3) else {
            return Err("usage: herdr session attach <name>".to_string());
        };
        if args.len() != 4 {
            return Err("usage: herdr session attach <name>".to_string());
        }
        apply_explicit_name(name)?;
        return Ok(cleaned);
    }

    let payload_slots = agent_prompt_payload_slots(args);
    let mut requested_session = None;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if payload_slots.contains(&index) {
            cleaned.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--session" {
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --session".to_string());
            };
            requested_session = Some(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--session=") {
            requested_session = Some(value.to_string());
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    if let Some(session) = requested_session {
        apply_explicit_name(&session)?;
    } else if crate::environment::var_os(crate::api::SOCKET_PATH_ENV_VAR).is_some() {
        set_explicit_session_requested(false);
    } else if let Ok(session) = crate::environment::var(SESSION_ENV_VAR) {
        if normalize_name(&session)?.is_none() {
            crate::environment::remove_var(SESSION_ENV_VAR);
        }
        set_explicit_session_requested(false);
    } else {
        set_explicit_session_requested(false);
    }

    Ok(cleaned)
}

pub fn active_name() -> Option<String> {
    crate::environment::var(SESSION_ENV_VAR)
        .ok()
        .filter(|name| name != DEFAULT_SESSION_NAME)
        .filter(|name| validate_name(name).is_ok())
}

pub fn local_attach_command() -> String {
    match active_name() {
        Some(name) => format!("herdr session attach {name}"),
        None => "herdr".to_string(),
    }
}

pub fn local_stop_command() -> String {
    stop_command_for(active_name().as_deref())
}

pub fn stop_command_for(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("herdr session stop {name}"),
        None => "herdr server stop".to_string(),
    }
}

pub fn restart_after_update_guidance(stop_command: &str, attach_command: Option<&str>) -> String {
    let restart = match attach_command {
        Some(command) => format!("Run `{stop_command}`, then run `{command}` again."),
        None => format!("Run `{stop_command}`, then restart Herdr with the same socket override."),
    };
    format!(
        "Stop the old server to use the new version.\nStopping exits pane processes.\n{restart}"
    )
}

pub fn active_restart_after_update_guidance() -> String {
    if !explicit_session_requested() {
        if let Ok(socket_path) = crate::environment::var(crate::api::SOCKET_PATH_ENV_VAR) {
            return restart_after_update_guidance(
                &format!(
                    "{}={} herdr server stop",
                    crate::api::SOCKET_PATH_ENV_VAR,
                    socket_path
                ),
                None,
            );
        }
    }

    restart_after_update_guidance(&local_stop_command(), Some(&local_attach_command()))
}

pub fn explicit_session_requested() -> bool {
    #[cfg(not(test))]
    {
        EXPLICIT_SESSION_REQUESTED.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    {
        EXPLICIT_SESSION_REQUESTED.with(std::cell::Cell::get)
    }
}

#[cfg(test)]
pub(crate) fn clear_explicit_session_for_test() {
    set_explicit_session_requested(false);
}

pub fn data_dir() -> PathBuf {
    data_dir_for(active_name().as_deref())
}

pub fn data_dir_for(name: Option<&str>) -> PathBuf {
    let config_dir = crate::config::config_dir();
    match name {
        Some(name) => config_dir.join("sessions").join(name),
        None => config_dir,
    }
}

pub fn api_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("herdr.sock")
}

pub fn active_api_socket_path() -> PathBuf {
    if explicit_session_requested() {
        return api_socket_path_for(active_name().as_deref());
    }
    if let Ok(path) = crate::environment::var(crate::api::SOCKET_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    api_socket_path_for(active_name().as_deref())
}

pub fn client_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("herdr-client.sock")
}

pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    let mut sessions = vec![session_info(None)];
    let sessions_dir = crate::config::config_dir().join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(err) => return Err(err),
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name != DEFAULT_SESSION_NAME && validate_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort();
    sessions.extend(names.iter().map(|name| session_info(Some(name))));
    Ok(sessions)
}

pub fn session_info(name: Option<&str>) -> SessionInfo {
    let default = name.is_none();
    let display_name = name.unwrap_or(DEFAULT_SESSION_NAME).to_string();
    let socket_path = api_socket_path_for(name);
    let session_dir = data_dir_for(name);
    SessionInfo {
        name: display_name,
        default,
        running: is_running_at(&socket_path),
        socket_path: socket_path.display().to_string(),
        session_dir: session_dir.display().to_string(),
    }
}

pub fn parse_target_name(name: &str) -> Result<Option<String>, String> {
    normalize_name(name)
}

pub fn stop_session(name: Option<&str>) -> Result<SessionInfo, String> {
    stop_session_with_timeout(name, STOP_WAIT_TIMEOUT)
}

pub(crate) fn stop_active_server() -> Result<(), String> {
    let socket_path = active_api_socket_path();
    let client_socket_path = crate::server::socket_paths::client_socket_path();
    stop_socket_with_timeout(
        socket_path.clone(),
        vec![socket_path, client_socket_path],
        STOP_WAIT_TIMEOUT,
        "server",
    )
}

fn stop_session_with_timeout(name: Option<&str>, timeout: Duration) -> Result<SessionInfo, String> {
    let socket_path = api_socket_path_for(name);
    let client_socket_path = client_socket_path_for(name);
    let label = format!("session {}", name.unwrap_or(DEFAULT_SESSION_NAME));
    stop_socket_with_timeout(
        socket_path.clone(),
        vec![socket_path, client_socket_path],
        timeout,
        &label,
    )?;
    Ok(session_info(name))
}

fn stop_socket_with_timeout(
    socket_path: PathBuf,
    stopped_socket_paths: Vec<PathBuf>,
    timeout: Duration,
    label: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let request = serde_json::json!({
        "id": "cli:session:stop",
        "method": "server.stop",
        "params": {}
    });
    let stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        format!(
            "{label} is not running or cannot be reached at {}: {err}",
            socket_path.display()
        )
    })?;
    let stop_response = send_stop_request(stream, &request, deadline)?;
    if let Some(response) = stop_response {
        if let Some(error) = response.get("error") {
            return Err(error.to_string());
        }
    }
    if !wait_until_stopped_until(&stopped_socket_paths, deadline) {
        let reachable = reachable_socket_paths(&stopped_socket_paths);
        return Err(format!(
            "{label} did not stop within {}ms; sockets are still reachable at {}",
            timeout.as_millis(),
            reachable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

pub fn delete_session(name: &str) -> Result<SessionInfo, String> {
    if name == DEFAULT_SESSION_NAME {
        return Err("deleting the default session is not supported".to_string());
    }
    validate_name(name)?;
    let Some(dir) = exact_session_dir_for_delete(name)? else {
        return Ok(session_info(Some(name)));
    };
    let socket_path = dir.join("herdr.sock");
    if is_running_at(&socket_path) {
        return Err(format!(
            "session {name} is running; stop it before deleting"
        ));
    }
    let info = session_info(Some(name));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(info),
        Err(err) => Err(err.to_string()),
    }
}

fn exact_session_dir_for_delete(name: &str) -> Result<Option<PathBuf>, String> {
    let sessions_dir = crate::config::config_dir().join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry.file_name() == std::ffi::OsStr::new(name) {
            return Ok(Some(entry.path()));
        }
    }

    // A path lookup alone can resolve a different spelling on case-insensitive
    // filesystems. Never probe its socket or delete it without an exact entry.
    match std::fs::symlink_metadata(sessions_dir.join(name)) {
        Ok(_) => Err(format!(
            "session {name} does not match an exact session name; use the spelling shown by `herdr session list`"
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request(
    mut stream: LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, String> {
    let Some(write_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_send_timeout(Some(write_timeout)) {
        if !stop_timeout_error_allows_wait(&err) {
            return Err(err.to_string());
        }
    }

    let response = send_stop_request_inner(&mut stream, request, deadline);
    match response {
        Ok(Some(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|err| err.to_string()),
        Ok(None) => Ok(None),
        Err(err) if stop_request_error_allows_wait(&err) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request_inner(
    stream: &mut LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    stream.write_all(request.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let Some(read_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_recv_timeout(Some(read_timeout)) {
        if stop_timeout_error_allows_wait(&err) {
            return Ok(None);
        }
        return Err(err);
    }

    let mut line = String::new();
    let bytes_read = BufReader::new(stream).read_line(&mut line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn stop_timeout_error_allows_wait(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::InvalidInput
        || (cfg!(windows) && err.kind() == std::io::ErrorKind::Unsupported)
}

fn stop_request_error_allows_wait(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    )
}

fn is_running_at(socket_path: &Path) -> bool {
    socket_path.exists() && crate::ipc::connect_local_stream(socket_path).is_ok()
}

fn wait_until_stopped_until(socket_paths: &[PathBuf], deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if socket_paths.iter().all(|path| !is_running_at(path)) {
            return true;
        }
        std::thread::sleep(STOP_WAIT_POLL.min(time_until(deadline)));
    }
    socket_paths.iter().all(|path| !is_running_at(path))
}

fn reachable_socket_paths(socket_paths: &[PathBuf]) -> Vec<PathBuf> {
    socket_paths
        .iter()
        .filter(|path| is_running_at(path))
        .cloned()
        .collect()
}

fn time_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn socket_timeout_until(deadline: Instant) -> Option<Duration> {
    socket_timeout_from_remaining(time_until(deadline))
}

fn socket_timeout_from_remaining(remaining: Duration) -> Option<Duration> {
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.max(MIN_SOCKET_TIMEOUT))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(format!(
            "session name cannot be longer than {MAX_SESSION_NAME_LEN} bytes"
        ));
    }
    if name == "." || name == ".." {
        return Err("session name cannot be . or ..".to_string());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "session name may only contain ASCII letters, numbers, '.', '_' and '-'".to_string(),
        );
    }
    Ok(())
}

fn apply_explicit_name(name: &str) -> Result<(), String> {
    let session = normalize_name(name)?;
    if let Some(session) = session {
        crate::environment::set_var(SESSION_ENV_VAR, session);
    } else {
        crate::environment::remove_var(SESSION_ENV_VAR);
    }
    set_explicit_session_requested(true);
    Ok(())
}

fn normalize_name(name: &str) -> Result<Option<String>, String> {
    if name == DEFAULT_SESSION_NAME {
        return Ok(None);
    }
    validate_name(name)?;
    Ok(Some(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use interprocess::local_socket::traits::Listener as _;
    #[cfg(unix)]
    use std::sync::atomic::Ordering;

    #[cfg(unix)]
    fn unique_test_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
    }

    #[cfg(unix)]
    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, std::path::PathBuf) {
        let path = unique_test_path(name);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    #[test]
    fn stop_wait_timeout_allows_slow_graceful_shutdown() {
        assert_eq!(STOP_WAIT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn stop_request_errors_wait_for_socket_state() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
        ] {
            let err = std::io::Error::from(kind);
            assert!(stop_request_error_allows_wait(&err), "{kind:?}");
        }
    }

    #[test]
    fn stop_timeout_invalid_input_waits_for_socket_state() {
        let err = std::io::Error::from(std::io::ErrorKind::InvalidInput);

        assert!(stop_timeout_error_allows_wait(&err));
    }

    #[test]
    fn stop_timeout_unsupported_waits_for_socket_state_only_on_windows() {
        let err = std::io::Error::from(std::io::ErrorKind::Unsupported);

        assert_eq!(stop_timeout_error_allows_wait(&err), cfg!(windows));
    }

    #[test]
    fn socket_timeouts_are_never_zero_duration() {
        assert_eq!(socket_timeout_from_remaining(Duration::ZERO), None);
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_nanos(1)),
            Some(MIN_SOCKET_TIMEOUT)
        );
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_millis(10)),
            Some(Duration::from_millis(10))
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_request_empty_response_waits_for_socket_state() {
        let (client, server, _path) = local_stream_pair("stop-empty-response");
        let handle = std::thread::spawn(move || {
            let mut request = String::new();
            let _ = BufReader::new(server).read_line(&mut request);
            request
        });
        let request = serde_json::json!({
            "id": "cli:session:stop",
            "method": "server.stop",
            "params": {}
        });

        assert_eq!(
            send_stop_request(
                client,
                &request,
                Instant::now() + Duration::from_millis(100)
            )
            .unwrap(),
            None
        );
        assert!(handle.join().unwrap().contains("server.stop"));
    }

    #[cfg(unix)]
    #[test]
    fn stop_session_times_out_when_socket_stays_open_without_response() {
        let env = crate::environment::test_env();
        let config_home = PathBuf::from(format!("/tmp/hs-stop-open-{}", std::process::id()));
        env.set("XDG_CONFIG_HOME", &config_home);
        let session_name = "silent";
        let socket_path = api_socket_path_for(Some(session_name));
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = keep_running.clone();
        let handle = std::thread::spawn(move || {
            let mut held_streams = Vec::new();
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(0) => continue,
                                Ok(_) if request.contains("server.stop") => {
                                    held_streams.push(stream)
                                }
                                Ok(_) => {}
                                Err(_) => continue,
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("silent session should fail after timeout");

        assert!(err.contains("did not stop"), "{err}");
        keep_running.store(false, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&config_home);
        env.remove("XDG_CONFIG_HOME");
    }

    #[test]
    fn concurrent_session_selection_and_socket_overrides_are_thread_local() {
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for explicit in [false, true] {
                let barrier = &barrier;
                threads.spawn(move || {
                    let env = crate::environment::test_env();
                    let config_home = std::env::temp_dir().join(if explicit {
                        "herdr-explicit-session-local"
                    } else {
                        "herdr-inherited-session-local"
                    });
                    env.set("XDG_CONFIG_HOME", &config_home);
                    env.set(SESSION_ENV_VAR, "inherited");
                    env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/local-api.sock");
                    env.set("HERDR_CLIENT_SOCKET_PATH", "/tmp/local-client.sock");
                    clear_explicit_session_for_test();
                    let args = if explicit {
                        vec!["herdr".to_string(), "--session=selected".to_string()]
                    } else {
                        vec!["herdr".to_string()]
                    };
                    configure_from_args(&args).unwrap();
                    barrier.wait();

                    assert_eq!(explicit_session_requested(), explicit);
                    if explicit {
                        assert_eq!(active_name().as_deref(), Some("selected"));
                        let data_dir = config_home
                            .join(crate::config::app_dir_name())
                            .join("sessions")
                            .join("selected");
                        assert_eq!(active_api_socket_path(), data_dir.join("herdr.sock"));
                        assert_eq!(
                            crate::server::socket_paths::client_socket_path(),
                            data_dir.join("herdr-client.sock")
                        );
                    } else {
                        assert_eq!(active_name().as_deref(), Some("inherited"));
                        assert_eq!(
                            active_api_socket_path(),
                            PathBuf::from("/tmp/local-api.sock")
                        );
                        assert_eq!(
                            crate::server::socket_paths::client_socket_path(),
                            PathBuf::from("/tmp/local-api-client.sock")
                        );
                    }
                    clear_explicit_session_for_test();
                });
            }
        });
    }

    #[test]
    fn configure_from_args_removes_global_session_option() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            "work".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(
            crate::environment::var(SESSION_ENV_VAR).as_deref(),
            Ok("work")
        );
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_accepts_equals_form() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "server".to_string(),
            "stop".to_string(),
            "--session=api".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(
            crate::environment::var(SESSION_ENV_VAR).as_deref(),
            Ok("api")
        );
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr", "server", "stop"]);
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_preserves_child_session_option_after_separator() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "agent".to_string(),
            "start".to_string(),
            "repro".to_string(),
            "--".to_string(),
            "/bin/echo".to_string(),
            "--session".to_string(),
            "child-session".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(crate::environment::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_preserves_child_session_equals_option_after_separator() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "agent".to_string(),
            "start".to_string(),
            "repro".to_string(),
            "--".to_string(),
            "/bin/echo".to_string(),
            "--session=child-session".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(crate::environment::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_rewrites_session_attach_to_default_launch() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "bad/name");
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "work".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(
            crate::environment::var(SESSION_ENV_VAR).as_deref(),
            Ok("work")
        );
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr"]);
        env.remove(SESSION_ENV_VAR);
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_leaves_session_attach_help_for_cli_dispatch() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "-h".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_maps_default_session_name_to_default_path() {
        let env = crate::environment::test_env();
        let config_home =
            std::env::temp_dir().join(format!("herdr-session-default-{}", std::process::id()));
        env.set("XDG_CONFIG_HOME", &config_home);
        env.set(SESSION_ENV_VAR, "work");
        clear_explicit_session_for_test();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            DEFAULT_SESSION_NAME.to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(crate::environment::var(SESSION_ENV_VAR).is_err());
        assert!(explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            config_home
                .join(crate::config::app_dir_name())
                .join("herdr.sock")
        );
        env.remove("XDG_CONFIG_HOME");
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_session_does_not_mark_session_explicit() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "env-session");
        set_explicit_session_requested(true);
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert_eq!(
            crate::environment::var(SESSION_ENV_VAR).as_deref(),
            Ok("env-session")
        );
        assert!(!explicit_session_requested());
        env.remove(SESSION_ENV_VAR);
    }

    #[test]
    fn env_default_session_name_uses_default_path() {
        let env = crate::environment::test_env();
        let config_home =
            std::env::temp_dir().join(format!("herdr-env-session-default-{}", std::process::id()));
        env.set("XDG_CONFIG_HOME", &config_home);
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
        env.set(SESSION_ENV_VAR, DEFAULT_SESSION_NAME);
        set_explicit_session_requested(true);
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(crate::environment::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            config_home
                .join(crate::config::app_dir_name())
                .join("herdr.sock")
        );
        env.remove("XDG_CONFIG_HOME");
        env.remove(SESSION_ENV_VAR);
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn local_attach_command_uses_default_launch_for_default_session() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);

        assert_eq!(local_attach_command(), "herdr");
    }

    #[test]
    fn local_attach_command_uses_session_attach_for_named_session() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "work");

        assert_eq!(local_attach_command(), "herdr session attach work");

        env.remove(SESSION_ENV_VAR);
    }

    #[test]
    fn local_stop_command_uses_server_stop_for_default_session() {
        let env = crate::environment::test_env();
        env.remove(SESSION_ENV_VAR);

        assert_eq!(local_stop_command(), "herdr server stop");

        env.remove(SESSION_ENV_VAR);
    }

    #[test]
    fn local_stop_command_uses_session_stop_for_named_session() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "work");

        assert_eq!(local_stop_command(), "herdr session stop work");

        env.remove(SESSION_ENV_VAR);
    }

    #[test]
    fn restart_after_update_guidance_names_stop_and_attach_commands() {
        assert_eq!(
            restart_after_update_guidance(
                "herdr session stop work",
                Some("herdr session attach work")
            ),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `herdr session stop work`, then run `herdr session attach work` again."
        );
    }

    #[test]
    fn active_restart_after_update_guidance_respects_socket_override() {
        let env = crate::environment::test_env();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/custom-herdr.sock");
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();

        assert_eq!(
            active_restart_after_update_guidance(),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `HERDR_SOCKET_PATH=/tmp/custom-herdr.sock herdr server stop`, then restart Herdr with the same socket override."
        );

        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn explicit_session_socket_ignores_inherited_socket_override() {
        let env = crate::environment::test_env();
        let config_home =
            std::env::temp_dir().join(format!("herdr-session-precedence-{}", std::process::id()));
        env.set("XDG_CONFIG_HOME", &config_home);
        env.set(SESSION_ENV_VAR, "work");
        set_explicit_session_requested(true);
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");

        let path = active_api_socket_path();

        assert_eq!(
            path,
            config_home
                .join(crate::config::app_dir_name())
                .join("sessions")
                .join("work")
                .join("herdr.sock")
        );
        env.remove("XDG_CONFIG_HOME");
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_socket_override_wins_without_explicit_session() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "work");
        clear_explicit_session_for_test();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/explicit.sock");

        assert_eq!(
            active_api_socket_path(),
            PathBuf::from("/tmp/explicit.sock")
        );

        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_socket_override_skips_invalid_env_session_validation_without_explicit_session() {
        let env = crate::environment::test_env();
        env.set(SESSION_ENV_VAR, "bad/name");
        clear_explicit_session_for_test();
        env.set(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/herdr.sock");
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(!explicit_session_requested());
        assert_eq!(active_api_socket_path(), PathBuf::from("/tmp/herdr.sock"));
        assert_eq!(
            crate::environment::var(SESSION_ENV_VAR).as_deref(),
            Ok("bad/name")
        );

        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        env.remove(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[cfg(unix)]
    #[test]
    fn stop_session_fails_when_socket_remains_reachable_after_timeout() {
        let env = crate::environment::test_env();
        let config_home = PathBuf::from(format!("/tmp/hs-stop-{}", std::process::id()));
        env.set("XDG_CONFIG_HOME", &config_home);
        let session_name = "slow";
        let socket_path = api_socket_path_for(Some(session_name));
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = keep_running.clone();
        let handle = std::thread::spawn(move || {
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(0) => continue,
                                Ok(_) if request.trim().is_empty() => continue,
                                Ok(_) => {}
                                Err(_) => continue,
                            }
                        }
                        let _ = stream.write_all(b"{\"id\":\"cli:session:stop\",\"result\":{}}\n");
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("still-running session should fail");

        assert!(err.contains("did not stop"), "{err}");
        assert!(
            err.contains(socket_path.to_string_lossy().as_ref()),
            "{err}"
        );
        keep_running.store(false, Ordering::Relaxed);
        handle.join().unwrap();
        let _ = std::fs::remove_dir_all(&config_home);
        env.remove("XDG_CONFIG_HOME");
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(validate_name("../prod").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("work session").is_err());
    }

    #[test]
    fn parse_default_target_name_maps_to_default_session() {
        assert_eq!(parse_target_name(DEFAULT_SESSION_NAME).unwrap(), None);
        assert_eq!(parse_target_name("work").unwrap(), Some("work".to_string()));
    }

    #[test]
    fn delete_default_session_is_rejected() {
        assert!(delete_session(DEFAULT_SESSION_NAME).is_err());
    }

    #[test]
    fn list_sessions_skips_reserved_default_directory() {
        let env = crate::environment::test_env();
        let config_home =
            std::env::temp_dir().join(format!("herdr-session-list-{}", std::process::id()));
        let sessions_dir = config_home
            .join(crate::config::app_dir_name())
            .join("sessions");
        std::fs::create_dir_all(sessions_dir.join(DEFAULT_SESSION_NAME)).unwrap();
        std::fs::create_dir_all(sessions_dir.join("work")).unwrap();
        env.set("XDG_CONFIG_HOME", &config_home);
        env.remove(SESSION_ENV_VAR);
        clear_explicit_session_for_test();

        let sessions = list_sessions().unwrap();
        let names: Vec<_> = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect();

        assert_eq!(names, vec![DEFAULT_SESSION_NAME, "work"]);
        std::fs::remove_dir_all(&config_home).unwrap();
        env.remove("XDG_CONFIG_HOME");
    }
}
