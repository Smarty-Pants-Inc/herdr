use super::*;

use crate::server::media::{
    error_code, error_response, error_response_with_close_code, success_response, MediaAction,
};

pub(super) fn is_media_method(method: &api::schema::Method) -> bool {
    matches!(
        method,
        api::schema::Method::PaneMediaOpen(_)
            | api::schema::Method::MediaAnswer(_)
            | api::schema::Method::MediaMute(_)
            | api::schema::Method::MediaControl(_)
            | api::schema::Method::MediaState(_)
            | api::schema::Method::MediaClose(_)
    )
}

impl HeadlessServer {
    pub(super) fn perform_media_actions(&mut self, actions: Vec<MediaAction>) {
        // Terminal follow-ups run before remaining fanout actions. In particular,
        // a failed source can legitimately retire its pending successor.
        let mut pending = std::collections::VecDeque::from(actions);
        while let Some(action) = pending.pop_front() {
            match action {
                MediaAction::Publish { event } => self.app.event_hub.push_media(event),
                MediaAction::Send { client_id, control } => {
                    // Keep the actual dispatch target, not an API alias or another
                    // member of the fanout, across both encoding steps.
                    let target = match &control {
                        crate::protocol::media::MediaControl::Control(control) => {
                            let live = self.media.state(&control.session_id).is_some_and(|view| {
                                !matches!(
                                    view.state,
                                    api::schema::MediaSessionState::Closed
                                        | api::schema::MediaSessionState::Failed
                                )
                            });
                            if !live {
                                continue;
                            }
                            Some(control.session_id.clone())
                        }
                        _ => None,
                    };
                    let delivered = match control.server_message() {
                        Ok(message) => self.send_media_to_client(client_id, message),
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to encode media control");
                            false
                        }
                    };
                    if !delivered {
                        if let Some(session_id) = target {
                            let follow_up = self.media.control_delivery_failed(
                                client_id,
                                &session_id,
                                Instant::now(),
                            );
                            for action in follow_up.into_iter().rev() {
                                pending.push_front(action);
                            }
                        }
                    }
                    // A successful local write is not a device acknowledgement.
                }
                MediaAction::Respond {
                    respond_to,
                    response,
                    session_id,
                } => {
                    if respond_to.send(response).is_err() {
                        if let Some(session_id) = session_id {
                            let follow_up = self
                                .media
                                .response_delivery_failed(&session_id, Instant::now());
                            for action in follow_up.into_iter().rev() {
                                pending.push_front(action);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A media delivery failure is target-local. The generic targeted sender
    /// removes a client on failure, recursively executing its media closes before
    /// removing the writer. Do not enter that path (or pretend a missing writer
    /// is success); authenticated transport teardown owns client removal.
    fn send_media_to_client(&self, client_id: u64, message: ServerMessage) -> bool {
        let Some(writer) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.writer.as_ref())
        else {
            return false;
        };
        let framed = match Self::frame_server_message(&message) {
            Ok(framed) => framed,
            Err(err) => {
                warn!(client_id, err = %err, "failed to serialize media control for client");
                return false;
            }
        };
        writer.control.send(framed).is_ok()
    }

    pub(super) fn drain_media_interest_transitions(&mut self) {
        let transitions = self
            .app
            .event_hub
            .take_media_interest_transitions(|canonical| {
                self.app
                    .parse_current_public_pane_id(canonical)
                    .map(|(_, pane)| pane)
            });
        let now = Instant::now();
        for (pane, enabled) in transitions {
            let actions = self.media.set_levels(pane, enabled, now);
            self.perform_media_actions(actions);
        }
    }

    pub(super) fn consume_api_input_receipts(&mut self) {
        for pane in self.app.accepted_api_inputs.drain(..) {
            self.media.invalidate_input_attribution(pane);
        }
    }

    /// Direct terminal clients have no endpoint hello identity. Invalidate only
    /// attribution in the current pane hosting this terminal, leaving media routing intact.
    pub(super) fn invalidate_terminal_input_attribution(&mut self, terminal_id: &str) {
        // ponytail: use current attachments, not a second index that can go stale on
        // rebinding. This direct-terminal path skips empty history and stops at its
        // unique match; unmatched/late attachments still require a linear lookup.
        if !self.media.has_input_owners() {
            return;
        }
        for workspace in &self.app.state.workspaces {
            for tab in &workspace.tabs {
                for (&pane_id, pane) in &tab.panes {
                    if pane.attached_terminal_id.as_str() == terminal_id {
                        self.media.invalidate_input_attribution(pane_id);
                        // AppState requires unique terminal attachments. Resolve from
                        // current topology on every call so moves/rebindings stay correct.
                        return;
                    }
                }
            }
        }
    }

    pub(super) fn expire_media_sessions(&mut self, now: Instant) {
        // Timer maintenance, never pane-scaled work per render/output event. Build
        // one live-id set so retaining N owners does not perform N topology walks.
        if self.media.owner_cleanup_due(now) {
            let live = self
                .app
                .state
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .flat_map(|tab| tab.panes.keys().copied())
                .collect::<HashSet<_>>();
            self.media
                .retain_live_panes(now, |pane| live.contains(&pane));
        }
        let actions = self
            .media
            .expire(now, |pane| self.app.find_pane(pane).is_some());
        self.perform_media_actions(actions);
    }

    pub(super) fn handle_last_input_api_request(&mut self, msg: api::ApiRequestMessage) {
        let api::schema::Method::PaneLastInput(target) = msg.request.method else {
            return;
        };
        let response = match self.app.parse_pane_id(&target.pane) {
            Some((_, pane)) => success_response(
                msg.request.id,
                api::schema::ResponseResult::PaneLastInput {
                    last_input: self.media.last_input(pane),
                },
            ),
            None => error_response(msg.request.id, "pane_not_found", "pane not found"),
        };
        let _ = msg.respond_to.send(response);
    }

    /// Opens wait for an offer and controls wait for the matching applied ack.
    /// Answer remains dispatch-only: it must never wait for Connected.
    pub(super) fn handle_media_api_request(&mut self, msg: api::ApiRequestMessage) {
        use api::schema::{Method, ResponseResult};

        let api::ApiRequestMessage {
            request,
            respond_to,
            response_write_complete,
            ..
        } = msg;
        let id = request.id;
        let now = Instant::now();
        // Epoch validation is a read-only broker gate, before any command can
        // change desired controls, clear an answer deadline, or reserve an open id.
        let (session_id, open_id, epoch) = match &request.method {
            Method::MediaAnswer(params) => (Some(params.session_id.as_str()), None, params.epoch),
            Method::MediaControl(params) => (Some(params.session_id.as_str()), None, params.epoch),
            Method::MediaClose(params)
                if params.session_id.is_some() != params.open_id.is_some() =>
            {
                (
                    params.session_id.as_deref(),
                    params.open_id.as_deref(),
                    params.epoch,
                )
            }
            _ => (None, None, None),
        };
        let terminal_code =
            session_id.and_then(|session_id| self.media.close_cause(session_id, now));
        if let Err((code, message)) = self.media.check_epoch(session_id, open_id, epoch) {
            let _ = respond_to.send(error_response_with_close_code(
                id,
                code,
                message,
                terminal_code.as_deref(),
            ));
            return;
        }
        let immediate = match request.method {
            Method::PaneMediaOpen(target) => {
                let Some((workspace_index, pane_id)) = self.app.parse_pane_id(&target.pane_id)
                else {
                    let _ = respond_to.send(error_response(id, "pane_not_found", "pane not found"));
                    return;
                };
                let Some(canonical) = self.app.public_pane_id(workspace_index, pane_id) else {
                    let _ = respond_to.send(error_response(id, "pane_not_found", "pane not found"));
                    return;
                };
                self.media.set_pane_event_id(pane_id, canonical);
                let viewers = self
                    .clients
                    .keys()
                    .copied()
                    .filter(|&client_id| {
                        self.shell_client_views_pane(client_id, workspace_index, pane_id)
                    })
                    .collect::<HashSet<_>>();
                let actions = self.media.open(
                    id,
                    respond_to,
                    pane_id,
                    target.open_id,
                    response_write_complete,
                    |client_id| viewers.contains(&client_id),
                    now,
                );
                self.perform_media_actions(actions);
                return;
            }
            Method::MediaAnswer(params) => {
                if params.sdp.is_empty()
                    || params.sdp.len() > crate::protocol::media::MAX_MEDIA_SDP_BYTES
                {
                    Err((
                        "invalid_params",
                        "sdp must be a non-empty SDP answer of at most 64 KiB".to_owned(),
                    ))
                } else {
                    self.media.answer(&params.session_id, params.sdp, now)
                }
            }
            Method::MediaControl(params) => {
                // No response-delivery receipt: losing this caller cannot abandon
                // an already established media session. The broker owns the reply.
                let actions = self.media.control(
                    id,
                    respond_to,
                    &params.session_id,
                    params.input_muted,
                    params.output_suppressed,
                    now,
                );
                self.perform_media_actions(actions);
                return;
            }
            Method::MediaMute(params) => self.media.mute(&params.session_id, params.muted),
            Method::MediaState(params) => {
                let response = match self.media.state(&params.session_id) {
                    Some(view) => success_response(id, view.into_result()),
                    None => error_response(
                        id,
                        error_code::SESSION_NOT_FOUND,
                        "no media session with this id",
                    ),
                };
                let _ = respond_to.send(response);
                return;
            }
            Method::MediaClose(params) => match (params.session_id, params.open_id) {
                (Some(session_id), None) => Ok(self.media.close(&session_id, now)),
                (None, Some(open_id)) => self.media.close_by_open_id(&open_id, now),
                (Some(_), Some(_)) | (None, None) => Err((
                    "invalid_params",
                    "media.close requires exactly one of session_id or open_id".to_owned(),
                )),
            },
            _ => Err(("invalid_params", "not a media method".to_owned())),
        };
        let response = match immediate {
            Ok(actions) => {
                self.perform_media_actions(actions);
                success_response(id, ResponseResult::Ok {})
            }
            Err((code, message)) => {
                let close_code =
                    matches!(code, error_code::SESSION_NOT_FOUND | error_code::REFUSED)
                        .then_some(terminal_code.as_deref())
                        .flatten();
                error_response_with_close_code(id, code, message, close_code)
            }
        };
        let _ = respond_to.send(response);
    }
}
