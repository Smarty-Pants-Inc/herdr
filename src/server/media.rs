//! Server-side routing of client-local media sessions.
//!
//! `pane.media_open` binds a session to the attached client whose input last reached the
//! pane, because that is the machine the user is sitting at. The broker is pure state: the
//! headless server feeds it client, input and API events and performs the returned actions.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, MediaSessionState, ResponseResult, SuccessResponse,
};
use crate::layout::PaneId;
use crate::protocol::media::{
    close_code, MediaApplied, MediaClose, MediaControl, MediaInfo, MediaLevels, MediaMute,
    MediaOpen, MediaPeerState, MediaSdp, MEDIA_INPUT_WINDOW,
};

/// How long `pane.media_open` waits for the client's offer. It covers the one-time consent
/// prompt (30 s on the client) plus ICE gathering.
pub(crate) const MEDIA_OPEN_TIMEOUT: Duration = Duration::from_secs(45);
/// How long an offered session waits for the remote SDP answer.
pub(crate) const MEDIA_ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
/// Socket response receipts use an mpsc channel, not a server-loop wakeup. Poll only
/// while a receipt is outstanding; all other media work has an absolute deadline.
const RESPONSE_DELIVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long a closed session's final state stays readable through `media.state`.
const CLOSED_SESSION_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_CLOSED_SESSIONS: usize = 32;
/// Bound cancel-before-open reservations independently of closed-session history.
const CANCELLED_OPEN_TTL: Duration = Duration::from_secs(120);
const MAX_CANCELLED_OPENS: usize = 256;

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
        /// The live session whose API response is being delivered, if any.
        session_id: Option<String>,
    },
}

/// The latest input that reached one pane, from any client.
#[derive(Debug, Clone)]
struct PaneOwner {
    client_id: u64,
    /// The pane id exactly as the client sent it, so the client can match its own record.
    pane_ref: String,
    at: Instant,
    /// The same last-input owner also supplies self-declared sender attribution.
    user: Option<String>,
    input_at: u64,
    /// API input invalidates attribution without changing media ownership or age.
    attribution_valid: bool,
}

#[derive(Debug)]
struct MediaClient {
    capable: bool,
    user: Option<String>,
}

