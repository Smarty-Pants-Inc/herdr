use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;

#[cfg(unix)]
use std::{
    io::{IsTerminal as _, Read as _, Write as _},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};

pub(crate) const REMOTE_EXEC_PROTOCOL: u32 = 3;
pub(crate) const REMOTE_EXEC_READY_OSC_PREFIX: &[u8] = b"\x1b]6973;herdr-remote-exec-ready=";

#[cfg(unix)]
const REMOTE_API_SOCKET_PREFIX: &str = "herdr-remote-api-";
#[cfg(unix)]
const REMOTE_REQUEST_SOCKET_PREFIX: &str = "herdr-remote-request-";
#[cfg(unix)]
const LOCAL_REQUEST_SOCKET_PREFIX: &str = "herdr-local-request-";
#[cfg(unix)]
const REMOTE_SOCKET_NONCE_BYTES: usize = 16;
#[cfg(unix)]
const REMOTE_SOCKET_BIND_ATTEMPTS: usize = 4;
#[cfg(unix)]
const REMOTE_EXEC_REQUEST_MAX_BYTES: usize = 1024 * 1024;
#[cfg(unix)]
const REMOTE_REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(unix)]
const REMOTE_REQUEST_IO_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(unix)]
const REMOTE_REQUEST_ACCEPT_POLL: Duration = Duration::from_millis(25);
#[cfg(unix)]
const REMOTE_SOCKET_PERMISSION_MODE: u32 = 0o600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionTarget {
    #[default]
    Local,
    Ssh {
        host: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Osc7Authority {
    accepted: Vec<String>,
}

impl Osc7Authority {
    pub(crate) fn accepts(&self, authority: Option<&str>, remote_hostname: Option<&str>) -> bool {
        let Some(authority) = authority else {
            return true;
        };
        authority.eq_ignore_ascii_case("localhost")
            || remote_hostname.is_some_and(|host| authority.eq_ignore_ascii_case(host))
            || self
                .accepted
                .iter()
                .any(|host| authority.eq_ignore_ascii_case(host))
    }
}

impl ExecutionTarget {
    pub fn ssh(host: impl Into<String>) -> std::io::Result<Self> {
        let host = host.into();
        validate_ssh_host(&host)?;
        Ok(Self::Ssh { host })
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn osc7_authority(&self) -> Osc7Authority {
        self.osc7_authority_with(
            crate::platform::hostname(),
            match self {
                Self::Local => None,
                Self::Ssh { host } => query_ssh_hostname(host),
            },
        )
    }

    pub(crate) fn osc7_authority_with(
        &self,
        local_hostname: Option<String>,
        effective_ssh_hostname: Option<String>,
    ) -> Osc7Authority {
        let mut accepted = Vec::with_capacity(2);
        match self {
            Self::Local => accepted.extend(local_hostname),
            Self::Ssh { host } => {
                accepted.push(ssh_destination_host(host).to_string());
                if let Some(hostname) = effective_ssh_hostname {
                    accepted.push(hostname);
                }
            }
        }
        Osc7Authority { accepted }
    }
}

impl std::fmt::Display for ExecutionTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => formatter.write_str("local"),
            Self::Ssh { host } => write!(formatter, "ssh:{host}"),
        }
    }
}

