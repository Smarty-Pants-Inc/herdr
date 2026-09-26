use crate::api::ApiRequestContext;
use crate::app::App;
use crate::input_origin::InputOrigin;

impl App {
    /// The origin frame for an API write to this pane, or `None` to write raw bytes.
    ///
    /// Only Pi reads origin frames today, so a pane gets them only while its agent is Pi and Pi
    /// is its foreground process. Every other program gets the raw bytes as before.
    pub(super) fn api_input_origin(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        runtime: &crate::terminal::TerminalRuntime,
        context: ApiRequestContext,
    ) -> Option<InputOrigin> {
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        let terminal = self.state.terminals.get(terminal_id)?;
        if terminal.effective_known_agent() != Some(crate::detect::Agent::Pi)
            || !super::super::agents::runtime_hosts_agent(runtime, crate::detect::Agent::Pi)
        {
            return None;
        }
        Some(self.api_caller_origin(context))
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
