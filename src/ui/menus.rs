use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::text::{display_width_u16, tail_within_width};
use super::widgets::{panel_contrast_fg, render_panel_shell};
use crate::app::AppState;

fn prefix_rhs_label(bindings: &crate::config::ActionKeybinds) -> String {
    bindings
        .prefix_rhs_label()
        .unwrap_or_else(|| "unset".to_string())
}

fn keybind_label(bindings: &crate::config::ActionKeybinds) -> String {
    bindings.label().unwrap_or_else(|| "unset".to_string())
}

fn bottom_bar_styles(app: &AppState, mode_bg: Color) -> (Style, Style, Style) {
    (
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
        Style::default().fg(app.palette.overlay0),
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(mode_bg)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_bottom_bar(frame: &mut Frame, area: Rect, line: Line<'_>, bg: Color) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    let bar = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    frame.render_widget(Clear, bar);
    let buf = frame.buffer_mut();
    for x in bar.x..bar.x.saturating_add(bar.width) {
        buf[(x, bar.y)].set_style(Style::default().bg(bg));
    }
    frame.render_widget(Paragraph::new(line), bar);
    bar
}

pub(super) fn render_prefix_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.accent);

    let workspace_picker = prefix_rhs_label(&app.keybinds.workspace_picker);
    let help = prefix_rhs_label(&app.keybinds.help);
    let prefix = crate::config::format_key_combo((app.prefix_code, app.prefix_mods));

    let line = Line::from(vec![
        Span::styled(" PREFIX ", mode_style),
        Span::raw(" "),
        Span::styled("esc", key),
        Span::styled(" cancel  ", dim),
        Span::styled(prefix, key),
        Span::styled(" send prefix  ", dim),
        Span::styled(workspace_picker, key),
        Span::styled(" workspace nav  ", dim),
        Span::styled(help, key),
        Span::styled(" keybinds", dim),
    ]);

    render_bottom_bar(frame, area, line, app.palette.panel_bg);
}

pub(super) fn render_copy_mode_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.accent);

    let Some(copy_mode) = app.copy_mode.as_ref() else {
        return;
    };
    let line = if let Some(prompt) = copy_mode.search.prompt.as_ref() {
        let marker = match prompt.direction {
            crate::app::state::CopyModeSearchDirection::Forward => "/",
            crate::app::state::CopyModeSearchDirection::Backward => "?",
        };
        let help = "  enter search  esc cancel";
        let base_width = 9;
        let help_width = display_width_u16(help);
        let query = if display_width_u16(&prompt.query)
            .saturating_add(base_width)
            .saturating_add(help_width)
            <= area.width
        {
            prompt.query.as_str()
        } else {
            tail_within_width(
                &prompt.query,
                usize::from(area.width.saturating_sub(base_width)),
            )
        };
        let show_help = display_width_u16(query)
            .saturating_add(base_width)
            .saturating_add(help_width)
            <= area.width;
        let mut spans = vec![
            Span::styled(" COPY ", mode_style),
            Span::raw(" "),
            Span::styled(marker, key),
            Span::styled(query, Style::default().fg(app.palette.text)),
            Span::styled("█", key),
        ];
        if show_help {
            spans.push(Span::styled(help, dim));
        }
        Line::from(spans)
    } else {
        let select = if copy_mode.selection.is_some() {
            "selecting"
        } else {
            "select"
        };
        let match_status = copy_mode
            .search
            .current
            .map(|current| format!(" {}/{}", current + 1, copy_mode.search.matches.len()))
            .or_else(|| (!copy_mode.search.query.is_empty()).then(|| " 0/0".to_string()))
            .unwrap_or_default();
        let (exit_keys, exit_label) =
            if copy_mode.search.query.is_empty() && copy_mode.selection.is_none() {
                ("q/esc", " exit")
            } else {
                ("esc", " clear  q exit")
            };
        Line::from(vec![
            Span::styled(" COPY ", mode_style),
            Span::raw(" "),
            Span::styled("h/j/k/l w/b/e { }", key),
            Span::styled(" move  ", dim),
            Span::styled("/ ?", key),
            Span::styled(" search  ", dim),
            Span::styled("n/N", key),
            Span::styled(format!(" repeat{match_status}  "), dim),
            Span::styled("v/space", key),
            Span::styled(format!(" {select}  "), dim),
            Span::styled("y/enter", key),
            Span::styled(" copy  ", dim),
            Span::styled(exit_keys, key),
            Span::styled(exit_label, dim),
        ])
    };

    render_bottom_bar(frame, area, line, app.palette.panel_bg);
}

