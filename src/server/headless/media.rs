use super::*;

use crate::server::media::{error_code, error_response, success_response, MediaAction};

pub(super) fn is_media_method(method: &api::schema::Method) -> bool {
    matches!(
        method,
        api::schema::Method::PaneMediaOpen(_)
            | api::schema::Method::MediaAnswer(_)
            | api::schema::Method::MediaMute(_)
            | api::schema::Method::MediaState(_)
            | api::schema::Method::MediaClose(_)
    )
}

impl HeadlessServer {
    pub(super) fn perform_media_actions(&mut self, actions: Vec<MediaAction>) {
        for action in actions {
            match action {
                MediaAction::Send { client_id, control } => match control.server_message() {
                    Ok(message) => {
                        self.send_to_client(client_id, message);
                    }
                    Err(err) => warn!(client_id, err = %err, "failed to encode media control"),
                },
                MediaAction::Respond {
                    respond_to,
                    response,
                } => {
                    let _ = respond_to.send(response);
                }
            }
        }
    }

    pub(super) fn expire_media_sessions(&mut self, now: Instant) {
        let actions = self
            .media
            .expire(now, |pane| self.app.find_pane(pane).is_some());
        self.perform_media_actions(actions);
    }

    /// `pane.media_open` answers later, when the bound client sends its offer; the other media
    /// methods answer at once.
    pub(super) fn handle_media_api_request(&mut self, msg: api::ApiRequestMessage) {
        use api::schema::{Method, ResponseResult};

        let api::ApiRequestMessage {
            request,
            respond_to,
            ..
        } = msg;
        let id = request.id;
        let now = Instant::now();
        let immediate = match request.method {
            Method::PaneMediaOpen(target) => {
                let Some((workspace_index, pane_id)) = self.app.parse_pane_id(&target.pane_id)
                else {
                    let _ = respond_to.send(error_response(id, "pane_not_found", "pane not found"));
                    return;
                };
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
                    self.media.answer(&params.session_id, params.sdp)
                }
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
            Method::MediaClose(params) => Ok(self.media.close(&params.session_id, now)),
            _ => Err(("invalid_params", "not a media method".to_owned())),
        };
        let response = match immediate {
            Ok(actions) => {
                self.perform_media_actions(actions);
                success_response(id, ResponseResult::Ok {})
            }
            Err((code, message)) => error_response(id, code, message),
        };
        let _ = respond_to.send(response);
    }
}
