use crate::api::ApiRequestContext;
use crate::app::App;
use crate::input_origin::InputOrigin;

/// The Pi processes Herdr can identify in a pane's foreground job. Used only to accept a claim:
/// identification is best effort, so absence from this list proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForegroundPi {
    pub pi_processes: Vec<crate::input_origin::InputOriginClaim>,
}

/// Whether the process that claimed to read origin frames still reads the pane, checked on
/// that process directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimantStatus {
    /// The same process generation runs in the pane's foreground process group.
    Reading,
    /// It exited, its pid now names a later process, or it left the foreground.
    NotReading,
    /// Herdr cannot tell.
    Unknown,
}

/// The API write for this pane cannot be sent: a Pi claimed to read origin frames, but Herdr
/// cannot see now whether that Pi still reads the pane. Sending raw bytes would record API
/// input as typed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputOriginUnavailable;

impl InputOriginUnavailable {
    pub(super) fn encode(self, id: String) -> String {
        super::responses::encode_error(
            id,
            "input_origin_unavailable",
            "cannot verify which process reads this pane; input was not sent",
        )
    }
}

/// Classifies a claimant from what the platform says about its pid and about the pane's
/// foreground process group. Nothing here depends on recognising the process as Pi.
pub(crate) fn classify_claimant(
    claim: crate::input_origin::InputOriginClaim,
    process: crate::platform::ProcessStart,
    pane_foreground_group: Option<u32>,
) -> ClaimantStatus {
    match process {
        crate::platform::ProcessStart::Gone => ClaimantStatus::NotReading,
        crate::platform::ProcessStart::Unknown => ClaimantStatus::Unknown,
        crate::platform::ProcessStart::Running { start_time, .. }
            if start_time != claim.start_time =>
        {
            // The pid names a later process: the claimant exited.
            ClaimantStatus::NotReading
        }
        crate::platform::ProcessStart::Running {
            process_group: Some(group),
            ..
        } => match pane_foreground_group {
            Some(foreground) if foreground == group => ClaimantStatus::Reading,
            Some(_) => ClaimantStatus::NotReading,
            None => ClaimantStatus::Unknown,
        },
        // No process groups on this platform: a running claimant in the pane is its reader.
        crate::platform::ProcessStart::Running {
            process_group: None,
            ..
        } => ClaimantStatus::Reading,
    }
}

/// The Pi processes Herdr can identify in the pane's foreground job, each identified on its
/// own, so a wrapper or sibling process in the same job is not taken for Pi. Tests can set the
/// job through `test_support::set_foreground_pi`.
fn foreground_pi(
    terminal_id: &crate::terminal::TerminalId,
    runtime: &crate::terminal::TerminalRuntime,
) -> ForegroundPi {
    #[cfg(test)]
    if let Some(job) = test_support::foreground_pi(terminal_id) {
        return job;
    }
    #[cfg(not(test))]
    let _ = terminal_id;
    let Some(job) = runtime.child_pid().and_then(crate::detect::foreground_job) else {
        return ForegroundPi {
            pi_processes: Vec::new(),
        };
    };
    let pi_processes = job
        .processes
        .iter()
        .filter(|process| {
            let alone = crate::platform::ForegroundJob {
                process_group_id: process.pid,
                processes: vec![(*process).clone()],
            };
            crate::detect::identify_agent_in_job(&alone)
                .is_some_and(|(agent, _)| agent == crate::detect::Agent::Pi)
        })
        .filter_map(
            |process| match crate::platform::process_start(process.pid) {
                crate::platform::ProcessStart::Running { start_time, .. } => {
                    Some(crate::input_origin::InputOriginClaim {
                        pid: process.pid,
                        start_time,
                    })
                }
                _ => None,
            },
        )
        .collect();
    ForegroundPi { pi_processes }
}