#[derive(Debug)]
struct PendingOpen {
    request_id: String,
    respond_to: Sender<String>,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct MediaTelemetry {
    applied: Option<MediaApplied>,
    levels: Option<MediaLevels>,
    info: Option<MediaInfo>,
}

#[derive(Debug)]
struct ReplacedSession {
    session_id: String,
    // Another pending attempt is superseded on acceptance, but is not the connected
    // source that authorizes this renewal. Cancelling it must not cancel this one.
    authorizes_renewal: bool,
    // Keep the caller's handle while a renewal is pending, even if the client already
    // retired the old peer as replaced before sending the successor's offer.
    open_id: Option<String>,
}

#[derive(Debug)]
struct MediaSession {
    client_id: u64,
    pane: PaneId,
    open_id: Option<String>,
    epoch: u64,
    /// The pane id as its client knows it, so a renewal can reopen for that client (smarty-voice#133).
    pane_ref: String,
    /// A handover (smarty-voice#133): the sessions this one replaces once its client accepts it (its offer). Until
    /// then they stay live; if it is refused or times out they are untouched.
    replaces: Vec<ReplacedSession>,
    /// The mute the caller last asked for (herdr#102 security pass): a renewal inherits it, even before the client
    /// reported applying it, and a mute asked for during a handover also reaches the successor.
    wants_muted: bool,
    state: MediaSessionState,
    muted: bool,
    offered_deadline: Option<Instant>,
    response_write_complete: Option<Receiver<()>>,
    abandoned: bool,
    pending: Option<PendingOpen>,
    telemetry: MediaTelemetry,
}

impl MediaSession {
    /// Recheck at event boundaries as well as on the maintenance timer: a late offer
    /// or answer must not revive a timed-out or abandoned request.
    fn retirement_reason(&mut self, now: Instant) -> Option<(&'static str, &'static str)> {
        if let Some(delivery) = self.response_write_complete.as_ref() {
            match delivery.try_recv() {
                Ok(()) => self.abandoned = true,
                Err(TryRecvError::Disconnected) => {
                    // Before the offer, a successful response write is impossible. After
                    // it, dropping the sender without a failure signal means delivery won.
                    self.abandoned |= self.pending.is_some();
                    self.response_write_complete = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.abandoned {
            Some((
                close_code::ABANDONED,
                "the API caller disconnected before the media offer was delivered",
            ))
        } else if self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            Some((
                close_code::TIMEOUT,
                "the client did not send an offer in time",
            ))
        } else if self
            .offered_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            Some((
                close_code::UNANSWERED,
                "the remote peer did not answer the media offer in time",
            ))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaSessionView {
    pub(crate) session_id: String,
    pub(crate) epoch: u64,
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

fn store_client_media_state(session: &mut MediaSession, control: MediaControl) {
    match control {
        MediaControl::Applied(applied) => session.telemetry.applied = Some(applied),
        MediaControl::Levels(levels) => session.telemetry.levels = Some(levels),
        MediaControl::Info(info) => session.telemetry.info = Some(info),
        _ => {}
    }
}

#[derive(Debug)]
pub(crate) struct MediaBroker {
    /// Attached client-shell clients and whether each advertised media support.
    clients: HashMap<u64, MediaClient>,
    /// Per pane, the client whose input reached it last. Input to another pane never moves
    /// this owner, so it cannot reroute a pane's microphone to an older client.
    pane_owners: HashMap<PaneId, PaneOwner>,
    next_owner_cleanup: Option<Instant>,
    pane_epochs: HashMap<PaneId, u64>,
    sessions: HashMap<String, MediaSession>,
    closed: VecDeque<ClosedSession>,
    /// First unknown close time and caller handle, in FIFO order. Repeats do not refresh either.
    cancelled_opens: VecDeque<(Instant, String)>,
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

/// Remove terminal controls and bidi/zero-width formatting before any UI/API exposure.
/// Bound by Unicode scalar count (at most 320 UTF-8 bytes), not untrusted source length.
fn sanitize_user(user: &str) -> Option<String> {
    let name: String = user.chars()
        .filter(|ch| !ch.is_control() && !matches!(*ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        .take(80)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn valid_open_id(open_id: &str) -> bool {
    !open_id.is_empty()
        && open_id.len() <= 128
        && open_id.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
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
            pane_owners: HashMap::new(),
            next_owner_cleanup: None,
            pane_epochs: HashMap::new(),
            sessions: HashMap::new(),
            closed: VecDeque::new(),
            cancelled_opens: VecDeque::new(),
            id_prefix: format!("media_{boot:x}_"),
            next_id: 1,
        }
    }

    pub(crate) fn client_connected(&mut self, client_id: u64, capable: bool) {
        self.clients.insert(
            client_id,
            MediaClient {
                capable,
                user: None,
            },
        );
    }

    /// Display metadata only. Never use this self-declared name for authorization.
    pub(crate) fn set_client_user(&mut self, client_id: u64, user: Option<&str>) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.user = user.and_then(sanitize_user);
        }
    }

    pub(crate) fn last_input(&self, pane: PaneId) -> Option<crate::api::schema::PaneLastInput> {
        let owner = self.pane_owners.get(&pane)?;
        owner
            .attribution_valid
            .then(|| crate::api::schema::PaneLastInput {
                user: owner.user.clone(),
                client_id: owner.client_id,
                at: owner.input_at,
            })
    }

    /// Whether direct input can invalidate any existing pane attribution.
    pub(crate) fn has_input_owners(&self) -> bool {
        !self.pane_owners.is_empty()
    }

    /// API input has no trusted client identity. Do not affect media routing or age.
    pub(crate) fn invalidate_input_attribution(&mut self, pane: PaneId) {
        if let Some(owner) = self.pane_owners.get_mut(&pane) {
            owner.attribution_valid = false;
        }
    }

    /// Record input that reached a pane the client views.
    pub(crate) fn note_pane_input(
        &mut self,
        client_id: u64,
        pane: PaneId,
        pane_ref: &str,
        now: Instant,
    ) {
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        // Attribution has no 10-second TTL. Media still checks its monotonic age in open().
        let input_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        self.pane_owners.insert(
            pane,
            PaneOwner {
                client_id,
                pane_ref: pane_ref.to_owned(),
                at: now,
                user: client.user.clone(),
                input_at,
                attribution_valid: true,
            },
        );
    }

    pub(crate) fn owner_cleanup_due(&self, now: Instant) -> bool {
        (!self.pane_owners.is_empty() || !self.pane_epochs.is_empty())
            && self
                .next_owner_cleanup
                .is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn retain_live_panes(&mut self, now: Instant, pane_exists: impl Fn(PaneId) -> bool) {
        self.pane_owners.retain(|pane, _| pane_exists(*pane));
        // Epochs belong to the pane's lifetime, not its input owner's connection.
        self.pane_epochs.retain(|pane, _| pane_exists(*pane));
        self.next_owner_cleanup = Some(now + Duration::from_secs(1));
    }

    pub(crate) fn client_removed(&mut self, client_id: u64, now: Instant) -> Vec<MediaAction> {
        self.clients.remove(&client_id);
        // Panes this client typed into last now have no owner; they never fall back to an
        // older client.
        self.pane_owners
            .retain(|_, owner| owner.client_id != client_id);
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
        open_id: Option<String>,
        response_write_complete: Option<Receiver<()>>,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        if let Some(open_id) = open_id.as_deref() {
            if !valid_open_id(open_id) {
                actions.push(MediaAction::Respond {
                    respond_to,
                    response: error_response(
                        request_id,
                        "invalid_params",
                        "open_id must be non-empty printable ASCII of at most 128 bytes",
                    ),
                    session_id: None,
                });
                return actions;
            }
            // Refuse before any session maintenance or routing: this request must not
            // send client controls, replace a call, or advance the pane's epoch.
            if self.cancelled_opens.iter().any(|(closed_at, id)| {
                id == open_id && now.saturating_duration_since(*closed_at) < CANCELLED_OPEN_TTL
            }) {
                actions.push(MediaAction::Respond {
                    respond_to,
                    response: error_response(
                        request_id,
                        error_code::REFUSED,
                        "closed: cancelled before open",
                    ),
                    session_id: None,
                });
                return actions;
            }
        }
        // A renewal must not borrow authorization from an expired or abandoned call,
        // and a dead request must not keep its open_id reserved until the next tick.
        actions.extend(self.expire(now, |_| true));
        if response_write_complete.as_ref().is_some_and(|delivery| {
            matches!(
                delivery.try_recv(),
                Ok(()) | Err(TryRecvError::Disconnected)
            )
        }) {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::REFUSED,
                    "abandoned: the API caller disconnected before the media request was opened",
                ),
                session_id: None,
            });
            return actions;
        }
        if let Some(open_id) = open_id.as_deref() {
            if self
                .sessions
                .values()
                .any(|session| session.open_id.as_deref() == Some(open_id))
            {
                actions.push(MediaAction::Respond {
                    respond_to,
                    response: error_response(
                        request_id,
                        "invalid_params",
                        "open_id is already in use by a live media session",
                    ),
                    session_id: None,
                });
                return actions;
            }
        }
        let fresh = self
            .pane_owners
            .get(&pane)
            .filter(|owner| now.saturating_duration_since(owner.at) <= MEDIA_INPUT_WINDOW)
            .and_then(|owner| {
                let capable = self.clients.get(&owner.client_id)?.capable;
                Some((owner.client_id, capable, owner.clone()))
            });
        let handover = fresh.is_none();
        let latest = fresh
            // A renewal or resume minutes into a call (smarty-voice#133): nobody typed in the last 10 s, but the
            // pane's call is live on a client. Only that client, only while its session on this pane is connected and
            // the client is still attached (and, below, capable and viewing the pane). A pane without a live session
            // stays media_no_client, and recent input by any client still wins.
            .or_else(|| {
                self.sessions.values().find_map(|session| {
                    if session.pane != pane
                        || session.state != MediaSessionState::Connected
                        || !views(session.client_id)
                    {
                        return None;
                    }
                    let capable = self.clients.get(&session.client_id)?.capable;
                    let owner = PaneOwner {
                        client_id: session.client_id,
                        pane_ref: session.pane_ref.clone(),
                        at: now,
                        user: None,
                        input_at: 0,
                        attribution_valid: false,
                    };
                    Some((session.client_id, capable, owner))
                })
            });
        let Some((client_id, capable, input)) = latest else {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::NO_CLIENT,
                    "no attached Herdr client typed into this pane in the last 10 seconds",
                ),
                session_id: None,
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
                session_id: None,
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
                session_id: None,
            });
            return actions;
        }

        let Some(epoch) = self
            .pane_epochs
            .get(&pane)
            .copied()
            .unwrap_or_default()
            .checked_add(1)
        else {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::NOT_READY,
                    "the pane's media epoch is exhausted",
                ),
                session_id: None,
            });
            return actions;
        };
        let replaced = self
            .sessions
            .iter()
            .filter(|(_, session)| session.pane == pane || session.client_id == client_id)
            .map(|(id, session)| ReplacedSession {
                session_id: id.clone(),
                authorizes_renewal: session.state == MediaSessionState::Connected,
                open_id: session.open_id.clone(),
            })
            .collect::<Vec<_>>();
        // A handover keeps the live call until the client accepts the new session (client_control, Offer): closing it
        // first would leave the client with no call to renew, and a refusal would end it (herdr#102 review).
        let replaces = if handover {
            replaced
        } else {
            for old in replaced {
                self.finish(
                    &old.session_id,
                    close_code::REPLACED,
                    "a newer media session replaced this one",
                    true,
                    now,
                    &mut actions,
                );
            }
            Vec::new()
        };

        // A renewal keeps the call's mute: muted if the caller asked for it or the client reported it (fail closed).
        let inherited_mute = replaces.iter().any(|old| {
            self.sessions
                .get(&old.session_id)
                .is_some_and(|old| old.wants_muted || old.muted)
        });
        let session_id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        self.pane_epochs.insert(pane, epoch);
        self.sessions.insert(
            session_id.clone(),
            MediaSession {
                client_id,
                pane,
                open_id,
                epoch,
                pane_ref: input.pane_ref.clone(),
                replaces,
                wants_muted: inherited_mute,
                state: MediaSessionState::Opening,
                muted: inherited_mute,
                offered_deadline: None,
                response_write_complete,
                abandoned: false,
                pending: Some(PendingOpen {
                    request_id,
                    respond_to,
                    deadline: now + MEDIA_OPEN_TIMEOUT,
                }),
                telemetry: MediaTelemetry::default(),
            },
        );
        actions.push(MediaAction::Send {
            client_id,
            control: MediaControl::Open(MediaOpen {
                session_id: session_id.clone(),
                pane_id: input.pane_ref,
            }),
        });
        if inherited_mute {
            // Right after the Open it follows: the new peer is muted before it can send any microphone audio.
            actions.push(MediaAction::Send {
                client_id,
                control: MediaControl::Mute(MediaMute {
                    session_id,
                    muted: true,
                }),
            });
        }
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
        if control.validate().is_err() {
            return actions;
        }
        let session_id = control.session_id().to_owned();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return actions;
        };
        if session.client_id != client_id {
            return actions;
        }
        if let Some((code, message)) = session.retirement_reason(now) {
            self.finish(&session_id, code, message, true, now, &mut actions);
            return actions;
        }
        // Telemetry must not bypass the same retirement guard as offers and peer state.
        if matches!(
            &control,
            MediaControl::Applied(_) | MediaControl::Levels(_) | MediaControl::Info(_)
        ) {
            store_client_media_state(session, control);
            return actions;
        }
        match control {
            MediaControl::Offer(MediaSdp { sdp, .. }) => {
                let Some(pending) = session.pending.take() else {
                    return actions;
                };
                session.state = MediaSessionState::Offered;
                session.offered_deadline = Some(now + MEDIA_ANSWER_TIMEOUT);
                let epoch = session.epoch;
                let replaces = std::mem::take(&mut session.replaces);
                // The client accepted a handover: only now is the call it renews replaced.
                for old in replaces {
                    self.finish(
                        &old.session_id,
                        close_code::REPLACED,
                        "a newer media session replaced this one",
                        true,
                        now,
                        &mut actions,
                    );
                }
                actions.push(MediaAction::Respond {
                    respond_to: pending.respond_to,
                    response: success_response(
                        pending.request_id,
                        ResponseResult::MediaOffer {
                            session_id: session_id.clone(),
                            sdp,
                            epoch,
                        },
                    ),
                    session_id: Some(session_id.clone()),
                });
            }
            MediaControl::State(update) => {
                if session.pending.is_some() {
                    return actions;
                }
                session.muted = update.muted;
                // Peer notifications are not proof that the API caller supplied an
                // answer. Keep the deadline and do not grant renewal authority early.
                if session.offered_deadline.is_some() {
                    return actions;
                }
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
            MediaControl::Open(_)
            | MediaControl::Answer(_)
            | MediaControl::Mute(_)
            | MediaControl::Control(_)
            | MediaControl::Applied(_)
            | MediaControl::Levels(_)
            | MediaControl::Info(_) => {}
        }
        actions
    }

    pub(crate) fn answer(
        &mut self,
        session_id: &str,
        sdp: String,
        now: Instant,
    ) -> Result<Vec<MediaAction>, (&'static str, String)> {
        let session = self.live_session(session_id)?;
        if let Some((code, message)) = session.retirement_reason(now) {
            // The timer performs the close side effect. Reject here without clearing
            // its evidence, even when this API event runs before that timer tick.
            return Err((error_code::REFUSED, format!("{code}: {message}")));
        }
        if session.pending.is_some() {
            return Err((
                error_code::NOT_READY,
                "the client has not sent its offer yet".to_owned(),
            ));
        }
        if session.offered_deadline.take().is_some() {
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
        let mut actions = Vec::new();
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.wants_muted = muted;
            actions.push(MediaAction::Send {
                client_id: session.client_id,
                control: MediaControl::Mute(MediaMute {
                    session_id: session_id.to_owned(),
                    muted,
                }),
            });
        } else if !self.has_successor(session_id) {
            // Neither live nor being handed over: the error it always was.
            self.live_session(session_id)?;
        }
        // A renewal of this call still being handed over gets the same mute (it replaces this session once accepted).
        for (id, successor) in self.sessions.iter_mut().filter(|(_, other)| {
            other
                .replaces
                .iter()
                .any(|old| old.authorizes_renewal && old.session_id == session_id)
        }) {
            successor.wants_muted = muted;
            actions.push(MediaAction::Send {
                client_id: successor.client_id,
                control: MediaControl::Mute(MediaMute {
                    session_id: id.clone(),
                    muted,
                }),
            });
        }
        Ok(actions)
    }

    pub(crate) fn state(&self, session_id: &str) -> Option<MediaSessionView> {
        if let Some(session) = self.sessions.get(session_id) {
            return Some(MediaSessionView {
                session_id: session_id.to_owned(),
                epoch: session.epoch,
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

    // This broker seam is consumed by the optional media API/event integration.
    #[allow(dead_code)]
    pub(crate) fn applied(&self, session_id: &str) -> Option<&MediaApplied> {
        self.sessions
            .get(session_id)
            .and_then(|session| session.telemetry.applied.as_ref())
    }

    // This broker seam is consumed by the optional media API/event integration.
    #[allow(dead_code)]
    pub(crate) fn levels(&self, session_id: &str) -> Option<&MediaLevels> {
        self.sessions
            .get(session_id)
            .and_then(|session| session.telemetry.levels.as_ref())
    }

    // This broker seam is consumed by the optional media API/event integration.
    #[allow(dead_code)]
    pub(crate) fn info(&self, session_id: &str) -> Option<&MediaInfo> {
        self.sessions
            .get(session_id)
            .and_then(|session| session.telemetry.info.as_ref())
    }

    /// End a session at the API caller's request. Unknown ids are a no-op.
    pub(crate) fn close(&mut self, session_id: &str, now: Instant) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        // Its client may already have retired it for a renewal still being handed over: ending the call it knows
        // cancels that renewal too (herdr#102 security pass).
        // Apply the caller's reason to authorized successors before finish's generic
        // source-retirement cascade, whether or not the source itself is still live.
        for id in self.successors(session_id) {
            self.finish(
                &id,
                close_code::CLOSED,
                "the caller ended the session",
                true,
                now,
                &mut actions,
            );
        }
        self.finish(
            session_id,
            close_code::CLOSED,
            "the caller ended the session",
            true,
            now,
            &mut actions,
        );
        actions
    }

    pub(crate) fn close_by_open_id(
        &mut self,
        open_id: &str,
        now: Instant,
    ) -> Result<Vec<MediaAction>, (&'static str, String)> {
        if !valid_open_id(open_id) {
            return Err((
                "invalid_params",
                "open_id must be non-empty printable ASCII of at most 128 bytes".to_owned(),
            ));
        }
        let session_id = self
            .sessions
            .iter()
            .find_map(|(session_id, session)| {
                (session.open_id.as_deref() == Some(open_id)).then(|| session_id.clone())
            })
            .or_else(|| {
                self.sessions.values().find_map(|session| {
                    session.replaces.iter().find_map(|old| {
                        (old.authorizes_renewal && old.open_id.as_deref() == Some(open_id))
                            .then(|| old.session_id.clone())
                    })
                })
            });
        let Some(session_id) = session_id else {
            // A valid close can overtake its open on another API connection. Keep a
            // bounded reservation, but let repeated closes retain the original TTL/order.
            self.expire_cancelled_opens(now);
            if !self.cancelled_opens.iter().any(|(_, id)| id == open_id) {
                if self.cancelled_opens.len() == MAX_CANCELLED_OPENS {
                    self.cancelled_opens.pop_front();
                }
                self.cancelled_opens.push_back((now, open_id.to_owned()));
            }
            return Ok(Vec::new());
        };
        Ok(self.close(&session_id, now))
    }

    /// The API response could not be delivered to the caller. Retire the session immediately
    /// and tell the client to release any microphone it opened for the offer.
    pub(crate) fn response_delivery_failed(
        &mut self,
        session_id: &str,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        self.finish(
            session_id,
            close_code::ABANDONED,
            "the API caller disconnected before the media offer was delivered",
            true,
            now,
            &mut actions,
        );
        actions
    }

    /// The next necessary server-loop wakeup. Input attribution and live-pane epoch
    /// bookkeeping alone do not keep an otherwise idle server ticking: topology/input
    /// events drive their existing, throttled maintenance path.
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.sessions
            .values()
            .flat_map(|session| {
                [
                    session.pending.as_ref().map(|pending| pending.deadline),
                    session.offered_deadline,
                    session
                        .response_write_complete
                        .as_ref()
                        .map(|_| now + RESPONSE_DELIVERY_POLL_INTERVAL),
                    session.abandoned.then_some(now),
                ]
                .into_iter()
                .flatten()
            })
            .chain(
                self.closed
                    .front()
                    .map(|closed| closed.closed_at + CLOSED_SESSION_RETENTION),
            )
            .chain(
                self.cancelled_opens
                    .front()
                    .map(|(closed_at, _)| *closed_at + CANCELLED_OPEN_TTL),
            )
            .min()
    }

    /// Time out slow offers, end sessions whose pane closed and drop old history/reservations.
    pub(crate) fn expire(
        &mut self,
        now: Instant,
        pane_exists: impl Fn(PaneId) -> bool,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        self.expire_cancelled_opens(now);
        if self.sessions.is_empty() && self.closed.is_empty() {
            return actions;
        }
        let retired = self
            .sessions
            .iter_mut()
            .filter_map(|(id, session)| {
                session
                    .retirement_reason(now)
                    .map(|(code, message)| (id.clone(), code, message))
            })
            .collect::<Vec<_>>();
        for (id, code, message) in retired {
            self.finish(&id, code, message, true, now, &mut actions);
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

    fn expire_cancelled_opens(&mut self, now: Instant) {
        while self.cancelled_opens.front().is_some_and(|(closed_at, _)| {
            now.saturating_duration_since(*closed_at) >= CANCELLED_OPEN_TTL
        }) {
            self.cancelled_opens.pop_front();
        }
    }

    #[cfg(test)]
    pub(crate) fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Sessions still being handed over as renewals of `session_id` (it is in their `replaces`).
    fn successors(&self, session_id: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, other)| {
                other
                    .replaces
                    .iter()
                    .any(|old| old.authorizes_renewal && old.session_id == session_id)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn has_successor(&self, session_id: &str) -> bool {
        !self.successors(session_id).is_empty()
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
        // Ending a call (not its own replacement) also cancels a renewal of it still being handed over: the successor
        // inherited its authorization from this session (herdr#102 security pass).
        if code != close_code::REPLACED {
            for id in self.successors(session_id) {
                self.finish(
                    &id,
                    close_code::CLOSED,
                    "the call it renewed was ended",
                    true,
                    now,
                    actions,
                );
            }
        }
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
                session_id: Some(session_id.to_owned()),
            });
        }
        self.closed.push_back(ClosedSession {
            closed_at: now,
            view: MediaSessionView {
                session_id: session_id.to_owned(),
                epoch: session.epoch,
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

    type SentControls = Vec<(u64, MediaControl)>;

    fn pane(raw: u32) -> PaneId {
        PaneId::from_raw(raw)
    }

    fn response(rx: &mpsc::Receiver<String>) -> serde_json::Value {
        serde_json::from_str(&rx.try_recv().expect("a response")).expect("json")
    }

    /// Perform actions the way the headless server does; return controls sent per client.
    fn run(actions: Vec<MediaAction>) -> SentControls {
        actions
            .into_iter()
            .filter_map(|action| match action {
                MediaAction::Send { client_id, control } => Some((client_id, control)),
                MediaAction::Respond {
                    respond_to,
                    response,
                    ..
                } => {
                    let _ = respond_to.send(response);
                    None
                }
            })
            .collect()
    }

    fn open_with_id(
        broker: &mut MediaBroker,
        pane_id: PaneId,
        open_id: Option<&str>,
        now: Instant,
    ) -> (Vec<(u64, MediaControl)>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open(
            "req".into(),
            tx,
            pane_id,
            open_id.map(str::to_owned),
            None,
            |_| true,
            now,
        ));
        (sent, rx)
    }

    fn open(
        broker: &mut MediaBroker,
        pane_id: PaneId,
        now: Instant,
    ) -> (Vec<(u64, MediaControl)>, mpsc::Receiver<String>) {
        open_with_id(broker, pane_id, None, now)
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

    fn tracked_open(
        broker: &mut MediaBroker,
        open_id: &str,
        now: Instant,
    ) -> (SentControls, mpsc::Receiver<String>, mpsc::Sender<()>) {
        let (delivery_tx, delivery_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open(
            "req".into(),
            tx,
            pane(7),
            Some(open_id.into()),
            Some(delivery_rx),
            |_| true,
            now,
        ));
        (sent, rx, delivery_tx)
    }

    fn send_offer(
        broker: &mut MediaBroker,
        session_id: &str,
        now: Instant,
    ) -> Vec<(u64, MediaControl)> {
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.into(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ))
    }

    fn peer_state(
        broker: &mut MediaBroker,
        session_id: &str,
        state: MediaPeerState,
        now: Instant,
    ) -> Vec<(u64, MediaControl)> {
        run(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: session_id.into(),
                state,
                muted: true,
                detail: None,
            }),
            now,
        ))
    }

    #[test]
    fn sender_attribution_last_client_wins_and_is_pane_local() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        broker.set_client_user(1, Some("Alice"));
        broker.set_client_user(2, Some("Bob"));
        assert_eq!(broker.last_input(pane(7)), None);
        broker.note_pane_input(1, pane(7), "p_7", now);
        broker.note_pane_input(1, pane(8), "p_8", now);
        broker.note_pane_input(2, pane(7), "w1:p7", now + Duration::from_secs(1));
        let latest = broker.last_input(pane(7)).unwrap();
        assert_eq!(latest.user.as_deref(), Some("Bob"));
        assert_eq!(latest.client_id, 2);
        assert!(
            latest.at >= 1_000_000_000_000,
            "unix milliseconds, not monotonic seconds"
        );
        assert_eq!(
            broker.last_input(pane(8)).unwrap().user.as_deref(),
            Some("Alice")
        );
        // Names are self-declared; a missing name never borrows another client's.
        broker.client_connected(3, false);
        broker.note_pane_input(3, pane(7), "p_7", now + Duration::from_secs(2));
        let anonymous = broker.last_input(pane(7)).unwrap();
        assert_eq!(anonymous.user, None);
        assert_eq!(anonymous.client_id, 3);
        assert!(anonymous.at > 0);
    }

    #[test]
    fn api_input_clears_attribution_but_preserves_media_owner_and_freshness() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.set_client_user(1, Some("Alice"));
        broker.note_pane_input(1, pane(7), "p_7", now);
        let input = broker.last_input(pane(7)).unwrap();
        broker.invalidate_input_attribution(pane(7));
        assert_eq!(broker.last_input(pane(7)), None);
        assert_eq!(broker.pane_owners[&pane(7)].at, now);
        let (sent, _) = open(&mut broker, pane(7), now + Duration::from_secs(1));
        assert_eq!(
            opened_session(&sent).0,
            1,
            "API input cannot reroute microphone"
        );
        let (sent, rx) = open(
            &mut broker,
            pane(7),
            now + MEDIA_INPUT_WINDOW + Duration::from_secs(1),
        );
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
        broker.note_pane_input(1, pane(7), "p_7", now + Duration::from_secs(20));
        assert_eq!(broker.last_input(pane(7)).unwrap().user, input.user);
    }

    #[test]
    fn attribution_outlives_media_window_but_not_panes_or_disconnect() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.set_client_user(1, Some("Alice"));
        broker.note_pane_input(1, pane(7), "p_7", now);
        let input = broker.last_input(pane(7));
        broker.note_pane_input(1, pane(8), "p_8", now + Duration::from_secs(30));
        assert_eq!(
            broker.last_input(pane(7)),
            input,
            "input on another pane must not prune attribution"
        );
        let (sent, rx) = open(&mut broker, pane(7), now + Duration::from_secs(30));
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
        broker.retain_live_panes(now, |id| id == pane(8));
        assert_eq!(broker.last_input(pane(7)), None);
        assert!(broker.last_input(pane(8)).is_some());
        assert!(!broker.owner_cleanup_due(now));
        assert!(broker.owner_cleanup_due(now + Duration::from_secs(1)));
        run(broker.client_removed(1, now));
        assert_eq!(broker.last_input(pane(8)), None);
    }

    #[test]
    fn self_declared_names_are_sanitized_and_bounded() {
        assert_eq!(
            sanitize_user("  Al\n\rice\t\u{1b}\u{7}\u{202e}  ").as_deref(),
            Some("Alice")
        );
        assert_eq!(sanitize_user(" \t\u{1b}\u{200b} "), None);
        let long = "界".repeat(1000);
        assert_eq!(sanitize_user(&long).unwrap().chars().count(), 80);
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
    fn input_to_another_pane_does_not_move_a_pane_to_an_older_client() {
        for b_capable in [true, false] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, b_capable);
            broker.note_pane_input(1, pane(7), "w1:p7", now);
            broker.note_pane_input(2, pane(7), "w1:p7", now + Duration::from_secs(1));
            broker.note_pane_input(2, pane(8), "w1:p8", now + Duration::from_secs(2));

            let (sent, rx) = open(&mut broker, pane(7), now + Duration::from_secs(3));
            assert!(
                sent.iter().all(|(client_id, _)| *client_id != 1),
                "client A must never receive the open"
            );
            if b_capable {
                assert_eq!(opened_session(&sent).0, 2);
            } else {
                assert!(sent.is_empty());
                assert_eq!(
                    response(&rx)["error"]["code"],
                    error_code::UNSUPPORTED_CLIENT
                );
            }
        }
    }

    #[test]
    fn a_departed_owner_leaves_the_pane_without_a_client() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        broker.note_pane_input(2, pane(7), "w1:p7", now + Duration::from_secs(1));
        run(broker.client_removed(2, now));

        let (sent, rx) = open(&mut broker, pane(7), now + Duration::from_secs(2));
        assert!(sent.is_empty(), "no fallback to the older client");
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
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
        let sent = run(broker.open("req".into(), tx, pane(7), None, None, |_| false, now));
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
            broker.answer(&session_id, "v=0".into(), now),
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

        let sent = run(broker
            .answer(&session_id, "v=0 answer".into(), now)
            .unwrap());
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
            broker.answer(&session_id, "v=0".into(), now),
            Err((error_code::SESSION_NOT_FOUND, _))
        ));
        // Closing again is a no-op.
        assert!(run(broker.close(&session_id, now)).is_empty());
    }

    #[test]
    fn open_id_cancels_a_pending_session_and_answers_the_caller() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("voice-1"), now);
        let (_, session_id, _) = opened_session(&sent);

        let sent = run(broker.close_by_open_id("voice-1", now).unwrap());
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == session_id && close.code.as_deref() == Some(close_code::CLOSED)));
        let refused = response(&rx);
        assert_eq!(refused["error"]["code"], error_code::REFUSED);
        assert_eq!(
            refused["error"]["message"],
            "closed: the caller ended the session"
        );
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::CLOSED)
        );
    }

    #[test]
    fn unknown_open_id_close_refuses_later_opens_without_touching_sessions() {
        for with_source in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            let source = with_source.then(|| {
                broker.client_connected(1, true);
                live_call(&mut broker, 1, pane(7), now)
            });
            let original = source.as_deref().and_then(|id| broker.state(id));
            let epochs = broker.pane_epochs.clone();
            let next_id = broker.next_id;
            assert!(broker
                .close_by_open_id("cancelled", now)
                .unwrap()
                .is_empty());
            for at in [now, now + MEDIA_INPUT_WINDOW + Duration::from_secs(1)] {
                for pane_id in [pane(7), pane(8)] {
                    let (tx, rx) = mpsc::channel();
                    let sent = run(broker.open(
                        "req".into(),
                        tx,
                        pane_id,
                        Some("cancelled".into()),
                        None,
                        |_| panic!("a cancelled open must not inspect client routing"),
                        at,
                    ));
                    assert!(sent.is_empty());
                    assert_eq!(
                        response(&rx),
                        serde_json::json!({
                            "id": "req",
                            "error": {
                                "code": "media_refused",
                                "message": "closed: cancelled before open"
                            }
                        })
                    );
                    assert_eq!(broker.sessions.len(), usize::from(with_source));
                    assert_eq!(source.as_deref().and_then(|id| broker.state(id)), original);
                    assert_eq!(broker.pane_epochs, epochs);
                    assert_eq!(broker.next_id, next_id);
                    assert!(broker.closed.is_empty());
                }
            }
            assert_eq!(
                broker.cancelled_opens.len(),
                1,
                "refusal does not consume the id"
            );
            assert_eq!(
                broker.next_deadline(now),
                Some(now + Duration::from_secs(120))
            );
        }
    }

    #[test]
    fn cancelled_open_id_expires_at_exactly_120_seconds_and_can_be_reused() {
        for expire_first in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            assert!(broker
                .close_by_open_id("cancelled", now)
                .unwrap()
                .is_empty());
            let deadline = now + Duration::from_secs(120);
            let before = deadline - Duration::from_nanos(1);
            assert_eq!(broker.next_deadline(now), Some(deadline));
            assert!(broker
                .expire(before, |_| panic!("no live sessions"))
                .is_empty());
            assert_eq!(broker.cancelled_opens.len(), 1);
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "p_7", before);
            let (sent, rx) = open_with_id(&mut broker, pane(7), Some("cancelled"), before);
            assert!(sent.is_empty());
            assert_eq!(
                response(&rx)["error"]["message"],
                "closed: cancelled before open"
            );
            assert!(broker.pane_epochs.is_empty());
            if expire_first {
                assert!(broker
                    .expire(deadline, |_| panic!("no live sessions"))
                    .is_empty());
                assert!(broker.cancelled_opens.is_empty());
                assert_eq!(broker.next_deadline(deadline), None);
            }
            // Refresh the real owner at reuse time so only the tombstone's TTL differs.
            broker.note_pane_input(1, pane(7), "p_7", deadline);
            // With no maintenance tick, open's existing expiry path must also release it.
            let (sent, rx) = open_with_id(&mut broker, pane(7), Some("cancelled"), deadline);
            let (_, session_id, _) = opened_session(&sent);
            assert!(broker.cancelled_opens.is_empty());
            assert_eq!(broker.state(&session_id).unwrap().epoch, 1);
            assert!(send_offer(&mut broker, &session_id, deadline).is_empty());
            assert_eq!(response(&rx)["result"]["session_id"], session_id);
        }
    }

    #[test]
    fn cancelled_open_ids_are_bounded_to_256_and_evict_the_oldest_first_close() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        for index in 0..256 {
            let at = now + Duration::from_millis(index);
            assert!(broker
                .close_by_open_id(&format!("cancelled-{index}"), at)
                .unwrap()
                .is_empty());
            assert_eq!(broker.cancelled_opens.len(), index as usize + 1);
        }
        let later = now + Duration::from_millis(256);
        assert!(broker
            .close_by_open_id("cancelled-0", later)
            .unwrap()
            .is_empty());
        assert_eq!(broker.cancelled_opens.len(), 256);
        assert_eq!(broker.cancelled_opens.front().unwrap().0, now);
        assert!(broker
            .close_by_open_id("cancelled-256", later)
            .unwrap()
            .is_empty());
        assert_eq!(broker.cancelled_opens.len(), 256);
        assert_eq!(broker.cancelled_opens.front().unwrap().1, "cancelled-1");
        assert_eq!(broker.cancelled_opens.back().unwrap().1, "cancelled-256");
        assert_eq!(
            broker.next_deadline(later),
            Some(now + Duration::from_millis(1) + Duration::from_secs(120))
        );

        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", later);
        let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("cancelled-0"), later);
        let (_, session_id, _) = opened_session(&sent);
        for id in ["cancelled-1", "cancelled-256"] {
            let (sent, rx) = open_with_id(&mut broker, pane(7), Some(id), later);
            assert!(sent.is_empty());
            assert_eq!(
                response(&rx)["error"]["message"],
                "closed: cancelled before open"
            );
            assert_eq!(broker.sessions.len(), 1);
            assert_eq!(
                broker.state(&session_id).unwrap().state,
                MediaSessionState::Opening
            );
            assert_eq!(broker.pane_epochs[&pane(7)], 1);
        }
    }

    #[test]
    fn repeated_unknown_closes_keep_one_reservation_without_refreshing_ttl_or_order() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        for index in 0..1024 {
            let at = now + Duration::from_millis(index);
            assert!(broker.close_by_open_id("cancelled", at).unwrap().is_empty());
            assert_eq!(broker.cancelled_opens.len(), 1);
            assert_eq!(broker.cancelled_opens.front().unwrap().0, now);
            assert_eq!(
                broker.next_deadline(at),
                Some(now + Duration::from_secs(120))
            );
        }
        assert!(broker
            .close_by_open_id("cancelled", now + Duration::from_secs(119))
            .unwrap()
            .is_empty());
        assert_eq!(
            broker.next_deadline(now),
            Some(now + Duration::from_secs(120))
        );
        // An unknown close at the old TTL starts one new reservation, even before a tick.
        let deadline = now + Duration::from_secs(120);
        assert!(broker
            .close_by_open_id("cancelled", deadline)
            .unwrap()
            .is_empty());
        assert_eq!(broker.cancelled_opens.len(), 1);
        assert_eq!(broker.cancelled_opens.front().unwrap().0, deadline);
        assert_eq!(
            broker.next_deadline(deadline),
            Some(now + Duration::from_secs(240))
        );
        assert!(broker
            .expire(now + Duration::from_secs(240), |_| panic!(
                "no live sessions"
            ))
            .is_empty());
        assert!(broker.cancelled_opens.is_empty());
        assert_eq!(broker.next_deadline(now + Duration::from_secs(240)), None);
    }

    #[test]
    fn tombstone_deadlines_do_not_delay_session_or_closed_history_expiry() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, first_rx) = open_with_id(&mut broker, pane(7), Some("first"), now);
        let (_, first, _) = opened_session(&sent);
        run(broker.close_by_open_id("first", now).unwrap());
        assert_eq!(response(&first_rx)["error"]["code"], error_code::REFUSED);
        assert_eq!(
            broker.next_deadline(now),
            Some(now + CLOSED_SESSION_RETENTION)
        );
        assert!(broker
            .close_by_open_id("cancelled", now)
            .unwrap()
            .is_empty());
        assert_eq!(
            broker.next_deadline(now),
            Some(now + Duration::from_secs(120))
        );
        let (sent, second_rx) = open(&mut broker, pane(7), now);
        let (_, second, _) = opened_session(&sent);
        assert_eq!(broker.next_deadline(now), Some(now + MEDIA_OPEN_TIMEOUT));
        let timed_out = now + MEDIA_OPEN_TIMEOUT;
        let sent = run(broker.expire(timed_out, |_| true));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == second && close.code.as_deref() == Some(close_code::TIMEOUT)));
        assert_eq!(response(&second_rx)["error"]["code"], error_code::TIMEOUT);
        assert_eq!(
            broker.next_deadline(timed_out),
            Some(now + Duration::from_secs(120))
        );
        assert!(broker
            .expire(now + Duration::from_secs(120), |_| true)
            .is_empty());
        assert!(broker.cancelled_opens.is_empty());
        assert_eq!(
            broker.next_deadline(now),
            Some(now + CLOSED_SESSION_RETENTION)
        );
        assert!(broker
            .expire(now + CLOSED_SESSION_RETENTION, |_| true)
            .is_empty());
        assert!(broker.state(&first).is_none());
        assert!(broker.state(&second).is_some());
        assert_eq!(
            broker.next_deadline(now),
            Some(timed_out + CLOSED_SESSION_RETENTION)
        );
        assert!(broker
            .expire(timed_out + CLOSED_SESSION_RETENTION, |_| true)
            .is_empty());
        assert_eq!(
            broker.next_deadline(timed_out + CLOSED_SESSION_RETENTION),
            None
        );
    }

    #[test]
    fn duplicate_live_open_id_is_rejected_without_replacing_the_first_session() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _first_rx) = open_with_id(&mut broker, pane(7), Some("voice-1"), now);
        let (_, first, _) = opened_session(&sent);

        let (sent, second_rx) = open_with_id(&mut broker, pane(7), Some("voice-1"), now);
        assert!(sent.is_empty());
        assert_eq!(response(&second_rx)["error"]["code"], "invalid_params");
        assert!(
            broker.state(&first).is_some(),
            "the original live session remains"
        );
    }

    #[test]
    fn open_id_can_close_after_offer_and_while_connected() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("offered"), now);
        let (_, offered, _) = opened_session(&sent);
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: offered.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ));
        assert_eq!(response(&rx)["result"]["epoch"], 1);
        let sent = run(broker.close_by_open_id("offered", now).unwrap());
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == offered && close.code.as_deref() == Some(close_code::CLOSED)));

        let connected = open_with_id(&mut broker, pane(7), Some("connected"), now);
        let (sent, _rx) = connected;
        let (_, connected, _) = opened_session(&sent);
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: connected.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ));
        run(broker.answer(&connected, "v=0 answer".into(), now).unwrap());
        run(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: connected.clone(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            }),
            now,
        ));
        let sent = run(broker.close_by_open_id("connected", now).unwrap());
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == connected && close.code.as_deref() == Some(close_code::CLOSED)));
    }

    #[test]
    fn abandoned_offer_closes_the_client_session() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (delivery_tx, delivery_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open(
            "req".into(),
            tx,
            pane(7),
            Some("abandoned".into()),
            Some(delivery_rx),
            |_| true,
            now,
        ));
        let (_, session_id, _) = opened_session(&sent);
        // This mirrors the API socket detecting that its waiting caller disconnected.
        drop(rx);
        delivery_tx.send(()).unwrap();
        let sent = run(broker.expire(now, |_| true));
        assert!(sent.iter().any(|(client, control)| *client == 1
            && matches!(control, MediaControl::Close(close)
                if close.session_id == session_id && close.code.as_deref() == Some(close_code::ABANDONED))));
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::ABANDONED)
        );
    }

    #[test]
    fn failed_offer_response_sender_abandons_the_offered_session() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("sender-failed"), now);
        let (_, session_id, _) = opened_session(&sent);
        drop(rx);
        let actions = broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        );
        assert!(actions
            .iter()
            .any(|action| matches!(action, MediaAction::Respond { .. })));
        let sent = run(broker.response_delivery_failed(&session_id, now));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == session_id && close.code.as_deref() == Some(close_code::ABANDONED)));
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::ABANDONED)
        );
    }

    #[test]
    fn unanswered_offer_expires_with_a_distinct_close_code() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("unanswered"), now);
        let (_, session_id, _) = opened_session(&sent);
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ));
        let _ = response(&rx);
        let sent = run(broker.expire(now + MEDIA_ANSWER_TIMEOUT, |_| true));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == session_id && close.code.as_deref() == Some(close_code::UNANSWERED)));
        assert_eq!(
            broker.state(&session_id).unwrap().code.as_deref(),
            Some(close_code::UNANSWERED)
        );
    }

    #[test]
    fn cancelling_a_renewal_source_cancels_its_pending_successor() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let old = {
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("old"), now);
            let (_, old, _) = opened_session(&sent);
            run(broker.client_control(
                1,
                MediaControl::Offer(MediaSdp {
                    session_id: old.clone(),
                    sdp: "v=0 old".into(),
                }),
                now,
            ));
            run(broker.answer(&old, "v=0 answer".into(), now).unwrap());
            run(broker.client_control(
                1,
                MediaControl::State(MediaStateUpdate {
                    session_id: old.clone(),
                    state: MediaPeerState::Connected,
                    muted: false,
                    detail: None,
                }),
                now,
            ));
            old
        };
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("successor"), later);
        let (_, successor, _) = opened_session(&sent);
        assert_eq!(
            broker.state(&old).unwrap().state,
            MediaSessionState::Connected
        );
        let sent = run(broker.close_by_open_id("old", later).unwrap());
        assert!(sent.iter().any(|(client, control)| *client == 1
            && matches!(control, MediaControl::Close(close)
                if close.session_id == old && close.code.as_deref() == Some(close_code::CLOSED))));
        assert!(sent.iter().any(|(client, control)| *client == 1
            && matches!(control, MediaControl::Close(close)
                if close.session_id == successor && close.code.as_deref() == Some(close_code::CLOSED))));
        assert_eq!(
            broker.state(&successor).unwrap().code.as_deref(),
            Some(close_code::CLOSED)
        );
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
    fn broker_rejects_wrong_owner_and_stores_latest_device_state() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let session_id = live_call(&mut broker, 1, pane(7), now);

        assert!(run(broker.client_control(
            2,
            MediaControl::Applied(MediaApplied {
                session_id: session_id.clone(),
                control_id: 1,
                input_muted: true,
                output_suppressed: false,
            }),
            now,
        ))
        .is_empty());
        assert!(broker.applied(&session_id).is_none());

        run(broker.client_control(
            1,
            MediaControl::Applied(MediaApplied {
                session_id: session_id.clone(),
                control_id: 1,
                input_muted: true,
                output_suppressed: false,
            }),
            now,
        ));
        run(broker.client_control(
            1,
            MediaControl::Applied(MediaApplied {
                session_id: session_id.clone(),
                control_id: 2,
                input_muted: false,
                output_suppressed: true,
            }),
            now,
        ));
        assert_eq!(broker.applied(&session_id).unwrap().control_id, 2);
        assert!(!broker.applied(&session_id).unwrap().input_muted);
        assert!(broker.applied(&session_id).unwrap().output_suppressed);

        run(broker.client_control(
            1,
            MediaControl::Levels(MediaLevels {
                session_id: session_id.clone(),
                input_peak: 0.25,
                output_peak: 0.75,
            }),
            now,
        ));
        assert_eq!(
            (
                broker.levels(&session_id).unwrap().input_peak,
                broker.levels(&session_id).unwrap().output_peak,
            ),
            (0.25, 0.75)
        );

        run(broker.client_control(
            1,
            MediaControl::Info(MediaInfo {
                session_id: session_id.clone(),
                version: "0.1.0".into(),
                commit: "abc123".into(),
                pid: 42,
                input_device: "mic".into(),
                output_device: "speaker".into(),
            }),
            now,
        ));
        assert_eq!(broker.info(&session_id).unwrap().pid, 42);
        assert_eq!(broker.info(&session_id).unwrap().input_device, "mic");
    }

    // Microphone ownership boundary: late device callbacks must obey cancellation and
    // deadline races even before maintenance runs, without touching a renewal's source.
    #[test]
    fn telemetry_cannot_bypass_retirement_or_revive_a_cancelled_renewal() {
        let telemetry = |session_id: &str| {
            [
                MediaControl::Applied(MediaApplied {
                    session_id: session_id.into(),
                    control_id: 1,
                    input_muted: true,
                    output_suppressed: false,
                }),
                MediaControl::Levels(MediaLevels {
                    session_id: session_id.into(),
                    input_peak: 0.25,
                    output_peak: 0.75,
                }),
                MediaControl::Info(MediaInfo {
                    session_id: session_id.into(),
                    version: "test".into(),
                    commit: "abc123".into(),
                    pid: 42,
                    input_device: "mic".into(),
                    output_device: "speaker".into(),
                }),
            ]
        };
        for kind in 0..3 {
            for code in [
                close_code::TIMEOUT,
                close_code::UNANSWERED,
                close_code::ABANDONED,
                close_code::CLOSED,
            ] {
                let now = Instant::now();
                let mut broker = MediaBroker::new();
                broker.client_connected(1, true);
                broker.client_connected(2, true);
                let source = live_call(&mut broker, 1, pane(7), now);
                let original = broker.state(&source);
                let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
                let (sent, rx, delivery) = tracked_open(&mut broker, "renewal", later);
                let (_, session_id, _) = opened_session(&sent);
                for control in telemetry(&session_id) {
                    assert!(run(broker.client_control(1, control, later)).is_empty());
                }
                assert!(broker.applied(&session_id).is_some());
                assert!(broker.levels(&session_id).is_some());
                assert!(broker.info(&session_id).is_some());
                let at = match code {
                    close_code::TIMEOUT => later + MEDIA_OPEN_TIMEOUT,
                    close_code::UNANSWERED => {
                        send_offer(&mut broker, &session_id, later);
                        let _ = response(&rx);
                        later + MEDIA_ANSWER_TIMEOUT
                    }
                    close_code::ABANDONED => {
                        delivery.send(()).unwrap();
                        later
                    }
                    close_code::CLOSED => {
                        run(broker.close_by_open_id("renewal", later).unwrap());
                        later
                    }
                    _ => unreachable!(),
                };
                let control = telemetry(&session_id)[kind].clone();
                let before = broker.state(&session_id);
                assert!(run(broker.client_control(2, control.clone(), at)).is_empty());
                assert_eq!(broker.state(&session_id), before, "wrong owner: {code}");
                let sent = run(broker.client_control(1, control, at));
                if code == close_code::CLOSED {
                    assert!(sent.is_empty(), "already cancelled");
                } else {
                    assert!(
                        matches!(&sent[..], [(1, MediaControl::Close(close))]
                        if close.session_id == session_id && close.code.as_deref() == Some(code)),
                        "{code}, telemetry kind {kind}: {sent:?}"
                    );
                }
                if code != close_code::UNANSWERED {
                    assert!(response(&rx).get("error").is_some());
                    assert_eq!(broker.state(&source), original, "{code}");
                }
                let closed = broker.state(&session_id).unwrap();
                assert_eq!(closed.state, MediaSessionState::Closed);
                assert_eq!(closed.code.as_deref(), Some(code));
                assert!(broker.applied(&session_id).is_none());
                assert!(broker.levels(&session_id).is_none());
                assert!(broker.info(&session_id).is_none());
                assert!(run(broker.expire(at, |_| true)).is_empty());

                // Reusing the caller's handle must not grant old callbacks the new identity.
                broker.note_pane_input(1, pane(7), "p_7", at);
                let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("renewal"), at);
                let (_, reused, _) = opened_session(&sent);
                assert_ne!(reused, session_id);
                for control in telemetry(&session_id) {
                    assert!(run(broker.client_control(1, control, at)).is_empty());
                }
                assert_eq!(broker.state(&session_id), Some(closed));
                let current = broker.state(&reused).unwrap();
                assert_eq!(current.state, MediaSessionState::Opening);
                assert_eq!(current.epoch, 3);
                assert!(broker.applied(&reused).is_none());
                assert!(broker.levels(&reused).is_none());
                assert!(broker.info(&reused).is_none());
            }
        }
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

    /// A session on `pane` for the client that typed into it, carried to Connected (a call in progress).
    fn live_call(
        broker: &mut MediaBroker,
        client_id: u64,
        pane_id: PaneId,
        now: Instant,
    ) -> String {
        broker.note_pane_input(client_id, pane_id, "w1:p7", now);
        let (sent, _rx) = open(broker, pane_id, now);
        let (_, session_id, _) = opened_session(&sent);
        run(broker.client_control(
            client_id,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "v=0 offer".into(),
            }),
            now,
        ));
        run(broker
            .answer(&session_id, "v=0 answer".into(), now)
            .unwrap());
        run(broker.client_control(
            client_id,
            MediaControl::State(MediaStateUpdate {
                session_id: session_id.clone(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            }),
            now,
        ));
        session_id
    }

    // smarty-voice#133: a provider renewal an hour into a native call reopens media with nobody typing.
    #[test]
    fn a_renewal_with_no_recent_input_binds_to_the_live_calls_client_and_replaces_it() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let old = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);

        let (sent, rx) = open(&mut broker, pane(7), later);
        let (client_id, new, pane_ref) = opened_session(&sent);
        assert_eq!((client_id, pane_ref.as_str()), (1, "w1:p7"));
        assert_ne!(new, old);
        assert!(rx.try_recv().is_err(), "the caller waits for the new offer");
        // A handover: the old call stays live until the client accepts the new session (its offer), so a refusal
        // or a timeout leaves the call as it was (herdr#102 review).
        assert!(broker.state(&new).is_some());
        assert!(sent
            .iter()
            .all(|(_, control)| !matches!(control, MediaControl::Close(_))));
        assert_eq!(
            broker.state(&old).map(|view| view.state),
            Some(MediaSessionState::Connected)
        );
        assert!(
            sent.iter().all(|(client_id, _)| *client_id == 1),
            "no other client is told anything"
        );
        let sent = run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: new.clone(),
                sdp: "v=0 new".into(),
            }),
            later,
        ));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == old && close.code.as_deref() == Some(close_code::REPLACED)));
        assert_ne!(
            broker.state(&old).map(|view| view.state),
            Some(MediaSessionState::Connected)
        );
        assert_eq!(response(&rx)["result"]["session_id"], new.as_str());
    }

    #[test]
    fn a_pane_with_no_live_call_still_has_no_client() {
        for state in [
            None,
            Some(MediaPeerState::Connecting),
            Some(MediaPeerState::Failed),
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "w1:p7", now);
            let (sent, _rx) = open(&mut broker, pane(7), now);
            let (_, session_id, _) = opened_session(&sent);
            if let Some(state) = state {
                run(broker.client_control(
                    1,
                    MediaControl::Offer(MediaSdp {
                        session_id: session_id.clone(),
                        sdp: "v=0".into(),
                    }),
                    now,
                ));
                run(broker.client_control(
                    1,
                    MediaControl::State(MediaStateUpdate {
                        session_id,
                        state,
                        muted: false,
                        detail: None,
                    }),
                    now,
                ));
            }
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
            let (sent, rx) = open(&mut broker, pane(7), later);
            assert!(sent
                .iter()
                .all(|(_, control)| !matches!(control, MediaControl::Open(_))));
            assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
        }
    }

    #[test]
    fn the_live_calls_client_that_no_longer_views_the_pane_is_refused() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let old = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open("req".into(), tx, pane(7), None, None, |_| false, later));
        assert!(sent
            .iter()
            .all(|(_, control)| !matches!(control, MediaControl::Open(_))));
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
        assert_eq!(
            broker.state(&old).map(|view| view.state),
            Some(MediaSessionState::Connected),
            "the call is left alone"
        );
        // A departed client leaves the pane with no client at all.
        run(broker.client_removed(1, later));
        let (_, rx) = open(&mut broker, pane(7), later);
        assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
    }

    #[test]
    fn recent_input_by_another_client_still_wins_over_the_live_call() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        broker.note_pane_input(2, pane(7), "p_7", later);
        let (sent, _rx) = open(&mut broker, pane(7), later + Duration::from_secs(1));
        let (client_id, _, pane_ref) = opened_session(&sent);
        assert_eq!((client_id, pane_ref.as_str()), (2, "p_7"));
    }

    // Cancellation is a microphone-ownership boundary. These deterministic tests also
    // cover deadline races that a live 20/45-second timeout flow cannot cheaply schedule.
    #[test]
    fn invalid_open_ids_never_replace_a_session_or_advance_its_epoch() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("original"), now);
        let (_, original, _) = opened_session(&sent);
        let mut invalid = vec![String::new(), "é".into(), "x".repeat(129)];
        invalid.extend(
            (0u8..=127)
                .filter(|byte| !(0x20..=0x7e).contains(byte))
                .map(|byte| char::from(byte).to_string()),
        );
        for id in invalid {
            let (sent, rx) = open_with_id(&mut broker, pane(7), Some(&id), now);
            assert!(sent.is_empty(), "invalid id {id:?}");
            assert_eq!(response(&rx)["error"]["code"], "invalid_params");
            assert!(matches!(
                broker.close_by_open_id(&id, now),
                Err(("invalid_params", _))
            ));
            assert!(
                broker.cancelled_opens.is_empty(),
                "invalid ids cannot reserve a handle"
            );
            assert_eq!(
                broker.state(&original).unwrap().state,
                MediaSessionState::Opening
            );
            assert_eq!(broker.pane_epochs[&pane(7)], 1);
        }
        for id in [
            " ".to_owned(),
            "~".to_owned(),
            "x".repeat(128),
            (0x20u8..=0x7e).map(char::from).collect(),
        ] {
            let (sent, _rx) = open_with_id(&mut broker, pane(7), Some(&id), now);
            let (_, session_id, _) = opened_session(&sent);
            run(broker.close_by_open_id(&id, now).unwrap());
            assert_eq!(
                broker.state(&session_id).unwrap().code.as_deref(),
                Some(close_code::CLOSED)
            );
        }
        assert!(run(broker.close_by_open_id("unknown", now).unwrap()).is_empty());
    }

    #[test]
    fn open_id_is_unique_across_panes_in_every_live_state_and_reusable_after_close() {
        for stage in 0..4 {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            broker.note_pane_input(2, pane(8), "p_8", now);
            let (sent, rx) = open_with_id(&mut broker, pane(7), Some("shared"), now);
            let (_, original, _) = opened_session(&sent);
            if stage > 0 {
                send_offer(&mut broker, &original, now);
                assert_eq!(response(&rx)["result"]["epoch"], 1);
            }
            if stage > 1 {
                run(broker.answer(&original, "v=0 answer".into(), now).unwrap());
            }
            if stage > 2 {
                peer_state(&mut broker, &original, MediaPeerState::Connected, now);
            }
            let (sent, duplicate_rx) = open_with_id(&mut broker, pane(8), Some("shared"), now);
            assert!(sent.is_empty());
            assert_eq!(response(&duplicate_rx)["error"]["code"], "invalid_params");
            assert_eq!(broker.sessions.len(), 1);
            assert!(!broker.pane_epochs.contains_key(&pane(8)));
            run(broker.close_by_open_id("shared", now).unwrap());
            assert!(
                broker.cancelled_opens.is_empty(),
                "a live cancellation must not reserve the handle"
            );
            let (sent, _rx) = open_with_id(&mut broker, pane(8), Some("shared"), now);
            let (client_id, reused, _) = opened_session(&sent);
            assert_eq!(client_id, 2);
            assert_ne!(original, reused);
            assert_eq!(broker.state(&reused).unwrap().epoch, 1);
            // A late response failure still owns only the old session, not its reused handle.
            assert!(run(broker.response_delivery_failed(&original, now)).is_empty());
            assert!(broker.sessions.contains_key(&reused));
        }
    }

    #[test]
    fn pane_epochs_count_sent_opens_survive_owner_disconnect_and_prune_with_dead_panes() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("reuse"), now);
        let (_, first, _) = opened_session(&sent);
        assert_eq!(broker.state(&first).unwrap().epoch, 1);
        run(broker.close_by_open_id("reuse", now).unwrap());
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("reuse"), now);
        let (_, second, _) = opened_session(&sent);
        send_offer(&mut broker, &second, now);
        assert_eq!(
            response(&rx)["result"]["epoch"],
            2,
            "even an unoffered open counts"
        );
        run(broker.client_removed(1, now));
        assert!(broker.pane_owners.is_empty());
        assert!(
            broker.owner_cleanup_due(now),
            "epochs need cleanup without owners"
        );
        broker.retain_live_panes(now, |_| true);
        assert_eq!(broker.pane_epochs[&pane(7)], 2);
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _rx) = open(&mut broker, pane(7), now);
        let (_, third, _) = opened_session(&sent);
        assert_eq!(broker.state(&third).unwrap().epoch, 3);
        run(broker.client_removed(1, now));
        broker.retain_live_panes(now + Duration::from_secs(1), |_| false);
        assert!(broker.pane_epochs.is_empty());
        assert!(!broker.owner_cleanup_due(now + Duration::from_secs(2)));
    }

    #[test]
    fn epoch_exhaustion_refuses_without_replacing_the_live_session() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _rx) = open(&mut broker, pane(7), now);
        let (_, original, _) = opened_session(&sent);
        broker.pane_epochs.insert(pane(7), u64::MAX);
        let (sent, rx) = open(&mut broker, pane(7), now);
        assert!(sent.is_empty());
        assert_eq!(response(&rx)["error"]["code"], error_code::NOT_READY);
        assert!(broker.sessions.contains_key(&original));
        assert_eq!(broker.pane_epochs[&pane(7)], u64::MAX);
    }

    #[test]
    fn early_peer_states_cannot_authorize_renewal_or_make_an_answered_call_expire() {
        for state in [
            MediaPeerState::Connecting,
            MediaPeerState::Connected,
            MediaPeerState::Failed,
            MediaPeerState::Unknown,
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, rx) = open(&mut broker, pane(7), now);
            let (_, session_id, _) = opened_session(&sent);
            send_offer(&mut broker, &session_id, now);
            let _ = response(&rx);
            peer_state(
                &mut broker,
                &session_id,
                state,
                now + Duration::from_secs(1),
            );
            assert_eq!(
                broker.state(&session_id).unwrap().state,
                MediaSessionState::Offered
            );
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
            let (sent, rx) = open(&mut broker, pane(7), later);
            assert!(
                sent.is_empty(),
                "no renewal without a real answer: {state:?}"
            );
            assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
            let sent = run(broker
                .answer(&session_id, "v=0 answer".into(), later)
                .unwrap());
            assert!(matches!(&sent[..], [(1, MediaControl::Answer(_))]));
            assert_eq!(
                broker.state(&session_id).unwrap().state,
                MediaSessionState::Connecting
            );
            assert!(run(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true)).is_empty());
            peer_state(
                &mut broker,
                &session_id,
                MediaPeerState::Connected,
                now + MEDIA_OPEN_TIMEOUT,
            );
            assert_eq!(
                broker.state(&session_id).unwrap().state,
                MediaSessionState::Connected
            );
        }
    }

    #[test]
    fn peer_states_and_duplicate_offers_cannot_extend_the_twenty_second_answer_deadline() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);
        send_offer(&mut broker, &session_id, now);
        let _ = response(&rx);
        let before = now + MEDIA_ANSWER_TIMEOUT - Duration::from_nanos(1);
        peer_state(&mut broker, &session_id, MediaPeerState::Connected, before);
        assert!(send_offer(&mut broker, &session_id, before).is_empty());
        assert!(run(broker.expire(before, |_| true)).is_empty());
        let sent = run(broker.expire(now + MEDIA_ANSWER_TIMEOUT, |_| true));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == session_id && close.code.as_deref() == Some(close_code::UNANSWERED)));
        assert!(rx.try_recv().is_err(), "no second offer response");
    }

    #[test]
    fn a_late_answer_is_rejected_before_the_timer_without_erasing_its_deadline() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);
        send_offer(&mut broker, &session_id, now);
        let _ = response(&rx);
        let deadline = now + MEDIA_ANSWER_TIMEOUT;
        let err = broker
            .answer(&session_id, "v=0 late".into(), deadline)
            .unwrap_err();
        assert_eq!(err.0, error_code::REFUSED);
        assert!(err.1.starts_with("unanswered:"));
        assert_eq!(
            broker.sessions[&session_id].offered_deadline,
            Some(deadline)
        );
        let sent = run(broker.expire(deadline, |_| true));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.code.as_deref() == Some(close_code::UNANSWERED)));
    }

    #[test]
    fn abandoned_or_late_renewal_offers_do_not_replace_the_source_call() {
        for cause in ["signal", "disconnect", "timeout"] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let source = live_call(&mut broker, 1, pane(7), now);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
            let (sent, rx, delivery) = tracked_open(&mut broker, "renewal", later);
            let (_, renewal, _) = opened_session(&sent);
            let offer_at = if cause == "timeout" {
                later + MEDIA_OPEN_TIMEOUT
            } else {
                later
            };
            let code = if cause == "timeout" {
                close_code::TIMEOUT
            } else {
                close_code::ABANDONED
            };
            if cause == "signal" {
                delivery.send(()).unwrap();
            }
            // Dropping the delivery sender while still pending cannot mean success.
            if cause == "disconnect" {
                drop(delivery);
            }
            let sent = send_offer(&mut broker, &renewal, offer_at);
            assert!(
                matches!(&sent[..], [(1, MediaControl::Close(close))]
                if close.session_id == renewal && close.code.as_deref() == Some(code)),
                "{cause}: {sent:?}"
            );
            assert!(response(&rx).get("error").is_some());
            assert_eq!(
                broker.state(&source).unwrap().state,
                MediaSessionState::Connected
            );
            assert!(send_offer(&mut broker, &renewal, offer_at).is_empty());
        }
    }

    #[test]
    fn abandonment_observed_by_answer_is_not_lost_before_timer_cleanup() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx, delivery) = tracked_open(&mut broker, "abandoned-answer", now);
        let (_, session_id, _) = opened_session(&sent);
        send_offer(&mut broker, &session_id, now);
        let _ = response(&rx);
        delivery.send(()).unwrap();
        drop(delivery);
        let err = broker
            .answer(&session_id, "v=0 answer".into(), now)
            .unwrap_err();
        assert!(err.1.starts_with("abandoned:"));
        let sent = run(broker.expire(now, |_| true));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.code.as_deref() == Some(close_code::ABANDONED)));
    }

    #[test]
    fn successful_offer_delivery_does_not_make_the_finished_api_socket_own_the_call() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx, delivery) = tracked_open(&mut broker, "delivered", now);
        let (_, session_id, _) = opened_session(&sent);
        send_offer(&mut broker, &session_id, now);
        let _ = response(&rx);
        // The socket worker drops this sender after a successful write. That is not cancellation.
        drop(delivery);
        drop(rx);
        run(broker
            .answer(&session_id, "v=0 answer".into(), now)
            .unwrap());
        peer_state(&mut broker, &session_id, MediaPeerState::Connected, now);
        assert!(run(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true)).is_empty());
        assert_eq!(
            broker.state(&session_id).unwrap().state,
            MediaSessionState::Connected
        );
        let (sent, _rx) = open(&mut broker, pane(7), now + MEDIA_OPEN_TIMEOUT);
        assert_eq!(
            opened_session(&sent).0,
            1,
            "the delivered call can still renew"
        );
    }

    #[test]
    fn a_request_abandoned_before_dispatch_never_sends_an_open_or_replaces_a_call() {
        for signalled in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let source = live_call(&mut broker, 1, pane(7), now);
            let (delivery_tx, delivery_rx) = mpsc::channel();
            if signalled {
                delivery_tx.send(()).unwrap();
            }
            drop(delivery_tx);
            let (tx, rx) = mpsc::channel();
            let sent = run(broker.open(
                "gone".into(),
                tx,
                pane(7),
                Some("gone".into()),
                Some(delivery_rx),
                |_| true,
                now,
            ));
            assert!(sent.is_empty());
            assert!(response(&rx)["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("abandoned:"));
            assert_eq!(broker.pane_epochs[&pane(7)], 1);
            assert_eq!(
                broker.state(&source).unwrap().state,
                MediaSessionState::Connected
            );
        }
    }

    #[test]
    fn caller_close_settles_authorized_successors_before_or_after_source_retirement() {
        for retired in [false, true] {
            for by_open_id in [false, true] {
                let now = Instant::now();
                let mut broker = MediaBroker::new();
                broker.client_connected(1, true);
                broker.note_pane_input(1, pane(7), "p_7", now);
                let (sent, rx) = open_with_id(&mut broker, pane(7), Some("source"), now);
                let (_, source, _) = opened_session(&sent);
                send_offer(&mut broker, &source, now);
                let _ = response(&rx);
                run(broker.answer(&source, "v=0 answer".into(), now).unwrap());
                peer_state(&mut broker, &source, MediaPeerState::Connected, now);
                let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
                let (sent, rx) = open_with_id(&mut broker, pane(7), Some("successor"), later);
                let (_, successor, _) = opened_session(&sent);
                if retired {
                    // The real client closes the source before producing its successor's offer.
                    run(broker.client_control(
                        1,
                        MediaControl::Close(MediaClose::new(
                            &source,
                            close_code::REPLACED,
                            "renewing",
                        )),
                        later,
                    ));
                    broker.closed.clear();
                    let sent = run(broker.mute(&source, true).unwrap());
                    assert!(matches!(&sent[..], [(1, MediaControl::Mute(mute))]
                        if mute.session_id == successor && mute.muted));
                }
                let sent = run(if by_open_id {
                    broker.close_by_open_id("source", later).unwrap()
                } else {
                    broker.close(&source, later)
                });
                assert_eq!(sent.len(), if retired { 1 } else { 2 });
                assert!(sent.iter().all(|(client, control)| *client == 1
                    && matches!(control, MediaControl::Close(close)
                        if close.code.as_deref() == Some(close_code::CLOSED)
                            && close.message.as_deref() == Some("the caller ended the session"))));
                assert!(sent
                    .iter()
                    .any(|(_, control)| matches!(control, MediaControl::Close(close)
                    if close.session_id == successor)));
                let refused = response(&rx);
                assert_eq!(refused["error"]["code"], error_code::REFUSED);
                assert_eq!(
                    refused["error"]["message"],
                    "closed: the caller ended the session"
                );
                assert!(!broker.has_sessions());
                assert!(
                    broker.cancelled_opens.is_empty(),
                    "an authorized source close is not unknown"
                );
                assert!(send_offer(&mut broker, &successor, later).is_empty());
                broker.note_pane_input(1, pane(7), "p_7", later);
                let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("source"), later);
                let (_, reused, _) = opened_session(&sent);
                assert_ne!(reused, source);
                assert_eq!(broker.state(&reused).unwrap().epoch, 3);
            }
        }
    }

    #[test]
    fn completed_handover_consumes_old_cancellation_authority_before_next_renewal() {
        for by_open_id in [false, true] {
            for close_source in [false, true] {
                let now = Instant::now();
                let mut broker = MediaBroker::new();
                broker.client_connected(1, true);
                broker.note_pane_input(1, pane(7), "p_7", now);
                let (sent, rx) = open_with_id(&mut broker, pane(7), Some("source"), now);
                let (_, source, _) = opened_session(&sent);
                send_offer(&mut broker, &source, now);
                let _ = response(&rx);
                run(broker.answer(&source, "v=0 answer".into(), now).unwrap());
                peer_state(&mut broker, &source, MediaPeerState::Connected, now);

                let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
                let (sent, rx) = open_with_id(&mut broker, pane(7), Some("connected"), later);
                let (_, connected, _) = opened_session(&sent);
                send_offer(&mut broker, &connected, later);
                let _ = response(&rx);
                run(broker
                    .answer(&connected, "v=0 answer".into(), later)
                    .unwrap());
                peer_state(&mut broker, &connected, MediaPeerState::Connected, later);
                let (sent, rx) = open_with_id(&mut broker, pane(7), Some("pending"), later);
                let (_, pending, _) = opened_session(&sent);
                let sent = run(if by_open_id {
                    broker.close_by_open_id("source", later).unwrap()
                } else {
                    broker.close(&source, later)
                });
                assert!(
                    sent.is_empty(),
                    "the completed handover consumed the old alias"
                );
                assert!(rx.try_recv().is_err(), "the new open must remain pending");
                assert_eq!(
                    broker.state(&connected).unwrap().state,
                    MediaSessionState::Connected
                );
                let (session_id, open_id) = if close_source {
                    (&connected, "connected")
                } else {
                    (&pending, "pending")
                };
                run(if by_open_id {
                    broker.close_by_open_id(open_id, later).unwrap()
                } else {
                    broker.close(session_id, later)
                });
                let refused = response(&rx);
                assert_eq!(refused["error"]["code"], error_code::REFUSED);
                assert_eq!(
                    refused["error"]["message"],
                    "closed: the caller ended the session"
                );
                assert_eq!(
                    broker.state(&connected).unwrap().state,
                    if close_source {
                        MediaSessionState::Closed
                    } else {
                        MediaSessionState::Connected
                    },
                    "ending only the pending renewal must preserve its connected source"
                );
                assert!(send_offer(&mut broker, &pending, later).is_empty());
            }
        }
    }

    #[test]
    fn a_retired_unrelated_open_id_does_not_cancel_another_pending_renewal() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, first_rx) = open_with_id(&mut broker, pane(7), Some("first-renewal"), later);
        let (_, first, _) = opened_session(&sent);
        let (sent, second_rx) = open_with_id(&mut broker, pane(7), Some("second-renewal"), later);
        let (_, second, _) = opened_session(&sent);
        assert!(broker.sessions[&second]
            .replaces
            .iter()
            .any(|old| old.session_id == first && !old.authorizes_renewal));
        run(broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(
                &first,
                close_code::REPLACED,
                "superseded attempt",
            )),
            later,
        ));
        assert_eq!(response(&first_rx)["error"]["code"], error_code::REFUSED);
        broker.closed.clear();
        assert!(broker.close(&first, later).is_empty());
        assert!(matches!(
            broker.mute(&first, true),
            Err((error_code::SESSION_NOT_FOUND, _))
        ));
        assert!(broker
            .close_by_open_id("first-renewal", later)
            .unwrap()
            .is_empty());
        assert_eq!(broker.cancelled_opens.len(), 1);
        assert_eq!(broker.cancelled_opens.front().unwrap().1, "first-renewal");
        assert_eq!(broker.sessions.len(), 2);
        assert_eq!(
            broker.state(&source).unwrap().state,
            MediaSessionState::Connected
        );
        assert_eq!(
            broker.state(&second).unwrap().state,
            MediaSessionState::Opening
        );
        assert_eq!(broker.pane_epochs[&pane(7)], 3);
        assert!(matches!(
            second_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let (sent, refused_rx) = open_with_id(&mut broker, pane(7), Some("first-renewal"), later);
        assert!(sent.is_empty());
        assert_eq!(
            response(&refused_rx)["error"]["message"],
            "closed: cancelled before open"
        );
        let sent = send_offer(&mut broker, &second, later);
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == source && close.code.as_deref() == Some(close_code::REPLACED)));
        assert_eq!(response(&second_rx)["result"]["epoch"], 3);
    }

    #[test]
    fn cancelling_only_a_pending_renewal_leaves_its_source_connected() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        run(broker.mute(&source, true).unwrap());
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, rx) = open_with_id(&mut broker, pane(7), Some("cancel-renewal"), later);
        let (_, renewal, _) = opened_session(&sent);
        assert!(sent.iter().any(|(_, control)| matches!(control, MediaControl::Mute(mute) if mute.session_id == renewal && mute.muted)));
        let sent = run(broker.close_by_open_id("cancel-renewal", later).unwrap());
        assert!(
            matches!(&sent[..], [(1, MediaControl::Close(close))] if close.session_id == renewal)
        );
        assert_eq!(response(&rx)["error"]["code"], error_code::REFUSED);
        assert_eq!(
            broker.state(&source).unwrap().state,
            MediaSessionState::Connected
        );
    }

    #[test]
    fn next_deadline_schedules_open_answer_and_retention_but_not_idle_owners_or_epochs() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        assert_eq!(broker.next_deadline(now), None);
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        assert_eq!(
            broker.next_deadline(now),
            None,
            "owners alone need no polling"
        );
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);
        assert_eq!(broker.next_deadline(now), Some(now + MEDIA_OPEN_TIMEOUT));
        let offered_at = now + Duration::from_secs(2);
        send_offer(&mut broker, &session_id, offered_at);
        let _ = response(&rx);
        assert_eq!(
            broker.next_deadline(offered_at),
            Some(offered_at + MEDIA_ANSWER_TIMEOUT)
        );
        run(broker
            .answer(&session_id, "v=0 answer".into(), offered_at)
            .unwrap());
        assert_eq!(
            broker.next_deadline(offered_at),
            None,
            "an answered call has no media timer"
        );
        run(broker.close(&session_id, offered_at));
        assert_eq!(
            broker.next_deadline(offered_at),
            Some(offered_at + CLOSED_SESSION_RETENTION)
        );
        run(broker.expire(offered_at + CLOSED_SESSION_RETENTION, |_| true));
        assert_eq!(
            broker.next_deadline(offered_at + CLOSED_SESSION_RETENTION),
            None
        );
        run(broker.client_removed(1, offered_at + CLOSED_SESSION_RETENTION));
        assert!(broker.pane_owners.is_empty());
        assert!(!broker.pane_epochs.is_empty());
        assert_eq!(
            broker.next_deadline(offered_at + CLOSED_SESSION_RETENTION),
            None,
            "epochs alone need no polling"
        );
    }

    #[test]
    fn delivery_receipts_get_bounded_wakeups_only_until_success_or_abandonment() {
        for delivered in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, rx, delivery) = tracked_open(&mut broker, "tracked", now);
            let (_, session_id, _) = opened_session(&sent);
            assert_eq!(
                broker.next_deadline(now),
                Some(now + RESPONSE_DELIVERY_POLL_INTERVAL)
            );
            send_offer(&mut broker, &session_id, now);
            let _ = response(&rx);
            if !delivered {
                delivery.send(()).unwrap();
            }
            drop(delivery);
            let wake = broker.next_deadline(now).unwrap();
            assert_eq!(wake, now + RESPONSE_DELIVERY_POLL_INTERVAL);
            let sent = run(broker.expire(wake, |_| true));
            if delivered {
                assert!(sent.is_empty());
                assert_eq!(broker.next_deadline(wake), Some(now + MEDIA_ANSWER_TIMEOUT));
                run(broker
                    .answer(&session_id, "v=0 answer".into(), wake)
                    .unwrap());
                assert_eq!(broker.next_deadline(wake), None);
            } else {
                assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
                    if close.code.as_deref() == Some(close_code::ABANDONED)));
                assert_eq!(
                    broker.next_deadline(wake),
                    Some(wake + CLOSED_SESSION_RETENTION)
                );
            }
        }
    }

    #[test]
    fn next_deadline_chooses_the_earliest_session_deadline() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        broker.note_pane_input(2, pane(8), "p_8", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, offered, _) = opened_session(&sent);
        send_offer(&mut broker, &offered, now);
        let _ = response(&rx);
        let (_sent, _rx) = open(&mut broker, pane(8), now);
        assert_eq!(broker.next_deadline(now), Some(now + MEDIA_ANSWER_TIMEOUT));
        run(broker.expire(now + MEDIA_ANSWER_TIMEOUT, |_| true));
        assert_eq!(
            broker.next_deadline(now + MEDIA_ANSWER_TIMEOUT),
            Some(now + MEDIA_OPEN_TIMEOUT)
        );
    }

    #[test]
    fn cancelling_an_older_pending_renewal_does_not_cancel_a_newer_independent_attempt() {
        for abandoned in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let source = live_call(&mut broker, 1, pane(7), now);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
            let (sent, first_rx, delivery) = tracked_open(&mut broker, "first-renewal", later);
            let (_, first, _) = opened_session(&sent);
            let (sent, second_rx) =
                open_with_id(&mut broker, pane(7), Some("second-renewal"), later);
            let (_, second, _) = opened_session(&sent);
            let sent = if abandoned {
                delivery.send(()).unwrap();
                run(broker.expire(later, |_| true))
            } else {
                run(broker.close_by_open_id("first-renewal", later).unwrap())
            };
            assert!(
                matches!(&sent[..], [(1, MediaControl::Close(close))] if close.session_id == first)
            );
            assert_eq!(response(&first_rx)["error"]["code"], error_code::REFUSED);
            assert_eq!(
                broker.state(&source).unwrap().state,
                MediaSessionState::Connected
            );
            assert_eq!(
                broker.state(&second).unwrap().state,
                MediaSessionState::Opening
            );
            let sent = send_offer(&mut broker, &second, later);
            assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
                if close.session_id == source && close.code.as_deref() == Some(close_code::REPLACED)));
            assert_eq!(response(&second_rx)["result"]["epoch"], 3);
        }
    }

    #[test]
    fn a_peer_state_at_the_answer_deadline_closes_before_timer_maintenance() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, session_id, _) = opened_session(&sent);
        send_offer(&mut broker, &session_id, now);
        let _ = response(&rx);
        let sent = peer_state(
            &mut broker,
            &session_id,
            MediaPeerState::Connected,
            now + MEDIA_ANSWER_TIMEOUT,
        );
        assert!(matches!(&sent[..], [(1, MediaControl::Close(close))]
            if close.session_id == session_id && close.code.as_deref() == Some(close_code::UNANSWERED)));
        assert!(!broker.has_sessions());
    }
}