impl FromStr for ExecutionTarget {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "local" {
            return Ok(Self::Local);
        }
        let Some(host) = value.strip_prefix("ssh:") else {
            return Err("execution target must be local or ssh:<host>".into());
        };
        Self::ssh(host).map_err(|err| err.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RemoteCommand {
    Shell,
    ShellCommand {
        command: String,
    },
    Argv {
        argv: Vec<String>,
    },
    Plugin {
        plugin_id: String,
        target: crate::plugin_command::PluginCommandTarget,
    },
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteExecRequest {
    cwd: PathBuf,
    command: RemoteCommand,
    env: Vec<(String, String)>,
    api_socket: PathBuf,
}

#[cfg(unix)]
struct RemoteExecSsh {
    api_forward: String,
    request_forward: String,
    remote_command: String,
}

#[cfg(unix)]
impl RemoteExecSsh {
    fn ssh_args(&self, host: &str, pty: bool) -> Vec<String> {
        vec![
            if pty { "-tt" } else { "-T" }.to_string(),
            "-o".to_string(),
            "ExitOnForwardFailure=yes".to_string(),
            "-o".to_string(),
            "ControlPath=none".to_string(),
            "-o".to_string(),
            "StreamLocalBindMask=0177".to_string(),
            "-R".to_string(),
            self.api_forward.clone(),
            "-R".to_string(),
            self.request_forward.clone(),
            host.to_string(),
            self.remote_command.clone(),
        ]
    }
}
#[cfg(unix)]
pub(crate) struct RemoteRequestChannel {
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
}

#[cfg(not(unix))]
pub(crate) struct RemoteRequestChannel;
#[cfg(not(unix))]
impl RemoteRequestChannel {
    pub(crate) fn cancel(self) {}
}

#[cfg(unix)]
impl RemoteRequestChannel {
    fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn cancel(self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
impl Drop for RemoteRequestChannel {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) struct PreparedSshPtyCommand {
    pub(crate) command: CommandBuilder,
    pub(crate) request_channel: RemoteRequestChannel,
}

pub(crate) struct PreparedSshProcessCommand {
    pub(crate) command: std::process::Command,
    pub(crate) request_channel: RemoteRequestChannel,
}

#[cfg(unix)]
fn prepare_remote_exec_ssh(
    host: &str,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
) -> std::io::Result<(RemoteExecSsh, RemoteRequestChannel)> {
    let api_socket = random_socket_path(REMOTE_API_SOCKET_PREFIX)?;
    let request_socket = random_socket_path(REMOTE_REQUEST_SOCKET_PREFIX)?;
    let api_forward = socket_forward(&api_socket, &crate::api::socket_path())?;
    let shell_path = crate::remote::resolve_prepared_remote_shell_path(host)?;
    let request = RemoteExecRequest {
        cwd: cwd.to_path_buf(),
        command,
        env,
        api_socket: api_socket.clone(),
    };
    let request_channel = start_remote_request_channel(request)?;
    let request_forward = socket_forward(&request_socket, request_channel.path())?;

    Ok((
        RemoteExecSsh {
            api_forward,
            request_forward,
            remote_command: remote_exec_command(&shell_path, &request_socket),
        },
        request_channel,
    ))
}

#[cfg(unix)]
fn socket_forward(remote_socket: &Path, local_socket: &Path) -> std::io::Result<String> {
    Ok(format!(
        "{}:{}",
        ssh_streamlocal_socket_path(remote_socket)?,
        ssh_streamlocal_socket_path(local_socket)?
    ))
}

#[cfg(unix)]
fn ssh_streamlocal_socket_path(path: &Path) -> std::io::Result<&str> {
    let path = path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH streamlocal socket path must be valid UTF-8",
        )
    })?;
    if path.starts_with('/') && !path.contains(':') && !path.contains('\\') && !path.contains('\0')
    {
        Ok(path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH streamlocal socket path must be absolute and contain no ':', '\\', or NUL",
        ))
    }
}

#[cfg(unix)]
fn remote_exec_command(shell_path: &str, request_socket: &Path) -> String {
    format!(
        "exec {shell_path} remote-exec {}",
        crate::remote::shell_quote(&request_socket.display().to_string())
    )
}

#[cfg(unix)]
fn start_remote_request_channel(
    request: RemoteExecRequest,
) -> std::io::Result<RemoteRequestChannel> {
    let payload = encode_remote_exec_request(&request)?;

    for _ in 0..REMOTE_SOCKET_BIND_ATTEMPTS {
        let local_socket = random_socket_path(LOCAL_REQUEST_SOCKET_PREFIX)?;
        let listener = match bind_private_request_listener(&local_socket) {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err),
        };
        let socket_identity = match crate::ipc::socket_file_identity(&local_socket) {
            Ok(identity) => identity,
            Err(err) => {
                drop(listener);
                return Err(err);
            }
        };

        let cleanup_identity = socket_identity.clone();
        let thread_socket = local_socket.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread = thread::Builder::new()
            .name("herdr-remote-exec-request".to_string())
            .spawn(move || {
                serve_remote_exec_request(
                    listener,
                    payload,
                    thread_socket,
                    socket_identity,
                    thread_cancelled,
                );
            });
        if let Err(err) = thread {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &cleanup_identity);
            return Err(err);
        }

