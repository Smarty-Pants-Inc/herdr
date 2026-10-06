//! App-side attribution and lifecycle of server-owned agent channels.
#[cfg(test)]
#[path = "agent_channel_tests.rs"]
mod tests;

use super::responses::{encode_error, encode_error_body};
use crate::api::agent_channel::{json_success, Channel, Outcome, MAX_ID_BYTES};
use crate::api::schema::{
    AgentChannelInfoParams, AgentPromptGuardedParams, AgentRegisterSelfParams, Method, Request,
};
use crate::app::App;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct AgentChannels {
    owners: std::collections::HashMap<String, Arc<Channel>>,
}
impl AgentChannels {
    pub(crate) fn revoke_all(&mut self) {
        for channel in self.owners.values() {
            channel.revoke();
        }
        self.owners.clear();
    }
    pub(crate) fn revoke_terminal(&mut self, terminal: &str) {
        if let Some(channel) = self.owners.remove(terminal) {
            channel.revoke();
        }
    }
}
impl Drop for AgentChannels {
    fn drop(&mut self) {
        self.revoke_all();
    }
}

fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

fn owner_allows_replacement(
    old: crate::platform::ProcessIdentity,
    new: crate::platform::ProcessIdentity,
    attached: bool,
    liveness: crate::platform::ProcessLiveness,
) -> bool {
    !attached || old == new || liveness == crate::platform::ProcessLiveness::Dead
}

