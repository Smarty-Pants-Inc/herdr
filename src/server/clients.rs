use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::protocol::RenderEncoding;
use crate::server::client_transport::ClientWriter;
use crate::server::render_stream::ClientRenderState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientConnectionMode {
    App,
    TerminalAttach {
        terminal_id: String,
    },
    TerminalObserve {
        terminal_id: String,
    },
    /// OMP sideband bridge. Deliberately excluded from terminal/UI ownership.
    OmpPane,
}

pub(crate) type RenderTarget = (
    u64,
    (u16, u16),
    crate::kitty_graphics::HostCellSize,
    bool,
    ClientConnectionMode,
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DeferredRender {
    #[default]
    None,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OmpRendererTargetState {
    pub(crate) launch_id: u64,
    pub(crate) route: Option<crate::protocol::OmpRendererRoute>,
    pub(crate) bound: bool,
    pub(crate) ready: bool,
    pub(crate) prefix: crate::protocol::OmpRendererPrefix,
    pub(crate) surface_active: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublicPaneFocusTarget {
    workspace_id: String,
    pane_id: String,
}

/// Client-local projection of navigation fields stored in the shared app model.
///
/// Only stable public identities cross projection boundaries. Runtime topology,
/// terminal state, overlays, selection, and host-terminal concerns remain shared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientNavigationState {
    pub(crate) active_workspace_id: Option<String>,
    pub(crate) selected_workspace_id: Option<String>,
    pub(crate) active_tab_by_workspace: HashMap<String, String>,
    pub(crate) focused_pane_by_tab: HashMap<String, String>,
    pub(crate) previous_pane_by_tab: HashMap<String, String>,
    pub(crate) previous_pane_focus: Option<PublicPaneFocusTarget>,
    pub(crate) zoomed_tabs: HashSet<String>,
    pub(crate) focused_workspace_plugin_pane: Option<String>,
}

impl ClientNavigationState {
    pub(crate) fn capture(state: &crate::app::state::AppState) -> Self {
        let active_workspace_id = state
            .active
            .and_then(|ws_idx| state.workspaces.get(ws_idx))
            .map(|workspace| workspace.id.clone());
        let selected_workspace_id = state
            .workspaces
            .get(state.selected)
            .map(|workspace| workspace.id.clone());
        let mut active_tab_by_workspace = HashMap::new();
        let mut focused_pane_by_tab = HashMap::new();
        let mut previous_pane_by_tab = HashMap::new();
        let mut zoomed_tabs = HashSet::new();

        for workspace in &state.workspaces {
            if let Some(tab_id) = public_tab_id(workspace, workspace.active_tab) {
                active_tab_by_workspace.insert(workspace.id.clone(), tab_id);
            }
            for (tab_idx, tab) in workspace.tabs.iter().enumerate() {
                let Some(tab_id) = public_tab_id(workspace, tab_idx) else {
                    continue;
                };
                if let Some(pane_id) = public_pane_id(workspace, tab.layout.focused()) {
                    focused_pane_by_tab.insert(tab_id.clone(), pane_id);
                }
                if let Some(pane_id) = tab
                    .layout
                    .previous_focus()
                    .and_then(|pane_id| public_pane_id(workspace, pane_id))
                {
                    previous_pane_by_tab.insert(tab_id.clone(), pane_id);
                }
                if tab.zoomed {
                    zoomed_tabs.insert(tab_id);
                }
            }
        }

        let previous_pane_focus = state.previous_pane_focus.as_ref().and_then(|target| {
            let workspace = state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == target.workspace_id)?;
            Some(PublicPaneFocusTarget {
                workspace_id: workspace.id.clone(),
                pane_id: public_pane_id(workspace, target.pane_id)?,
            })
        });
        let focused_workspace_plugin_pane = state.workspaces.iter().find_map(|workspace| {
            state
                .workspace_plugin_panes
                .get(&workspace.id)
                .filter(|pane| pane.focused)
                .map(|_| format!("{}:plugin", workspace.id))
        });

        Self {
            active_workspace_id,
            selected_workspace_id,
            active_tab_by_workspace,
            focused_pane_by_tab,
            previous_pane_by_tab,
            previous_pane_focus,
            zoomed_tabs,
            focused_workspace_plugin_pane,
        }
    }

    /// Drop stale identities and fill new topology from the canonical projection.
    pub(crate) fn reconciled(&self, state: &crate::app::state::AppState, canonical: &Self) -> Self {
        let current = Self::capture(state);
        let active_workspace_id = valid_workspace_id(state, self.active_workspace_id.as_ref())
            .or_else(|| valid_workspace_id(state, canonical.active_workspace_id.as_ref()))
            .or(current.active_workspace_id.clone());
        let selected_workspace_id = valid_workspace_id(state, self.selected_workspace_id.as_ref())
            .or_else(|| valid_workspace_id(state, canonical.selected_workspace_id.as_ref()))
            .or_else(|| active_workspace_id.clone())
            .or(current.selected_workspace_id.clone());
        let mut active_tab_by_workspace = HashMap::new();
        let mut focused_pane_by_tab = HashMap::new();
        let mut previous_pane_by_tab = HashMap::new();
        let mut zoomed_tabs = HashSet::new();

        for workspace in &state.workspaces {
            let active_tab_id = self
                .active_tab_by_workspace
                .get(&workspace.id)
                .and_then(|id| valid_tab_id(workspace, id))
                .or_else(|| {
                    canonical
                        .active_tab_by_workspace
                        .get(&workspace.id)
                        .and_then(|id| valid_tab_id(workspace, id))
                })
                .or_else(|| current.active_tab_by_workspace.get(&workspace.id).cloned());
            if let Some(active_tab_id) = active_tab_id {
                active_tab_by_workspace.insert(workspace.id.clone(), active_tab_id);
            }

            for (tab_idx, _) in workspace.tabs.iter().enumerate() {
                let Some(tab_id) = public_tab_id(workspace, tab_idx) else {
                    continue;
                };
                let focused_pane_id = self
                    .focused_pane_by_tab
                    .get(&tab_id)
                    .and_then(|id| valid_pane_id(workspace, tab_idx, id))
                    .or_else(|| {
                        canonical
                            .focused_pane_by_tab
                            .get(&tab_id)
                            .and_then(|id| valid_pane_id(workspace, tab_idx, id))
                    })
                    .or_else(|| current.focused_pane_by_tab.get(&tab_id).cloned());
                if let Some(focused_pane_id) = focused_pane_id {
                    focused_pane_by_tab.insert(tab_id.clone(), focused_pane_id);
                }
                let previous_pane_id = if self.focused_pane_by_tab.contains_key(&tab_id) {
                    self.previous_pane_by_tab
                        .get(&tab_id)
                        .and_then(|id| valid_pane_id(workspace, tab_idx, id))
                } else if canonical.focused_pane_by_tab.contains_key(&tab_id) {
                    canonical
                        .previous_pane_by_tab
                        .get(&tab_id)
                        .and_then(|id| valid_pane_id(workspace, tab_idx, id))
                } else {
                    current.previous_pane_by_tab.get(&tab_id).cloned()
                };
                if let Some(previous_pane_id) = previous_pane_id {
                    previous_pane_by_tab.insert(tab_id.clone(), previous_pane_id);
                }

                let zoomed = if self.focused_pane_by_tab.contains_key(&tab_id) {
                    self.zoomed_tabs.contains(&tab_id)
                } else if canonical.focused_pane_by_tab.contains_key(&tab_id) {
                    canonical.zoomed_tabs.contains(&tab_id)
                } else {
                    current.zoomed_tabs.contains(&tab_id)
                };
                if zoomed {
                    zoomed_tabs.insert(tab_id);
                }
            }
        }

        let previous_pane_focus = valid_previous_focus(state, self.previous_pane_focus.as_ref())
            .or_else(|| valid_previous_focus(state, canonical.previous_pane_focus.as_ref()))
            .or(current.previous_pane_focus);
        let focused_workspace_plugin_pane =
            valid_workspace_plugin_pane(state, self.focused_workspace_plugin_pane.as_ref())
                .or_else(|| {
                    valid_workspace_plugin_pane(
                        state,
                        canonical.focused_workspace_plugin_pane.as_ref(),
                    )
                })
                .or(current.focused_workspace_plugin_pane);

        Self {
            active_workspace_id,
            selected_workspace_id,
            active_tab_by_workspace,
            focused_pane_by_tab,
            previous_pane_by_tab,
            previous_pane_focus,
            zoomed_tabs,
            focused_workspace_plugin_pane,
        }
    }

    /// Apply only navigation projection fields, without focus events or persistence effects.
    pub(crate) fn apply_to(
        &self,
        state: &mut crate::app::state::AppState,
    ) -> ClientNavigationState {
        let canonical = Self::capture(state);
        let reconciled = self.reconciled(state, &canonical);

        state.active = reconciled
            .active_workspace_id
            .as_ref()
            .and_then(|id| workspace_index(state, id));
        state.selected = reconciled
            .selected_workspace_id
            .as_ref()
            .and_then(|id| workspace_index(state, id))
            .or(state.active)
            .unwrap_or(0);

        for workspace in &mut state.workspaces {
            if let Some(tab_idx) = reconciled
                .active_tab_by_workspace
                .get(&workspace.id)
                .and_then(|id| tab_index(workspace, id))
            {
                workspace.active_tab = tab_idx;
            }
            for tab_idx in 0..workspace.tabs.len() {
                let Some(tab_id) = public_tab_id(workspace, tab_idx) else {
                    continue;
                };
                let focused_pane_id = reconciled
                    .focused_pane_by_tab
                    .get(&tab_id)
                    .and_then(|id| pane_id(workspace, tab_idx, id));
                let previous_pane_id = reconciled
                    .previous_pane_by_tab
                    .get(&tab_id)
                    .and_then(|id| pane_id(workspace, tab_idx, id));
                if let Some(focused_pane_id) = focused_pane_id {
                    workspace.tabs[tab_idx]
                        .layout
                        .project_focus_state(focused_pane_id, previous_pane_id);
                }
                workspace.tabs[tab_idx].zoomed = reconciled.zoomed_tabs.contains(&tab_id);
            }
        }

        state.previous_pane_focus = reconciled.previous_pane_focus.as_ref().and_then(|target| {
            let ws_idx = workspace_index(state, &target.workspace_id)?;
            let workspace = state.workspaces.get(ws_idx)?;
            let (_, pane_id) = pane_location(workspace, &target.pane_id)?;
            Some(crate::app::state::PaneFocusTarget {
                workspace_id: target.workspace_id.clone(),
                pane_id,
            })
        });
        for pane in state.workspace_plugin_panes.values_mut() {
            pane.focused = false;
        }
        if let Some(plugin_id) = reconciled.focused_workspace_plugin_pane.as_ref() {
            if let Some(workspace_id) = plugin_id.strip_suffix(":plugin") {
                if let Some(pane) = state.workspace_plugin_panes.get_mut(workspace_id) {
                    pane.focused = true;
                }
            }
        }

        reconciled
    }
}

fn public_tab_id(workspace: &crate::workspace::Workspace, tab_idx: usize) -> Option<String> {
    Some(crate::workspace::public_tab_id_for_number(
        &workspace.id,
        workspace.public_tab_number(tab_idx)?,
    ))
}

fn public_pane_id(
    workspace: &crate::workspace::Workspace,
    pane_id: crate::layout::PaneId,
) -> Option<String> {
    Some(crate::workspace::public_pane_id_for_number(
        &workspace.id,
        workspace.public_pane_number(pane_id)?,
    ))
}

fn workspace_index(state: &crate::app::state::AppState, workspace_id: &str) -> Option<usize> {
    state
        .workspaces
        .iter()
        .position(|workspace| workspace.id == workspace_id)
}

fn valid_workspace_id(
    state: &crate::app::state::AppState,
    workspace_id: Option<&String>,
) -> Option<String> {
    let workspace_id = workspace_id?;
    workspace_index(state, workspace_id).map(|_| workspace_id.clone())
}

fn tab_index(workspace: &crate::workspace::Workspace, tab_id: &str) -> Option<usize> {
    workspace.tabs.iter().enumerate().find_map(|(tab_idx, _)| {
        (public_tab_id(workspace, tab_idx)?.as_str() == tab_id).then_some(tab_idx)
    })
}

fn valid_tab_id(workspace: &crate::workspace::Workspace, tab_id: &str) -> Option<String> {
    tab_index(workspace, tab_id).map(|_| tab_id.to_owned())
}

fn pane_id(
    workspace: &crate::workspace::Workspace,
    tab_idx: usize,
    public_id: &str,
) -> Option<crate::layout::PaneId> {
    let tab = workspace.tabs.get(tab_idx)?;
    tab.layout
        .pane_ids()
        .into_iter()
        .find(|&pane_id| public_pane_id(workspace, pane_id).as_deref() == Some(public_id))
}

fn pane_location(
    workspace: &crate::workspace::Workspace,
    public_id: &str,
) -> Option<(usize, crate::layout::PaneId)> {
    workspace.tabs.iter().enumerate().find_map(|(tab_idx, _)| {
        pane_id(workspace, tab_idx, public_id).map(|pane_id| (tab_idx, pane_id))
    })
}

fn valid_pane_id(
    workspace: &crate::workspace::Workspace,
    tab_idx: usize,
    public_id: &str,
) -> Option<String> {
    pane_id(workspace, tab_idx, public_id).map(|_| public_id.to_owned())
}

fn valid_previous_focus(
    state: &crate::app::state::AppState,
    target: Option<&PublicPaneFocusTarget>,
) -> Option<PublicPaneFocusTarget> {
    let target = target?;
    let workspace = state
        .workspaces
        .iter()
        .find(|workspace| workspace.id == target.workspace_id)?;
    pane_location(workspace, &target.pane_id)?;
    Some(target.clone())
}

fn valid_workspace_plugin_pane(
    state: &crate::app::state::AppState,
    pane_id: Option<&String>,
) -> Option<String> {
    let pane_id = pane_id?;
    let workspace_id = pane_id.strip_suffix(":plugin")?;
    (workspace_index(state, workspace_id).is_some()
        && state.workspace_plugin_panes.contains_key(workspace_id))
    .then(|| pane_id.clone())
}

/// A committed display-only identity snapshot for one App connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedIdentity {
    pub(crate) name: String,
    pub(crate) revision: u64,
}

