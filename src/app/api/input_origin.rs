use crate::api::ApiRequestContext;
use crate::app::App;
use crate::input_origin::InputOrigin;

/// The Pi job in a pane's foreground: its process group and the processes in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForegroundPi {
    pub process_group: u32,
    pub pids: Vec<u32>,
}

/// The pane's foreground job, when it hosts Pi.
#[cfg(not(test))]
fn foreground_pi(
    _terminal_id: &crate::terminal::TerminalId,
    runtime: &crate::terminal::TerminalRuntime,
) -> Option<ForegroundPi> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    let (agent, _) = crate::detect::identify_agent_in_job(&job)?;
    (agent == crate::detect::Agent::Pi).then(|| ForegroundPi {
        process_group: job.process_group_id,
        pids: job.processes.iter().map(|process| process.pid).collect(),
    })
}

/// Tests set the foreground job through [`test_support::set_foreground_pi`].
#[cfg(test)]
fn foreground_pi(
    terminal_id: &crate::terminal::TerminalId,
    _runtime: &crate::terminal::TerminalRuntime,
) -> Option<ForegroundPi> {
    test_support::foreground_pi(terminal_id)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::ForegroundPi;
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static JOBS: RefCell<HashMap<String, ForegroundPi>> = RefCell::new(HashMap::new());
    }

    /// Makes `terminal_id`'s foreground job a Pi job in tests.
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

impl App {
    /// The origin frame for an API write to this pane, or `None` to write raw bytes.
    ///
    /// A pane gets frames only while its foreground job is the Pi job that claimed, from inside
    /// that job, to read them. Every other program, including an older Pi, a later Pi process in
    /// the same pane and one that is only named `pi`, gets the raw bytes as before.
    pub(super) fn api_input_origin(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        runtime: &crate::terminal::TerminalRuntime,
        context: ApiRequestContext,
    ) -> Option<InputOrigin> {
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        let claim = self
            .state
            .terminals
            .get(terminal_id)?
            .input_origin_claim()?;
        if foreground_pi(terminal_id, runtime)?.process_group != claim {
            return None;
        }
        Some(self.api_caller_origin(context))
    }

    /// Records a Pi's claim to read origin frames, only when the socket peer that sent it is a
    /// process in the pane's foreground Pi job. Another pane's process, a process outside every
    /// pane, or an unattributed caller cannot change it. A report without the claim changes
    /// nothing: frames stop when that job leaves the foreground.
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
        let Some(pi) = foreground_pi(&terminal_id, runtime) else {
            return;
        };
        if !pi.pids.contains(&peer_pid) {
            return;
        }
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.set_input_origin_claim(pi.process_group);
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
