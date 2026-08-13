use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::layout::Rect;

use crate::{
    app::{
        state::{FindrState, FINDR_MAX_MATCHES, FINDR_MAX_QUERY_CHARS, FINDR_SCAN_MAX_CELLS},
        App, AppState, Mode,
    },
    input::TerminalKey,
    pane::TerminalTextSearchChunkStatus,
    terminal::TerminalRuntimeRegistry,
};

fn smart_case(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

fn findr_key_text(key: &TerminalKey) -> Option<String> {
    if let Some(text) = key.generated_text.as_ref() {
        return Some(text.clone());
    }
    if !key
        .modifiers
        .difference(crossterm::event::KeyModifiers::SHIFT)
        .is_empty()
    {
        return None;
    }
    key.shifted_codepoint
        .and_then(char::from_u32)
        .or(match key.code {
            KeyCode::Char(ch) => Some(ch),
            _ => None,
        })
        .map(|ch| ch.to_string())
}

fn findr_scan_overlap_rows(query: &str, cols: u16) -> u32 {
    let cols = usize::from(cols).max(1);
    u32::try_from(
        crate::ghostty::unicode_text_width(query)
            .saturating_sub(1)
            .div_ceil(cols),
    )
    .unwrap_or(u32::MAX)
}

fn findr_match_intersects_viewport(
    text_match: crate::pane::TerminalTextMatch,
    start_row: u32,
    end_row: u32,
) -> bool {
    text_match.end.row >= start_row && text_match.start.row < end_row
}

fn findr_refresh_needed(
    findr: &FindrState,
    pty_sources: &HashSet<crate::layout::PaneId>,
    visible_range: Option<(u32, u32)>,
    visible_geometry: Option<(u16, u16)>,
) -> bool {
    !findr.query.is_empty()
        && (pty_sources.contains(&findr.pane_id)
            || visible_geometry != findr.visible_geometry
            || (!findr.scrollback && visible_range != findr.visible_range))
}

impl AppState {
    pub(crate) fn open_findr(&mut self) {
        let Some(ws_idx) = self.active else {
            return;
        };
        let Some(pane_id) = self.focused_terminal_pane_id(ws_idx) else {
            return;
        };
        self.findr = Some(FindrState::new(pane_id));
        self.mode = Mode::Findr;
        self.reconcile_focus_lifecycle();
    }

    pub(crate) fn close_findr(&mut self) {
        self.findr = None;
        self.mode = if self.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    fn findr_target_runtime<'a>(
        &'a self,
        terminal_runtimes: &'a TerminalRuntimeRegistry,
    ) -> Option<&'a crate::terminal::TerminalRuntime> {
        let pane_id = self.findr.as_ref()?.pane_id;
        if let Some(terminal_id) = self
            .workspace_plugin_panes
            .values()
            .find(|pane| pane.pane_id == pane_id)
            .map(|pane| &pane.terminal_id)
        {
            return terminal_runtimes.get(terminal_id);
        }
        self.runtime_for_pane_in_workspace(terminal_runtimes, self.active?, pane_id)
    }

    fn findr_target_rect(&self) -> Option<Rect> {
        let pane_id = self.findr.as_ref()?.pane_id;
        if self
            .workspace_plugin_panes
            .values()
            .any(|pane| pane.pane_id == pane_id)
        {
            return (!self.view.workspace_plugin_pane_inner.is_empty())
                .then_some(self.view.workspace_plugin_pane_inner);
        }
        self.pane_info_by_id(pane_id).map(|info| info.inner_rect)
    }

    fn findr_visible_range(
        &self,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<(u32, u32)> {
        let rect = self.findr_target_rect()?;
        let metrics = self
            .findr_target_runtime(terminal_runtimes)?
            .scroll_metrics()?;
        let total_rows = metrics
            .max_offset_from_bottom
            .saturating_add(metrics.viewport_rows)
            .min(u32::MAX as usize) as u32;
        let end_row =
            total_rows.saturating_sub(metrics.offset_from_bottom.min(u32::MAX as usize) as u32);
        Some((end_row.saturating_sub(u32::from(rect.height)), end_row))
    }

    fn begin_findr_scan(&mut self, terminal_runtimes: &TerminalRuntimeRegistry) -> bool {
        let Some((start_row, end_row)) = self.findr_visible_range(terminal_runtimes) else {
            return false;
        };
        let Some(rect) = self.findr_target_rect() else {
            return false;
        };
        let Some(findr) = self.findr.as_mut() else {
            return false;
        };
        findr.matches.clear();
        findr.scan_snapshot = None;
        findr.scan_start_row = if findr.scrollback {
            0
        } else {
            start_row.saturating_sub(findr_scan_overlap_rows(&findr.query, rect.width))
        };
        findr.scan_end_row_exclusive = if findr.scrollback { u32::MAX } else { end_row };
        findr.visible_range = Some((start_row, end_row));
        findr.visible_geometry = Some((rect.width, rect.height));
        findr.complete = findr.query.is_empty() || start_row == end_row;
        findr.capped = false;
        findr.budget_limited = false;
        true
    }

    pub(crate) fn refresh_findr_visible(
        &mut self,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> bool {
        if !self.begin_findr_scan(terminal_runtimes) {
            return false;
        }
        self.tick_findr_scan(terminal_runtimes)
    }

    pub(crate) fn tick_findr_scan(&mut self, terminal_runtimes: &TerminalRuntimeRegistry) -> bool {
        let Some(findr) = self.findr.as_ref() else {
            return false;
        };
        if findr.complete || findr.query.is_empty() || findr.capped {
            return false;
        }
        let Some(runtime) = self.findr_target_runtime(terminal_runtimes) else {
            self.close_findr();
            return true;
        };
        let query = findr.query.clone();
        let scan_start_row = findr.scan_start_row;
        let end_row = findr.scan_end_row_exclusive;
        let expected_snapshot = findr.scan_snapshot;
        let remaining = FINDR_MAX_MATCHES.saturating_sub(findr.matches.len());
        let chunk = runtime.search_text_matches_reverse_chunk(
            &query,
            smart_case(&query),
            end_row,
            FINDR_SCAN_MAX_CELLS,
            remaining,
            expected_snapshot,
        );

        match chunk.status {
            TerminalTextSearchChunkStatus::SnapshotMismatch => {
                self.begin_findr_scan(terminal_runtimes);
            }
            TerminalTextSearchChunkStatus::InsufficientBudget => {
                if let Some(findr) = self.findr.as_mut() {
                    findr.complete = true;
                    findr.budget_limited = true;
                }
            }
            TerminalTextSearchChunkStatus::InvalidQuery
            | TerminalTextSearchChunkStatus::Unavailable => {
                if let Some(findr) = self.findr.as_mut() {
                    findr.complete = true;
                }
            }
            TerminalTextSearchChunkStatus::Scanned => {
                let progressed = chunk.start_row < chunk.end_row;
                if let Some(findr) = self.findr.as_mut() {
                    if findr.query != query {
                        return false;
                    }
                    let visible_range = findr.visible_range;
                    findr
                        .matches
                        .extend(chunk.matches.into_iter().filter(|text_match| {
                            findr.scrollback
                                || visible_range.is_some_and(|(start, end)| {
                                    findr_match_intersects_viewport(*text_match, start, end)
                                })
                        }));
                    findr.scan_snapshot = chunk.snapshot;
                    findr.scan_end_row_exclusive = chunk.start_row;
                    findr.capped = findr.matches.len() >= FINDR_MAX_MATCHES;
                    findr.complete =
                        findr.capped || !progressed || chunk.start_row <= scan_start_row;
                }
            }
        }
        true
    }

    fn reset_findr_search(&mut self, terminal_runtimes: &TerminalRuntimeRegistry) {
        let Some(findr) = self.findr.as_mut() else {
            return;
        };
        findr.reset_scan();
        if findr.query.is_empty() {
            return;
        }
        self.refresh_findr_visible(terminal_runtimes);
    }

    pub(crate) fn handle_findr_key(
        &mut self,
        terminal_runtimes: &TerminalRuntimeRegistry,
        key: TerminalKey,
    ) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        match key.code {
            KeyCode::Esc => self.close_findr(),
            KeyCode::Tab => self.toggle_findr_scrollback(terminal_runtimes),
            KeyCode::PageUp if key.modifiers.is_empty() => {
                let lines = self
                    .findr_target_rect()
                    .map(|rect| usize::from(rect.height))
                    .unwrap_or(10)
                    .max(1);
                self.scroll_findr(terminal_runtimes, lines, true);
            }
            KeyCode::PageDown if key.modifiers.is_empty() => {
                let lines = self
                    .findr_target_rect()
                    .map(|rect| usize::from(rect.height))
                    .unwrap_or(10)
                    .max(1);
                self.scroll_findr(terminal_runtimes, lines, false);
            }
            KeyCode::Up if key.modifiers.is_empty() => {
                self.scroll_findr(terminal_runtimes, 1, true);
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                self.scroll_findr(terminal_runtimes, 1, false);
            }
            KeyCode::Backspace => {
                if let Some(findr) = self.findr.as_mut() {
                    findr.query.pop();
                }
                self.reset_findr_search(terminal_runtimes);
            }
            _ => {
                if let Some(text) = findr_key_text(&key) {
                    self.insert_findr_text(terminal_runtimes, &text);
                }
            }
        }
    }

    pub(crate) fn toggle_findr_scrollback(&mut self, terminal_runtimes: &TerminalRuntimeRegistry) {
        if let Some(findr) = self.findr.as_mut() {
            findr.scrollback = !findr.scrollback;
        }
        self.reset_findr_search(terminal_runtimes);
    }
    pub(crate) fn scroll_findr(
        &mut self,
        terminal_runtimes: &TerminalRuntimeRegistry,
        lines: usize,
        up: bool,
    ) {
        let Some(pane_id) = self.findr.as_ref().map(|findr| findr.pane_id) else {
            return;
        };
        if let Some(runtime) = self.findr_target_runtime(terminal_runtimes) {
            if up {
                runtime.scroll_up(lines);
            } else {
                runtime.scroll_down(lines);
            }
        } else if up {
            self.scroll_pane_up(terminal_runtimes, pane_id, lines);
        } else {
            self.scroll_pane_down(terminal_runtimes, pane_id, lines);
        }
        if self.findr.as_ref().is_some_and(|findr| !findr.scrollback) {
            self.refresh_findr_visible(terminal_runtimes);
        }
    }

    pub(crate) fn insert_findr_text(
        &mut self,
        terminal_runtimes: &TerminalRuntimeRegistry,
        text: &str,
    ) {
        let Some(findr) = self.findr.as_mut() else {
            return;
        };
        let before = findr.query.chars().count();
        let remaining = FINDR_MAX_QUERY_CHARS.saturating_sub(before);
        findr
            .query
            .extend(text.chars().filter(|ch| !ch.is_control()).take(remaining));
        if findr.query.chars().count() != before {
            self.reset_findr_search(terminal_runtimes);
        }
    }
}

impl App {
    pub(crate) fn open_findr(&mut self) {
        self.state.open_findr();
        self.findr_scan_deadline = None;
    }

    pub(crate) fn reset_findr_scan_deadline(&mut self) {
        self.findr_scan_deadline = self
            .state
            .findr
            .as_ref()
            .is_some_and(|findr| !findr.complete)
            .then_some(std::time::Instant::now());
    }

    pub(crate) fn handle_findr_key(&mut self, key: TerminalKey) {
        let was_open = self.state.findr.is_some();
        self.state.handle_findr_key(&self.terminal_runtimes, key);
        if was_open && self.state.findr.is_none() {
            self.findr_scan_deadline = None;
        } else {
            self.reset_findr_scan_deadline();
        }
    }

    pub(crate) fn tick_findr_scan(&mut self, now: std::time::Instant) -> bool {
        if self.state.findr.is_none() {
            self.findr_scan_deadline = None;
            return false;
        }
        if self
            .findr_scan_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return false;
        }
        let changed = self.state.tick_findr_scan(&self.terminal_runtimes);
        self.findr_scan_deadline = self
            .state
            .findr
            .as_ref()
            .is_some_and(|findr| !findr.complete)
            .then_some(now + crate::app::state::FINDR_SCAN_INTERVAL);
        changed
    }

    pub(crate) fn refresh_findr_visible_if_needed(
        &mut self,
        pty_sources: &HashSet<crate::layout::PaneId>,
    ) -> bool {
        let Some(findr) = self.state.findr.as_ref() else {
            return false;
        };
        let visible_range = self.state.findr_visible_range(&self.terminal_runtimes);
        let visible_geometry = self
            .state
            .findr_target_rect()
            .map(|rect| (rect.width, rect.height));
        if findr_refresh_needed(findr, pty_sources, visible_range, visible_geometry) {
            let changed = self.state.refresh_findr_visible(&self.terminal_runtimes);
            self.reset_findr_scan_deadline();
            return changed;
        }
        if findr.scrollback && findr.visible_range != visible_range {
            if let Some(findr) = self.state.findr.as_mut() {
                findr.visible_range = visible_range;
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findr_resets_results_and_bounds_query() {
        let mut findr = FindrState::new(crate::layout::PaneId::from_raw(1));
        findr.query = "x".repeat(FINDR_MAX_QUERY_CHARS);
        findr.matches = Vec::with_capacity(FINDR_MAX_MATCHES);
        findr.reset_scan();
        assert!(!findr.complete);
        assert!(findr.matches.is_empty());
        assert!(findr.scan_snapshot.is_none());
    }

    #[test]
    fn insert_findr_text_filters_controls_and_caps_query() {
        let mut state = crate::app::state::AppState::test_new();
        state.findr = Some(FindrState::new(crate::layout::PaneId::from_raw(1)));
        state.insert_findr_text(
            &TerminalRuntimeRegistry::new(),
            &format!("{}\nignored", "x".repeat(FINDR_MAX_QUERY_CHARS + 4)),
        );

        let query = &state.findr.as_ref().expect("Findr state").query;
        assert_eq!(query.chars().count(), FINDR_MAX_QUERY_CHARS);
        assert!(!query.contains('\n'));
    }

    #[test]
    fn findr_key_text_accepts_native_and_shifted_characters_only() {
        let native = TerminalKey::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::empty())
            .with_windows_record(crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 0x41,
                virtual_scan_code: 0x1e,
                unicode: 'a' as u16,
                control_key_state: 0,
            });
        let shifted = TerminalKey::new(KeyCode::Char('1'), crossterm::event::KeyModifiers::SHIFT)
            .with_shifted_codepoint('!' as u32);
        let modified =
            TerminalKey::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::CONTROL);

        assert_eq!(findr_key_text(&native).as_deref(), Some("a"));
        assert_eq!(findr_key_text(&shifted).as_deref(), Some("!"));
        assert_eq!(findr_key_text(&modified), None);
    }

    #[test]
    fn capped_findr_query_does_not_restart_scan() {
        let pane_id = crate::layout::PaneId::from_raw(1);
        let mut state = crate::app::state::AppState::test_new();
        let mut findr = FindrState::new(pane_id);
        findr.query = "x".repeat(FINDR_MAX_QUERY_CHARS);
        findr.complete = true;
        state.findr = Some(findr);

        state.insert_findr_text(&TerminalRuntimeRegistry::new(), "y");

        assert!(state.findr.as_ref().unwrap().complete);
    }

    #[tokio::test]
    async fn findr_page_keys_scroll_target_pane_without_forwarding() {
        let mut state = crate::app::state::AppState::test_new();
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 40, 5));
        let bytes = (0..40)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                pane_infos[0].inner_rect.width,
                pane_infos[0].inner_rect.height,
                16 * 1024,
                bytes.as_bytes(),
            ),
        );
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.view.pane_infos = pane_infos;
        state.findr = Some(FindrState::new(pane_id));
        state.mode = Mode::Findr;
        let runtimes = TerminalRuntimeRegistry::new();

        state.handle_findr_key(
            &runtimes,
            TerminalKey::new(KeyCode::PageUp, crossterm::event::KeyModifiers::empty()),
        );

        let offset = state
            .pane_scroll_metrics(&runtimes, pane_id)
            .expect("Findr pane scroll metrics")
            .offset_from_bottom;
        assert!(offset > 0);
        assert_eq!(state.mode, Mode::Findr);
    }
    #[test]
    fn findr_visible_scan_overlaps_enough_rows_for_wrapped_query() {
        assert_eq!(findr_scan_overlap_rows("لالالا", 4), 2);
        assert_eq!(findr_scan_overlap_rows("1", 4), 0);
        assert_eq!(findr_scan_overlap_rows("1234", 4), 1);
        assert_eq!(findr_scan_overlap_rows("123456", 4), 2);
        assert_eq!(findr_scan_overlap_rows("123456789", 4), 2);
    }

    #[tokio::test]
    async fn visible_findr_finds_wrapped_match_crossing_viewport_top() {
        let mut state = crate::app::state::AppState::test_new();
        let mut workspace = crate::workspace::Workspace::test_new("wrapped-findr");
        let pane_id = workspace.tabs[0].root_pane;
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 4, 2));
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                pane_infos[0].inner_rect.width,
                pane_infos[0].inner_rect.height,
                16 * 1024,
                b"aaaaxxneedle!tail",
            ),
        );
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.view.pane_infos = pane_infos;
        let runtimes = TerminalRuntimeRegistry::new();
        state.scroll_pane_up(&runtimes, pane_id, 1);
        let mut findr = FindrState::new(pane_id);
        findr.query = "needle".into();
        state.findr = Some(findr);
        state.mode = Mode::Findr;

        assert!(state.refresh_findr_visible(&runtimes));
        while !state.findr.as_ref().unwrap().complete {
            assert!(state.tick_findr_scan(&runtimes));
        }

        let matches = &state.findr.as_ref().unwrap().matches;
        assert_eq!(matches.len(), 1);
        assert!(matches[0].start.row < state.findr.as_ref().unwrap().visible_range.unwrap().0);
    }

    #[test]
    fn findr_ignores_unrelated_pty_damage_when_viewport_is_unchanged() {
        let target = crate::layout::PaneId::from_raw(1);
        let mut findr = FindrState::new(target);
        findr.query = "needle".to_owned();
        findr.visible_range = Some((2, 7));
        findr.visible_geometry = Some((20, 5));

        assert!(!findr_refresh_needed(
            &findr,
            &HashSet::from([crate::layout::PaneId::from_raw(2)]),
            Some((2, 7)),
            Some((20, 5)),
        ));
    }

    #[test]
    fn findr_refreshes_when_visible_geometry_changes() {
        let target = crate::layout::PaneId::from_raw(1);
        let mut findr = FindrState::new(target);
        findr.query = "needle".to_owned();
        findr.visible_range = Some((2, 7));
        findr.visible_geometry = Some((20, 5));

        assert!(findr_refresh_needed(
            &findr,
            &HashSet::new(),
            Some((2, 7)),
            Some((21, 5)),
        ));
    }

    #[tokio::test]
    async fn visible_findr_refreshes_only_for_its_dirty_runtime() {
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("test");
        let pane_id = workspace.tabs[0].root_pane;
        let info = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 20, 3))[0]
            .clone();
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 3, b"needle"),
        );
        app.state.workspaces.push(workspace);
        app.state.active = Some(0);
        app.state.view.pane_infos = vec![info];
        let mut findr = FindrState::new(pane_id);
        findr.query = "needle".to_owned();
        app.state.findr = Some(findr);

        assert!(app.refresh_findr_visible_if_needed(&HashSet::from([pane_id])));
        assert_eq!(app.state.findr.as_ref().unwrap().matches.len(), 1);
        assert!(!app.refresh_findr_visible_if_needed(&HashSet::from([
            crate::layout::PaneId::from_raw(2),
        ])));

        app.state.view.pane_infos[0].inner_rect.width = 21;
        app.state
            .findr_target_runtime(&app.terminal_runtimes)
            .unwrap()
            .resize(3, 21, 0, 0);
        assert!(app.refresh_findr_visible_if_needed(&HashSet::new()));
        assert_eq!(
            app.state.findr.as_ref().unwrap().visible_geometry,
            Some((21, 3))
        );

        app.state
            .findr_target_runtime(&app.terminal_runtimes)
            .unwrap()
            .test_process_pty_bytes(b"\r      ");
        assert!(app.refresh_findr_visible_if_needed(&HashSet::from([pane_id])));
        assert!(app.state.findr.as_ref().unwrap().matches.is_empty());
    }

    #[tokio::test]
    async fn visible_findr_scans_viewports_larger_than_one_cell_budget() {
        let mut state = crate::app::state::AppState::test_new();
        let mut workspace = crate::workspace::Workspace::test_new("large-findr");
        let pane_id = workspace.tabs[0].root_pane;
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 82, 502));
        let bytes = (0..700)
            .map(|row| {
                if row == 210 {
                    "large viewport target\r\n".to_owned()
                } else {
                    format!("line {row}\r\n")
                }
            })
            .collect::<String>();
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                pane_infos[0].inner_rect.width,
                pane_infos[0].inner_rect.height,
                128 * 1024,
                bytes.as_bytes(),
            ),
        );
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.view.pane_infos = pane_infos;
        let mut findr = FindrState::new(pane_id);
        findr.query = "large viewport target".into();
        state.findr = Some(findr);
        state.mode = Mode::Findr;
        let runtimes = TerminalRuntimeRegistry::new();

        assert!(state.refresh_findr_visible(&runtimes));
        assert!(!state.findr.as_ref().unwrap().complete);
        for _ in 0..8 {
            if state.findr.as_ref().unwrap().complete {
                break;
            }
            assert!(state.tick_findr_scan(&runtimes));
        }

        let findr = state.findr.as_ref().unwrap();
        assert!(findr.complete);
        assert_eq!(findr.matches.len(), 1);
    }

    #[tokio::test]
    async fn scrollback_findr_includes_rows_below_scrolled_viewport() {
        let mut state = crate::app::state::AppState::test_new();
        let mut workspace = crate::workspace::Workspace::test_new("scrolled-findr");
        let pane_id = workspace.tabs[0].root_pane;
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 20, 3));
        let bytes = (0..30)
            .map(|row| {
                if row == 29 {
                    "newer needle\r\n".to_owned()
                } else {
                    format!("line {row}\r\n")
                }
            })
            .collect::<String>();
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                pane_infos[0].inner_rect.width,
                pane_infos[0].inner_rect.height,
                16 * 1024,
                bytes.as_bytes(),
            ),
        );
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.view.pane_infos = pane_infos;
        let runtimes = TerminalRuntimeRegistry::new();
        state.scroll_pane_up(&runtimes, pane_id, 20);
        let mut findr = FindrState::new(pane_id);
        findr.query = "newer needle".into();
        findr.scrollback = true;
        state.findr = Some(findr);
        state.mode = Mode::Findr;

        assert!(state.refresh_findr_visible(&runtimes));
        while !state.findr.as_ref().unwrap().complete {
            assert!(state.tick_findr_scan(&runtimes));
        }

        assert_eq!(state.findr.as_ref().unwrap().matches.len(), 1);
    }

    #[tokio::test]
    async fn scrollback_findr_restarts_when_its_target_runtime_is_dirty() {
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("dirty-findr");
        let pane_id = workspace.tabs[0].root_pane;
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 20, 3));
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                pane_infos[0].inner_rect.width,
                pane_infos[0].inner_rect.height,
                16 * 1024,
                b"old output\r\n",
            ),
        );
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.view.pane_infos = pane_infos;
        let mut findr = FindrState::new(pane_id);
        findr.query = "fresh".into();
        findr.scrollback = true;
        app.state.findr = Some(findr);
        app.state.mode = Mode::Findr;

        assert!(app.state.refresh_findr_visible(&app.terminal_runtimes));
        assert!(app.state.findr.as_ref().unwrap().complete);
        assert!(app.state.findr.as_ref().unwrap().matches.is_empty());

        app.state
            .findr_target_runtime(&app.terminal_runtimes)
            .unwrap()
            .test_process_pty_bytes(b"fresh\r\n");
        assert!(app.refresh_findr_visible_if_needed(&HashSet::from([pane_id])));
        assert_eq!(app.state.findr.as_ref().unwrap().matches.len(), 1);
    }

    #[tokio::test]
    async fn findr_targets_focused_workspace_plugin_runtime() {
        let mut state = crate::app::state::AppState::test_new();
        let workspace = crate::workspace::Workspace::test_new("plugin-findr");
        let workspace_id = workspace.id.clone();
        let plugin_pane_id = crate::layout::PaneId::alloc();
        let plugin_terminal_id = crate::terminal::TerminalId::alloc();
        let mut runtimes = TerminalRuntimeRegistry::new();
        runtimes.insert(
            plugin_terminal_id.clone(),
            crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 3, b"plugin needle"),
        );
        state.workspaces = vec![workspace];
        state.active = Some(0);
        state.view.layout = crate::app::state::ViewLayout::Desktop;
        state.view.workspace_plugin_pane_inner = Rect::new(40, 1, 40, 3);
        state.workspace_plugin_panes.insert(
            workspace_id,
            crate::app::state::WorkspacePluginPaneState {
                pane_id: plugin_pane_id,
                terminal_id: plugin_terminal_id,
                plugin_id: "example.findr".into(),
                entrypoint: "explorer".into(),
                width: None,
                focused: true,
                collapsed: false,
            },
        );

        state.open_findr();
        assert_eq!(state.mode, Mode::Findr);
        assert_eq!(state.findr.as_ref().unwrap().pane_id, plugin_pane_id);
        state.insert_findr_text(&runtimes, "needle");
        assert_eq!(state.findr.as_ref().unwrap().matches.len(), 1);
    }
}
