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
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
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
        self.state.mode = crate::app::Mode::Terminal;
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
        let pane = self.parse_current_public_pane_id(&params.pane_id);
        let terminal_id =
            pane.and_then(|(ws_idx, pane_id)| self.state.terminal_id_for_pane(ws_idx, pane_id));
        self.check_expected_terminal(params.expected_terminal.as_deref(), terminal_id.as_ref())
            .map_err(AgentStartError::TerminalIdentityMismatch)?;
        // No await or mutable target resolution between the guard, name binding and
        // PTY enqueue: all startup effects use this checked server-owned terminal.
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
        let persisted_agent_session =
            crate::agent_resume::persisted_session_from_launch_args(kind, &params.args);
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let (ws_idx, pane_id) =
            pane.ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal_id =
            terminal_id.ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        let shell_name = available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args);
        let command = crate::platform::interactive_shell_command(&argv, &shell_name)
            .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }

        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            terminal.clear_agent_name();
            return Err(AgentStartError::InputFailed(err.to_string()));
        }
        if let Some(session) = persisted_agent_session {
            terminal.set_managed_agent_launch_session(session);
        }
        self.accepted_api_inputs.push(pane_id);
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
            AgentStartError::TerminalIdentityMismatch(error) => error,
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
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
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
            completion_seq: terminal.last_agent_completion_seq,
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

fn submission_not_ready() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "pinned agent is not a verified foreground runtime: unsupported launcher or missing/changed executable or runtime CLI entry evidence",
    )
}

/// A display/job label is not input authorization. Pin only the member whose
/// own generation-checked OS executable and runtime entry identify the agent.
fn identified_agent_process(
    job: &crate::platform::ForegroundJob,
    expected: crate::detect::Agent,
) -> Option<crate::platform::ProcessIdentity> {
    identified_agent_process_observed(
        job,
        expected,
        crate::platform::process_identity,
        crate::platform::process_executable_basename,
    )
}

fn identified_agent_process_observed(
    job: &crate::platform::ForegroundJob,
    expected: crate::detect::Agent,
    identity_of: impl Fn(u32) -> Option<crate::platform::ProcessIdentity>,
    executable_of: impl Fn(crate::platform::ProcessIdentity) -> Option<String>,
) -> Option<crate::platform::ProcessIdentity> {
    let identifies = |process: &crate::platform::ForegroundProcess| {
        let identity = identity_of(process.pid)?;
        let executable = executable_of(identity)?;
        (crate::detect::identify_guarded_agent_process(process, &executable) == Some(expected)
            && identity_of(process.pid) == Some(identity))
        .then_some(identity)
    };
    // A recognized non-exec launcher never qualifies. If its actual runtime
    // child does, that child's generation (not the surviving leader) is pinned.
    job.processes
        .iter()
        .find(|process| process.pid == job.process_group_id)
        .and_then(identifies)
        .or_else(|| {
            job.processes
                .iter()
                .filter(|process| process.pid != job.process_group_id)
                .find_map(identifies)
        })
}

#[derive(Clone)]
struct AgentSubmissionPin {
    // Evidence belongs to this checked runtime's terminal, never a re-resolved pane.
    terminal_id: crate::terminal::TerminalId,
    shell: crate::platform::ProcessIdentity,
    agent: crate::platform::ProcessIdentity,
    process_group_id: u32,
    expected: crate::detect::Agent,
}

impl AgentSubmissionPin {
    fn check(&self) -> std::io::Result<()> {
        self.check_observed(
            crate::platform::process_identity,
            crate::platform::process_executable_basename,
            || crate::platform::fresh_foreground_job(self.shell.pid),
        )
        .map_err(|err| {
            std::io::Error::new(err.kind(), format!("terminal {}: {err}", self.terminal_id))
        })
    }

    fn check_observed(
        &self,
        identity_of: impl Fn(u32) -> Option<crate::platform::ProcessIdentity>,
        executable_of: impl Fn(crate::platform::ProcessIdentity) -> Option<String>,
        job_now: impl FnOnce() -> Option<crate::platform::ForegroundJob>,
    ) -> std::io::Result<()> {
        let instances_live = || {
            identity_of(self.shell.pid) == Some(self.shell)
                && identity_of(self.agent.pid) == Some(self.agent)
        };
        if !instances_live() {
            return Err(submission_not_ready());
        }
        let job = job_now().ok_or_else(submission_not_ready)?;
        let recognized_member = job
            .processes
            .iter()
            .find(|process| process.pid == self.agent.pid)
            .is_some_and(|member| {
                executable_of(self.agent).is_some_and(|executable| {
                    crate::detect::identify_guarded_agent_process(member, &executable)
                        == Some(self.expected)
                })
            });
        // Recheck both generations after numeric OS reads: an exited or reused
        // endpoint must not lend its evidence to a replacement process.
        if job.process_group_id != self.process_group_id || !recognized_member || !instances_live()
        {
            return Err(submission_not_ready());
        }
        Ok(())
    }
}

