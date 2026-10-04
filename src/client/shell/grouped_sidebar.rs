//! Client-only integration of the portable grouping projection. Routing remains endpoint-local.
use super::grouped_projection::{GroupKey, GroupedRow};
use super::render::{display_width, put_right_text, put_text, ShellRenderState};
use super::*;

impl ClientShellState {
    /// Called at metadata/catalog/config boundaries, never by the rendering loop.
    pub(super) fn invalidate_grouped_projection(&mut self) {
        self.grouped_projection_dirty = true;
        if self.config.grouping.enabled {
            // Geometry from a previous boot/catalog must not route to a recycled ID.
            self.hits.workspaces.clear();
            self.hits.grouped_headings.clear();
            self.hits.mobile_targets.clear();
            self.workspace_press = None;
            if matches!(self.chrome_drag, Some(ClientChromeDrag::Workspace { .. })) {
                self.chrome_drag = None;
            }
        }
    }

    pub(super) fn prepare_grouped_view(&mut self, cols: u16, rows: u16) {
        self.refresh_grouped_projection();
        if !self.config.grouping.enabled {
            return;
        }
        let target = if self.reveal_navigation_workspace {
            self.navigate_workspace_id
                .as_ref()
                .or(self.grouped_reveal_target.as_ref())
                .cloned()
        } else if self.reveal_focused_workspace {
            self.focused_navigation_target()
        } else {
            None
        };
        if let Some(target) = target.filter(|target| self.navigation_target_valid(target)) {
            self.reveal_grouped_workspace(&target.endpoint_id, &target.workspace_id);
        }
        // Mobile has no sidebar renderer to consume these one-shot reveal flags.
        if self.snapshot.is_some()
            && self.pane_surface.is_some()
            && !self.layout(cols, rows).mobile_header.is_empty()
        {
            self.reveal_mobile_workspace |= self.reveal_navigation_workspace;
            self.reveal_navigation_workspace = false;
            self.reveal_focused_workspace = false;
        }
    }

    pub(super) fn refresh_grouped_projection(&mut self) {
        if !self.grouped_projection_dirty {
            return;
        }
        self.grouped_projection_dirty = false;
        #[cfg(test)]
        if self.config.grouping.enabled {
            self.grouped_projection_rebuilds += 1;
        }
        self.grouped_rows = if self.config.grouping.enabled {
            super::grouped_projection::project(&self.endpoints, &self.config.grouping)
        } else {
            Vec::new()
        };
        self.refresh_grouped_visibility();
    }

    fn refresh_grouped_visibility(&mut self) {
        self.grouped_visible_rows =
            super::grouped_projection::visible_rows(&self.grouped_rows, &self.grouped_collapsed);
    }

    pub(super) fn toggle_grouped_heading(&mut self, key: GroupKey) {
        self.refresh_grouped_projection();
        if !self.grouped_collapsed.remove(&key) {
            self.grouped_collapsed.insert(key);
        }
        self.refresh_grouped_visibility();
        // Manual heading changes supersede pending reveal requests, just like '-'.
        // Subsequent keyboard navigation or focus changes can request a fresh reveal.
        self.reveal_navigation_workspace = false;
        self.reveal_focused_workspace = false;
        self.reveal_mobile_workspace = false;
        self.hits.workspaces.clear();
        self.hits.grouped_headings.clear();
        self.hits.mobile_targets.clear();
        self.workspace_press = None;
    }