/// The exact client-local persistence request currently awaiting an acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingIdentityPersistence {
    pub(crate) request_id: u64,
    pub(crate) name: String,
}

/// Client-local persistence work requested by this server connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityPersistenceRequest {
    pub(crate) request_id: u64,
    pub(crate) display_name: String,
}

/// Native identity editor state, isolated to one App connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityEditor {
    pub(crate) open: bool,
    pub(crate) draft: String,
    /// Cursor position measured in Unicode scalar values.
    pub(crate) cursor: usize,
    pub(crate) error: Option<String>,
}

/// Per-App identity state. It is attribution only, never an authority token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppIdentity {
    pub(crate) committed: Option<CommittedIdentity>,
    pub(crate) editor: IdentityEditor,
    pub(crate) pending: Option<PendingIdentityPersistence>,
}

impl AppIdentity {
    pub(crate) fn new(committed_name: Option<String>) -> Self {
        let draft = committed_name.clone().unwrap_or_default();
        let cursor = draft.chars().count();
        Self {
            committed: committed_name.map(|name| CommittedIdentity { name, revision: 1 }),
            editor: IdentityEditor {
                open: false,
                draft,
                cursor,
                error: None,
            },
            pending: None,
        }
    }

    pub(crate) fn open_editor(&mut self) {
        if !self.editor.open {
            self.editor.draft = self
                .committed
                .as_ref()
                .map_or_else(String::new, |identity| identity.name.clone());
            self.editor.cursor = self.editor.draft.chars().count();
            self.editor.error = None;
            self.editor.open = true;
        }
    }

