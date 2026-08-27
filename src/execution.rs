use std::path::{Path, PathBuf};
use std::str::FromStr;

#[cfg(unix)]
use std::{
    io::{IsTerminal as _, Read as _, Write as _},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};

pub(crate) const REMOTE_EXEC_PROTOCOL: u32 = 3;
pub(crate) const EXECUTION_PROVIDER_PROTOCOL: u32 = 1;
pub(crate) const EXECUTION_PROVIDER_REQUEST_ENV: &str = "HERDR_EXECUTION_PROVIDER_REQUEST";
const EXECUTION_PROVIDER_REQUEST_MAX_BYTES: usize = 16 * 1024;
pub(crate) const REMOTE_EXEC_READY_OSC_PREFIX: &[u8] = b"\x1b]6973;herdr-remote-exec-ready=";
pub(crate) const REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES: usize = 32 * 1024;
const REMOTE_EXEC_READY_NONCE_BYTES: usize = 16;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteExecReadyNonce(String);

impl std::fmt::Debug for RemoteExecReadyNonce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RemoteExecReadyNonce([redacted])")
    }
}

fn random_hex_string(byte_len: usize) -> std::io::Result<String> {
    let mut bytes = vec![0_u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(byte_len * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(value)
}

impl RemoteExecReadyNonce {
    pub(crate) fn generate() -> std::io::Result<Self> {
        Ok(Self(random_hex_string(REMOTE_EXEC_READY_NONCE_BYTES)?))
    }

    #[cfg(any(unix, test))]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        self.0 == candidate
    }
}

impl Serialize for RemoteExecReadyNonce {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RemoteExecReadyNonce {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let valid = value.len() == REMOTE_EXEC_READY_NONCE_BYTES * 2
            && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        valid
            .then_some(Self(value))
            .ok_or_else(|| serde::de::Error::custom("invalid remote execution readiness nonce"))
    }
}

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
const REMOTE_REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
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
    Extension {
        scheme: String,
        target: String,
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

    pub fn extension(
        scheme: impl Into<String>,
        target: impl Into<String>,
    ) -> std::io::Result<Self> {
        let scheme = scheme.into();
        let target = target.into();
        validate_execution_provider_scheme(&scheme)?;
        validate_execution_provider_target(&target)?;
        Ok(Self::Extension { scheme, target })
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) fn osc7_authority(&self) -> Osc7Authority {
        self.osc7_authority_with(crate::platform::hostname(), None)
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
            Self::Extension { .. } => {}
        }
        Osc7Authority { accepted }
    }
}

impl std::fmt::Display for ExecutionTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => formatter.write_str("local"),
            Self::Ssh { host } => write!(formatter, "ssh:{host}"),
            Self::Extension { scheme, target } => write!(formatter, "{scheme}:{target}"),
        }
    }
}

impl FromStr for ExecutionTarget {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "local" {
            return Ok(Self::Local);
        }
        if let Some(host) = value.strip_prefix("ssh:") {
            return Self::ssh(host).map_err(|err| err.to_string());
        }
        if let Some((scheme, target)) = value.split_once(':') {
            return Self::extension(scheme, target).map_err(|err| err.to_string());
        }
        Err("execution target must be local, ssh:<host>, or <provider-scheme>:<target>".into())
    }
}

fn validate_execution_provider_scheme(scheme: &str) -> std::io::Result<()> {
    let valid = crate::app::normalize_execution_provider_scheme(scheme)
        .is_some_and(|normalized| normalized == scheme);
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "execution provider scheme must match [a-z][a-z0-9+.-]* and must not be local or ssh",
        ))
    }
}

