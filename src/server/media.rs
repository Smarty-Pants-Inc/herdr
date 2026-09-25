//! Server-side routing of client-local media sessions.
//!
//! `pane.media_open` binds a session to the attached client whose input last reached the
//! pane, because that is the machine the user is sitting at. The broker is pure state: the
//! headless server feeds it client, input and API events and performs the returned actions.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, MediaSessionState, ResponseResult, SuccessResponse,
};
use crate::layout::PaneId;
use crate::protocol::media::{
    close_code, MediaClose, MediaControl, MediaMute, MediaOpen, MediaPeerState, MediaSdp,
    MEDIA_INPUT_WINDOW,
};

/// How long `pane.media_open` waits for the client's offer. It covers the one-time consent
/// prompt (30 s on the client) plus ICE gathering.
pub(crate) const MEDIA_OPEN_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a closed session's final state stays readable through `media.state`.
const CLOSED_SESSION_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_CLOSED_SESSIONS: usize = 32;

/// API error codes returned by the media methods.
pub(crate) mod error_code {
    pub(crate) const NO_CLIENT: &str = "media_no_client";
    pub(crate) const UNSUPPORTED_CLIENT: &str = "media_unsupported_client";
    pub(crate) const REFUSED: &str = "media_refused";
    pub(crate) const TIMEOUT: &str = "media_timeout";
    pub(crate) const SESSION_NOT_FOUND: &str = "media_session_not_found";
    pub(crate) const NOT_READY: &str = "media_session_not_ready";
}

/// Side effects the headless server performs for the broker.
#[derive(Debug)]
pub(crate) enum MediaAction {
    Send {
        client_id: u64,
        control: MediaControl,
    },
    Respond {
        respond_to: Sender<String>,
        response: String,
    },
}

#[derive(Debug, Clone)]
struct PaneInput {
    pane: PaneId,
    /// The pane id exactly as the client sent it, so the client can match its own record.
    pane_ref: String,
    at: Instant,
}

#[derive(Debug, Default)]
struct MediaClient {
    capable: bool,
    last_input: Option<PaneInput>,
}

#[derive(Debug)]
struct PendingOpen {
    request_id: String,
    respond_to: Sender<String>,
    deadline: Instant,
}

#[derive(Debug)]
struct MediaSession {
    client_id: u64,
    pane: PaneId,
    state: MediaSessionState,
    muted: bool,
    pending: Option<PendingOpen>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaSessionView {
    pub(crate) session_id: String,
    pub(crate) state: MediaSessionState,
    pub(crate) muted: bool,
    pub(crate) code: Option<String>,
    pub(crate) message: Option<String>,
}

impl MediaSessionView {
    pub(crate) fn into_result(self) -> ResponseResult {
        ResponseResult::MediaSession {
            session_id: self.session_id,
            state: self.state,
            muted: self.muted,
            code: self.code,
            message: self.message,
        }
    }
}

#[derive(Debug)]
struct ClosedSession {
    closed_at: Instant,
    view: MediaSessionView,
}

#[derive(Debug)]
pub(crate) struct MediaBroker {
    clients: HashMap<u64, MediaClient>,
    sessions: HashMap<String, MediaSession>,
    closed: VecDeque<ClosedSession>,
    id_prefix: String,
    next_id: u64,
}

pub(crate) fn success_response(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_error","message":"failed to encode response"}}"#
            .to_owned()
    })
}

pub(crate) fn error_response(id: String, code: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.to_owned(),
            message: message.into(),
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_error","message":"failed to encode response"}}"#
            .to_owned()
    })
}

impl MediaBroker {
    pub(crate) fn new() -> Self {
        // Session ids only need to be unique for this server's lifetime; the boot stamp keeps
        // a restarted server from reusing an id a caller still holds.
        let boot = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        Self {
            clients: HashMap::new(),
            sessions: HashMap::new(),
            closed: VecDeque::new(),
            id_prefix: format!("media_{boot:x}_"),
            next_id: 1,
        }
    }

    pub(crate) fn client_connected(&mut self, client_id: u64, capable: bool) {
        self.clients.insert(
            client_id,
            MediaClient {
                capable,
                last_input: None,
            },
        );
    }