        return Ok(RemoteRequestChannel {
            path: local_socket,
            cancelled,
        });
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "could not create a unique remote execution request socket",
    ))
}

#[cfg(unix)]
fn bind_private_request_listener(path: &Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    let socket_identity = match crate::ipc::socket_file_identity(path) {
        Ok(identity) => identity,
        Err(err) => {
            drop(listener);
            return Err(err);
        }
    };
    if let Err(err) = crate::ipc::restrict_socket_permissions(path, REMOTE_SOCKET_PERMISSION_MODE) {
        let _ = crate::ipc::remove_socket_file_if_owned(path, &socket_identity);
        return Err(err);
    }
    if let Err(err) = listener.set_nonblocking(true) {
        let _ = crate::ipc::remove_socket_file_if_owned(path, &socket_identity);
        return Err(err);
    }
    Ok(listener)
}

#[cfg(unix)]
fn serve_remote_exec_request(
    listener: UnixListener,
    payload: Vec<u8>,
    socket: PathBuf,
    socket_identity: crate::ipc::SocketFileIdentity,
    cancelled: Arc<AtomicBool>,
) {
    let deadline = Instant::now() + REMOTE_REQUEST_CONNECT_TIMEOUT;
    let result = loop {
        if cancelled.load(Ordering::Acquire) {
            break Ok(());
        }
        match listener.accept() {
            Ok((mut stream, _)) => break write_remote_exec_request(&mut stream, &payload),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "remote execution did not connect to its request socket",
                    ));
                }
                thread::sleep(REMOTE_REQUEST_ACCEPT_POLL);
            }
            Err(err) => break Err(err),
        }
    };
    if let Err(err) = result {
        tracing::debug!(%err, "remote execution request channel closed without a request");
    }
    drop(listener);
    if let Err(err) = crate::ipc::remove_socket_file_if_owned(&socket, &socket_identity) {
        tracing::debug!(socket = %socket.display(), %err, "failed to remove remote execution request socket");
    }
}

#[cfg(unix)]
fn encode_remote_exec_request(request: &RemoteExecRequest) -> std::io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    if payload.len() > REMOTE_EXEC_REQUEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("remote execution request exceeds {REMOTE_EXEC_REQUEST_MAX_BYTES} bytes"),
        ));
    }
    Ok(payload)
}

#[cfg(unix)]
fn write_remote_exec_request(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > REMOTE_EXEC_REQUEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "remote execution request is too large",
        ));
    }
    let length = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "remote execution request length does not fit its frame",
        )
    })?;
    stream.set_write_timeout(Some(REMOTE_REQUEST_IO_TIMEOUT))?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.shutdown(Shutdown::Write)
}

#[cfg(unix)]
fn read_remote_exec_request(stream: &mut UnixStream) -> std::io::Result<RemoteExecRequest> {
    stream.set_read_timeout(Some(REMOTE_REQUEST_IO_TIMEOUT))?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > REMOTE_EXEC_REQUEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("remote execution request exceeds {REMOTE_EXEC_REQUEST_MAX_BYTES} bytes"),
        ));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
}

#[cfg(unix)]
fn random_socket_path(prefix: &str) -> std::io::Result<PathBuf> {
    let mut bytes = [0_u8; REMOTE_SOCKET_NONCE_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| std::io::Error::other(err.to_string()))?;

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut nonce = String::with_capacity(REMOTE_SOCKET_NONCE_BYTES * 2);
    for byte in bytes {
        nonce.push(HEX[(byte >> 4) as usize] as char);
        nonce.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(Path::new("/tmp").join(format!("{prefix}{nonce}.sock")))
}

pub(crate) fn ssh_pty_command(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
) -> std::io::Result<PreparedSshPtyCommand> {
    let ExecutionTarget::Ssh { host } = target else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH command requires an SSH execution target",
        ));
    };
    validate_ssh_host(host)?;

    #[cfg(not(unix))]
    {
        let _ = (cwd, command, env);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "per-terminal SSH execution is only supported on Unix",
        ))
    }

    #[cfg(unix)]
    {
        let (remote_exec, request_channel) = prepare_remote_exec_ssh(host, cwd, command, env)?;
        let mut ssh = CommandBuilder::new("ssh");
        ssh.args(remote_exec.ssh_args(host, true));
        ssh.env("TERM", crate::pane::PANE_TERM);
        ssh.env("COLORTERM", crate::pane::PANE_COLORTERM);
        Ok(PreparedSshPtyCommand {
            command: ssh,
            request_channel,
        })
    }
}