    pub(crate) fn cancel_editor(&mut self) {
        if self.pending.is_none() {
            self.editor.open = false;
            self.editor.error = None;
        }
    }

    pub(crate) fn insert_editor_text(&mut self, text: &str) {
        let offset = scalar_byte_offset(&self.editor.draft, self.editor.cursor);
        self.editor.draft.insert_str(offset, text);
        self.editor.cursor += text.chars().count();
        self.editor.error = None;
    }

    pub(crate) fn backspace_editor(&mut self) {
        if self.editor.cursor > 0 {
            let end = scalar_byte_offset(&self.editor.draft, self.editor.cursor);
            let start = scalar_byte_offset(&self.editor.draft, self.editor.cursor - 1);
            self.editor.draft.replace_range(start..end, "");
            self.editor.cursor -= 1;
            self.editor.error = None;
        }
    }

    pub(crate) fn delete_editor(&mut self) {
        let count = self.editor.draft.chars().count();
        if self.editor.cursor < count {
            let start = scalar_byte_offset(&self.editor.draft, self.editor.cursor);
            let end = scalar_byte_offset(&self.editor.draft, self.editor.cursor + 1);
            self.editor.draft.replace_range(start..end, "");
            self.editor.error = None;
        }
    }

    pub(crate) fn move_editor_left(&mut self) {
        self.editor.cursor = self.editor.cursor.saturating_sub(1);
    }
    pub(crate) fn move_editor_right(&mut self) {
        self.editor.cursor = (self.editor.cursor + 1).min(self.editor.draft.chars().count());
    }
    pub(crate) fn move_editor_home(&mut self) {
        self.editor.cursor = 0;
    }
    pub(crate) fn move_editor_end(&mut self) {
        self.editor.cursor = self.editor.draft.chars().count();
    }

