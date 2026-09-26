use crate::api::ApiRequestContext;
use crate::app::App;
use crate::input_origin::InputOrigin;

/// What Herdr can see of the Pi processes in a pane's foreground job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForegroundPi {
    /// The job was read. These are its Pi processes with their start times (maybe none).
    Known(Vec<crate::input_origin::InputOriginClaim>),
    /// The job, or a Pi process's start time, could not be read.
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

/// The Pi processes in the pane's foreground job, each identified on its own, so a wrapper or
/// sibling process in the same job is not taken for Pi. Tests can set the job through
/// `test_support::set_foreground_pi`.
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
    let Some(child_pid) = runtime.child_pid() else {
        // No process runs in the pane.
        return ForegroundPi::Known(Vec::new());
    };
    let Some(job) = crate::detect::foreground_job(child_pid) else {
        return ForegroundPi::Unknown;
    };
    let mut pi_processes = Vec::new();
    for process in &job.processes {
        let alone = crate::platform::ForegroundJob {
            process_group_id: process.pid,
            processes: vec![process.clone()],
        };
        let is_pi = crate::detect::identify_agent_in_job(&alone)
            .is_some_and(|(agent, _)| agent == crate::detect::Agent::Pi);
        if !is_pi {
            continue;
        }
        let Some(start_time) = crate::platform::process_start_time(process.pid) else {
            return ForegroundPi::Unknown;
        };
        pi_processes.push(crate::input_origin::InputOriginClaim {
            pid: process.pid,
            start_time,
        });
    }
    ForegroundPi::Known(pi_processes)
}

impl App {
    /// The origin frame for an API write to this pane: `Ok(None)` to write raw bytes, or an
    /// error when the write must not be sent.
    ///
    /// A pane gets frames while the Pi process that claimed to read them (same pid and start
    /// time) is a Pi in its foreground job. It gets raw bytes when it has no claim, or when Herdr
    /// can see that the claimant is gone. If Herdr cannot see the job, a claimed pane gets
    /// nothing: raw bytes would be recorded as typed input.
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
        match foreground_pi(terminal_id, runtime) {
            ForegroundPi::Unknown => Err(InputOriginUnavailable),
            ForegroundPi::Known(pis) if pis.contains(&claim) => {
                Ok(Some(self.api_caller_origin(context)))
            }
            ForegroundPi::Known(_) => Ok(None),
        }
    }

    /// Records a Pi's claim to read origin frames, only when the socket peer that sent it is
    /// itself a Pi process in the pane's foreground job. A wrapper or sibling in that job,
    /// another pane's process, a process outside every pane, or an unattributed caller cannot
    /// change it. While the current claimant is still a Pi in the job, no other process can
    /// replace its claim: the first Pi to claim is the one that reads the terminal (only the
    /// TUI sets the claim, and any other Pi in its job was started later). A report without the
    /// claim changes nothing: frames stop when the claimant leaves the foreground or exits.
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
        let ForegroundPi::Known(pis) = foreground_pi(&terminal_id, runtime) else {
            return;
        };
        let Some(claim) = pis.iter().copied().find(|process| process.pid == peer_pid) else {
            return;
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return;
        };
        if let Some(current) = terminal.input_origin_claim() {
            if current != claim && pis.contains(&current) {
                return;
            }
        }
        terminal.set_input_origin_claim(claim);
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
    use super::ForegroundPi;
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static JOBS: RefCell<HashMap<String, ForegroundPi>> = RefCell::new(HashMap::new());
    }

    /// Sets what a test sees of `terminal_id`'s foreground job; `None` uses the live lookup.
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

    pub(super) fn foreground_pi(terminal_id: &crate::terminal::TerminalId) -> Option<ForegroundPi> {
        JOBS.with(|jobs| jobs.borrow().get(&terminal_id.to_string()).cloned())
    }
}
