pub mod client;
mod event_hub;
pub mod schema;
mod server;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use server::handle_connection as test_handle_connection;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
#[cfg(unix)]
pub(crate) use server::start_server_at_with_stop_control;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::ServerReloadAgentManifests(_)
            | Method::NotificationShow(_)
            | Method::ProductAnnouncementDismiss(_)
            | Method::ReleaseNotesDismiss(_)
            | Method::CommandInvoke(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceCreateLinked(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceMove(_)
            | Method::WorkspaceMoveBlock(_)
            | Method::WorkspaceReportMetadata(_)
            | Method::WorkspaceClose(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeCreateProjectChecked(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeOpenProjectChecked(_)
            | Method::WorktreeRemove(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabMove(_)
            | Method::TabMoveProjectChecked(_)
            | Method::TabClose(_)
            | Method::LayoutApply(_)
            | Method::LayoutApplyRestorable(_)
            | Method::LayoutApplyProjectChecked(_)
            | Method::LayoutSetSplitRatio(_)
            | Method::AgentRename(_)
            | Method::AgentViewSet(_)
            | Method::AgentViewClear(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentStartGuarded(_)
            | Method::AgentPrompt(_)
            | Method::AgentPromptSessionChecked(_)
            | Method::AgentPromptStatusChecked(_)
            | Method::AgentSendKeys(_)
            | Method::PaneSplit(_)
            | Method::PaneSwap(_)
            | Method::PaneSwapProjectChecked(_)
            | Method::PaneMove(_)
            | Method::PaneMoveProjectChecked(_)
            | Method::PaneZoom(_)
            | Method::PaneFocusDirection(_)
            | Method::PaneResize(_)
            | Method::PaneScroll(_)
            | Method::PaneClear(_)
            | Method::PaneEditScrollback(_)
            | Method::PaneFocus(_)
            | Method::PaneInputSet(_)
            | Method::PaneRename(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
            | Method::PopupClose(_)
            | Method::PluginUnlink(_)
            | Method::PluginDisable(_)
            | Method::PluginActionInvoke(_)
            | Method::PluginPaneOpen(_)
            | Method::PluginPaneFocus(_)
            | Method::PluginPaneClose(_)
    )
}

/// Best-effort origin metadata carried alongside an API request.
///
/// This is transport-local context, not part of the JSON request schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ApiRequestContext {
    pub(crate) local_peer_identity: Option<crate::platform::ProcessIdentity>,
    /// Marker presence captured from the peer's initial OS environment.
    ///
    /// This is deliberately transport-local and never binds a public pane ID;
    /// the app must still validate live pane ancestry before granting an agent
    /// origin. A captured marker that loses that live relationship is unknown,
    /// not ordinary.
    pub(crate) local_peer_pane_origin: crate::platform::PeerPaneOrigin,
}

impl ApiRequestContext {
    /// Build the immutable accept-time context for a local peer.
    pub(crate) fn capture(local_peer_identity: Option<crate::platform::ProcessIdentity>) -> Self {
        let local_peer_pane_origin = local_peer_identity
            .map(crate::platform::process_initial_pane_origin)
            .unwrap_or(crate::platform::PeerPaneOrigin::Unknown);
        Self {
            local_peer_identity,
            local_peer_pane_origin,
        }
    }

    /// Test constructor for a captured numeric peer PID.
    #[cfg(test)]
    pub(crate) fn for_local_peer_pid(pid: Option<u32>) -> Self {
        Self::capture(pid.and_then(crate::platform::process_identity))
    }

    pub(crate) fn local_peer_pid(self) -> Option<u32> {
        self.local_peer_identity.map(|identity| identity.pid)
    }
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub(crate) context: ApiRequestContext,

    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod context_tests {
    use super::ApiRequestContext;

    #[test]
    fn queued_context_preserves_captured_marker_and_peer_pin() {
        let peer = crate::platform::ProcessIdentity {
            pid: 123,
            start_time: 456,
        };
        let captured = ApiRequestContext {
            local_peer_identity: Some(peer),
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::HasPane,
        };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        sender.send(captured).expect("queue captured context");
        let queued = receiver.try_recv().expect("queued context");
        assert_eq!(queued, captured);
        assert_eq!(queued.local_peer_identity, Some(peer));
        assert_eq!(
            queued.local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::HasPane
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn local_peer_context_pins_the_captured_instance() {
        let identity = crate::platform::process_identity(std::process::id())
            .expect("current process identity");
        let context = ApiRequestContext::for_local_peer_pid(Some(identity.pid));
        assert_eq!(context.local_peer_identity, Some(identity));
        assert_eq!(context.local_peer_pid(), Some(identity.pid));
        assert_eq!(
            ApiRequestContext::for_local_peer_pid(None).local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::Unknown
        );
        assert_eq!(
            ApiRequestContext::for_local_peer_pid(Some(0)).local_peer_pane_origin,
            crate::platform::PeerPaneOrigin::Unknown
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn exited_peer_context_keeps_its_original_instance() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("peer child");
        let context = ApiRequestContext::for_local_peer_pid(Some(child.id()));
        let identity = context.local_peer_identity.expect("live peer identity");
        drop(child.stdin.take());
        child.wait().expect("peer reaped");
        assert_ne!(
            crate::platform::process_identity(identity.pid),
            Some(identity)
        );
        assert_eq!(context.local_peer_identity, Some(identity));
        assert_eq!(context.local_peer_pid(), Some(identity.pid));
    }
}
