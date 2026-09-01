use std::io::Read;
use std::process::Stdio;

use super::manifest::{effective_platforms, ensure_platform_supported};
use super::plugin_manifest_available;
use crate::api::schema::{
    InstalledPluginInfo, PluginCommandLogInfo, PluginCommandStatus, PluginInvocationContext,
};
use crate::app::App;

const PLUGIN_COMMAND_OUTPUT_MAX_BYTES: usize = 64 * 1024;
pub(super) const MAX_PLUGIN_COMMANDS_IN_FLIGHT: usize = 32;
const PLUGIN_COMMAND_LOG_LIMIT: usize = 200;

const PLUGIN_COMMAND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const PLUGIN_COMMAND_TERMINATION_ATTEMPTS: usize = 40;
const PLUGIN_COMMAND_OUTPUT_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(250);
#[cfg(windows)]
trait PluginOutputPipe: Read + std::os::windows::io::AsRawHandle + Send {}
#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle + Send> PluginOutputPipe for T {}

#[cfg(not(windows))]
trait PluginOutputPipe: Read + Send {}
#[cfg(not(windows))]
impl<T: Read + Send> PluginOutputPipe for T {}

pub(crate) struct PluginCommandRuntime {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PluginCommandRuntime {
    fn new(
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        worker: std::thread::JoinHandle<()>,
    ) -> Self {
        Self {
            cancelled,
            worker: Some(worker),
        }
    }

    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for PluginCommandRuntime {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

struct PluginOutputReader {
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<String>>,
}

impl PluginOutputReader {
    fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .map(std::thread::JoinHandle::is_finished)
            .unwrap_or(true)
    }

    fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn join(&mut self) -> String {
        self.worker
            .take()
            .and_then(|worker| worker.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for PluginOutputReader {
    fn drop(&mut self) {
        self.stop();
        let _ = self.join();
    }
}

#[cfg(unix)]
fn configure_plugin_output_pipes(child: &std::process::Child) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    if let Some(stdout) = child.stdout.as_ref() {
        crate::pty::fd::set_nonblocking(stdout.as_raw_fd())?;
    }
    if let Some(stderr) = child.stderr.as_ref() {
        crate::pty::fd::set_nonblocking(stderr.as_raw_fd())?;
    }
    Ok(())
}

fn spawn_plugin_command(
    mut command: std::process::Command,
) -> std::io::Result<(std::process::Child, crate::platform::ProcessTreeGuard)> {
    crate::platform::configure_process_tree_command(&mut command);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    match crate::platform::ProcessTreeGuard::new_std(&child) {
        Ok(process_tree) => {
            #[cfg(unix)]
            let process_tree = {
                let mut process_tree = process_tree;
                if let Err(error) = configure_plugin_output_pipes(&child) {
                    process_tree.terminate();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                process_tree
            };
            Ok((child, process_tree))
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

fn wait_for_plugin_child_termination(
    mut poll: impl FnMut() -> std::io::Result<bool>,
    mut wait: impl FnMut(),
) -> std::io::Result<()> {
    for attempt in 0..PLUGIN_COMMAND_TERMINATION_ATTEMPTS {
        match poll() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 < PLUGIN_COMMAND_TERMINATION_ATTEMPTS {
            wait();
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out reaping terminated plugin command",
    ))
}

type PluginReaperItem = (std::process::Child, crate::platform::ProcessTreeGuard);

static PLUGIN_REAPER: std::sync::LazyLock<std::sync::mpsc::Sender<PluginReaperItem>> =
    std::sync::LazyLock::new(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<PluginReaperItem>();
        if let Err(error) = std::thread::Builder::new()
            .name("herdr-plugin-reaper".to_owned())
            .spawn(move || {
                for (mut child, mut process_tree) in receiver {
                    let pid = child.id();
                    process_tree.terminate();
                    let _ = child.kill();
                    if let Err(error) = child.wait() {
                        tracing::warn!(pid, err = %error, "background plugin command reaper failed");
                    }
                }
            })
        {
            tracing::warn!(err = %error, "failed to start background plugin command reaper");
        }
        sender
    });

fn plugin_reaper_sender() -> &'static std::sync::mpsc::Sender<PluginReaperItem> {
    &PLUGIN_REAPER
}

fn handoff_plugin_child_to_reaper(
    child: std::process::Child,
    process_tree: crate::platform::ProcessTreeGuard,
) -> Result<(), PluginReaperItem> {
    plugin_reaper_sender()
        .send((child, process_tree))
        .map_err(|error| error.0)
}

fn terminate_plugin_child(
    mut child: std::process::Child,
    mut process_tree: crate::platform::ProcessTreeGuard,
) {
    process_tree.terminate();
    let result = wait_for_plugin_child_termination(
        || match child.try_wait()? {
            Some(_) => Ok(true),
            None => {
                let _ = child.kill();
                Ok(false)
            }
        },
        || std::thread::sleep(PLUGIN_COMMAND_POLL_INTERVAL),
    );
    if result.is_ok() {
        return;
    }

    let pid = child.id();
    tracing::warn!(pid, err = %result.unwrap_err(), "plugin command reaping exceeded the shutdown deadline; continuing in background");
    if let Err((mut child, mut process_tree)) = handoff_plugin_child_to_reaper(child, process_tree)
    {
        tracing::warn!(
            pid,
            "background plugin command reaper unavailable; reaping synchronously"
        );
        process_tree.terminate();
        let _ = child.kill();
        if let Err(error) = child.wait() {
            tracing::warn!(pid, err = %error, "synchronous plugin command reaper failed");
        }
    }
}

fn spawn_plugin_output_reader(reader: impl PluginOutputPipe + 'static) -> PluginOutputReader {
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_stopped = stopped.clone();
    let worker = std::thread::spawn(move || {
        read_capped_plugin_output_until_stopped(
            reader,
            PLUGIN_COMMAND_OUTPUT_MAX_BYTES,
            worker_stopped.as_ref(),
        )
    });
    PluginOutputReader {
        stopped,
        worker: Some(worker),
    }
}

fn collect_plugin_outputs(
    mut stdout: Option<PluginOutputReader>,
    mut stderr: Option<PluginOutputReader>,
    deadline: std::time::Instant,
) -> (String, String) {
    loop {
        let all_finished = stdout
            .as_ref()
            .map(PluginOutputReader::is_finished)
            .unwrap_or(true)
            && stderr
                .as_ref()
                .map(PluginOutputReader::is_finished)
                .unwrap_or(true);
        if all_finished || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(
            PLUGIN_COMMAND_POLL_INTERVAL
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
    for reader in [&stdout, &stderr].into_iter().flatten() {
        reader.stop();
    }
    (
        stdout
            .as_mut()
            .map(PluginOutputReader::join)
            .unwrap_or_default(),
        stderr
            .as_mut()
            .map(PluginOutputReader::join)
            .unwrap_or_default(),
    )
}

fn publish_plugin_command_finished(
    event_tx: &tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    cancelled: &std::sync::atomic::AtomicBool,
    mut event: crate::events::AppEvent,
) {
    loop {
        match event_tx.try_send(event) {
            Ok(()) => return,
            Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                event = returned;
                std::thread::sleep(PLUGIN_COMMAND_POLL_INTERVAL);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

impl App {
    pub(super) fn start_plugin_command(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        command: Vec<String>,
        context: &PluginInvocationContext,
        execution_target: crate::execution::ExecutionTarget,
        event_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        let local_command = if execution_target.is_local() {
            let Some(program) = command.first().cloned() else {
                return Err((
                    "invalid_plugin_command",
                    "command must not be empty".to_string(),
                ));
            };
            let args = command.iter().skip(1).cloned().collect::<Vec<_>>();
            super::env::ensure_plugin_user_dirs(plugin)
                .map_err(|err| ("plugin_user_dir_create_failed", err.to_string()))?;
            Some((program, args))
        } else {
            None
        };
        let context_json = serde_json::to_string(context)
            .map_err(|err| ("invalid_plugin_context", err.to_string()))?;
        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        let mut env = Vec::new();
        if execution_target.is_local() {
            env.extend(super::env::local_plugin_runtime_env(plugin));
        }
        env.extend([
            ("HERDR_ENV".to_string(), "1".to_string()),
            ("HERDR_PLUGIN_ID".to_string(), plugin.plugin_id.clone()),
            ("HERDR_PLUGIN_CONTEXT_JSON".to_string(), context_json),
        ]);
        if let Some(action_id) = action_id.as_ref() {
            env.push(("HERDR_PLUGIN_ACTION_ID".to_string(), action_id.clone()));
        }
        if let Some(event) = event.as_ref() {
            env.push(("HERDR_PLUGIN_EVENT".to_string(), event.clone()));
        }
        if let Some(event_json) = event_json {
            env.push(("HERDR_PLUGIN_EVENT_JSON".to_string(), event_json));
        }
        if let Some(workspace_id) = context.workspace_id.as_ref() {
            env.push(("HERDR_WORKSPACE_ID".to_string(), workspace_id.clone()));
        }
        if let Some(tab_id) = context.tab_id.as_ref() {
            env.push(("HERDR_TAB_ID".to_string(), tab_id.clone()));
        }
        if let Some(pane_id) = context.focused_pane_id.as_ref() {
            env.push(("HERDR_PANE_ID".to_string(), pane_id.clone()));
        }
        if let Some(view_id) = context.view_id.as_ref() {
            env.push(("HERDR_VIEW_ID".to_string(), view_id.to_string()));
        }
        if let Some(clicked_url) = context.clicked_url.as_ref() {
            env.push(("HERDR_PLUGIN_CLICKED_URL".to_string(), clicked_url.clone()));
        }
        if let Some(link_handler_id) = context.link_handler_id.as_ref() {
            env.push((
                "HERDR_PLUGIN_LINK_HANDLER_ID".to_string(),
                link_handler_id.clone(),
            ));
        }
        if self.state.plugin_commands_in_flight >= MAX_PLUGIN_COMMANDS_IN_FLIGHT {
            let message = format!(
                "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
            );
            let log = PluginCommandLogInfo {
                log_id,
                plugin_id: plugin.plugin_id.clone(),
                action_id,
                event,
                command,
                status: PluginCommandStatus::Failed,
                started_unix_ms,
                finished_unix_ms: Some(started_unix_ms),
                exit_code: None,
                stdout: Some(String::new()),
                stderr: Some(String::new()),
                error: Some(message.clone()),
            };
            self.push_plugin_command_log(log);
            return Err(("plugin_command_limit_reached", message));
        }
        let plugin_root = std::path::PathBuf::from(&plugin.plugin_root);
        let action_id_for_launch = action_id.clone();
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin.plugin_id.clone(),
            action_id,
            event,
            command: command.clone(),
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        self.push_plugin_command_log(log.clone());
        self.state.plugin_commands_in_flight += 1;
        let plugin_id = plugin.plugin_id.clone();
        let link_handler_id = context.link_handler_id.clone();
        let event_tx = self.event_tx.clone();
        let runtime_log_id = log_id.clone();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker = std::thread::spawn(move || {
            let (child, remote_request_channel) = match execution_target {
                crate::execution::ExecutionTarget::Local => match local_command {
                    Some((program, args)) => {
                        let mut command = crate::plugin_command::command_for_argv_in_dir(
                            &program,
                            &args,
                            &plugin_root,
                        );
                        command.env_remove("HERDR_VIEW_ID");
                        apply_plugin_runtime_env(&mut command, env);
                        (spawn_plugin_command(command), None)
                    }
                    None => (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "local plugin command is missing",
                        )),
                        None,
                    ),
                },
                crate::execution::ExecutionTarget::Ssh { .. } => match action_id_for_launch {
                    Some(action_id) => match crate::execution::ssh_process_command(
                        &execution_target,
                        std::path::Path::new(""),
                        crate::execution::RemoteCommand::Plugin {
                            plugin_id,
                            target: crate::plugin_command::PluginCommandTarget::Action {
                                action_id,
                                link_handler_id,
                            },
                        },
                        env,
                    ) {
                        Ok(prepared) => (
                            spawn_plugin_command(prepared.command),
                            Some(prepared.request_channel),
                        ),
                        Err(err) => (Err(err), None),
                    },
                    None => (
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "remote SSH plugin action id is missing",
                        )),
                        None,
                    ),
                },
                crate::execution::ExecutionTarget::Extension { .. } => {
                    match crate::execution::execution_provider_process_command(
                        &execution_target,
                        std::path::Path::new(""),
                        crate::execution::RemoteCommand::Argv { argv: command },
                        env,
                    ) {
                        Ok(prepared) => (spawn_plugin_command(prepared.command), None),
                        Err(err) => (Err(err), None),
                    }
                }
            };
            let finished = match child {
                Ok((mut child, process_tree)) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let stdout_reader = stdout.map(spawn_plugin_output_reader);
                    let stderr_reader = stderr.map(spawn_plugin_output_reader);
                    let mut child_and_tree = Some((child, process_tree));
                    let mut remote_request_channel = remote_request_channel;
                    #[cfg(unix)]
                    let mut request_delivered = remote_request_channel.is_none();
                    let status = loop {
                        if worker_cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            if let Some(channel) = remote_request_channel.take() {
                                channel.cancel();
                            }
                            let (child, process_tree) = child_and_tree.take().unwrap();
                            terminate_plugin_child(child, process_tree);
                            break Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "plugin command cancelled",
                            ));
                        }
                        #[cfg(unix)]
                        if !request_delivered {
                            match remote_request_channel
                                .as_ref()
                                .expect("pending request channel")
                                .delivery_status()
                            {
                                crate::execution::RemoteRequestDelivery::Delivered => {
                                    request_delivered = true;
                                }
                                crate::execution::RemoteRequestDelivery::Failed(err) => {
                                    if let Some(channel) = remote_request_channel.take() {
                                        channel.cancel();
                                    }
                                    let (child, process_tree) = child_and_tree.take().unwrap();
                                    terminate_plugin_child(child, process_tree);
                                    break Err(std::io::Error::other(err));
                                }
                                crate::execution::RemoteRequestDelivery::Pending => {}
                            }
                        }
                        let (child, process_tree) = child_and_tree.as_mut().unwrap();
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                process_tree.terminate();
                                break Ok(status);
                            }
                            Ok(None) => std::thread::sleep(PLUGIN_COMMAND_POLL_INTERVAL),
                            Err(err) => {
                                if let Some(channel) = remote_request_channel.take() {
                                    channel.cancel();
                                }
                                let (child, process_tree) = child_and_tree.take().unwrap();
                                terminate_plugin_child(child, process_tree);
                                break Err(err);
                            }
                        }
                    };
                    let output_deadline =
                        std::time::Instant::now() + PLUGIN_COMMAND_OUTPUT_DRAIN_TIMEOUT;
                    let (stdout, stderr) =
                        collect_plugin_outputs(stdout_reader, stderr_reader, output_deadline);
                    match status {
                        Ok(status) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: status.code(),
                            stdout,
                            stderr,
                            error: None,
                        },
                        Err(err) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: None,
                            stdout,
                            stderr,
                            error: Some(err.to_string()),
                        },
                    }
                }
                Err(err) => {
                    if let Some(channel) = remote_request_channel {
                        channel.cancel();
                    }
                    crate::events::AppEvent::PluginCommandFinished {
                        log_id,
                        finished_unix_ms: current_unix_ms(),
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(err.to_string()),
                    }
                }
            };
            publish_plugin_command_finished(&event_tx, worker_cancelled.as_ref(), finished);
        });
        self.plugin_command_runtimes
            .insert(runtime_log_id, PluginCommandRuntime::new(cancelled, worker));
        Ok(log)
    }

    pub(crate) fn shutdown_plugin_commands(&mut self) {
        let mut runtimes = std::mem::take(&mut self.plugin_command_runtimes);
        for runtime in runtimes.values() {
            runtime.cancel();
        }
        self.state.plugin_commands_in_flight = 0;
        for runtime in runtimes.values_mut() {
            runtime.join();
        }
    }

    pub(super) fn plugin_command_execution_target(
        &self,
        context: &PluginInvocationContext,
    ) -> crate::execution::ExecutionTarget {
        context
            .focused_pane_id
            .as_deref()
            .and_then(|pane_id| {
                self.plugin_context_and_execution_target_for_source_pane(pane_id, "plugin-target")
            })
            .map(|(_, target)| target)
            .unwrap_or_default()
    }

    pub(crate) fn run_plugin_startup_hooks(&mut self) {
        let mut context = self.current_plugin_context("plugin.startup");
        context.invocation_source = Some("startup".to_string());
        let mut plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled && plugin_manifest_available(plugin) && !plugin.startup.is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        plugins.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        for plugin in plugins {
            for startup in plugin.startup.clone() {
                if ensure_platform_supported(
                    &effective_platforms(&startup.platforms, &plugin.platforms).clone(),
                    "startup",
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some("startup".to_string()),
                    startup.command,
                    &context,
                    crate::execution::ExecutionTarget::Local,
                    None,
                );
            }
        }
    }

    pub(crate) fn run_plugin_event_hooks(&mut self, event: &crate::api::schema::EventEnvelope) {
        let event_name = event.event.dot_name();
        if !crate::api::schema::PLUGIN_HOOK_EVENT_KINDS.contains(&event.event) {
            return;
        }
        if let Err(err) = self.refresh_installed_plugins() {
            tracing::warn!(err = %err, "failed to refresh plugin registry before event hooks");
            return;
        }
        let plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled
                    && plugin_manifest_available(plugin)
                    && plugin.events.iter().any(|hook| hook.on == event_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if plugins.is_empty() {
            return;
        }
        let event_json = serde_json::to_string(event).ok();
        let context = self.plugin_context_for_event(event, event_name);
        for plugin in plugins {
            for hook in plugin.events.clone() {
                if hook.on != event_name {
                    continue;
                }
                if ensure_platform_supported(
                    &effective_platforms(&hook.platforms, &plugin.platforms).clone(),
                    event_name,
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some(event_name.to_string()),
                    hook.command.clone(),
                    &context,
                    crate::execution::ExecutionTarget::Local,
                    event_json.clone(),
                );
            }
        }
    }

    fn push_plugin_command_log(&mut self, log: PluginCommandLogInfo) {
        self.state.plugin_command_logs.push(log);
        if self.state.plugin_command_logs.len() > PLUGIN_COMMAND_LOG_LIMIT {
            let extra = self.state.plugin_command_logs.len() - PLUGIN_COMMAND_LOG_LIMIT;
            self.state.plugin_command_logs.drain(0..extra);
        }
    }
}

