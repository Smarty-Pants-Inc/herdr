use super::{api_helpers::pane_agent_status, App};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTarget {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: crate::layout::PaneId,
    pub terminal_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTargetCandidate {
    pub terminal_id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalTargetError {
    NotFound {
        target: String,
    },
    Ambiguous {
        target: String,
        candidates: Vec<TerminalTargetCandidate>,
    },
}

/// Failed evidence is not proof that a caller is ordinary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputOrigin {
    Ordinary,
    Agent(TerminalTarget),
    /// A live, server-minted plugin action grant: user-initiated input.
    PluginAction(crate::plugin_action_origin::PluginActionGrant),
    Unknown,
}

impl App {
    /// Compare only opaque identities, never pane positions or agent names. The caller
    /// holds server ownership through the effect and uses the checked terminal directly.
    pub(super) fn check_expected_terminal(
        &self,
        expected: Option<&str>,
        actual: Option<&crate::terminal::TerminalId>,
    ) -> Result<(), crate::api::schema::ErrorBody> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let actual = actual.filter(|id| self.state.terminals.contains_key(*id));
        if actual.is_some_and(|id| id.as_str() == expected) {
            return Ok(());
        }
        Err(crate::api::schema::ErrorBody {
            code: "terminal_identity_mismatch".into(),
            message: format!(
                "expected terminal {expected:?}, but the target pane owns {}",
                actual.map_or("no terminal", |id| id.as_str()),
            ),
        })
    }

    pub(crate) fn resolve_terminal_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        let terminal_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| candidate.terminal_id == target)
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, terminal_matches)? {
            return Ok(resolved);
        }

        if let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(target) {
            if let Some(resolved) = self.terminal_target_for_pane(ws_idx, pane_id) {
                return Ok(resolved);
            }
        }

        let agent_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| {
                self.state
                    .terminals
                    .get(candidate.terminal_id.as_str())
                    .is_some_and(|terminal| {
                        terminal.agent_name.as_deref() == Some(target)
                            || terminal.effective_agent_label() == Some(target)
                    })
            })
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, agent_matches)? {
            return Ok(resolved);
        }

        Err(TerminalTargetError::NotFound {
            target: target.to_string(),
        })
    }

    pub(crate) fn resolve_agent_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        if let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(target) {
            if let Some(resolved) = self
                .terminal_target_for_pane(ws_idx, pane_id)
                .filter(|resolved| self.target_is_agent(resolved))
            {
                return Ok(resolved);
            }
        }

        let name_matches: Vec<_> = self
            .terminal_targets()
            .into_iter()
            .filter(|candidate| {
                self.state
                    .terminals
                    .get(candidate.terminal_id.as_str())
                    .is_some_and(|terminal| terminal.agent_name.as_deref() == Some(target))
            })
            .collect();
        if let Some(resolved) = self.single_terminal_match(target, name_matches)? {
            return Ok(resolved);
        }

        Err(TerminalTargetError::NotFound {
            target: target.to_string(),
        })
    }

    fn target_is_agent(&self, target: &TerminalTarget) -> bool {
        self.state
            .terminals
            .get(target.terminal_id.as_str())
            .is_some_and(|terminal| terminal.is_agent_terminal())
    }

    fn single_terminal_match(
        &self,
        target: &str,
        matches: Vec<TerminalTarget>,
    ) -> Result<Option<TerminalTarget>, TerminalTargetError> {
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(TerminalTargetError::Ambiguous {
                target: target.to_string(),
                candidates: matches
                    .into_iter()
                    .filter_map(|candidate| {
                        self.terminal_target_candidate(candidate.ws_idx, candidate.pane_id)
                    })
                    .collect(),
            }),
        }
    }

    pub(crate) fn terminal_targets(&self) -> Vec<TerminalTarget> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().enumerate().flat_map(move |(tab_idx, tab)| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| {
                            tab.terminal_id(pane_id).map(|terminal_id| TerminalTarget {
                                ws_idx,
                                tab_idx,
                                pane_id,
                                terminal_id: terminal_id.to_string(),
                            })
                        })
                })
            })
            .collect()
    }

    pub(crate) fn terminal_target_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<TerminalTarget> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let terminal_id = ws.terminal_id(pane_id)?.to_string();
        Some(TerminalTarget {
            ws_idx,
            tab_idx,
            pane_id,
            terminal_id,
        })
    }

    /// Test convenience lookup for a positively identified agent.
    #[cfg(test)]
    pub(crate) fn agent_terminal_target_for_peer_identity(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
    ) -> Option<TerminalTarget> {
        match self.input_origin_for_peer_identity(peer_identity) {
            InputOrigin::Agent(target) => Some(target),
            InputOrigin::Ordinary | InputOrigin::PluginAction(_) | InputOrigin::Unknown => None,
        }
    }

    pub(crate) fn input_origin_for_peer_identity(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
    ) -> InputOrigin {
        match self.checked_pane_target_for_peer_identity(peer_identity) {
            Ok(Some(target)) if self.target_is_agent(&target) => InputOrigin::Agent(target),
            Ok(_) => InputOrigin::Ordinary,
            Err(()) => InputOrigin::Unknown,
        }
    }

    /// Captured launch markers only remove the ordinary ancestry exemption. They
    /// never select a pane, whose public ID may have been reused or renumbered.
    /// A marked caller must still have a live, validated relationship to a pane.
    pub(crate) fn input_origin_for_context(
        &self,
        context: crate::api::ApiRequestContext,
    ) -> InputOrigin {
        // A presented grant decides alone: valid is a user-initiated plugin
        // action; any invalid claim is unknown and never falls through to the
        // peer's ordinary or agent attribution.
        match self.plugin_action_grants.resolve(context.plugin_action) {
            Ok(Some(grant)) => return InputOrigin::PluginAction(grant),
            Ok(None) => {}
            Err(()) => return InputOrigin::Unknown,
        }
        let Some(peer) = context.local_peer_identity else {
            return InputOrigin::Unknown;
        };
        match context.local_peer_pane_origin {
            crate::platform::PeerPaneOrigin::Unknown => {
                #[cfg(target_os = "macos")]
                {
                    self.input_origin_for_unknown_pane_origin(peer)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    InputOrigin::Unknown
                }
            }
            crate::platform::PeerPaneOrigin::Absent => self.input_origin_for_peer_identity(peer),
            crate::platform::PeerPaneOrigin::HasPane => {
                match self.checked_pane_target_for_peer_identity_with_outside_proof(peer, false) {
                    Ok(Some(target)) if self.target_is_agent(&target) => InputOrigin::Agent(target),
                    Ok(Some(_)) => InputOrigin::Ordinary,
                    Ok(None) | Err(()) => InputOrigin::Unknown,
                }
            }
        }
    }

    /// The pane a public API caller is invoking from, derived only from its
    /// accept-time pinned peer identity and live pane ancestry, under the same
    /// marker policy as the input guard. Request fields, inherited
    /// `HERDR_PANE_ID` text and request ids are never authority. Unknown,
    /// stale, outside or broken peers have no invoker (fail closed), and this
    /// never falls back to UI focus.
    pub(crate) fn trusted_invoking_pane(
        &self,
        context: crate::api::ApiRequestContext,
    ) -> Option<TerminalTarget> {
        let peer = context.local_peer_identity?;
        let allow_outside_proof = match context.local_peer_pane_origin {
            crate::platform::PeerPaneOrigin::Unknown => {
                // Same as the input guard: only Darwin's positive live
                // known-agent recovery attributes an unobservable peer.
                #[cfg(target_os = "macos")]
                if let InputOrigin::Agent(target) = self.input_origin_for_unknown_pane_origin(peer)
                {
                    return Some(target);
                }
                return None;
            }
            crate::platform::PeerPaneOrigin::Absent => true,
            crate::platform::PeerPaneOrigin::HasPane => false,
        };
        self.checked_pane_target_for_peer_identity_with_outside_proof(peer, allow_outside_proof)
            .ok()
            .flatten()
    }

    /// Darwin recovery for protected executables whose initial environment is
    /// unobservable: a live, checked pane link may still positively attribute a
    /// known agent. Never infer ordinary origin from this path.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn input_origin_for_unknown_pane_origin(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
    ) -> InputOrigin {
        match self.checked_pane_target_for_peer_identity_with_outside_proof(peer_identity, false) {
            Ok(Some(target)) if self.target_is_agent(&target) => InputOrigin::Agent(target),
            Ok(_) | Err(()) => InputOrigin::Unknown,
        }
    }

    /// Maps a locally attributed process to the one pane whose session it runs in,
    /// agent or not. Used when a process reports under a pane ID it inherited
    /// before its pane was renumbered or moved (smarty-dev#509).
    ///
    /// A process that left its pane's session (a tool runner that calls `setsid`, as Pi's shell
    /// tool does) still descends from the pane's shell, so the lookup walks the peer's
    /// ancestors until one runs in a pane session (smarty-dev#931).
    pub(crate) fn pane_target_for_peer_identity(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
    ) -> Option<TerminalTarget> {
        self.checked_pane_target_for_peer_identity_with_outside_proof(peer_identity, false)
            .ok()
            .flatten()
    }

    fn checked_pane_target_for_peer_identity(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
    ) -> Result<Option<TerminalTarget>, ()> {
        self.checked_pane_target_for_peer_identity_with_outside_proof(peer_identity, true)
    }

    fn checked_pane_target_for_peer_identity_with_outside_proof(
        &self,
        peer_identity: crate::platform::ProcessIdentity,
        allow_outside_proof: bool,
    ) -> Result<Option<TerminalTarget>, ()> {
        // Darwin's ordinary server descendants must not need access to launchd.
        // Pin this server instance, but only stop after the complete peer-to-server
        // walk has checked every live pane at every step (including the server).
        #[cfg(target_os = "macos")]
        let server_boundary =
            Some(crate::platform::process_identity(std::process::id()).ok_or(())?);
        if allow_outside_proof {
            match crate::platform::process_identity_server_ancestry(peer_identity) {
                crate::platform::ServerAncestry::Outside => return Ok(None),
                crate::platform::ServerAncestry::Unknown => return Err(()),
                crate::platform::ServerAncestry::NotApplicable
                | crate::platform::ServerAncestry::ReachedServer => {}
            }
        }
        let mut missing_root = false;
        let panes: Vec<(TerminalTarget, crate::platform::ProcessIdentity)> = self
            .terminal_targets()
            .into_iter()
            .filter_map(|target| {
                let child_identity = self
                    .state
                    .runtime_for_pane_in_workspace(
                        &self.terminal_runtimes,
                        target.ws_idx,
                        target.pane_id,
                    )
                    .and_then(crate::terminal::TerminalRuntime::child_process_identity);
                let Some(child_identity) = child_identity else {
                    missing_root = true;
                    return None;
                };
                Some((target, child_identity))
            })
            .collect();
        let find = |identity| {
            let mut matched = None;
            for (target, root) in &panes {
                let belongs =
                    crate::platform::process_identity_in_pane_session(*root, identity).ok_or(())?;
                if belongs {
                    if matched.is_some() {
                        return Err(());
                    }
                    matched = Some((target.clone(), *root));
                }
            }
            match matched {
                Some((target, root)) => {
                    if crate::platform::process_identity(root.pid) != Some(root) {
                        return Err(());
                    }
                    Ok(Some(target))
                }
                None => Ok(None),
            }
        };
        let target = if crate::platform::checked_membership_covers_ancestry() {
            // A checked ancestry membership result already covers detached descendants
            // and a negative result proves a complete walk to the platform's root.
            // Keep endpoint validation even when there are no observable pane roots.
            if peer_identity.pid == 0
                || crate::platform::process_identity(peer_identity.pid) != Some(peer_identity)
            {
                return Err(());
            }
            let target = find(peer_identity)?;
            if crate::platform::process_identity(peer_identity.pid) != Some(peer_identity) {
                return Err(());
            }
            target
        } else {
            #[cfg(target_os = "macos")]
            let target = find_in_ancestors_until_boundary(
                peer_identity,
                server_boundary,
                crate::platform::process_identity,
                crate::platform::parent_process_identity,
                find,
            )?;
            #[cfg(not(target_os = "macos"))]
            let target = find_in_ancestors(
                peer_identity,
                crate::platform::process_identity,
                crate::platform::parent_process_identity,
                find,
            )?;
            target
        };
        if target.is_none() && missing_root {
            return Err(());
        }
        Ok(target)
    }

    fn terminal_target_candidate(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<TerminalTargetCandidate> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        Some(TerminalTargetCandidate {
            terminal_id: terminal.id.to_string(),
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            cwd: ws.tabs[tab_idx]
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            agent_status: pane_agent_status(terminal.state, pane.seen),
        })
    }
}

