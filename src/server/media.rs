//! Server-side routing of client-local media sessions.
//!
//! `pane.media_open` binds a session to the attached client whose input last reached the
//! pane, because that is the machine the user is sitting at. The broker is pure state: the
//! headless server feeds it client, input and API events and performs the returned actions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, MediaControlsAppliedEvent, MediaErrorEvent, MediaHelperInfo,
    MediaLevelsEvent, MediaSessionState, MediaStartedEvent, MediaStoppedEvent, ResponseResult,
    SubscriptionEventData, SubscriptionEventEnvelope, SubscriptionEventKind, SuccessResponse,
};
use crate::layout::PaneId;
use crate::protocol::media::{
    close_code, MediaApplied, MediaClose, MediaControl, MediaControlFailed, MediaDeviceControl,
    MediaInfo, MediaLevels, MediaMute, MediaOpen, MediaOpenInitial, MediaPeerState, MediaSdp,
    MEDIA_DEVICE_CONTROL_CAPACITY, MEDIA_INPUT_WINDOW,
};

/// How long `pane.media_open` waits for the client's offer. It covers the one-time consent
/// prompt (30 s on the client) plus ICE gathering.
pub(crate) const MEDIA_OPEN_TIMEOUT: Duration = Duration::from_secs(45);
/// How long an offered session waits for the remote SDP answer.
pub(crate) const MEDIA_ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
/// A supplied answer must establish the peer and its data channel within this window.
pub(crate) const MEDIA_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A missing device acknowledgement is uncertain, not evidence that the call ended.
pub(crate) const MEDIA_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_LEVELS_INTERVAL: Duration = Duration::from_millis(100);
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
    pub(crate) const CONTROL_FAILED: &str = "media_control_failed";
}

/// Side effects the headless server performs for the broker.
#[derive(Debug)]
pub(crate) enum MediaAction {
    Publish {
        /// Producer attribution, never reconstructed from the public pane alias.
        pane_id: PaneId,
        event: SubscriptionEventEnvelope,
    },
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

/// Public response waiting only; device admission belongs exclusively to outstanding.
#[derive(Debug)]
struct PendingControl {
    control_id: u64,
    request_id: String,
    respond_to: Sender<String>,
    deadline: Instant,
}

/// Event-only correlation for each actual target of the same admitted dispatch.
/// It never owns a response, allocates an ID, or reserves device capacity. Bounded
/// by that target's outstanding records, it survives the named response's end.
#[derive(Debug)]
struct ControlEventContext {
    request_id: String,
}

/// Runtime pane policy, retained until pane death or an explicit request changes an axis.
/// Session outcomes (including caller close) never clear this policy or renew consent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MediaIntent {
    pub(crate) input_muted: bool,
    pub(crate) output_suppressed: bool,
}

/// Broker intent acceptance only: actions are not native admission or delivery receipts.
#[derive(Debug)]
pub(crate) struct MediaControlDispatch {
    pub(crate) control_id: u64,
    pub(crate) session_ids: Vec<String>,
    pub(crate) actions: Vec<MediaAction>,
}

struct ControlTargets {
    actual: String,
    targets: Vec<(String, u64)>,
}

#[derive(Debug, Default)]
struct MediaTelemetry {
    applied: Option<MediaApplied>,
    control_failed: Option<MediaControlFailed>,
    legacy_control_failed: Option<MediaControlFailed>,
    levels: Option<MediaLevels>,
    info: Option<MediaInfo>,
}

#[derive(Debug)]
struct ReplacedSession {
    session_id: String,
    // Another pending attempt is superseded on acceptance, but is not the connected
    // source that authorizes this renewal. Cancelling it must not cancel this one.
    authorizes_renewal: bool,
    // Capture authority independently of bounded closed-history retention.
    pane: PaneId,
    client_id: u64,
    // Keep the caller's handle while a renewal is pending, even if the client already
    // retired the old peer as replaced before sending the successor's offer.
    open_id: Option<String>,
}

#[derive(Debug)]
struct MediaSession {
    client_id: u64,
    pane: PaneId,
    /// Runtime-supplied public identity is captured before a pane can be deleted.
    event_pane_id: String,
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
    wants_output_suppressed: bool,
    state: MediaSessionState,
    /// Observations, never pane-policy authority. Startup requests are not observations.
    muted: bool,
    output_suppressed: bool,
    /// Expected complete pairs for accepted device controls, independent of desired state.
    outstanding: HashMap<u64, MediaIntent>,
    terminal_watermark: u64,
    offered_deadline: Option<Instant>,
    connect_deadline: Option<Instant>,
    started: bool,
    pending_controls: VecDeque<PendingControl>,
    control_events: HashMap<u64, ControlEventContext>,
    /// Last admitted levels request, not observed application. None needs reconciliation.
    levels_requested: Option<bool>,
    levels_control_id: Option<u64>,
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
        } else if self
            .connect_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            Some((
                close_code::NEGOTIATION_TIMEOUT,
                "the media peer did not connect in time after the answer",
            ))
        } else {
            None
        }
    }

    fn publish_started(&mut self, session_id: &str, actions: &mut Vec<MediaAction>) {
        if self.started || self.state != MediaSessionState::Connected {
            return;
        }
        let Some(info) = self.telemetry.info.as_ref() else {
            return;
        };
        self.started = true;
        actions.push(MediaAction::Publish {
            pane_id: self.pane,
            event: SubscriptionEventEnvelope {
                event: SubscriptionEventKind::MediaStarted,
                data: SubscriptionEventData::MediaStarted(MediaStartedEvent {
                    pane_id: self.event_pane_id.clone(),
                    session_id: session_id.to_owned(),
                    epoch: self.epoch,
                    helper: MediaHelperInfo {
                        version: info.version.clone(),
                        commit: info.commit.clone(),
                        pid: info.pid,
                    },
                    input_device: info.input_device.clone(),
                    output_device: info.output_device.clone(),
                }),
            },
        });
    }

    fn expire_controls(&mut self, now: Instant, actions: &mut Vec<MediaAction>) {
        while self
            .pending_controls
            .front()
            .is_some_and(|pending| now >= pending.deadline)
        {
            let Some(pending) = self.pending_controls.pop_front() else {
                break;
            };
            // Timeout settles only the public response; outstanding and control_events
            // retain the bounded CID correlation for a later validated device ACK.
            actions.push(MediaAction::Respond {
                respond_to: pending.respond_to,
                response: error_response(
                    pending.request_id,
                    error_code::TIMEOUT,
                    "the client did not acknowledge the media control in time; application is uncertain",
                ),
                // Only open/offer responses own microphone-abandonment receipts.
                session_id: None,
            });
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
    client_id: u64,
    pane: PaneId,
    /// Only a connected source retired by replacement can authorize a pending alias.
    authorizes_renewal: bool,
    view: MediaSessionView,
}

#[derive(Debug)]
pub(crate) struct MediaBroker {
    /// Attached client-shell clients and whether each advertised media support.
    clients: HashMap<u64, MediaClient>,
    /// Per pane, the client whose input reached it last. Input to another pane never moves
    /// this owner, so it cannot reroute a pane's microphone to an older client.
    pane_owners: HashMap<PaneId, PaneOwner>,
    pane_intents: HashMap<PaneId, MediaIntent>,
    next_owner_cleanup: Option<Instant>,
    pane_epochs: HashMap<PaneId, u64>,
    pane_event_ids: HashMap<PaneId, String>,
    levels_interest: HashSet<PaneId>,
    levels_last_published: HashMap<PaneId, Instant>,
    sessions: HashMap<String, MediaSession>,
    closed: VecDeque<ClosedSession>,
    /// First unknown close time and caller handle, in FIFO order. Repeats do not refresh either.
    cancelled_opens: VecDeque<(Instant, String)>,
    id_prefix: String,
    next_id: u64,
    next_control_id: u64,
}

pub(crate) fn success_response(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_error","message":"failed to encode response"}}"#
            .to_owned()
    })
}

pub(crate) fn error_response(id: String, code: &str, message: impl Into<String>) -> String {
    error_response_with_close_code(id, code, message, None)
}

