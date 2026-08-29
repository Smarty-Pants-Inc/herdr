#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffAgentState {
    agent: Option<String>,
    state: crate::api::schema::PaneAgentState,
    seen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hook: Option<HandoffHookAuthority>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoffHookAuthority {
    source: String,
    agent_label: String,
    message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_ref: Option<HandoffAgentSessionRef>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoffAgentSessionRef {
    kind: crate::agent_resume::AgentSessionRefKind,
    value: String,
}

#[cfg(unix)]
impl HandoffAgentState {
    pub fn capture(terminal: &crate::terminal::TerminalState, seen: bool) -> Self {
        Self {
            agent: terminal.effective_agent_label().map(str::to_string),
            state: api_agent_state(terminal.state),
            seen,
            hook: terminal
                .hook_authority
                .as_ref()
                .map(|hook| HandoffHookAuthority {
                    source: hook.source.clone(),
                    agent_label: hook.agent_label.clone(),
                    message: hook.message.clone(),
                    session_ref: hook.session_ref.as_ref().map(|session_ref| {
                        HandoffAgentSessionRef {
                            kind: session_ref.kind,
                            value: session_ref.value.clone(),
                        }
                    }),
                }),
        }
    }

    pub fn detection_seed(
        &self,
    ) -> (
        Option<crate::detect::Agent>,
        crate::detect::AgentState,
        bool,
    ) {
        let agent = self
            .agent
            .as_deref()
            .and_then(crate::detect::parse_agent_label);
        let state = agent
            .map(|_| detect_agent_state(self.state))
            .unwrap_or(crate::detect::AgentState::Unknown);
        (agent, state, self.hook.is_some())
    }

    pub fn restore(&self, terminal: &mut crate::terminal::TerminalState) -> bool {
        let state = detect_agent_state(self.state);
        let now = std::time::Instant::now();
        if let Some(agent) = self
            .agent
            .as_deref()
            .and_then(crate::detect::parse_agent_label)
        {
            let _ = terminal.set_detected_state_with_screen_signals_at(
                Some(agent),
                state,
                false,
                false,
                false,
                false,
                now,
            );
        }
        if let Some(hook) = &self.hook {
            let session_ref =
                hook.session_ref
                    .as_ref()
                    .map(|session_ref| crate::agent_resume::AgentSessionRef {
                        kind: session_ref.kind,
                        value: session_ref.value.clone(),
                    });
            let _ = terminal.set_hook_authority_at(
                hook.source.clone(),
                hook.agent_label.clone(),
                state,
                hook.message.clone(),
                session_ref,
                None,
                now,
            );
        }
        self.seen
    }
}

#[cfg(unix)]
fn api_agent_state(state: crate::detect::AgentState) -> crate::api::schema::PaneAgentState {
    match state {
        crate::detect::AgentState::Idle => crate::api::schema::PaneAgentState::Idle,
        crate::detect::AgentState::Working => crate::api::schema::PaneAgentState::Working,
        crate::detect::AgentState::Blocked => crate::api::schema::PaneAgentState::Blocked,
        crate::detect::AgentState::Unknown => crate::api::schema::PaneAgentState::Unknown,
    }
}

#[cfg(unix)]
fn detect_agent_state(state: crate::api::schema::PaneAgentState) -> crate::detect::AgentState {
    match state {
        crate::api::schema::PaneAgentState::Idle => crate::detect::AgentState::Idle,
        crate::api::schema::PaneAgentState::Working => crate::detect::AgentState::Working,
        crate::api::schema::PaneAgentState::Blocked => crate::detect::AgentState::Blocked,
        crate::api::schema::PaneAgentState::Unknown => crate::detect::AgentState::Unknown,
    }
}

/// Long-lived pane runtime transferred during server replacement.
///
/// Handoff preserves server-owned session state such as PTYs, processes, agent
/// identity, and durable plugin/session metadata. It intentionally does not
/// preserve transient coordination such as in-flight requests, waits,
/// subscriptions, client sockets, or pane-to-pane messages; clients reconnect
/// and retry those operations after replacement.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRuntimeState {
    pub pane_id: u32,
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub remote_execution_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_hostname: Option<String>,
    #[serde(default)]
    pub remote_exec_ready_filter: crate::pane::RemoteExecReadyFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_agent_resume_plan: Option<crate::agent_resume::AgentResumePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_agent_resume_attempt_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_agent_resume_retired_pids: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respawn_shell_on_exit: Option<bool>,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<crate::pane::InputState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_history_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<HandoffAgentState>,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    pub fn with_pane_id(mut self, pane_id: crate::layout::PaneId) -> Self {
        self.pane_id = pane_id.raw();
        self
    }
}

#[derive(Debug)]
pub(crate) struct ImportedHandoffRuntime {
    #[cfg(unix)]
    pub master_fd: std::os::fd::RawFd,
    #[cfg(unix)]
    pub state: HandoffRuntimeState,
}

#[cfg(all(test, unix))]
mod tests {
    use super::HandoffRuntimeState;

    #[test]
    fn remote_execution_state_serializes_and_defaults_for_older_handoffs() {
        let older = r#"{
            "pane_id": 7,
            "child_pid": 42,
            "rows": 24,
            "cols": 80,
            "cell_width_px": 0,
            "cell_height_px": 0
        }"#;
        let older: HandoffRuntimeState = serde_json::from_str(older).unwrap();
        assert!(!older.remote_execution_ready);
        assert_eq!(older.remote_hostname, None);
        assert!(older.pending_agent_resume_plan.is_none());
        assert!(older.pending_agent_resume_attempt_pid.is_none());
        assert!(older.pending_agent_resume_retired_pids.is_empty());
        assert!(older.respawn_shell_on_exit.is_none());

        let current = serde_json::json!({
            "pane_id": 7,
            "child_pid": 42,
            "rows": 24,
            "cols": 80,
            "cell_width_px": 0,
            "cell_height_px": 0,
            "remote_execution_ready": true,
            "remote_hostname": "actual-node",
            "pending_agent_resume_plan": {
                "agent": "codex",
                "argv": ["codex", "resume", "session-1"],
                "dedupe_key": "codex:id:session-1"
            },
            "pending_agent_resume_attempt_pid": 42,
            "pending_agent_resume_retired_pids": [40, 41],
            "respawn_shell_on_exit": false
        });
        let current: HandoffRuntimeState = serde_json::from_value(current).unwrap();
        let encoded = serde_json::to_value(current).unwrap();
        assert_eq!(encoded["remote_execution_ready"], true);
        assert_eq!(encoded["remote_hostname"], "actual-node");
        assert_eq!(encoded["pending_agent_resume_plan"]["agent"], "codex");
        assert_eq!(encoded["pending_agent_resume_attempt_pid"], 42);
        assert_eq!(
            encoded["pending_agent_resume_retired_pids"],
            serde_json::json!([40, 41])
        );
        assert_eq!(encoded["respawn_shell_on_exit"], false);
    }
}