    /// Begins only one exact local persistence operation; current attribution remains committed.
    pub(crate) fn begin_save(&mut self, request_id: u64) -> Option<IdentityPersistenceRequest> {
        if self.pending.is_some() {
            return None;
        }
        if let Err(error) = crate::config::validate_display_name(&self.editor.draft) {
            self.editor.error = Some(error.to_string());
            return None;
        }
        let name = self.editor.draft.clone();
        self.pending = Some(PendingIdentityPersistence {
            request_id,
            name: name.clone(),
        });
        Some(IdentityPersistenceRequest {
            request_id,
            display_name: name,
        })
    }

    /// Applies only an acknowledgement for the currently pending request and exact value.
    pub(crate) fn apply_persistence_ack(
        &mut self,
        request_id: u64,
        display_name: &str,
        result: Result<(), String>,
    ) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.request_id != request_id || pending.name != display_name {
            return false;
        }
        self.pending = None;
        match result {
            Ok(()) => {
                let revision = self
                    .committed
                    .as_ref()
                    .map_or(1, |identity| identity.revision + 1);
                self.committed = Some(CommittedIdentity {
                    name: display_name.to_owned(),
                    revision,
                });
                self.editor.draft = display_name.to_owned();
                self.editor.cursor = self.editor.draft.chars().count();
                self.editor.error = None;
                self.editor.open = false;
            }
            Err(error) => self.editor.error = Some(error),
        }
        true
    }
}

