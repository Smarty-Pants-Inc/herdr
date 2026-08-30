use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{terminal_targets::TerminalTargetError, App};
use crate::api::schema::AgentStartParams;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        let Ok(resolved) = self.resolve_agent_target(target) else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return;
        };
        let changed = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .is_some_and(|terminal| terminal.reconcile_managed_agent_at(Instant::now(), false));
        if changed {
            self.sync_managed_agent_detection_hint(resolved.ws_idx, resolved.pane_id);
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        }
    }

    pub(super) fn sync_managed_agent_detection_hint(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
        else {
            return;
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return;
        };
        let hint = matches!(
            &terminal.execution_target,
            crate::execution::ExecutionTarget::Extension { .. }
        )
        .then(|| terminal.managed_agent_kind())
        .flatten();
        if let Some(runtime) = self.terminal_runtimes.get(terminal_id) {
            runtime.set_managed_agent_hint(hint);
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mark_active_tab_seen();
        self.state.settle_terminal_mode_after_focus();
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let name = params.name;
        if !valid_agent_name(&name) {
            return Err(AgentStartError::InvalidName);
        }
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentStartError::InvalidArgument);
        }
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if matches!(
            &terminal.execution_target,
            crate::execution::ExecutionTarget::Ssh { .. }
        ) {
            return Err(AgentStartError::UnsupportedExecutionTarget(params.pane_id));
        }
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let execution_target = terminal.execution_target.clone();
        let cwd = terminal.cwd.clone();
        let local_execution = execution_target.is_local();
        let provider_execution = matches!(
            &execution_target,
            crate::execution::ExecutionTarget::Extension { .. }
        );
        let provider_shell = terminal.launch_argv.is_none();

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args);
        let argv = resolve_agent_argv(crate::detect::agent_label(kind), argv, local_execution)
            .map_err(AgentStartError::OmpUnavailable)?;
        let (rows, cols, submission) = {
            let runtime = self
                .terminal_runtimes
                .get(&terminal_id)
                .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
            let (rows, cols) = runtime.current_size();
            let submission = if provider_execution {
                #[cfg(unix)]
                let remote_ready = runtime.remote_execution_ready();
                #[cfg(not(unix))]
                let remote_ready = true;
                if !provider_shell || !remote_ready || runtime.input_written() {
                    return Err(AgentStartError::TargetBusy(params.pane_id.clone()));
                }
                None
            } else {
                let shell_name = available_shell_name(runtime)
                    .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;
                let command = crate::platform::interactive_shell_command(&argv, &shell_name)
                    .ok_or(AgentStartError::InvalidArgument)?;
                Some(Bytes::from(crate::app::api_helpers::encode_api_submission(
                    runtime, &command,
                )))
            };
            (rows, cols, submission)
        };
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }

        let provider_runtime = if provider_execution {
            let launch_env = self
                .pane_launch_env(ws_idx, pane_id, Vec::new())
                .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
            Some(
                crate::terminal::TerminalRuntime::spawn_argv_command_on(
                    pane_id,
                    rows,
                    cols,
                    cwd,
                    &execution_target,
                    &argv,
                    &launch_env,
                    crate::pane::AgentDetection::Enabled,
                    self.state.pane_scrollback_limit_bytes,
                    self.state.host_terminal_theme,
                    self.state.host_terminal_appearance,
                    self.event_tx.clone(),
                    self.render_notify.clone(),
                    self.render_dirty.clone(),
                )
                .map_err(|error| AgentStartError::LaunchFailed(error.to_string()))?,
            )
        } else {
            None
        };

        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        if provider_execution {
            terminal.clear_agent_runtime_identity_after_respawn(false);
            terminal.launch_argv = Some(argv.clone());
            terminal.respawn_shell_on_exit = true;
        }
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        if let Some(runtime) = provider_runtime {
            runtime.set_managed_agent_hint(Some(kind));
            if let Some(previous) = self.terminal_runtimes.insert(terminal_id.clone(), runtime) {
                previous.shutdown();
            }
        } else {
            let runtime = self
                .terminal_runtimes
                .get(&terminal_id)
                .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
            if let Err(err) = runtime.try_send_bytes(submission.expect("submission is prepared")) {
                runtime.set_managed_agent_hint(None);
                terminal.clear_agent_name();
                return Err(AgentStartError::InputFailed(err.to_string()));
            }
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or(AgentStartError::TargetUnavailable(params.pane_id))?;
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentStartError::UnsupportedKind(kind) => crate::api::schema::ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {kind}"),
            },
            AgentStartError::InvalidArgument => crate::api::schema::ErrorBody {
                code: "invalid_agent_argument".into(),
                message: "agent arguments cannot be encoded safely for the target shell".into(),
            },
            AgentStartError::InvalidTimeout => crate::api::schema::ErrorBody {
                code: "invalid_agent_timeout".into(),
                message: INVALID_AGENT_TIMEOUT_MESSAGE.into(),
            },
            AgentStartError::OmpUnavailable(message) => crate::api::schema::ErrorBody {
                code: "agent_omp_unavailable".into(),
                message: format!("failed to resolve the paired OMP executable: {message}"),
            },
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
            AgentStartError::UnsupportedExecutionTarget(target) => {
                crate::api::schema::ErrorBody {
                    code: "agent_start_unsupported_execution_target".into(),
                    message: format!(
                        "agent start does not support built-in SSH panes; {target} is SSH-backed"
                    ),
                }
            }
            AgentStartError::TargetBusy(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_busy".into(),
                message: format!("agent target pane {target} is not an available shell"),
            },
            AgentStartError::TargetUnavailable(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_unavailable".into(),
                message: format!("agent target pane {target} has no live terminal"),
            },
            AgentStartError::InputFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_input_failed".into(),
                message,
            },
            AgentStartError::LaunchFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_launch_failed".into(),
                message,
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentRenameError::NotAgent => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: "agent target does not currently host an agent".into(),
            },
            AgentRenameError::PendingLaunch => crate::api::schema::ErrorBody {
                code: "agent_launch_pending".into(),
                message: "agent name cannot change while startup is pending".into(),
            },
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

