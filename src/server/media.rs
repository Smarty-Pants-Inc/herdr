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
    close_code, MediaApplied, MediaClose, MediaControl, MediaControlFailed, MediaDeviceControl,
    MediaInfo, MediaLevels, MediaMute, MediaOpen, MediaOpenInitial, MediaPeerState, MediaSdp,
    MEDIA_DEVICE_CONTROL_CAPACITY, MEDIA_INPUT_WINDOW,
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

/// Runtime pane policy, retained until pane death or an explicit request changes an axis.
/// Session outcomes (including caller close) never clear this policy or renew consent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MediaIntent {
    pub(crate) input_muted: bool,
    pub(crate) output_suppressed: bool,
}

/// Broker intent acceptance only: actions are not native admission or delivery receipts.
// The optional lane-C API integration consumes this dispatch at its production seam.
#[allow(dead_code)]
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
    wants_output_suppressed: bool,
    state: MediaSessionState,
    /// Observations, never pane-policy authority. Startup requests are not observations.
    muted: bool,
    output_suppressed: bool,
    /// Expected complete pairs for accepted device controls, independent of desired state.
    outstanding: HashMap<u64, MediaIntent>,
    terminal_watermark: u64,
    pending: Option<PendingOpen>,
    telemetry: MediaTelemetry,
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
    client_id: u64,
    pane: PaneId,
    /// Only a connected source retired by replacement can authorize a pending alias.
    authorizes_renewal: bool,
    view: MediaSessionView,
}