fn validate_execution_provider_target(target: &str) -> std::io::Result<()> {
    if target.is_empty()
        || target
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "execution provider target must be non-empty and contain no whitespace or control characters",
        ))
    } else {
        Ok(())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExecutionProviderCommand {
    Shell,
    ShellCommand { command: String },
    Argv { argv: Vec<String> },
}

impl TryFrom<RemoteCommand> for ExecutionProviderCommand {
    type Error = std::io::Error;

    fn try_from(command: RemoteCommand) -> Result<Self, Self::Error> {
        match command {
            RemoteCommand::Shell => Ok(Self::Shell),
            RemoteCommand::ShellCommand { command } => Ok(Self::ShellCommand { command }),
            RemoteCommand::Argv { argv } => Ok(Self::Argv { argv }),
            RemoteCommand::Plugin { .. } => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "execution providers require Herdr to resolve plugin commands to argv",
            )),
        }
    }
}

#[derive(Debug, Serialize)]
struct ExecutionProviderRequest {
    version: u32,
    target: String,
    cwd: PathBuf,
    command: ExecutionProviderCommand,
    env: Vec<(String, String)>,
    remove_env: Vec<String>,
    ready_nonce: RemoteExecReadyNonce,
}

pub(crate) struct PreparedExecutionProviderPtyCommand {
    pub(crate) command: CommandBuilder,
    pub(crate) ready_nonce: RemoteExecReadyNonce,
}

pub(crate) struct PreparedExecutionProviderProcessCommand {
    pub(crate) command: std::process::Command,
}

fn execution_provider_request(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
    remove_env: Vec<String>,
) -> std::io::Result<(String, String, RemoteExecReadyNonce)> {
    let ExecutionTarget::Extension { scheme, target } = target else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "execution provider command requires an extension execution target",
        ));
    };
    validate_execution_provider_scheme(scheme)?;
    validate_execution_provider_target(target)?;
    let ready_nonce = RemoteExecReadyNonce::generate()?;
    let request = ExecutionProviderRequest {
        version: EXECUTION_PROVIDER_PROTOCOL,
        target: target.clone(),
        cwd: cwd.to_path_buf(),
        command: command.try_into()?,
        env,
        remove_env,
        ready_nonce: ready_nonce.clone(),
    };
    let payload = serde_json::to_string(&request).map_err(std::io::Error::other)?;
    if payload.len() > EXECUTION_PROVIDER_REQUEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "execution provider request exceeds {EXECUTION_PROVIDER_REQUEST_MAX_BYTES} bytes"
            ),
        ));
    }
    Ok((scheme.clone(), payload, ready_nonce))
}

pub(crate) fn execution_provider_pty_command_with_removals(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
    remove_env: Vec<String>,
) -> std::io::Result<PreparedExecutionProviderPtyCommand> {
    let (scheme, payload, ready_nonce) =
        execution_provider_request(target, cwd, command, env, remove_env)?;
    let provider = crate::plugin_command::resolve_installed_execution_provider(
        &scheme,
        crate::plugin_command::ExecutionProviderCommandKind::Pty,
    )?;
    crate::plugin_paths::ensure_plugin_user_dirs(&provider.plugin_id)?;
    let mut command =
        crate::plugin_command::pty_command_for_argv_in_dir(&provider.command, &provider.cwd)?;
    for key in provider.remove_env {
        command.env_remove(key);
    }
    for (key, value) in provider.env {
        command.env(key, value);
    }
    command.env(EXECUTION_PROVIDER_REQUEST_ENV, payload);
    command.env("TERM", crate::pane::PANE_TERM);
    command.env("COLORTERM", crate::pane::PANE_COLORTERM);
    Ok(PreparedExecutionProviderPtyCommand {
        command,
        ready_nonce,
    })
}