/// Whether `claim`'s process still reads the pane. Tests can set it through
/// `test_support::set_claimant_status`.
fn claimant_status(
    terminal_id: &crate::terminal::TerminalId,
    runtime: &crate::terminal::TerminalRuntime,
    claim: crate::input_origin::InputOriginClaim,
) -> ClaimantStatus {
    #[cfg(test)]
    if let Some(status) = test_support::claimant_status(terminal_id, claim) {
        return status;
    }
    #[cfg(not(test))]
    let _ = terminal_id;
    let Some(child_pid) = runtime.child_pid() else {
        // No process runs in the pane.
        return ClaimantStatus::NotReading;
    };
    classify_claimant(
        claim,
        crate::platform::process_start(claim.pid),
        crate::platform::foreground_process_group_id(child_pid),
    )
}

impl App {
    /// The origin frame for an API write to this pane: `Ok(None)` to write raw bytes, or an
    /// error when the write must not be sent.
    ///
    /// A pane gets frames while the Pi process that claimed to read them (same pid and start
    /// time) runs in its foreground process group. It gets raw bytes when it has no claim, or
    /// when Herdr sees that the claimant exited or left the foreground. If Herdr cannot tell,
    /// a claimed pane gets nothing: raw bytes would be recorded as typed input.
    pub(super) fn api_input_origin(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        runtime: &crate::terminal::TerminalRuntime,
        context: ApiRequestContext,
    ) -> Result<Option<InputOrigin>, InputOriginUnavailable> {
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
        else {
            return Ok(None);
        };
        let Some(claim) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.input_origin_claim())
        else {
            return Ok(None);
        };
        match claimant_status(terminal_id, runtime, claim) {
            ClaimantStatus::Reading => Ok(Some(self.api_caller_origin(context))),
            ClaimantStatus::NotReading => Ok(None),
            ClaimantStatus::Unknown => Err(InputOriginUnavailable),
        }
    }

    /// Records a Pi's claim to read origin frames, only when the socket peer that sent it is
    /// itself a Pi process in the pane's foreground job. A wrapper or sibling in that job,
    /// another pane's process, a process outside every pane, or an unattributed caller cannot
    /// change it. No other process can replace a claim unless its claimant is known not to read
    /// the pane any more: the first Pi to claim is the one that reads the terminal (only the TUI
    /// sets the claim, and any other Pi in its job was started later). A report without the
    /// claim changes nothing.
    pub(super) fn record_input_origin_claim(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        context: ApiRequestContext,
    ) {
        let Some(peer_pid) = context.local_peer_pid else {
            return;
        };
        let Some(terminal_id) = self.state.terminal_id_for_pane(ws_idx, pane_id) else {
            return;
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return;
        };
        let Some(claim) = foreground_pi(&terminal_id, runtime)
            .pi_processes
            .into_iter()
            .find(|process| process.pid == peer_pid)
        else {
            return;
        };
        let current = self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| terminal.input_origin_claim());
        if let Some(current) = current {
            if current != claim
                && claimant_status(&terminal_id, runtime, current) != ClaimantStatus::NotReading
            {
                return;
            }
        }
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_input_origin_claim(claim);
        }
    }

    /// Names the API caller from socket attribution only, never from request text.
    fn api_caller_origin(&self, context: ApiRequestContext) -> InputOrigin {
        let caller = context
            .local_peer_pid
            .and_then(|pid| self.pane_target_for_peer_pid(pid));
        let pane = caller
            .as_ref()
            .and_then(|target| self.public_pane_id(target.ws_idx, target.pane_id));
        let agent = caller
            .as_ref()
            .and_then(|target| self.agent_info(target.ws_idx, target.pane_id));
        let session = agent
            .as_ref()
            .and_then(|agent| agent.agent_session.as_ref())
            .map(|session| session.value.clone());
        let sender = agent
            .and_then(|agent| agent.name)
            .or_else(|| pane.clone())
            .or_else(|| context.local_peer_pid.map(|pid| format!("pid:{pid}")))
            .unwrap_or_else(|| "unknown".to_string());
        InputOrigin::new(sender, pane, session)
    }
}

