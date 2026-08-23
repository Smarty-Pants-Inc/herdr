use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    execution_target: crate::execution::ExecutionTarget,
    plan: Option<crate::agent_resume::AgentResumePlan>,
    rows: u16,
    cols: u16,
}

impl App {
    fn retry_timed_out_remote_agent_resumes(&mut self, now: Instant) -> bool {
        let pane_ids = self
            .state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.layout.pane_ids())
            .filter(|pane_id| {
                let Some(terminal_id) = self.state.terminal_id_for_runtime_pane(*pane_id) else {
                    return false;
                };
                let Some(terminal) = self.state.terminals.get(&terminal_id) else {
                    return false;
                };
                !terminal.execution_target.is_local()
                    && terminal.pending_agent_resume_plan.is_some()
                    && terminal.pending_agent_resume_confirmation_due(now)
                    && self
                        .terminal_runtimes
                        .get(&terminal_id)
                        .is_some_and(|runtime| {
                            terminal.pending_agent_resume_attempt_matches_peer(runtime.child_pid())
                        })
            })
            .collect::<Vec<_>>();

        if pane_ids.is_empty() {
            return false;
        }

        for pane_id in pane_ids {
            tracing::warn!(
                pane = pane_id.raw(),
                "remote agent resume confirmation timed out; retrying"
            );
            self.handle_internal_event(crate::events::AppEvent::PaneDied { pane_id });
        }
        true
    }

    pub(crate) fn sync_pending_agent_resume_retry_at(&mut self, now: Instant) {
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_retry_at = None;
        } else {
            self.pending_agent_resume_retry_at
                .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
        }
    }

    pub(crate) fn pending_agent_resume_retry_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_retry_at
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn next_pending_agent_resume_deadline(&self) -> Option<Instant> {
        std::iter::once(self.pending_agent_resume_retry_at)
            .chain(
                self.state
                    .terminals
                    .values()
                    .map(|terminal| terminal.pending_agent_resume_confirmation_deadline()),
            )
            .flatten()
            .min()
    }

    pub(crate) fn start_pending_agent_resumes(
        &mut self,
        now: Instant,
        allow_empty_theme: bool,
    ) -> bool {
        if self.retry_timed_out_remote_agent_resumes(now) {
            self.schedule_session_save();
            return true;
        }

        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            execution_target,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                execution_target,
                plan,
                rows,
                cols,
                now,
                allow_empty_theme,
            );
        }

        if changed {
            self.schedule_session_save();
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_retry_at = None;
        }
        changed
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let plan = terminal.pending_agent_resume_plan.clone();
                    if plan.is_none() && !terminal.respawn_shell_on_exit {
                        continue;
                    }
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: terminal.cwd.clone(),
                        execution_target: terminal.execution_target.clone(),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(
            tab,
            terminal_area,
            self.state.pane_borders,
            self.state.pane_gaps,
            self.state.pane_outer_borders,
        );

        if self.state.active == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, execution_target, plan)) =
            self.state.workspaces.iter().find_map(|ws| {
                ws.tabs.iter().find_map(|tab| {
                    tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                        let candidate_terminal_id = tab.terminal_id(pane_id)?;
                        if candidate_terminal_id != terminal_id {
                            return None;
                        }
                        let terminal = self.state.terminals.get(candidate_terminal_id)?;
                        let plan = terminal.pending_agent_resume_plan.clone();
                        if plan.is_none() && !terminal.respawn_shell_on_exit {
                            return None;
                        }
                        Some((
                            pane_id,
                            terminal.cwd.clone(),
                            terminal.execution_target.clone(),
                            plan,
                        ))
                    })
                })
            })
        else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            execution_target,
            plan,
            rows,
            cols,
            Instant::now(),
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_retry_at = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        execution_target: crate::execution::ExecutionTarget,
        plan: Option<crate::agent_resume::AgentResumePlan>,
        rows: u16,
        cols: u16,
        now: Instant,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        let resume_command = if let Some(plan) = plan.as_ref() {
            let Some(command) = shell_command_from_argv(&plan.argv) else {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    "failed to start deferred agent resume with empty argv"
                );
                return false;
            };
            Some(command)
        } else {
            None
        };
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()))
        else {
            return false;
        };

        let runtime = match crate::terminal::TerminalRuntime::spawn_on(
            pane_id,
            rows,
            cols,
            cwd,
            &execution_target,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = plan.as_ref().map(|plan| plan.agent.as_str()).unwrap_or("shell"),
                    err = %err,
                    "failed to start pending pane runtime"
                );
                self.reschedule_pending_agent_resume_retry();
                return false;
            }
        };

        if let Some(mut input) = resume_command {
            input.push('\r');
            if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
                let agent = plan
                    .as_ref()
                    .map(|plan| plan.agent.as_str())
                    .unwrap_or("unknown");
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent,
                    err = %err,
                    "failed to send deferred agent resume command to shell"
                );
                runtime.shutdown();
                self.reschedule_pending_agent_resume_retry();
                return false;
            }
        }

        let attempt_pid = runtime.child_pid();
        let mut rejected_attempt_identity = false;
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            if plan.is_some() && !execution_target.is_local() {
                rejected_attempt_identity = !attempt_pid
                    .is_some_and(|pid| terminal.mark_pending_agent_resume_attempt_live(pid, now));
            } else {
                terminal.clear_pending_agent_resume_attempt_live();
            }
            if plan.is_none() || execution_target.is_local() {
                terminal.pending_agent_resume_plan = None;
            }
            if execution_target.is_local() {
                terminal.respawn_shell_on_exit = false;
            }
        }
        if rejected_attempt_identity {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                child_pid = attempt_pid,
                "refusing reused or missing SSH resume attempt identity"
            );
            runtime.shutdown();
            self.reschedule_pending_agent_resume_retry();
            return false;
        }
        self.terminal_runtimes.insert(terminal_id, runtime);
        true
    }
    fn reschedule_pending_agent_resume_retry(&mut self) {
        self.pending_agent_resume_retry_at =
            Some(Instant::now() + super::PENDING_AGENT_RESUME_THEME_WAIT);
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
    pane_borders: bool,
    pane_gaps: bool,
    pane_outer_borders: bool,
) -> Vec<crate::layout::PaneInfo> {
    crate::ui::apply_pane_chrome(
        tab.layout.panes(terminal_area),
        pane_borders,
        pane_gaps,
        pane_outer_borders,
    )
    .into_iter()
    .map(|mut info| {
        let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
        info.inner_rect = stable_terminal_inner_rect(pane_inner);
        info
    })
    .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[cfg(unix)]
    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(unix)]
    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }

    #[cfg(unix)]
    #[test]
    fn pending_agent_resume_candidate_preserves_execution_target() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let execution_target = crate::execution::ExecutionTarget::ssh("primary").unwrap();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.execution_target = execution_target.clone();
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].execution_target, execution_target);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_agent_resume_confirmation_deadlines_are_per_terminal() {
        let mut app = test_app();
        let first_workspace = crate::workspace::Workspace::test_new("remote-resume-1");
        let first_pane = first_workspace.tabs[0].root_pane;
        let first_terminal = first_workspace.terminal_id(first_pane).cloned().unwrap();
        let second_workspace = crate::workspace::Workspace::test_new("remote-resume-2");
        let second_pane = second_workspace.tabs[0].root_pane;
        let second_terminal = second_workspace.terminal_id(second_pane).cloned().unwrap();
        app.state.workspaces = vec![first_workspace, second_workspace];
        app.state.ensure_test_terminals();

        let now = Instant::now();
        for (terminal_id, pid, started_at) in [
            (&first_terminal, 101, now),
            (
                &second_terminal,
                202,
                now + std::time::Duration::from_secs(10),
            ),
        ] {
            let terminal = app.state.terminals.get_mut(terminal_id).unwrap();
            terminal.execution_target = crate::execution::ExecutionTarget::ssh("dev1").unwrap();
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("herdr:codex\0codex\0Id\0{pid}"),
            });
            assert!(terminal.mark_pending_agent_resume_attempt_live(pid, started_at));
            let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
            runtime.test_set_child_pid(pid);
            app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        }

        let first_deadline = app.state.terminals[&first_terminal]
            .pending_agent_resume_confirmation_deadline()
            .unwrap();
        let second_deadline = app.state.terminals[&second_terminal]
            .pending_agent_resume_confirmation_deadline()
            .unwrap();
        assert!(first_deadline < second_deadline);
        app.pending_agent_resume_retry_at =
            Some(second_deadline + std::time::Duration::from_secs(1));
        assert_eq!(
            app.next_pending_agent_resume_deadline(),
            Some(first_deadline)
        );

        assert!(app.start_pending_agent_resumes(first_deadline, false));
        assert!(app.terminal_runtimes.get(&first_terminal).is_none());
        assert!(app.terminal_runtimes.get(&second_terminal).is_some());
        let first = &app.state.terminals[&first_terminal];
        assert!(first.pending_agent_resume_plan.is_some());
        assert!(!first.pending_agent_resume_attempt_matches_peer(Some(101)));
        assert_eq!(first.pending_agent_resume_retired_pids(), &[101]);
        assert!(app.state.terminals[&second_terminal]
            .pending_agent_resume_attempt_matches_peer(Some(202)));

        app.terminal_runtimes
            .remove(&second_terminal)
            .unwrap()
            .shutdown();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: marker_resume_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_scheduler_launches_shell_only_runtime() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored-shell");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .respawn_shell_on_exit = true;

        assert!(app.start_pending_agent_resumes(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = &app.state.terminals[&terminal_id];
        assert!(!terminal.respawn_shell_on_exit);
        assert!(terminal.pending_agent_resume_plan.is_none());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        let now = Instant::now();
        app.sync_pending_agent_resume_retry_at(now);
        assert!(!app.start_pending_agent_resumes(now, false));
        assert!(app.start_pending_agent_resumes(now, true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace.terminal_id(active_pane).cloned().unwrap();
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("herdr:codex\0codex\0Id\0{terminal_id}"),
            });
        }
        app.pending_agent_resume_retry_at =
            Some(Instant::now() - std::time::Duration::from_millis(1));

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_retry_at.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(app
            .state
            .workspaces
            .first()
            .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
            .is_some());
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0inactive-tab-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0zoom-hidden-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .unwrap();
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.active = Some(1);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        let now = Instant::now();
        app.sync_pending_agent_resume_retry_at(now);
        assert!(app.pending_agent_resume_retry_at.is_some());
        assert!(app.start_pending_agent_resumes(now, false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }
}