fn scalar_byte_offset(value: &str, scalar_offset: usize) -> usize {
    value
        .char_indices()
        .nth(scalar_offset)
        .map_or(value.len(), |(offset, _)| offset)
}

/// A connected client tracked by the server.
pub(crate) struct ClientConnection {
    /// Whether this connection is the full app client or a direct terminal attach.
    pub(crate) mode: ClientConnectionMode,
    /// True after the handshake for clients that will switch into direct terminal attach mode.
    pub(crate) pending_terminal_attach: bool,
    /// Client-local stable-ID navigation projection for full-app connections.
    pub(crate) navigation: Option<ClientNavigationState>,
    /// Client-local app keybindings. None means use the server's keybindings.
    pub(crate) keybindings: Option<Box<crate::config::LiveKeybindConfig>>,
    /// The client's terminal size after clamping.
    pub(crate) terminal_size: (u16, u16),
    /// Pixel size of one client terminal cell.
    pub(crate) cell_size: crate::kitty_graphics::HostCellSize,
    /// Last known host terminal default colors for this client.
    pub(crate) host_terminal_theme: crate::terminal_theme::TerminalTheme,
    /// Last known host terminal appearance for this client.
    pub(crate) host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    /// True when appearance came from an explicit host color-scheme report.
    pub(crate) host_terminal_appearance_explicit: bool,
    /// Last reported focus state for this client's outer terminal.
    pub(crate) outer_terminal_focus: Option<bool>,
    /// Stateful parser for app-client input split across transport reads.
    pub(crate) raw_input: crate::raw_input::RawInputFramer,
    /// Monotonic activity stamp used to choose the fallback foreground client.
    pub(crate) last_activity: u64,
    /// Render baseline for the negotiated client encoding.
    pub(crate) render_state: ClientRenderState,
    /// Client-local host Kitty graphics cache.
    pub(crate) graphics_cache: crate::kitty_graphics::HostGraphicsCache,
    /// Passive eligibility for audited local Kitty regular-file graphics.
    pub(crate) direct_graphics: bool,
    /// Whether this frontend preserves exact SGR pixel reports.
    pub(crate) pixel_mouse: bool,
    /// Whether the next graphics frame must clear and rebuild host-side Kitty state.
    pub(crate) graphics_surface_reset_pending: bool,
    /// Whether an ordinary render was skipped because the render channel was full.
    pub(crate) render_pending: bool,
    /// Last host mouse capture mode sent to this client.
    pub(crate) host_mouse_capture_active: Option<bool>,
    /// Last SGR pixel provenance mode sent to this client.
    pub(crate) host_sgr_pixels_active: Option<bool>,
    /// Last Kitty report-all mode sent to this client's host terminal.
    pub(crate) host_keyboard_report_all_active: Option<bool>,
    /// Temporary files staged from this client's local clipboard image pastes.
    pub(crate) staged_clipboard_files: Vec<PathBuf>,
    /// Channels for sending framed ServerMessage data to the client writer thread.
    pub(crate) writer: Option<ClientWriter>,
    /// Opaque client-local correlation identifier; never used for authority.
    pub(crate) frontend_profile_id: Option<String>,
    /// Client-local high-entropy capability used only for native renderer binding.
    pub(crate) renderer_binding_token: Option<String>,
    /// Presentation-only native renderer capability advertised by this App.
    pub(crate) omp_renderer_capabilities: crate::protocol::OmpRendererCapabilities,
    /// Last server-authoritative native renderer target sent to this App.
    pub(crate) omp_renderer_target: Option<OmpRendererTargetState>,
    /// App-only display identity and local editor state.
    pub(crate) identity: Option<AppIdentity>,
    /// Standalone server-owned OMP guest PTY scoped to this App connection.
    pub(crate) private_omp_guest: Option<crate::server::omp_private_renderer::PrivateOmpGuest>,
}

