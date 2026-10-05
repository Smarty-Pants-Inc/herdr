#[cfg(unix)]
use serde::{Deserialize, Serialize};

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
    /// Original runtime root pin, not a timestamp recaptured during replacement.
    /// Legacy manifests require separate PTY-bound validation before attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_start_time: Option<u64>,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
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
    pub agent_state: Option<crate::terminal::state::HandoffAgentState>,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    /// Validate the transferred instance; never bootstrap a missing legacy pin.
    pub(crate) fn child_process_identity(&self) -> Option<crate::platform::ProcessIdentity> {
        let identity = crate::platform::ProcessIdentity {
            pid: self.child_pid,
            start_time: self.child_start_time?,
        };
        (crate::platform::process_identity(identity.pid) == Some(identity)).then_some(identity)
    }

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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::HandoffRuntimeState;

    fn legacy_state(pid: u32) -> HandoffRuntimeState {
        serde_json::from_value(serde_json::json!({
            "pane_id": 12,
            "child_pid": pid,
            "rows": 24,
            "cols": 80,
            "cell_width_px": 0,
            "cell_height_px": 0
        }))
        .expect("legacy handoff manifest")
    }

    #[test]
    fn handoff_pin_round_trip_preserves_original_instance() {
        let identity = crate::platform::process_identity(std::process::id())
            .expect("current process identity");
        let mut state = legacy_state(identity.pid);
        state.child_start_time = Some(identity.start_time);
        let json = serde_json::to_string(&state).expect("serialize pin");
        let imported: HandoffRuntimeState = serde_json::from_str(&json).expect("import pin");
        assert_eq!(imported.child_start_time, Some(identity.start_time));
        assert_eq!(imported.child_process_identity(), Some(identity));
    }

    #[test]
    fn handoff_missing_or_changed_pin_never_adopts_live_numeric_pid() {
        let identity = crate::platform::process_identity(std::process::id())
            .expect("current process identity");
        let mut state = legacy_state(identity.pid);
        assert_eq!(state.child_start_time, None);
        assert_eq!(state.child_process_identity(), None);
        assert!(!serde_json::to_value(&state)
            .expect("serialize legacy")
            .as_object()
            .expect("manifest object")
            .contains_key("child_start_time"));
        let old_start_time = identity.start_time.wrapping_add(1);
        state.child_start_time = Some(old_start_time);
        // Live counterexample to unsafe recapture: this numeric PID does resolve,
        // but not to the instance supplied by the manifest.
        assert_eq!(
            crate::platform::process_identity(identity.pid),
            Some(identity)
        );
        assert_eq!(state.child_process_identity(), None);
        let imported: HandoffRuntimeState =
            serde_json::from_str(&serde_json::to_string(&state).expect("serialize stale pin"))
                .expect("import stale pin");
        assert_eq!(imported.child_start_time, Some(old_start_time));
        assert_eq!(imported.child_process_identity(), None);
    }

    #[test]
    fn handoff_exited_root_pin_is_rejected() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("handoff root");
        let identity = crate::platform::process_identity(child.id()).expect("root pin");
        let mut state = legacy_state(identity.pid);
        state.child_start_time = Some(identity.start_time);
        assert_eq!(state.child_process_identity(), Some(identity));
        drop(child.stdin.take());
        child.wait().expect("reap root");
        assert_eq!(state.child_process_identity(), None);
        assert_eq!(state.child_start_time, Some(identity.start_time));
    }
}