impl App {
    pub(crate) fn revoke_agent_channels(&mut self) {
        self.agent_channels.revoke_all();
    }
    pub(crate) fn revoke_agent_channels_for_terminals(
        &mut self,
        terminals: impl IntoIterator<Item = crate::terminal::TerminalId>,
    ) {
        for terminal in terminals {
            self.agent_channels.revoke_terminal(terminal.as_str());
        }
    }
    pub(crate) fn revoke_agent_channels_for_workspace_close(&mut self, ws_idx: usize) {
        let terminals: Vec<_> = self
            .state
            .workspace_close_indices(ws_idx)
            .into_iter()
            .flat_map(|index| self.state.terminal_ids_for_workspace(index))
            .collect();
        self.revoke_agent_channels_for_terminals(terminals);
    }
    fn channel_attached(&self, channel: &Channel) -> bool {
        let Some(workspace) = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == channel.workspace_id)
        else {
            return false;
        };
        if workspace.terminal_id(channel.pane_id).map(|id| id.as_str())
            != Some(channel.terminal_id.as_str())
        {
            return false;
        }
        // A missing OS observation is not attachment replacement and cannot release
        // the live-owner reservation. Runtime insertion/removal revokes this binding.
        self.terminal_runtimes
            .agent_channel_bound(&channel.terminal_id, channel)
    }
    pub(crate) fn terminal_has_registered_owner(&self, terminal: &str) -> bool {
        self.agent_channels
            .owners
            .get(terminal)
            .is_some_and(|channel| {
                self.channel_attached(channel)
                    && crate::platform::registered_process_liveness(channel.peer)
                        != crate::platform::ProcessLiveness::Dead
            })
    }
    fn channel_valid(&self, channel: &Channel) -> bool {
        if !channel.is_ready()
            || !self.channel_attached(channel)
            || !crate::platform::registered_process_is_foreground(channel.root, channel.peer)
        {
            return false;
        }
        self.pane_target_for_peer_identity(channel.peer)
            .is_some_and(|target| {
                target.terminal_id == channel.terminal_id
                    && target.pane_id == channel.pane_id
                    && self.state.workspaces[target.ws_idx].id == channel.workspace_id
            })
    }
    pub(super) fn handle_agent_register_self(
        &mut self,
        id: String,
        params: AgentRegisterSelfParams,
        context: crate::api::ApiRequestContext,
    ) -> String {
        if !crate::platform::capabilities().registered_agent_channel {
            return encode_error(
                id,
                "unsupported_platform",
                "registered agent channels are unsupported on this platform",
            );
        }
        let Some(transport) = params.transport else {
            return encode_error(
                id,
                "connection_local_only",
                "registration requires a persistent direct local socket",
            );
        };
        if !valid_token(&params.session_generation) {
            return encode_error(
                id,
                "invalid_request",
                "nonempty bounded session_generation required",
            );
        }
        let Some(peer) = context.local_peer_identity else {
            return encode_error(
                id,
                "agent_registration_denied",
                "kernel peer identity unavailable",
            );
        };
        // #152 checked ancestry, not a supplied PID, launch marker or detected agent.
        let Some(target) = self.pane_target_for_peer_identity(peer) else {
            return encode_error(
                id,
                "agent_registration_denied",
                "cannot attribute peer to exactly one live pane",
            );
        };
        if let Some(asserted) = params.pane_id {
            if self.parse_current_public_pane_id(&asserted) != Some((target.ws_idx, target.pane_id))
            {
                return encode_error(
                    id,
                    "agent_registration_denied",
                    "pane_id does not assert the peer's current pane",
                );
            }
        }
        let Some(root) = self
            .terminal_runtimes
            .get_str(target.terminal_id.as_str())
            .and_then(crate::terminal::TerminalRuntime::child_process_identity)
        else {
            return encode_error(
                id,
                "agent_registration_denied",
                "pane root identity unavailable",
            );
        };
        if !crate::platform::registered_process_is_foreground(root, peer) {
            return encode_error(
                id,
                "agent_not_foreground",
                "registrant is not the pane's foreground job",
            );
        }
        if let Some(old) = self.agent_channels.owners.get(&target.terminal_id) {
            if !owner_allows_replacement(
                old.peer,
                peer,
                self.channel_attached(old),
                crate::platform::registered_process_liveness(old.peer),
            ) {
                return encode_error(
                    id,
                    "agent_owner_conflict",
                    "a live or unobservable registered owner reserves this terminal",
                );
            }
        }
        let epoch = match crate::platform::fresh_registration_epoch() {
            Ok(epoch) => epoch,
            Err(error) => return encode_error(id, "agent_registration_denied", error.to_string()),
        };
        // Recheck all endpoints before serialized ownership replacement.
        if self.pane_target_for_peer_identity(peer).as_ref() != Some(&target)
            || self
                .terminal_runtimes
                .get_str(target.terminal_id.as_str())
                .and_then(crate::terminal::TerminalRuntime::child_process_identity)
                != Some(root)
            || !crate::platform::registered_process_is_foreground(root, peer)
        {
            return encode_error(
                id,
                "agent_registration_denied",
                "registration endpoints changed",
            );
        }
        let Ok(mut slot) = transport.0.lock() else {
            return encode_error(
                id,
                "agent_registration_denied",
                "connection install unavailable",
            );
        };
        if slot.closed || slot.channel.is_some() {
            return encode_error(
                id,
                "agent_registration_denied",
                "connection closed or already registered",
            );
        }
        self.agent_channels.revoke_terminal(&target.terminal_id); // Old epoch revoked before new activation.
        let (channel, receiver) = Channel::new(
            target.terminal_id.clone(),
            epoch.clone(),
            params.session_generation.clone(),
            peer,
            root,
            self.state.workspaces[target.ws_idx].id.clone(),
            target.pane_id,
        );
        if let Some(terminal) = self.state.terminals.get(target.terminal_id.as_str()) {
            self.terminal_runtimes
                .bind_agent_channel(terminal.id.clone(), &channel);
        }
        self.agent_channels
            .owners
            .insert(target.terminal_id.clone(), channel.clone());
        slot.channel = Some((channel, receiver));
        json_success(
            id,
            serde_json::json!({"terminal_id":target.terminal_id,"registration_epoch":epoch,
            "session_generation":params.session_generation,"ready":true}),
        )
    }
    pub(super) fn handle_agent_channel_info(
        &mut self,
        id: String,
        params: AgentChannelInfoParams,
    ) -> String {
        let target = match self.resolve_terminal_target(&params.target) {
            Ok(target) => target,
            Err(error) => return encode_error_body(id, self.agent_target_error_body(error)),
        };
        let mut result = serde_json::json!({"terminal_id":target.terminal_id,"ready":false});
        if crate::platform::capabilities().registered_agent_channel {
            if let Some(channel) = self.agent_channels.owners.get(&target.terminal_id) {
                if self.channel_valid(channel) {
                    result["ready"] = true.into();
                    result["registration_epoch"] = channel.epoch.clone().into();
                    result["session_generation"] = channel.session_generation.clone().into();
                } else if !self.channel_attached(channel)
                    || crate::platform::registered_process_liveness(channel.peer)
                        == crate::platform::ProcessLiveness::Dead
                {
                    self.agent_channels.revoke_terminal(&target.terminal_id);
                }
            }
        }
        json_success(id, result)
    }
    pub(crate) fn handle_deferred_guarded_agent_prompt(
        &mut self,
        request: Request,
        context: crate::api::ApiRequestContext,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        if !matches!(request.method, Method::AgentPromptGuarded(_)) {
            return false;
        }
        if !crate::platform::capabilities().registered_agent_channel {
            let _ = respond_to.send(encode_error(
                request.id,
                "unsupported_platform",
                "registered agent channels unsupported",
            ));
            return true;
        }
        if let Some(response) = self.cross_pane_input_denial(&request, context) {
            let _ = respond_to.send(response);
            return true;
        }
        let Method::AgentPromptGuarded(params) = request.method else {
            return false;
        };
        let reservation = self.reserve_guarded_prompt(&params);
        match reservation {
            Ok((delivery, duplicate, waiter)) => {
                std::thread::spawn(move || {
                    let _waiter = waiter;
                    let _ = respond_to.send(delivery.wait().response(request.id, duplicate));
                });
            }
            Err(outcome) => {
                let _ = respond_to.send(outcome.response(request.id, false));
            }
        }
        true
    }
    fn reserve_guarded_prompt(
        &mut self,
        params: &AgentPromptGuardedParams,
    ) -> Result<
        (
            Arc<crate::api::agent_channel::Delivery>,
            bool,
            crate::api::agent_channel::ReceiptWaiter,
        ),
        Outcome,
    > {
        if !crate::platform::capabilities().registered_agent_channel {
            return Err(Outcome::failure(
                "unsupported_platform",
                "registered agent channels unsupported",
            ));
        }
        if params.text.is_empty()
            || !valid_token(&params.request_id)
            || !valid_token(&params.expected_terminal)
            || !valid_token(&params.expected_registration_epoch)
        {
            return Err(Outcome::failure(
                "invalid_request",
                "nonempty bounded IDs and text required",
            ));
        }
        let timeout = params.timeout_ms.unwrap_or(10_000);
        if timeout == 0 || timeout > 300_000 {
            return Err(Outcome::failure(
                "invalid_timeout",
                "timeout_ms must be between 1 and 300000",
            ));
        }
        let target = self.resolve_terminal_target(&params.target).map_err(|_| {
            Outcome::failure("agent_not_found", "terminal target not found or ambiguous")
        })?;
        if target.terminal_id != params.expected_terminal {
            return Err(Outcome::failure(
                "terminal_identity_mismatch",
                "target no longer owns expected terminal",
            ));
        }
        let channel = self
            .agent_channels
            .owners
            .get(&target.terminal_id)
            .ok_or_else(|| Outcome::failure("agent_channel_unavailable", "no registered owner"))?;
        if channel.epoch != params.expected_registration_epoch {
            return Err(Outcome::failure(
                "registration_epoch_mismatch",
                "obsolete registration epoch; never replay into a new channel",
            ));
        }
        if !self.channel_valid(channel) {
            return Err(Outcome::failure(
                "agent_channel_unavailable",
                "registered owner is disconnected, stale or not foreground",
            ));
        }
        channel.reserve(
            params.request_id.clone(),
            params.text.clone(),
            Duration::from_millis(timeout),
        )
    }
}
