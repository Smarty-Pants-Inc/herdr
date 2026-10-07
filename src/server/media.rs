//! Server-side routing of client-local media sessions.
//!
//! `pane.media_open` binds a session to the attached client whose input last reached the
//! pane, because that is the machine the user is sitting at. The broker is pure state: the
//! headless server feeds it client, input and API events and performs the returned actions.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventData, EventEnvelope, EventKind, MediaEndedReceipt,
    MediaSessionState, MediaTeardownStuckEvent, ResponseResult, SuccessResponse,
};
use crate::layout::PaneId;
use crate::protocol::media::{
    close_code, valid_media_token, MediaClose, MediaControl, MediaEndOrigin, MediaMute, MediaOpen,
    MediaPeerState, MediaSdp, MEDIA_INPUT_WINDOW,
};

/// How long `pane.media_open` waits for the client's offer. It covers the one-time consent
/// prompt (30 s on the client) plus ICE gathering.
pub(crate) const MEDIA_OPEN_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a closed session's final state stays readable through `media.state`.
const CLOSED_SESSION_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_CLOSED_SESSIONS: usize = 32;
const RECEIPT_RETENTION: Duration = Duration::from_secs(10 * 60);
const MAX_RECEIPT_RECORDS_PER_ENDPOINT: usize = 64;

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
    Publish(Box<EventEnvelope>),
    Send {
        client_id: u64,
        control: MediaControl,
    },
    Respond {
        respond_to: Sender<String>,
        response: String,
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
    ended_receipt: bool,
    user: Option<String>,
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
    /// The pane id as its client knows it, so a renewal can reopen for that client (smarty-voice#133).
    pane_ref: String,
    /// A handover (smarty-voice#133): the sessions this one replaces once its client accepts it (its offer). Until
    /// then they stay live; if it is refused or times out they are untouched.
    replaces: Vec<String>,
    /// The mute the caller last asked for (herdr#102 security pass): a renewal inherits it, even before the client
    /// reported applying it, and a mute asked for during a handover also reaches the successor.
    wants_muted: bool,
    state: MediaSessionState,
    muted: bool,
    pending: Option<PendingOpen>,
    receipt: Option<ReceiptIdentity>,
}

#[derive(Debug)]
struct ReceiptIdentity {
    generation: String,
    attempt: String,
    server_close: Option<(MediaEndOrigin, Option<String>)>,
    stuck: bool,
}

#[derive(Debug)]
struct ReceiptRecord {
    session: MediaSession,
    view: MediaSessionView,
    ended_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaSessionView {
    pub(crate) session_id: String,
    pub(crate) state: MediaSessionState,
    pub(crate) muted: bool,
    pub(crate) code: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) ended: Option<MediaEndedReceipt>,
}

impl MediaSessionView {
    pub(crate) fn into_result(self) -> ResponseResult {
        ResponseResult::MediaSession {
            session_id: self.session_id,
            state: self.state,
            muted: self.muted,
            code: self.code,
            message: self.message,
            ended: self.ended,
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
    /// Attached client-shell clients and whether each advertised media support.
    clients: HashMap<u64, MediaClient>,
    /// Per pane, the client whose input reached it last. Input to another pane never moves
    /// this owner, so it cannot reroute a pane's microphone to an older client.
    pane_owners: HashMap<PaneId, PaneOwner>,
    next_owner_cleanup: Option<Instant>,
    sessions: HashMap<String, MediaSession>,
    closed: VecDeque<ClosedSession>,
    receipts: VecDeque<ReceiptRecord>,
    id_prefix: String,
    next_id: u64,
    /// Rejected teardown evidence per (client, bounded reason), to cap warn volume.
    rejections: HashMap<(u64, &'static str), u64>,
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
            sessions: HashMap::new(),
            closed: VecDeque::new(),
            receipts: VecDeque::new(),
            id_prefix: format!("media_{boot:x}_"),
            next_id: 1,
            rejections: HashMap::new(),
        }
    }

    pub(crate) fn client_connected(&mut self, client_id: u64, capable: bool) {
        self.clients.insert(
            client_id,
            MediaClient {
                capable,
                ended_receipt: false,
                user: None,
            },
        );
    }

    pub(crate) fn set_client_ended_receipt(&mut self, client_id: u64, capable: bool) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.ended_receipt = capable;
        }
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
        !self.pane_owners.is_empty()
            && self
                .next_owner_cleanup
                .is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn retain_live_panes(&mut self, now: Instant, pane_exists: impl Fn(PaneId) -> bool) {
        self.pane_owners.retain(|pane, _| pane_exists(*pane));
        self.next_owner_cleanup = Some(now + Duration::from_secs(1));
    }

    pub(crate) fn client_removed(&mut self, client_id: u64, now: Instant) -> Vec<MediaAction> {
        self.clients.remove(&client_id);
        self.rejections
            .retain(|(client, _), _| *client != client_id);
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
        self.receipts
            .retain(|record| record.session.client_id != client_id);
        actions
    }

    /// Bind a new session to the client whose input last reached `pane`.
    ///
    /// `views` reports whether a client currently shows the pane.
    #[cfg(test)]
    pub(crate) fn open(
        &mut self,
        request_id: String,
        respond_to: Sender<String>,
        pane: PaneId,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        self.open_with_receipt(request_id, respond_to, pane, None, None, views, now)
    }

    fn binding(
        &self,
        pane: PaneId,
        views: &impl Fn(u64) -> bool,
        now: Instant,
    ) -> Option<(u64, bool, PaneOwner, bool)> {
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
        latest.map(|(client, capable, owner)| (client, capable, owner, handover))
    }

