use std::sync::Arc;

use crate::render_signal::RenderSignal;

use bytes::Bytes;
use ratatui::{layout::Rect, Frame};
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;

/// Live runtime for a server-owned terminal.
///
/// The PTY implementation still delegates to the legacy pane runtime while the
/// migration proceeds, but production code now depends on this terminal-layer
/// type instead of the pane module's implementation detail.
pub struct TerminalRuntime(crate::pane::PaneRuntime);

const MAX_TRACKED_OMP_INPUT_PRESSES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OmpReplyNavigationRoute {
    Consumed { navigated: bool },
    Forwarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OmpReplyNavigationDisposition {
    Consumed,
    Forwarded,
    Suppressed,
}

#[derive(Clone, Debug)]
struct OmpReplyNavigationPress {
    identity: crate::input::KeyIdentity,
    key: crate::input::TerminalKey,
    disposition: OmpReplyNavigationDisposition,
}

// `reply_navigation_key_identity` admits only semantic Up and Down here; physical identities use
// `OmpPhysicalKeyPresses`, so this collection is inherently bounded at two entries.
#[derive(Default)]
pub(crate) struct OmpReplyNavigationPresses {
    presses: Vec<OmpReplyNavigationPress>,
}

impl OmpReplyNavigationPresses {
    pub(crate) fn route(
        &mut self,
        key: &crate::input::TerminalKey,
        mut navigate: impl FnMut() -> bool,
    ) -> OmpReplyNavigationRoute {
        if let Some(route) = self.route_existing_with(key, &mut navigate) {
            return route;
        }

        let identity = reply_navigation_key_identity(key);
        if matches!(
            key.kind,
            crossterm::event::KeyEventKind::Repeat | crossterm::event::KeyEventKind::Release
        ) && identity.is_some()
        {
            return OmpReplyNavigationRoute::Consumed { navigated: false };
        }

        let navigated = navigate();
        if let Some(identity) = identity {
            let disposition = if navigated {
                OmpReplyNavigationDisposition::Consumed
            } else {
                OmpReplyNavigationDisposition::Forwarded
            };
            self.presses.push(OmpReplyNavigationPress {
                identity,
                key: key.clone(),
                disposition,
            });
        }
        if navigated {
            OmpReplyNavigationRoute::Consumed { navigated: true }
        } else {
            OmpReplyNavigationRoute::Forwarded
        }
    }

    pub(crate) fn route_existing_with(
        &mut self,
        key: &crate::input::TerminalKey,
        mut navigate: impl FnMut() -> bool,
    ) -> Option<OmpReplyNavigationRoute> {
        let identity = reply_navigation_key_identity(key)?;
        let index = self
            .presses
            .iter()
            .position(|press| press.identity == identity)?;
        let disposition = self.presses[index].disposition;
        // Nonphysical input has no generation or hardware identity. A new Press is the only
        // reliable lifecycle boundary for press-only protocols; late Repeat/Release events stay
        // suppressed only until that explicit new lifecycle begins.
        if key.kind == crossterm::event::KeyEventKind::Press && !key.has_physical_identity() {
            self.presses.swap_remove(index);
            return None;
        }
        match disposition {
            OmpReplyNavigationDisposition::Consumed => {
                let navigated = key.kind != crossterm::event::KeyEventKind::Release && navigate();
                if key.kind == crossterm::event::KeyEventKind::Release {
                    self.presses.swap_remove(index);
                }
                Some(OmpReplyNavigationRoute::Consumed { navigated })
            }
            OmpReplyNavigationDisposition::Forwarded => Some(OmpReplyNavigationRoute::Forwarded),
            OmpReplyNavigationDisposition::Suppressed => {
                if key.kind == crossterm::event::KeyEventKind::Release {
                    self.presses.swap_remove(index);
                }
                Some(OmpReplyNavigationRoute::Consumed { navigated: false })
            }
        }
    }

    pub(crate) fn owns_key(&self, key: &crate::input::TerminalKey) -> bool {
        reply_navigation_key_identity(key)
            .is_some_and(|identity| self.presses.iter().any(|press| press.identity == identity))
    }

    pub(crate) fn suppress_existing(&mut self, key: &crate::input::TerminalKey) {
        let Some(identity) = reply_navigation_key_identity(key) else {
            return;
        };
        if let Some(press) = self
            .presses
            .iter_mut()
            .find(|press| press.identity == identity)
        {
            press.disposition = OmpReplyNavigationDisposition::Suppressed;
        }
    }

    pub(crate) fn forget(&mut self, key: &crate::input::TerminalKey) {
        let Some(identity) = reply_navigation_key_identity(key) else {
            return;
        };
        self.presses.retain(|press| press.identity != identity);
    }

    pub(crate) fn retire_owner(&mut self) -> Vec<crate::input::TerminalKey> {
        let mut releases = Vec::new();
        for press in &mut self.presses {
            if press.disposition == OmpReplyNavigationDisposition::Forwarded {
                releases.push(
                    press
                        .key
                        .clone()
                        .with_kind(crossterm::event::KeyEventKind::Release),
                );
            }
            press.disposition = OmpReplyNavigationDisposition::Suppressed;
        }
        releases
    }

    pub(crate) fn release_for_focus_loss(&mut self) -> Vec<crate::input::TerminalKey> {
        let releases = self.retire_owner();
        self.presses.clear();
        releases
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn has_forwarded(&self) -> bool {
        self.presses
            .iter()
            .any(|press| press.disposition == OmpReplyNavigationDisposition::Forwarded)
    }

    #[cfg(not(windows))]
    pub(crate) fn is_empty(&self) -> bool {
        self.presses.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.presses.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OmpPhysicalKeyRoute {
    Forwarded,
    ReplyNavigation,
    Suppressed,
}

#[derive(Clone, Debug)]
struct OmpPhysicalKeyPress {
    identity: crate::input::KeyIdentity,
    key: crate::input::TerminalKey,
    route: OmpPhysicalKeyRoute,
}

/// Bounded ownership for ordinary OMP key lifecycles. The historical name is retained because
/// physical Windows identities were its first consumer; Unix Kitty input uses semantic identities
/// with explicit Press/Repeat/Release events and requires the same owner continuity.
#[derive(Default)]
pub(crate) struct OmpPhysicalKeyPresses {
    presses: Vec<OmpPhysicalKeyPress>,
}

impl OmpPhysicalKeyPresses {
    pub(crate) fn route_existing(
        &mut self,
        key: &crate::input::TerminalKey,
    ) -> Option<OmpPhysicalKeyRoute> {
        let identity = key.identity();
        if let Some(index) = self
            .presses
            .iter()
            .position(|press| press.identity == identity)
        {
            let route = self.presses[index].route;
            if key.kind == crossterm::event::KeyEventKind::Press && !key.has_physical_identity() {
                self.presses.swap_remove(index);
                return None;
            }
            if key.kind == crossterm::event::KeyEventKind::Release
                && route != OmpPhysicalKeyRoute::Forwarded
            {
                self.presses.swap_remove(index);
            }
            return Some(route);
        }
        None
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn owns_key(&self, key: &crate::input::TerminalKey) -> bool {
        self.presses
            .iter()
            .any(|press| press.identity == key.identity())
    }

    pub(crate) fn reserve_press(
        &mut self,
        key: &crate::input::TerminalKey,
    ) -> Option<Vec<crate::input::TerminalKey>> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return None;
        }
        let identity = key.identity();
        if self.presses.iter().any(|press| press.identity == identity) {
            return None;
        }
        let mut releases = Vec::new();
        if self.presses.len() >= MAX_TRACKED_OMP_INPUT_PRESSES {
            let evicted = self.presses.remove(0);
            if evicted.route == OmpPhysicalKeyRoute::Forwarded {
                releases.push(
                    evicted
                        .key
                        .with_kind(crossterm::event::KeyEventKind::Release),
                );
            }
        }
        self.presses.push(OmpPhysicalKeyPress {
            identity,
            key: key.clone(),
            route: OmpPhysicalKeyRoute::Suppressed,
        });
        Some(releases)
    }

    pub(crate) fn commit_press(
        &mut self,
        key: &crate::input::TerminalKey,
        route: OmpPhysicalKeyRoute,
    ) -> bool {
        let identity = key.identity();
        let Some(press) = self
            .presses
            .iter_mut()
            .find(|press| press.identity == identity)
        else {
            return false;
        };
        press.route = route;
        true
    }

    #[cfg(test)]
    pub(crate) fn track(
        &mut self,
        key: &crate::input::TerminalKey,
        route: OmpPhysicalKeyRoute,
    ) -> bool {
        self.reserve_press(key).is_some() && self.commit_press(key, route)
    }

    pub(crate) fn forget(&mut self, key: &crate::input::TerminalKey) {
        let identity = key.identity();
        self.presses.retain(|press| press.identity != identity);
    }
    pub(crate) fn suppress_existing(&mut self, key: &crate::input::TerminalKey) {
        let identity = key.identity();
        if let Some(press) = self
            .presses
            .iter_mut()
            .find(|press| press.identity == identity)
        {
            press.route = OmpPhysicalKeyRoute::Suppressed;
        }
    }

    pub(crate) fn retire_owner(&mut self) -> Vec<crate::input::TerminalKey> {
        let mut releases = Vec::new();
        for press in &mut self.presses {
            if press.route == OmpPhysicalKeyRoute::Forwarded {
                releases.push(
                    press
                        .key
                        .clone()
                        .with_kind(crossterm::event::KeyEventKind::Release),
                );
            }
            press.route = OmpPhysicalKeyRoute::Suppressed;
        }
        releases
    }

    pub(crate) fn release_for_focus_loss(&mut self) -> Vec<crate::input::TerminalKey> {
        let releases = self.retire_owner();
        self.presses.clear();
        releases
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn owns_input(&self) -> bool {
        !self.presses.is_empty()
    }

    #[cfg(any(not(windows), test))]
    pub(crate) fn has_forwarded(&self) -> bool {
        self.presses
            .iter()
            .any(|press| press.route == OmpPhysicalKeyRoute::Forwarded)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.presses.len()
    }
}

/// Identifies the arrow key that owns a consumed reply-navigation press.
/// Modifiers are deliberately excluded because Windows can report Alt's
/// release before the arrow's release.
fn reply_navigation_key_identity(
    key: &crate::input::TerminalKey,
) -> Option<crate::input::KeyIdentity> {
    matches!(
        key.code,
        crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Down
    )
    .then(|| key.identity())
}

impl TerminalRuntime {
    pub fn shutdown(self) {
        self.0.shutdown();
    }

    pub(crate) fn set_preserve_primary_scrollback(&self, enabled: bool) {
        self.0.set_preserve_primary_scrollback(enabled);
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        self.0.duplicate_handoff_fd()
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(self) {
        self.0.preserve_for_handoff()
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        self.0.assume_handoff_ownership();
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        self.0.set_handoff_reader_paused(paused);
    }

    #[cfg(unix)]
    pub fn pause_handoff_reader(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        self.0.pause_handoff_reader(timeout)
    }

    #[cfg(unix)]
    pub fn remote_execution_ready(&self) -> bool {
        self.0.remote_execution_ready()
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_remote_execution_ready_for_test(&self, ready: bool) {
        self.0.set_remote_execution_ready_for_test(ready);
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        self.0.handoff_runtime_state(pane_id)
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        self.0.handoff_history_ansi()
    }

    #[cfg(unix)]
    pub fn from_handoff_fd(
        import: crate::handoff_runtime::ImportedHandoffRuntime,
        execution_target: &crate::execution::ExecutionTarget,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::from_handoff_fd(
            import,
            execution_target,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[cfg(test)]
    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_on(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        execution_target: &crate::execution::ExecutionTarget,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_on(
            pane_id,
            rows,
            cols,
            cwd,
            execution_target,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_initial_history_on(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        execution_target: &crate::execution::ExecutionTarget,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_with_initial_history_on(
            pane_id,
            rows,
            cols,
            cwd,
            execution_target,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            initial_history_ansi,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_command_on(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        execution_target: &crate::execution::ExecutionTarget,
        command: &str,
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_shell_command_on(
            pane_id,
            rows,
            cols,
            cwd,
            execution_target,
            command,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd,
            argv,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_argv_command_on(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        execution_target: &crate::execution::ExecutionTarget,
        argv: &[String],
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_argv_command_on(
            pane_id,
            rows,
            cols,
            cwd,
            execution_target,
            argv,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_plugin_command_on(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        execution_target: &crate::execution::ExecutionTarget,
        plugin_id: &str,
        entrypoint: &str,
        local_argv: &[String],
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_plugin_command_on(
            pane_id,
            rows,
            cols,
            cwd,
            execution_target,
            plugin_id,
            entrypoint,
            local_argv,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.0.apply_host_terminal_theme(theme);
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<crate::terminal_theme::HostAppearance>,
    ) {
        self.0.apply_host_terminal_appearance(appearance);
    }

    pub fn begin_graceful_release(&self, agent: crate::detect::Agent) {
        self.0.begin_graceful_release(agent);
    }

    pub fn reset_agent_detection(&self) {
        self.0.reset_agent_detection();
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_reset_notify_for_test(
        &self,
    ) -> std::sync::Arc<tokio::sync::Notify> {
        self.0.agent_detection_reset_notify_for_test()
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_enabled_for_test(&self) -> bool {
        self.0.agent_detection_enabled_for_test()
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        self.0.set_full_lifecycle_authority_active(active);
    }

    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        self.0.resize(rows, cols, cell_width_px, cell_height_px);
    }

    #[cfg(unix)]
    pub fn nudge_child_redraw_after_handoff(&self) {
        self.0.nudge_child_redraw_after_handoff();
    }

    pub fn scroll_up(&self, lines: usize) {
        self.0.scroll_up(lines);
    }

    pub fn scroll_down(&self, lines: usize) {
        self.0.scroll_down(lines);
    }

    pub fn scroll_reset(&self) {
        self.0.scroll_reset();
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.0.set_scroll_offset_from_bottom(lines);
    }

    #[allow(dead_code)]
    pub fn jump_to_previous_semantic_prompt(&self) -> bool {
        self.0.jump_to_previous_semantic_prompt()
    }

    #[allow(dead_code)]
    pub fn jump_to_next_semantic_prompt(&self) -> bool {
        self.0.jump_to_next_semantic_prompt()
    }

    /// Handles grouped OMP reply navigation in one locked, clamped terminal pass.
    pub fn try_navigate_omp_reply_repeated(
        &self,
        recognized_omp: bool,
        key: &crate::input::TerminalKey,
    ) -> bool {
        self.try_navigate_omp_reply(recognized_omp, key)
    }

    /// Handles the exact OMP reply-navigation chord when this runtime belongs
    /// to a recognized OMP pane. Returns false so callers can forward every
    /// other key unchanged.
    pub fn try_navigate_omp_reply(
        &self,
        recognized_omp: bool,
        key: &crate::input::TerminalKey,
    ) -> bool {
        self.0.try_navigate_omp_reply(recognized_omp, key)
    }

    pub fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        self.0.scroll_metrics()
    }

    pub(crate) fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        self.0.search_text_matches(query, case_sensitive)
    }

    pub(crate) fn search_text_matches_reverse_chunk(
        &self,
        query: &str,
        case_sensitive: bool,
        end_row_exclusive: u32,
        max_cells: usize,
        max_matches: usize,
        expected_snapshot: Option<crate::pane::TerminalTextSearchSnapshot>,
    ) -> crate::pane::TerminalTextSearchChunk {
        self.0.search_text_matches_reverse_chunk(
            query,
            case_sensitive,
            end_row_exclusive,
            max_cells,
            max_matches,
            expected_snapshot,
        )
    }

    pub(crate) fn text_match_is_current(&self, text_match: crate::pane::TerminalTextMatch) -> bool {
        self.0.text_match_is_current(text_match)
    }

    pub(crate) fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        self.0.text_matches_are_current(text_matches)
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.0.word_motion_target(row, col, motion)
    }

    /// Collects the complete terminal input-mode snapshot.
    ///
    /// This performs multiple terminal queries. Keep it out of render/layout
    /// and pane-scaled loops; add a narrow accessor when one fact is needed.
    #[cfg(test)]
    pub fn input_state(&self) -> Option<crate::pane::InputState> {
        self.0.input_state()
    }

    pub fn keyboard_report_all_requested(&self) -> bool {
        self.0.keyboard_report_all_requested()
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.0.focus_reporting_enabled()
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.0.bracketed_paste_enabled()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.0.mouse_reporting_enabled()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.0.sgr_pixel_mouse_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.0.plain_page_keys_use_host_scrollback()
    }

    /// Reads only whether the alternate screen is active.
    pub fn alternate_screen_active(&self) -> bool {
        self.0.alternate_screen_active()
    }

    pub fn cursor_state(
        &self,
        area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        self.0.cursor_state(area, show_cursor)
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.0.synchronized_output_active()
    }

    pub fn visible_text(&self) -> String {
        self.0.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        self.0.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        self.0.detection_text()
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.0.terminal_title()
    }

    pub fn agent_osc_title(&self) -> String {
        self.0.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.0.agent_osc_progress()
    }

    pub(crate) fn recent_text_snapshot(&self, lines: usize) -> crate::pane::TerminalReadSnapshot {
        self.0.recent_text_snapshot(lines)
    }

    pub(crate) fn recent_ansi_snapshot(&self, lines: usize) -> crate::pane::TerminalReadSnapshot {
        self.0.recent_ansi_snapshot(lines)
    }

    #[cfg(test)]
    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.0.recent_unwrapped_text_snapshot(lines).text
    }

    pub(crate) fn recent_unwrapped_text_snapshot(
        &self,
        lines: usize,
    ) -> crate::pane::TerminalReadSnapshot {
        self.0.recent_unwrapped_text_snapshot(lines)
    }

    pub(crate) fn recent_unwrapped_ansi_snapshot(
        &self,
        lines: usize,
    ) -> crate::pane::TerminalReadSnapshot {
        self.0.recent_unwrapped_ansi_snapshot(lines)
    }

    pub fn snapshot_history(&self) -> Option<String> {
        self.0.snapshot_history()
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.0.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.0.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> crate::pane::TerminalDirtyPatchOutcome {
        self.0.collect_dirty_patch(area_width, area_height)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.0.visible_hyperlinks(area)
    }

    pub(crate) fn hyperlink_at_viewport_cell(
        &self,
        col: u16,
        row: u16,
        width: u16,
        height: u16,
    ) -> Option<crate::pane::ViewportHyperlink> {
        self.0.hyperlink_at_viewport_cell(col, row, width, height)
    }

    pub(crate) fn logical_line_at_viewport_row(
        &self,
        row: u16,
        width: u16,
        height: u16,
    ) -> Option<crate::pane::ViewportLogicalLine> {
        self.0.logical_line_at_viewport_row(row, width, height)
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        self.0.kitty_image_placements_with_data_filter(needs_data)
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        self.0.keyboard_protocol()
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        self.0.encode_terminal_key(key)
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.0.send_bytes(bytes).await
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.0.try_send_bytes(bytes)
    }

    pub fn send_bytes_after(&self, bytes: Bytes, delay: std::time::Duration) {
        self.0.send_bytes_after(bytes, delay);
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.scroll_reset();
        self.0.send_paste(text).await
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.scroll_reset();
        self.0.try_send_paste(text)
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        self.0.try_send_focus_event(event)
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.0.wheel_routing()
    }

    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        crate::terminal::ScreenSnapshot,
    )> {
        let (screen, cols, rows) = self.0.screen_text_snapshot()?;
        Some((screen, crate::terminal::ScreenSnapshot { cols, rows }))
    }

    pub(crate) fn screen_text_snapshot_with_seq(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        crate::terminal::ScreenSnapshot,
        u64,
    )> {
        for _ in 0..3 {
            let before = self.content_seq();
            if !before.is_multiple_of(2) {
                continue;
            }
            let (screen, snapshot) = self.screen_text_snapshot()?;
            let after = self.content_seq();
            if before == after {
                return Some((screen, snapshot, after));
            }
        }
        None
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_button(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_motion(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_wheel(kind, position, modifiers)
    }

    pub(crate) fn pixel_size(&self) -> Option<(u32, u32)> {
        self.0.pixel_size()
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.0.encode_alternate_scroll(kind)
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.0.cwd()
    }

    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        self.0.follow_cwd()
    }

    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        self.0.foreground_cwd()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.0.child_pid()
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        self.0.current_size()
    }

    pub(crate) fn content_seq(&self) -> u64 {
        self.0.content_seq()
    }
}

#[cfg(test)]
impl TerminalRuntime {
    pub(crate) fn test_set_child_pid(&self, pid: u32) {
        self.0.test_set_child_pid(pid);
    }

    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel(cols, rows);
        (Self(runtime), rx)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) =
            crate::pane::PaneRuntime::test_with_channel_capacity(cols, rows, capacity);
        (Self(runtime), rx)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self(crate::pane::PaneRuntime::test_with_screen_bytes(
            cols, rows, bytes,
        ))
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        self.0.test_process_pty_bytes(bytes);
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self(crate::pane::PaneRuntime::test_with_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
        ))
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
            channel_capacity,
        );
        (Self(runtime), rx)
    }
}
#[cfg(test)]
mod omp_input_tracker_tests {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    use super::*;

    fn physical_key(index: u16, kind: KeyEventKind) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT)
            .with_kind(kind)
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down: kind != KeyEventKind::Release,
                repeat_count: 1,
                virtual_key_code: 0x26,
                virtual_scan_code: index,
                unicode: 0,
                control_key_state: 0,
            })
    }
    #[test]
    fn physical_key_tracker_is_bounded_by_releasing_the_oldest_press() {
        let mut presses = OmpPhysicalKeyPresses::default();
        for index in 1..=MAX_TRACKED_OMP_INPUT_PRESSES as u16 {
            assert!(presses.track(
                &physical_key(index, KeyEventKind::Press),
                OmpPhysicalKeyRoute::Forwarded,
            ));
        }
        let newest = physical_key(
            MAX_TRACKED_OMP_INPUT_PRESSES as u16 + 1,
            KeyEventKind::Press,
        );
        let releases = presses.reserve_press(&newest).expect("bounded reservation");
        assert_eq!(
            releases,
            [physical_key(1, KeyEventKind::Press).with_kind(KeyEventKind::Release)]
        );
        assert!(presses.commit_press(&newest, OmpPhysicalKeyRoute::Forwarded));
        assert_eq!(presses.len(), MAX_TRACKED_OMP_INPUT_PRESSES);
        assert_eq!(
            presses.route_existing(&physical_key(1, KeyEventKind::Repeat)),
            None
        );
        assert_eq!(
            presses.route_existing(&newest.with_kind(KeyEventKind::Repeat)),
            Some(OmpPhysicalKeyRoute::Forwarded)
        );
        assert_eq!(
            presses.release_for_focus_loss().len(),
            MAX_TRACKED_OMP_INPUT_PRESSES
        );
        assert_eq!(presses.len(), 0);
    }

    #[test]
    fn semantic_key_tracker_stays_live_without_release_events() {
        let mut presses = OmpPhysicalKeyPresses::default();
        let mut latest = None;
        for index in 0..300_u32 {
            let key = crate::input::TerminalKey::new(
                KeyCode::Char(char::from_u32(0x1000 + index).expect("test character")),
                KeyModifiers::empty(),
            );
            let releases = presses.reserve_press(&key).expect("semantic reservation");
            assert_eq!(
                releases.len(),
                usize::from(index >= MAX_TRACKED_OMP_INPUT_PRESSES as u32)
            );
            assert!(presses.commit_press(&key, OmpPhysicalKeyRoute::Forwarded));
            latest = Some(key);
        }

        assert_eq!(presses.len(), MAX_TRACKED_OMP_INPUT_PRESSES);
        assert_eq!(
            presses.route_existing(&latest.expect("latest key").with_kind(KeyEventKind::Repeat)),
            Some(OmpPhysicalKeyRoute::Forwarded)
        );
    }

    #[test]
    fn ordinary_semantic_key_stays_with_its_owner_until_release_or_retirement() {
        let press = crate::input::TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty());
        let mut presses = OmpPhysicalKeyPresses::default();
        assert!(presses.track(&press, OmpPhysicalKeyRoute::Forwarded));
        assert_eq!(
            presses.route_existing(&press.clone().with_kind(KeyEventKind::Repeat)),
            Some(OmpPhysicalKeyRoute::Forwarded)
        );

        assert_eq!(
            presses.retire_owner(),
            vec![press.clone().with_kind(KeyEventKind::Release)]
        );
        assert_eq!(
            presses.route_existing(&press.clone().with_kind(KeyEventKind::Repeat)),
            Some(OmpPhysicalKeyRoute::Suppressed)
        );
        assert!(presses.route_existing(&press).is_none());
        assert!(presses.track(&press, OmpPhysicalKeyRoute::Forwarded));
        let release = press.with_kind(KeyEventKind::Release);
        assert_eq!(
            presses.route_existing(&release),
            Some(OmpPhysicalKeyRoute::Forwarded)
        );
        presses.forget(&release);
        assert_eq!(presses.len(), 0);
    }

    #[test]
    fn retired_semantic_navigation_suppresses_late_events_but_allows_a_new_press() {
        let mut presses = OmpReplyNavigationPresses::default();
        let key = crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(
            presses.route(&key, || true),
            OmpReplyNavigationRoute::Consumed { navigated: true }
        );
        presses.retire_owner();

        let mut called = false;
        assert_eq!(
            presses.route_existing_with(&key.clone().with_kind(KeyEventKind::Repeat), || {
                called = true;
                true
            }),
            Some(OmpReplyNavigationRoute::Consumed { navigated: false })
        );
        assert!(!called);
        assert!(presses.route_existing_with(&key, || true).is_none());
        assert_eq!(
            presses.route(&key, || false),
            OmpReplyNavigationRoute::Forwarded
        );
        assert_eq!(
            presses.route_existing_with(&key.clone().with_kind(KeyEventKind::Release), || true),
            Some(OmpReplyNavigationRoute::Forwarded)
        );
        presses.forget(&key);
        assert_eq!(presses.len(), 0);
    }

    #[test]
    fn orphan_semantic_repeat_is_suppressed_without_navigation() {
        let mut presses = OmpReplyNavigationPresses::default();
        let mut called = false;
        assert_eq!(
            presses.route(
                &crate::input::TerminalKey::new(KeyCode::Up, KeyModifiers::ALT)
                    .with_kind(KeyEventKind::Repeat),
                || {
                    called = true;
                    true
                },
            ),
            OmpReplyNavigationRoute::Consumed { navigated: false }
        );
        assert!(!called);
        assert_eq!(presses.len(), 0);
    }
}