fn available_shell_name(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    crate::platform::available_pane_shell(runtime.child_pid()?)
}
/// Resolve a managed agent executable when Herdr starts it in a local pane.
/// Persisted resume plans keep the logical `omp` command and resolve the
/// release-local companion only at execution time.
pub(crate) fn resolve_agent_argv(
    agent: &str,
    argv: Vec<String>,
    local: bool,
) -> Result<Vec<String>, String> {
    resolve_agent_argv_with(
        agent,
        argv,
        local,
        crate::update::server_private_omp_executable,
    )
}

fn resolve_agent_argv_with(
    agent: &str,
    mut argv: Vec<String>,
    local: bool,
    resolve: impl FnOnce() -> Result<crate::update::OmpExecutable, String>,
) -> Result<Vec<String>, String> {
    if !local || agent != "omp" || argv.is_empty() {
        return Ok(argv);
    }

    let executable = resolve()?;
    executable.verify()?;
    argv[0] = executable.executable().to_string_lossy().into_owned();
    Ok(argv)
}

pub(super) fn runtime_hosts_agent(
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(runtime: &crate::terminal::TerminalRuntime) -> Option<crate::detect::Agent> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    crate::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| crate::platform::process_agent_hint(process.pid))
        })
}

pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidTimeout,
    OmpUnavailable(String),
    TargetNotFound(String),
    UnsupportedExecutionTarget(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    LaunchFailed(String),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use super::{resolve_agent_argv_with, valid_agent_name};

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }
    #[test]
    fn local_omp_launch_uses_exact_companion_without_mutating_plan() {
        let argv = vec!["omp".into(), "--resume=session".into()];
        let expected = std::env::current_exe().unwrap();
        let resolved = resolve_agent_argv_with("omp", argv.clone(), true, || {
            Ok(crate::update::OmpExecutable::Explicit(expected.clone()))
        })
        .unwrap();

        assert_eq!(resolved[0], expected.to_string_lossy());
        assert_eq!(resolved[1], "--resume=session");
        assert_eq!(argv[0], "omp");
    }

    #[test]
    fn local_omp_launch_fails_closed_when_companion_resolution_fails() {
        let err = resolve_agent_argv_with("omp", vec!["omp".into()], true, || {
            Err("paired companion is unavailable".into())
        })
        .unwrap_err();
        assert_eq!(err, "paired companion is unavailable");
    }

    #[test]
    fn remote_and_non_omp_launches_do_not_resolve_a_companion() {
        let argv = vec!["omp".into(), "--resume=session".into()];
        let remote = resolve_agent_argv_with("omp", argv.clone(), false, || {
            panic!("remote launches must not resolve a local companion")
        })
        .unwrap();
        assert_eq!(remote, argv);

        let other = resolve_agent_argv_with("codex", argv.clone(), true, || {
            panic!("non-OMP launches must not resolve a companion")
        })
        .unwrap();
        assert_eq!(other, argv);
    }
}