pub(crate) fn ssh_process_command(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
) -> std::io::Result<PreparedSshProcessCommand> {
    let ExecutionTarget::Ssh { host } = target else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH command requires an SSH execution target",
        ));
    };
    validate_ssh_host(host)?;

    #[cfg(not(unix))]
    {
        let _ = (cwd, command, env);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "per-terminal SSH execution is only supported on Unix",
        ))
    }

    #[cfg(unix)]
    {
        let (remote_exec, request_channel) = prepare_remote_exec_ssh(host, cwd, command, env)?;
        let mut ssh = crate::noninteractive_process::command("ssh");
        ssh.args(remote_exec.ssh_args(host, false));
        Ok(PreparedSshProcessCommand {
            command: ssh,
            request_channel,
        })
    }
}

pub(crate) fn run_remote_exec(request_socket: &str) -> std::io::Result<i32> {
    #[cfg(not(unix))]
    {
        let _ = request_socket;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "remote-exec is only supported on Unix",
        ))
    }

    #[cfg(unix)]
    {
        let request_socket = Path::new(request_socket);
        validate_remote_request_socket_path(request_socket)?;
        let mut stream = UnixStream::connect(request_socket).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!(
                    "failed to connect to remote execution request socket {}: {err}",
                    request_socket.display()
                ),
            )
        })?;
        let request = read_remote_exec_request(&mut stream)?;
        validate_remote_api_socket_path(&request.api_socket)?;

        let resolved = remote_process_command(&request.command)?;
        let mut command = resolved.command;
        let cwd = if request.cwd.as_os_str().is_empty() {
            resolved.cwd
        } else {
            Some(request.cwd)
        };
        let mut env = request.env;
        env.extend(resolved.env);
        let cwd = apply_remote_cwd(&mut command, cwd.as_deref(), &mut env)?;
        apply_remote_exec_env(&mut command, env);
        command.env("TERM", crate::pane::PANE_TERM);
        command.env("COLORTERM", crate::pane::PANE_COLORTERM);
        command.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        command.env(crate::api::SOCKET_PATH_ENV_VAR, &request.api_socket);
        if let Ok(executable) = std::env::current_exe() {
            command.env("HERDR_BIN_PATH", executable);
        }

        let ready_marker = remote_exec_ready_marker_for_terminal(
            std::io::stdout().is_terminal(),
            crate::platform::hostname().as_deref(),
            &cwd,
        )?;
        let mut child = command.spawn()?;
        if let Some(ready_marker) = ready_marker {
            if let Err(err) = write_remote_exec_ready_marker(&ready_marker) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        }
        let status = child.wait()?;
        Ok(status.code().unwrap_or(1))
    }
}
#[cfg(unix)]
fn apply_remote_exec_env(command: &mut std::process::Command, env: Vec<(String, String)>) {
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove(crate::integration::HERDR_WORKSPACE_ID_ENV_VAR);
    command.env_remove(crate::integration::HERDR_TAB_ID_ENV_VAR);
    command.env_remove(crate::integration::HERDR_PANE_ID_ENV_VAR);
    command.env_remove("HERDR_VIEW_ID");
    command.envs(env);
}
#[cfg(unix)]
#[derive(Serialize)]
struct RemoteExecReadyPayload<'a> {
    hostname: Option<&'a str>,
    cwd: &'a Path,
}