pub(super) fn capture_agent_submission_guard(
    terminal_id: &crate::terminal::TerminalId,
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> std::io::Result<crate::pty::actor::SubmissionGuard> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        // Synthetic runtimes have no OS process. Production has no such fallback.
        return Ok(crate::pty::actor::SubmissionGuard::new(|| Ok(())));
    }
    let shell = runtime
        .child_process_identity()
        .ok_or_else(submission_not_ready)?;
    let job = crate::platform::fresh_foreground_job(shell.pid).ok_or_else(submission_not_ready)?;
    let agent = identified_agent_process(&job, expected).ok_or_else(submission_not_ready)?;
    let pin = AgentSubmissionPin {
        terminal_id: terminal_id.clone(),
        shell,
        agent,
        process_group_id: job.process_group_id,
        expected,
    };
    // The second fresh observation binds identification to the captured instance,
    // including replacement during initial metadata collection.
    pin.check()?;
    Ok(crate::pty::actor::SubmissionGuard::new(move || pin.check()))
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
    TerminalIdentityMismatch(crate::api::schema::ErrorBody),
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
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
    use super::*;

    fn submission_pin_fixture() -> (AgentSubmissionPin, crate::platform::ForegroundJob) {
        let pin = AgentSubmissionPin {
            terminal_id: crate::terminal::TerminalId::alloc(),
            shell: crate::platform::ProcessIdentity {
                pid: 10,
                start_time: 1,
            },
            agent: crate::platform::ProcessIdentity {
                pid: 21,
                start_time: 2,
            },
            process_group_id: 20,
            expected: crate::detect::Agent::Pi,
        };
        let job = crate::platform::ForegroundJob {
            process_group_id: 20,
            processes: vec![
                crate::platform::ForegroundProcess {
                    pid: 20,
                    name: "sh".into(),
                    argv0: None,
                    argv: Some(vec!["/bin/sh".into(), "/tmp/test-bin/pi".into()]),
                    cmdline: None,
                },
                crate::platform::ForegroundProcess {
                    pid: 21,
                    name: "pi".into(),
                    argv0: Some("pi".into()),
                    argv: Some(vec![
                        "node".into(),
                        "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    ]),
                    cmdline: None,
                },
            ],
        };
        (pin, job)
    }

    fn fixture_executable(identity: crate::platform::ProcessIdentity) -> Option<String> {
        Some(
            match identity.pid {
                20 => "sh",
                22 => "cat",
                _ => "node",
            }
            .into(),
        )
    }

    #[test]
    fn guarded_submission_pins_recognized_member_not_wrapper_group() {
        let (pin, job) = submission_pin_fixture();
        // Display deliberately recognizes the shell leader; authorization must not.
        assert_eq!(
            crate::detect::identify_agent_in_job(&job).map(|(agent, _)| agent),
            Some(pin.expected)
        );
        assert_eq!(
            identified_agent_process_observed(
                &job,
                pin.expected,
                |pid| Some(crate::platform::ProcessIdentity { pid, start_time: 2 }),
                fixture_executable,
            ),
            Some(pin.agent),
        );
        assert!(pin
            .check_observed(
                |pid| Some(if pid == pin.shell.pid {
                    pin.shell
                } else {
                    pin.agent
                }),
                fixture_executable,
                || Some(job),
            )
            .is_ok());
    }

    #[test]
    fn guarded_submission_capture_rejects_generation_loss_during_executable_read() {
        let (pin, job) = submission_pin_fixture();
        for replacement in [
            None,
            Some(crate::platform::ProcessIdentity {
                start_time: pin.agent.start_time + 1,
                ..pin.agent
            }),
        ] {
            let changed = std::cell::Cell::new(false);
            assert_eq!(
                identified_agent_process_observed(
                    &job,
                    pin.expected,
                    |pid| {
                        if pid == pin.agent.pid && changed.get() {
                            replacement
                        } else {
                            Some(crate::platform::ProcessIdentity { pid, start_time: 2 })
                        }
                    },
                    |identity| {
                        if identity == pin.agent {
                            changed.set(true);
                        }
                        fixture_executable(identity)
                    },
                ),
                None
            );
        }
    }

    #[test]
    fn guarded_submission_refusal_retains_checked_terminal_identity() {
        let (mut pin, _) = submission_pin_fixture();
        pin.shell.pid = 0;
        let error = pin.check().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(pin.terminal_id.as_str()));
    }

    #[test]
    fn guarded_submission_rejects_unknown_reused_and_lost_processes() {
        let (pin, job) = submission_pin_fixture();
        for endpoint in [pin.shell, pin.agent] {
            for replacement in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: endpoint.start_time + 1,
                    ..endpoint
                }),
            ] {
                let error = pin
                    .check_observed(
                        |pid| {
                            if pid == endpoint.pid {
                                replacement
                            } else {
                                Some(if pid == pin.shell.pid {
                                    pin.shell
                                } else {
                                    pin.agent
                                })
                            }
                        },
                        fixture_executable,
                        || Some(job.clone()),
                    )
                    .unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            }
        }
        let live = |pid| {
            Some(if pid == pin.shell.pid {
                pin.shell
            } else {
                pin.agent
            })
        };
        for observation in [
            None,
            Some(crate::platform::ForegroundJob {
                process_group_id: pin.shell.pid,
                ..job.clone()
            }),
            Some(crate::platform::ForegroundJob {
                processes: Vec::new(),
                ..job.clone()
            }),
        ] {
            assert_eq!(
                pin.check_observed(live, fixture_executable, || observation)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        let mut launcher_only = job.clone();
        launcher_only
            .processes
            .retain(|member| member.pid != pin.agent.pid);
        launcher_only
            .processes
            .push(crate::platform::ForegroundProcess {
                pid: 22,
                name: "pi".into(),
                argv0: Some("pi".into()),
                argv: Some(vec!["pi".into()]),
                cmdline: Some("pi".into()),
            });
        // The recognized launcher, rebranded native follow-on reader and same
        // foreground group survive child loss. None may inherit the child's pin.
        assert_eq!(
            crate::detect::identify_agent_in_job(&launcher_only).map(|(agent, _)| agent),
            Some(pin.expected)
        );
        assert!(pin
            .check_observed(live, fixture_executable, || Some(launcher_only.clone()))
            .is_err());
        assert_eq!(
            identified_agent_process_observed(
                &launcher_only,
                pin.expected,
                |pid| Some(crate::platform::ProcessIdentity { pid, start_time: 2 }),
                fixture_executable,
            ),
            None
        );
    }

    #[test]
    fn guarded_submission_rechecks_own_executable_and_entry_not_mutable_titles() {
        let (pin, mut job) = submission_pin_fixture();
        let live = |pid| {
            Some(if pid == pin.shell.pid {
                pin.shell
            } else {
                pin.agent
            })
        };
        assert!(pin
            .check_observed(live, fixture_executable, || Some(job.clone()))
            .is_ok());
        // Exec keeps the start time. Neither surviving argv/title nor the same
        // PID/group may lend identity to a shell or rebranded generic binary.
        for executable in [None, Some("sh"), Some("python3"), Some("cat"), Some("pi")] {
            assert!(pin
                .check_observed(
                    live,
                    |_| executable.map(str::to_owned),
                    || Some(job.clone())
                )
                .is_err());
        }
        for argv in [
            None,
            Some(vec!["pi".into()]),
            Some(vec!["node".into(), "/tmp/pi.js".into()]),
            Some(vec!["/bin/sh".into()]),
        ] {
            job.processes[1].argv = argv;
            assert!(pin
                .check_observed(live, fixture_executable, || Some(job.clone()))
                .is_err());
        }
    }

    #[test]
    fn guarded_submission_revalidates_instances_after_foreground_capture() {
        let (pin, job) = submission_pin_fixture();
        for endpoint in [pin.shell, pin.agent] {
            let replaced = std::cell::Cell::new(false);
            assert_eq!(
                pin.check_observed(
                    |pid| {
                        let mut identity = if pid == pin.shell.pid {
                            pin.shell
                        } else {
                            pin.agent
                        };
                        if pid == endpoint.pid && replaced.get() {
                            identity.start_time += 1;
                        }
                        Some(identity)
                    },
                    fixture_executable,
                    || {
                        replaced.set(true);
                        Some(job.clone())
                    },
                )
                .unwrap_err()
                .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[tokio::test]
    async fn guarded_submission_synthetic_runtime_is_explicitly_test_only() {
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let terminal_id = crate::terminal::TerminalId::alloc();
        let guard =
            capture_agent_submission_guard(&terminal_id, &runtime, crate::detect::Agent::Pi)
                .expect("synthetic test guard");
        assert!(guard.check().is_ok());
        runtime.test_set_child_pid(u32::MAX);
        assert!(
            capture_agent_submission_guard(&terminal_id, &runtime, crate::detect::Agent::Pi)
                .is_err()
        );
    }

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
}
