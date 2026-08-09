use super::App;
use crate::terminal::state::TerminalTitleChange;

impl App {
    pub(crate) fn terminal_title_sidebar_needs_render(&self, change: TerminalTitleChange) -> bool {
        let config = &self.state.sidebar_agents;
        std::iter::once(&config.rows)
            .chain(config.rows_by_agent.values())
            .flatten()
            .flatten()
            .any(|token| match token.parts().0 {
                crate::config::AgentSidebarToken::TerminalTitle => change.raw_changed,
                crate::config::AgentSidebarToken::TerminalTitleStripped => change.stripped_changed,
                _ => false,
            })
    }

    pub(crate) fn sync_terminal_titles(&mut self) -> TerminalTitleChange {
        let mut observations = Vec::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for (pane_id, pane) in &tab.panes {
                    let terminal_id = &pane.attached_terminal_id;
                    let Some(runtime) = self.terminal_runtimes.get(terminal_id) else {
                        continue;
                    };
                    observations.push((
                        ws_idx,
                        *pane_id,
                        terminal_id.clone(),
                        runtime.terminal_title(),
                    ));
                }
            }
        }

        let mut aggregate = TerminalTitleChange::default();
        let mut publish = Vec::new();
        for (ws_idx, pane_id, terminal_id, title) in observations {
            let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
                continue;
            };
            let change = terminal.set_terminal_title(title);
            aggregate.raw_changed |= change.raw_changed;
            aggregate.stripped_changed |= change.stripped_changed;
            if change.stripped_changed {
                publish.push((ws_idx, pane_id));
            }
        }

        for (ws_idx, pane_id) in publish {
            self.emit_pane_updated(ws_idx, pane_id);
        }

        aggregate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    #[tokio::test]
    async fn sync_keeps_latest_raw_title_and_emits_only_for_stripped_changes() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.detected_agent = Some(Agent::Claude);
        terminal.state = AgentState::Working;
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes("\x1b]0;π ⠋ 修复🙂标题\x07".as_bytes());
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);

        assert_eq!(
            app.sync_terminal_titles(),
            TerminalTitleChange {
                raw_changed: true,
                stripped_changed: true,
            }
        );
        let pane = app.pane_info(0, pane_id).unwrap();
        assert_eq!(pane.terminal_title.as_deref(), Some("π ⠋ 修复🙂标题"));
        assert_eq!(
            pane.terminal_title_stripped.as_deref(),
            Some("π 修复🙂标题")
        );
        assert_eq!(pane.title, None);
        assert_eq!(pane.agent_status, crate::api::schema::AgentStatus::Working);
        assert_eq!(pane.revision, 1);
        let agent = app.collect_agent_infos().pop().unwrap();
        assert_eq!(agent.terminal_title.as_deref(), Some("π ⠋ 修复🙂标题"));
        assert_eq!(
            agent.terminal_title_stripped.as_deref(),
            Some("π 修复🙂标题")
        );

        app.terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes("\x1b]2;π ⠙ 修复🙂标题\x1b\\".as_bytes());
        let spinner_change = app.sync_terminal_titles();
        assert!(spinner_change.raw_changed);
        assert!(!spinner_change.stripped_changed);
        app.state.sidebar_agents.rows = vec![vec![
            crate::config::AgentSidebarToken::TerminalTitleStripped,
        ]];
        assert!(!app.terminal_title_sidebar_needs_render(spinner_change));
        app.state.sidebar_agents.rows = vec![vec![crate::config::AgentSidebarToken::TerminalTitle]];
        assert!(app.terminal_title_sidebar_needs_render(spinner_change));
        let pane = app.pane_info(0, pane_id).unwrap();
        assert_eq!(pane.terminal_title.as_deref(), Some("π ⠙ 修复🙂标题"));
        assert_eq!(
            pane.terminal_title_stripped.as_deref(),
            Some("π 修复🙂标题")
        );
        assert_eq!(pane.revision, 1);
        assert_eq!(pane_updated_events(&event_hub), 1);

        app.terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b]0;Done reviewing\x07");
        assert_eq!(
            app.sync_terminal_titles(),
            TerminalTitleChange {
                raw_changed: true,
                stripped_changed: true,
            }
        );
        assert_eq!(pane_updated_events(&event_hub), 2);

        app.terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b]0;\x07");
        assert_eq!(
            app.sync_terminal_titles(),
            TerminalTitleChange {
                raw_changed: true,
                stripped_changed: true,
            }
        );
        let pane = app.pane_info(0, pane_id).unwrap();
        assert_eq!(pane.terminal_title, None);
        assert_eq!(pane.terminal_title_stripped, None);
        assert_eq!(pane.revision, 3);
        assert_eq!(pane_updated_events(&event_hub), 3);
    }

    #[test]
    fn override_only_stripped_title_token_filters_raw_only_redraws() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub);
        app.state.sidebar_agents.rows = vec![vec![crate::config::AgentSidebarToken::Agent]];
        app.state.sidebar_agents.rows_by_agent.insert(
            "claude".into(),
            vec![vec![
                crate::config::AgentSidebarToken::TerminalTitleStripped,
            ]],
        );

        let stripped_change = TerminalTitleChange {
            raw_changed: false,
            stripped_changed: true,
        };
        assert!(app.terminal_title_sidebar_needs_render(stripped_change));
        let raw_change = TerminalTitleChange {
            raw_changed: true,
            stripped_changed: false,
        };
        assert!(!app.terminal_title_sidebar_needs_render(raw_change));
    }

    fn pane_updated_events(event_hub: &crate::api::EventHub) -> usize {
        event_hub
            .events_after(0)
            .iter()
            .filter(|(_, event)| event.event == crate::api::schema::EventKind::PaneUpdated)
            .count()
    }
}