#[cfg(unix)]
fn remote_exec_ready_marker(hostname: Option<&str>, cwd: &Path) -> std::io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(&RemoteExecReadyPayload { hostname, cwd })
        .map_err(std::io::Error::other)?;
    let mut marker =
        Vec::with_capacity(REMOTE_EXEC_READY_OSC_PREFIX.len() + payload.len() + b"\x1b\\".len());
    marker.extend_from_slice(REMOTE_EXEC_READY_OSC_PREFIX);
    marker.extend_from_slice(&payload);
    marker.extend_from_slice(b"\x1b\\");
    Ok(marker)
}
#[cfg(unix)]
fn remote_exec_ready_marker_for_terminal(
    stdout_is_terminal: bool,
    hostname: Option<&str>,
    cwd: &Path,
) -> std::io::Result<Option<Vec<u8>>> {
    stdout_is_terminal
        .then(|| remote_exec_ready_marker(hostname, cwd))
        .transpose()
}

#[cfg(unix)]
fn write_remote_exec_ready_marker(marker: &[u8]) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(marker)?;
    stdout.flush()
}
#[cfg(unix)]
fn apply_remote_cwd(
    command: &mut std::process::Command,
    cwd: Option<&Path>,
    env: &mut Vec<(String, String)>,
) -> std::io::Result<PathBuf> {
    let process_cwd = std::env::current_dir()?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let cwd = resolve_remote_cwd(cwd, &process_cwd, home.as_deref());
    command.current_dir(&cwd);
    crate::platform::set_default_plugin_pane_pwd(env, &cwd);
    Ok(cwd)
}

#[cfg(unix)]
fn resolve_remote_cwd(cwd: Option<&Path>, process_cwd: &Path, home: Option<&Path>) -> PathBuf {
    let requested = cwd.or(home).unwrap_or(process_cwd);
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        process_cwd.join(requested)
    }
}

#[cfg(unix)]
fn remote_process_command(command: &RemoteCommand) -> std::io::Result<ResolvedRemoteProcess> {
    let resolved = match command {
        RemoteCommand::Shell => {
            let loaded = crate::config::Config::load();
            let shell = remote_shell(&loaded.config.terminal.default_shell);
            let mut command = std::process::Command::new(shell);
            if remote_shell_is_login(loaded.config.terminal.shell_mode) {
                command.arg("-l");
            }
            ResolvedRemoteProcess {
                command,
                cwd: None,
                env: Vec::new(),
            }
        }
        RemoteCommand::ShellCommand { command } => {
            let loaded = crate::config::Config::load();
            let shell = remote_shell(&loaded.config.terminal.default_shell);
            let mut process = std::process::Command::new(shell);
            process.arg("-lc").arg(command);
            ResolvedRemoteProcess {
                command: process,
                cwd: None,
                env: Vec::new(),
            }
        }
        RemoteCommand::Argv { argv } => {
            let Some((program, args)) = argv.split_first() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "argv must not be empty",
                ));
            };
            let mut process = std::process::Command::new(program);
            process.args(args);
            ResolvedRemoteProcess {
                command: process,
                cwd: None,
                env: Vec::new(),
            }
        }
        RemoteCommand::Plugin { plugin_id, target } => {
            let resolved =
                crate::plugin_command::resolve_installed_plugin_command(plugin_id, target)?;
            let Some((program, args)) = resolved.command.split_first() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "resolved plugin command is empty",
                ));
            };
            let mut process = std::process::Command::new(program);
            process.args(args);
            ResolvedRemoteProcess {
                command: process,
                cwd: Some(resolved.cwd),
                env: resolved.env,
            }
        }
    };
    Ok(resolved)
}
#[cfg(unix)]
struct ResolvedRemoteProcess {
    command: std::process::Command,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
}

#[cfg(unix)]
fn remote_shell(configured: &str) -> String {
    if configured.trim().is_empty() {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    } else {
        configured.to_string()
    }
}

#[cfg(unix)]
fn remote_shell_is_login(mode: crate::config::ShellModeConfig) -> bool {
    match mode {
        crate::config::ShellModeConfig::Auto => cfg!(target_os = "macos"),
        crate::config::ShellModeConfig::Login => true,
        crate::config::ShellModeConfig::NonLogin => false,
    }
}