const FINDR_BASE_WIDTH: u16 = 9;
const FINDR_CHECKBOX_WIDTH: u16 = 14;
const FINDR_CHECKBOX_OFFSET: u16 = 11;
const FINDR_TOGGLE_RENDER_WIDTH: u16 = 16;

fn findr_visible_matches(findr: &crate::app::state::FindrState) -> usize {
    findr
        .visible_range
        .map(|(top, bottom)| {
            findr
                .matches
                .iter()
                .filter(|text_match| text_match.end.row >= top && text_match.start.row < bottom)
                .count()
        })
        .unwrap_or(findr.matches.len())
}

fn findr_status(findr: &crate::app::state::FindrState) -> String {
    let visible_matches = findr_visible_matches(findr);
    if findr.query.is_empty() {
        String::new()
    } else if findr.budget_limited {
        format!(" {visible_matches} visible matches (pane too wide)")
    } else if findr.capped {
        format!(" {visible_matches} visible matches (4096+)")
    } else if !findr.complete {
        format!(" {visible_matches} visible matches scanning")
    } else {
        format!(" {visible_matches} visible matches")
    }
}

fn findr_overlay_shows_toggle(area: Rect, status_width: u16) -> bool {
    area.width
        >= FINDR_CHECKBOX_OFFSET
            .saturating_add(FINDR_CHECKBOX_WIDTH)
            .saturating_add(status_width)
}

fn findr_overlay_query(
    findr: &crate::app::state::FindrState,
    area: Rect,
    status_width: u16,
    show_toggle: bool,
) -> &str {
    let chrome_width = if show_toggle {
        FINDR_CHECKBOX_OFFSET.saturating_add(FINDR_CHECKBOX_WIDTH)
    } else {
        FINDR_BASE_WIDTH
    }
    .saturating_add(status_width);
    let budget = area.width.saturating_sub(chrome_width);
    tail_within_width(&findr.query, usize::from(budget))
}

pub(crate) fn findr_scrollback_toggle_rect(app: &AppState, area: Rect) -> Rect {
    let Some(findr) = app.findr.as_ref() else {
        return Rect::default();
    };
    let status_width = display_width_u16(&findr_status(findr));
    if !findr_overlay_shows_toggle(area, status_width) {
        return Rect::default();
    }
    let query_width = display_width_u16(findr_overlay_query(findr, area, status_width, true));
    let x = area
        .x
        .saturating_add(FINDR_CHECKBOX_OFFSET)
        .saturating_add(query_width);
    let y = area.y.saturating_add(area.height.saturating_sub(1));
    if area.height > 0 {
        Rect::new(x, y, FINDR_CHECKBOX_WIDTH, 1)
    } else {
        Rect::default()
    }
}

pub(super) fn render_findr_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(findr) = app.findr.as_ref() else {
        return;
    };
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.accent);
    let check = if findr.scrollback { "x" } else { " " };
    let status = findr_status(findr);
    let status_width = display_width_u16(&status);
    let show_toggle = findr_overlay_shows_toggle(area, status_width);
    let query = findr_overlay_query(findr, area, status_width, show_toggle);
    let help = "  ↑↓/pgup/pgdn scroll  tab toggle  esc close";
    let toggle = findr_scrollback_toggle_rect(app, area);
    let mut spans = vec![
        Span::styled(" FINDR ", mode_style),
        Span::raw(" "),
        Span::styled(query, Style::default().fg(app.palette.text)),
        Span::styled("█", key),
    ];
    if !toggle.is_empty() {
        spans.push(Span::styled(format!("  [{check}]"), key));
        spans.push(Span::styled(" scrollback", dim));
    }
    let toggle_width = if toggle.is_empty() {
        0
    } else {
        FINDR_TOGGLE_RENDER_WIDTH
    };
    let used = FINDR_BASE_WIDTH
        .saturating_add(display_width_u16(query))
        .saturating_add(toggle_width);
    if used.saturating_add(display_width_u16(&status)) <= area.width {
        spans.push(Span::styled(&status, dim));
        if used
            .saturating_add(display_width_u16(&status))
            .saturating_add(display_width_u16(help))
            <= area.width
        {
            spans.push(Span::styled(help, dim));
        }
    }
    let bar = render_bottom_bar(frame, area, Line::from(spans), app.palette.panel_bg);
    if bar.width > 0 {
        let cursor_x = bar
            .x
            .saturating_add(8)
            .saturating_add(display_width_u16(query))
            .min(bar.x.saturating_add(bar.width.saturating_sub(1)));
        frame.set_cursor_position((cursor_x, bar.y));
    }
}