pub(crate) fn execution_provider_process_command(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
) -> std::io::Result<PreparedExecutionProviderProcessCommand> {
    let (scheme, payload, _ready_nonce) =
        execution_provider_request(target, cwd, command, env, Vec::new())?;
    let provider = crate::plugin_command::resolve_installed_execution_provider(
        &scheme,
        crate::plugin_command::ExecutionProviderCommandKind::Process,
    )?;
    crate::plugin_paths::ensure_plugin_user_dirs(&provider.plugin_id)?;
    let (program, args) = provider.command.split_first().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "execution provider process command must not be empty",
        )
    })?;
    let mut command = crate::plugin_command::command_for_argv_in_dir(program, args, &provider.cwd);
    for key in provider.remove_env {
        command.env_remove(key);
    }
    for (key, value) in provider.env {
        command.env(key, value);
    }
    command.env(EXECUTION_PROVIDER_REQUEST_ENV, payload);
    Ok(PreparedExecutionProviderProcessCommand { command })
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteExecRequest {
    cwd: PathBuf,
    command: RemoteCommand,
    env: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remove_env: Vec<String>,
    api_socket: PathBuf,
    ready_nonce: RemoteExecReadyNonce,
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
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
            "-o".to_string(),
            "ConnectionAttempts=1".to_string(),
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
    delivery: std_mpsc::Receiver<std::io::Result<()>>,
}

#[cfg(not(unix))]
pub(crate) struct RemoteRequestChannel;

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemoteRequestDelivery {
    Pending,
    Delivered,
    Failed(String),
}

#[cfg(not(unix))]
impl RemoteRequestChannel {
    pub(crate) fn cancel(self) {}
}

#[cfg(unix)]
impl RemoteRequestChannel {
    fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn delivery_status(&self) -> RemoteRequestDelivery {
        match self.delivery.try_recv() {
            Ok(Ok(())) => RemoteRequestDelivery::Delivered,
            Ok(Err(err)) => RemoteRequestDelivery::Failed(err.to_string()),
            Err(std_mpsc::TryRecvError::Empty) => RemoteRequestDelivery::Pending,
            Err(std_mpsc::TryRecvError::Disconnected) => RemoteRequestDelivery::Failed(
                "remote execution request channel disconnected".into(),
            ),
        }
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
    pub(crate) ready_nonce: RemoteExecReadyNonce,
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
    remove_env: Vec<String>,
) -> std::io::Result<(RemoteExecSsh, RemoteRequestChannel, RemoteExecReadyNonce)> {
    let api_socket = random_socket_path(REMOTE_API_SOCKET_PREFIX)?;
    let request_socket = random_socket_path(REMOTE_REQUEST_SOCKET_PREFIX)?;
    let api_forward = socket_forward(&api_socket, &crate::api::socket_path())?;
    let ready_nonce = RemoteExecReadyNonce::generate()?;
    let shell_path = crate::remote::resolve_prepared_remote_shell_path(host)?;
    let request = RemoteExecRequest {
        cwd: cwd.to_path_buf(),
        command,
        env,
        remove_env,
        api_socket: api_socket.clone(),
        ready_nonce: ready_nonce.clone(),
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
        ready_nonce,
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
    let socket = crate::remote::shell_quote(&request_socket.display().to_string());
    format!("exec {shell_path} remote-exec {socket}")
}

#[cfg(unix)]
fn start_remote_request_channel(
    request: RemoteExecRequest,
) -> std::io::Result<RemoteRequestChannel> {
    let payload = encode_remote_exec_request(&request)?;

    for _ in 0..REMOTE_SOCKET_BIND_ATTEMPTS {
        let local_socket = random_socket_path(LOCAL_REQUEST_SOCKET_PREFIX)?;
        let (listener, socket_identity) = match bind_private_request_listener(&local_socket) {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err),
        };

        let cleanup_identity = socket_identity.clone();
        let thread_socket = local_socket.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let (delivery_tx, delivery) = std_mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("herdr-remote-exec-request".to_string())
            .spawn(move || {
                let result = serve_remote_exec_request(
                    listener,
                    payload,
                    thread_socket,
                    socket_identity,
                    thread_cancelled,
                    REMOTE_REQUEST_CONNECT_TIMEOUT,
                );
                let _ = delivery_tx.send(result);
            });
        if let Err(err) = thread {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &cleanup_identity);
            return Err(err);
        }

        return Ok(RemoteRequestChannel {
            path: local_socket,
            cancelled,
            delivery,
        });
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "could not create a unique remote execution request socket",
    ))
}