impl ClientConnection {
    #[cfg(test)]
    pub(crate) fn new(
        terminal_size: (u16, u16),
        cell_size: crate::kitty_graphics::HostCellSize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        outer_terminal_focus: Option<bool>,
        last_activity: u64,
        render_encoding: RenderEncoding,
        writer: Option<ClientWriter>,
    ) -> Self {
        Self::new_with_mode(
            ClientConnectionMode::App,
            None,
            None,
            None,
            None,
            terminal_size,
            cell_size,
            host_terminal_theme,
            outer_terminal_focus,
            last_activity,
            render_encoding,
            false,
            writer,
        )
    }

    pub(crate) fn new_with_mode(
        mode: ClientConnectionMode,
        keybindings: Option<Box<crate::config::LiveKeybindConfig>>,
        display_name: Option<String>,
        frontend_profile_id: Option<String>,
        renderer_binding_token: Option<String>,
        terminal_size: (u16, u16),
        cell_size: crate::kitty_graphics::HostCellSize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        outer_terminal_focus: Option<bool>,
        last_activity: u64,
        render_encoding: RenderEncoding,
        pending_terminal_attach: bool,
        writer: Option<ClientWriter>,
    ) -> Self {
        let identity =
            matches!(mode, ClientConnectionMode::App).then(|| AppIdentity::new(display_name));
        Self {
            mode,
            pending_terminal_attach,
            navigation: None,
            keybindings,
            frontend_profile_id,
            renderer_binding_token,
            identity,
            omp_renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            omp_renderer_target: None,
            private_omp_guest: None,
            terminal_size,
            cell_size,
            host_terminal_appearance: host_terminal_theme
                .background
                .map(crate::terminal_theme::RgbColor::inferred_appearance),
            host_terminal_appearance_explicit: false,
            host_terminal_theme,
            outer_terminal_focus,
            raw_input: crate::raw_input::RawInputFramer::default(),
            last_activity,
            render_state: ClientRenderState::new(render_encoding),
            graphics_cache: crate::kitty_graphics::HostGraphicsCache::default(),
            direct_graphics: false,
            pixel_mouse: false,
            graphics_surface_reset_pending: false,
            render_pending: false,
            host_mouse_capture_active: None,
            host_sgr_pixels_active: None,
            host_keyboard_report_all_active: None,
            staged_clipboard_files: Vec::new(),
            writer,
        }
    }
    pub(crate) fn committed_identity(&self) -> Option<&CommittedIdentity> {
        self.identity.as_ref()?.committed.as_ref()
    }

    pub(crate) fn request_repaint(&mut self) {
        self.render_state.request_repaint();
    }

    pub(crate) fn deferred_render(&self) -> DeferredRender {
        if self.render_pending {
            DeferredRender::Full
        } else {
            DeferredRender::None
        }
    }

    pub(crate) fn clear_deferred_render(&mut self) {
        self.render_pending = false;
    }

    pub(crate) fn defer_full_render(&mut self) {
        self.render_pending = true;
    }

    pub(crate) fn take_deferred_render(&mut self) -> DeferredRender {
        let deferred = self.deferred_render();
        self.clear_deferred_render();
        deferred
    }

    pub(crate) fn is_full_app_client(&self) -> bool {
        matches!(self.mode, ClientConnectionMode::App) && !self.pending_terminal_attach
    }

    pub(crate) fn request_semantic_redraw_after_input(&mut self) {
        self.render_state.reset_semantic_input_baseline();
    }

    pub(crate) fn update_host_theme_from_events(
        &mut self,
        events: &[crate::raw_input::RawInputEvent],
    ) -> bool {
        let mut next_theme = self.host_terminal_theme;
        let mut changed = false;
        for event in events {
            match event {
                crate::raw_input::RawInputEvent::HostDefaultColor { kind, color } => {
                    next_theme = next_theme.with_color(*kind, *color);
                    if matches!(kind, crate::terminal_theme::DefaultColorKind::Background)
                        && !self.host_terminal_appearance_explicit
                    {
                        changed |=
                            self.set_host_appearance(Some(color.inferred_appearance()), false);
                    }
                }
                crate::raw_input::RawInputEvent::HostPaletteColors { colors } => {
                    for &(index, color) in colors {
                        next_theme = next_theme.with_palette_color(index, color);
                    }
                }
                crate::raw_input::RawInputEvent::HostColorSchemeChanged(appearance) => {
                    changed |= self.set_host_appearance(Some(*appearance), true);
                }
                _ => {}
            }
        }

        if next_theme != self.host_terminal_theme {
            self.host_terminal_theme = next_theme;
            changed = true;
        }
        changed
    }