pub(super) fn render_navigate_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.accent);

    let kb = &app.keybinds;
    let new_tab = prefix_rhs_label(&kb.new_tab);
    let split_vertical = prefix_rhs_label(&kb.split_vertical);
    let split_horizontal = prefix_rhs_label(&kb.split_horizontal);
    let close_pane = prefix_rhs_label(&kb.close_pane);
    let zoom = prefix_rhs_label(&kb.zoom);
    let resize = prefix_rhs_label(&kb.resize_mode);
    let help = prefix_rhs_label(&kb.help);
    let settings = prefix_rhs_label(&kb.settings);
    let goto = prefix_rhs_label(&kb.goto);
    let detach = prefix_rhs_label(&kb.detach);
    let workspace_nav = format!(
        "{} / {}",
        keybind_label(&kb.navigate.workspace_up),
        keybind_label(&kb.navigate.workspace_down)
    );
    let line = Line::from(vec![
        Span::styled(" NAVIGATE ", mode_style),
        Span::raw(" "),
        Span::styled("esc", key),
        Span::styled(" back  ", dim),
        Span::styled(workspace_nav, key),
        Span::styled(" ws  ", dim),
        Span::styled("⇥", key),
        Span::styled(" pane  ", dim),
        Span::styled(goto, key),
        Span::styled(" navigator  ", dim),
        Span::styled(new_tab, key),
        Span::styled(" new tab  ", dim),
        Span::styled(split_vertical, key),
        Span::styled(" split│  ", dim),
        Span::styled(split_horizontal, key),
        Span::styled(" split─  ", dim),
        Span::styled(close_pane, key),
        Span::styled(" close  ", dim),
        Span::styled(zoom, key),
        Span::styled(" zoom  ", dim),
        Span::styled(resize, key),
        Span::styled(" resize  ", dim),
        Span::styled(help, key),
        Span::styled(" keybinds  ", dim),
        Span::styled(settings, key),
        Span::styled(" settings  ", dim),
        Span::styled(detach, key),
        Span::styled(" detach", dim),
    ]);

    let overlay_area = render_bottom_bar(frame, area, line, app.palette.panel_bg);

    if app.update_available.is_some() {
        let status = Line::from(vec![Span::styled(
            " update ready",
            Style::default()
                .fg(app.palette.accent)
                .add_modifier(Modifier::BOLD),
        )]);
        let width = 13u16.min(overlay_area.width);
        let status_area = Rect::new(
            overlay_area.x + overlay_area.width.saturating_sub(width),
            overlay_area.y,
            width,
            overlay_area.height,
        );
        frame.render_widget(Clear, status_area);
        frame.render_widget(
            Paragraph::new(status).alignment(Alignment::Right),
            status_area,
        );
    }
}

pub(super) fn render_workspace_plugin_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.accent);
    let prefix = crate::config::format_key_combo((app.prefix_code, app.prefix_mods));
    let line = Line::from(vec![
        Span::styled(" EXPLORER ", mode_style),
        Span::raw(" "),
        Span::styled("esc", key),
        Span::styled(" back  ", dim),
        Span::styled(prefix, key),
        Span::styled(" back  ", dim),
        Span::styled("↑↓/jk", key),
        Span::styled(" move  ", dim),
        Span::styled("←→/hl", key),
        Span::styled(" tree  ", dim),
        Span::styled("enter", key),
        Span::styled(" open", dim),
    ]);

    render_bottom_bar(frame, area, line, app.palette.panel_bg);
}

