use serde::{Deserialize, Serialize};

use super::agents::AgentInfo;
use super::common::{ClientWindowTitleReason, NotificationShowReason};
use super::events::EventEnvelope;
use super::integrations::{
    IntegrationInstallResult, IntegrationTarget, IntegrationUninstallResult,
};
use super::panes::{
    LayoutDescription, PaneEdgesResult, PaneFocusDirectionResult, PaneInfo, PaneLayoutSnapshot,
    PaneMoveResult, PaneNeighborResult, PaneProcessInfo, PaneReadResult, PaneResizeResult,
    PaneSwapResult, PaneTextPoint, PaneTextRange, PaneZoomResult,
};
use super::plugins::{
    InstalledPluginInfo, PluginActionInfo, PluginCommandLogInfo, PluginInvocationContext,
    PluginPaneInfo,
};
use super::server::ServerCapabilities;
use super::session::SessionSnapshot;
use super::tabs::TabInfo;
use super::workspaces::WorkspaceInfo;
use super::worktrees::{WorktreeInfo, WorktreeSourceInfo};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SuccessResponse {
    pub id: String,
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ErrorResponse {
    pub id: String,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        protocol: u32,
        #[serde(default)]
        capabilities: Option<ServerCapabilities>,
    },
    SessionSnapshot {
        snapshot: Box<SessionSnapshot>,
    },
    WorkspaceInfo {
        workspace: WorkspaceInfo,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
    },
    WorktreeList {
        source: WorktreeSourceInfo,
        worktrees: Vec<WorktreeInfo>,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        path: String,
        forced: bool,
    },
    TabInfo {
        tab: TabInfo,
    },
    TabCreated {
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    TabList {
        tabs: Vec<TabInfo>,
    },
    AgentInfo {
        agent: AgentInfo,
    },
    AgentStarted {
        agent: AgentInfo,
        argv: Vec<String>,
    },
    AgentPrompted {
        agent: AgentInfo,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    AgentView {
        active: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    PaneInfo {
        pane: PaneInfo,
    },
    PaneList {
        panes: Vec<PaneInfo>,
    },
    PaneCurrent {
        pane: PaneInfo,
    },
    PaneSwap {
        swap: PaneSwapResult,
    },
    PaneMove {
        move_result: PaneMoveResult,
    },
    PaneZoom {
        zoom: PaneZoomResult,
    },
    PaneLayout {
        layout: PaneLayoutSnapshot,
    },
    PaneProcessInfo {
        process_info: PaneProcessInfo,
    },
    LayoutExport {
        layout: LayoutDescription,
    },
    LayoutApply {
        layout: LayoutDescription,
    },
    LayoutSplitRatioSet {
        layout: LayoutDescription,
    },
    PaneNeighbor {
        neighbor: PaneNeighborResult,
    },
    PaneEdges {
        edges: PaneEdgesResult,
    },
    PaneFocusDirection {
        focus: PaneFocusDirectionResult,
    },
    PaneResize {
        resize: PaneResizeResult,
    },
    PaneRead {
        read: PaneReadResult,
    },
    PaneSelection {
        pane_id: String,
        text: String,
    },
    PaneCopyMotion {
        pane_id: String,
        cursor: PaneTextPoint,
        content_revision: u64,
    },
    PaneCopySearch {
        pane_id: String,
        content_revision: u64,
        matches: Vec<PaneTextRange>,
        total: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_global: Option<u64>,
    },
    PaneGraphicsFrameAck {
        sequence: u64,
        revision: u64,
    },
    PaneGraphicsInfo {
        cell_width_px: u32,
        cell_height_px: u32,
        /// True only when this pane is on the currently rendered terminal surface.
        pane_visible: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_frame_directory: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        file_frame_formats: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_frame_max_bytes: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_frame_direct_max_bytes: Option<usize>,
        /// Accepts damage metadata while still consuming a complete canonical file.
        #[serde(default)]
        file_frame_damage: bool,
        #[serde(default)]
        max_layers_per_pane: usize,
        #[serde(default)]
        pixel_mouse: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_frame_transport: Option<String>,
    },
    AgentExplain {
        explain: serde_json::Value,
    },
    SubscriptionStarted {},
    WaitMatched {
        event: EventEnvelope,
    },
    OutputMatched {
        pane_id: String,
        revision: u64,
        matched_line: Option<String>,
        read: PaneReadResult,
    },
    NotificationShow {
        shown: bool,
        reason: NotificationShowReason,
    },
    ClientWindowTitle {
        changed: bool,
        reason: ClientWindowTitleReason,
    },
    IntegrationList {
        integrations: Vec<super::integrations::IntegrationInfo>,
    },
    IntegrationInstall {
        target: IntegrationTarget,
        details: IntegrationInstallResult,
    },
    IntegrationUninstall {
        target: IntegrationTarget,
        details: IntegrationUninstallResult,
    },
    AgentManifestReload {
        manifests: Vec<AgentManifestInfo>,
    },
    AgentManifestStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_check_unix: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_result: Option<String>,
        manifests: Vec<AgentManifestInfo>,
    },
    PluginLinked {
        plugin: InstalledPluginInfo,
    },
    PluginList {
        plugins: Vec<InstalledPluginInfo>,
    },
    PluginUnlinked {
        plugin_id: String,
        removed: bool,
    },
    PluginEnabled {
        plugin: InstalledPluginInfo,
    },
    PluginDisabled {
        plugin: InstalledPluginInfo,
    },
    PluginActionList {
        actions: Vec<PluginActionInfo>,
    },
    PluginActionInvoked {
        action: PluginActionInfo,
        context: PluginInvocationContext,
        log: PluginCommandLogInfo,
    },
    PaneLinkResolved {
        regions: Vec<super::panes::PaneLinkRegion>,
    },
    PaneLinkActivated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        handled: bool,
    },
    PluginLogList {
        logs: Vec<PluginCommandLogInfo>,
    },
    PluginPaneOpened {
        plugin_pane: PluginPaneInfo,
    },
    PluginPaneFocused {
        plugin_pane: PluginPaneInfo,
    },
    PluginPaneClosed {
        pane_id: String,
    },
    ConfigReload {
        status: crate::config::ConfigReloadStatus,
        diagnostics: Vec<String>,
    },
    /// Acknowledgement for the client-shell surface interest lease. This method is new on the
    /// endpoint protocol, so its revision-bearing result can establish an activation floor.
    ClientShellSurfaceSet {
        active: bool,
        projection_revision: u64,
    },
    Ok {},
    MediaOffer {
        session_id: String,
        sdp: String,
        /// Audio devices actually opened by the client for this offer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_devices: Option<crate::protocol::media::MediaAudioDevices>,
    },
    MediaSession {
        session_id: String,
        state: super::media::MediaSessionState,
        muted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentManifestInfo {
    pub agent: String,
    pub source: String,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_remote_version: Option<String>,
    pub local_override_shadowing_remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_update_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_update_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_last_checked_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::media::{MediaAudioDevice, MediaAudioDevices};

    #[test]
    fn media_offer_response_accepts_old_absence_and_round_trips_device_metadata() {
        let old = serde_json::json!({"type": "media_offer", "session_id": "m", "sdp": "v=0"});
        let decoded: ResponseResult = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            decoded,
            ResponseResult::MediaOffer {
                session_id: "m".into(),
                sdp: "v=0".into(),
                audio_devices: None,
            }
        );
        assert_eq!(serde_json::to_value(decoded).unwrap(), old);

        let result = ResponseResult::MediaOffer {
            session_id: "m".into(),
            sdp: "v=0".into(),
            audio_devices: Some(MediaAudioDevices {
                input: MediaAudioDevice {
                    name: "Mic".into(),
                    missing: None,
                },
                output: MediaAudioDevice {
                    name: "Speakers".into(),
                    missing: Some("Preferred speakers".into()),
                },
            }),
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "media_offer", "session_id": "m", "sdp": "v=0",
                "audio_devices": {
                    "input": {"name": "Mic"},
                    "output": {"name": "Speakers", "missing": "Preferred speakers"}
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<ResponseResult>(value).unwrap(),
            result
        );
    }

    #[test]
    fn media_offer_json_schema_exposes_optional_devices() {
        fn offer_schema(value: &serde_json::Value) -> Option<&serde_json::Value> {
            if value
                .get("properties")
                .and_then(|properties| properties.get("audio_devices"))
                .is_some()
            {
                return Some(value);
            }
            match value {
                serde_json::Value::Object(object) => object.values().find_map(offer_schema),
                serde_json::Value::Array(array) => array.iter().find_map(offer_schema),
                _ => None,
            }
        }
        let schema = serde_json::to_value(schemars::schema_for!(SuccessResponse)).unwrap();
        let offer = offer_schema(&schema).expect("the response schema includes offer devices");
        assert!(!offer["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("audio_devices")));
        let devices = &schema["$defs"]["MediaAudioDevices"];
        assert!(devices["properties"].get("input").is_some());
        assert!(devices["properties"].get("output").is_some());
        let device = &schema["$defs"]["MediaAudioDevice"];
        assert_eq!(device["required"], serde_json::json!(["name"]));
        assert!(device["properties"].get("missing").is_some());
    }
}