    /// Record input that reached a pane the client views.
    pub(crate) fn note_pane_input(
        &mut self,
        client_id: u64,
        pane: PaneId,
        pane_ref: &str,
        now: Instant,
    ) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.last_input = Some(PaneInput {
                pane,
                pane_ref: pane_ref.to_owned(),
                at: now,
            });
        }
    }

    pub(crate) fn client_removed(&mut self, client_id: u64, now: Instant) -> Vec<MediaAction> {
        self.clients.remove(&client_id);
        let ids = self
            .sessions
            .iter()
            .filter(|(_, session)| session.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut actions = Vec::new();
        for id in ids {
            // The client is gone, so there is nobody to tell; only answer a waiting caller.
            self.finish(
                &id,
                close_code::DISCONNECTED,
                "the client disconnected",
                false,
                now,
                &mut actions,
            );
        }
        actions
    }

    /// Bind a new session to the client whose input last reached `pane`.
    ///
    /// `views` reports whether a client currently shows the pane.
    pub(crate) fn open(
        &mut self,
        request_id: String,
        respond_to: Sender<String>,
        pane: PaneId,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        let latest = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| {
                let input = client.last_input.as_ref()?;
                (input.pane == pane
                    && now.saturating_duration_since(input.at) <= MEDIA_INPUT_WINDOW)
                    .then_some((client_id, client.capable, input.clone()))
            })
            .max_by_key(|(client_id, _, input)| (input.at, *client_id));
        let Some((client_id, capable, input)) = latest else {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::NO_CLIENT,
                    "no attached Herdr client typed into this pane in the last 10 seconds",
                ),
            });
            return actions;
        };
        if !capable {
            // ponytail: never fall through to another client. The user is at the client that
            // typed last; opening a microphone on a different machine would surprise them.
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::UNSUPPORTED_CLIENT,
                    "the Herdr client that typed into this pane has no native media support",
                ),
            });
            return actions;
        }
        if !views(client_id) {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::REFUSED,
                    format!(
                        "{}: the client no longer shows this pane",
                        close_code::NOT_VIEWED
                    ),
                ),
            });
            return actions;
        }

        let replaced = self
            .sessions
            .iter()
            .filter(|(_, session)| session.pane == pane || session.client_id == client_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in replaced {
            self.finish(
                &id,
                close_code::REPLACED,
                "a newer media session replaced this one",
                true,
                now,
                &mut actions,
            );
        }

        let session_id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        self.sessions.insert(
            session_id.clone(),
            MediaSession {
                client_id,
                pane,
                state: MediaSessionState::Opening,
                muted: false,
                pending: Some(PendingOpen {
                    request_id,
                    respond_to,
                    deadline: now + MEDIA_OPEN_TIMEOUT,
                }),
            },
        );
        actions.push(MediaAction::Send {
            client_id,
            control: MediaControl::Open(MediaOpen {
                session_id,
                pane_id: input.pane_ref,
            }),
        });
        actions
    }

    /// Apply one control a client sent.
    pub(crate) fn client_control(
        &mut self,
        client_id: u64,
        control: MediaControl,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        let session_id = control.session_id().to_owned();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return actions;
        };
        if session.client_id != client_id {
            return actions;
        }
        match control {
            MediaControl::Offer(MediaSdp { sdp, .. }) => {
                let Some(pending) = session.pending.take() else {
                    return actions;
                };
                session.state = MediaSessionState::Offered;
                actions.push(MediaAction::Respond {
                    respond_to: pending.respond_to,
                    response: success_response(
                        pending.request_id,
                        ResponseResult::MediaOffer { session_id, sdp },
                    ),
                });
            }
            MediaControl::State(update) => {
                if session.pending.is_some() {
                    return actions;
                }
                session.muted = update.muted;
                match update.state {
                    MediaPeerState::Connecting => session.state = MediaSessionState::Connecting,
                    MediaPeerState::Connected => session.state = MediaSessionState::Connected,
                    MediaPeerState::Failed => session.state = MediaSessionState::Failed,
                    MediaPeerState::Unknown => {}
                }
            }
            MediaControl::Close(MediaClose { code, message, .. }) => {
                let code = code.unwrap_or_else(|| close_code::CLOSED.to_owned());
                let message = message.unwrap_or_else(|| "the client ended the session".to_owned());
                self.finish(&session_id, &code, &message, false, now, &mut actions);
            }
            MediaControl::Open(_) | MediaControl::Answer(_) | MediaControl::Mute(_) => {}
        }
        actions
    }

    pub(crate) fn answer(
        &mut self,
        session_id: &str,
        sdp: String,
    ) -> Result<Vec<MediaAction>, (&'static str, String)> {
        let session = self.live_session(session_id)?;
        if session.pending.is_some() {
            return Err((
                error_code::NOT_READY,
                "the client has not sent its offer yet".to_owned(),
            ));
        }
        if session.state == MediaSessionState::Offered {
            session.state = MediaSessionState::Connecting;
        }
        Ok(vec![MediaAction::Send {
            client_id: session.client_id,
            control: MediaControl::Answer(MediaSdp {
                session_id: session_id.to_owned(),
                sdp,
            }),
        }])
    }

    pub(crate) fn mute(
        &mut self,
        session_id: &str,
        muted: bool,
    ) -> Result<Vec<MediaAction>, (&'static str, String)> {
        let session = self.live_session(session_id)?;
        Ok(vec![MediaAction::Send {
            client_id: session.client_id,
            control: MediaControl::Mute(MediaMute {
                session_id: session_id.to_owned(),
                muted,
            }),
        }])
    }

    pub(crate) fn state(&self, session_id: &str) -> Option<MediaSessionView> {
        if let Some(session) = self.sessions.get(session_id) {
            return Some(MediaSessionView {
                session_id: session_id.to_owned(),
                state: session.state,
                muted: session.muted,
                code: None,
                message: None,
            });
        }
        self.closed
            .iter()
            .find(|closed| closed.view.session_id == session_id)
            .map(|closed| closed.view.clone())
    }

    /// End a session at the API caller's request. Unknown ids are a no-op.
    pub(crate) fn close(&mut self, session_id: &str, now: Instant) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        if self.sessions.contains_key(session_id) {
            self.finish(
                session_id,
                close_code::CLOSED,
                "the caller ended the session",
                true,
                now,
                &mut actions,
            );
        }
        actions
    }

    /// Time out slow offers, end sessions whose pane closed and drop old closed sessions.
    pub(crate) fn expire(
        &mut self,
        now: Instant,
        pane_exists: impl Fn(PaneId) -> bool,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        if self.sessions.is_empty() && self.closed.is_empty() {
            return actions;
        }
        let timed_out = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session
                    .pending
                    .as_ref()
                    .is_some_and(|pending| now >= pending.deadline)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in timed_out {
            self.finish(
                &id,
                close_code::TIMEOUT,
                "the client did not send an offer in time",
                true,
                now,
                &mut actions,
            );
        }
        let orphaned = self
            .sessions
            .iter()
            .filter(|(_, session)| !pane_exists(session.pane))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in orphaned {
            self.finish(
                &id,
                close_code::PANE_CLOSED,
                "the pane closed",
                true,
                now,
                &mut actions,
            );
        }
        while self.closed.front().is_some_and(|closed| {
            now.saturating_duration_since(closed.closed_at) >= CLOSED_SESSION_RETENTION
        }) {
            self.closed.pop_front();
        }
        actions
    }

    #[cfg(test)]
    pub(crate) fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    fn live_session(
        &mut self,
        session_id: &str,
    ) -> Result<&mut MediaSession, (&'static str, String)> {
        self.sessions.get_mut(session_id).ok_or_else(|| {
            (
                error_code::SESSION_NOT_FOUND,
                "no live media session with this id".to_owned(),
            )
        })
    }

    fn finish(
        &mut self,
        session_id: &str,
        code: &str,
        message: &str,
        notify_client: bool,
        now: Instant,
        actions: &mut Vec<MediaAction>,
    ) {
        let Some(session) = self.sessions.remove(session_id) else {
            return;
        };
        if notify_client {
            actions.push(MediaAction::Send {
                client_id: session.client_id,
                control: MediaControl::Close(MediaClose::new(session_id, code, message)),
            });
        }
        if let Some(pending) = session.pending {
            let (error, text) = if code == close_code::TIMEOUT {
                (error_code::TIMEOUT, message.to_owned())
            } else {
                (error_code::REFUSED, format!("{code}: {message}"))
            };
            actions.push(MediaAction::Respond {
                respond_to: pending.respond_to,
                response: error_response(pending.request_id, error, text),
            });
        }
        self.closed.push_back(ClosedSession {
            closed_at: now,
            view: MediaSessionView {
                session_id: session_id.to_owned(),
                state: MediaSessionState::Closed,
                muted: session.muted,
                code: Some(code.to_owned()),
                message: Some(message.to_owned()),
            },
        });
        while self.closed.len() > MAX_CLOSED_SESSIONS {
            self.closed.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::media::MediaStateUpdate;
    use std::sync::mpsc;

    fn pane(raw: u32) -> PaneId {
        PaneId::from_raw(raw)
    }

    fn response(rx: &mpsc::Receiver<String>) -> serde_json::Value {
        serde_json::from_str(&rx.try_recv().expect("a response")).expect("json")
    }

    /// Perform actions the way the headless server does; return controls sent per client.
    fn run(actions: Vec<MediaAction>) -> Vec<(u64, MediaControl)> {
        actions
            .into_iter()
            .filter_map(|action| match action {
                MediaAction::Send { client_id, control } => Some((client_id, control)),
                MediaAction::Respond {
                    respond_to,
                    response,
                } => {
                    let _ = respond_to.send(response);
                    None
                }
            })
            .collect()
    }

    fn open(
        broker: &mut MediaBroker,
        pane_id: PaneId,
        now: Instant,
    ) -> (Vec<(u64, MediaControl)>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open("req".into(), tx, pane_id, |_| true, now));
        (sent, rx)
    }

    fn opened_session(sent: &[(u64, MediaControl)]) -> (u64, String, String) {
        sent.iter()
            .find_map(|(client_id, control)| match control {
                MediaControl::Open(open) => {
                    Some((*client_id, open.session_id.clone(), open.pane_id.clone()))
                }
                _ => None,
            })
            .expect("an open control")
    }

    #[test]
    fn last_input_client_wins_and_gets_the_pane_ref_it_sent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        broker.note_pane_input(2, pane(7), "p_7", now + Duration::from_secs(2));

        let (sent, rx) = open(&mut broker, pane(7), now + Duration::from_secs(3));
        let (client_id, _, pane_ref) = opened_session(&sent);
        assert_eq!(client_id, 2);
        assert_eq!(pane_ref, "p_7");
        assert!(rx.try_recv().is_err(), "the caller waits for the offer");
    }

    #[test]
    fn no_client_input_is_an_error() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(8), "w1:p8", now);

        let (sent, rx) = open(&mut broker, pane(7), now);
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);

        let (sent, rx) = open(&mut MediaBroker::new(), pane(7), now);
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
    }

    #[test]
    fn stale_input_is_refused_and_fresh_input_within_the_window_is_accepted() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);

        let (sent, rx) = open(
            &mut broker,
            pane(7),
            now + MEDIA_INPUT_WINDOW + Duration::from_millis(1),
        );
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);

        let (sent, _rx) = open(&mut broker, pane(7), now + MEDIA_INPUT_WINDOW);
        assert_eq!(opened_session(&sent).0, 1);
    }

    #[test]
    fn a_newer_incapable_client_is_not_skipped_for_an_older_capable_one() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, false);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        broker.note_pane_input(2, pane(7), "w1:p7", now + Duration::from_secs(1));

        let (sent, rx) = open(&mut broker, pane(7), now + Duration::from_secs(1));
        assert!(sent.is_empty());
        assert_eq!(
            response(&rx)["error"]["code"],
            error_code::UNSUPPORTED_CLIENT
        );
    }

    #[test]
    fn a_client_that_no_longer_views_the_pane_is_refused() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open("req".into(), tx, pane(7), |_| false, now));
        assert!(sent.is_empty());
        let body = response(&rx);
        assert_eq!(body["error"]["code"], error_code::REFUSED);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("not_viewed"));
    }

    #[test]
    fn offer_answer_state_and_close_flow_through_the_broker() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);

        assert!(matches!(
            broker.answer(&session_id, "v=0".into()),
            Err((error_code::NOT_READY, _))
        ));

        // Another client cannot speak for the session.
        assert!(run(broker.client_control(
            2,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "x".into()
            }),
            now
        ))
        .is_empty());
        assert!(rx.try_recv().is_err());

        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ));
        let body = response(&rx);
        assert_eq!(body["result"]["type"], "media_offer");
        assert_eq!(body["result"]["session_id"], session_id.as_str());
        assert_eq!(body["result"]["sdp"], "v=0 offer");

        let sent = run(broker.answer(&session_id, "v=0 answer".into()).unwrap());
        assert!(matches!(&sent[..], [(1, MediaControl::Answer(sdp))] if sdp.sdp == "v=0 answer"));
        assert_eq!(
            broker.state(&session_id).unwrap().state,
            MediaSessionState::Connecting
        );

        let sent = run(broker.mute(&session_id, true).unwrap());
        assert!(matches!(&sent[..], [(1, MediaControl::Mute(mute))] if mute.muted));
        run(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: session_id.clone(),
                state: MediaPeerState::Connected,
                muted: true,
                detail: None,
            }),
            now,
        ));
        let view = broker.state(&session_id).unwrap();
        assert_eq!(
            (view.state, view.muted),
            (MediaSessionState::Connected, true)
        );

        let sent = run(broker.close(&session_id, now));
        assert!(
            matches!(&sent[..], [(1, MediaControl::Close(close))] if close.code.as_deref() == Some(close_code::CLOSED))
        );
        assert!(!broker.has_sessions());
        let view = broker.state(&session_id).unwrap();
        assert_eq!(view.state, MediaSessionState::Closed);
        assert!(matches!(
            broker.answer(&session_id, "v=0".into()),
            Err((error_code::SESSION_NOT_FOUND, _))
        ));
        // Closing again is a no-op.
        assert!(run(broker.close(&session_id, now)).is_empty());
    }

    #[test]
    fn client_refusal_reaches_the_waiting_caller_with_its_code() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);

        let sent = run(broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(
                session_id.clone(),
                close_code::DECLINED,
                "the user declined",
            )),
            now,
        ));
        assert!(sent.is_empty(), "the client already knows it refused");
        let body = response(&rx);
        assert_eq!(body["error"]["code"], error_code::REFUSED);
        assert_eq!(body["error"]["message"], "declined: the user declined");
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::DECLINED)
        );
    }

    #[test]
    fn client_disconnect_releases_its_sessions_and_answers_the_caller() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);

        let sent = run(broker.client_removed(1, now));
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::REFUSED);
        assert!(!broker.has_sessions());
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::DISCONNECTED)
        );

        // The removed client's input no longer binds anything.
        let (sent, rx) = open(&mut broker, pane(7), now);
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
    }

    #[test]
    fn a_new_open_replaces_the_old_session_and_timeouts_and_closed_panes_end_sessions() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let (sent, first_rx) = open(&mut broker, pane(7), now);
        let (_, first, _) = opened_session(&sent);

        let (sent, second_rx) = open(&mut broker, pane(7), now);
        assert!(sent.iter().any(|(client, control)| *client == 1 && matches!(control, MediaControl::Close(close) if close.session_id == first && close.code.as_deref() == Some(close_code::REPLACED))));
        assert_eq!(response(&first_rx)["error"]["code"], error_code::REFUSED);
        let (_, second, _) = opened_session(&sent);

        let sent = run(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true));
        assert!(
            matches!(&sent[..], [(1, MediaControl::Close(close))] if close.session_id == second && close.code.as_deref() == Some(close_code::TIMEOUT))
        );
        assert_eq!(response(&second_rx)["error"]["code"], error_code::TIMEOUT);

        let (sent, _rx) = open(&mut broker, pane(7), now);
        let (_, third, _) = opened_session(&sent);
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: third.clone(),
                sdp: "v=0".into(),
            }),
            now,
        ));
        let sent = run(broker.expire(now, |_| false));
        assert!(
            matches!(&sent[..], [(1, MediaControl::Close(close))] if close.session_id == third && close.code.as_deref() == Some(close_code::PANE_CLOSED))
        );

        broker.expire(now + MEDIA_OPEN_TIMEOUT + CLOSED_SESSION_RETENTION, |_| {
            true
        });
        assert!(broker.state(&third).is_none());
    }
}