fn ssh_destination_host(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

fn query_ssh_hostname(destination: &str) -> Option<String> {
    validate_ssh_host(destination).ok()?;
    let output = crate::noninteractive_process::command("ssh")
        .args(["-G", destination])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = std::str::from_utf8(&output.stdout).ok()?;
    parse_ssh_config_hostname(output).map(str::to_owned)
}

fn parse_ssh_config_hostname(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        let key = fields.next()?;
        let hostname = fields.next()?;
        (key.eq_ignore_ascii_case("hostname") && fields.next().is_none()).then_some(hostname)
    })
}

fn validate_ssh_host(host: &str) -> std::io::Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH host must be a non-empty target without whitespace or a leading dash",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_remote_api_socket_path(path: &Path) -> std::io::Result<()> {
    validate_random_remote_socket_path(path, REMOTE_API_SOCKET_PREFIX, "remote API socket")
}

#[cfg(unix)]
fn validate_remote_request_socket_path(path: &Path) -> std::io::Result<()> {
    validate_random_remote_socket_path(
        path,
        REMOTE_REQUEST_SOCKET_PREFIX,
        "remote execution request socket",
    )
}

#[cfg(unix)]
fn validate_random_remote_socket_path(
    path: &Path,
    prefix: &str,
    description: &str,
) -> std::io::Result<()> {
    let nonce = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|name| name.strip_suffix(".sock"));
    let valid = path.parent() == Some(Path::new("/tmp"))
        && nonce.is_some_and(|nonce| {
            nonce.len() == REMOTE_SOCKET_NONCE_BYTES * 2
                && nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        });
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{description} must be a random Herdr socket under /tmp"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn remote_cwd_sets_default_pwd_without_overriding_explicit_value() {
        let cwd = Path::new("/remote/plugin-root");
        let mut command = std::process::Command::new("true");
        let mut env = Vec::new();
        let resolved = apply_remote_cwd(&mut command, Some(cwd), &mut env).unwrap();
        assert_eq!(command.get_current_dir(), Some(cwd));
        assert_eq!(
            env,
            [("PWD".to_string(), "/remote/plugin-root".to_string())]
        );
        assert_eq!(resolved, cwd);

        let mut command = std::process::Command::new("true");
        let mut env = vec![("PWD".to_string(), "/caller/pwd".to_string())];
        apply_remote_cwd(&mut command, Some(cwd), &mut env).unwrap();
        assert_eq!(env, [("PWD".to_string(), "/caller/pwd".to_string())]);

        let relative = Path::new("nested/plugin-root");
        let mut command = std::process::Command::new("true");
        let mut env = Vec::new();
        let resolved = apply_remote_cwd(&mut command, Some(relative), &mut env).unwrap();
        assert_eq!(resolved, std::env::current_dir().unwrap().join(relative));
        assert!(resolved.is_absolute());

        assert_eq!(
            resolve_remote_cwd(
                None,
                Path::new("/remote/process-cwd"),
                Some(Path::new("/home/remote"))
            ),
            Path::new("/home/remote")
        );
    }
    #[cfg(unix)]
    #[test]
    fn remote_exec_scrubs_inherited_identity_before_request_env() {
        let mut command = std::process::Command::new("true");
        for key in [
            "CODEX_THREAD_ID",
            crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
            crate::integration::HERDR_TAB_ID_ENV_VAR,
            crate::integration::HERDR_PANE_ID_ENV_VAR,
            "HERDR_VIEW_ID",
        ] {
            command.env(key, "stale");
        }

        apply_remote_exec_env(
            &mut command,
            vec![
                ("HERDR_VIEW_ID".to_string(), "request-view".to_string()),
                (
                    crate::integration::HERDR_WORKSPACE_ID_ENV_VAR.to_string(),
                    "request-workspace".to_string(),
                ),
            ],
        );

        let env_value = |key: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(key))
                .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        };
        assert_eq!(
            env_value("HERDR_VIEW_ID"),
            Some(Some(std::ffi::OsString::from("request-view")))
        );
        assert_eq!(
            env_value(crate::integration::HERDR_WORKSPACE_ID_ENV_VAR),
            Some(Some(std::ffi::OsString::from("request-workspace")))
        );
        for key in [
            "CODEX_THREAD_ID",
            crate::integration::HERDR_TAB_ID_ENV_VAR,
            crate::integration::HERDR_PANE_ID_ENV_VAR,
        ] {
            assert_eq!(env_value(key), Some(None), "{key} should remain scrubbed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_ready_marker_json_encodes_hostname_and_resolved_cwd() {
        let cwd = Path::new("/remote/plugin-root");
        assert_eq!(
            remote_exec_ready_marker(Some("build-\"node"), cwd).unwrap(),
            b"\x1b]6973;herdr-remote-exec-ready={\"hostname\":\"build-\\\"node\",\"cwd\":\"/remote/plugin-root\"}\x1b\\"
        );
        assert_eq!(
            remote_exec_ready_marker_for_terminal(false, Some("build-node"), cwd).unwrap(),
            None
        );
    }

    #[test]
    fn osc7_authority_uses_only_precomputed_and_runtime_hostnames() {
        let local =
            ExecutionTarget::Local.osc7_authority_with(Some("local-node".to_string()), None);
        assert!(local.accepts(Some("LOCAL-NODE"), None));
        assert!(!local.accepts(Some("other-node"), None));

        let remote = ExecutionTarget::ssh("deploy@build-alias")
            .unwrap()
            .osc7_authority_with(None, Some("real.example.com".to_string()));
        assert!(remote.accepts(Some("BUILD-ALIAS"), None));
        assert!(remote.accepts(Some("REAL.EXAMPLE.COM"), None));
        assert!(remote.accepts(Some("ACTUAL-NODE"), Some("actual-node")));
        assert!(remote.accepts(None, Some("actual-node")));
        assert!(remote.accepts(Some("localhost"), Some("actual-node")));
        assert!(!remote.accepts(Some("deploy@build-alias"), None));
        assert!(!remote.accepts(Some("other-node"), None));
    }

    #[test]
    fn execution_target_round_trips_and_defaults_local() {
        let local: ExecutionTarget = serde_json::from_str("{\"kind\":\"local\"}").unwrap();
        let ssh: ExecutionTarget = "ssh:primary".parse().unwrap();
        assert_eq!(local, ExecutionTarget::Local);
        assert_eq!(ssh.to_string(), "ssh:primary");
        assert!("ssh:-bad".parse::<ExecutionTarget>().is_err());
    }

    #[test]
    fn ssh_config_hostname_parser_requires_one_hostname_value() {
        let output = "host build-alias\nuser deploy\nhostname Real.Example.COM\nport 22\n";
        assert_eq!(parse_ssh_config_hostname(output), Some("Real.Example.COM"));
        assert_eq!(parse_ssh_config_hostname("user deploy\nport 22\n"), None);
        assert_eq!(parse_ssh_config_hostname("hostname\n"), None);
        assert_eq!(
            parse_ssh_config_hostname("hostname real.example.com extra\n"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn random_remote_socket_paths_require_full_nonces() {
        let api_socket = random_socket_path(REMOTE_API_SOCKET_PREFIX).unwrap();
        let request_socket = random_socket_path(REMOTE_REQUEST_SOCKET_PREFIX).unwrap();

        assert_ne!(api_socket, request_socket);
        assert!(validate_remote_api_socket_path(&api_socket).is_ok());
        assert!(validate_remote_request_socket_path(&request_socket).is_ok());
        assert!(
            validate_remote_api_socket_path(Path::new("/tmp/herdr-remote-api-short.sock")).is_err()
        );
        assert!(validate_remote_request_socket_path(Path::new(
            "/var/tmp/herdr-remote-request-0123456789abcdef0123456789abcdef.sock"
        ))
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn request_listener_is_user_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let socket = random_socket_path(LOCAL_REQUEST_SOCKET_PREFIX).unwrap();
        let listener = bind_private_request_listener(&socket).unwrap();
        let identity = crate::ipc::socket_file_identity(&socket).unwrap();

        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, REMOTE_SOCKET_PERMISSION_MODE);

        drop(listener);
        crate::ipc::remove_socket_file_if_owned(&socket, &identity).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_request_channel_cancels_listener_and_removes_socket() {
        let request = RemoteExecRequest {
            cwd: PathBuf::new(),
            command: RemoteCommand::Shell,
            env: Vec::new(),
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
        };
        let channel = start_remote_request_channel(request).unwrap();
        let socket = channel.path().to_path_buf();
        assert!(socket.exists());

        drop(channel);
        let deadline = Instant::now() + Duration::from_secs(1);
        while socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            !socket.exists(),
            "cancelled listener socket was not reclaimed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_request_round_trips_over_a_framed_socket() {
        let request = RemoteExecRequest {
            cwd: PathBuf::from("/tmp/work"),
            command: RemoteCommand::ShellCommand {
                command: "printf '%s' structured".to_string(),
            },
            env: vec![("EXAMPLE".to_string(), "value".to_string())],
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
        };
        let payload = encode_remote_exec_request(&request).unwrap();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();

        write_remote_exec_request(&mut sender, &payload).unwrap();
        let received = read_remote_exec_request(&mut receiver).unwrap();

        assert_eq!(received.cwd, request.cwd);
        assert_eq!(received.command, request.command);
        assert_eq!(received.env, request.env);
        assert_eq!(received.api_socket, request.api_socket);
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_request_size_is_bounded() {
        let request = RemoteExecRequest {
            cwd: PathBuf::new(),
            command: RemoteCommand::ShellCommand {
                command: "x".repeat(REMOTE_EXEC_REQUEST_MAX_BYTES),
            },
            env: Vec::new(),
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
        };
        assert_eq!(
            encode_remote_exec_request(&request).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );

        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender
            .write_all(&((REMOTE_EXEC_REQUEST_MAX_BYTES as u32) + 1).to_be_bytes())
            .unwrap();
        assert_eq!(
            read_remote_exec_request(&mut receiver).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_streamlocal_forward_rejects_unrepresentable_socket_paths() {
        let remote_socket = Path::new("/tmp/herdr-remote.sock");
        assert_eq!(
            socket_forward(remote_socket, Path::new("/tmp/herdr-local.sock")).unwrap(),
            "/tmp/herdr-remote.sock:/tmp/herdr-local.sock"
        );

        for local_socket in [
            Path::new("relative.sock"),
            Path::new("/tmp/herdr:local.sock"),
            Path::new(r"/tmp/herdr\local.sock"),
        ] {
            assert_eq!(
                socket_forward(remote_socket, local_socket)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }

        use std::os::unix::ffi::OsStringExt as _;
        for bytes in [b"/tmp/herdr-\xff.sock".as_slice(), b"/tmp/herdr-\0.sock"] {
            let local_socket = PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()));
            assert_eq!(
                socket_forward(remote_socket, &local_socket)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn ssh_args_keep_request_data_off_the_command_line() {
        let request_socket =
            Path::new("/tmp/herdr-remote-request-0123456789abcdef0123456789abcdef.sock");
        let remote_command = remote_exec_command("/opt/herdr/bin/herdr", request_socket);
        let remote_exec = RemoteExecSsh {
            api_forward:
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock:/tmp/api.sock"
                    .to_string(),
            request_forward: format!("{}:/tmp/herdr-local-request.sock", request_socket.display()),
            remote_command: remote_command.clone(),
        };

        let pty_args = remote_exec.ssh_args("user@example", true);
        let process_args = remote_exec.ssh_args("user@example", false);

        assert_eq!(pty_args.first().map(String::as_str), Some("-tt"));
        assert_eq!(process_args.first().map(String::as_str), Some("-T"));
        assert!(pty_args
            .windows(2)
            .any(|args| args == ["-o", "StreamLocalBindMask=0177"]));
        for args in [&pty_args, &process_args] {
            assert!(args
                .windows(2)
                .any(|args| args == ["-o", "ControlPath=none"]));
        }
        assert!(!pty_args
            .iter()
            .any(|arg| arg.contains("StreamLocalBindUnlink")));
        assert_eq!(pty_args.last(), Some(&remote_command));
        assert!(!pty_args.iter().any(|arg| arg.contains("structured")));
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_command_keeps_a_resolved_shell_path_quoted() {
        let request_socket =
            Path::new("/tmp/herdr-remote-request-0123456789abcdef0123456789abcdef.sock");
        assert_eq!(
            remote_exec_command("'/opt/herdr bin/herdr'", request_socket),
            format!(
                "exec '/opt/herdr bin/herdr' remote-exec {}",
                request_socket.display()
            )
        );
    }
}