pub(crate) fn error_response_with_close_code(
    id: String,
    code: &str,
    message: impl Into<String>,
    close_code: Option<&str>,
) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.to_owned(),
            message: message.into(),
            close_code: close_code.map(str::to_owned),
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
        // Independent randomized hash keys provide a 128-bit boot generation, rather
        // than a clock stamp that can collide across restarts in the same millisecond.
        let mut boot = [0_u8; 16];
        if let Err(error) = crate::platform::secure_random(&mut boot) {
            // Preserve the infallible constructor without falling back to a clock.
            // RandomState obtains independent randomized hash keys from std's source.
            tracing::warn!(%error, "media boot generation using randomized hash fallback");
            for chunk in boot.chunks_exact_mut(8) {
                chunk.copy_from_slice(
                    &std::collections::hash_map::RandomState::new()
                        .build_hasher()
                        .finish()
                        .to_ne_bytes(),
                );
            }
        }
        let boot = u128::from_ne_bytes(boot);
        Self {
            clients: HashMap::new(),
            pane_owners: HashMap::new(),
            pane_intents: HashMap::new(),
            next_owner_cleanup: None,
            pane_epochs: HashMap::new(),
            pane_event_ids: HashMap::new(),
            levels_interest: HashSet::new(),
            levels_last_published: HashMap::new(),
            sessions: HashMap::new(),
            closed: VecDeque::new(),
            cancelled_opens: VecDeque::new(),
            id_prefix: format!("media_{boot:032x}_"),
            next_id: 1,
            next_control_id: 1,
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
        (!self.pane_owners.is_empty()
            || !self.pane_epochs.is_empty()
            || !self.pane_event_ids.is_empty()
            || !self.pane_intents.is_empty()
            || !self.levels_interest.is_empty())
            && self
                .next_owner_cleanup
                .is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn retain_live_panes(&mut self, now: Instant, pane_exists: impl Fn(PaneId) -> bool) {
        self.pane_owners.retain(|pane, _| pane_exists(*pane));
        // Epochs and intent belong to the pane's lifetime, not its input connection.
        self.pane_epochs.retain(|pane, _| pane_exists(*pane));
        self.pane_intents.retain(|pane, _| pane_exists(*pane));
        self.pane_event_ids.retain(|pane, _| pane_exists(*pane));
        self.levels_interest.retain(|pane| pane_exists(*pane));
        self.levels_last_published
            .retain(|pane, _| pane_exists(*pane));
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
                    response: error_response_with_close_code(
                        request_id,
                        error_code::REFUSED,
                        "closed: cancelled before open",
                        Some(close_code::CLOSED),
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
                response: error_response_with_close_code(
                    request_id,
                    error_code::REFUSED,
                    "abandoned: the API caller disconnected before the media request was opened",
                    Some(close_code::ABANDONED),
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
                response: error_response_with_close_code(
                    request_id,
                    error_code::REFUSED,
                    format!(
                        "{}: the client no longer shows this pane",
                        close_code::NOT_VIEWED
                    ),
                    Some(close_code::NOT_VIEWED),
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
        // Startup gates use retained policy, never an older observed acknowledgement.
        let initial = self.intent(pane);
        let replaced = self
            .sessions
            .iter()
            .filter(|(_, session)| session.pane == pane || session.client_id == client_id)
            .map(|(id, session)| ReplacedSession {
                session_id: id.clone(),
                authorizes_renewal: session.state == MediaSessionState::Connected,
                pane: session.pane,
                client_id: session.client_id,
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

        let session_id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        self.pane_epochs.insert(pane, epoch);
        self.sessions.insert(
            session_id.clone(),
            MediaSession {
                client_id,
                pane,
                event_pane_id: self
                    .pane_event_ids
                    .get(&pane)
                    .cloned()
                    .unwrap_or_else(|| input.pane_ref.clone()),
                open_id,
                epoch,
                pane_ref: input.pane_ref.clone(),
                replaces,
                wants_muted: initial.input_muted,
                wants_output_suppressed: initial.output_suppressed,
                state: MediaSessionState::Opening,
                muted: false,
                output_suppressed: false,
                outstanding: HashMap::new(),
                terminal_watermark: 0,
                offered_deadline: None,
                connect_deadline: None,
                started: false,
                pending_controls: VecDeque::new(),
                control_events: HashMap::new(),
                levels_requested: Some(false),
                levels_control_id: None,
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
            control: MediaControl::OpenInitial(MediaOpenInitial {
                session_id: session_id.clone(),
                pane_id: input.pane_ref.clone(),
                input_muted: initial.input_muted,
                output_suppressed: initial.output_suppressed,
            }),
        });
        if !initial.input_muted && !initial.output_suppressed {
            // Contiguous, same-ID v1 alternate only for the complete non-protective pair.
            // Protected opens never downgrade or depend on an after-open Mute.
            actions.push(MediaAction::Send {
                client_id,
                control: MediaControl::Open(MediaOpen {
                    session_id: session_id.clone(),
                    pane_id: input.pane_ref,
                }),
            });
        }
        // Privacy is installed before audio.open by OpenInitial, never a later control.
        // Levels default off; opt in only through the same admitted dispatch seam.
        self.reconcile_levels(&session_id, now, &mut actions);
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
        // Callback deadlines have the same authority as maintenance. In particular an
        // acknowledgement at 5s cannot win a race against the broker's timeout.
        session.expire_controls(now, &mut actions);
        let reconcile_levels = matches!(&control, MediaControl::State(_) | MediaControl::Offer(_));
        match control {
            MediaControl::Applied(applied) => {
                let Some(expected) = session.outstanding.get(&applied.control_id) else {
                    return actions;
                };
                if expected.input_muted != applied.input_muted
                    || expected.output_suppressed != applied.output_suppressed
                {
                    return actions;
                }
                // Terminal accounting and public settlement are lossless even for an
                // older recognized CID. Only the latest telemetry obeys the watermark.
                session.outstanding.remove(&applied.control_id);
                if session.levels_control_id == Some(applied.control_id) {
                    session.levels_control_id = None;
                }
                if applied.control_id > session.terminal_watermark {
                    session.terminal_watermark = applied.control_id;
                    session.muted = applied.input_muted;
                    session.output_suppressed = applied.output_suppressed;
                    session.telemetry.applied = Some(applied.clone());
                }
                let context = session.control_events.remove(&applied.control_id);
                let pending = session
                    .pending_controls
                    .iter()
                    .position(|pending| pending.control_id == applied.control_id)
                    .and_then(|index| session.pending_controls.remove(index));
                let request_id = if let Some(pending) = pending {
                    actions.push(MediaAction::Respond {
                        respond_to: pending.respond_to,
                        response: success_response(
                            pending.request_id.clone(),
                            ResponseResult::MediaControlsApplied {
                                session_id: session_id.clone(),
                                request_id: pending.request_id.clone(),
                                input_muted: applied.input_muted,
                                output_suppressed: applied.output_suppressed,
                                epoch: session.epoch,
                            },
                        ),
                        session_id: None,
                    });
                    Some(pending.request_id)
                } else {
                    context.map(|context| context.request_id)
                };
                if let Some(request_id) = request_id {
                    actions.push(MediaAction::Publish {
                        pane_id: session.pane,
                        event: SubscriptionEventEnvelope {
                            event: SubscriptionEventKind::MediaControlsApplied,
                            data: SubscriptionEventData::MediaControlsApplied(
                                MediaControlsAppliedEvent {
                                    pane_id: session.event_pane_id.clone(),
                                    session_id: session_id.clone(),
                                    request_id,
                                    input_muted: applied.input_muted,
                                    output_suppressed: applied.output_suppressed,
                                    epoch: session.epoch,
                                },
                            ),
                        },
                    });
                }
            }
            MediaControl::ControlFailed(failed) => {
                let Some(control_id) = failed.control_id else {
                    // A legacy failure is telemetry only, never a correlated outcome.
                    session.telemetry.legacy_control_failed = Some(failed);
                    return actions;
                };
                if session.outstanding.remove(&control_id).is_none() {
                    return actions;
                }
                session.control_events.remove(&control_id);
                if session.levels_control_id == Some(control_id) {
                    session.levels_requested = None;
                    session.levels_control_id = None;
                }
                if let Some(pending) = session
                    .pending_controls
                    .iter()
                    .position(|pending| pending.control_id == control_id)
                    .and_then(|index| session.pending_controls.remove(index))
                {
                    let message = failed
                        .message
                        .as_deref()
                        .unwrap_or("the client could not apply the media control");
                    actions.push(MediaAction::Respond {
                        respond_to: pending.respond_to,
                        response: error_response(
                            pending.request_id,
                            error_code::CONTROL_FAILED,
                            format!("{}: {message}", failed.code),
                        ),
                        session_id: None,
                    });
                }
                if control_id > session.terminal_watermark {
                    session.terminal_watermark = control_id;
                    session.telemetry.control_failed = Some(failed);
                }
            }
            MediaControl::Info(info) => {
                session.telemetry.info = Some(info);
                session.publish_started(&session_id, &mut actions);
            }
            MediaControl::Levels(levels) => {
                if !self.levels_interest.contains(&session.pane) {
                    return actions;
                }
                if self
                    .levels_last_published
                    .get(&session.pane)
                    .is_none_or(|last| {
                        now.saturating_duration_since(*last) >= MEDIA_LEVELS_INTERVAL
                    })
                {
                    self.levels_last_published.insert(session.pane, now);
                    actions.push(MediaAction::Publish {
                        pane_id: session.pane,
                        event: SubscriptionEventEnvelope {
                            event: SubscriptionEventKind::MediaLevels,
                            data: SubscriptionEventData::MediaLevels(MediaLevelsEvent {
                                pane_id: session.event_pane_id.clone(),
                                session_id: session_id.clone(),
                                input_peak: levels.input_peak,
                                output_peak: levels.output_peak,
                            }),
                        },
                    });
                }
                session.telemetry.levels = Some(levels);
            }
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
                // State is not an answer or authority to renew. Still use this existing
                // lifecycle event to reconcile desired levels, including pre-answer.
                if session.pending.is_none() {
                    session.muted = update.muted;
                }
                if session.pending.is_none() && session.offered_deadline.is_none() {
                    match update.state {
                        MediaPeerState::Connecting => {
                            // A delayed connecting notification must not undo connection.
                            if session.state != MediaSessionState::Connected {
                                session.state = MediaSessionState::Connecting;
                            }
                        }
                        MediaPeerState::Connected => {
                            session.state = MediaSessionState::Connected;
                            session.connect_deadline = None;
                            session.publish_started(&session_id, &mut actions);
                        }
                        MediaPeerState::Failed => {
                            let message =
                                update.detail.as_deref().unwrap_or("the media peer failed");
                            self.finish(
                                &session_id,
                                close_code::DEVICE_ERROR,
                                message,
                                true,
                                now,
                                &mut actions,
                            );
                        }
                        MediaPeerState::Unknown => {}
                    }
                }
            }
            MediaControl::Close(MediaClose { code, message, .. }) => {
                let code = code.unwrap_or_else(|| close_code::CLOSED.to_owned());
                let message = message.unwrap_or_else(|| "the client ended the session".to_owned());
                self.finish(&session_id, &code, &message, false, now, &mut actions);
            }
            MediaControl::Open(_)
            | MediaControl::OpenInitial(_)
            | MediaControl::Answer(_)
            | MediaControl::Mute(_)
            | MediaControl::Control(_) => {}
        }
        if reconcile_levels {
            self.reconcile_levels(&session_id, now, &mut actions);
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
            session.connect_deadline = Some(now + MEDIA_CONNECT_TIMEOUT);
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
        MediaControl::Mute(MediaMute {
            session_id: session_id.to_owned(),
            muted,
        })
        .validate()
        .map_err(|message| (error_code::NOT_READY, message))?;
        let ControlTargets { actual, targets } =
            self.control_targets(session_id, true, None, Instant::now())?;
        let intent = self.record_control_intent(&actual, Some(muted), None)?;
        for (id, _) in &targets {
            if let Some(session) = self.sessions.get_mut(id) {
                session.wants_muted = intent.input_muted;
                session.wants_output_suppressed = intent.output_suppressed;
            }
        }
        Ok(targets
            .into_iter()
            .map(|(session_id, client_id)| MediaAction::Send {
                client_id,
                control: MediaControl::Mute(MediaMute { session_id, muted }),
            })
            .collect())
    }

    /// Atomically accept intent and correlation records for every authorized actual target.
    /// C executes these actions; acceptance is not a queued/delivered/applied receipt.
    pub(crate) fn dispatch_control(
        &mut self,
        session_id: &str,
        input_muted: Option<bool>,
        output_suppressed: Option<bool>,
        levels: Option<bool>,
        now: Instant,
    ) -> Result<MediaControlDispatch, (&'static str, String)> {
        MediaControl::Control(MediaDeviceControl {
            session_id: session_id.to_owned(),
            control_id: 0,
            input_muted,
            output_suppressed,
            levels,
        })
        .validate()
        .map_err(|message| (error_code::NOT_READY, message))?;
        let privacy = input_muted.is_some() || output_suppressed.is_some();
        let ControlTargets { actual, targets } =
            self.control_targets(session_id, privacy, levels, now)?;
        for (id, _) in &targets {
            if self
                .sessions
                .get(id)
                .is_none_or(|session| session.outstanding.len() >= MEDIA_DEVICE_CONTROL_CAPACITY)
            {
                return Err((
                    error_code::CONTROL_FAILED,
                    "a media target has no device-control allowance (32 outstanding controls)"
                        .to_owned(),
                ));
            }
        }
        let control_id = self.next_control_id;
        let next_control_id = control_id.checked_add(1).ok_or_else(|| {
            (
                error_code::CONTROL_FAILED,
                "media control identifiers are exhausted".to_owned(),
            )
        })?;
        // All fallible target/capacity/ID checks precede this single commit helper.
        let intent = self.record_control_intent(&actual, input_muted, output_suppressed)?;
        self.next_control_id = next_control_id;
        let session_ids = targets.iter().map(|(id, _)| id.clone()).collect();
        let mut actions = Vec::with_capacity(targets.len());
        for (id, client_id) in targets {
            if let Some(session) = self.sessions.get_mut(&id) {
                let desired_levels = self.levels_interest.contains(&session.pane);
                let target_levels = if id == session_id { levels } else { None };
                let target_levels = target_levels
                    .or((session.levels_requested != Some(desired_levels))
                        .then_some(desired_levels));
                session.wants_muted = intent.input_muted;
                session.wants_output_suppressed = intent.output_suppressed;
                session.outstanding.insert(control_id, intent);
                if let Some(enabled) = target_levels {
                    session.levels_requested = Some(enabled);
                    session.levels_control_id = Some(control_id);
                }
                actions.push(MediaAction::Send {
                    client_id,
                    control: MediaControl::Control(MediaDeviceControl {
                        session_id: id,
                        control_id,
                        input_muted,
                        output_suppressed,
                        levels: target_levels,
                    }),
                });
            }
        }
        // Every expected pair is published before any returned action can execute.
        Ok(MediaControlDispatch {
            control_id,
            session_ids,
            actions,
        })
    }

    /// Commit only the actual preflight-resolved live target's pane policy. No sends or
    /// alias coordination here; callers must finish their entire preflight before calling.
    fn record_control_intent(
        &mut self,
        session_id: &str,
        input_muted: Option<bool>,
        output_suppressed: Option<bool>,
    ) -> Result<MediaIntent, (&'static str, String)> {
        let pane = self
            .sessions
            .get(session_id)
            .map(|session| session.pane)
            .ok_or_else(|| {
                (
                    error_code::SESSION_NOT_FOUND,
                    "no live media session with this id".to_owned(),
                )
            })?;
        let intent = self.pane_intents.entry(pane).or_default();
        if let Some(value) = input_muted {
            intent.input_muted = value;
        }
        if let Some(value) = output_suppressed {
            intent.output_suppressed = value;
        }
        Ok(*intent)
    }

    pub(crate) fn intent(&self, pane: PaneId) -> MediaIntent {
        self.pane_intents.get(&pane).copied().unwrap_or_default()
    }

    /// A known failed targeted send ends that owned session; stale/repeated failures do
    /// nothing. Existing finish cancellation of its dependent renewals remains intact.
    // Called after writer-availability, encoding and targeted-send checks.
    pub(crate) fn control_delivery_failed(
        &mut self,
        client_id: u64,
        session_id: &str,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        if self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.client_id == client_id)
        {
            self.finish(
                session_id,
                close_code::DISCONNECTED,
                "media control delivery failed",
                false,
                now,
                &mut actions,
            );
        }
        actions
    }

    /// Guard an optional API epoch without changing legacy callers or mutating any
    /// session. A retired identity cannot authorize a new session after a restart.
    pub(crate) fn check_epoch(
        &self,
        session_id: Option<&str>,
        open_id: Option<&str>,
        epoch: Option<u64>,
    ) -> Result<(), (&'static str, String)> {
        let Some(epoch) = epoch else {
            return Ok(());
        };
        let matches = self.sessions.iter().any(|(id, session)| {
            (session_id.is_some() || open_id.is_some())
                && session_id.is_none_or(|wanted| wanted == id)
                && open_id.is_none_or(|wanted| {
                    session.open_id.as_deref() == Some(wanted)
                        || session.replaces.iter().any(|old| {
                            old.authorizes_renewal && old.open_id.as_deref() == Some(wanted)
                        })
                })
                && session.epoch == epoch
        });
        if matches {
            Ok(())
        } else {
            Err((
                error_code::SESSION_NOT_FOUND,
                "no live media session with this id".to_owned(),
            ))
        }
    }

    /// Request device changes, keeping the API response until the matching client
    /// acknowledgement. Endpoint controls use the existing client's ordered device
    /// queue; ids are allocated in dispatch order, including internal controls.
    pub(crate) fn control(
        &mut self,
        request_id: String,
        respond_to: Sender<String>,
        session_id: &str,
        input_muted: Option<bool>,
        output_suppressed: Option<bool>,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
        if let Some((code, message)) = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.retirement_reason(now))
        {
            self.finish(session_id, code, message, true, now, &mut actions);
        }
        // Public requests name a live, offered session, never a retired alias. Internal
        // lineage propagation still goes through the combined dispatch below.
        let failure = match self.sessions.get(session_id) {
            None => Some((
                error_code::SESSION_NOT_FOUND,
                "no live media session with this id",
            )),
            Some(session) if session.pending.is_some() => Some((
                error_code::SESSION_NOT_FOUND,
                "the client has not sent its offer yet",
            )),
            Some(_) => None,
        };
        if let Some((code, message)) = failure {
            let close_code = self.state(session_id).and_then(|view| view.code);
            actions.push(MediaAction::Respond {
                respond_to,
                response: error_response_with_close_code(
                    request_id,
                    code,
                    message,
                    close_code.as_deref(),
                ),
                session_id: None,
            });
            return actions;
        }
        let dispatch =
            match self.dispatch_control(session_id, input_muted, output_suppressed, None, now) {
                Ok(dispatch) => dispatch,
                Err((code, message)) => {
                    let code = if code == error_code::NOT_READY {
                        error_code::SESSION_NOT_FOUND
                    } else {
                        code
                    };
                    let close_code = self.close_cause(session_id, now);
                    actions.push(MediaAction::Respond {
                        respond_to,
                        response: error_response_with_close_code(
                            request_id,
                            code,
                            message,
                            close_code.as_deref(),
                        ),
                        session_id: None,
                    });
                    return actions;
                }
            };
        let deadline = now + MEDIA_CONTROL_TIMEOUT;
        for target in &dispatch.session_ids {
            if let Some(session) = self.sessions.get_mut(target) {
                session.control_events.insert(
                    dispatch.control_id,
                    ControlEventContext {
                        request_id: request_id.clone(),
                    },
                );
            }
        }
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.expire_controls(now, &mut actions);
            session.pending_controls.push_back(PendingControl {
                control_id: dispatch.control_id,
                request_id,
                respond_to,
                deadline,
            });
        }
        // Waiters/correlation exist before synchronous delivery failures can execute.
        actions.extend(dispatch.actions);
        actions
    }

    /// Capture the runtime's canonical public pane id, not the client's legacy alias.
    /// Call before open; retaining it on the session also preserves pane-closed events.
    pub(crate) fn set_pane_event_id(&mut self, pane: PaneId, pane_id: String) {
        for session in self
            .sessions
            .values_mut()
            .filter(|session| session.pane == pane)
        {
            session.event_pane_id = pane_id.clone();
        }
        self.pane_event_ids.insert(pane, pane_id);
    }

    pub(crate) fn set_levels(
        &mut self,
        pane: PaneId,
        enabled: bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        let changed = if enabled {
            self.levels_interest.insert(pane)
        } else {
            self.levels_interest.remove(&pane)
        };
        let mut actions = Vec::new();
        if !changed {
            return actions;
        }
        let sessions = self
            .sessions
            .iter()
            .filter(|(_, session)| session.pane == pane)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for session_id in sessions {
            self.reconcile_levels(&session_id, now, &mut actions);
        }
        actions
    }

    /// Event-driven only: a rejected transition stays desired in levels_interest.
    /// Reuse the next dispatch/state/renewal, never a timer or a second coordinator.
    fn reconcile_levels(&mut self, session_id: &str, now: Instant, actions: &mut Vec<MediaAction>) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let enabled = self.levels_interest.contains(&session.pane);
        if session.levels_requested == Some(enabled) {
            return;
        }
        if let Ok(dispatch) = self.dispatch_control(session_id, None, None, Some(enabled), now) {
            actions.extend(dispatch.actions);
        }
    }

    /// Read the causal close code for an API rejection even when its event precedes
    /// maintenance. Like answer's guard, this remembers abandonment but leaves all
    /// terminal side effects to expire. Retained closes keep their original reason.
    pub(crate) fn close_cause(&mut self, session_id: &str, now: Instant) -> Option<String> {
        if let Some(session) = self.sessions.get_mut(session_id) {
            return session
                .retirement_reason(now)
                .map(|(code, _)| code.to_owned());
        }
        self.state(session_id).and_then(|view| view.code)
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
    // Correlated and legacy failures are stored separately; None never settles an ID.
    #[allow(dead_code)]
    pub(crate) fn control_failed(&self, session_id: &str) -> Option<&MediaControlFailed> {
        self.sessions.get(session_id).and_then(|session| {
            session
                .telemetry
                .control_failed
                .as_ref()
                .or(session.telemetry.legacy_control_failed.as_ref())
        })
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
                    session.connect_deadline,
                    session
                        .pending_controls
                        .front()
                        .map(|pending| pending.deadline),
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
        // Topology, not session retention or stale input, bounds runtime pane policy.
        // This must run even when no live or retained session remains.
        self.pane_intents.retain(|pane, _| pane_exists(*pane));
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
        for session in self.sessions.values_mut() {
            session.expire_controls(now, &mut actions);
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
                other.replaces.iter().any(|old| {
                    old.authorizes_renewal
                        && old.session_id == session_id
                        && old.pane == other.pane
                        && old.client_id == other.client_id
                })
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// A replacement string alone is not authority: constrain lineage to a connected
    /// source (or its retained replacement receipt), the same runtime pane and owner.
    fn control_alias_authorized(&self, id: &str, pane: PaneId, client_id: u64) -> bool {
        if let Some(source) = self.sessions.get(id) {
            return source.pane == pane
                && source.client_id == client_id
                && source.state == MediaSessionState::Connected;
        }
        if let Some(source) = self
            .closed
            .iter()
            .find(|source| source.view.session_id == id)
        {
            return source.pane == pane
                && source.client_id == client_id
                && source.authorizes_renewal;
        }
        // A's acceptance-time receipt outlives bounded closed history, but only on
        // an unconsumed pending successor with the captured pane and owner.
        self.sessions.values().any(|candidate| {
            candidate.pending.is_some()
                && candidate.pane == pane
                && candidate.client_id == client_id
                && candidate.replaces.iter().any(|old| {
                    old.session_id == id
                        && old.authorizes_renewal
                        && old.pane == pane
                        && old.client_id == client_id
                })
        })
    }

    /// Shared authority preflight, including A's retirement and consumed-alias guards.
    /// Only retirement evidence may change here; policy, credits and IDs remain untouched.
    fn control_targets(
        &mut self,
        session_id: &str,
        privacy: bool,
        levels: Option<bool>,
        now: Instant,
    ) -> Result<ControlTargets, (&'static str, String)> {
        let actual = if self.sessions.contains_key(session_id) {
            session_id.to_owned()
        } else {
            let mut aliases = self
                .sessions
                .iter()
                .filter(|(_, session)| {
                    session.pending.is_some()
                        && session.replaces.iter().any(|old| {
                            old.session_id == session_id
                                && old.authorizes_renewal
                                && old.pane == session.pane
                                && old.client_id == session.client_id
                        })
                        && self.control_alias_authorized(
                            session_id,
                            session.pane,
                            session.client_id,
                        )
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            aliases.sort();
            aliases.into_iter().next().ok_or_else(|| {
                (
                    error_code::SESSION_NOT_FOUND,
                    "no live media session with this id".to_owned(),
                )
            })?
        };
        if actual != session_id && levels.is_some() {
            return Err((
                error_code::NOT_READY,
                "levels require a named live media session".to_owned(),
            ));
        }
        let Some(named) = self.sessions.get(&actual) else {
            return Err((
                error_code::SESSION_NOT_FOUND,
                "no live media target".to_owned(),
            ));
        };
        let pane = named.pane;
        let client_id = named.client_id;
        if named.pending.is_some()
            && named.replaces.iter().any(|old| old.authorizes_renewal)
            && !named.replaces.iter().any(|old| {
                old.authorizes_renewal
                    && old.pane == pane
                    && old.client_id == client_id
                    && self.control_alias_authorized(&old.session_id, pane, client_id)
            })
        {
            return Err((
                error_code::SESSION_NOT_FOUND,
                "the pending renewal no longer has its source authority".to_owned(),
            ));
        }
        let mut roots = vec![session_id.to_owned()];
        roots.extend(
            named
                .replaces
                .iter()
                .filter(|old| {
                    old.authorizes_renewal
                        && old.pane == pane
                        && old.client_id == client_id
                        && self.control_alias_authorized(&old.session_id, pane, client_id)
                })
                .map(|old| old.session_id.clone()),
        );
        let mut targets = vec![(actual.clone(), client_id)];
        if privacy {
            for (id, candidate) in &self.sessions {
                if id == &actual || candidate.pane != pane || candidate.client_id != client_id {
                    continue;
                }
                let source = roots.iter().any(|root| root == id)
                    && self.control_alias_authorized(id, pane, client_id);
                let successor = candidate.pending.is_some()
                    && candidate.replaces.iter().any(|old| {
                        old.authorizes_renewal
                            && old.pane == pane
                            && old.client_id == client_id
                            && roots.contains(&old.session_id)
                            && self.control_alias_authorized(&old.session_id, pane, client_id)
                    });
                if source || successor {
                    targets.push((id.clone(), client_id));
                }
            }
        }
        targets[1..].sort_by(|left, right| left.0.cmp(&right.0));
        // A levels-only dispatch still cannot borrow a retired source's authority.
        // Check authorizing live sources even when they are not device-send targets.
        for root in &roots {
            if !targets.iter().any(|(id, _)| id == root)
                && self
                    .sessions
                    .get_mut(root)
                    .is_some_and(|source| source.retirement_reason(now).is_some())
            {
                return Err((
                    error_code::SESSION_NOT_FOUND,
                    "the renewal source is no longer available".to_owned(),
                ));
            }
        }
        for (id, target_client) in &targets {
            let available = self.sessions.get(id).is_some_and(|session| {
                session.pane == pane
                    && session.client_id == *target_client
                    && session.state != MediaSessionState::Failed
                    && session.state != MediaSessionState::Closed
            }) && self
                .clients
                .get(target_client)
                .is_some_and(|client| client.capable);
            if !available
                || self
                    .sessions
                    .get_mut(id)
                    .is_none_or(|session| session.retirement_reason(now).is_some())
            {
                return Err((
                    error_code::SESSION_NOT_FOUND,
                    "a media target is no longer available".to_owned(),
                ));
            }
        }
        Ok(ControlTargets { actual, targets })
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
        // The public deadline is independent of device credits and session retirement.
        session.expire_controls(now, actions);
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
                response: error_response_with_close_code(
                    pending.request_id,
                    error,
                    text,
                    Some(code),
                ),
                session_id: Some(session_id.to_owned()),
            });
        }
        for pending in session.pending_controls {
            actions.push(MediaAction::Respond {
                respond_to: pending.respond_to,
                response: error_response_with_close_code(
                    pending.request_id,
                    error_code::CONTROL_FAILED,
                    format!("{code}: {message}"),
                    Some(code),
                ),
                session_id: None,
            });
        }
        if matches!(
            code,
            close_code::DEVICE_ERROR
                | close_code::NEGOTIATION_TIMEOUT
                | close_code::TIMEOUT
                | close_code::UNANSWERED
        ) {
            actions.push(MediaAction::Publish {
                pane_id: session.pane,
                event: SubscriptionEventEnvelope {
                    event: SubscriptionEventKind::MediaError,
                    data: SubscriptionEventData::MediaError(MediaErrorEvent {
                        pane_id: session.event_pane_id.clone(),
                        session_id: session_id.to_owned(),
                        code: code.to_owned(),
                        message: message.to_owned(),
                    }),
                },
            });
        }
        actions.push(MediaAction::Publish {
            pane_id: session.pane,
            event: SubscriptionEventEnvelope {
                event: SubscriptionEventKind::MediaStopped,
                data: SubscriptionEventData::MediaStopped(MediaStoppedEvent {
                    pane_id: session.event_pane_id.clone(),
                    session_id: session_id.to_owned(),
                    epoch: session.epoch,
                    code: code.to_owned(),
                    message: message.to_owned(),
                }),
            },
        });
        self.closed.push_back(ClosedSession {
            closed_at: now,
            client_id: session.client_id,
            pane: session.pane,
            authorizes_renewal: code == close_code::REPLACED
                && session.state == MediaSessionState::Connected,
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
    use crate::protocol::ServerMessage;
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
                MediaAction::Publish { .. } => None,
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
        broker.set_pane_event_id(pane_id, format!("p_{}", pane_id.raw()));
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
                MediaControl::OpenInitial(open) => {
                    Some((*client_id, open.session_id.clone(), open.pane_id.clone()))
                }
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
                                "message": "closed: cancelled before open",
                                "close_code": "closed"
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
        let first = broker
            .dispatch_control(&session_id, Some(true), None, None, now)
            .unwrap();
        run(first.actions);
        run(broker.client_control(
            1,
            MediaControl::Applied(MediaApplied {
                session_id: session_id.clone(),
                control_id: first.control_id,
                input_muted: true,
                output_suppressed: false,
            }),
            now,
        ));
        let second = broker
            .dispatch_control(&session_id, Some(false), Some(true), None, now)
            .unwrap();
        run(second.actions);
        run(broker.client_control(
            1,
            MediaControl::Applied(MediaApplied {
                session_id: session_id.clone(),
                control_id: second.control_id,
                input_muted: false,
                output_suppressed: true,
            }),
            now,
        ));
        assert_eq!(broker.applied(&session_id).unwrap().control_id, 2);
        assert!(!broker.applied(&session_id).unwrap().input_muted);
        assert!(broker.applied(&session_id).unwrap().output_suppressed);

        run(broker.set_levels(pane(7), true, now));
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
                run(broker
                    .dispatch_control(&session_id, Some(true), None, Some(true), later)
                    .unwrap()
                    .actions);
                run(broker.set_levels(pane(7), true, later));
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

    fn initial_control(sent: &[(u64, MediaControl)]) -> &MediaOpenInitial {
        sent.iter()
            .find_map(|(_, control)| match control {
                MediaControl::OpenInitial(initial) => Some(initial),
                _ => None,
            })
            .expect("a complete startup policy")
    }

    fn acknowledge(
        broker: &mut MediaBroker,
        client: u64,
        session: &str,
        id: u64,
        pair: MediaIntent,
    ) {
        run(broker.client_control(
            client,
            MediaControl::Applied(MediaApplied {
                session_id: session.to_owned(),
                control_id: id,
                input_muted: pair.input_muted,
                output_suppressed: pair.output_suppressed,
            }),
            Instant::now(),
        ));
    }

    // Keep: #316 privacy regression and the named-JSON/client startup trust boundary.
    #[test]
    fn media_intent_survives_device_changed_and_requires_fresh_authorized_open() {
        for pair in [
            MediaIntent {
                input_muted: true,
                output_suppressed: false,
            },
            MediaIntent {
                input_muted: false,
                output_suppressed: true,
            },
            MediaIntent {
                input_muted: true,
                output_suppressed: true,
            },
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            let old = live_call(&mut broker, 1, pane(7), now);
            let dispatch = broker
                .dispatch_control(
                    &old,
                    Some(pair.input_muted),
                    Some(pair.output_suppressed),
                    None,
                    now,
                )
                .unwrap();
            run(dispatch.actions);
            acknowledge(&mut broker, 1, &old, dispatch.control_id, pair);
            assert_eq!(broker.state(&old).unwrap().muted, pair.input_muted);
            assert_eq!(
                broker.sessions[&old].output_suppressed,
                pair.output_suppressed
            );
            run(broker.client_control(
                1,
                MediaControl::Close(MediaClose::new(&old, close_code::DEVICE_CHANGED, "renew")),
                now,
            ));
            assert_eq!(broker.intent(pane(7)), pair);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
            let (sent, rx) = open(&mut broker, pane(7), later);
            assert!(
                sent.is_empty(),
                "retained policy is not input authorization"
            );
            assert_eq!(response(&rx)["error"]["code"], error_code::NO_CLIENT);
            broker.note_pane_input(2, pane(7), "p_7", later);
            let (sent, _rx) = open(&mut broker, pane(7), later);
            let initial = initial_control(&sent);
            assert_eq!(
                (initial.input_muted, initial.output_suppressed),
                (pair.input_muted, pair.output_suppressed)
            );
            assert_eq!(
                sent.len(),
                1,
                "protected startup has no v1 or Mute downgrade"
            );
            assert_eq!(sent[0].0, 2, "pane policy survives owner changes");
            let ServerMessage::EndpointControl { kind, data } = sent[0].1.server_message().unwrap()
            else {
                panic!("startup must use the existing endpoint envelope");
            };
            assert_eq!(
                MediaControl::decode(&kind, &data),
                Some(Ok(MediaControl::OpenInitial(initial.clone())))
            );
            let current = &broker.sessions[&initial.session_id];
            assert_eq!((current.muted, current.output_suppressed), (false, false));
            assert!(
                broker.applied(&initial.session_id).is_none(),
                "request is not evidence"
            );
            assert_eq!(broker.intent(pane(8)), MediaIntent::default());
        }
    }

    #[test]
    fn media_open_initial_compatibility_alternate_is_exact_and_only_unprotected() {
        for (input_muted, output_suppressed) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let old = live_call(&mut broker, 1, pane(7), now);
            run(broker
                .dispatch_control(&old, Some(input_muted), Some(output_suppressed), None, now)
                .unwrap()
                .actions);
            let (sent, _rx) = open(&mut broker, pane(7), now);
            let position = sent
                .iter()
                .position(|(_, control)| matches!(control, MediaControl::OpenInitial(_)))
                .unwrap();
            let initial = initial_control(&sent);
            assert_eq!(
                (initial.input_muted, initial.output_suppressed),
                (input_muted, output_suppressed)
            );
            assert!(sent
                .iter()
                .all(|(_, control)| !matches!(control, MediaControl::Mute(_))));
            if !input_muted && !output_suppressed {
                assert!(
                    matches!(&sent[position + 1..], [(1, MediaControl::Open(open))]
                    if open.session_id == initial.session_id && open.pane_id == initial.pane_id)
                );
            } else {
                assert_eq!(position + 1, sent.len());
            }
        }
    }

    #[test]
    fn media_explicit_false_survives_stale_applied_and_legacy_state() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let protected = MediaIntent {
            input_muted: true,
            output_suppressed: true,
        };
        let first = broker
            .dispatch_control(&id, Some(true), Some(true), None, now)
            .unwrap();
        run(first.actions);
        let second = broker
            .dispatch_control(&id, Some(false), None, None, now)
            .unwrap();
        run(second.actions);
        let output_only = MediaIntent {
            input_muted: false,
            output_suppressed: true,
        };
        acknowledge(&mut broker, 1, &id, second.control_id, output_only);
        acknowledge(&mut broker, 1, &id, first.control_id, protected);
        assert_eq!(broker.intent(pane(7)), output_only);
        assert!(!broker.sessions[&id].muted);
        assert!(broker.sessions[&id].output_suppressed);
        assert!(
            broker.sessions[&id].outstanding.is_empty(),
            "old recognized terminal still releases capacity"
        );
        assert_eq!(broker.applied(&id).unwrap().control_id, second.control_id);
        run(broker.mute(&id, true).unwrap());
        assert_eq!(
            broker.intent(pane(7)),
            protected,
            "legacy Mute shares the recorder"
        );
        run(broker.mute(&id, false).unwrap());
        assert_eq!(
            broker.intent(pane(7)),
            output_only,
            "legacy explicit false is authoritative"
        );
        run(broker
            .dispatch_control(&id, None, Some(false), None, now)
            .unwrap()
            .actions);
        run(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: id.clone(),
                state: MediaPeerState::Connected,
                muted: true,
                detail: None,
            }),
            now,
        ));
        assert_eq!(
            broker.intent(pane(7)),
            MediaIntent::default(),
            "legacy observation never relatches intent"
        );
        run(broker.close(&id, now));
        let (sent, _rx) = open(&mut broker, pane(7), now);
        let initial = initial_control(&sent);
        assert!(!initial.input_muted && !initial.output_suppressed);
        assert!(matches!(
            &sent[..],
            [
                (1, MediaControl::OpenInitial(_)),
                (1, MediaControl::Open(_))
            ]
        ));
    }

    #[test]
    fn media_all_finish_reasons_retain_intent_but_dead_panes_prune_without_sessions() {
        let reasons = [
            close_code::CLOSED,
            close_code::DEVICE_CHANGED,
            close_code::DEVICE_ERROR,
            close_code::DISCONNECTED,
            close_code::DECLINED,
            close_code::TIMEOUT,
            close_code::REPLACED,
            close_code::DISABLED,
            close_code::UNSUPPORTED,
        ];
        for reason in reasons {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let id = live_call(&mut broker, 1, pane(7), now);
            run(broker
                .dispatch_control(&id, Some(true), Some(true), None, now)
                .unwrap()
                .actions);
            if reason == close_code::CLOSED {
                run(broker.close(&id, now));
            } else if reason == close_code::DISCONNECTED {
                run(broker.client_removed(1, now));
            } else {
                run(broker.client_control(
                    1,
                    MediaControl::Close(MediaClose::new(&id, reason, "ended")),
                    now,
                ));
            }
            assert!(!broker.sessions.contains_key(&id));
            assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
            run(broker.expire(now + CLOSED_SESSION_RETENTION, |_| true));
            assert!(broker.closed.is_empty());
            broker.pane_owners.clear();
            assert!(broker.owner_cleanup_due(now + CLOSED_SESSION_RETENTION));
            assert!(
                broker.intent(pane(7)).input_muted,
                "session retention is not policy expiry"
            );
            run(broker.expire(now + CLOSED_SESSION_RETENTION, |_| false));
            assert_eq!(
                broker.intent(pane(7)),
                MediaIntent::default(),
                "prune before empty-session return"
            );
        }
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        run(broker.mute(&id, true).unwrap());
        run(broker.close(&id, now));
        broker.pane_owners.clear();
        broker.retain_live_panes(now, |_| false);
        assert_eq!(broker.intent(pane(7)), MediaIntent::default());
    }

    // Keep: bounded admission and atomic fanout are a privacy/data-safety failure path.
    #[test]
    fn media_control_dispatch_records_intent_atomically() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let old = live_call(&mut broker, 1, pane(7), now);
        let other = live_call(&mut broker, 2, pane(8), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, _rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        // Even a captured source receipt on a foreign owner/pane grants no fanout.
        broker
            .sessions
            .get_mut(&other)
            .unwrap()
            .replaces
            .push(ReplacedSession {
                session_id: old.clone(),
                authorizes_renewal: true,
                pane: pane(7),
                client_id: 1,
                open_id: None,
            });
        let mut ids = Vec::new();
        for _ in 0..MEDIA_DEVICE_CONTROL_CAPACITY {
            let dispatch = broker
                .dispatch_control(&successor, None, None, Some(false), later)
                .unwrap();
            assert_eq!(dispatch.session_ids, vec![successor.clone()]);
            ids.push(dispatch.control_id);
            run(dispatch.actions);
        }
        let next = broker.next_control_id;
        let before_intents = broker.pane_intents.clone();
        let before_old = broker.sessions[&old].outstanding.clone();
        let before_successor = broker.sessions[&successor].outstanding.clone();
        assert!(matches!(
            broker.dispatch_control(&old, Some(true), Some(true), Some(true), later),
            Err((error_code::CONTROL_FAILED, _))
        ));
        assert_eq!(broker.next_control_id, next);
        assert_eq!(broker.pane_intents, before_intents);
        assert_eq!(broker.sessions[&old].outstanding, before_old);
        assert_eq!(broker.sessions[&successor].outstanding, before_successor);
        for session in broker.sessions.values() {
            assert!(!session.wants_muted && !session.wants_output_suppressed);
        }
        acknowledge(&mut broker, 1, &successor, ids[0], MediaIntent::default());
        let dispatch = broker
            .dispatch_control(&old, Some(true), Some(true), Some(true), later)
            .unwrap();
        assert_eq!(dispatch.control_id, next, "rejection did not consume an ID");
        assert_eq!(dispatch.session_ids.len(), 2);
        let sent = run(dispatch.actions);
        assert_eq!(sent.len(), 2);
        for (client, control) in sent {
            assert_eq!(client, 1);
            let MediaControl::Control(control) = control else {
                panic!("only device controls")
            };
            assert!(control.session_id == old || control.session_id == successor);
            assert_eq!(
                control.levels,
                if control.session_id == old {
                    Some(true)
                } else {
                    None
                }
            );
            assert!(broker.sessions[&control.session_id]
                .outstanding
                .contains_key(&control.control_id));
        }
        assert_eq!(broker.intent(pane(8)), MediaIntent::default());
        assert!(broker.sessions[&other].outstanding.is_empty());
    }

    #[test]
    fn media_control_fanout_never_crosses_pane_or_owner_independently() {
        for same_pane in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            let source = live_call(&mut broker, 1, pane(7), now);
            let unrelated = live_call(&mut broker, 2, pane(8), now);
            // Minimal adversarial lineage: one dimension matches, the other must still
            // independently deny fanout. A raw replaces string is not authority.
            let session = broker.sessions.get_mut(&unrelated).unwrap();
            session.replaces.push(ReplacedSession {
                session_id: source.clone(),
                authorizes_renewal: true,
                pane: pane(7),
                client_id: 1,
                open_id: None,
            });
            session.state = MediaSessionState::Opening;
            let (tx, _rx) = mpsc::channel();
            session.pending = Some(PendingOpen {
                request_id: "unrelated".into(),
                respond_to: tx,
                deadline: now + MEDIA_OPEN_TIMEOUT,
            });
            if same_pane {
                session.pane = pane(7);
            } else {
                session.client_id = 1;
            }
            let dispatch = broker
                .dispatch_control(&source, Some(true), Some(true), Some(true), now)
                .unwrap();
            assert_eq!(dispatch.session_ids, vec![source.clone()]);
            let sent = run(dispatch.actions);
            assert_eq!(sent.len(), 1);
            assert!(!broker.sessions[&unrelated].wants_muted);
            assert!(!broker.sessions[&unrelated].wants_output_suppressed);
            assert!(broker.sessions[&unrelated].outstanding.is_empty());
        }
    }

    #[test]
    fn media_intent_survives_actual_pending_timeout_and_fresh_replacement() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, id, _) = opened_session(&sent);
        run(broker
            .dispatch_control(&id, Some(true), Some(true), None, now)
            .unwrap()
            .actions);
        run(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true));
        assert_eq!(response(&rx)["error"]["code"], error_code::TIMEOUT);
        assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
        broker.note_pane_input(1, pane(7), "p_7", now + MEDIA_OPEN_TIMEOUT);
        let (sent, _rx) = open(&mut broker, pane(7), now + MEDIA_OPEN_TIMEOUT);
        let first = initial_control(&sent).session_id.clone();
        let (sent, _rx) = open(&mut broker, pane(7), now + MEDIA_OPEN_TIMEOUT);
        assert_eq!(
            broker.state(&first).unwrap().code.as_deref(),
            Some(close_code::REPLACED)
        );
        assert!(initial_control(&sent).input_muted && initial_control(&sent).output_suppressed);
        let successor = initial_control(&sent).session_id.clone();
        let before = broker.next_control_id;
        assert!(matches!(
            broker.dispatch_control(
                &successor,
                Some(false),
                Some(false),
                None,
                now + MEDIA_OPEN_TIMEOUT * 2
            ),
            Err((error_code::SESSION_NOT_FOUND, _))
        ));
        assert_eq!(broker.next_control_id, before);
        assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
    }

    #[test]
    fn media_control_identifier_exhaustion_and_missing_target_do_not_mutate_intent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        broker.next_control_id = u64::MAX;
        assert!(matches!(
            broker.dispatch_control(&id, Some(true), Some(true), None, now),
            Err((error_code::CONTROL_FAILED, _))
        ));
        assert_eq!(broker.next_control_id, u64::MAX);
        assert!(broker.pane_intents.is_empty());
        let session = &broker.sessions[&id];
        assert!(!session.wants_muted && !session.wants_output_suppressed);
        assert!(session.outstanding.is_empty());
        assert!(broker
            .record_control_intent("missing", Some(true), Some(true))
            .is_err());
        assert!(broker.pane_intents.is_empty());
        assert!(broker
            .dispatch_control("missing", Some(true), None, None, now)
            .is_err());
        assert!(broker.mute("missing", true).is_err());
        assert!(broker.pane_intents.is_empty());
    }

    #[test]
    fn media_applied_requires_known_pair_and_terminal_failures_cannot_revive() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let pair = MediaIntent {
            input_muted: true,
            output_suppressed: false,
        };
        acknowledge(&mut broker, 1, &id, 999, pair);
        assert!(broker.applied(&id).is_none(), "unknown ACK is not evidence");
        let first = broker
            .dispatch_control(&id, Some(true), None, None, now)
            .unwrap();
        run(first.actions);
        acknowledge(
            &mut broker,
            1,
            &id,
            first.control_id,
            MediaIntent::default(),
        );
        acknowledge(&mut broker, 2, &id, first.control_id, pair);
        assert!(broker.applied(&id).is_none());
        assert!(broker.sessions[&id]
            .outstanding
            .contains_key(&first.control_id));
        acknowledge(&mut broker, 1, &id, first.control_id, pair);
        assert!(broker.sessions[&id].muted);
        assert!(broker.sessions[&id].outstanding.is_empty());
        let second = broker
            .dispatch_control(&id, Some(false), Some(true), None, now)
            .unwrap();
        run(second.actions);
        let failure = MediaControlFailed {
            session_id: id.clone(),
            control_id: Some(second.control_id),
            code: "control_timeout".into(),
            message: None,
        };
        run(broker.client_control(1, MediaControl::ControlFailed(failure.clone()), now));
        assert_eq!(broker.control_failed(&id), Some(&failure));
        acknowledge(
            &mut broker,
            1,
            &id,
            second.control_id,
            MediaIntent {
                input_muted: false,
                output_suppressed: true,
            },
        );
        assert_eq!(broker.applied(&id).unwrap().control_id, first.control_id);
        assert!(
            broker.sessions[&id].muted,
            "failure cannot fabricate observation"
        );
        assert_eq!(
            broker.intent(pane(7)),
            MediaIntent {
                input_muted: false,
                output_suppressed: true
            }
        );
        let third = broker.dispatch_control(&id, None, None, None, now).unwrap();
        run(third.actions);
        let mut legacy = failure.clone();
        legacy.control_id = None;
        run(broker.client_control(1, MediaControl::ControlFailed(legacy), now));
        assert!(broker.sessions[&id]
            .outstanding
            .contains_key(&third.control_id));
        let mut unknown = failure.clone();
        unknown.control_id = Some(999);
        unknown.code = "awaiting_consent".into();
        run(broker.client_control(1, MediaControl::ControlFailed(unknown), now));
        let mut duplicate = failure.clone();
        duplicate.code = "awaiting_consent".into();
        run(broker.client_control(1, MediaControl::ControlFailed(duplicate), now));
        assert_eq!(broker.control_failed(&id), Some(&failure));
        run(broker.close(&id, now));
        acknowledge(
            &mut broker,
            1,
            &id,
            third.control_id,
            MediaIntent {
                input_muted: false,
                output_suppressed: true,
            },
        );
        assert!(broker.applied(&id).is_none());
        assert_eq!(broker.state(&id).unwrap().state, MediaSessionState::Closed);
    }

    #[test]
    fn media_older_failure_settles_record_without_overwriting_newer_terminal_snapshot() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let first = broker
            .dispatch_control(&id, Some(true), None, None, now)
            .unwrap();
        run(first.actions);
        let second = broker
            .dispatch_control(&id, Some(false), None, None, now)
            .unwrap();
        run(second.actions);
        acknowledge(
            &mut broker,
            1,
            &id,
            second.control_id,
            MediaIntent::default(),
        );
        run(broker.client_control(
            1,
            MediaControl::ControlFailed(MediaControlFailed {
                session_id: id.clone(),
                control_id: Some(first.control_id),
                code: "control_timeout".into(),
                message: None,
            }),
            now,
        ));
        assert!(broker.sessions[&id].outstanding.is_empty());
        assert!(broker.control_failed(&id).is_none());
        assert_eq!(broker.applied(&id).unwrap().control_id, second.control_id);
        acknowledge(
            &mut broker,
            1,
            &id,
            first.control_id,
            MediaIntent {
                input_muted: true,
                output_suppressed: false,
            },
        );
        assert!(!broker.state(&id).unwrap().muted);
    }

    #[test]
    fn media_control_alias_is_pending_authorized_same_pane_and_cannot_enable_levels() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let old = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        run(broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(&old, close_code::REPLACED, "renewal")),
            later,
        ));
        let before = broker.next_control_id;
        assert!(matches!(
            broker.dispatch_control(&old, Some(true), None, Some(false), later),
            Err((error_code::NOT_READY, _))
        ));
        assert_eq!(broker.next_control_id, before);
        assert_eq!(broker.intent(pane(7)), MediaIntent::default());
        let dispatch = broker
            .dispatch_control(&old, Some(true), Some(true), None, later)
            .unwrap();
        assert_eq!(dispatch.session_ids, vec![successor.clone()]);
        run(dispatch.actions);
        assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
        let sent = run(broker.mute(&old, false).unwrap());
        assert!(
            matches!(&sent[..], [(1, MediaControl::Mute(mute))] if mute.session_id == successor && !mute.muted)
        );
        run(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: successor.clone(),
                sdp: "v=0".into(),
            }),
            later,
        ));
        assert_eq!(response(&rx)["result"]["session_id"], successor);
        assert!(broker.mute(&old, true).is_err(), "offer consumed the alias");
        assert!(broker
            .dispatch_control(&old, Some(true), None, None, later)
            .is_err());
    }

    #[test]
    fn media_control_delivery_failure_is_owned_idempotent_and_never_rolls_back_intent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let other = live_call(&mut broker, 2, pane(8), now);
        let dispatch = broker
            .dispatch_control(&id, Some(true), Some(true), None, now)
            .unwrap();
        run(dispatch.actions);
        assert!(broker.control_delivery_failed(2, &id, now).is_empty());
        assert!(broker.sessions.contains_key(&id));
        let sent = run(broker.control_delivery_failed(1, &id, now));
        assert!(
            sent.iter().all(|(_, control)| control.session_id() != id),
            "never notify the failed target"
        );
        assert_eq!(
            broker.state(&id).unwrap().code.as_deref(),
            Some(close_code::DISCONNECTED)
        );
        assert!(broker.control_delivery_failed(1, &id, now).is_empty());
        assert!(broker.sessions.contains_key(&other));
        assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
        acknowledge(
            &mut broker,
            1,
            &id,
            dispatch.control_id,
            MediaIntent {
                input_muted: true,
                output_suppressed: true,
            },
        );
        assert!(broker.applied(&id).is_none());
        // Even an equal reused session string owned by another client cannot be ended.
        let replacement = broker.sessions.remove(&other).unwrap();
        broker.sessions.insert(id.clone(), replacement);
        assert!(broker.control_delivery_failed(1, &id, now).is_empty());
        assert_eq!(broker.sessions[&id].client_id, 2);
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
            let connected_at = later + MEDIA_CONNECT_TIMEOUT - Duration::from_nanos(1);
            assert!(run(broker.expire(connected_at, |_| true)).is_empty());
            peer_state(
                &mut broker,
                &session_id,
                MediaPeerState::Connected,
                connected_at,
            );
            assert!(run(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true)).is_empty());
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
        assert!(
            matches!(&sent[..], [(1, MediaControl::OpenInitial(initial))]
            if initial.session_id == renewal && initial.input_muted && !initial.output_suppressed)
        );
        assert!(initial_control(&sent).input_muted);
        assert_eq!(initial_control(&sent).session_id, renewal);
        assert!(sent.iter().all(|(_, control)| !matches!(
            control,
            MediaControl::Mute(_) | MediaControl::Control(_)
        )));
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
            Some(offered_at + MEDIA_CONNECT_TIMEOUT),
            "the answer starts the connection deadline"
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
                assert_eq!(
                    broker.next_deadline(wake),
                    Some(wake + MEDIA_CONNECT_TIMEOUT)
                );
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

    // Keep reasons: authenticated device boundary, microphone intent safety, and
    // deterministic deadline/disconnect races that a physical-device flow cannot
    // reach cheaply. The parent also exercises the socket/subscriber boundary.
    fn observe(actions: Vec<MediaAction>) -> (SentControls, Vec<SubscriptionEventEnvelope>) {
        let mut sent = Vec::new();
        let mut events = Vec::new();
        for action in actions {
            match action {
                MediaAction::Publish { event, .. } => events.push(event),
                MediaAction::Send { client_id, control } => sent.push((client_id, control)),
                MediaAction::Respond {
                    respond_to,
                    response,
                    ..
                } => {
                    let _ = respond_to.send(response);
                }
            }
        }
        (sent, events)
    }

    fn device_request(
        broker: &mut MediaBroker,
        session_id: &str,
        request_id: &str,
        input: Option<bool>,
        output: Option<bool>,
        now: Instant,
    ) -> (SentControls, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let sent = run(broker.control(request_id.into(), tx, session_id, input, output, now));
        (sent, rx)
    }

    fn device_id(sent: &SentControls, session_id: &str) -> u64 {
        sent.iter()
            .find_map(|(_, control)| match control {
                MediaControl::Control(control) if control.session_id == session_id => {
                    Some(control.control_id)
                }
                _ => None,
            })
            .expect("device control")
    }

    fn applied(session_id: &str, control_id: u64, input: bool, output: bool) -> MediaControl {
        MediaControl::Applied(MediaApplied {
            session_id: session_id.into(),
            control_id,
            input_muted: input,
            output_suppressed: output,
        })
    }

    fn helper_info(session_id: &str) -> MediaControl {
        MediaControl::Info(MediaInfo {
            session_id: session_id.into(),
            version: "0.1".into(),
            commit: "abc".into(),
            pid: 42,
            input_device: "mic".into(),
            output_device: "speaker".into(),
        })
    }

    fn connected(session_id: &str) -> MediaControl {
        MediaControl::State(MediaStateUpdate {
            session_id: session_id.into(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        })
    }

    #[test]
    fn acknowledged_control_is_held_before_answer_and_echoes_outer_request_identity() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, offer_rx) = open(&mut broker, pane(7), now);
        let (_, id, _) = opened_session(&sent);
        send_offer(&mut broker, &id, now);
        let _ = response(&offer_rx);
        let (sent, rx) = device_request(
            &mut broker,
            &id,
            "outer-api-id",
            Some(true),
            Some(true),
            now,
        );
        let control_id = device_id(&sent, &id);
        assert!(rx.try_recv().is_err());
        assert_eq!(broker.state(&id).unwrap().state, MediaSessionState::Offered);
        assert_eq!(broker.next_deadline(now), Some(now + MEDIA_CONTROL_TIMEOUT));
        let actions = broker.client_control(1, applied(&id, control_id, true, true), now);
        assert!(actions.iter().any(|action| matches!(
            action,
            MediaAction::Respond {
                session_id: None,
                ..
            }
        )));
        let (_, events) = observe(actions);
        let value = response(&rx);
        assert_eq!(value["id"], "outer-api-id");
        assert_eq!(
            value["result"],
            serde_json::json!({
                "type": "media_controls_applied", "session_id": id, "request_id": "outer-api-id",
                "input_muted": true, "output_suppressed": true, "epoch": 1,
            })
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].data, SubscriptionEventData::MediaControlsApplied(event)
            if event.request_id == "outer-api-id" && event.epoch == 1 && event.pane_id == "p_7")
        );
        assert!(broker.sessions[&id].offered_deadline.is_some());
        assert!(broker.state(&id).unwrap().muted);
    }

    #[test]
    fn control_errors_are_actions_and_close_preserves_cause_without_abandonment_marker() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        let (_, rx) = device_request(&mut broker, "missing", "unknown", Some(true), None, now);
        let value = response(&rx);
        assert_eq!(value["error"]["code"], error_code::SESSION_NOT_FOUND);
        assert!(value["error"].get("close_code").is_none());
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, offer_rx) = open(&mut broker, pane(7), now);
        let (_, id, _) = opened_session(&sent);
        let (_, rx) = device_request(&mut broker, &id, "opening", Some(true), None, now);
        assert_eq!(
            response(&rx)["error"]["code"],
            error_code::SESSION_NOT_FOUND
        );
        assert!(!broker.sessions[&id].wants_muted);
        send_offer(&mut broker, &id, now);
        let _ = response(&offer_rx);
        let (_, rx) = device_request(&mut broker, &id, "pending", Some(true), None, now);
        let actions = broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(
                &id,
                close_code::DEVICE_ERROR,
                "device disconnected",
            )),
            now,
        );
        assert!(actions
            .iter()
            .filter_map(|action| match action {
                MediaAction::Respond { session_id, .. } => Some(session_id),
                _ => None,
            })
            .all(Option::is_none));
        let (_, events) = observe(actions);
        let value = response(&rx);
        assert_eq!(value["error"]["code"], error_code::CONTROL_FAILED);
        assert_eq!(value["error"]["close_code"], close_code::DEVICE_ERROR);
        assert_eq!(
            value["error"]["message"],
            "device_error: device disconnected"
        );
        assert_eq!(
            events.iter().map(|event| event.event).collect::<Vec<_>>(),
            [
                SubscriptionEventKind::MediaError,
                SubscriptionEventKind::MediaStopped
            ]
        );
        assert_eq!(
            broker.state(&id).unwrap().code.as_deref(),
            Some(close_code::DEVICE_ERROR)
        );
        let (_, rx) = device_request(&mut broker, &id, "closed", None, Some(true), now);
        let value = response(&rx);
        assert_eq!(value["error"]["code"], error_code::SESSION_NOT_FOUND);
        assert_eq!(value["error"]["close_code"], close_code::DEVICE_ERROR);
        assert!(broker.client_control(1, helper_info(&id), now).is_empty());
        assert!(broker.close(&id, now).is_empty());
    }

    #[test]
    fn ordered_controls_ignore_wrong_client_unsolicited_and_stale_acks_without_losing_intent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (first, first_rx) =
            device_request(&mut broker, &id, "first", Some(true), Some(true), now);
        let (second, second_rx) =
            device_request(&mut broker, &id, "second", Some(false), Some(false), now);
        let first = device_id(&first, &id);
        let second = device_id(&second, &id);
        assert!(second > first);
        assert!(broker
            .client_control(2, applied(&id, second, false, false), now)
            .is_empty());
        assert!(broker
            .client_control(1, applied(&id, second + 100, false, false), now)
            .is_empty());
        assert!(broker.applied(&id).is_none());
        run(broker.client_control(1, applied(&id, first, true, true), now));
        assert_eq!(response(&first_rx)["id"], "first");
        assert!(second_rx.try_recv().is_err());
        assert!(!broker.sessions[&id].wants_muted);
        assert!(!broker.sessions[&id].wants_output_suppressed);
        run(broker.client_control(1, applied(&id, second, false, false), now));
        assert_eq!(response(&second_rx)["id"], "second");
        assert!(broker
            .client_control(1, applied(&id, first, true, true), now)
            .is_empty());
        assert!(!broker.state(&id).unwrap().muted);
        assert_eq!(broker.applied(&id).unwrap().control_id, second);
        assert_eq!(broker.intent(pane(7)), MediaIntent::default());
    }

    #[test]
    fn control_timeout_keeps_late_ack_correlation_without_response_resurrection() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (sent, rx) = device_request(&mut broker, &id, "timed-out", Some(true), Some(true), now);
        let old = device_id(&sent, &id);
        let before = now + MEDIA_CONTROL_TIMEOUT - Duration::from_nanos(1);
        assert!(broker.expire(before, |_| true).is_empty());

        let (sent, new_rx) =
            device_request(&mut broker, &id, "new", Some(false), Some(false), before);
        let new = device_id(&sent, &id);
        let (_, events) =
            observe(broker.client_control(1, applied(&id, new, false, false), before));
        assert!(matches!(&events[..], [event] if matches!(
            &event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "new"
                    && data.session_id == id
                    && !data.input_muted
                    && !data.output_suppressed
                    && data.epoch == 1
        )));
        assert_eq!(response(&new_rx)["id"], "new");
        assert_eq!(broker.applied(&id).unwrap().control_id, new);
        assert!(!broker.state(&id).unwrap().muted);

        let at = now + MEDIA_CONTROL_TIMEOUT;
        let (sent, events) = observe(broker.expire(at, |_| true));
        assert!(sent.is_empty());
        assert!(events.is_empty());
        let value = response(&rx);
        assert_eq!(value["error"]["code"], error_code::TIMEOUT);
        assert!(value["error"].get("close_code").is_none());
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("uncertain"));
        assert_eq!(
            broker.state(&id).unwrap().state,
            MediaSessionState::Connected
        );

        let (_, events) = observe(broker.client_control(2, applied(&id, old, true, true), at));
        assert!(events.is_empty(), "wrong client cannot publish");
        let (_, events) =
            observe(broker.client_control(1, applied(&id, old + 100, true, true), at));
        assert!(events.is_empty(), "unknown CID cannot publish");
        let (_, events) = observe(broker.client_control(1, applied(&id, old, false, true), at));
        assert!(events.is_empty(), "wrong pair cannot publish");
        assert!(rx.try_recv().is_err());

        let (_, events) = observe(broker.client_control(1, applied(&id, old, true, true), at));
        assert!(matches!(&events[..], [event] if matches!(
            &event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "timed-out"
                    && data.session_id == id
                    && data.input_muted
                    && data.output_suppressed
                    && data.epoch == 1
        )));
        assert!(
            rx.try_recv().is_err(),
            "late ACK cannot send a second response"
        );
        assert_eq!(broker.applied(&id).unwrap().control_id, new);
        assert!(!broker.state(&id).unwrap().muted);
        assert!(!broker.sessions[&id].wants_muted);
        assert!(!broker.sessions[&id].wants_output_suppressed);
        assert!(broker
            .client_control(1, applied(&id, old, true, true), at)
            .is_empty());

        let (sent, false_rx) = device_request(
            &mut broker,
            &id,
            "explicit-false",
            Some(false),
            Some(false),
            at,
        );
        let explicit_false = device_id(&sent, &id);
        let (_, events) =
            observe(broker.client_control(1, applied(&id, explicit_false, false, false), at));
        assert!(matches!(&events[..], [event] if matches!(
            &event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "explicit-false"
                    && !data.input_muted
                    && !data.output_suppressed
        )));
        let value = response(&false_rx);
        assert_eq!(value["result"]["input_muted"], false);
        assert_eq!(value["result"]["output_suppressed"], false);
    }

    #[test]
    fn late_successor_ack_keeps_fanout_event_identity_after_named_timeout() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, _offer_rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        let (sent, source_rx) = device_request(
            &mut broker,
            &source,
            "fanout-timeout",
            Some(false),
            Some(false),
            later,
        );
        let source_cid = device_id(&sent, &source);
        let successor_cid = device_id(&sent, &successor);
        assert_eq!(source_cid, successor_cid);

        let at = later + MEDIA_CONTROL_TIMEOUT;
        let (sent, events) = observe(broker.expire(at, |_| true));
        assert!(sent.is_empty());
        assert!(events.is_empty());
        assert_eq!(response(&source_rx)["error"]["code"], error_code::TIMEOUT);

        let (_, events) =
            observe(broker.client_control(1, applied(&successor, successor_cid, false, false), at));
        assert!(matches!(&events[..], [event] if matches!(
            &event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "fanout-timeout"
                    && data.session_id == successor
                    && data.epoch == 2
                    && !data.input_muted
                    && !data.output_suppressed
        )));
        assert!(source_rx.try_recv().is_err());

        let (_, events) =
            observe(broker.client_control(1, applied(&source, source_cid, false, false), at));
        assert!(matches!(&events[..], [event] if matches!(
            &event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "fanout-timeout"
                    && data.session_id == source
                    && data.epoch == 1
                    && !data.input_muted
                    && !data.output_suppressed
        )));
        assert!(source_rx.try_recv().is_err());
        assert_eq!(
            broker.state(&source).unwrap().state,
            MediaSessionState::Connected
        );
        assert!(broker
            .client_control(1, applied(&successor, successor_cid, false, false), at)
            .is_empty());
        assert!(broker
            .client_control(1, applied(&source, source_cid, false, false), at)
            .is_empty());
    }

    #[test]
    fn ack_at_control_deadline_times_out_even_before_maintenance_and_disconnect_does_not_abandon() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (sent, rx) = device_request(&mut broker, &id, "deadline", Some(true), None, now);
        let (_, events) = observe(broker.client_control(
            1,
            applied(&id, device_id(&sent, &id), true, false),
            now + MEDIA_CONTROL_TIMEOUT,
        ));
        let value = response(&rx);
        assert_eq!(value["id"], "deadline");
        assert_eq!(value["error"]["code"], error_code::TIMEOUT);
        assert!(value.get("result").is_none());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, SubscriptionEventKind::MediaControlsApplied);
        assert!(matches!(
            &events[0].data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.request_id == "deadline"
                    && data.session_id == id
                    && data.epoch == 1
                    && data.input_muted
                    && !data.output_suppressed
        ));
        assert!(
            rx.try_recv().is_err(),
            "late ACK cannot send a second response"
        );
        let (sent, rx) = device_request(
            &mut broker,
            &id,
            "disconnect",
            Some(false),
            None,
            now + MEDIA_CONTROL_TIMEOUT,
        );
        drop(rx);
        let actions = broker.client_control(
            1,
            applied(&id, device_id(&sent, &id), false, false),
            now + MEDIA_CONTROL_TIMEOUT,
        );
        assert!(actions.iter().any(|action| matches!(
            action,
            MediaAction::Respond {
                session_id: None,
                ..
            }
        )));
        run(actions);
        assert_eq!(
            broker.state(&id).unwrap().state,
            MediaSessionState::Connected
        );
        assert!(broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true).is_empty());
    }

    #[test]
    fn both_desired_flags_inherit_and_propagate_during_handover_without_sharing_api_waiters() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let (sent, rx) =
            device_request(&mut broker, &source, "initial", Some(true), Some(true), now);
        run(broker.client_control(
            1,
            applied(&source, device_id(&sent, &source), true, true),
            now,
        ));
        let _ = response(&rx);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, _offer_rx) = open_with_id(&mut broker, pane(7), Some("renewal"), later);
        let (_, successor, _) = opened_session(&sent);
        assert_eq!(initial_control(&sent).session_id, successor);
        assert!(initial_control(&sent).input_muted && initial_control(&sent).output_suppressed);
        assert!(sent.iter().all(|(_, control)| !matches!(
            control,
            MediaControl::Mute(_) | MediaControl::Control(_)
        )));
        let (sent, rx) = device_request(
            &mut broker,
            &source,
            "handover",
            Some(false),
            Some(false),
            later,
        );
        assert_eq!(
            sent.iter()
                .filter(|(_, control)| matches!(control, MediaControl::Control(_)))
                .count(),
            2
        );
        let inherited = device_id(&sent, &successor);
        let (_, events) =
            observe(broker.client_control(1, applied(&successor, inherited, false, false), later));
        assert!(matches!(&events[..], [event] if matches!(&event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.session_id == successor && data.request_id == "handover"
                    && data.epoch == 2 && data.pane_id == "p_7")));
        assert!(
            rx.try_recv().is_err(),
            "internal successor ack cannot settle source control"
        );
        assert!(!broker.sessions[&successor].wants_muted);
        assert!(!broker.sessions[&successor].wants_output_suppressed);
        run(broker.client_control(
            1,
            applied(&source, device_id(&sent, &source), false, false),
            later,
        ));
        assert_eq!(response(&rx)["id"], "handover");
        run(broker.close_by_open_id("renewal", later).unwrap());
        assert_eq!(
            broker.state(&source).unwrap().state,
            MediaSessionState::Connected
        );
        let before = broker.state(&source);
        assert!(broker
            .client_control(1, applied(&successor, inherited, true, true), later)
            .is_empty());
        assert_eq!(broker.state(&source), before);
    }

    #[test]
    fn pane_intent_survives_device_change_and_explicit_unmute_survives_a_late_ack_and_close() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let (_, rx) = device_request(&mut broker, &source, "mute", Some(true), Some(true), now);
        run(broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(
                &source,
                close_code::DEVICE_CHANGED,
                "default input changed",
            )),
            now,
        ));
        assert_eq!(
            response(&rx)["error"]["close_code"],
            close_code::DEVICE_CHANGED
        );
        let (sent, offer_rx) = open(&mut broker, pane(7), now);
        let (_, next, _) = opened_session(&sent);
        assert!(initial_control(&sent).input_muted && initial_control(&sent).output_suppressed);
        assert!(sent.iter().all(|(_, control)| !matches!(
            control,
            MediaControl::Mute(_) | MediaControl::Control(_)
        )));
        send_offer(&mut broker, &next, now);
        let _ = response(&offer_rx);
        // An actual older admitted control, not a fabricated startup ACK.
        let dispatch = broker
            .dispatch_control(&next, None, None, None, now)
            .unwrap();
        let inherited = dispatch.control_id;
        run(dispatch.actions);
        let (_, unmute_rx) =
            device_request(&mut broker, &next, "unmute", Some(false), Some(false), now);
        run(broker.client_control(1, applied(&next, inherited, true, true), now));
        run(broker.close(&next, now));
        assert_eq!(
            response(&unmute_rx)["error"]["code"],
            error_code::CONTROL_FAILED
        );
        let (sent, _rx) = open(&mut broker, pane(7), now);
        let (_, last, _) = opened_session(&sent);
        assert!(!broker.sessions[&last].wants_muted);
        assert!(!broker.sessions[&last].wants_output_suppressed);
        assert!(!initial_control(&sent).input_muted && !initial_control(&sent).output_suppressed);
        assert!(matches!(
            &sent[..],
            [
                (1, MediaControl::OpenInitial(_)),
                (1, MediaControl::Open(_))
            ]
        ));
    }

    #[test]
    fn levels_interest_is_pane_local_idempotent_and_inherited_by_future_and_renewal_sessions() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        assert!(broker.set_levels(pane(7), true, now).is_empty());
        assert!(broker.set_levels(pane(7), true, now).is_empty());
        let source = live_call(&mut broker, 1, pane(7), now);
        let other = live_call(&mut broker, 2, pane(8), now);
        assert_eq!(
            broker.sessions[&source].outstanding.len(),
            1,
            "future open inherited levels"
        );
        assert_eq!(broker.sessions[&source].levels_requested, Some(true));
        assert!(broker.sessions[&other].outstanding.is_empty());
        assert_eq!(broker.sessions[&other].levels_requested, Some(false));
        let sent = run(broker.set_levels(pane(7), false, now));
        assert!(matches!(&sent[..], [(1, MediaControl::Control(control))]
            if control.session_id == source && control.levels == Some(false)));
        assert!(broker.set_levels(pane(7), false, now).is_empty());
        let sent = run(broker.set_levels(pane(7), true, now));
        let internal = device_id(&sent, &source);
        let (sent, rx) = device_request(&mut broker, &source, "api", Some(true), None, now);
        run(broker.client_control(1, applied(&source, internal, false, false), now));
        assert!(
            rx.try_recv().is_err(),
            "levels id cannot settle API control"
        );
        run(broker.client_control(
            1,
            applied(&source, device_id(&sent, &source), true, false),
            now,
        ));
        let _ = response(&rx);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, _rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        assert!(sent.iter().any(
            |(_, control)| matches!(control, MediaControl::Control(control)
            if control.session_id == successor && control.levels == Some(true))
        ));
        let sent = run(broker.set_levels(pane(7), false, later));
        assert_eq!(
            sent.len(),
            2,
            "last subscriber disables source and pending renewal"
        );
        assert!(sent.iter().all(
            |(_, control)| matches!(control, MediaControl::Control(control)
            if control.session_id != other && control.levels == Some(false))
        ));
    }

    #[test]
    fn levels_publish_at_most_ten_hz_per_interested_pane_even_across_replacement() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let levels = |id: &str| {
            MediaControl::Levels(MediaLevels {
                session_id: id.into(),
                input_peak: 0.25,
                output_peak: 0.75,
            })
        };
        assert!(broker.client_control(1, levels(&source), now).is_empty());
        run(broker.set_levels(pane(8), true, now));
        assert!(broker.client_control(1, levels(&source), now).is_empty());
        assert!(
            broker.levels(&source).is_none(),
            "unsubscribed levels are dropped"
        );
        run(broker.set_levels(pane(7), true, now));
        let (_, events) = observe(broker.client_control(1, levels(&source), now));
        assert!(
            matches!(&events[..], [SubscriptionEventEnvelope { data: SubscriptionEventData::MediaLevels(event), .. }]
            if event.pane_id == "p_7" && event.input_peak == 0.25)
        );
        assert!(broker
            .client_control(1, levels(&source), now + Duration::from_millis(99))
            .is_empty());
        let (sent, _rx) = open(&mut broker, pane(7), now + Duration::from_millis(99));
        let (_, next, _) = opened_session(&sent);
        assert!(broker
            .client_control(1, levels(&next), now + Duration::from_millis(99))
            .is_empty());
        let (_, events) =
            observe(broker.client_control(1, levels(&next), now + Duration::from_millis(100)));
        assert_eq!(events.len(), 1);
        run(broker.set_levels(pane(7), false, now + Duration::from_millis(100)));
        let previous = broker.levels(&next).cloned();
        assert!(broker
            .client_control(
                1,
                MediaControl::Levels(MediaLevels {
                    session_id: next.clone(),
                    input_peak: 0.9,
                    output_peak: 0.8,
                }),
                now + Duration::from_secs(1)
            )
            .is_empty());
        assert_eq!(
            broker.levels(&next),
            previous.as_ref(),
            "unsubscribed levels cannot update telemetry"
        );
    }

    // Trust boundary: all producers must attribute the actual session pane,
    // even if the presentation alias changes or names another pane.
    #[test]
    fn all_media_producers_carry_session_raw_pane_without_changing_public_envelopes() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let raw = pane(707);
        broker.note_pane_input(1, raw, "w1:p1", now);
        let (sent, rx) = open(&mut broker, raw, now);
        let (_, id, _) = opened_session(&sent);
        send_offer(&mut broker, &id, now);
        let _ = response(&rx);
        run(broker.answer(&id, "v=0".into(), now).unwrap());
        broker.set_pane_event_id(raw, "reused-public-alias".into());
        run(broker.client_control(1, helper_info(&id), now));
        let mut actions = broker.client_control(1, connected(&id), now);
        run(broker.set_levels(raw, true, now));
        actions.extend(broker.client_control(
            1,
            MediaControl::Levels(MediaLevels {
                session_id: id.clone(),
                input_peak: 0.25,
                output_peak: 0.5,
            }),
            now,
        ));
        let (sent, _control_rx) =
            device_request(&mut broker, &id, "raw-attribution", Some(true), None, now);
        actions.extend(broker.client_control(
            1,
            applied(&id, device_id(&sent, &id), true, false),
            now,
        ));
        actions.extend(broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(
                &id,
                close_code::DEVICE_ERROR,
                "device failed",
            )),
            now,
        ));
        let events = actions
            .into_iter()
            .filter_map(|action| match action {
                MediaAction::Publish { pane_id, event } => {
                    assert_eq!(pane_id, raw);
                    let json = serde_json::to_value(&event).unwrap();
                    assert_eq!(json["data"]["pane_id"], "reused-public-alias");
                    assert_eq!(json.as_object().unwrap().len(), 2);
                    Some(event.event)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                SubscriptionEventKind::MediaStarted,
                SubscriptionEventKind::MediaLevels,
                SubscriptionEventKind::MediaControlsApplied,
                SubscriptionEventKind::MediaError,
                SubscriptionEventKind::MediaStopped,
            ]
        );
    }

    #[test]
    fn started_requires_both_connected_and_info_in_either_order_and_emits_only_once() {
        for info_first in [false, true] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, rx) = open(&mut broker, pane(7), now);
            let (_, id, _) = opened_session(&sent);
            send_offer(&mut broker, &id, now);
            let _ = response(&rx);
            assert!(broker.client_control(2, helper_info(&id), now).is_empty());
            assert!(broker.info(&id).is_none());
            run(broker.answer(&id, "v=0".into(), now).unwrap());
            let first = if info_first {
                helper_info(&id)
            } else {
                connected(&id)
            };
            assert!(broker.client_control(1, first, now).is_empty());
            let second = if info_first {
                connected(&id)
            } else {
                helper_info(&id)
            };
            let (_, events) = observe(broker.client_control(1, second, now));
            assert!(
                matches!(&events[..], [SubscriptionEventEnvelope { data: SubscriptionEventData::MediaStarted(event), .. }]
                if event.session_id == id && event.epoch == 1 && event.helper.pid == 42 && event.helper.commit == "abc"
                    && event.input_device == "mic" && event.output_device == "speaker")
            );
            assert!(broker.client_control(1, connected(&id), now).is_empty());
            assert!(broker.client_control(1, helper_info(&id), now).is_empty());
            peer_state(&mut broker, &id, MediaPeerState::Connecting, now);
            assert_eq!(
                broker.state(&id).unwrap().state,
                MediaSessionState::Connected
            );
            assert_eq!(broker.next_deadline(now), None);
        }
    }

    #[test]
    fn terminal_close_publishes_epoch_and_exact_cause_once_without_device_changed_event() {
        for code in [
            close_code::CLOSED,
            close_code::ABANDONED,
            close_code::UNANSWERED,
            close_code::TIMEOUT,
            close_code::PANE_CLOSED,
            close_code::DISCONNECTED,
            close_code::REPLACED,
            close_code::DEVICE_CHANGED,
            close_code::DEVICE_ERROR,
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            let id = live_call(&mut broker, 1, pane(7), now);
            let (_, events) = observe(broker.client_control(
                1,
                MediaControl::Close(MediaClose::new(&id, code, "exact reason")),
                now,
            ));
            let stopped = events
                .iter()
                .filter(|event| event.event == SubscriptionEventKind::MediaStopped)
                .collect::<Vec<_>>();
            assert!(
                matches!(&stopped[..], [event] if matches!(&event.data, SubscriptionEventData::MediaStopped(data)
                if data.session_id == id && data.pane_id == "p_7" && data.epoch == 1
                    && data.code == code && data.message == "exact reason"))
            );
            assert!(events.iter().all(
                |event| serde_json::to_value(event).unwrap()["event"] != "media.device_changed"
            ));
            assert_eq!(
                broker.state(&id).unwrap().message.as_deref(),
                Some("exact reason")
            );
            assert!(broker.client_control(1, helper_info(&id), now).is_empty());
            assert!(broker.close(&id, now).is_empty());
        }
    }

    #[test]
    fn failed_peer_reports_runtime_error_and_fails_held_controls_with_the_cause() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (_, rx) = device_request(&mut broker, &id, "pending", Some(true), None, now);
        let (_, events) = observe(broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: id.clone(),
                state: MediaPeerState::Failed,
                muted: true,
                detail: Some("ICE failed".into()),
            }),
            now,
        ));
        assert!(
            matches!(&events[0].data, SubscriptionEventData::MediaError(event)
            if event.code == close_code::DEVICE_ERROR && event.message == "ICE failed")
        );
        assert_eq!(events[1].event, SubscriptionEventKind::MediaStopped);
        assert_eq!(
            response(&rx)["error"]["close_code"],
            close_code::DEVICE_ERROR
        );
    }

    #[test]
    fn answer_connection_deadline_is_fifteen_seconds_not_extended_by_duplicate_answers() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, id, _) = opened_session(&sent);
        send_offer(&mut broker, &id, now);
        let _ = response(&rx);
        let answered_at = now + Duration::from_secs(1);
        run(broker.answer(&id, "v=0".into(), answered_at).unwrap());
        assert_eq!(
            broker.state(&id).unwrap().state,
            MediaSessionState::Connecting
        );
        let deadline = answered_at + Duration::from_secs(15);
        assert_eq!(broker.next_deadline(answered_at), Some(deadline));
        run(broker
            .answer(&id, "v=0 retry".into(), deadline - Duration::from_nanos(1))
            .unwrap());
        peer_state(
            &mut broker,
            &id,
            MediaPeerState::Connecting,
            deadline - Duration::from_nanos(1),
        );
        assert_eq!(broker.next_deadline(answered_at), Some(deadline));
        assert!(broker
            .expire(deadline - Duration::from_nanos(1), |_| true)
            .is_empty());
        let (_, events) = observe(broker.expire(deadline, |_| true));
        assert_eq!(
            broker.state(&id).unwrap().code.as_deref(),
            Some(close_code::NEGOTIATION_TIMEOUT)
        );
        assert!(
            matches!(&events[1].data, SubscriptionEventData::MediaStopped(event)
            if event.code == close_code::NEGOTIATION_TIMEOUT && event.epoch == 1)
        );
        assert!(broker
            .client_control(1, connected(&id), deadline)
            .is_empty());
        assert_eq!(
            broker.next_deadline(deadline),
            Some(deadline + CLOSED_SESSION_RETENTION)
        );
    }

    #[test]
    fn late_callbacks_obey_connection_deadline_before_timer_but_wrong_client_cannot_retire() {
        for kind in 0..4 {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, rx) = open(&mut broker, pane(7), now);
            let (_, id, _) = opened_session(&sent);
            send_offer(&mut broker, &id, now);
            let _ = response(&rx);
            run(broker.answer(&id, "v=0".into(), now).unwrap());
            let deadline = now + MEDIA_CONNECT_TIMEOUT;
            let control = match kind {
                0 => helper_info(&id),
                1 => connected(&id),
                2 => applied(&id, 1, true, true),
                _ => MediaControl::Levels(MediaLevels {
                    session_id: id.clone(),
                    input_peak: 0.1,
                    output_peak: 0.2,
                }),
            };
            assert!(broker
                .client_control(2, control.clone(), deadline)
                .is_empty());
            assert_eq!(
                broker.state(&id).unwrap().state,
                MediaSessionState::Connecting
            );
            let (_, events) = observe(broker.client_control(1, control, deadline));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event == SubscriptionEventKind::MediaStopped)
                    .count(),
                1
            );
            assert_eq!(
                broker.state(&id).unwrap().code.as_deref(),
                Some(close_code::NEGOTIATION_TIMEOUT)
            );
            assert!(broker.info(&id).is_none());
            assert!(broker.applied(&id).is_none());
            assert!(broker.levels(&id).is_none());
        }
    }

    #[test]
    fn epoch_guard_is_optional_non_mutating_and_rejects_closed_or_mismatched_targets() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, _rx) = open_with_id(&mut broker, pane(7), Some("handle"), now);
        let (_, id, _) = opened_session(&sent);
        let original = broker.state(&id);
        assert!(broker.check_epoch(None, None, None).is_ok());
        assert!(broker.check_epoch(Some("missing"), None, None).is_ok());
        assert!(broker.check_epoch(Some(&id), None, Some(1)).is_ok());
        assert!(broker.check_epoch(None, Some("handle"), Some(1)).is_ok());
        for (session, open, epoch) in [
            (Some(id.as_str()), None, 2),
            (None, Some("handle"), 2),
            (Some("missing"), None, 1),
            (Some(id.as_str()), Some("other"), 1),
            (None, None, 1),
        ] {
            let error = broker.check_epoch(session, open, Some(epoch)).unwrap_err();
            assert_eq!(error.0, error_code::SESSION_NOT_FOUND);
            // Runtime gates answer/control/close on this seam before dispatch.
            assert_eq!(broker.state(&id), original);
            assert!(broker.sessions[&id].pending_controls.is_empty());
            assert_eq!(broker.next_control_id, 1);
            assert!(broker.sessions[&id].outstanding.is_empty());
        }
        run(broker.close(&id, now));
        assert!(broker.check_epoch(Some(&id), None, Some(1)).is_err());
        assert_eq!(
            broker.state(&id).unwrap().code.as_deref(),
            Some(close_code::CLOSED)
        );
        assert!(broker.check_epoch(Some(&id), None, None).is_ok());
    }

    #[test]
    fn causal_code_is_available_for_api_rejection_before_deadline_maintenance() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, pane(7), "p_7", now);
        let (sent, rx) = open(&mut broker, pane(7), now);
        let (_, id, _) = opened_session(&sent);
        send_offer(&mut broker, &id, now);
        let _ = response(&rx);
        let at = now + MEDIA_ANSWER_TIMEOUT;
        assert_eq!(
            broker.close_cause(&id, at).as_deref(),
            Some(close_code::UNANSWERED)
        );
        let error = broker.answer(&id, "v=0".into(), at).unwrap_err();
        assert_eq!(error.0, error_code::REFUSED);
        assert_eq!(
            error.1,
            "unanswered: the remote peer did not answer the media offer in time"
        );
        let (_, rx) = device_request(&mut broker, &id, "stale", Some(true), Some(true), at);
        let value = response(&rx);
        assert_eq!(value["error"]["code"], error_code::SESSION_NOT_FOUND);
        assert_eq!(value["error"]["close_code"], close_code::UNANSWERED);
        assert_eq!(
            broker.close_cause(&id, at).as_deref(),
            Some(close_code::UNANSWERED)
        );
        assert!(broker.close_cause("unknown", at).is_none());
        let plain: serde_json::Value =
            serde_json::from_str(&error_response("old".into(), "code", "message")).unwrap();
        assert_eq!(
            plain,
            serde_json::json!({"id": "old", "error": {"code": "code", "message": "message"}})
        );
    }

    #[test]
    fn actual_terminal_paths_publish_stopped_once_and_settle_pending_controls() {
        for cause in [
            close_code::CLOSED,
            close_code::ABANDONED,
            close_code::TIMEOUT,
            close_code::UNANSWERED,
            close_code::PANE_CLOSED,
            close_code::DISCONNECTED,
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "client-alias", now);
            broker.set_pane_event_id(pane(7), "public:pane".into());
            let (tx, offer_rx) = mpsc::channel();
            let sent = run(broker.open("open".into(), tx, pane(7), None, None, |_| true, now));
            let (_, id, _) = opened_session(&sent);
            let control_rx = if cause != close_code::TIMEOUT {
                send_offer(&mut broker, &id, now);
                let _ = response(&offer_rx);
                let control_at = if cause == close_code::UNANSWERED {
                    now + MEDIA_ANSWER_TIMEOUT - Duration::from_secs(1)
                } else {
                    now
                };
                Some(
                    device_request(
                        &mut broker,
                        &id,
                        "pending",
                        Some(true),
                        Some(true),
                        control_at,
                    )
                    .1,
                )
            } else {
                None
            };
            let actions = match cause {
                close_code::CLOSED => broker.close(&id, now),
                close_code::ABANDONED => broker.response_delivery_failed(&id, now),
                close_code::TIMEOUT => broker.expire(now + MEDIA_OPEN_TIMEOUT, |_| true),
                close_code::UNANSWERED => broker.expire(now + MEDIA_ANSWER_TIMEOUT, |_| true),
                close_code::PANE_CLOSED => broker.expire(now, |_| false),
                close_code::DISCONNECTED => broker.client_removed(1, now),
                _ => unreachable!(),
            };
            let (_, events) = observe(actions);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event == SubscriptionEventKind::MediaStopped)
                    .count(),
                1
            );
            assert!(events.iter().any(
                |event| matches!(&event.data, SubscriptionEventData::MediaStopped(data)
                if data.code == cause && data.pane_id == "public:pane" && data.epoch == 1)
            ));
            if let Some(rx) = control_rx {
                let value = response(&rx);
                assert_eq!(value["error"]["code"], error_code::CONTROL_FAILED);
                assert_eq!(value["error"]["close_code"], cause);
            } else {
                assert_eq!(response(&offer_rx)["error"]["close_code"], cause);
            }
            assert!(broker.close(&id, now).is_empty());
        }
    }

    #[test]
    fn older_recognized_ack_settles_its_api_request_without_rewriting_newer_telemetry_or_intent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (older, older_rx) =
            device_request(&mut broker, &id, "older", Some(true), Some(true), now);
        let (newer, newer_rx) =
            device_request(&mut broker, &id, "newer", Some(false), Some(false), now);
        run(broker.client_control(1, applied(&id, device_id(&newer, &id), false, false), now));
        assert_eq!(response(&newer_rx)["id"], "newer");
        assert!(older_rx.try_recv().is_err());
        let (_, events) = observe(broker.client_control(
            1,
            applied(&id, device_id(&older, &id), true, true),
            now,
        ));
        assert_eq!(response(&older_rx)["id"], "older");
        assert!(matches!(&events[..], [event] if matches!(&event.data,
            SubscriptionEventData::MediaControlsApplied(data) if data.request_id == "older" && data.input_muted)));
        assert!(!broker.state(&id).unwrap().muted);
        assert_eq!(
            broker.applied(&id).unwrap().control_id,
            device_id(&newer, &id)
        );
        assert!(!broker.sessions[&id].wants_muted);
        assert!(!broker.sessions[&id].wants_output_suppressed);
        assert!(broker.sessions[&id].outstanding.is_empty());
        assert!(broker
            .expire(now + MEDIA_CONTROL_TIMEOUT, |_| true)
            .is_empty());
        assert!(older_rx.try_recv().is_err());
    }

    // Keep: these trust-boundary and deadline races cannot be reached cheaply with
    // physical audio; the runtime writer owns the complementary actual JSON flow.
    #[test]
    fn authority_preflight_precedes_intent_for_retired_consumed_abandoned_expired_and_foreign_targets(
    ) {
        for cause in [
            "retired",
            "consumed",
            "abandoned",
            "expired",
            "expired_source",
            "wrong_client",
            "wrong_pane",
        ] {
            let now = Instant::now();
            let mut broker = MediaBroker::new();
            broker.client_connected(1, true);
            broker.client_connected(2, true);
            let source = live_call(&mut broker, 1, pane(7), now);
            let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
            let (sent, offer_rx, delivery) = tracked_open(&mut broker, "renewal", later);
            let (_, successor, _) = opened_session(&sent);
            let target = match cause {
                "retired" => {
                    run(broker.close(&source, later));
                    source.clone()
                }
                "consumed" => {
                    send_offer(&mut broker, &successor, later);
                    let _ = response(&offer_rx);
                    run(broker.answer(&successor, "v=0".into(), later).unwrap());
                    peer_state(&mut broker, &successor, MediaPeerState::Connected, later);
                    let _ = open(&mut broker, pane(7), later);
                    source.clone()
                }
                "abandoned" => {
                    delivery.send(()).unwrap();
                    successor.clone()
                }
                "expired" => {
                    broker
                        .sessions
                        .get_mut(&successor)
                        .unwrap()
                        .pending
                        .as_mut()
                        .unwrap()
                        .deadline = now;
                    successor.clone()
                }
                "expired_source" => {
                    broker.sessions.get_mut(&source).unwrap().connect_deadline = Some(now);
                    successor.clone()
                }
                "wrong_client" => {
                    broker.sessions.get_mut(&successor).unwrap().client_id = 2;
                    successor.clone()
                }
                "wrong_pane" => {
                    broker.sessions.get_mut(&successor).unwrap().pane = pane(8);
                    successor.clone()
                }
                _ => unreachable!(),
            };
            let policies = broker.pane_intents.clone();
            let next = broker.next_control_id;
            let snapshot = |broker: &MediaBroker| {
                broker
                    .sessions
                    .iter()
                    .map(|(id, session)| {
                        (
                            id.clone(),
                            (
                                session.wants_muted,
                                session.wants_output_suppressed,
                                session.outstanding.clone(),
                            ),
                        )
                    })
                    .collect::<HashMap<_, _>>()
            };
            let before = snapshot(&broker);
            assert!(
                broker
                    .dispatch_control(&target, Some(true), Some(true), None, later)
                    .is_err(),
                "{cause}"
            );
            assert!(broker.mute(&target, true).is_err(), "{cause}");
            assert_eq!(broker.pane_intents, policies, "{cause}");
            assert_eq!(broker.next_control_id, next, "{cause}");
            assert_eq!(snapshot(&broker), before, "{cause}");
        }
    }

    #[test]
    fn named_replacement_fails_its_waiter_but_successor_ack_publishes_successor_identity_only() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, offer_rx) = open_with_id(&mut broker, pane(7), Some("successor"), later);
        let (_, successor, _) = opened_session(&sent);
        let (sent, rx) =
            device_request(&mut broker, &source, "outer", Some(true), Some(true), later);
        let cid = device_id(&sent, &source);
        assert_eq!(
            device_id(&sent, &successor),
            cid,
            "one admitted fanout, one CID"
        );
        let actions = broker.client_control(
            1,
            MediaControl::Close(MediaClose::new(&source, close_code::REPLACED, "renewing")),
            later,
        );
        assert!(actions
            .iter()
            .filter_map(|action| match action {
                MediaAction::Respond { session_id, .. } => Some(session_id),
                _ => None,
            })
            .all(Option::is_none));
        observe(actions);
        let failed = response(&rx);
        assert_eq!(failed["error"]["code"], error_code::CONTROL_FAILED);
        assert_eq!(failed["error"]["close_code"], close_code::REPLACED);
        // The source's response lifetime never becomes a successor event deadline.
        let applied_at = later + MEDIA_CONTROL_TIMEOUT;
        let (_, events) =
            observe(broker.client_control(1, applied(&successor, cid, true, true), applied_at));
        assert!(matches!(&events[..], [event] if matches!(&event.data,
            SubscriptionEventData::MediaControlsApplied(data)
                if data.session_id == successor && data.request_id == "outer"
                    && data.epoch == 2 && data.pane_id == "p_7")));
        assert!(rx.try_recv().is_err());
        assert!(broker
            .client_control(1, applied(&source, cid, true, true), applied_at)
            .is_empty());
        assert!(broker.sessions[&successor].pending_controls.is_empty());
        let next = broker.next_control_id;
        let (_, retired_rx) = device_request(
            &mut broker,
            &source,
            "retired",
            Some(false),
            None,
            applied_at,
        );
        let retired = response(&retired_rx);
        assert_eq!(retired["error"]["code"], error_code::SESSION_NOT_FOUND);
        assert_eq!(retired["error"]["close_code"], close_code::REPLACED);
        assert_eq!(broker.next_control_id, next);
        assert!(broker.intent(pane(7)).input_muted && broker.intent(pane(7)).output_suppressed);
        send_offer(&mut broker, &successor, applied_at);
        assert_eq!(response(&offer_rx)["result"]["session_id"], successor);
    }

    #[test]
    fn older_failure_settles_its_waiter_but_legacy_unknown_and_failed_ids_never_apply() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        broker.client_connected(2, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let (older, older_rx) = device_request(&mut broker, &id, "older", Some(true), None, now);
        let older = device_id(&older, &id);
        let (newer, newer_rx) = device_request(&mut broker, &id, "newer", Some(false), None, now);
        let newer = device_id(&newer, &id);
        run(broker.client_control(1, applied(&id, newer, false, false), now));
        assert_eq!(response(&newer_rx)["id"], "newer");
        let failure = |control_id| {
            MediaControl::ControlFailed(MediaControlFailed {
                session_id: id.clone(),
                control_id,
                code: "control_timeout".into(),
                message: None,
            })
        };
        assert!(broker.client_control(1, failure(None), now).is_empty());
        assert!(broker
            .client_control(2, failure(Some(older)), now)
            .is_empty());
        assert!(broker
            .client_control(1, failure(Some(newer + 100)), now)
            .is_empty());
        assert!(older_rx.try_recv().is_err());
        assert!(broker.sessions[&id].outstanding.contains_key(&older));
        let actions = broker.client_control(1, failure(Some(older)), now);
        assert!(actions.iter().all(|action| matches!(
            action,
            MediaAction::Respond {
                session_id: None,
                ..
            }
        )));
        run(actions);
        let failed = response(&older_rx);
        assert_eq!(failed["error"]["code"], error_code::CONTROL_FAILED);
        assert!(failed["error"].get("close_code").is_none());
        assert!(broker.sessions[&id].outstanding.is_empty());
        assert_eq!(broker.applied(&id).unwrap().control_id, newer);
        assert!(
            broker.sessions[&id].telemetry.control_failed.is_none(),
            "older failure cannot replace newest snapshot"
        );
        assert!(broker
            .client_control(1, applied(&id, older, true, false), now)
            .is_empty());
        assert!(broker
            .client_control(1, failure(Some(older)), now)
            .is_empty());
        assert_eq!(broker.intent(pane(7)), MediaIntent::default());
        assert!(!broker.state(&id).unwrap().muted);
        assert_eq!(
            broker.state(&id).unwrap().state,
            MediaSessionState::Connected
        );
    }

    #[test]
    fn public_timeout_does_not_release_device_credit_and_capacity_error_keeps_session_and_intent() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let id = live_call(&mut broker, 1, pane(7), now);
        let mut requests = Vec::new();
        for index in 0..MEDIA_DEVICE_CONTROL_CAPACITY {
            let (sent, rx) = device_request(
                &mut broker,
                &id,
                &format!("held-{index}"),
                Some(index % 2 == 1),
                None,
                now,
            );
            requests.push((device_id(&sent, &id), rx));
        }
        let next = broker.next_control_id;
        let intent = broker.intent(pane(7));
        assert!(broker
            .expire(
                now + MEDIA_CONTROL_TIMEOUT - Duration::from_nanos(1),
                |_| true
            )
            .is_empty());
        let actions = broker.expire(now + MEDIA_CONTROL_TIMEOUT, |_| true);
        assert_eq!(actions.len(), MEDIA_DEVICE_CONTROL_CAPACITY);
        assert!(actions.iter().all(|action| matches!(
            action,
            MediaAction::Respond {
                session_id: None,
                ..
            }
        )));
        run(actions);
        for (_, rx) in &requests {
            assert_eq!(response(rx)["error"]["code"], error_code::TIMEOUT);
        }
        assert!(broker.sessions[&id].pending_controls.is_empty());
        assert_eq!(
            broker.sessions[&id].outstanding.len(),
            MEDIA_DEVICE_CONTROL_CAPACITY
        );
        let (sent, rx) = device_request(
            &mut broker,
            &id,
            "full",
            Some(false),
            Some(true),
            now + MEDIA_CONTROL_TIMEOUT,
        );
        assert!(sent.is_empty());
        let full = response(&rx);
        assert_eq!(full["error"]["code"], error_code::CONTROL_FAILED);
        assert!(full["error"]["message"]
            .as_str()
            .unwrap()
            .contains("32 outstanding"));
        assert!(full["error"].get("close_code").is_none());
        assert_eq!(broker.next_control_id, next);
        assert_eq!(broker.intent(pane(7)), intent);
        assert_eq!(
            broker.state(&id).unwrap().state,
            MediaSessionState::Connected
        );
        let epoch = broker.sessions[&id].epoch;
        let (_, events) = observe(broker.client_control(
            1,
            applied(&id, requests[0].0, false, false),
            now + MEDIA_CONTROL_TIMEOUT,
        ));
        assert!(
            matches!(
                &events[..],
                [event]
                    if event.event == SubscriptionEventKind::MediaControlsApplied
                        && matches!(
                            &event.data,
                            SubscriptionEventData::MediaControlsApplied(data)
                                if data.request_id == "held-0"
                                    && data.session_id == id
                                    && data.epoch == epoch
                                    && !data.input_muted
                                    && !data.output_suppressed
                        )
            ),
            "late valid ACK publishes one correlated application event"
        );
        assert!(
            requests[0].1.try_recv().is_err(),
            "late ACK cannot send a second API response"
        );
        assert!(
            broker
                .client_control(
                    1,
                    applied(&id, requests[0].0, false, false),
                    now + MEDIA_CONTROL_TIMEOUT,
                )
                .is_empty(),
            "duplicate ACK cannot publish a second event"
        );
        assert_eq!(
            broker.sessions[&id].outstanding.len(),
            MEDIA_DEVICE_CONTROL_CAPACITY - 1
        );
        let (sent, rx) = device_request(
            &mut broker,
            &id,
            "recovered",
            Some(false),
            None,
            now + MEDIA_CONTROL_TIMEOUT,
        );
        assert_eq!(device_id(&sent, &id), next);
        assert!(rx.try_recv().is_err());
        assert!(requests[0].1.try_recv().is_err());
    }

    #[test]
    fn successor_delivery_failure_preserves_source_waiter_until_its_own_ack() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, offer_rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        let (sent, rx) =
            device_request(&mut broker, &source, "source-only", Some(true), None, later);
        observe(broker.control_delivery_failed(1, &successor, later));
        assert_eq!(
            response(&offer_rx)["error"]["close_code"],
            close_code::DISCONNECTED
        );
        assert_eq!(
            broker.state(&source).unwrap().state,
            MediaSessionState::Connected
        );
        assert!(
            rx.try_recv().is_err(),
            "a different target cannot fail the named waiter"
        );
        assert!(broker
            .control_delivery_failed(1, &successor, later)
            .is_empty());
        run(broker.client_control(
            1,
            applied(&source, device_id(&sent, &source), true, false),
            later,
        ));
        assert_eq!(response(&rx)["result"]["session_id"], source);
        assert!(broker.intent(pane(7)).input_muted);
    }

    #[test]
    fn rejected_or_failed_levels_off_reconciles_on_existing_events_and_renewal_defaults_off() {
        let now = Instant::now();
        let mut broker = MediaBroker::new();
        broker.client_connected(1, true);
        let source = live_call(&mut broker, 1, pane(7), now);
        let sent = run(broker.set_levels(pane(7), true, now));
        run(broker.client_control(
            1,
            applied(&source, device_id(&sent, &source), false, false),
            now,
        ));
        let mut ids = Vec::new();
        for _ in 0..MEDIA_DEVICE_CONTROL_CAPACITY {
            let dispatch = broker
                .dispatch_control(&source, None, None, None, now)
                .unwrap();
            ids.push(dispatch.control_id);
            run(dispatch.actions);
        }
        let next = broker.next_control_id;
        assert!(
            broker.set_levels(pane(7), false, now).is_empty(),
            "off cannot bypass capacity"
        );
        assert!(!broker.levels_interest.contains(&pane(7)));
        assert_eq!(broker.sessions[&source].levels_requested, Some(true));
        assert_eq!(broker.next_control_id, next);
        assert!(broker
            .client_control(
                1,
                MediaControl::Levels(MediaLevels {
                    session_id: source.clone(),
                    input_peak: 0.9,
                    output_peak: 0.8,
                }),
                now
            )
            .is_empty());
        assert!(
            broker.levels(&source).is_none(),
            "always drop unsubscribed levels"
        );
        assert!(broker
            .client_control(1, applied(&source, ids[0], false, false), now)
            .is_empty());
        let sent = run(broker.client_control(1, connected(&source), now));
        assert!(matches!(&sent[..], [(1, MediaControl::Control(control))]
            if control.session_id == source && control.levels == Some(false)));
        let off = device_id(&sent, &source);
        assert!(broker
            .client_control(
                1,
                MediaControl::ControlFailed(MediaControlFailed {
                    session_id: source.clone(),
                    control_id: Some(off),
                    code: "control_timeout".into(),
                    message: None,
                }),
                now
            )
            .is_empty());
        assert_eq!(broker.sessions[&source].levels_requested, None);
        let dispatch = broker
            .dispatch_control(&source, Some(true), None, None, now)
            .unwrap();
        let sent = run(dispatch.actions);
        assert!(matches!(&sent[..], [(1, MediaControl::Control(control))]
            if control.levels == Some(false) && control.input_muted == Some(true)));
        assert!(
            broker.client_control(1, connected(&source), now).is_empty(),
            "one retry per real event, no recursion"
        );
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        let (sent, _offer_rx) = open(&mut broker, pane(7), later);
        let (_, successor, _) = opened_session(&sent);
        assert_eq!(broker.sessions[&successor].levels_requested, Some(false));
        assert!(broker.sessions[&successor].outstanding.is_empty());
        assert!(sent.iter().all(|(_, control)| !matches!(
            control,
            MediaControl::Control(_) | MediaControl::Mute(_)
        )));
    }

    #[test]
    fn boot_generation_is_random_128_bit_and_never_reuses_a_first_session_identity() {
        let now = Instant::now();
        let mut generations = HashSet::new();
        let mut session_ids = HashSet::new();
        for _ in 0..128 {
            let mut broker = MediaBroker::new();
            let generation = broker
                .id_prefix
                .strip_prefix("media_")
                .unwrap()
                .strip_suffix('_')
                .unwrap();
            assert_eq!(generation.len(), 32);
            assert!(generation.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(generations.insert(broker.id_prefix.clone()));
            broker.client_connected(1, true);
            broker.note_pane_input(1, pane(7), "p_7", now);
            let (sent, _rx) = open(&mut broker, pane(7), now);
            let (_, id, _) = opened_session(&sent);
            assert!(session_ids.insert(id));
        }
    }
}
