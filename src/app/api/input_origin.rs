use crate::api::ApiRequestContext;
use crate::app::App;
use crate::input_origin::InputOrigin;

/// The Pi processes Herdr can identify in a pane's foreground job. Used only to accept a claim:
/// identification is best effort, so absence from this list proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForegroundPi {
    pub pi_processes: Vec<crate::input_origin::InputOriginClaim>,
}

/// Whether a process that claimed to read origin frames reads the pane now, checked on that
/// process directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimantStatus {
    /// The same process generation runs in the pane's foreground process group.
    Reading,
    /// It runs, but another process group owns the terminal (for example, it is suspended). It
    /// can read the pane again later, so its claim is kept.
    Background,
    /// It exited, or its pid now names a later process.
    Gone,
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
        crate::platform::ProcessStart::Gone => ClaimantStatus::Gone,
        crate::platform::ProcessStart::Unknown => ClaimantStatus::Unknown,
        crate::platform::ProcessStart::Running { start_time, .. }
            if start_time != claim.start_time =>
        {
            // The pid names a later process: the claimant exited.
            ClaimantStatus::Gone
        }
        crate::platform::ProcessStart::Running {
            process_group: Some(group),
            ..
        } => match pane_foreground_group {
            Some(foreground) if foreground == group => ClaimantStatus::Reading,
            Some(_) => ClaimantStatus::Background,
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
/// own, so a wrapper or sibling process in the same job is not taken for Pi, and each with the
/// pane's terminal as its standard input. Tests can set the
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
    let Some(child_pid) = runtime.child_pid() else {
        return ForegroundPi {
            pi_processes: Vec::new(),
        };
    };
    let Some(job) = crate::detect::foreground_job(child_pid) else {
        return ForegroundPi {
            pi_processes: Vec::new(),
        };
    };
    // The pane's terminal, as the pane's first process has it on standard input.
    let pane_terminal = crate::platform::process_stdin_terminal(child_pid);
    let pi_processes = job
        .processes
        .iter()
        .filter(|process| {
            reads_pane_terminal(
                pane_terminal,
                crate::platform::process_stdin_terminal(process.pid),
            )
        })
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

/// Whether a process with `stdin` on standard input may read the pane whose first process has
/// `pane`. A process on a pipe or on another terminal cannot, so a helper that Pi starts with
/// piped input is not a candidate. When either side cannot be read (and on Windows, which
/// cannot tell), the candidate passes: refusing a genuine reader's claim would send it raw
/// input, which it would record as typed.
///
/// ponytail: this narrows who can claim; it does not prove that the process reads the
/// terminal. A same-job process on the terminal that is named like Pi can still claim while no
/// claimant reads; the effect is frame text in an unsupported reader's input, not a false
/// attribution (P3 by scope, smarty-dev#931). A challenge sent through the terminal would prove
/// the reader; revisit it if that frame text is seen in real use.
pub(crate) fn reads_pane_terminal(
    pane: crate::platform::StdinTerminal,
    stdin: crate::platform::StdinTerminal,
) -> bool {
    use crate::platform::StdinTerminal;
    match (pane, stdin) {
        (_, StdinTerminal::NotTerminal) => false,
        (StdinTerminal::Terminal(pane), StdinTerminal::Terminal(stdin)) => pane == stdin,
        _ => true,
    }
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
        return ClaimantStatus::Gone;
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
    /// A pane gets frames while one of the Pi processes that claimed to read them (same pid and
    /// start time) runs in its foreground process group. It gets raw bytes when it has no claim,
    /// or when Herdr sees that every claimant has exited or is in the background. If Herdr
    /// cannot tell for any claimant, a claimed pane gets nothing: raw bytes would be recorded as
    /// typed input.
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
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return Ok(None);
        };
        let mut unknown = false;
        for claim in terminal.input_origin_claims() {
            match claimant_status(terminal_id, runtime, *claim) {
                ClaimantStatus::Reading => return Ok(Some(self.api_caller_origin(context))),
                ClaimantStatus::Unknown => unknown = true,
                ClaimantStatus::Background | ClaimantStatus::Gone => {}
            }
        }
        if unknown {
            Err(InputOriginUnavailable)
        } else {
            Ok(None)
        }
    }

    /// Records a Pi's claim to read origin frames, only when the socket peer that sent it is
    /// itself a Pi process in the pane's foreground job. A wrapper or sibling in that job,
    /// another pane's process, a process outside every pane, or an unattributed caller cannot
    /// add one. While a claimant reads the pane or cannot be checked, no other process can add a
    /// claim: the first Pi to claim is the one that reads the terminal (only the TUI sets the
    /// claim, and any other Pi in its job was started later). Claims are kept per process, so a
    /// suspended Pi keeps its claim for when it resumes; only claims of processes known to be
    /// gone are dropped. A report without the claim changes nothing.
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
        let current: Vec<_> = self
            .state
            .terminals
            .get(&terminal_id)
            .map(|terminal| {
                terminal
                    .input_origin_claims()
                    .iter()
                    .map(|current| (*current, claimant_status(&terminal_id, runtime, *current)))
                    .collect()
            })
            .unwrap_or_default();
        // While a claimant reads the pane, or may, it is the reader: another process in its job
        // cannot become one by claiming (a helper named like Pi, say). A new reader is admitted
        // only once every other claimant is known to be in the background or gone.
        if current.iter().any(|(existing, status)| {
            *existing != claim
                && matches!(status, ClaimantStatus::Reading | ClaimantStatus::Unknown)
        }) {
            return;
        }
        let gone: Vec<_> = current
            .iter()
            .filter(|(_, status)| *status == ClaimantStatus::Gone)
            .map(|(existing, _)| *existing)
            .collect();
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.retain_input_origin_claims(|current| !gone.contains(current));
            terminal.add_input_origin_claim(claim);
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
        static CLAIMANTS: RefCell<HashMap<(String, Option<u32>), ClaimantStatus>> =
            RefCell::new(HashMap::new());
    }

    /// Sets the Pi processes a test identifies in `terminal_id`'s foreground job. A claimant
    /// without an explicit status then reads the pane while it is in that list, and is gone
    /// otherwise; `None` uses the live lookups.
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

    /// Overrides the claimant check for every claimant of `terminal_id`.
    pub(crate) fn set_claimant_status(
        terminal_id: &crate::terminal::TerminalId,
        status: ClaimantStatus,
    ) {
        CLAIMANTS.with(|claimants| {
            claimants
                .borrow_mut()
                .insert((terminal_id.to_string(), None), status)
        });
    }

    /// Overrides the claimant check for the claimant `pid` of `terminal_id`.
    pub(crate) fn set_pid_status(
        terminal_id: &crate::terminal::TerminalId,
        pid: u32,
        status: ClaimantStatus,
    ) {
        CLAIMANTS.with(|claimants| {
            claimants
                .borrow_mut()
                .insert((terminal_id.to_string(), Some(pid)), status)
        });
    }

    pub(super) fn foreground_pi(terminal_id: &crate::terminal::TerminalId) -> Option<ForegroundPi> {
        JOBS.with(|jobs| jobs.borrow().get(&terminal_id.to_string()).cloned())
    }

    pub(super) fn claimant_status(
        terminal_id: &crate::terminal::TerminalId,
        claim: crate::input_origin::InputOriginClaim,
    ) -> Option<ClaimantStatus> {
        let key = terminal_id.to_string();
        CLAIMANTS
            .with(|claimants| {
                let claimants = claimants.borrow();
                claimants
                    .get(&(key.clone(), Some(claim.pid)))
                    .or_else(|| claimants.get(&(key.clone(), None)))
                    .copied()
            })
            .or_else(|| {
                let job = foreground_pi(terminal_id)?;
                Some(if job.pi_processes.contains(&claim) {
                    ClaimantStatus::Reading
                } else {
                    ClaimantStatus::Gone
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
    fn only_a_process_on_the_pane_terminal_is_a_candidate_reader() {
        use crate::platform::StdinTerminal::{NotTerminal, Terminal, Unknown};
        assert!(super::reads_pane_terminal(Terminal(1), Terminal(1)));
        assert!(!super::reads_pane_terminal(Terminal(1), Terminal(2)));
        // A piped helper.
        assert!(!super::reads_pane_terminal(Terminal(1), NotTerminal));
        // Unreadable either side: a genuine reader must not lose its claim.
        assert!(super::reads_pane_terminal(Unknown, Terminal(1)));
        assert!(super::reads_pane_terminal(Terminal(1), Unknown));
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
            ClaimantStatus::Gone
        );
        // The pid now names a later process.
        assert_eq!(
            classify_claimant(CLAIM, running(101, Some(7000)), Some(7000)),
            ClaimantStatus::Gone
        );
        // It runs but another group owns the terminal (for example, it was suspended). It keeps
        // its claim for when it returns.
        assert_eq!(
            classify_claimant(CLAIM, running(100, Some(7000)), Some(9000)),
            ClaimantStatus::Background
        );
    }
}