pub(super) fn render_global_launcher_menu(app: &AppState, frame: &mut Frame) {
    let rect = app.global_menu_rect();
    let Some(inner) = render_panel_shell(frame, rect, app.palette.accent, app.palette.panel_bg)
    else {
        return;
    };

    let items = app.global_menu_labels();
    for (idx, item) in items.iter().enumerate() {
        let y = inner.y + idx as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let selected = idx == app.global_menu.highlighted;
        let rect = Rect::new(inner.x, y, inner.width, 1);

        let selected_style = Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD);
        let item_style = if selected {
            selected_style
        } else {
            Style::default().fg(app.palette.text)
        };
        let badge_style = if selected {
            selected_style
        } else {
            Style::default()
                .fg(app.palette.accent)
                .add_modifier(Modifier::BOLD)
        };

        let line = if app.global_menu_item_has_badge(item) {
            Line::from(vec![
                Span::styled(" ●", badge_style),
                Span::styled(format!(" {item} "), item_style),
            ])
        } else {
            Line::from(Span::styled(format!(" {item} "), item_style))
        };
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Left), rect);
    }
}

pub(super) fn render_resize_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (key, dim, mode_style) = bottom_bar_styles(app, app.palette.mauve);

    let line = Line::from(vec![
        Span::styled(" RESIZE ", mode_style),
        Span::raw("  "),
        Span::styled("h/l", key),
        Span::styled(" width  ", dim),
        Span::styled("j/k", key),
        Span::styled(" height  ", dim),
        Span::styled("esc", key),
        Span::styled(" done", dim),
    ]);

    render_bottom_bar(frame, area, line, app.palette.panel_bg);
}

pub(super) fn render_context_menu(app: &AppState, frame: &mut Frame) {
    let Some(menu) = &app.context_menu else {
        return;
    };

    let p = &app.palette;
    let Some(menu_rect) = app.context_menu_rect() else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, menu_rect, p.accent, p.panel_bg) else {
        return;
    };

    let items: Vec<ListItem> = menu
        .items()
        .iter()
        .map(|item| ListItem::new(Line::from(*item)))
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(p.text))
        .highlight_style(
            Style::default()
                .bg(p.accent)
                .fg(panel_contrast_fg(p))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ");
    let mut state = ListState::default().with_selected(Some(menu.list.highlighted));
    frame.render_stateful_widget(list, inner, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn workspace_plugin_footer_lists_escape_back() {
        let app = AppState::test_new();
        let mut terminal = Terminal::new(TestBackend::new(120, 1)).unwrap();

        terminal
            .draw(|frame| render_workspace_plugin_overlay(&app, frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<Vec<_>>()
            .concat();
        assert!(text.contains("esc back"), "{text}");
    }

    #[test]
    fn findr_overlay_names_mode_and_aligns_scrollback_toggle() {
        let mut app = AppState::test_new();
        let mut findr = crate::app::state::FindrState::new(crate::layout::PaneId::from_raw(1));
        findr.query = "needle".to_string();
        app.findr = Some(findr);
        let mut terminal = Terminal::new(TestBackend::new(120, 1)).unwrap();

        terminal
            .draw(|frame| render_findr_overlay(&app, frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<Vec<_>>()
            .concat();
        assert!(text.contains("FINDR  needle█  [ ] scrollback"), "{text}");
        let toggle = findr_scrollback_toggle_rect(&app, buffer.area);
        assert_eq!(buffer[(toggle.x, toggle.y)].symbol(), "[");
        assert_eq!(buffer[(toggle.x + 2, toggle.y)].symbol(), "]");
    }

    #[test]
    fn findr_overlay_keeps_status_visible_before_toggle() {
        let mut app = AppState::test_new();
        let mut findr = crate::app::state::FindrState::new(crate::layout::PaneId::from_raw(1));
        findr.query = "a very long query that cannot fit".to_string();
        findr.complete = true;
        app.findr = Some(findr);
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();

        terminal
            .draw(|frame| render_findr_overlay(&app, frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<Vec<_>>()
            .concat();
        assert!(text.contains("0 visible matches"), "{text}");
        assert!(!text.contains("scrollback"), "{text}");
    }

    #[test]
    fn bottom_bar_ignores_empty_area() {
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).unwrap();

        terminal
            .draw(|frame| {
                assert_eq!(
                    render_bottom_bar(frame, Rect::default(), Line::raw("ignored"), Color::Black),
                    Rect::default()
                );
            })
            .unwrap();
    }
}