/// Writes API input, framed when the pane takes origin frames.
pub(super) fn send_api_bytes(
    runtime: &crate::terminal::TerminalRuntime,
    origin: Option<&InputOrigin>,
    input: Vec<u8>,
) -> Result<(), tokio::sync::mpsc::error::TrySendError<bytes::Bytes>> {
    match origin {
        Some(origin) => runtime.try_send_framed(origin, &input),
        None => runtime.try_send_bytes(bytes::Bytes::from(input)),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{ClaimantStatus, ForegroundPi};
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static JOBS: RefCell<HashMap<String, ForegroundPi>> = RefCell::new(HashMap::new());
        static CLAIMANTS: RefCell<HashMap<String, ClaimantStatus>> = RefCell::new(HashMap::new());
    }

    /// Sets the Pi processes a test identifies in `terminal_id`'s foreground job. The claimant
    /// then reads the pane while it is in that list; `None` uses the live lookups.
    pub(crate) fn set_foreground_pi(
        terminal_id: &crate::terminal::TerminalId,
        job: Option<ForegroundPi>,
    ) {
        JOBS.with(|jobs| {
            let mut jobs = jobs.borrow_mut();
            match job {
                Some(job) => jobs.insert(terminal_id.to_string(), job),
                None => jobs.remove(&terminal_id.to_string()),
            };
        });
    }

    /// Overrides the claimant check for `terminal_id`.
    pub(crate) fn set_claimant_status(
        terminal_id: &crate::terminal::TerminalId,
        status: ClaimantStatus,
    ) {
        CLAIMANTS.with(|claimants| {
            claimants
                .borrow_mut()
                .insert(terminal_id.to_string(), status)
        });
    }

    pub(super) fn foreground_pi(terminal_id: &crate::terminal::TerminalId) -> Option<ForegroundPi> {
        JOBS.with(|jobs| jobs.borrow().get(&terminal_id.to_string()).cloned())
    }

    pub(super) fn claimant_status(
        terminal_id: &crate::terminal::TerminalId,
        claim: crate::input_origin::InputOriginClaim,
    ) -> Option<ClaimantStatus> {
        CLAIMANTS
            .with(|claimants| claimants.borrow().get(&terminal_id.to_string()).copied())
            .or_else(|| {
                // Without an override, the claimant reads the pane while the test lists it.
                let job = foreground_pi(terminal_id)?;
                Some(if job.pi_processes.contains(&claim) {
                    ClaimantStatus::Reading
                } else {
                    ClaimantStatus::NotReading
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_claimant, ClaimantStatus};
    use crate::input_origin::InputOriginClaim;
    use crate::platform::ProcessStart;

    const CLAIM: InputOriginClaim = InputOriginClaim {
        pid: 7001,
        start_time: 100,
    };

    fn running(start_time: u64, process_group: Option<u32>) -> ProcessStart {
        ProcessStart::Running {
            start_time,
            process_group,
        }
    }

    #[test]
    fn a_claimant_is_checked_on_its_own_pid_not_by_name() {
        // A Node Pi whose argv cannot be read is still the same process in the foreground.
        assert_eq!(
            classify_claimant(CLAIM, running(100, Some(7000)), Some(7000)),
            ClaimantStatus::Reading
        );
        // Platforms without process groups.
        assert_eq!(
            classify_claimant(CLAIM, running(100, None), None),
            ClaimantStatus::Reading
        );
    }

    #[test]
    fn a_claimant_that_cannot_be_read_is_unknown_not_absent() {
        // An unreadable claimant next to a readable sibling, or an unreadable foreground group.
        assert_eq!(
            classify_claimant(CLAIM, ProcessStart::Unknown, Some(7000)),
            ClaimantStatus::Unknown
        );
        assert_eq!(
            classify_claimant(CLAIM, running(100, Some(7000)), None),
            ClaimantStatus::Unknown
        );
    }

    #[test]
    fn a_claimant_is_gone_only_on_positive_evidence() {
        assert_eq!(
            classify_claimant(CLAIM, ProcessStart::Gone, Some(7000)),
            ClaimantStatus::NotReading
        );
        // The pid now names a later process.
        assert_eq!(
            classify_claimant(CLAIM, running(101, Some(7000)), Some(7000)),
            ClaimantStatus::NotReading
        );
        // It runs but another group owns the terminal (for example, it was suspended).
        assert_eq!(
            classify_claimant(CLAIM, running(100, Some(7000)), Some(9000)),
            ClaimantStatus::NotReading
        );
    }
}