fn apply_plugin_runtime_env(command: &mut std::process::Command, env: Vec<(String, String)>) {
    command.envs(env);
    command.env_remove("HERDR_BUILD_BIN_PATH");
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn poll_windows_plugin_output<R>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize>
where
    R: Read + std::os::windows::io::AsRawHandle,
{
    let mut available = 0;
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            reader.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if available == 0 {
        return Err(std::io::ErrorKind::WouldBlock.into());
    }
    let available = buf.len().min(available as usize);
    reader.read(&mut buf[..available])
}

#[cfg(test)]
pub(super) fn read_capped_plugin_output(mut reader: impl Read, cap: usize) -> String {
    let stopped = std::sync::atomic::AtomicBool::new(false);
    read_capped_plugin_output_with(cap, &stopped, |buf| reader.read(buf))
}

fn read_capped_plugin_output_until_stopped(
    mut reader: impl PluginOutputPipe,
    cap: usize,
    stopped: &std::sync::atomic::AtomicBool,
) -> String {
    read_capped_plugin_output_with(cap, stopped, |buf| {
        #[cfg(windows)]
        {
            poll_windows_plugin_output(&mut reader, buf)
        }
        #[cfg(not(windows))]
        {
            reader.read(buf)
        }
    })
}

fn read_capped_plugin_output_with(
    cap: usize,
    stopped: &std::sync::atomic::AtomicBool,
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> String {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        match read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = cap.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buf[..n.min(remaining)]);
                }
                if n > remaining {
                    truncated = true;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(PLUGIN_COMMAND_POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str(&format!(
            "\n[herdr truncated plugin output after {cap} bytes]"
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    struct ObservedWindowsPluginPipe {
        stdout: std::process::ChildStdout,
        polled: std::sync::mpsc::Sender<()>,
    }

    #[cfg(windows)]
    impl std::io::Read for ObservedWindowsPluginPipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.stdout.read(buf)
        }
    }

    #[cfg(windows)]
    impl std::os::windows::io::AsRawHandle for ObservedWindowsPluginPipe {
        fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
            let _ = self.polled.send(());
            std::os::windows::io::AsRawHandle::as_raw_handle(&self.stdout)
        }
    }

    #[test]
    fn plugin_child_wait_bounds_repeated_interrupted_errors() {
        let mut attempts = 0;
        let mut waits = 0;
        let error = wait_for_plugin_child_termination(
            || {
                attempts += 1;
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            },
            || waits += 1,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(attempts, PLUGIN_COMMAND_TERMINATION_ATTEMPTS);
        assert_eq!(waits, PLUGIN_COMMAND_TERMINATION_ATTEMPTS - 1);
    }
    #[cfg(windows)]
    #[test]
    fn windows_plugin_output_reader_stop_and_join_is_bounded_while_pipe_open() {
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (polled_tx, polled_rx) = std::sync::mpsc::channel();
        let mut reader = spawn_plugin_output_reader(ObservedWindowsPluginPipe {
            stdout,
            polled: polled_tx,
        });

        let polling_open_pipe = polled_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok()
            && !reader.is_finished();
        if !polling_open_pipe {
            reader.stop();
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
        }
        assert!(
            polling_open_pipe,
            "plugin output reader did not remain polling while the pipe was open"
        );

        let (joined_tx, joined_rx) = std::sync::mpsc::sync_channel(1);
        let joiner = std::thread::spawn(move || {
            reader.stop();
            let _ = reader.join();
            let _ = joined_tx.send(());
        });
        let joined = joined_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok();
        let _ = child.kill();
        let _ = child.wait();
        let _ = joiner.join();

        assert!(
            joined,
            "plugin output reader stop+join blocked while the Windows pipe remained open"
        );
    }

    #[test]
    fn plugin_shutdown_cancels_all_runtimes_before_joining() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        for index in 0..MAX_PLUGIN_COMMANDS_IN_FLIGHT {
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_cancelled = cancelled.clone();
            let worker_observed = observed_tx.clone();
            let worker_release = release.clone();
            let worker = std::thread::spawn(move || {
                while !worker_cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let _ = worker_observed.send(());
                while !worker_release.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            app.plugin_command_runtimes.insert(
                format!("fanout-{index}"),
                PluginCommandRuntime::new(cancelled, worker),
            );
        }
        drop(observed_tx);
        app.state.plugin_commands_in_flight = MAX_PLUGIN_COMMANDS_IN_FLIGHT;
        let observer_release = release.clone();
        let observer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut observed = 0;
            while observed < MAX_PLUGIN_COMMANDS_IN_FLIGHT {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() || observed_rx.recv_timeout(remaining).is_err() {
                    break;
                }
                observed += 1;
            }
            observer_release.store(true, std::sync::atomic::Ordering::Release);
            observed
        });

        app.shutdown_plugin_commands();

        assert_eq!(
            observer.join().unwrap(),
            MAX_PLUGIN_COMMANDS_IN_FLIGHT,
            "every runtime must observe cancellation before the first join blocks"
        );
        assert!(app.plugin_command_runtimes.is_empty());
        assert_eq!(app.state.plugin_commands_in_flight, 0);
    }

    #[test]
    fn plugin_runtime_env_does_not_expose_build_manager_path() {
        let mut command = std::process::Command::new("true");
        command.env("HERDR_BUILD_BIN_PATH", "inherited-manager");

        apply_plugin_runtime_env(
            &mut command,
            vec![("HERDR_BUILD_BIN_PATH".into(), "spoofed-manager".into())],
        );

        let value = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new("HERDR_BUILD_BIN_PATH"))
            .map(|(_, value)| value);
        assert_eq!(value, Some(None));
    }
}