    fn set_host_appearance(
        &mut self,
        appearance: Option<crate::terminal_theme::HostAppearance>,
        explicit: bool,
    ) -> bool {
        if self.host_terminal_appearance_explicit && !explicit {
            return false;
        }
        if self.host_terminal_appearance == appearance
            && self.host_terminal_appearance_explicit == explicit
        {
            return false;
        }
        self.host_terminal_appearance = appearance;
        self.host_terminal_appearance_explicit = explicit;
        true
    }

    pub(crate) fn update_outer_focus_from_events(
        &mut self,
        events: &[crate::raw_input::RawInputEvent],
    ) -> Option<bool> {
        let next_focus = events
            .iter()
            .filter_map(|event| match event {
                crate::raw_input::RawInputEvent::OuterFocusGained => Some(true),
                crate::raw_input::RawInputEvent::OuterFocusLost => Some(false),
                _ => None,
            })
            .next_back()?;

        self.outer_terminal_focus = Some(next_focus);
        Some(next_focus)
    }
}

pub(crate) fn events_include_interaction(events: &[crate::raw_input::RawInputEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            crate::raw_input::RawInputEvent::Key(_)
                | crate::raw_input::RawInputEvent::Text(_)
                | crate::raw_input::RawInputEvent::Mouse(_)
                | crate::raw_input::RawInputEvent::Paste(_)
                | crate::raw_input::RawInputEvent::OuterFocusGained
        )
    })
}

pub(crate) fn latest_app_client(clients: &HashMap<u64, ClientConnection>) -> Option<u64> {
    clients
        .iter()
        .filter(|(_, client)| client.is_full_app_client())
        .max_by_key(|(_, client)| client.last_activity)
        .map(|(&client_id, _)| client_id)
}

pub(crate) fn terminal_stream_client_ids(
    clients: &HashMap<u64, ClientConnection>,
    terminal_id: &str,
) -> Vec<u64> {
    clients
        .iter()
        .filter_map(|(&client_id, client)| match &client.mode {
            ClientConnectionMode::TerminalAttach {
                terminal_id: attached,
            }
            | ClientConnectionMode::TerminalObserve {
                terminal_id: attached,
            } if attached == terminal_id => Some(client_id),
            _ => None,
        })
        .collect()
}

pub(crate) fn render_targets(
    clients: &HashMap<u64, ClientConnection>,
    foreground_client_id: Option<u64>,
) -> Vec<RenderTarget> {
    let mut targets: Vec<RenderTarget> = clients
        .iter()
        .filter(|(_, client)| {
            client.writer.is_some()
                && (client.is_full_app_client()
                    || matches!(
                        client.mode,
                        ClientConnectionMode::TerminalAttach { .. }
                            | ClientConnectionMode::TerminalObserve { .. }
                    ))
        })
        .map(|(&client_id, client)| {
            (
                client_id,
                client.terminal_size,
                client.cell_size,
                foreground_client_id == Some(client_id),
                client.mode.clone(),
            )
        })
        .collect();

    targets.sort_by_key(|(client_id, _, _, is_foreground, _)| (*is_foreground, *client_id));
    targets
}

#[cfg(test)]
mod navigation_tests {
    use super::ClientNavigationState;
    use ratatui::layout::Direction;

    fn navigation_state() -> crate::app::state::AppState {
        let mut state = crate::app::state::AppState::test_new();
        let mut first = crate::workspace::Workspace::test_new("first");
        let first_root = first.tabs[0].root_pane;
        first.test_split(Direction::Horizontal);
        first.tabs[0].layout.focus_pane(first_root);
        first.tabs[0].zoomed = true;
        let second_tab = first.test_add_tab(Some("second tab"));
        first.active_tab = second_tab;

        let second = crate::workspace::Workspace::test_new("second");
        let second_root = second.tabs[0].root_pane;
        let second_id = second.id.clone();
        state.workspaces = vec![first, second];
        state.active = Some(0);
        state.selected = 1;
        state.previous_pane_focus = Some(crate::app::state::PaneFocusTarget {
            workspace_id: second_id.clone(),
            pane_id: second_root,
        });
        state.workspace_plugin_panes.insert(
            second_id,
            crate::app::state::WorkspacePluginPaneState {
                pane_id: crate::layout::PaneId::alloc(),
                terminal_id: crate::terminal::TerminalId::alloc(),
                plugin_id: "test".into(),
                entrypoint: "panel".into(),
                width: None,
                focused: true,
                collapsed: false,
            },
        );
        state
    }

