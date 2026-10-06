use std::collections::HashMap;

use super::{TerminalId, TerminalRuntime};

/// Server-owned live terminal runtimes, keyed by durable terminal id.
///
/// This sits outside `AppState` so pure state can stay focused on workspace,
/// pane, and terminal metadata while the server/application layer owns PTYs,
/// parser backends, detector tasks, and channels.
#[derive(Default)]
pub(crate) struct TerminalRuntimeRegistry {
    runtimes: HashMap<TerminalId, TerminalRuntime>,
    agent_channels: HashMap<TerminalId, std::sync::Weak<crate::api::agent_channel::Channel>>,
}

impl TerminalRuntimeRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, terminal_id: &TerminalId) -> Option<&TerminalRuntime> {
        self.runtimes.get(terminal_id)
    }

    pub(crate) fn get_str(&self, terminal_id: &str) -> Option<&TerminalRuntime> {
        self.runtimes.get(terminal_id)
    }

    pub(crate) fn bind_agent_channel(
        &mut self,
        terminal_id: TerminalId,
        channel: &std::sync::Arc<crate::api::agent_channel::Channel>,
    ) {
        self.revoke_agent_channel(&terminal_id);
        self.agent_channels
            .insert(terminal_id, std::sync::Arc::downgrade(channel));
    }

    pub(crate) fn agent_channel_bound(
        &self,
        terminal_id: &str,
        channel: &crate::api::agent_channel::Channel,
    ) -> bool {
        self.runtimes.contains_key(terminal_id)
            && self
                .agent_channels
                .get(terminal_id)
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|bound| std::ptr::eq(bound.as_ref(), channel))
    }

    fn revoke_agent_channel(&mut self, terminal_id: &TerminalId) {
        if let Some(channel) = self
            .agent_channels
            .remove(terminal_id)
            .and_then(|channel| channel.upgrade())
        {
            channel.revoke();
        }
    }

    pub(crate) fn insert(
        &mut self,
        terminal_id: TerminalId,
        runtime: TerminalRuntime,
    ) -> Option<TerminalRuntime> {
        self.revoke_agent_channel(&terminal_id);
        self.runtimes.insert(terminal_id, runtime)
    }

    pub(crate) fn remove(&mut self, terminal_id: &TerminalId) -> Option<TerminalRuntime> {
        self.revoke_agent_channel(terminal_id);
        self.runtimes.remove(terminal_id)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &TerminalRuntime> {
        self.runtimes.values()
    }

    #[cfg(unix)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&TerminalId, &TerminalRuntime)> {
        self.runtimes.iter()
    }

    #[cfg(unix)]
    pub(crate) fn set_handoff_readers_paused(&self, paused: bool) {
        for runtime in self.runtimes.values() {
            runtime.set_handoff_reader_paused(paused);
        }
    }

    #[cfg(unix)]
    pub(crate) fn assume_handoff_ownership(&mut self) {
        for runtime in self.runtimes.values_mut() {
            runtime.assume_handoff_ownership();
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.runtimes.len()
    }

    #[cfg(unix)]
    pub(crate) fn nudge_child_redraw_after_handoff(&self) {
        for runtime in self.runtimes.values() {
            runtime.nudge_child_redraw_after_handoff();
        }
    }

    #[cfg(unix)]
    pub(crate) fn drain_for_handoff(
        &mut self,
    ) -> impl Iterator<Item = (TerminalId, TerminalRuntime)> + '_ {
        for channel in self
            .agent_channels
            .drain()
            .filter_map(|(_, channel)| channel.upgrade())
        {
            channel.revoke();
        }
        self.runtimes.drain()
    }

    #[cfg(test)]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (TerminalId, TerminalRuntime)> + '_ {
        self.runtimes.drain()
    }
}

impl From<HashMap<TerminalId, TerminalRuntime>> for TerminalRuntimeRegistry {
    fn from(runtimes: HashMap<TerminalId, TerminalRuntime>) -> Self {
        Self {
            runtimes,
            agent_channels: HashMap::new(),
        }
    }
}