/// The deepest ancestor walk: a pane's shell sits a few levels above any tool it runs.
const MAX_ANCESTOR_DEPTH: usize = 32;

/// Nearest-first, bounded ancestry with pinned origin/current validation across
/// successful hits and parent transitions. Unknown or replaced instances stop
/// attribution, rather than restarting from a replacement PID.
#[cfg(any(not(target_os = "macos"), test))]
fn find_in_ancestors<T>(
    peer: crate::platform::ProcessIdentity,
    identity_of: impl Fn(u32) -> Option<crate::platform::ProcessIdentity>,
    parent_of: impl Fn(crate::platform::ProcessIdentity) -> Option<crate::platform::ProcessIdentity>,
    found: impl FnMut(crate::platform::ProcessIdentity) -> Result<Option<T>, ()>,
) -> Result<Option<T>, ()> {
    find_in_ancestors_until_boundary(peer, None, identity_of, parent_of, found)
}

/// An optional pinned server is a negative boundary only after pane membership
/// checks. Failure to reach it is not a negative proof. The unbounded wrapper
/// retains its original PID-1 policy on Linux and other POSIX platforms.
fn find_in_ancestors_until_boundary<T>(
    peer: crate::platform::ProcessIdentity,
    boundary: Option<crate::platform::ProcessIdentity>,
    identity_of: impl Fn(u32) -> Option<crate::platform::ProcessIdentity>,
    parent_of: impl Fn(crate::platform::ProcessIdentity) -> Option<crate::platform::ProcessIdentity>,
    mut found: impl FnMut(crate::platform::ProcessIdentity) -> Result<Option<T>, ()>,
) -> Result<Option<T>, ()> {
    if boundary.is_some_and(|server| server.pid == 0 || identity_of(server.pid) != Some(server)) {
        return Err(());
    }
    let mut current = peer;
    for _ in 0..=MAX_ANCESTOR_DEPTH {
        if current.pid == 0
            || identity_of(peer.pid) != Some(peer)
            || identity_of(current.pid) != Some(current)
        {
            return Err(());
        }
        let hit = found(current)?;
        if identity_of(peer.pid) != Some(peer) || identity_of(current.pid) != Some(current) {
            return Err(());
        }
        if let Some(hit) = hit {
            return Ok(Some(hit));
        }
        if boundary == Some(current) {
            // Recheck all three pinned endpoints before terminal negative proof.
            // `found` has already checked every pane root at this step.
            if identity_of(peer.pid) != Some(peer)
                || identity_of(current.pid) != Some(current)
                || boundary.is_some_and(|server| identity_of(server.pid) != Some(server))
            {
                return Err(());
            }
            return Ok(None);
        }
        // Without a server boundary, preserve the original process-tree-root proof.
        if current.pid == 1 {
            return if boundary.is_none() {
                Ok(None)
            } else {
                Err(())
            };
        }
        let parent = parent_of(current).ok_or(())?;
        if identity_of(peer.pid) != Some(peer)
            || identity_of(current.pid) != Some(current)
            || identity_of(parent.pid) != Some(parent)
            || parent.start_time > current.start_time
        {
            return Err(());
        }
        current = parent;
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppPolicy, AppState};
    use std::collections::HashMap;

    fn test_app() -> App {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn attribution_app(root: crate::platform::ProcessIdentity) -> (App, TerminalTarget) {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("attribution")];
        app.state.ensure_test_terminals();
        let target = app.terminal_targets().pop().expect("one pane");
        app.state
            .terminals
            .get_mut(target.terminal_id.as_str())
            .expect("terminal")
            .set_agent_name("imported-sender".into());
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        runtime.test_set_child_pid(root.pid);
        app.state.insert_test_runtime(target.pane_id, runtime);
        (app, target)
    }

    fn live_peer() -> crate::platform::ProcessIdentity {
        crate::platform::process_identity(std::process::id()).expect("live test process")
    }

    fn peer_context(peer: crate::platform::ProcessIdentity) -> crate::api::ApiRequestContext {
        crate::api::ApiRequestContext {
            local_peer_identity: Some(peer),
            local_peer_pane_origin: crate::platform::PeerPaneOrigin::Absent,
            ..Default::default()
        }
    }

    // These scoped OS observations model imported ancestry, not a real server
    // replacement. Runtime root and caller instance validation still run live.
    #[tokio::test]
    async fn optional_attribution_maps_imported_root_outside_server_ancestry() {
        let peer = live_peer();
        let (app, target) = attribution_app(peer);
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                assert_eq!(app.pane_target_for_peer_identity(peer), Some(target));
            });
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn optional_attribution_maps_imported_caller_outside_server_ancestry() {
        let peer = live_peer();
        let root = crate::platform::parent_process_identity(peer).expect("live parent");
        assert_ne!(root, peer);
        let (app, target) = attribution_app(root);
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                assert_eq!(app.pane_target_for_peer_identity(peer), Some(target));
            });
        });
    }

    #[tokio::test]
    async fn optional_attribution_rebinds_stale_report_outside_server_ancestry() {
        use crate::api::schema::{Method, PaneReleaseAgentParams, Request};
        let peer = live_peer();
        let (app, target) = attribution_app(peer);
        let current = app.public_pane_id(target.ws_idx, target.pane_id).unwrap();
        let report = |pane_id: &str| Request {
            id: "rebind".into(),
            method: Method::PaneReleaseAgent(PaneReleaseAgentParams {
                pane_id: pane_id.into(),
                source: "custom:pi".into(),
                agent: "pi".into(),
                seq: None,
            }),
        };
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                let mut known = report(&current);
                app.rebind_stale_report_pane(&mut known, peer_context(peer));
                assert_eq!(known.method, report(&current).method);
                let mut stale = report("w42:p1F");
                assert!(app.parse_pane_id("w42:p1F").is_none());
                app.rebind_stale_report_pane(&mut stale, peer_context(peer));
                assert_eq!(stale.method, report(&current).method);
            });
        });
    }

    #[tokio::test]
    async fn optional_attribution_logs_api_caller_outside_server_ancestry() {
        use crate::api::schema::{Method, PaneSendTextParams, Request};
        let peer = live_peer();
        let (mut app, source) = attribution_app(peer);
        let target_pane =
            app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let (runtime, mut target_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(target_pane, runtime);
        let target_id = app.public_pane_id(0, target_pane).unwrap();
        let source_id = app.public_pane_id(0, source.pane_id).unwrap();
        let response = crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                app.handle_api_request_with_context(
                    Request {
                        id: "logged".into(),
                        method: Method::PaneSendText(PaneSendTextParams {
                            pane_id: target_id.clone(),
                            text: "private prompt".into(),
                            allow_cross_pane: false,
                        }),
                    },
                    peer_context(peer),
                )
            })
        });
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(target_rx.try_recv().unwrap(), "private prompt");
        let raw = std::fs::read_to_string(&app.api_input_log).unwrap();
        std::fs::remove_file(&app.api_input_log).unwrap();
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 1);
        let line: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(line["method"], "pane.send_text");
        assert_eq!(line["target_pane"], target_id);
        assert_eq!(line["bytes"], 14);
        assert_eq!(line["caller"]["pid"], peer.pid);
        assert_eq!(line["caller"]["pane"], source_id);
        assert_eq!(line["caller"]["agent"], "imported-sender");
        assert!(!raw.contains("private prompt"));
    }

    #[tokio::test]
    async fn optional_attribution_maps_live_pane_despite_unknown_server_ancestry() {
        let peer = live_peer();
        let (app, target) = attribution_app(peer);
        crate::platform::with_server_ancestry_for_test(peer, None, || {
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                assert_eq!(app.pane_target_for_peer_identity(peer), Some(target));
            });
        });
    }

    #[tokio::test]
    async fn optional_attribution_keeps_permission_server_ancestry_shortcut() {
        let peer = live_peer();
        let (app, _) = attribution_app(peer);
        for (observation, expected) in [
            (Some(true), InputOrigin::Ordinary),
            (None, InputOrigin::Unknown),
        ] {
            crate::platform::with_server_ancestry_for_test(peer, observation, || {
                crate::platform::with_ancestry_membership_for_test(Some(true), || {
                    assert_eq!(app.input_origin_for_peer_identity(peer), expected);
                    assert_eq!(app.input_origin_for_context(peer_context(peer)), expected);
                });
            });
        }
    }

    #[tokio::test]
    async fn optional_attribution_rejects_unproven_or_stale_membership() {
        let peer = live_peer();
        let (app, _) = attribution_app(peer);
        crate::platform::with_server_ancestry_for_test(peer, Some(true), || {
            for observation in [Some(false), None] {
                crate::platform::with_ancestry_membership_for_test(observation, || {
                    assert_eq!(app.pane_target_for_peer_identity(peer), None);
                });
            }
            crate::platform::with_ancestry_membership_for_test(Some(true), || {
                let stale = crate::platform::ProcessIdentity {
                    start_time: peer.start_time.wrapping_add(1),
                    ..peer
                };
                assert_eq!(app.pane_target_for_peer_identity(stale), None);
                app.state
                    .runtime_for_pane_in_workspace(
                        &app.terminal_runtimes,
                        0,
                        app.state.workspaces[0].tabs[0].root_pane,
                    )
                    .unwrap()
                    .test_set_child_pid(0);
                assert_eq!(app.pane_target_for_peer_identity(peer), None);
            });
        });
    }

    /// Fake process table: pid -> parent. Pane shells are pid 100 (pane "a") and 200 (pane "b").
    fn walk(table: &HashMap<u32, u32>, pid: u32) -> Option<&'static str> {
        find_in_ancestors(
            crate::platform::ProcessIdentity { pid, start_time: 1 },
            |pid| Some(crate::platform::ProcessIdentity { pid, start_time: 1 }),
            |identity| {
                table
                    .get(&identity.pid)
                    .copied()
                    .map(|pid| crate::platform::ProcessIdentity { pid, start_time: 1 })
            },
            |identity| {
                Ok(match identity.pid {
                    100 => Some("a"),
                    200 => Some("b"),
                    _ => None,
                })
            },
        )
        .ok()
        .flatten()
    }

    fn boundary_identity(pid: u32) -> crate::platform::ProcessIdentity {
        crate::platform::ProcessIdentity {
            pid,
            start_time: u64::from(pid),
        }
    }

    #[test]
    fn server_boundary_checks_every_pane_before_ordinary_proof() {
        // The server's parent (launchd on Darwin) is inaccessible. Sibling pane
        // roots are live, but neither owns any step of the caller's ancestry.
        let peer = boundary_identity(300);
        let intermediate = boundary_identity(200);
        let server = boundary_identity(100);
        let roots = [boundary_identity(400), boundary_identity(500)];
        let mut checked = Vec::new();
        assert_eq!(
            find_in_ancestors_until_boundary(
                peer,
                Some(server),
                |pid| Some(boundary_identity(pid)),
                |current| match current.pid {
                    300 => Some(intermediate),
                    200 => Some(server),
                    _ => panic!("must not read ancestors above pinned server"),
                },
                |current| {
                    for root in roots {
                        checked.push((current, root));
                    }
                    Ok::<Option<&str>, ()>(None)
                },
            ),
            Ok(None)
        );
        assert_eq!(
            checked,
            [peer, intermediate, server]
                .into_iter()
                .flat_map(|current| roots.map(|root| (current, root)))
                .collect::<Vec<_>>()
        );
        // The original wrapper still needs the next link, not a server exemption.
        assert_eq!(
            find_in_ancestors(
                peer,
                |pid| Some(boundary_identity(pid)),
                |current| match current.pid {
                    300 => Some(intermediate),
                    200 => Some(server),
                    _ => None,
                },
                |_| Ok::<Option<&str>, ()>(None),
            ),
            Err(())
        );
    }

    #[test]
    fn pane_ancestor_precedes_server_boundary_including_boundary_itself() {
        let peer = boundary_identity(300);
        let server = boundary_identity(100);
        for pane_pid in [300, 200, 100] {
            assert_eq!(
                find_in_ancestors_until_boundary(
                    peer,
                    Some(server),
                    |pid| Some(boundary_identity(pid)),
                    |current| Some(boundary_identity(current.pid - 100)),
                    |current| Ok((current.pid == pane_pid).then_some("pane")),
                ),
                Ok(Some("pane"))
            );
        }
    }

    #[test]
    fn server_boundary_requires_live_identity_and_complete_link() {
        let peer = boundary_identity(300);
        let server = boundary_identity(100);
        for invalid in [peer, server] {
            for replacement in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: invalid.start_time + 1,
                    ..invalid
                }),
            ] {
                assert_eq!(
                    find_in_ancestors_until_boundary(
                        peer,
                        Some(server),
                        |pid| if pid == invalid.pid {
                            replacement
                        } else {
                            Some(boundary_identity(pid))
                        },
                        |_| panic!("invalid endpoint must stop"),
                        |_| Ok::<Option<&str>, ()>(None),
                    ),
                    Err(())
                );
            }
        }
        for premature_parent in [None, Some(boundary_identity(1))] {
            assert_eq!(
                find_in_ancestors_until_boundary(
                    peer,
                    Some(server),
                    |pid| Some(boundary_identity(pid)),
                    |_| premature_parent,
                    |_| Ok::<Option<&str>, ()>(None),
                ),
                Err(())
            );
        }
        assert_eq!(
            find_in_ancestors_until_boundary(
                peer,
                Some(crate::platform::ProcessIdentity {
                    pid: 0,
                    start_time: 0
                }),
                |pid| Some(boundary_identity(pid)),
                |_| panic!("zero boundary must stop"),
                |_| Ok::<Option<&str>, ()>(None),
            ),
            Err(())
        );
    }

    #[test]
    fn server_boundary_rechecks_peer_server_and_current_after_pane_checks() {
        use std::cell::Cell;
        let peer = boundary_identity(300);
        let server = boundary_identity(100);
        let intermediate = boundary_identity(200);
        for changed in [peer, server, intermediate] {
            for replacement in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: changed.start_time + 1,
                    ..changed
                }),
            ] {
                let live = Cell::new(Some(changed));
                assert_eq!(
                    find_in_ancestors_until_boundary(
                        peer,
                        Some(server),
                        |pid| if pid == changed.pid {
                            live.get()
                        } else {
                            Some(boundary_identity(pid))
                        },
                        |current| Some(boundary_identity(current.pid - 100)),
                        |current| {
                            if current == intermediate && changed == intermediate
                                || current == server
                            {
                                live.set(replacement);
                            }
                            Ok::<Option<&str>, ()>(None)
                        },
                    ),
                    Err(())
                );
            }
        }
    }

    #[test]
    fn server_boundary_never_masks_invalid_or_missing_pane_membership() {
        let peer = boundary_identity(300);
        let server = boundary_identity(100);
        let root = boundary_identity(400);
        for invalid_at in [peer, server] {
            for root_now in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: root.start_time + 1,
                    ..root
                }),
            ] {
                assert_eq!(
                    find_in_ancestors_until_boundary(
                        peer,
                        Some(server),
                        |pid| Some(boundary_identity(pid)),
                        |_| Some(server),
                        |current| {
                            // Model the production checked membership failure for a
                            // missing/replaced root, even at the terminal boundary.
                            if current == invalid_at && root_now != Some(root) {
                                return Err(());
                            }
                            Ok::<Option<&str>, ()>(None)
                        },
                    ),
                    Err(())
                );
            }
        }
    }

    #[test]
    fn ancestor_walk_maps_grandchild_to_its_pane_shell() {
        // 100 (pane a shell) -> 101 (pi) -> 102 (setsid tool shell) -> 103 (herdr cli)
        let table = HashMap::from([(100, 50), (101, 100), (102, 101), (103, 102), (50, 1)]);
        assert_eq!(walk(&table, 103), Some("a"));
        assert_eq!(walk(&table, 100), Some("a"));
    }

    #[test]
    fn ancestor_walk_ignores_unrelated_process() {
        let table = HashMap::from([(300, 301), (301, 1), (400, 999)]);
        assert_eq!(walk(&table, 300), None);
        // The parent is unknown (exited), so the walk stops.
        assert_eq!(walk(&table, 400), None);
        assert_eq!(walk(&table, 1), None);
        assert_eq!(walk(&table, 0), None);
    }

    #[test]
    fn ancestor_walk_does_not_follow_reused_pid_identity() {
        let parent = crate::platform::ProcessIdentity {
            pid: 100,
            start_time: 2,
        };
        let caller = crate::platform::ProcessIdentity {
            pid: 103,
            start_time: 1,
        };
        assert_eq!(
            find_in_ancestors(
                caller,
                |pid| Some(crate::platform::ProcessIdentity { pid, start_time: 1 }),
                |identity| (identity == caller).then_some(parent),
                |identity| Ok((identity.pid == 100).then_some("reused")),
            ),
            Err(())
        );
    }

    #[test]
    fn ancestor_walk_revalidates_origin_and_ancestor_after_found_hit() {
        use std::cell::Cell;
        let peer = crate::platform::ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let ancestor = crate::platform::ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        for changed in [peer, ancestor] {
            for replacement in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: 30,
                    ..changed
                }),
            ] {
                let current = Cell::new(Some(changed));
                assert_eq!(
                    find_in_ancestors(
                        peer,
                        |pid| if pid == changed.pid {
                            current.get()
                        } else if pid == peer.pid {
                            Some(peer)
                        } else {
                            Some(ancestor)
                        },
                        |identity| (identity == peer).then_some(ancestor),
                        |identity| {
                            if identity == ancestor {
                                current.set(replacement);
                                Ok(Some("wrong hit"))
                            } else {
                                Ok(None)
                            }
                        }
                    ),
                    Err(())
                );
            }
        }
    }

    #[test]
    fn ancestor_walk_revalidates_parent_transition() {
        use std::cell::Cell;
        let peer = crate::platform::ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let parent = crate::platform::ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        for changed in [peer, parent] {
            for replacement in [
                None,
                Some(crate::platform::ProcessIdentity {
                    start_time: 30,
                    ..changed
                }),
            ] {
                let current = Cell::new(Some(changed));
                assert_eq!(
                    find_in_ancestors(
                        peer,
                        |pid| if pid == changed.pid {
                            current.get()
                        } else if pid == peer.pid {
                            Some(peer)
                        } else {
                            Some(parent)
                        },
                        |_| {
                            current.set(replacement);
                            Some(parent)
                        },
                        |identity| Ok((identity == parent).then_some("replacement"))
                    ),
                    Err(())
                );
            }
        }
    }

    #[test]
    fn invalid_membership_observation_stops_before_parent_attribution() {
        let peer = crate::platform::ProcessIdentity {
            pid: 200,
            start_time: 20,
        };
        let parent = crate::platform::ProcessIdentity {
            pid: 100,
            start_time: 10,
        };
        assert_eq!(
            find_in_ancestors(
                peer,
                |pid| if pid == peer.pid {
                    Some(peer)
                } else {
                    Some(parent)
                },
                |_| panic!("invalid membership must not resume at parent"),
                |_| Err::<Option<&str>, ()>(())
            ),
            Err(())
        );
    }

    #[test]
    fn ancestor_walk_stops_on_cycle_and_depth_cap() {
        let cycle = HashMap::from([(10, 11), (11, 12), (12, 10)]);
        assert_eq!(walk(&cycle, 10), None);

        // A chain whose pane shell sits one step past the cap is not reached; at the cap it is.
        let chain = |len: u32| -> HashMap<u32, u32> {
            let mut table: HashMap<u32, u32> = (0..len).map(|i| (1000 + i, 1000 + i + 1)).collect();
            table.insert(1000 + len, 100);
            table
        };
        let depth = MAX_ANCESTOR_DEPTH as u32;
        assert_eq!(walk(&chain(depth - 1), 1000), Some("a"));
        assert_eq!(walk(&chain(depth), 1000), None);
    }

    #[test]
    fn named_targets_follow_terminal_identity_after_pane_and_tab_reordering() {
        let mut app = test_app();
        app.state = AppState::test_with_adversarial_identity_state();
        let targets = app.terminal_targets();
        for (index, target) in targets.iter().enumerate() {
            app.state
                .terminals
                .get_mut(target.terminal_id.as_str())
                .unwrap()
                .set_agent_name(format!("worker-{index}"));
        }
        for (index, target) in targets.iter().enumerate() {
            let name = format!("worker-{index}");
            assert_eq!(app.resolve_terminal_target(&name).unwrap(), *target);
            assert_eq!(app.resolve_agent_target(&name).unwrap(), *target);
            let pane = app.public_pane_id(target.ws_idx, target.pane_id).unwrap();
            assert_eq!(app.resolve_agent_target(&pane).unwrap(), *target);
        }
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn name_lookup_keeps_pane_order_and_ignores_detached_terminals() {
        let mut app = test_app();
        app.state = AppState::test_with_adversarial_identity_state();
        let targets = app.terminal_targets();
        for target in &targets {
            app.state
                .terminals
                .get_mut(target.terminal_id.as_str())
                .unwrap()
                .set_agent_name("shared".into());
        }
        let detached_id = crate::terminal::TerminalId::alloc();
        let mut detached =
            crate::terminal::TerminalState::new(detached_id.clone(), std::env::temp_dir());
        detached.set_agent_name("detached".into());
        app.state.terminals.insert(detached_id, detached);
        assert!(matches!(
            app.resolve_agent_target("detached"),
            Err(TerminalTargetError::NotFound { .. })
        ));
        for result in [
            app.resolve_terminal_target("shared"),
            app.resolve_agent_target("shared"),
        ] {
            let Err(TerminalTargetError::Ambiguous { candidates, .. }) = result else {
                panic!("expected all attached panes to remain ambiguous");
            };
            assert_eq!(
                candidates
                    .iter()
                    .map(|candidate| candidate.terminal_id.as_str())
                    .collect::<Vec<_>>(),
                targets
                    .iter()
                    .map(|target| target.terminal_id.as_str())
                    .collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    #[ignore = "manual terminal target lookup scaling profile"]
    fn terminal_target_lookup_profile() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        for count in [1, 15, 128, 512] {
            let mut app = test_app();
            let mut workspace = crate::workspace::Workspace::test_new("lookup-profile");
            for _ in 1..count {
                workspace.test_split(ratatui::layout::Direction::Horizontal);
            }
            app.state.workspaces = vec![workspace];
            app.state.ensure_test_terminals();
            let id = app.state.workspaces[0]
                .terminal_id(app.state.workspaces[0].tabs[0].root_pane)
                .unwrap()
                .clone();
            app.state
                .terminals
                .get_mut(&id)
                .unwrap()
                .set_agent_name("profile-target".into());
            for (label, agent, target) in [
                ("terminal-name", false, "profile-target"),
                ("agent-name", true, "profile-target"),
                ("missing", false, "missing-target"),
            ] {
                let lookup = || {
                    if agent {
                        app.resolve_agent_target(target)
                    } else {
                        app.resolve_terminal_target(target)
                    }
                };
                for _ in 0..32 {
                    black_box(lookup()).ok();
                }
                let mut samples = Vec::new();
                for _ in 0..7 {
                    let start = Instant::now();
                    let mut iterations = 0;
                    while start.elapsed() < Duration::from_millis(20) {
                        black_box(lookup()).ok();
                        iterations += 1;
                    }
                    samples.push(start.elapsed().as_secs_f64() * 1e6 / f64::from(iterations));
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "terminal-target panes={count} case={label} median_us={:.3}",
                    samples[3]
                );
            }
        }
    }
}
