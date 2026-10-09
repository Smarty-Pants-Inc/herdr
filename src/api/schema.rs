use serde::{Deserialize, Serialize};

pub mod agents;
pub mod commands;
pub mod common;
pub mod events;
pub mod input_consumer;
pub mod integrations;
pub mod media;
pub mod panes;
pub mod plugins;
pub mod response;
pub mod server;
pub mod session;
pub mod tabs;
pub mod workspaces;
pub mod worktrees;

pub use agents::*;
pub use commands::*;
pub use common::*;
pub use events::*;
pub use input_consumer::*;
pub use integrations::*;
pub use media::*;
pub use panes::*;
pub use plugins::*;
pub use response::*;
pub use server::*;
pub use session::*;
pub use tabs::*;
pub use workspaces::*;
pub use worktrees::*;

pub const PANE_INPUT_POISON_NOTICE: &str = "Input is blocked because a failed flush may have left staged text on the line. Inspect or discard that text before explicitly clearing input poison.";
pub const PANE_INPUT_POISON_CLEAR_WARNING: &str =
    "Clearing input poison does not discard staged text; staged text may remain on the line.";

fn is_false(value: &bool) -> bool {
    !*value
}

// Missing means unguarded; a present null or non-string must never disable the guard.
fn deserialize_expected_terminal<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn deserialize_expected_agent_status<'de, D>(
    deserializer: D,
) -> Result<Option<AgentStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    AgentStatus::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "method", content = "params")]
// Request enums are short-lived wire values; keeping variants direct preserves
// the simple serde shape and avoids boxing churn across every caller.
#[allow(clippy::large_enum_variant)]
pub enum Method {
    #[serde(rename = "ping")]
    Ping(PingParams),
    #[serde(rename = "server.stop")]
    ServerStop(EmptyParams),
    #[serde(rename = "server.live_handoff")]
    ServerLiveHandoff(ServerLiveHandoffParams),
    /// A live handoff that must honour its source guards. Servers that predate
    /// the guards reject this method, so a guarded request never runs unguarded.
    #[serde(rename = "server.live_handoff_guarded")]
    ServerLiveHandoffGuarded(ServerLiveHandoffParams),
    /// A guarded live handoff to the importer that sent the request, which
    /// connects to the handoff socket itself (for example a systemd unit's
    /// main process). Servers without it reject the method, so it never runs
    /// as a push handoff.
    #[serde(rename = "server.live_handoff_pull")]
    ServerLiveHandoffPull(ServerLiveHandoffParams),
    #[serde(rename = "server.reload_config")]
    ServerReloadConfig(EmptyParams),
    #[serde(rename = "server.ssh_agent.register")]
    ServerSshAgentRegister(ServerSshAgentRegisterParams),
    #[serde(rename = "server.agent_manifests")]
    ServerAgentManifests(EmptyParams),
    #[serde(rename = "server.reload_agent_manifests")]
    ServerReloadAgentManifests(EmptyParams),
    #[serde(rename = "notification.show")]
    NotificationShow(NotificationShowParams),
    #[serde(rename = "product_announcement.dismiss")]
    ProductAnnouncementDismiss(ProductAnnouncementDismissParams),
    #[serde(rename = "release_notes.dismiss")]
    ReleaseNotesDismiss(ReleaseNotesDismissParams),
    #[serde(rename = "command.invoke")]
    CommandInvoke(CommandInvokeParams),
    #[serde(rename = "client.window_title.set")]
    ClientWindowTitleSet(ClientWindowTitleSetParams),
    #[serde(rename = "client.window_title.clear")]
    ClientWindowTitleClear(EmptyParams),
    #[serde(rename = "client_shell.surface.set")]
    ClientShellSurfaceSet(ClientShellSurfaceSetParams),
    #[serde(rename = "session.snapshot")]
    SessionSnapshot(EmptyParams),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateParams),
    // Creates a workspace linked to the Git checkout that contains `cwd`, even when
    // another workspace already holds that checkout. A separate method, because
    // `workspace.create` is a frozen endpoint shape and an older server would ignore a flag.
    #[serde(rename = "workspace.create_linked")]
    WorkspaceCreateLinked(WorkspaceCreateParams),
    #[serde(rename = "workspace.list")]
    WorkspaceList(EmptyParams),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceTarget),
    #[serde(rename = "workspace.focus")]
    WorkspaceFocus(WorkspaceTarget),
    #[serde(rename = "workspace.rename")]
    WorkspaceRename(WorkspaceRenameParams),
    #[serde(rename = "workspace.move")]
    WorkspaceMove(WorkspaceMoveParams),
    #[serde(rename = "workspace.move_block")]
    WorkspaceMoveBlock(WorkspaceMoveBlockParams),
    #[serde(rename = "workspace.report_metadata")]
    WorkspaceReportMetadata(WorkspaceReportMetadataParams),
    #[serde(rename = "workspace.close")]
    WorkspaceClose(WorkspaceCloseParams),
    #[serde(rename = "worktree.list")]
    WorktreeList(WorktreeListParams),
    #[serde(rename = "worktree.create")]
    WorktreeCreate(WorktreeCreateParams),
    /// Project-checked creation; older servers reject instead of ignoring permission.
    #[serde(rename = "worktree.create_project_checked")]
    WorktreeCreateProjectChecked(WorktreeCreateProjectCheckedParams),
    #[serde(rename = "worktree.open")]
    WorktreeOpen(WorktreeOpenParams),
    /// Project-checked opening; older servers reject instead of ignoring permission.
    #[serde(rename = "worktree.open_project_checked")]
    WorktreeOpenProjectChecked(WorktreeOpenProjectCheckedParams),
    #[serde(rename = "worktree.remove")]
    WorktreeRemove(WorktreeRemoveParams),
    #[serde(rename = "tab.create")]
    TabCreate(TabCreateParams),
    #[serde(rename = "tab.list")]
    TabList(TabListParams),
    #[serde(rename = "tab.get")]
    TabGet(TabTarget),
    #[serde(rename = "tab.focus")]
    TabFocus(TabTarget),
    #[serde(rename = "tab.rename")]
    TabRename(TabRenameParams),
    #[serde(rename = "tab.move")]
    TabMove(TabMoveParams),
    #[serde(rename = "tab.move_project_checked")]
    TabMoveProjectChecked(TabMoveProjectCheckedParams),
    #[serde(rename = "tab.close")]
    TabClose(TabTarget),
    #[serde(rename = "agent.list")]
    AgentList(EmptyParams),
    #[serde(rename = "agent.get")]
    AgentGet(AgentTarget),
    #[serde(rename = "agent.read")]
    AgentRead(AgentReadParams),
    #[serde(rename = "agent.explain")]
    AgentExplain(AgentTarget),
    #[serde(rename = "agent.send_keys")]
    AgentSendKeys(AgentSendKeysParams),
    #[serde(rename = "agent.rename")]
    AgentRename(AgentRenameParams),
    #[serde(rename = "agent.view.set")]
    AgentViewSet(AgentViewSetParams),
    #[serde(rename = "agent.view.clear")]
    AgentViewClear(AgentViewClearParams),
    #[serde(rename = "agent.focus")]
    AgentFocus(AgentTarget),
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    /// Requires `expected_terminal`; older servers reject instead of ignoring the guard.
    #[serde(rename = "agent.start_guarded")]
    AgentStartGuarded(AgentStartParams),
    #[serde(rename = "agent.prompt")]
    AgentPrompt(AgentPromptParams),
    /// Local JSON only; older servers reject instead of ignoring identity guards.
    #[serde(rename = "agent.prompt_session_checked")]
    AgentPromptSessionChecked(AgentPromptParams),
    /// Local JSON only; requires status and session expectations. Older servers
    /// reject instead of silently ignoring the detected-status guard.
    #[serde(rename = "agent.prompt_status_checked")]
    AgentPromptStatusChecked(AgentPromptParams),
    #[serde(rename = "agent.wait")]
    AgentWait(AgentWaitParams),
    #[serde(rename = "pane.split")]
    PaneSplit(PaneSplitParams),
    #[serde(rename = "pane.swap")]
    PaneSwap(PaneSwapParams),
    /// Project-checked swap; its distinct name prevents unsafe old-server fallback.
    #[serde(rename = "pane.swap_project_checked")]
    PaneSwapProjectChecked(PaneSwapProjectCheckedParams),
    #[serde(rename = "pane.move")]
    PaneMove(PaneMoveParams),
    /// Project-checked move; its distinct name prevents unsafe old-server fallback.
    #[serde(rename = "pane.move_project_checked")]
    PaneMoveProjectChecked(PaneMoveParams),
    #[serde(rename = "pane.zoom")]
    PaneZoom(PaneZoomParams),
    #[serde(rename = "pane.layout")]
    PaneLayout(PaneLayoutParams),
    #[serde(rename = "pane.process_info")]
    PaneProcessInfo(PaneProcessInfoParams),
    #[serde(rename = "layout.export")]
    LayoutExport(LayoutExportParams),
    #[serde(rename = "layout.apply")]
    LayoutApply(LayoutApplyParams),
    #[serde(rename = "layout.apply_restorable")]
    LayoutApplyRestorable(LayoutApplyParams),
    /// Project-checked apply; its distinct name prevents unsafe old-server fallback.
    #[serde(rename = "layout.apply_project_checked")]
    LayoutApplyProjectChecked(LayoutApplyProjectCheckedParams),
    #[serde(rename = "layout.set_split_ratio")]
    LayoutSetSplitRatio(LayoutSetSplitRatioParams),
    #[serde(rename = "pane.neighbor")]
    PaneNeighbor(PaneNeighborParams),
    #[serde(rename = "pane.edges")]
    PaneEdges(PaneEdgesParams),
    #[serde(rename = "pane.focus_direction")]
    PaneFocusDirection(PaneFocusDirectionParams),
    #[serde(rename = "pane.resize")]
    PaneResize(PaneResizeParams),
    #[serde(rename = "pane.scroll")]
    PaneScroll(PaneScrollParams),
    #[serde(rename = "pane.clear")]
    PaneClear(PaneTarget),
    #[serde(rename = "pane.clear_input_poison")]
    PaneClearInputPoison(PaneTarget),
    #[serde(rename = "pane.edit_scrollback")]
    PaneEditScrollback(PaneTarget),
    #[serde(rename = "pane.selection.read")]
    PaneSelectionRead(PaneSelectionReadParams),
    #[serde(rename = "pane.copy_motion")]
    PaneCopyMotion(PaneCopyMotionParams),
    #[serde(rename = "pane.copy_search")]
    PaneCopySearch(PaneCopySearchParams),
    #[serde(rename = "pane.list")]
    PaneList(PaneListParams),
    #[serde(rename = "pane.current")]
    PaneCurrent(PaneCurrentParams),
    #[serde(rename = "pane.get")]
    PaneGet(PaneTarget),
    #[serde(rename = "pane.last_input")]
    PaneLastInput(PaneLastInputParams),
    /// Local API only; the pane actor validates the kernel-pinned socket peer.
    #[serde(rename = "pane.input_consumer.enroll")]
    PaneInputConsumerEnroll(PaneInputConsumerEnrollParams),
    #[serde(rename = "pane.input_consumer.cut")]
    PaneInputConsumerCut(PaneInputConsumerCutParams),
    #[serde(rename = "pane.input_consumer.release")]
    PaneInputConsumerRelease(PaneInputConsumerReleaseParams),
    #[serde(rename = "pane.focus")]
    PaneFocus(PaneTarget),
    #[serde(rename = "pane.input.set")]
    PaneInputSet(PaneInputSetParams),
    #[serde(rename = "pane.link.activate")]
    PaneLinkActivate(PaneLinkActivateParams),
    #[serde(rename = "pane.link.resolve")]
    PaneLinkResolve(PaneLinkActivateParams),
    #[serde(rename = "pane.rename")]
    PaneRename(PaneRenameParams),
    #[serde(rename = "pane.send_text")]
    PaneSendText(PaneSendTextParams),
    #[serde(rename = "pane.send_text_session_checked")]
    PaneSendTextSessionChecked(PaneSendTextParams),
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys(PaneSendKeysParams),
    #[serde(rename = "pane.send_keys_session_checked")]
    PaneSendKeysSessionChecked(PaneSendKeysParams),
    #[serde(rename = "pane.send_input")]
    PaneSendInput(PaneSendInputParams),
    /// Requires `expected_terminal`; older servers reject instead of ignoring the guard.
    #[serde(rename = "pane.send_input_guarded")]
    PaneSendInputGuarded(PaneSendInputParams),
    #[serde(rename = "pane.read")]
    PaneRead(PaneReadParams),
    #[serde(rename = "pane.report_agent")]
    PaneReportAgent(PaneReportAgentParams),
    #[serde(rename = "pane.report_agent_session")]
    PaneReportAgentSession(PaneReportAgentSessionParams),
    #[serde(rename = "pane.report_metadata")]
    PaneReportMetadata(PaneReportMetadataParams),
    #[serde(rename = "pane.clear_agent_authority")]
    PaneClearAgentAuthority(PaneClearAgentAuthorityParams),
    #[serde(rename = "pane.release_agent")]
    PaneReleaseAgent(PaneReleaseAgentParams),
    #[serde(rename = "pane.close")]
    PaneClose(PaneTarget),
    #[serde(rename = "popup.close")]
    PopupClose(EmptyParams),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(EventsSubscribeParams),
    #[serde(rename = "events.wait")]
    EventsWait(EventsWaitParams),
    #[serde(rename = "pane.wait_for_output")]
    PaneWaitForOutput(PaneWaitForOutputParams),
    #[serde(rename = "integration.list")]
    IntegrationList(EmptyParams),
    #[serde(rename = "integration.install")]
    IntegrationInstall(IntegrationInstallParams),
    #[serde(rename = "integration.uninstall")]
    IntegrationUninstall(IntegrationUninstallParams),
    #[serde(rename = "plugin.link")]
    PluginLink(PluginLinkParams),
    #[serde(rename = "plugin.list")]
    PluginList(PluginListParams),
    #[serde(rename = "plugin.unlink")]
    PluginUnlink(PluginUnlinkParams),
    #[serde(rename = "plugin.enable")]
    PluginEnable(PluginSetEnabledParams),
    #[serde(rename = "plugin.disable")]
    PluginDisable(PluginSetEnabledParams),
    #[serde(rename = "plugin.action.list")]
    PluginActionList(PluginActionListParams),
    #[serde(rename = "plugin.action.invoke")]
    PluginActionInvoke(PluginActionInvokeParams),
    #[serde(rename = "plugin.log.list")]
    PluginLogList(PluginLogListParams),
    #[serde(rename = "plugin.pane.open")]
    PluginPaneOpen(PluginPaneOpenParams),
    #[serde(rename = "plugin.pane.focus")]
    PluginPaneFocus(PluginPaneFocusParams),
    #[serde(rename = "plugin.pane.close")]
    PluginPaneClose(PluginPaneCloseParams),
    #[serde(rename = "pane.media_open")]
    PaneMediaOpen(PaneTarget),
    #[serde(rename = "media.answer")]
    MediaAnswer(MediaAnswerParams),
    #[serde(rename = "media.mute")]
    MediaMute(MediaMuteParams),
    #[serde(rename = "media.state")]
    MediaState(MediaSessionTarget),
    #[serde(rename = "media.close")]
    MediaClose(MediaSessionTarget),
}

#[cfg(test)]
mod tests;
