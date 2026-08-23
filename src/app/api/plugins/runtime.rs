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
            env.extend(super::env::plugin_path_env(plugin));
            env.push((
                crate::api::SOCKET_PATH_ENV_VAR.to_string(),
                crate::api::socket_path().display().to_string(),
            ));
            if let Ok(current_exe) = std::env::current_exe() {
                env.push((
                    "HERDR_BIN_PATH".to_string(),
                    current_exe.display().to_string(),
                ));
            }
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
        std::thread::spawn(move || {
            let (child, remote_request_channel) = if let Some((program, args)) = local_command {
                let mut command =
                    crate::plugin_command::command_for_argv_in_dir(&program, &args, &plugin_root);
                command.env_remove("HERDR_VIEW_ID");
                apply_plugin_runtime_env(&mut command, env);
                (
                    command
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn(),
                    None,
                )
            } else {
                let action_id = action_id_for_launch.expect("remote plugin action requires id");
                match crate::execution::ssh_process_command(
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
                    Ok(mut prepared) => (
                        prepared
                            .command
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .spawn(),
                        Some(prepared.request_channel),
                    ),
                    Err(err) => (Err(err), None),
                }
            };
            let finished = match child {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let stdout_reader = stdout.map(|stdout| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stdout, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    let stderr_reader = stderr.map(|stderr| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stderr, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    let status = {
                        #[cfg(unix)]
                        {
                            if let Some(channel) = remote_request_channel {
                                loop {
                                    match channel.delivery_status() {
                                        crate::execution::RemoteRequestDelivery::Delivered => {
                                            break child.wait()
                                        }
                                        crate::execution::RemoteRequestDelivery::Failed(err) => {
                                            let _ = child.kill();
                                            let _ = child.wait();
                                            break Err(std::io::Error::other(err));
                                        }
                                        crate::execution::RemoteRequestDelivery::Pending => {}
                                    }
                                    match child.try_wait() {
                                        Ok(Some(status)) => break Ok(status),
                                        Ok(None) => {
                                            std::thread::sleep(std::time::Duration::from_millis(25))
                                        }
                                        Err(err) => break Err(err),
                                    }
                                }
                            } else {
                                child.wait()
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = remote_request_channel;
                            child.wait()
                        }
                    };
                    match status {
                        Ok(status) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: status.code(),
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            error: None,
                        },
                        Err(err) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: None,
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
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
            let _ = event_tx.blocking_send(finished);
        });
        Ok(log)
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

pub(super) fn read_capped_plugin_output(mut reader: impl Read, cap: usize) -> String {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
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