#[cfg(unix)]
fn bind_private_request_listener(
    path: &Path,
) -> std::io::Result<(UnixListener, crate::ipc::SocketFileIdentity)> {
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
    Ok((listener, socket_identity))
}

#[cfg(unix)]
fn serve_remote_exec_request(
    listener: UnixListener,
    payload: Vec<u8>,
    socket: PathBuf,
    socket_identity: crate::ipc::SocketFileIdentity,
    cancelled: Arc<AtomicBool>,
    connect_timeout: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + connect_timeout;
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
    if let Err(err) = &result {
        tracing::debug!(%err, "remote execution request channel closed without a request");
    }
    drop(listener);
    if let Err(err) = crate::ipc::remove_socket_file_if_owned(&socket, &socket_identity) {
        tracing::debug!(socket = %socket.display(), %err, "failed to remove remote execution request socket");
    }
    result
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
    let nonce = random_hex_string(REMOTE_SOCKET_NONCE_BYTES)?;
    Ok(Path::new("/tmp").join(format!("{prefix}{nonce}.sock")))
}

pub(crate) fn ssh_pty_command_with_removals(
    target: &ExecutionTarget,
    cwd: &Path,
    command: RemoteCommand,
    env: Vec<(String, String)>,
    remove_env: Vec<String>,
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
        let _ = (cwd, command, env, remove_env);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "per-terminal SSH execution is only supported on Unix",
        ))
    }

    #[cfg(unix)]
    {
        let (remote_exec, request_channel, ready_nonce) =
            prepare_remote_exec_ssh(host, cwd, command, env, remove_env)?;
        let mut ssh = CommandBuilder::new("ssh");
        ssh.args(remote_exec.ssh_args(host, true));
        ssh.env("TERM", crate::pane::PANE_TERM);
        ssh.env("COLORTERM", crate::pane::PANE_COLORTERM);
        Ok(PreparedSshPtyCommand {
            command: ssh,
            request_channel,
            ready_nonce,
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
        let (remote_exec, request_channel, _ready_nonce) =
            prepare_remote_exec_ssh(host, cwd, command, env, Vec::new())?;
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
        let RemoteExecRequest {
            cwd: requested_cwd,
            command: requested_command,
            env: mut requested_env,
            remove_env,
            api_socket,
            ready_nonce,
        } = request;

        let resolved = remote_process_command(&requested_command)?;
        let mut command = resolved.command;
        let cwd = if requested_cwd.as_os_str().is_empty() {
            resolved.cwd
        } else {
            Some(requested_cwd)
        };
        requested_env.extend(resolved.env);
        let cwd = apply_remote_cwd(&mut command, cwd.as_deref(), &mut requested_env)?;
        apply_remote_exec_env(&mut command, requested_env, remove_env);
        command.env("TERM", crate::pane::PANE_TERM);
        command.env("COLORTERM", crate::pane::PANE_COLORTERM);
        command.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        command.env(crate::api::SOCKET_PATH_ENV_VAR, &api_socket);
        if let Ok(executable) = std::env::current_exe() {
            command.env("HERDR_BIN_PATH", executable);
        }

        let ready_marker = remote_exec_ready_marker_for_terminal(
            std::io::stdout().is_terminal(),
            &ready_nonce,
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
fn apply_remote_exec_env(
    command: &mut std::process::Command,
    env: Vec<(String, String)>,
    remove_env: Vec<String>,
) {
    command.env_remove("CODEX_THREAD_ID");
    // Outer mux markers no longer describe the process launched inside Herdr.
    for key in ["TMUX", "STY", "ZELLIJ"] {
        command.env_remove(key);
    }
    command.env_remove(crate::integration::HERDR_WORKSPACE_ID_ENV_VAR);
    command.env_remove(crate::integration::HERDR_TAB_ID_ENV_VAR);
    command.env_remove(crate::integration::HERDR_PANE_ID_ENV_VAR);
    command.env_remove("HERDR_VIEW_ID");
    for key in remove_env {
        command.env_remove(key);
    }
    command.envs(env);
    command.env_remove("HERDR_BUILD_BIN_PATH");
}
#[cfg(unix)]
#[derive(Serialize)]
struct RemoteExecReadyPayload<'a> {
    nonce: &'a str,
    hostname: Option<&'a str>,
    cwd: &'a Path,
}

#[cfg(unix)]
pub(crate) fn remote_exec_ready_marker(
    nonce: &RemoteExecReadyNonce,
    hostname: Option<&str>,
    cwd: &Path,
) -> std::io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(&RemoteExecReadyPayload {
        nonce: nonce.as_str(),
        hostname,
        cwd,
    })
    .map_err(std::io::Error::other)?;
    if payload.len() > REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "remote execution ready payload exceeds {REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES} bytes"
            ),
        ));
    }
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
    nonce: &RemoteExecReadyNonce,
    hostname: Option<&str>,
    cwd: &Path,
) -> std::io::Result<Option<Vec<u8>>> {
    stdout_is_terminal
        .then(|| remote_exec_ready_marker(nonce, hostname, cwd))
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
            Vec::new(),
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
    fn remote_exec_scrubs_inherited_mux_markers() {
        let mut command = std::process::Command::new("true");
        for key in ["TMUX", "STY", "ZELLIJ"] {
            command.env(key, "outer");
        }

        apply_remote_exec_env(&mut command, Vec::new(), Vec::new());

        for key in ["TMUX", "STY", "ZELLIJ"] {
            assert_eq!(
                command.get_envs().find(|(name, _)| *name == key),
                Some((std::ffi::OsStr::new(key), None)),
                "inherited {key} marker survived"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_applies_explicit_environment_removals() {
        let mut command = std::process::Command::new("true");
        command.env("BUN_OPTIONS", "outer");

        apply_remote_exec_env(&mut command, Vec::new(), vec!["BUN_OPTIONS".to_string()]);

        assert_eq!(
            command.get_envs().find(|(name, _)| *name == "BUN_OPTIONS"),
            Some((std::ffi::OsStr::new("BUN_OPTIONS"), None))
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_ready_marker_json_encodes_hostname_and_resolved_cwd() {
        let cwd = Path::new("/remote/plugin-root");
        let nonce = RemoteExecReadyNonce::generate().unwrap();
        let expected = format!(
            "\x1b]6973;herdr-remote-exec-ready={{\"nonce\":\"{}\",\"hostname\":\"build-\\\"node\",\"cwd\":\"/remote/plugin-root\"}}\x1b\\",
            nonce.as_str()
        );
        assert_eq!(
            remote_exec_ready_marker(&nonce, Some("build-\"node"), cwd).unwrap(),
            expected.as_bytes()
        );
        assert_eq!(
            remote_exec_ready_marker_for_terminal(false, &nonce, Some("build-node"), cwd).unwrap(),
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
        let runtime: ExecutionTarget = "runtime:dev2".parse().unwrap();
        assert_eq!(local, ExecutionTarget::Local);
        assert_eq!(ssh.to_string(), "ssh:primary");
        assert_eq!(runtime.to_string(), "runtime:dev2");
        assert_eq!(
            serde_json::to_value(&runtime).unwrap(),
            serde_json::json!({
                "kind": "extension",
                "scheme": "runtime",
                "target": "dev2"
            })
        );
        assert!("ssh:-bad".parse::<ExecutionTarget>().is_err());
        assert!("local:anywhere".parse::<ExecutionTarget>().is_err());
        assert!("Runtime:dev2".parse::<ExecutionTarget>().is_err());
        assert!("runtime:bad target".parse::<ExecutionTarget>().is_err());
    }

    #[test]
    fn execution_provider_request_is_versioned_tagged_and_bounded() {
        let target = ExecutionTarget::extension("runtime", "dev2").unwrap();
        let (_, payload, ready_nonce) = execution_provider_request(
            &target,
            Path::new("/work"),
            RemoteCommand::Argv {
                argv: vec!["omp".into(), "--help".into()],
            },
            vec![("EXAMPLE".into(), "value".into())],
            vec!["BUN_OPTIONS".into()],
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(request["version"], EXECUTION_PROVIDER_PROTOCOL);
        assert_eq!(request["target"], "dev2");
        assert_eq!(request["cwd"], "/work");
        assert_eq!(request["command"]["kind"], "argv");
        assert_eq!(request["command"]["argv"][0], "omp");
        assert_eq!(request["ready_nonce"], ready_nonce.as_str());

        let error = execution_provider_request(
            &target,
            Path::new(""),
            RemoteCommand::ShellCommand {
                command: "x".repeat(EXECUTION_PROVIDER_REQUEST_MAX_BYTES),
            },
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
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
        let (listener, identity) = bind_private_request_listener(&socket).unwrap();

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
            remove_env: Vec::new(),
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
            ready_nonce: RemoteExecReadyNonce::generate().unwrap(),
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
    fn request_delivery_timeout_is_reported_and_reclaims_socket() {
        let request = RemoteExecRequest {
            cwd: PathBuf::new(),
            command: RemoteCommand::Shell,
            env: Vec::new(),
            remove_env: Vec::new(),
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
            ready_nonce: RemoteExecReadyNonce::generate().unwrap(),
        };
        let payload = encode_remote_exec_request(&request).unwrap();
        let socket = random_socket_path(LOCAL_REQUEST_SOCKET_PREFIX).unwrap();
        let (listener, identity) = bind_private_request_listener(&socket).unwrap();

        let result = serve_remote_exec_request(
            listener,
            payload,
            socket.clone(),
            identity,
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(1),
        );

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(!socket.exists());
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
            remove_env: vec!["BUN_OPTIONS".to_string()],
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
            ready_nonce: RemoteExecReadyNonce::generate().unwrap(),
        };
        let payload = encode_remote_exec_request(&request).unwrap();
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();

        write_remote_exec_request(&mut sender, &payload).unwrap();
        let received = read_remote_exec_request(&mut receiver).unwrap();

        assert_eq!(received.cwd, request.cwd);
        assert_eq!(received.command, request.command);
        assert_eq!(received.env, request.env);
        assert_eq!(received.remove_env, request.remove_env);
        assert_eq!(received.api_socket, request.api_socket);
        assert_eq!(received.ready_nonce, request.ready_nonce);
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
            remove_env: Vec::new(),
            api_socket: PathBuf::from(
                "/tmp/herdr-remote-api-0123456789abcdef0123456789abcdef.sock",
            ),
            ready_nonce: RemoteExecReadyNonce::generate().unwrap(),
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
        for option in [
            "BatchMode=yes",
            "ConnectTimeout=10",
            "ConnectionAttempts=1",
            "StreamLocalBindMask=0177",
            "ControlPath=none",
        ] {
            for args in [&pty_args, &process_args] {
                assert!(args.windows(2).any(|args| args == ["-o", option]));
            }
        }
        assert!(!pty_args
            .iter()
            .any(|arg| arg.contains("StreamLocalBindUnlink")));
        assert_eq!(pty_args.last(), Some(&remote_command));
        assert!(!pty_args.iter().any(|arg| arg.contains("structured")));
    }

    #[cfg(unix)]
    #[test]
    fn remote_exec_command_uses_only_the_verified_helper() {
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

    #[cfg(unix)]
    #[test]
    fn remote_exec_ready_marker_enforces_shared_payload_bound() {
        let nonce = RemoteExecReadyNonce::generate().unwrap();
        let accepted = PathBuf::from(format!("/{}", "x".repeat(4096)));
        assert!(remote_exec_ready_marker(&nonce, Some("build-node"), &accepted).is_ok());

        let oversized = PathBuf::from(format!(
            "/{}",
            "x".repeat(REMOTE_EXEC_READY_PAYLOAD_MAX_BYTES)
        ));
        assert_eq!(
            remote_exec_ready_marker(&nonce, Some("build-node"), &oversized)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