    #[test]
    fn navigation_snapshot_survives_reordering_without_dirtying_session() {
        let mut state = navigation_state();
        state.session_dirty = false;
        let snapshot = ClientNavigationState::capture(&state);

        state.workspaces.swap(0, 1);
        state.active = Some(0);
        state.selected = 0;
        for workspace in &mut state.workspaces {
            workspace.active_tab = 0;
            for tab in &mut workspace.tabs {
                let root_pane = tab.root_pane;
                tab.layout.project_focus_state(root_pane, None);
                tab.zoomed = false;
            }
        }
        state.previous_pane_focus = None;
        for pane in state.workspace_plugin_panes.values_mut() {
            pane.focused = false;
        }

        let applied = snapshot.apply_to(&mut state);
        assert_eq!(applied, snapshot);
        assert_eq!(ClientNavigationState::capture(&state), snapshot);
        assert!(!state.session_dirty);
    }

    #[test]
    fn stale_navigation_ids_fall_back_to_canonical_topology() {
        let mut state = navigation_state();
        let stale = ClientNavigationState::capture(&state);
        state.workspaces.remove(0);
        state.active = Some(0);
        state.selected = 0;
        state.previous_pane_focus = None;
        let canonical = ClientNavigationState::capture(&state);

        let reconciled = stale.reconciled(&state, &canonical);
        reconciled.apply_to(&mut state);

        assert_eq!(
            reconciled.active_workspace_id,
            canonical.active_workspace_id
        );
        assert_eq!(
            reconciled.selected_workspace_id,
            canonical.selected_workspace_id
        );
        assert_eq!(ClientNavigationState::capture(&state), reconciled);
        assert!(reconciled
            .active_tab_by_workspace
            .keys()
            .all(|workspace_id| workspace_id.as_str() == state.workspaces[0].id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persistence_ack_commits_only_the_exact_pending_request_and_value() {
        let mut identity = AppIdentity::new(Some("Ada".into()));
        identity.open_editor();
        identity.editor.draft = "Grace".into();
        let request = identity
            .begin_save(7)
            .expect("valid name starts persistence");
        assert_eq!(request.display_name, "Grace");
        assert_eq!(
            identity.committed.as_ref().map(|name| name.name.as_str()),
            Some("Ada")
        );

        assert!(!identity.apply_persistence_ack(8, "Grace", Ok(())));
        assert!(!identity.apply_persistence_ack(7, "Ada", Ok(())));
        assert_eq!(
            identity.committed.as_ref().map(|name| name.name.as_str()),
            Some("Ada")
        );
        assert_eq!(
            identity.pending.as_ref().map(|pending| pending.request_id),
            Some(7)
        );

        assert!(identity.apply_persistence_ack(7, "Grace", Err("disk full".into())));
        assert_eq!(
            identity.committed.as_ref().map(|name| name.name.as_str()),
            Some("Ada")
        );
        assert!(identity.pending.is_none());
        assert_eq!(identity.editor.error.as_deref(), Some("disk full"));

        let retry = identity
            .begin_save(9)
            .expect("failed persistence can retry");
        assert_eq!(retry.display_name, "Grace");
        assert!(identity.apply_persistence_ack(9, "Grace", Ok(())));
        assert_eq!(
            identity.committed,
            Some(CommittedIdentity {
                name: "Grace".into(),
                revision: 2
            })
        );
        assert!(!identity.editor.open);
    }

    #[test]
    fn identities_keep_editor_drafts_and_modals_per_client() {
        let mut first = AppIdentity::new(Some("Ada".into()));
        let second = AppIdentity::new(Some("Grace".into()));
        first.open_editor();
        first.editor.draft = "Lin".into();

        assert!(first.editor.open);
        assert_eq!(first.editor.draft, "Lin");
        assert!(!second.editor.open);
        assert_eq!(second.editor.draft, "Grace");
        assert_eq!(
            second.committed.as_ref().map(|name| name.name.as_str()),
            Some("Grace")
        );
    }

    #[test]
    fn app_connections_keep_same_named_identities_and_profiles_independent() {
        let theme = crate::terminal_theme::TerminalTheme::default();
        let first = ClientConnection::new_with_mode(
            ClientConnectionMode::App,
            None,
            Some("Ada".into()),
            Some("profile-one".into()),
            Some("binding-one".into()),
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            theme,
            None,
            1,
            RenderEncoding::SemanticFrame,
            false,
            None,
        );
        let second = ClientConnection::new_with_mode(
            ClientConnectionMode::App,
            None,
            Some("Ada".into()),
            Some("profile-two".into()),
            Some("binding-two".into()),
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            theme,
            None,
            2,
            RenderEncoding::SemanticFrame,
            false,
            None,
        );

        assert_eq!(
            first
                .committed_identity()
                .map(|identity| identity.name.as_str()),
            Some("Ada")
        );
        assert_eq!(
            second
                .committed_identity()
                .map(|identity| identity.name.as_str()),
            Some("Ada")
        );
        assert_ne!(first.frontend_profile_id, second.frontend_profile_id);
    }
}