    pub(crate) fn preflight(
        &self,
        pane: PaneId,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> ResponseResult {
        let binding = self.binding(pane, &views, now);
        let client = binding.as_ref().map(|entry| entry.0);
        let webrtc = binding.as_ref().is_some_and(|entry| entry.1);
        let ended_receipt = client
            .and_then(|id| self.clients.get(&id))
            .is_some_and(|entry| entry.ended_receipt);
        // Same priority as a receipt-bearing open, the caller this preflight serves.
        let refusal = match client {
            None => Some(close_code::STALE_INPUT.to_owned()),
            Some(_) if !ended_receipt => Some(close_code::RECEIPT_UNSUPPORTED.to_owned()),
            Some(_) if !webrtc => Some(close_code::UNSUPPORTED.to_owned()),
            Some(id) if !views(id) => Some(close_code::NOT_VIEWED.to_owned()),
            Some(_) => None,
        };
        ResponseResult::MediaPreflight {
            bound: client.is_some_and(views),
            client,
            webrtc,
            ended_receipt,
            refusal,
        }
    }

    pub(crate) fn open_with_receipt(
        &mut self,
        request_id: String,
        respond_to: Sender<String>,
        pane: PaneId,
        generation: Option<String>,
        attempt: Option<String>,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        let identity_valid = match (&generation, &attempt) {
            (None, None) => true,
            (Some(generation), Some(attempt)) => {
                valid_media_token(generation) && valid_media_token(attempt)
            }
            _ => false,
        };
        if !identity_valid {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    "invalid_params",
                    "generation and attempt must be a valid token pair",
                ),
            });
            return actions;
        }
        let Some((client_id, capable, input, handover)) = self.binding(pane, &views, now) else {
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
        // A receipt-bearing open must be refused with the distinct positively-never-opened
        // code before any other client refusal, even when WebRTC is missing too.
        if generation.is_some()
            && !self
                .clients
                .get(&client_id)
                .is_some_and(|client| client.ended_receipt)
        {
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response(
                    request_id,
                    error_code::REFUSED,
                    close_code::RECEIPT_UNSUPPORTED,
                ),
            });
            return actions;
        }
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
        // A handover keeps the live call until the client accepts the new session (client_control, Offer): closing it
        // first would leave the client with no call to renew, and a refusal would end it (herdr#102 review).
        let replaces = if handover {
            replaced
        } else {
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
            Vec::new()
        };

        // A renewal keeps the call's mute: muted if the caller asked for it or the client reported it (fail closed).
        let inherited_mute = replaces.iter().any(|id| {
            self.sessions
                .get(id)
                .is_some_and(|old| old.wants_muted || old.muted)
        });
        let session_id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        self.sessions.insert(
            session_id.clone(),
            MediaSession {
                client_id,
                pane,
                pane_ref: input.pane_ref.clone(),
                replaces,
                wants_muted: inherited_mute,
                state: MediaSessionState::Opening,
                muted: inherited_mute,
                receipt: generation
                    .clone()
                    .zip(attempt.clone())
                    .map(|(generation, attempt)| ReceiptIdentity {
                        generation,
                        attempt,
                        server_close: None,
                        stuck: false,
                    }),
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
                session_id: session_id.clone(),
                pane_id: input.pane_ref,
                generation,
                attempt,
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
        if let Err(error) = control.validate() {
            tracing::warn!(client_id, %error, "invalid media control at broker boundary");
            return actions;
        }
        if matches!(
            control,
            MediaControl::Ended(_) | MediaControl::TeardownStuck(_)
        ) {
            return self.completion(client_id, control, now);
        }
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
                // The client accepted a handover: only now is the call it renews replaced.
                let successor_client = session.client_id;
                let successor_pane = session.pane;
                for id in std::mem::take(&mut session.replaces) {
                    // A legacy client Close may already have moved the predecessor to
                    // receipts while its actual teardown is still running. Preserve the
                    // authenticated replacement proof before retiring this link; finish
                    // cannot update a record that is no longer live. Never infer origin
                    // from the client's close code or overwrite an earlier server close.
                    if let Some(record) = self.receipts.iter_mut().find(|record| {
                        record.view.session_id == id
                            && record.view.ended.is_none()
                            && record.session.client_id == successor_client
                            && record.session.pane == successor_pane
                    }) {
                        if let Some(identity) = record.session.receipt.as_mut() {
                            identity
                                .server_close
                                .get_or_insert((MediaEndOrigin::Replaced, None));
                        }
                    }
                    self.finish(
                        &id,
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
            MediaControl::Open(_)
            | MediaControl::Answer(_)
            | MediaControl::Mute(_)
            | MediaControl::Ended(_)
            | MediaControl::TeardownStuck(_) => {}
        }
        actions
    }

    /// Validate cooperative teardown evidence against the authenticated endpoint and
    /// immutable open identity. A close admission or diagnostic is never completion.
    fn completion(
        &mut self,
        client_id: u64,
        control: MediaControl,
        now: Instant,
    ) -> Vec<MediaAction> {
        let id = control.session_id().to_owned();
        // Never log the untrusted id itself, only its length and a bounded reason.
        // ponytail: per (client, reason), log the first 8 and then each power of two with
        // the running count, so a flooding endpoint costs O(log n) lines.
        let rejections = &mut self.rejections;
        let mut reject = |reason: &'static str| {
            let count = rejections.entry((client_id, reason)).or_default();
            *count += 1;
            if *count <= 8 || count.is_power_of_two() {
                tracing::warn!(
                    client_id,
                    reason,
                    rejected = *count,
                    session_id_len = id.len(),
                    "dropped media teardown evidence"
                );
            }
            Vec::new()
        };
        let (generation, attempt) = match &control {
            MediaControl::Ended(ended) => (&ended.generation, &ended.attempt),
            MediaControl::TeardownStuck(stuck) => (&stuck.generation, &stuck.attempt),
            _ => return Vec::new(),
        };
        let session = self.sessions.get(&id).or_else(|| {
            self.receipts
                .iter()
                .find(|record| record.view.session_id == id && record.view.ended.is_none())
                .map(|record| &record.session)
        });
        let Some(session) = session else {
            // Only an exact replay of a retained completed receipt (same endpoint,
            // identity, origin and request) is quiet and idempotent. Anything else is
            // unknown/expired/evicted or contradictory evidence.
            let Some(recorded) = self
                .receipts
                .iter()
                .find(|record| record.view.session_id == id)
                .and_then(|record| record.view.ended.as_ref())
            else {
                return reject("unknown_session");
            };
            if recorded.client != client_id {
                return reject("wrong_client");
            }
            if &recorded.generation != generation || &recorded.attempt != attempt {
                return reject("wrong_identity");
            }
            return match &control {
                MediaControl::Ended(ended)
                    if ended.origin == recorded.origin
                        && ended.request_id == recorded.request_id
                        && ended.acquired == recorded.acquired =>
                {
                    Vec::new()
                }
                MediaControl::Ended(_) => reject("replay_mismatch"),
                _ => reject("already_completed"),
            };
        };
        let Some(identity) = session.receipt.as_ref() else {
            return reject("no_receipt_identity");
        };
        if session.client_id != client_id {
            return reject("wrong_client");
        }
        if &identity.generation != generation || &identity.attempt != attempt {
            return reject("wrong_identity");
        }
        match control {
            MediaControl::TeardownStuck(_) => {
                if identity.stuck {
                    return Vec::new();
                }
                let diagnostic = MediaTeardownStuckEvent {
                    session_id: id.clone(),
                    pane_id: session.pane_ref.clone(),
                    client: session.client_id,
                    generation: identity.generation.clone(),
                    attempt: identity.attempt.clone(),
                };
                let session = self.sessions.get_mut(&id).or_else(|| {
                    self.receipts
                        .iter_mut()
                        .find(|record| record.view.session_id == id)
                        .map(|record| &mut record.session)
                });
                if let Some(identity) = session.and_then(|session| session.receipt.as_mut()) {
                    identity.stuck = true;
                }
                vec![MediaAction::Publish(Box::new(EventEnvelope {
                    event: EventKind::MediaTeardownStuck,
                    data: EventData::MediaTeardownStuck(diagnostic),
                }))]
            }
            MediaControl::Ended(ended) => {
                let valid_origin = match &identity.server_close {
                    Some((origin, request_id)) => {
                        *origin == ended.origin && *request_id == ended.request_id
                    }
                    None => {
                        ended.request_id.is_none()
                            && (ended.origin == MediaEndOrigin::Natural
                                || (ended.origin == MediaEndOrigin::Replaced
                                    && self
                                        .sessions
                                        .values()
                                        .chain(self.receipts.iter().map(|record| &record.session))
                                        .any(|successor| {
                                            successor.client_id == client_id
                                                && successor.pane == session.pane
                                                && successor.replaces.contains(&id)
                                        })))
                    }
                };
                if !valid_origin {
                    return reject("uncorrelated_origin");
                }
                let receipt = MediaEndedReceipt {
                    session_id: id.clone(),
                    pane_id: session.pane_ref.clone(),
                    client: session.client_id,
                    generation: identity.generation.clone(),
                    attempt: identity.attempt.clone(),
                    origin: ended.origin,
                    request_id: ended.request_id,
                    acquired: ended.acquired,
                };
                let mut actions = Vec::new();
                // Client-first renewal completion must retain the successor and its
                // relation; later Offer/Close(replaced) is a no-op for this record.
                let code = if receipt.origin == MediaEndOrigin::Replaced {
                    close_code::REPLACED
                } else {
                    close_code::CLOSED
                };
                self.finish(
                    &id,
                    code,
                    "the client completed media teardown",
                    false,
                    now,
                    &mut actions,
                );
                if let Some(record) = self
                    .receipts
                    .iter_mut()
                    .find(|record| record.view.session_id == id)
                {
                    record.view.ended = Some(receipt.clone());
                    record.ended_at = Some(now);
                    actions.push(MediaAction::Publish(Box::new(EventEnvelope {
                        event: EventKind::MediaEnded,
                        data: EventData::MediaEnded(receipt),
                    })));
                }
                actions
            }
            _ => Vec::new(),
        }
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
        for (id, successor) in self
            .sessions
            .iter_mut()
            .filter(|(_, other)| other.replaces.iter().any(|old| old == session_id))
        {
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
                state: session.state,
                muted: session.muted,
                code: None,
                message: None,
                ended: None,
            });
        }
        if let Some(record) = self
            .receipts
            .iter()
            .find(|record| record.view.session_id == session_id)
        {
            return Some(record.view.clone());
        }
        self.closed
            .iter()
            .find(|closed| closed.view.session_id == session_id)
            .map(|closed| closed.view.clone())
    }

    /// End a session at the API caller's request. Unknown ids are a no-op.
    #[cfg(test)]
    pub(crate) fn close(&mut self, session_id: &str, now: Instant) -> Vec<MediaAction> {
        self.close_with_request(session_id, None, now)
    }

    pub(crate) fn close_with_request(
        &mut self,
        session_id: &str,
        request_id: Option<String>,
        now: Instant,
    ) -> Vec<MediaAction> {
        if request_id
            .as_deref()
            .is_some_and(|id| !valid_media_token(id))
        {
            return Vec::new();
        }
        if let Some(identity) = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.receipt.as_mut())
        {
            identity.server_close = Some((MediaEndOrigin::Requested, request_id));
        }
        let mut actions = Vec::new();
        // Its client may already have retired it for a renewal still being handed over: ending the call it knows
        // cancels that renewal too (herdr#102 security pass).
        if !self.sessions.contains_key(session_id) {
            for id in self.successors(session_id) {
                self.finish(
                    &id,
                    close_code::CLOSED,
                    "the call it renewed was ended",
                    true,
                    now,
                    &mut actions,
                );
            }
        }
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
        if self.sessions.is_empty() && self.closed.is_empty() && self.receipts.is_empty() {
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
        self.receipts.retain(|record| {
            record
                .ended_at
                .is_none_or(|at| now.saturating_duration_since(at) < RECEIPT_RETENTION)
        });
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

    /// Sessions still being handed over as renewals of `session_id` (it is in their `replaces`).
    fn successors(&self, session_id: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, other)| other.replaces.iter().any(|old| old == session_id))
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
        let Some(mut session) = self.sessions.remove(session_id) else {
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
            let mut close = MediaClose::new(session_id, code, message);
            if let Some(identity) = session.receipt.as_mut() {
                let origin = if code == close_code::REPLACED {
                    MediaEndOrigin::Replaced
                } else if code == close_code::TIMEOUT || code == close_code::PANE_CLOSED {
                    MediaEndOrigin::Cancelled
                } else {
                    MediaEndOrigin::Requested
                };
                let (origin, request_id) = identity.server_close.get_or_insert((origin, None));
                close.origin = Some(*origin);
                close.request_id = request_id.clone();
            }
            actions.push(MediaAction::Send {
                client_id: session.client_id,
                control: MediaControl::Close(close),
            });
        }
        if let Some(pending) = session.pending.take() {
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
        let view = MediaSessionView {
            session_id: session_id.to_owned(),
            state: MediaSessionState::Closed,
            muted: session.muted,
            code: Some(code.to_owned()),
            message: Some(message.to_owned()),
            ended: None,
        };
        if session.receipt.is_some() {
            if code != close_code::DISCONNECTED {
                let client_id = session.client_id;
                self.receipts.push_back(ReceiptRecord {
                    session,
                    view,
                    ended_at: None,
                });
                while self
                    .receipts
                    .iter()
                    .filter(|record| record.session.client_id == client_id)
                    .count()
                    > MAX_RECEIPT_RECORDS_PER_ENDPOINT
                {
                    if let Some(index) = self
                        .receipts
                        .iter()
                        .position(|record| record.session.client_id == client_id)
                    {
                        self.receipts.remove(index);
                    }
                }
            }
        } else {
            self.closed.push_back(ClosedSession {
                closed_at: now,
                view,
            });
        }
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
                MediaAction::Publish(_) => None,
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

    // smarty-voice#370: exercise the actual endpoint JSON trust boundary, not a
    // fabricated receipt type. Unknown kinds must not silently pass these tests.
    fn receipt_json(session_id: &str, origin: &str, request_id: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "session_id": session_id,
            "generation": "floor:370.1",
            "attempt": "attempt:370.1",
            "origin": origin,
            "acquired": true,
        });
        if let Some(request_id) = request_id {
            value["request_id"] = serde_json::json!(request_id);
        }
        value
    }

    fn decode_receipt(value: serde_json::Value) -> MediaControl {
        MediaControl::decode("media.ended.v1", &value.to_string())
            .expect("media.ended.v1 must be a recognized endpoint control")
            .expect("a bounded receipt must decode")
    }

    // These signatures are the planned production seams from .local/contract-370.md.
    // There are deliberately no fallback implementations of the new methods.
    fn receipt_broker(now: Instant) -> MediaBroker {
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.set_client_ended_receipt(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        broker
    }

    fn receipt_open(broker: &mut MediaBroker, now: Instant) -> (String, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.open_with_receipt(
            "open:370.1".into(),
            tx,
            pane(7),
            Some("floor:370.1".into()),
            Some("attempt:370.1".into()),
            |_| true,
            now,
        ));
        let (client, session_id, pane_ref) = opened_session(&sent);
        assert_eq!((client, pane_ref.as_str()), (1, "w1:p7"));
        let crate::protocol::ServerMessage::EndpointControl { data, .. } = sent
            .iter()
            .find(|(_, control)| matches!(control, MediaControl::Open(_)))
            .unwrap()
            .1
            .server_message()
            .unwrap()
        else {
            panic!("open must use the endpoint control envelope");
        };
        let payload: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(payload["generation"], "floor:370.1");
        assert_eq!(payload["attempt"], "attempt:370.1");
        (session_id, rx)
    }

    fn receipt_state(broker: &MediaBroker, session_id: &str) -> serde_json::Value {
        broker
            .state(session_id)
            .map_or(serde_json::Value::Null, |view| {
                serde_json::to_value(view.into_result()).unwrap()
            })
    }

    fn accept_receipt(
        broker: &mut MediaBroker,
        client: u64,
        value: serde_json::Value,
        now: Instant,
    ) -> Vec<MediaAction> {
        broker.client_control(client, decode_receipt(value), now)
    }

    fn published_media_events(actions: &[MediaAction]) -> usize {
        // Publication is a new broker action, distinct from transport Send and API
        // Respond. Avoid inventing its variant name before implementation is granted.
        actions
            .iter()
            .filter(|action| {
                !matches!(
                    action,
                    MediaAction::Send { .. } | MediaAction::Respond { .. }
                )
            })
            .count()
    }

    fn assert_receipt(
        broker: &MediaBroker,
        session_id: &str,
        origin: &str,
        request_id: Option<&str>,
    ) {
        let state = receipt_state(broker, session_id);
        let ended = &state["ended"];
        assert_eq!(ended["session_id"], session_id);
        assert_eq!(
            ended["pane_id"], "w1:p7",
            "binding comes from the server record"
        );
        assert_eq!(ended["client"], 1);
        assert_eq!(ended["generation"], "floor:370.1");
        assert_eq!(ended["attempt"], "attempt:370.1");
        assert_eq!(ended["origin"], origin);
        assert_eq!(ended["request_id"].as_str(), request_id);
        assert_eq!(ended["acquired"], true);
    }

    #[test]
    fn receipt_broker_wrong_endpoint_is_rejected_then_bound_endpoint_is_accepted() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        broker.client_connected(2, true);
        broker.set_client_ended_receipt(2, true);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        let good = receipt_json(&session_id, "natural", None);
        let before = receipt_state(&broker, &session_id);
        assert!(accept_receipt(&mut broker, 2, good.clone(), now).is_empty());
        assert_eq!(receipt_state(&broker, &session_id), before);
        assert_eq!(
            published_media_events(&accept_receipt(&mut broker, 1, good, now)),
            1,
            "accepted completion must publish exactly one event, not merely an API response"
        );
        assert_receipt(&broker, &session_id, "natural", None);
    }

    #[test]
    fn receipt_broker_wrong_generation_attempt_or_session_is_rejected() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        let good = receipt_json(&session_id, "natural", None);
        let before = receipt_state(&broker, &session_id);
        for (field, wrong) in [
            ("generation", "floor:370.old"),
            ("attempt", "attempt:370.old"),
            ("session_id", "media_nonexistent"),
        ] {
            let mut bad = good.clone();
            bad[field] = serde_json::json!(wrong);
            assert!(
                accept_receipt(&mut broker, 1, bad, now).is_empty(),
                "reject wrong {field}"
            );
            assert_eq!(receipt_state(&broker, &session_id), before);
        }
        accept_receipt(&mut broker, 1, good, now);
        assert_receipt(&broker, &session_id, "natural", None);
    }

    #[test]
    fn receipt_broker_replay_cannot_republish_or_change_retained_receipt() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        let good = receipt_json(&session_id, "natural", None);
        assert_eq!(
            published_media_events(&accept_receipt(&mut broker, 1, good.clone(), now)),
            1
        );
        assert_receipt(&broker, &session_id, "natural", None);
        let before = receipt_state(&broker, &session_id);
        assert!(accept_receipt(&mut broker, 1, good.clone(), now).is_empty());
        let mut contradictory_replay = good;
        contradictory_replay["acquired"] = serde_json::json!(false);
        assert!(accept_receipt(&mut broker, 1, contradictory_replay, now).is_empty());
        assert_eq!(receipt_state(&broker, &session_id), before);
    }

    #[test]
    fn receipt_broker_malformed_json_never_becomes_completion() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        let good = receipt_json(&session_id, "natural", None);
        let before = receipt_state(&broker, &session_id);
        for (field, bad) in [
            ("generation", serde_json::json!("a".repeat(129))),
            ("attempt", serde_json::json!("not an ASCII token")),
            ("origin", serde_json::json!("unknown")),
            ("acquired", serde_json::json!("false")),
        ] {
            let mut value = good.clone();
            value[field] = bad;
            let decoded = MediaControl::decode("media.ended.v1", &value.to_string())
                .expect("receipt control must be known, not silently ignored");
            if let Ok(control) = decoded {
                assert!(
                    broker.client_control(1, control, now).is_empty(),
                    "broker must reject malformed {field}"
                );
            }
            assert_eq!(
                receipt_state(&broker, &session_id),
                before,
                "no completion for malformed {field}"
            );
        }
        accept_receipt(&mut broker, 1, good, now);
        assert_receipt(&broker, &session_id, "natural", None);
    }

    #[test]
    fn receipt_broker_legacy_capability_refuses_before_send_or_session_creation() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "w1:p7", now);
        let next_id = broker.next_id;
        let (tx, rx) = mpsc::channel();
        let actions = broker.open_with_receipt(
            "open:370.1".into(),
            tx,
            pane(7),
            Some("floor:370.1".into()),
            Some("attempt:370.1".into()),
            |_| true,
            now,
        );
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, MediaAction::Send { .. })),
            "no open or consent prompt may reach the client"
        );
        run(actions);
        let body = response(&rx);
        assert!(
            body.get("error").is_some(),
            "must refuse synchronously, not wait for an offer"
        );
        assert!(body["error"].to_string().contains("receipt_unsupported"));
        assert!(!broker.has_sessions());
        assert!(broker.closed.is_empty());
        assert_eq!(
            broker.next_id, next_id,
            "refusal must not create even a hidden session"
        );
        let (sent, _rx) = open(&mut broker, pane(7), now);
        assert_eq!(
            opened_session(&sent).0,
            1,
            "legacy non-receipt opens stay supported"
        );
    }

    #[test]
    fn receipt_broker_preflight_is_advisory_and_touches_no_session_or_client() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, rx) = receipt_open(&mut broker, now);
        let before = format!("{broker:?}");
        let result = serde_json::to_value(broker.preflight(pane(7), |_| true, now)).unwrap();
        assert_eq!(result["bound"], true);
        assert_eq!(result["client"], 1);
        assert_eq!(result["webrtc"], true);
        assert_eq!(result["ended_receipt"], true);
        assert!(result["refusal"].is_null());
        assert_eq!(
            format!("{broker:?}"),
            before,
            "preflight cannot mutate ownership, ids, pending opens or sessions"
        );
        assert!(
            rx.try_recv().is_err(),
            "preflight cannot finish or replace a pending open"
        );
        assert!(receipt_state(&broker, &session_id)["ended"].is_null());
        let stale = serde_json::to_value(broker.preflight(
            pane(7),
            |_| true,
            now + MEDIA_INPUT_WINDOW + Duration::from_millis(1),
        ))
        .unwrap();
        assert_eq!(stale["bound"], false);
        assert!(!stale["refusal"].is_null());
        let hidden = serde_json::to_value(broker.preflight(pane(7), |_| false, now)).unwrap();
        assert_eq!(hidden["bound"], false);
        assert!(!hidden["refusal"].is_null());
        assert_eq!(format!("{broker:?}"), before);
        // An advisory success cannot bypass the authoritative receipt-capability gate.
        broker.set_client_ended_receipt(1, false);
        let legacy = serde_json::to_value(broker.preflight(pane(7), |_| true, now)).unwrap();
        assert_eq!(legacy["client"], 1);
        assert_eq!(legacy["webrtc"], true);
        assert_eq!(legacy["ended_receipt"], false);
    }

    #[test]
    fn receipt_broker_api_close_is_not_completion_and_late_receipt_is_requeryable() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        let sent = run(broker.close_with_request(&session_id, Some("close:370.1".into()), now));
        assert!(matches!(&sent[..], [(1, MediaControl::Close(_))]));
        let crate::protocol::ServerMessage::EndpointControl { data, .. } =
            sent[0].1.server_message().unwrap()
        else {
            panic!("close must use the endpoint control envelope");
        };
        let close: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(close["origin"], "requested");
        assert_eq!(close["request_id"], "close:370.1");
        assert!(
            receipt_state(&broker, &session_id)["ended"].is_null(),
            "API-close admission is not evidence of device teardown"
        );
        // Pass the legacy closed-cache lifetime while the teardown receipt is still held.
        let later = now + Duration::from_secs(6 * 60);
        run(broker.expire(later, |_| true));
        assert!(
            broker.state(&session_id).is_some(),
            "closing records cannot expire as legacy closed-cache entries"
        );
        assert_eq!(
            published_media_events(&accept_receipt(
                &mut broker,
                1,
                receipt_json(&session_id, "requested", Some("close:370.1")),
                later
            )),
            1
        );
        assert_receipt(&broker, &session_id, "requested", Some("close:370.1"));
        run(broker.expire(later + Duration::from_secs(10 * 60 - 1), |_| true));
        assert_receipt(&broker, &session_id, "requested", Some("close:370.1"));
        run(broker.expire(later + Duration::from_secs(10 * 60), |_| true));
        assert!(
            broker.state(&session_id).is_none(),
            "receipt expires ten minutes after receipt, not API close"
        );
    }

    #[test]
    fn receipt_broker_server_close_origin_and_request_must_match() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        run(broker.close_with_request(&session_id, Some("close:370.1".into()), now));
        let before = receipt_state(&broker, &session_id);
        for bad in [
            receipt_json(&session_id, "natural", None),
            receipt_json(&session_id, "cancelled", None),
            receipt_json(&session_id, "requested", Some("close:370.old")),
        ] {
            assert!(accept_receipt(&mut broker, 1, bad, now).is_empty());
            assert_eq!(receipt_state(&broker, &session_id), before);
        }
        accept_receipt(
            &mut broker,
            1,
            receipt_json(&session_id, "requested", Some("close:370.1")),
            now,
        );
        assert_receipt(&broker, &session_id, "requested", Some("close:370.1"));
    }

    #[test]
    fn receipt_broker_disconnect_makes_closing_and_completed_records_unknown() {
        for completed in [false, true] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let (session_id, _rx) = receipt_open(&mut broker, now);
            run(broker.close_with_request(&session_id, Some("close:370.1".into()), now));
            if completed {
                accept_receipt(
                    &mut broker,
                    1,
                    receipt_json(&session_id, "requested", Some("close:370.1")),
                    now,
                );
                assert_receipt(&broker, &session_id, "requested", Some("close:370.1"));
            }
            let actions = broker.client_removed(1, now);
            assert!(
                actions
                    .iter()
                    .all(|action| matches!(action, MediaAction::Respond { .. })),
                "disconnect must never publish ended"
            );
            assert!(
                broker.state(&session_id).is_none(),
                "endpoint loss invalidates requery evidence, completed={completed}"
            );
            broker.client_connected(1, true);
            broker.set_client_ended_receipt(1, true);
            assert!(
                accept_receipt(
                    &mut broker,
                    1,
                    receipt_json(&session_id, "requested", Some("close:370.1")),
                    now
                )
                .is_empty(),
                "reconnect cannot revive removed evidence"
            );
        }
    }

    #[test]
    fn receipt_broker_cap_is_64_per_endpoint_and_eviction_is_unknown() {
        for completed in [false, true] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            broker.client_connected(2, true);
            broker.set_client_ended_receipt(2, true);
            broker.note_pane_input(2, pane(8), "w1:p8", now);
            let (tx, _rx) = mpsc::channel();
            let sent = run(broker.open_with_receipt(
                "other-open".into(),
                tx,
                pane(8),
                Some("other-floor".into()),
                Some("other-attempt".into()),
                |_| true,
                now,
            ));
            let (_, other, _) = opened_session(&sent);
            run(broker.close_with_request(&other, Some("other-close".into()), now));
            let mut ids = Vec::new();
            for index in 0..65 {
                let at = now + Duration::from_millis(index);
                let (id, _rx) = receipt_open(&mut broker, at);
                run(broker.close_with_request(&id, Some("close:370.1".into()), at));
                if completed {
                    accept_receipt(
                        &mut broker,
                        1,
                        receipt_json(&id, "requested", Some("close:370.1")),
                        at,
                    );
                    assert_receipt(&broker, &id, "requested", Some("close:370.1"));
                }
                ids.push(id);
                if index == 63 {
                    assert!(
                        broker.state(&ids[0]).is_some(),
                        "all 64 retained records must survive, completed={completed}"
                    );
                }
            }
            assert!(
                broker.state(&ids[0]).is_none(),
                "oldest is unknown after the 65th record"
            );
            assert!(ids[1..].iter().all(|id| broker.state(id).is_some()));
            assert!(
                broker.state(&other).is_some(),
                "one endpoint's cap cannot evict another endpoint"
            );
            assert!(
                accept_receipt(
                    &mut broker,
                    1,
                    receipt_json(&ids[0], "requested", Some("close:370.1")),
                    now
                )
                .is_empty(),
                "evicted receipt is not resurrected"
            );
        }
    }

    #[test]
    fn receipt_broker_stuck_diagnostic_is_not_ended_and_later_receipt_is_accepted() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let (session_id, _rx) = receipt_open(&mut broker, now);
        run(broker.close_with_request(&session_id, Some("close:370.1".into()), now));
        let diagnostic = serde_json::json!({
            "session_id": session_id,
            "generation": "floor:370.1",
            "attempt": "attempt:370.1",
        });
        let decode = |value: &serde_json::Value| {
            MediaControl::decode("media.teardown_stuck.v1", &value.to_string())
                .expect("diagnostic kind must be known")
                .expect("valid diagnostic")
        };
        let before = receipt_state(&broker, &session_id);
        assert!(broker
            .client_control(2, decode(&diagnostic), now)
            .is_empty());
        let mut wrong = diagnostic.clone();
        wrong["generation"] = serde_json::json!("floor:370.old");
        assert!(broker.client_control(1, decode(&wrong), now).is_empty());
        assert_eq!(receipt_state(&broker, &session_id), before);
        assert_eq!(
            published_media_events(&broker.client_control(1, decode(&diagnostic), now)),
            1,
            "a valid stuck diagnostic must be published separately"
        );
        assert!(
            receipt_state(&broker, &session_id)["ended"].is_null(),
            "a diagnostic is never a completion receipt"
        );
        assert!(
            broker
                .client_control(1, decode(&diagnostic), now)
                .is_empty(),
            "at most one diagnostic"
        );
        let later = now + Duration::from_secs(1);
        assert_eq!(
            published_media_events(&accept_receipt(
                &mut broker,
                1,
                receipt_json(&session_id, "requested", Some("close:370.1")),
                later
            )),
            1
        );
        assert_receipt(&broker, &session_id, "requested", Some("close:370.1"));
    }

    fn receipt_connected_call(broker: &mut MediaBroker, now: Instant) -> String {
        let (session_id, rx) = receipt_open(broker, now);
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: session_id.clone(),
                sdp: "v=0 predecessor".into(),
            }),
            now,
        ));
        assert_eq!(response(&rx)["result"]["session_id"], session_id);
        run(broker.answer(&session_id, "v=0 answer".into()).unwrap());
        run(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: session_id.clone(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            }),
            now,
        ));
        assert_eq!(
            broker.state(&session_id).unwrap().state,
            MediaSessionState::Connected
        );
        session_id
    }

    fn receipt_renewal_actions(
        broker: &mut MediaBroker,
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> (Vec<MediaAction>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let actions = broker.open_with_receipt(
            "open:370.renewal".into(),
            tx,
            pane(7),
            Some("floor:370.1".into()),
            Some("attempt:370.2".into()),
            views,
            now,
        );
        (actions, rx)
    }

    #[test]
    fn receipt_broker_renewal_client_first_replaced_requires_bound_successor() {
        // No refreshed input: exercise the existing connected-call renewal fallback.
        for next in ["offer", "mute", "cancel"] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let old = receipt_connected_call(&mut broker, now);
            run(broker.mute(&old, true).unwrap());
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
            let (actions, rx) = receipt_renewal_actions(&mut broker, |_| true, later);
            assert_eq!(published_media_events(&actions), 0);
            let sent = run(actions);
            let (client, new, pane_ref) = opened_session(&sent);
            assert_eq!((client, pane_ref.as_str()), (1, "w1:p7"));
            assert_ne!(new, old);
            assert!(sent
                .iter()
                .all(|(_, control)| !matches!(control, MediaControl::Close(_))));
            assert!(sent.iter().any(|(client, control)| *client == 1
                && matches!(control, MediaControl::Mute(mute) if mute.session_id == new && mute.muted)));
            assert_eq!(
                broker.sessions[&new].client_id,
                broker.sessions[&old].client_id
            );
            assert_eq!(broker.sessions[&new].pane, broker.sessions[&old].pane);
            assert!(broker.sessions[&new].replaces.contains(&old));
            assert!(rx.try_recv().is_err());
            assert_eq!(
                broker.state(&old).unwrap().state,
                MediaSessionState::Connected
            );

            // Client starts the successor and tears down its predecessor BEFORE Offer.
            let ended = receipt_json(&old, "replaced", None);
            assert_eq!(
                published_media_events(&accept_receipt(&mut broker, 1, ended.clone(), later,)),
                1
            );
            assert_receipt(&broker, &old, "replaced", None);
            let retained = receipt_state(&broker, &old);
            assert!(
                broker.sessions.contains_key(&new),
                "replacement cannot cancel its successor"
            );
            assert!(
                rx.try_recv().is_err(),
                "predecessor receipt is not a successor offer"
            );

            match next {
                "offer" => {
                    let actions = broker.client_control(
                        1,
                        MediaControl::Offer(MediaSdp {
                            session_id: new.clone(),
                            sdp: "v=0 successor".into(),
                        }),
                        later,
                    );
                    assert_eq!(
                        published_media_events(&actions),
                        0,
                        "later server Close(replaced) cannot publish a second receipt"
                    );
                    run(actions);
                    assert_eq!(response(&rx)["result"]["session_id"], new);
                }
                "mute" => {
                    let sent = run(broker.mute(&old, true).unwrap());
                    assert!(sent.iter().any(|(client, control)| *client == 1
                        && matches!(control, MediaControl::Mute(mute) if mute.session_id == new && mute.muted)),
                        "mute on a retired predecessor still reaches its pending successor");
                    assert!(broker.sessions[&new].wants_muted);
                }
                "cancel" => {
                    let actions =
                        broker.close_with_request(&old, Some("close:370.renewal".into()), later);
                    assert_eq!(
                        published_media_events(&actions),
                        0,
                        "cancel admission is not successor completion"
                    );
                    let sent = run(actions);
                    assert!(sent.iter().any(|(client, control)| *client == 1
                        && matches!(control, MediaControl::Close(close) if close.session_id == new)),
                        "closing the retired predecessor must cancel its pending renewal");
                    assert_eq!(response(&rx)["error"]["code"], error_code::REFUSED);
                    assert!(receipt_state(&broker, &new)["ended"].is_null());
                }
                _ => unreachable!(),
            }
            assert_eq!(receipt_state(&broker, &old), retained, "first receipt wins");
            assert!(accept_receipt(&mut broker, 1, ended, later).is_empty());
        }
    }

    #[test]
    fn receipt_broker_late_replaced_receipt_after_client_close_and_successor_offer() {
        let now = Instant::now();
        let mut broker = receipt_broker(now);
        let old = receipt_connected_call(&mut broker, now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (actions, rx) = receipt_renewal_actions(&mut broker, |_| true, later);
        let sent = run(actions);
        let (_, successor, _) = opened_session(&sent);
        assert!(broker.sessions[&successor].replaces.contains(&old));

        // The client retires the predecessor first using the legacy Close control. The
        // completion receipt is deliberately held until after the successor's Offer.
        assert!(broker
            .client_control(
                1,
                MediaControl::Close(MediaClose::new(
                    &old,
                    close_code::REPLACED,
                    "successor started",
                )),
                later,
            )
            .is_empty());
        assert!(receipt_state(&broker, &old)["ended"].is_null());

        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: successor.clone(),
                sdp: "v=0 successor".into(),
            }),
            later,
        ));
        assert_eq!(response(&rx)["result"]["session_id"], successor);

        let receipt = receipt_json(&old, "replaced", None);
        assert!(accept_receipt(&mut broker, 2, receipt.clone(), later).is_empty());
        assert!(receipt_state(&broker, &old)["ended"].is_null());
        assert_eq!(
            published_media_events(&accept_receipt(&mut broker, 1, receipt.clone(), later)),
            1,
            "a delayed replaced receipt remains authenticated after Offer consumes the live link"
        );
        assert_receipt(&broker, &old, "replaced", None);
        assert!(accept_receipt(&mut broker, 1, receipt, later).is_empty());
        assert_receipt(&broker, &old, "replaced", None);
    }

    #[test]
    fn receipt_broker_late_replaced_proof_rejects_unbound_links_and_preserves_request() {
        for relation in ["no link", "wrong endpoint", "wrong pane", "requested"] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let old = receipt_connected_call(&mut broker, now);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
            let (actions, rx) = receipt_renewal_actions(&mut broker, |_| true, later);
            let (_, successor, _) = opened_session(&run(actions));
            // Seed a prior server request without cancelling this adversarial successor;
            // the proof-retirement path must never replace its correlation.
            if relation == "requested" {
                broker
                    .sessions
                    .get_mut(&old)
                    .unwrap()
                    .receipt
                    .as_mut()
                    .unwrap()
                    .server_close = Some((MediaEndOrigin::Requested, Some("close:370.1".into())));
            }
            run(broker.client_control(
                1,
                MediaControl::Close(MediaClose::new(&old, close_code::REPLACED, "retired")),
                later,
            ));
            let successor_record = broker.sessions.get_mut(&successor).unwrap();
            let offer_client = match relation {
                "no link" => {
                    successor_record.replaces.clear();
                    1
                }
                "wrong endpoint" => {
                    successor_record.client_id = 2;
                    2
                }
                "wrong pane" => {
                    successor_record.pane = pane(8);
                    1
                }
                "requested" => 1,
                _ => unreachable!(),
            };
            run(broker.client_control(
                offer_client,
                MediaControl::Offer(MediaSdp {
                    session_id: successor.clone(),
                    sdp: "v=0 successor".into(),
                }),
                later,
            ));
            assert_eq!(response(&rx)["result"]["session_id"], successor);
            let before = receipt_state(&broker, &old);
            assert!(
                accept_receipt(&mut broker, 1, receipt_json(&old, "replaced", None), later)
                    .is_empty(),
                "retired replacement proof must reject {relation}"
            );
            assert_eq!(receipt_state(&broker, &old), before);
            if relation == "requested" {
                assert_eq!(
                    published_media_events(&accept_receipt(
                        &mut broker,
                        1,
                        receipt_json(&old, "requested", Some("close:370.1")),
                        later,
                    )),
                    1
                );
                assert_receipt(&broker, &old, "requested", Some("close:370.1"));
            } else {
                assert_eq!(
                    published_media_events(&accept_receipt(
                        &mut broker,
                        1,
                        receipt_json(&old, "natural", None),
                        later,
                    )),
                    1
                );
                assert_receipt(&broker, &old, "natural", None);
            }
        }
    }

    #[test]
    fn receipt_broker_renewal_replaced_without_same_endpoint_same_pane_link_is_rejected() {
        for relation in ["no successor", "no link", "wrong endpoint", "wrong pane"] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let old = receipt_connected_call(&mut broker, now);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
            // Adversarial relation controls isolate each conjunct of the validation gate.
            let _pending = if relation == "no successor" {
                None
            } else {
                let (actions, rx) = receipt_renewal_actions(&mut broker, |_| true, later);
                let sent = run(actions);
                let (_, new, _) = opened_session(&sent);
                assert!(broker.sessions[&new].replaces.contains(&old));
                if relation == "wrong endpoint" {
                    broker.client_connected(2, true);
                    broker.set_client_ended_receipt(2, true);
                }
                let successor = broker.sessions.get_mut(&new).unwrap();
                match relation {
                    "no link" => successor.replaces.clear(),
                    "wrong endpoint" => successor.client_id = 2,
                    "wrong pane" => successor.pane = pane(8),
                    _ => unreachable!(),
                }
                Some(rx)
            };
            let before = receipt_state(&broker, &old);
            assert!(
                accept_receipt(&mut broker, 1, receipt_json(&old, "replaced", None), later)
                    .is_empty(),
                "client-first replaced must reject {relation}"
            );
            assert_eq!(receipt_state(&broker, &old), before);
            assert_eq!(
                broker.state(&old).unwrap().state,
                MediaSessionState::Connected
            );
            assert!(before["ended"].is_null());
            // With no qualifying replacement, a client-initiated end is natural.
            assert_eq!(
                published_media_events(&accept_receipt(
                    &mut broker,
                    1,
                    receipt_json(&old, "natural", None),
                    later,
                )),
                1
            );
            assert_receipt(&broker, &old, "natural", None);
        }
    }

    #[test]
    fn receipt_broker_renewal_refusal_keeps_connected_predecessor_alive_without_receipt() {
        for refusal in ["receipt unsupported", "not viewed", "client refused"] {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let old = receipt_connected_call(&mut broker, now);
            run(broker.mute(&old, true).unwrap());
            let before = receipt_state(&broker, &old);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
            if refusal == "receipt unsupported" {
                broker.set_client_ended_receipt(1, false);
            }
            let (actions, rx) =
                receipt_renewal_actions(&mut broker, |_| refusal != "not viewed", later);
            assert_eq!(published_media_events(&actions), 0);
            if refusal == "client refused" {
                let sent = run(actions);
                let (_, new, _) = opened_session(&sent);
                assert!(sent
                    .iter()
                    .all(|(_, control)| !matches!(control, MediaControl::Close(_))));
                let actions = broker.client_control(
                    1,
                    MediaControl::Close(MediaClose::new(
                        &new,
                        close_code::DECLINED,
                        "renewal refused before starting",
                    )),
                    later,
                );
                assert_eq!(
                    published_media_events(&actions),
                    0,
                    "a refusal control is not teardown evidence"
                );
                let sent = run(actions);
                assert!(sent.iter().all(|(_, control)|
                    !matches!(control, MediaControl::Close(close) if close.session_id == old)));
                assert_eq!(response(&rx)["error"]["code"], error_code::REFUSED);
                assert!(receipt_state(&broker, &new)["ended"].is_null());
            } else {
                assert!(
                    actions
                        .iter()
                        .all(|action| matches!(action, MediaAction::Respond { .. })),
                    "admission refusal sends no open, close or prompt"
                );
                run(actions);
                let error = response(&rx);
                if refusal == "receipt unsupported" {
                    assert!(error["error"].to_string().contains("receipt_unsupported"));
                } else {
                    assert_eq!(error["error"]["code"], error_code::NO_CLIENT);
                }
            }
            assert_eq!(
                receipt_state(&broker, &old),
                before,
                "refusal leaves predecessor unchanged"
            );
            assert_eq!(
                broker.state(&old).unwrap().state,
                MediaSessionState::Connected
            );
            assert!(
                broker.sessions[&old].wants_muted,
                "refusal cannot unmute the live call"
            );
            assert!(receipt_state(&broker, &old)["ended"].is_null());
        }
    }

    // SEC-370-01: receipt_unsupported outranks the generic WebRTC refusal.
    #[test]
    fn receipt_broker_no_capability_client_gets_receipt_unsupported() {
        let now = Instant::now();
        let mut observed = Vec::new();
        for after_preflight in [false, true] {
            let mut broker = receipt_broker(now);
            if after_preflight {
                let preflight =
                    serde_json::to_value(broker.preflight(pane(7), |_| true, now)).unwrap();
                assert_eq!(preflight["client"], 1);
                assert_eq!(preflight["ended_receipt"], true);
                assert!(preflight["refusal"].is_null());
            }
            broker.client_connected(2, false); // no WebRTC, no ended receipt
            broker.note_pane_input(2, pane(7), "w1:p7", now);
            // Advisory preflight now reports what the receipt-bearing open will get.
            let rebound = serde_json::to_value(broker.preflight(pane(7), |_| true, now)).unwrap();
            assert_eq!(rebound["client"], 2);
            assert_eq!(rebound["refusal"], close_code::RECEIPT_UNSUPPORTED);
            let before_next_id = broker.next_id;
            let (tx, rx) = mpsc::channel();
            let actions = broker.open_with_receipt(
                "required".into(),
                tx,
                pane(7),
                Some("floor.B".into()),
                Some("attempt.B".into()),
                |_| true,
                now,
            );
            assert!(
                actions
                    .iter()
                    .all(|a| matches!(a, MediaAction::Respond { .. })),
                "no Open, consent, publication or device work"
            );
            run(actions);
            let value = response(&rx);
            assert!(!broker.has_sessions());
            assert!(broker.receipts.is_empty());
            assert!(broker.closed.is_empty());
            assert_eq!(broker.next_id, before_next_id);
            eprintln!("C2_CAPABILITY_PRIORITY after_preflight={after_preflight}: {value}");
            observed.push(value["error"]["message"].as_str().unwrap().to_owned());
        }
        assert!(observed.iter().all(|message| message.contains(close_code::RECEIPT_UNSUPPORTED)),
            "receipt-bearing open must report receipt_unsupported when the bound client lacks ended receipts, including when it lacks WebRTC too");
    }

    #[test]
    fn receipt_broker_legacy_open_on_no_capability_client_keeps_unsupported_client() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(2, false);
        broker.note_pane_input(2, pane(7), "w1:p7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        assert!(sent.is_empty());
        assert_eq!(
            response(&rx)["error"]["code"],
            error_code::UNSUPPORTED_CLIENT
        );
        assert!(!broker.has_sessions());
        assert!(broker.receipts.is_empty());
    }

    // SEC-370-02: non-replay teardown evidence rejections are logged.
    #[derive(Clone, Default)]
    struct SecurityLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SecurityLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl SecurityLog {
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    #[test]
    fn receipt_broker_non_replay_rejections_are_logged() {
        let log = SecurityLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let observations = tracing::subscriber::with_default(subscriber, || {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let (live, _rx) = receipt_open(&mut broker, now);
            assert!(
                accept_receipt(&mut broker, 2, receipt_json(&live, "natural", None), now)
                    .is_empty()
            );
            assert!(
                log.take().contains("reason=\"wrong_client\""),
                "positive logging control proves the tracing capture is connected"
            );
            let mut observations = Vec::new();
            assert!(accept_receipt(
                &mut broker,
                1,
                receipt_json("media_nonexistent", "natural", None),
                now
            )
            .is_empty());
            observations.push(("unmatched", log.take()));
            let mut legacy = receipt_broker(now);
            let (sent, _rx) = open(&mut legacy, pane(7), now);
            let (_, legacy_id, _) = opened_session(&sent);
            assert!(accept_receipt(
                &mut legacy,
                1,
                receipt_json(&legacy_id, "natural", None),
                now
            )
            .is_empty());
            observations.push(("live legacy without receipt identity", log.take()));
            assert_eq!(
                published_media_events(&accept_receipt(
                    &mut broker,
                    1,
                    receipt_json(&live, "natural", None),
                    now
                )),
                1
            );
            log.take();
            let later = now + RECEIPT_RETENTION;
            broker.expire(later, |_| true);
            assert!(broker.state(&live).is_none());
            assert!(
                accept_receipt(&mut broker, 1, receipt_json(&live, "natural", None), later)
                    .is_empty()
            );
            observations.push(("expired record", log.take()));
            let mut capped = receipt_broker(now);
            let (first, _rx) = receipt_open(&mut capped, now);
            run(capped.close_with_request(&first, None, now));
            for _ in 0..MAX_RECEIPT_RECORDS_PER_ENDPOINT {
                let (id, _rx) = receipt_open(&mut capped, now);
                run(capped.close_with_request(&id, None, now));
            }
            assert!(capped.state(&first).is_none());
            assert!(
                accept_receipt(&mut capped, 1, receipt_json(&first, "requested", None), now)
                    .is_empty()
            );
            observations.push(("cap-evicted record", log.take()));
            observations
        });
        eprintln!("NON_REPLAY_REJECTION_LOGS: {observations:?}");
        assert!(observations.iter().all(|(_, text)| !text.is_empty()),
            "freeze requires invalid receipts dropped AND logged, not silent unmatched/legacy returns");
    }

    #[test]
    fn receipt_broker_valid_completion_and_replay_publish_once_quietly() {
        let log = SecurityLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            let (id, _rx) = receipt_open(&mut broker, now);
            let good = receipt_json(&id, "natural", None);
            assert_eq!(
                published_media_events(&accept_receipt(&mut broker, 1, good.clone(), now)),
                1
            );
            let before = receipt_state(&broker, &id);
            assert!(accept_receipt(&mut broker, 1, good, now).is_empty());
            assert_eq!(receipt_state(&broker, &id), before);
            assert!(
                log.take().is_empty(),
                "valid completion and replay may be quiet"
            );
        });
    }

    // NEW-02: a flooding endpoint gets O(log n) warn lines per reason, with the count.
    #[test]
    fn receipt_broker_rejection_logs_are_rate_limited_per_client_and_reason() {
        let log = SecurityLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let now = Instant::now();
            let mut broker = receipt_broker(now);
            for _ in 0..1000 {
                let bogus = receipt_json("media_unknown", "natural", None);
                assert!(accept_receipt(&mut broker, 1, bogus, now).is_empty());
            }
            let text = log.take();
            let lines = text
                .lines()
                .filter(|line| line.contains("unknown_session"))
                .count();
            // 1..=8, then 16, 32, 64, 128, 256, 512.
            assert_eq!(lines, 14, "{text}");
            assert!(text.contains("rejected=512"));
            assert!(!text.contains("media_unknown"));
            // A disconnect forgets the count.
            broker.client_removed(1, now);
            assert!(broker.rejections.is_empty());
        });
    }

    // SEC-370-02: every rejected teardown evidence is logged with a bounded reason
    // code and never the untrusted session id; only an exact replay of a retained
    // completed record is quiet, and it never publishes.
    #[test]
    fn receipt_broker_rejections_log_bounded_reason_never_session_id() {
        let log = SecurityLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let now = Instant::now();
            let check = |label: &str, actions: Vec<MediaAction>, id: &str, reason: &str| {
                assert!(
                    actions.is_empty(),
                    "{label}: rejected evidence has no action"
                );
                let text = log.take();
                assert!(
                    text.contains(&format!("reason=\"{reason}\"")),
                    "{label}: expected reason {reason}, got {text:?}"
                );
                assert!(
                    !text.contains(id),
                    "{label}: raw session id logged: {text:?}"
                );
            };
            let mut broker = receipt_broker(now);
            broker.client_connected(2, true);
            broker.set_client_ended_receipt(2, true);
            let (live, _rx) = receipt_open(&mut broker, now);
            let good = receipt_json(&live, "natural", None);
            let stuck = |id: &str, generation: &str| {
                MediaControl::decode(
                    "media.teardown_stuck.v1",
                    &serde_json::json!({
                        "session_id": id,
                        "generation": generation,
                        "attempt": "attempt:370.1",
                    })
                    .to_string(),
                )
                .expect("recognized control")
                .expect("bounded diagnostic")
            };
            let a = accept_receipt(&mut broker, 2, good.clone(), now);
            check("live wrong client", a, &live, "wrong_client");
            let a = broker.client_control(2, stuck(&live, "floor:370.1"), now);
            check("live stuck wrong client", a, &live, "wrong_client");
            for field in ["generation", "attempt"] {
                let mut bad = good.clone();
                bad[field] = serde_json::json!("token:370.old");
                let a = accept_receipt(&mut broker, 1, bad, now);
                check("live wrong identity", a, &live, "wrong_identity");
            }
            let a = accept_receipt(&mut broker, 1, receipt_json(&live, "replaced", None), now);
            check(
                "live uncorrelated replaced",
                a,
                &live,
                "uncorrelated_origin",
            );
            let forged = "media_forged_sid_370";
            let a = accept_receipt(&mut broker, 1, receipt_json(forged, "natural", None), now);
            check("unknown", a, forged, "unknown_session");
            let mut legacy = receipt_broker(now);
            let (sent, _rx) = open(&mut legacy, pane(7), now);
            let (_, legacy_id, _) = opened_session(&sent);
            let a = accept_receipt(
                &mut legacy,
                1,
                receipt_json(&legacy_id, "natural", None),
                now,
            );
            check("legacy", a, &legacy_id, "no_receipt_identity");

            // Complete the record; only the exact replay is quiet.
            assert_eq!(
                published_media_events(&accept_receipt(&mut broker, 1, good.clone(), now)),
                1
            );
            log.take();
            let before = receipt_state(&broker, &live);
            assert!(accept_receipt(&mut broker, 1, good.clone(), now).is_empty());
            assert!(log.take().is_empty(), "exact replay is quiet");
            let a = accept_receipt(&mut broker, 2, good.clone(), now);
            check("completed wrong client", a, &live, "wrong_client");
            let mut bad = good.clone();
            bad["generation"] = serde_json::json!("floor:370.old");
            let a = accept_receipt(&mut broker, 1, bad, now);
            check("completed wrong generation", a, &live, "wrong_identity");
            let mut bad = good.clone();
            bad["acquired"] = serde_json::json!(false);
            let a = accept_receipt(&mut broker, 1, bad, now);
            check(
                "completed contradictory acquired",
                a,
                &live,
                "replay_mismatch",
            );
            let a = accept_receipt(&mut broker, 1, receipt_json(&live, "cancelled", None), now);
            check("completed other origin", a, &live, "replay_mismatch");
            let a = accept_receipt(
                &mut broker,
                1,
                receipt_json(&live, "requested", Some("close:370.1")),
                now,
            );
            check("completed other request", a, &live, "replay_mismatch");
            let a = broker.client_control(1, stuck(&live, "floor:370.1"), now);
            check("completed stuck", a, &live, "already_completed");
            assert_eq!(receipt_state(&broker, &live), before);

            // A live server-requested close with a mismatched request id.
            let mut closing = receipt_broker(now);
            let (id, _rx) = receipt_open(&mut closing, now);
            run(closing.close_with_request(&id, Some("close:370.1".into()), now));
            log.take();
            let a = accept_receipt(
                &mut closing,
                1,
                receipt_json(&id, "requested", Some("close:370.old")),
                now,
            );
            check("closing other request", a, &id, "uncorrelated_origin");
            assert!(!accept_receipt(
                &mut closing,
                1,
                receipt_json(&id, "requested", Some("close:370.1")),
                now
            )
            .is_empty());
            log.take();
            assert!(accept_receipt(
                &mut closing,
                1,
                receipt_json(&id, "requested", Some("close:370.1")),
                now
            )
            .is_empty());
            assert!(log.take().is_empty(), "exact requested replay is quiet");
        });
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
        run(broker.answer(&session_id, "v=0 answer".into()).unwrap());
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
        let sent = run(broker.open("req".into(), tx, pane(7), |_| false, later));
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
}