fn store_client_media_state(session: &mut MediaSession, control: MediaControl) {
    match control {
        MediaControl::Applied(applied) => {
            let Some(expected) = session.outstanding.get(&applied.control_id) else {
                return;
            };
            if expected.input_muted != applied.input_muted
                || expected.output_suppressed != applied.output_suppressed
            {
                return;
            }
            // A recognized terminal result always releases its allowance, even when its
            // observation is older than a terminal result already recorded.
            session.outstanding.remove(&applied.control_id);
            if applied.control_id > session.terminal_watermark {
                session.terminal_watermark = applied.control_id;
                session.muted = applied.input_muted;
                session.output_suppressed = applied.output_suppressed;
                session.telemetry.applied = Some(MediaApplied {
                    input_muted: session.muted,
                    output_suppressed: session.output_suppressed,
                    ..applied
                });
            }
        }
        MediaControl::ControlFailed(failed) => {
            if let Some(control_id) = failed.control_id {
                if session.outstanding.remove(&control_id).is_none() {
                    return;
                }
                if control_id > session.terminal_watermark {
                    session.terminal_watermark = control_id;
                    session.telemetry.control_failed = Some(failed);
                }
            } else {
                // Legacy Mute telemetry cannot settle a correlated ID or move its watermark.
                session.telemetry.legacy_control_failed = Some(failed);
            }
        }
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
    pane_intents: HashMap<PaneId, MediaIntent>,
    next_owner_cleanup: Option<Instant>,
    sessions: HashMap<String, MediaSession>,
    closed: VecDeque<ClosedSession>,
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
            pane_intents: HashMap::new(),
            next_owner_cleanup: None,
            sessions: HashMap::new(),
            closed: VecDeque::new(),
            id_prefix: format!("media_{boot:x}_"),
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
        (!self.pane_owners.is_empty() || !self.pane_intents.is_empty())
            && self
                .next_owner_cleanup
                .is_none_or(|deadline| now >= deadline)
    }

    pub(crate) fn retain_live_panes(&mut self, now: Instant, pane_exists: impl Fn(PaneId) -> bool) {
        self.pane_owners.retain(|pane, _| pane_exists(*pane));
        self.pane_intents.retain(|pane, _| pane_exists(*pane));
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
        views: impl Fn(u64) -> bool,
        now: Instant,
    ) -> Vec<MediaAction> {
        let mut actions = Vec::new();
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

        // Read policy before retiring any session. Intent is not inferred from old ACKs
        // and is independent of whether this is a fresh input-authorized open or renewal.
        let initial = self.intent(pane);
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

        let session_id = format!("{}{}", self.id_prefix, self.next_id);
        self.next_id += 1;
        self.sessions.insert(
            session_id.clone(),
            MediaSession {
                client_id,
                pane,
                pane_ref: input.pane_ref.clone(),
                replaces,
                wants_muted: initial.input_muted,
                wants_output_suppressed: initial.output_suppressed,
                state: MediaSessionState::Opening,
                muted: false,
                output_suppressed: false,
                outstanding: HashMap::new(),
                terminal_watermark: 0,
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
                    session_id,
                    pane_id: input.pane_ref,
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
        if matches!(
            &control,
            MediaControl::Applied(_)
                | MediaControl::ControlFailed(_)
                | MediaControl::Levels(_)
                | MediaControl::Info(_)
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
                // The client accepted a handover: only now is the call it renews replaced.
                for id in std::mem::take(&mut session.replaces) {
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
            | MediaControl::OpenInitial(_)
            | MediaControl::Answer(_)
            | MediaControl::Mute(_)
            | MediaControl::Control(_)
            | MediaControl::Applied(_)
            | MediaControl::ControlFailed(_)
            | MediaControl::Levels(_)
            | MediaControl::Info(_) => {}
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
        MediaControl::Mute(MediaMute {
            session_id: session_id.to_owned(),
            muted,
        })
        .validate()
        .map_err(|message| (error_code::NOT_READY, message))?;
        let ControlTargets { actual, targets } =
            self.control_targets(session_id, true, None, None)?;
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
    // The optional lane-C API integration calls this combined entry point.
    #[allow(dead_code)]
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
            self.control_targets(session_id, privacy, levels, Some(now))?;
        for (id, _) in &targets {
            if self
                .sessions
                .get(id)
                .is_none_or(|session| session.outstanding.len() >= MEDIA_DEVICE_CONTROL_CAPACITY)
            {
                return Err((
                    error_code::NOT_READY,
                    "a media target has no device-control allowance".to_owned(),
                ));
            }
        }
        let control_id = self.next_control_id;
        let next_control_id = control_id.checked_add(1).ok_or_else(|| {
            (
                error_code::NOT_READY,
                "media control identifiers are exhausted".to_owned(),
            )
        })?;
        // All fallible target/capacity/ID checks precede this single commit helper.
        let intent = self.record_control_intent(&actual, input_muted, output_suppressed)?;
        self.next_control_id = next_control_id;
        for (id, _) in &targets {
            if let Some(session) = self.sessions.get_mut(id) {
                session.wants_muted = intent.input_muted;
                session.wants_output_suppressed = intent.output_suppressed;
                session.outstanding.insert(
                    control_id,
                    MediaIntent {
                        input_muted: session.wants_muted,
                        output_suppressed: session.wants_output_suppressed,
                    },
                );
            }
        }
        // Publish every expected pair before exposing the first Send action.
        let session_ids = targets.iter().map(|(id, _)| id.clone()).collect();
        let actions = targets
            .into_iter()
            .map(|(id, client_id)| {
                let target_levels = if id == session_id { levels } else { None };
                MediaAction::Send {
                    client_id,
                    control: MediaControl::Control(MediaDeviceControl {
                        session_id: id,
                        control_id,
                        input_muted,
                        output_suppressed,
                        levels: target_levels,
                    }),
                }
            })
            .collect();
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
    // Called by lane C after writer-availability, encoding and targeted-send checks.
    #[allow(dead_code)]
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
        // Topology, not session retention or stale input, bounds runtime pane policy.
        // This must run even when no live or retained session remains.
        self.pane_intents.retain(|pane, _| pane_exists(*pane));
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

    /// Sessions still being handed over as renewals of `session_id` (it is in their `replaces`).
    fn successors(&self, session_id: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, other)| other.replaces.iter().any(|old| old == session_id))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// A replacement string alone is not authority: constrain lineage to a connected
    /// source (or its retained replacement receipt), the same runtime pane and owner.
    fn control_alias_authorized(&self, id: &str, pane: PaneId, client_id: u64) -> bool {
        self.sessions.get(id).is_some_and(|source| {
            source.pane == pane
                && source.client_id == client_id
                && source.state == MediaSessionState::Connected
        }) || self.closed.iter().any(|source| {
            source.view.session_id == id
                && source.pane == pane
                && source.client_id == client_id
                && source.authorizes_renewal
        })
    }

    /// Shared read-only preflight for legacy Mute and acknowledged dispatch. A's richer
    /// retirement/consumed-alias guards must remain before this commit seam on re-merge.
    fn control_targets(
        &self,
        session_id: &str,
        privacy: bool,
        levels: Option<bool>,
        now: Option<Instant>,
    ) -> Result<ControlTargets, (&'static str, String)> {
        let actual = if self.sessions.contains_key(session_id) {
            session_id.to_owned()
        } else {
            let mut aliases = self
                .sessions
                .iter()
                .filter(|(_, session)| {
                    session.pending.is_some()
                        && session.replaces.iter().any(|old| old == session_id)
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
        let mut roots = vec![session_id.to_owned()];
        roots.extend(
            named
                .replaces
                .iter()
                .filter(|old| self.control_alias_authorized(old, pane, client_id))
                .cloned(),
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
                        roots.contains(old) && self.control_alias_authorized(old, pane, client_id)
                    });
                if source || successor {
                    targets.push((id.clone(), client_id));
                }
            }
        }
        targets[1..].sort_by(|left, right| left.0.cmp(&right.0));
        for (id, target_client) in &targets {
            let available = self.sessions.get(id).is_some_and(|session| {
                session.pane == pane
                    && session.client_id == *target_client
                    && session.state != MediaSessionState::Failed
                    && session.state != MediaSessionState::Closed
                    && !now.is_some_and(|now| {
                        session
                            .pending
                            .as_ref()
                            .is_some_and(|pending| now >= pending.deadline)
                    })
            }) && self
                .clients
                .get(target_client)
                .is_some_and(|client| client.capable);
            if !available {
                return Err((
                    error_code::NOT_READY,
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
            });
        }
        self.closed.push_back(ClosedSession {
            closed_at: now,
            client_id: session.client_id,
            pane: session.pane,
            authorizes_renewal: code == close_code::REPLACED
                && session.state == MediaSessionState::Connected,
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
    use crate::protocol::ServerMessage;
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
        // An incidental replaces string on a foreign owner/pane grants no fanout.
        broker
            .sessions
            .get_mut(&other)
            .unwrap()
            .replaces
            .push(old.clone());
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
            Err((error_code::NOT_READY, _))
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
            session.replaces.push(source.clone());
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
            Err((error_code::NOT_READY, _))
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
            Err((error_code::NOT_READY, _))
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