    pub(super) fn handle_grouped_heading_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let key = self
            .hits
            .grouped_headings
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, key)| key.clone());
        let Some(key) = key else {
            return false;
        };
        self.toggle_grouped_heading(key);
        self.persist_chrome_preferences(outcome);
        outcome.repaint = true;
        true
    }

    /// Grouped-only presentation shortcuts leave existing pane-arrow bindings untouched.
    pub(super) fn handle_grouped_navigation_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.config.grouping.enabled
            || !key
                .modifiers
                .difference(crossterm::event::KeyModifiers::SHIFT)
                .is_empty()
        {
            return false;
        }
        match key.code {
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.refresh_grouped_projection();
                self.grouped_collapsed.clear();
                self.refresh_grouped_visibility();
                self.reveal_navigation_workspace = true;
                self.reveal_mobile_workspace = true;
            }
            KeyCode::Char('-') if key.modifiers.is_empty() => {
                self.refresh_grouped_projection();
                let target = self
                    .navigate_workspace_id
                    .clone()
                    .or_else(|| self.focused_navigation_target());
                if let Some(target) = target {
                    let mut nearest = None;
                    for row in &self.grouped_rows {
                        match row {
                            GroupedRow::Heading { key, .. } => nearest = Some(key.clone()),
                            GroupedRow::Workspace {
                                endpoint, index, ..
                            } => {
                                if self.endpoints.get(*endpoint).is_some_and(|endpoint| {
                                    endpoint.endpoint_id == target.endpoint_id
                                        && endpoint
                                            .snapshot
                                            .as_deref()
                                            .and_then(|snapshot| snapshot.workspaces.get(*index))
                                            .is_some_and(|workspace| {
                                                workspace.workspace_id == target.workspace_id
                                            })
                                }) {
                                    if let Some(key) = nearest {
                                        self.grouped_collapsed.insert(key);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    self.refresh_grouped_visibility();
                    // A manual collapse must not be immediately undone by reveal.
                    self.reveal_navigation_workspace = false;
                    self.reveal_focused_workspace = false;
                    self.reveal_mobile_workspace = false;
                }
            }
            _ => return false,
        }
        self.hits.workspaces.clear();
        self.hits.grouped_headings.clear();
        self.hits.mobile_targets.clear();
        self.workspace_press = None;
        self.persist_chrome_preferences(outcome);
        outcome.repaint = true;
        true
    }

    /// The same visible leaf order is used by expanded, compact, mobile and keyboard views.
    pub(super) fn grouped_navigation_targets(&mut self) -> Vec<WorkspaceNavigationTarget> {
        self.refresh_grouped_projection();
        self.grouped_visible_rows
            .iter()
            .filter_map(|row| {
                let GroupedRow::Workspace {
                    endpoint, index, ..
                } = row
                else {
                    return None;
                };
                let endpoint = self.endpoints.get(*endpoint)?;
                if endpoint.status != ClientEndpointStatus::Online {
                    return None;
                }
                let workspace = endpoint.snapshot.as_deref()?.workspaces.get(*index)?;
                self.navigation_target(&endpoint.endpoint_id, &workspace.workspace_id)
            })
            .collect()
    }

    /// Open only ancestors of a real, endpoint-qualified leaf, without focusing a heading.
    pub(super) fn reveal_grouped_workspace(
        &mut self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &str,
    ) {
        self.refresh_grouped_projection();
        let mut ancestors = Vec::<(u16, GroupKey)>::new();
        let mut found = false;
        for row in &self.grouped_rows {
            match row {
                GroupedRow::Heading { key, depth, .. } => {
                    ancestors.retain(|(parent_depth, _)| parent_depth < depth);
                    ancestors.push((*depth, key.clone()));
                }
                GroupedRow::Workspace {
                    endpoint, index, ..
                } => {
                    if self.endpoints.get(*endpoint).is_some_and(|endpoint| {
                        &endpoint.endpoint_id == endpoint_id
                            && endpoint
                                .snapshot
                                .as_deref()
                                .and_then(|snapshot| snapshot.workspaces.get(*index))
                                .is_some_and(|workspace| workspace.workspace_id == workspace_id)
                    }) {
                        found = true;
                        break;
                    }
                }
            }
        }
        if found {
            let mut changed = false;
            for (_, key) in ancestors {
                changed |= self.grouped_collapsed.remove(&key);
            }
            if changed {
                self.refresh_grouped_visibility();
            }
        }
    }
}

fn workspace_for_row<'a>(
    row: &GroupedRow,
    endpoints: &'a [ClientShellEndpoint],
) -> Option<(&'a ClientShellEndpoint, &'a ClientShellWorkspace)> {
    let GroupedRow::Workspace {
        endpoint, index, ..
    } = row
    else {
        return None;
    };
    let endpoint = endpoints.get(*endpoint)?;
    Some((
        endpoint,
        endpoint.snapshot.as_deref()?.workspaces.get(*index)?,
    ))
}

pub(super) fn render(
    buffer: &mut Buffer,
    area: Rect,
    active_snapshot: Option<&ClientShellSnapshot>,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    super::render::render_sidebar_background(buffer, area, palette);
    let compact = state.sidebar_collapsed;
    let (workspace_area, detail_area) = if compact {
        let (workspace, divider, detail) = super::sidebar::collapsed_sidebar_sections(area);
        if let Some(y) = divider {
            put_text(
                buffer,
                workspace.x,
                y,
                workspace.width,
                &"─".repeat(usize::from(workspace.width)),
                Style::default().fg(palette.surface_dim),
            );
        }
        (workspace, detail)
    } else {
        hits.sidebar_divider = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        hits.sidebar_section_divider =
            crate::ui::sidebar_section_divider_rect(area, state.sidebar_section_split);
        crate::ui::expanded_sidebar_sections(area, state.sidebar_section_split)
    };
    let body = if compact {
        workspace_area
    } else {
        put_text(
            buffer,
            workspace_area.x,
            workspace_area.y,
            workspace_area.width,
            " projects",
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
        Rect::new(
            workspace_area.x,
            workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
            workspace_area.width,
            workspace_area
                .height
                .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
        )
    };
    hits.workspace_body = body;
    // Connection diagnostics are always reachable, even when there are no projected leaves.
    // They share the scroll document, but are never keyboard workspace focus targets.
    let row_count = state.grouped_rows.len() + 1 + state.endpoints.len();
    let heights = (0..row_count)
        .map(|index| {
            if compact {
                return 1;
            }
            state
                .grouped_rows
                .get(index)
                .and_then(|row| {
                    let (_, workspace) = workspace_for_row(row, state.endpoints)?;
                    let GroupedRow::Workspace { label, .. } = row else {
                        return None;
                    };
                    Some(
                        super::sidebar::workspace_rows_with_label(
                            workspace,
                            workspace.agent_status,
                            label,
                            false,
                            &config.spaces,
                        )
                        .len()
                        .max(1)
                        .min(u16::MAX as usize) as u16,
                    )
                })
                .unwrap_or(1)
        })
        .collect::<Vec<_>>();
    let gaps = (0..row_count)
        .map(|index| {
            if !compact
                && matches!(
                    state.grouped_rows.get(index),
                    Some(GroupedRow::Workspace { .. })
                )
            {
                config.spaces.row_gap
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let reveal_navigation = !body.is_empty() && std::mem::take(state.reveal_navigation_workspace);
    let reveal_focus = !body.is_empty() && std::mem::take(state.reveal_focused_workspace);
    if reveal_navigation || reveal_focus {
        let target = state.grouped_rows.iter().position(|row| {
            workspace_for_row(row, state.endpoints).is_some_and(|(endpoint, workspace)| {
                if reveal_navigation {
                    state
                        .selected_workspace_id
                        .or(state.grouped_reveal_target)
                        .is_some_and(|target| {
                            target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
                        })
                } else {
                    &endpoint.endpoint_id == state.active_endpoint_id
                        && active_snapshot.is_some_and(|snapshot| {
                            snapshot.focused_workspace_id.as_deref()
                                == Some(workspace.workspace_id.as_str())
                        })
                }
            })
        });
        if let Some(target) = target {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
        }
    }
    let metrics =
        super::scroll::list_scroll_metrics(&heights, &gaps, body.height, *state.workspace_scroll);
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = body.width > 1 && metrics.max_offset_from_bottom > 0;
    let width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for row_index in *state.workspace_scroll..row_count {
        let height = heights[row_index].min(body.height);
        if y.saturating_add(height) > body.bottom() || body.is_empty() {
            break;
        }
        let rect = Rect::new(body.x, y, width, height);
        if let Some(row) = state.grouped_rows.get(row_index) {
            match row {
                GroupedRow::Heading { key, label, depth } => {
                    let marker = if state.grouped_collapsed.contains(key) {
                        "▸"
                    } else {
                        "▾"
                    };
                    let indent = if compact { 0 } else { depth.saturating_mul(2) };
                    put_text(
                        buffer,
                        rect.x.saturating_add(indent),
                        y,
                        width.saturating_sub(indent),
                        &format!("{marker} {label}"),
                        Style::default()
                            .fg(palette.overlay1)
                            .add_modifier(Modifier::BOLD),
                    );
                    hits.grouped_headings.push((rect, key.clone()));
                }
                GroupedRow::Workspace {
                    depth, label, host, ..
                } => {
                    let Some((endpoint, workspace)) = workspace_for_row(row, state.endpoints)
                    else {
                        continue;
                    };
                    let stale = endpoint.status != ClientEndpointStatus::Online;
                    let focused =
                        &endpoint.endpoint_id == state.active_endpoint_id && workspace.focused;
                    let selected = state.selected_workspace_id.is_some_and(|target| {
                        target.matches(&endpoint.endpoint_id, &workspace.workspace_id)
                    });
                    if compact {
                        if focused || selected {
                            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
                        }
                        put_text(
                            buffer,
                            rect.x,
                            y,
                            width,
                            &format!(
                                "{}{}",
                                workspace.number,
                                status_icon(workspace.agent_status, config.status_indicators)
                            ),
                            Style::default().fg(palette.text),
                        );
                    } else {
                        let indent = depth.saturating_mul(2).min(width.saturating_sub(1));
                        let host_label = format!("[{host}]");
                        let host_width =
                            display_width(&host_label).min(width.saturating_sub(indent + 3) / 2);
                        let nested = Rect::new(
                            rect.x.saturating_add(indent),
                            y,
                            width.saturating_sub(indent + host_width),
                            height,
                        );
                        let tokens = super::sidebar::workspace_rows_with_label(
                            workspace,
                            workspace.agent_status,
                            label,
                            false,
                            &config.spaces,
                        );
                        super::sidebar::render_workspace_rows(
                            buffer,
                            nested,
                            workspace.agent_status,
                            config.status_indicators,
                            &WorkspaceEntry {
                                index: 0,
                                indented: false,
                                last_child: false,
                            },
                            tokens,
                            focused,
                            selected,
                            state.selected_workspace_id.is_some(),
                            false,
                            palette,
                        );
                        put_right_text(
                            buffer,
                            rect,
                            y,
                            &crate::ui::truncate_end(&host_label, usize::from(host_width)),
                            Style::default().fg(palette.overlay0),
                        );
                    }
                    if stale {
                        buffer.set_style(
                            rect,
                            Style::default()
                                .fg(palette.overlay0)
                                .add_modifier(Modifier::DIM),
                        );
                    } else {
                        hits.workspaces.push(WorkspaceHit {
                            rect,
                            endpoint_id: endpoint.endpoint_id.clone(),
                            workspace_id: workspace.workspace_id.clone(),
                            indented: false,
                            group_toggle: None,
                        });
                    }
                }
            }
        } else if row_index == state.grouped_rows.len() {
            put_text(
                buffer,
                rect.x,
                y,
                width,
                " connections",
                Style::default().fg(palette.overlay0),
            );
        } else if let Some(endpoint) = state
            .endpoints
            .get(row_index - state.grouped_rows.len() - 1)
        {
            let badge = super::endpoint_sidebar::render_endpoint_row(
                buffer,
                rect,
                "·",
                endpoint,
                &endpoint.endpoint_id == state.active_endpoint_id,
                state.machine_diagnostics,
                palette,
            );
            hits.machines.push(MachineHit {
                rect,
                status_badge: badge,
                collapse_toggle: Rect::default(),
                endpoint_id: endpoint.endpoint_id.clone(),
            });
        }
        y = y.saturating_add(height).saturating_add(gaps[row_index]);
    }
    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }
    if compact {
        super::endpoint_agents::render_collapsed(
            buffer,
            detail_area,
            state.endpoints,
            state.active_endpoint_id,
            config,
            hits,
        );
    } else {
        let footer_y = workspace_area.bottom().saturating_sub(1);
        if config.mouse_capture && !workspace_area.is_empty() {
            let label = " new";
            hits.new_workspace =
                Rect::new(workspace_area.x, footer_y, 4.min(workspace_area.width), 1);
            put_text(
                buffer,
                workspace_area.x,
                footer_y,
                workspace_area.width,
                label,
                Style::default().fg(palette.overlay0),
            );
            hits.global_launcher = Rect::new(
                workspace_area.right().saturating_sub(6),
                footer_y,
                6.min(workspace_area.width),
                1,
            );
            put_right_text(
                buffer,
                workspace_area,
                footer_y,
                "menu",
                Style::default().fg(palette.overlay0),
            );
        }
        super::endpoint_agents::render_expanded(
            buffer,
            detail_area,
            active_snapshot.and_then(|snapshot| snapshot.agent_view_label.as_deref()),
            state.endpoints,
            state.active_endpoint_id,
            config,
            state.agent_scroll,
            hits,
        );
    }
    hits.sidebar_toggle = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(
            area.right().saturating_sub(2),
            area.bottom().saturating_sub(1),
            1,
            1,
        )
    };
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        if compact { "»" } else { "«" },
        Style::default().fg(palette.overlay0),
    );
}
