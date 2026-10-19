//! Client media control plane.
//!
//! [`ClientMedia`] decides whether this client accepts a server's `media.open.v1`, asks the
//! user when the `media` setting is `ask`, drives one [`MediaPeer`] and turns peer events into
//! media controls for the owning endpoint. It does no I/O: the client loop passes in the view
//! facts and the clock, then applies the returned [`MediaEffect`]s.

#[cfg(feature = "native-media")]
mod native;
pub(crate) mod peer;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use self::peer::{
    MediaPeer, PeerCommandError, PeerEvent, PeerEventSink, PeerFactory, PeerInitialState,
};
use super::endpoint::ClientEndpointId;
use crate::config::MediaMode;
use crate::protocol::media::{
    close_code, MediaClose, MediaControl, MediaControlFailed, MediaDeviceControl, MediaOpen,
    MediaSdp, MediaStateUpdate, MAX_MEDIA_TEXT_BYTES, MEDIA_INPUT_WINDOW,
};

/// The consent prompt declines on its own after this long.
pub(super) const MEDIA_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Process-local v1 safety memory. Protected keys are never evicted or expired.
const MAX_LEGACY_INTENTS: usize = 256;

/// Preserve the test-only synchronous discard fixture in client/state.rs without giving
/// production peers a synchronous delivery backdoor.
pub(super) trait IntoPeerEventSink {
    fn into_peer_event_sink(self) -> PeerEventSink;
}

impl IntoPeerEventSink for PeerEventSink {
    fn into_peer_event_sink(self) -> PeerEventSink {
        self
    }
}

#[cfg(test)]
impl<F> IntoPeerEventSink for std::sync::Arc<F>
where
    F: Fn(PeerEvent) + Send + Sync + 'static,
{
    fn into_peer_event_sink(self) -> PeerEventSink {
        std::sync::Arc::new(move |event| {
            self(event);
            Box::pin(async { Ok(()) })
        })
    }
}

/// Work for the client loop after a controller call.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MediaEffect {
    /// Send this control to the endpoint.
    Send(ClientEndpointId, MediaControl),
    /// Show one short client notice.
    Notice(String),
    /// Show the consent prompt for a session.
    AskConsent {
        session_id: String,
        pane_label: String,
    },
    /// Remove the consent prompt for a session.
    CancelConsent { session_id: String },
}

struct MediaSession {
    endpoint_id: ClientEndpointId,
    session_id: String,
    /// The pane this call is for, so a renewal of it can be recognised (smarty-voice#133).
    pane_id: String,
    /// Latest requested input mute, including controls awaiting the audio paths. A renewal
    /// must retain this intent, not an older acknowledgement (herdr#102 security pass).
    desired_input_muted: bool,
    desired_output_suppressed: bool,
    generation: u64,
    peer: Box<dyn MediaPeer>,
}

struct PendingConsent {
    endpoint_id: ClientEndpointId,
    session_id: String,
    pane_id: String,
    initial: PeerInitialState,
    deadline: Instant,
}

pub(super) struct ClientMedia {
    mode: MediaMode,
    peer_available: bool,
    factory: PeerFactory,
    sink: PeerEventSink,
    /// This client's last pane input per endpoint: `(pane_id, when)`.
    last_input: HashMap<ClientEndpointId, (String, Instant)>,
    legacy_intents: HashMap<(ClientEndpointId, String), PeerInitialState>,
    /// Ordered Initial/v1 alternates are one attempt, even after refusal or factory failure.
    initial_alternates: HashMap<ClientEndpointId, (String, String)>,
    peer_generation: u64,
    /// The user allowed media once in this client process.
    allowed: bool,
    pending: Option<PendingConsent>,
    session: Option<MediaSession>,
    effects: Vec<MediaEffect>,
}

impl ClientMedia {
    pub(super) fn new(mode: MediaMode, factory: PeerFactory, sink: impl IntoPeerEventSink) -> Self {
        Self {
            mode,
            peer_available: peer::NATIVE_PEER_AVAILABLE,
            factory,
            sink: sink.into_peer_event_sink(),
            last_input: HashMap::new(),
            legacy_intents: HashMap::new(),
            initial_alternates: HashMap::new(),
            peer_generation: 0,
            allowed: false,
            pending: None,
            session: None,
            effects: Vec::new(),
        }
    }

    /// Apply a reloaded `media` setting. Turning media off also refuses an unanswered prompt
    /// and ends a live call, so nothing keeps the microphone against the new setting.
    pub(super) fn set_mode(&mut self, mode: MediaMode) {
        self.mode = mode;
        if mode != MediaMode::Off {
            return;
        }
        if let Some(pending) = self.pending.take() {
            self.effects.push(MediaEffect::CancelConsent {
                session_id: pending.session_id.clone(),
            });
            self.send_close(
                pending.endpoint_id,
                pending.session_id,
                close_code::DISABLED,
                "media is off in this client's config",
            );
        }
        if let Some(mut session) = self.session.take() {
            session.peer.close();
            self.send_close(
                session.endpoint_id,
                session.session_id,
                close_code::DISABLED,
                "media is off in this client's config",
            );
            self.notice("Voice call ended: media is off in this client's config");
        }
    }

    pub(super) fn take_effects(&mut self) -> Vec<MediaEffect> {
        std::mem::take(&mut self.effects)
    }

    /// When the consent prompt expires, if one is open.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.deadline)
    }

    /// Record pane input this client actually sent to `endpoint_id`.
    pub(super) fn note_pane_input(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        now: Instant,
    ) {
        self.last_input
            .insert(endpoint_id.clone(), (pane_id.to_owned(), now));
    }

    /// Handle one media control from `endpoint_id`.
    ///
    /// `endpoint_shown` is true when that endpoint owns the surface this client shows.
    /// `pane_label` returns the pane's label when the pane is in the layout this client shows.
    pub(super) fn handle_server_control(
        &mut self,
        endpoint_id: &ClientEndpointId,
        control: MediaControl,
        endpoint_shown: bool,
        pane_label: impl FnOnce(&str) -> Option<String>,
        now: Instant,
    ) {
        match control {
            MediaControl::Open(open) => {
                if self
                    .initial_alternates
                    .get(endpoint_id)
                    .is_some_and(|(session, pane)| {
                        *session == open.session_id && *pane == open.pane_id
                    })
                {
                    return;
                }
                self.handle_open(endpoint_id, open, None, endpoint_shown, pane_label, now);
            }
            MediaControl::OpenInitial(initial) => {
                // Mark a valid Initial before any acceptance/refusal side effect. Its v1
                // alternate must not retry a failed open or bypass consent/config/input.
                if MediaControl::OpenInitial(initial.clone())
                    .validate()
                    .is_err()
                {
                    return;
                }
                self.initial_alternates.insert(
                    endpoint_id.clone(),
                    (initial.session_id.clone(), initial.pane_id.clone()),
                );
                self.handle_open(
                    endpoint_id,
                    MediaOpen {
                        session_id: initial.session_id,
                        pane_id: initial.pane_id,
                    },
                    Some(PeerInitialState {
                        input_muted: initial.input_muted,
                        output_suppressed: initial.output_suppressed,
                    }),
                    endpoint_shown,
                    pane_label,
                    now,
                );
            }
            MediaControl::Answer(answer) => {
                if let Some(session) = self.owned_session(endpoint_id, &answer.session_id) {
                    let result = session.peer.apply_answer(answer.sdp);
                    self.command_result(endpoint_id, &answer.session_id, result);
                }
            }
            MediaControl::Mute(mute) => {
                self.patch_owned_intent(endpoint_id, &mute.session_id, Some(mute.muted), None);
                if let Some(session) = self.owned_session(endpoint_id, &mute.session_id) {
                    let result = session.peer.set_muted(mute.muted);
                    self.command_result(endpoint_id, &mute.session_id, result);
                }
            }
            MediaControl::Control(control) => self.handle_device_control(endpoint_id, control),
            MediaControl::Close(close) => {
                if self.owned_session(endpoint_id, &close.session_id).is_some() {
                    self.end_session("Voice call ended");
                } else if self.pending.as_ref().is_some_and(|pending| {
                    &pending.endpoint_id == endpoint_id && pending.session_id == close.session_id
                }) {
                    self.cancel_pending();
                }
            }
            // Client-to-server kinds; a server never sends them.
            MediaControl::Offer(_)
            | MediaControl::State(_)
            | MediaControl::Applied(_)
            | MediaControl::ControlFailed(_)
            | MediaControl::Levels(_)
            | MediaControl::Info(_) => {}
        }
    }

    fn handle_open(
        &mut self,
        endpoint_id: &ClientEndpointId,
        open: MediaOpen,
        initial: Option<PeerInitialState>,
        endpoint_shown: bool,
        pane_label: impl FnOnce(&str) -> Option<String>,
        now: Instant,
    ) {
        let MediaOpen {
            session_id,
            pane_id,
        } = open;
        let duplicate = self.session.as_ref().is_some_and(|session| {
            session.endpoint_id == *endpoint_id && session.session_id == session_id
        }) || self.pending.as_ref().is_some_and(|pending| {
            pending.endpoint_id == *endpoint_id && pending.session_id == session_id
        });
        if duplicate {
            return;
        }
        if !endpoint_shown {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::WRONG_ENDPOINT,
                "this client is not showing that server",
            );
        }
        if !self.peer_available {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::UNSUPPORTED,
                "this client was built without native media",
            );
        }
        if self.mode == MediaMode::Off {
            self.notice("Voice call blocked: media is off in this client's config");
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::DISABLED,
                "media is off in this client's config",
            );
        }
        let Some(label) = pane_label(&pane_id) else {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::NOT_VIEWED,
                "the pane is not in the layout this client shows",
            );
        };
        let fresh_input = self
            .last_input
            .get(endpoint_id)
            .is_some_and(|(last_pane, at)| {
                *last_pane == pane_id && now.saturating_duration_since(*at) <= MEDIA_INPUT_WINDOW
            });
        // smarty-voice#133: a renewal of this client's own call on this pane (the server reopens for the client of the
        // pane's live call) needs no fresh input: the user started that call here and it is still running.
        let renewal = self.session.as_ref().is_some_and(|session| {
            session.endpoint_id == *endpoint_id && session.pane_id == pane_id
        });
        if !fresh_input && !renewal {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::STALE_INPUT,
                "this client did not just type into that pane",
            );
        }
        let Some(initial) = self.reserve_intent(endpoint_id, &pane_id, initial) else {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::INTENT_CAPACITY,
                "this client's retained media privacy guard is full",
            );
        };
        if self.mode == MediaMode::Auto || self.allowed {
            return self.start(endpoint_id.clone(), session_id, pane_id, initial);
        }
        // A newer request supersedes an unanswered prompt.
        if let Some(previous) = self.pending.take() {
            self.effects.push(MediaEffect::CancelConsent {
                session_id: previous.session_id.clone(),
            });
            self.send_close(
                previous.endpoint_id,
                previous.session_id,
                close_code::REPLACED,
                "a newer media request replaced this one",
            );
        }
        self.pending = Some(PendingConsent {
            endpoint_id: endpoint_id.clone(),
            session_id: session_id.clone(),
            pane_id,
            initial,
            deadline: now + MEDIA_CONSENT_TIMEOUT,
        });
        self.effects.push(MediaEffect::AskConsent {
            session_id,
            pane_label: label,
        });
    }

    /// The user's answer to the consent prompt.
    pub(super) fn consent(&mut self, session_id: &str, allowed: bool) {
        let Some(pending) = self
            .pending
            .take_if(|pending| pending.session_id == session_id)
        else {
            return;
        };
        if allowed && self.mode == MediaMode::Off {
            // The policy may have changed while the prompt was open; it wins over the answer.
            self.send_close(
                pending.endpoint_id,
                pending.session_id,
                close_code::DISABLED,
                "media is off in this client's config",
            );
        } else if allowed {
            self.allowed = true;
            // A still-live source can receive an authorized privacy patch for this same
            // key while its successor awaits consent. Bootstrap the latest requested pair,
            // never an older prompt snapshot or a peer acknowledgement.
            let initial = self
                .legacy_intents
                .get(&(pending.endpoint_id.clone(), pending.pane_id.clone()))
                .copied()
                .unwrap_or(pending.initial);
            self.start(
                pending.endpoint_id,
                pending.session_id,
                pending.pane_id,
                initial,
            );
        } else {
            self.decline(pending, "Voice call declined");
        }
    }

    /// Expire an unanswered consent prompt.
    pub(super) fn tick(&mut self, now: Instant) {
        let Some(pending) = self.pending.take_if(|pending| now >= pending.deadline) else {
            return;
        };
        self.effects.push(MediaEffect::CancelConsent {
            session_id: pending.session_id.clone(),
        });
        self.decline(pending, "Voice call declined: no answer");
    }

    /// Handle one event from the live peer.
    pub(super) fn handle_peer_event(&mut self, event: PeerEvent) {
        let PeerEvent::Scoped { generation, event } = event else {
            return;
        };
        let Some((endpoint_id, current)) = self.session.as_ref().and_then(|session| {
            (session.generation == generation)
                .then(|| (session.endpoint_id.clone(), session.session_id.clone()))
        }) else {
            return;
        };
        // Match only one wrapper: nested scopes, stale generations and unscoped native
        // callbacks can never be attributed by equal session text to another endpoint.
        match *event {
            PeerEvent::Offer { session_id, sdp } if session_id == current => {
                self.effects.push(MediaEffect::Send(
                    endpoint_id,
                    MediaControl::Offer(MediaSdp { session_id, sdp }),
                ));
            }
            PeerEvent::State {
                session_id,
                state,
                muted,
                detail,
            } if session_id == current => {
                self.effects.push(MediaEffect::Send(
                    endpoint_id,
                    MediaControl::State(MediaStateUpdate {
                        session_id,
                        state,
                        muted,
                        detail: detail.map(|detail| bounded(&detail)),
                    }),
                ));
            }
            PeerEvent::Applied(applied) if applied.session_id == current => {
                self.effects.push(MediaEffect::Send(
                    endpoint_id,
                    MediaControl::Applied(applied),
                ));
            }
            PeerEvent::ControlFailed(mut failed) if failed.session_id == current => {
                failed.message = failed.message.map(|message| bounded(&message));
                self.effects.push(MediaEffect::Send(
                    endpoint_id,
                    MediaControl::ControlFailed(failed),
                ));
            }
            PeerEvent::Levels(levels) if levels.session_id == current => {
                self.effects
                    .push(MediaEffect::Send(endpoint_id, MediaControl::Levels(levels)));
            }
            PeerEvent::Info(info) if info.session_id == current => {
                self.effects
                    .push(MediaEffect::Send(endpoint_id, MediaControl::Info(info)));
            }
            PeerEvent::Closed {
                session_id,
                code,
                message,
            } if session_id == current => {
                // The peer already released the devices; dropping it is idempotent.
                self.session = None;
                self.send_close(endpoint_id, session_id, code, &message);
                self.notice("Voice call ended");
            }
            // Late events from a replaced or closed peer.
            _ => {}
        }
    }

    /// The endpoint's connection ended or was replaced: release its session without a message.
    pub(super) fn endpoint_gone(&mut self, endpoint_id: &ClientEndpointId) {
        self.last_input.remove(endpoint_id);
        self.initial_alternates.remove(endpoint_id);
        // Retain protected v1 safety memory across disconnect, including identity reuse.
        if self
            .session
            .as_ref()
            .is_some_and(|session| &session.endpoint_id == endpoint_id)
        {
            self.end_session("Voice call ended: server disconnected");
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.endpoint_id == endpoint_id)
        {
            self.cancel_pending();
        }
    }

    fn start(
        &mut self,
        endpoint_id: ClientEndpointId,
        session_id: String,
        pane_id: String,
        initial: PeerInitialState,
    ) {
        let Some(generation) = self.peer_generation.checked_add(1) else {
            self.send_close(
                endpoint_id,
                session_id,
                close_code::CLOSED,
                "media peer generation exhausted",
            );
            return;
        };
        self.peer_generation = generation;
        // The client retains single-peer replacement: it closes the old peer before opening
        // the successor. If the successor's audio.open fails, the call ends with no audio;
        // voice must renew. This does not provide physical failed-handover preservation.
        if let Some(mut previous) = self.session.take() {
            previous.peer.close();
            self.send_close(
                previous.endpoint_id,
                previous.session_id,
                close_code::REPLACED,
                "a newer media session replaced this one",
            );
        }
        if let Some(previous) = self.pending.take() {
            self.effects.push(MediaEffect::CancelConsent {
                session_id: previous.session_id.clone(),
            });
            self.send_close(
                previous.endpoint_id,
                previous.session_id,
                close_code::REPLACED,
                "a newer media session replaced this one",
            );
        }
        let sink = self.sink.clone();
        let scoped_sink: PeerEventSink = std::sync::Arc::new(move |event| {
            // Return the actual admission future, including its error, unchanged.
            sink(PeerEvent::Scoped {
                generation,
                event: Box::new(event),
            })
        });
        match (self.factory)(session_id.clone(), initial, scoped_sink) {
            Ok(peer) => {
                self.session = Some(MediaSession {
                    endpoint_id,
                    session_id,
                    pane_id,
                    desired_input_muted: initial.input_muted,
                    desired_output_suppressed: initial.output_suppressed,
                    generation,
                    peer,
                });
                self.notice("Voice call started");
            }
            Err(error) => {
                self.notice(&format!("Voice call failed: {error}"));
                self.send_close(endpoint_id, session_id, close_code::DEVICE_ERROR, &error);
            }
        }
    }

    /// Reserve before consent or native startup. Only unprotected, unused entries can be
    /// reclaimed; saturation never evicts a protected pane or ends an unrelated call.
    fn reserve_intent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        initial: Option<PeerInitialState>,
    ) -> Option<PeerInitialState> {
        let key = (endpoint_id.clone(), pane_id.to_owned());
        if !self.legacy_intents.contains_key(&key)
            && self.legacy_intents.len() >= MAX_LEGACY_INTENTS
        {
            let session = self.session.as_ref();
            let pending = self.pending.as_ref();
            self.legacy_intents.retain(|(endpoint, pane), intent| {
                intent.input_muted
                    || intent.output_suppressed
                    || session
                        .is_some_and(|live| live.endpoint_id == *endpoint && live.pane_id == *pane)
                    || pending.is_some_and(|waiting| {
                        waiting.endpoint_id == *endpoint && waiting.pane_id == *pane
                    })
            });
            if self.legacy_intents.len() >= MAX_LEGACY_INTENTS {
                return None;
            }
        }
        let intent = self.legacy_intents.entry(key).or_default();
        if let Some(initial) = initial {
            *intent = initial; // A complete Initial is authoritative, including false.
        }
        Some(*intent)
    }

    fn patch_owned_intent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: &str,
        input_muted: Option<bool>,
        output_suppressed: Option<bool>,
    ) {
        let (pane, desired) = if let Some(session) = self.session.as_mut().filter(|session| {
            session.endpoint_id == *endpoint_id && session.session_id == session_id
        }) {
            if let Some(muted) = input_muted {
                session.desired_input_muted = muted;
            }
            if let Some(suppressed) = output_suppressed {
                session.desired_output_suppressed = suppressed;
            }
            (
                &session.pane_id,
                PeerInitialState {
                    input_muted: session.desired_input_muted,
                    output_suppressed: session.desired_output_suppressed,
                },
            )
        } else if let Some(pending) = self.pending.as_mut().filter(|pending| {
            pending.endpoint_id == *endpoint_id && pending.session_id == session_id
        }) {
            if let Some(muted) = input_muted {
                pending.initial.input_muted = muted;
            }
            if let Some(suppressed) = output_suppressed {
                pending.initial.output_suppressed = suppressed;
            }
            (&pending.pane_id, pending.initial)
        } else {
            return;
        };
        if let Some(intent) = self
            .legacy_intents
            .get_mut(&(endpoint_id.clone(), pane.clone()))
        {
            // Patch only requested axes: another authorized same-key target may already
            // have supplied a newer request on an omitted axis.
            if input_muted.is_some() {
                intent.input_muted = desired.input_muted;
            }
            if output_suppressed.is_some() {
                intent.output_suppressed = desired.output_suppressed;
            }
        }
    }

    fn handle_device_control(
        &mut self,
        endpoint_id: &ClientEndpointId,
        control: MediaDeviceControl,
    ) {
        let live = self.session.as_ref().is_some_and(|session| {
            session.endpoint_id == *endpoint_id && session.session_id == control.session_id
        });
        let pending = self.pending.as_ref().is_some_and(|pending| {
            pending.endpoint_id == *endpoint_id && pending.session_id == control.session_id
        });
        if !live && !pending {
            let wrong_endpoint = self
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == control.session_id)
                || self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.session_id == control.session_id);
            let (code, message) = if wrong_endpoint {
                (
                    close_code::WRONG_ENDPOINT,
                    "this media session belongs to another server",
                )
            } else {
                (
                    close_code::CLOSED,
                    "this client has no live peer for that media session",
                )
            };
            self.refuse(endpoint_id, control.session_id, code, message);
            return;
        }
        self.patch_owned_intent(
            endpoint_id,
            &control.session_id,
            control.input_muted,
            control.output_suppressed,
        );
        if pending && !live {
            // Retain privacy, not a speculative native operation or levels enablement.
            self.effects.push(MediaEffect::Send(
                endpoint_id.clone(),
                MediaControl::ControlFailed(MediaControlFailed {
                    session_id: control.session_id,
                    control_id: Some(control.control_id),
                    code: "awaiting_consent".to_owned(),
                    message: Some("microphone access is awaiting user consent".to_owned()),
                }),
            ));
            return;
        }
        let session_id = control.session_id.clone();
        if let Some(session) = self.owned_session(endpoint_id, &session_id) {
            let result = session.peer.apply_control(control);
            self.command_result(endpoint_id, &session_id, result);
        }
    }

    fn command_result(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: &str,
        result: Result<(), PeerCommandError>,
    ) {
        let Err(error) = result else {
            return;
        };
        // Never turn a rejected command into admission, or affect an equal ID elsewhere.
        if let Some(mut session) = self.session.take_if(|session| {
            session.endpoint_id == *endpoint_id && session.session_id == session_id
        }) {
            session.peer.close();
            let (code, message) = match error {
                PeerCommandError::Full => (
                    close_code::CONTROL_QUEUE_FULL,
                    "media command queue is full",
                ),
                PeerCommandError::Closed => (close_code::CLOSED, "media command queue is closed"),
            };
            self.send_close(endpoint_id.clone(), session.session_id, code, message);
            self.notice("Voice call ended");
        }
    }

    fn owned_session(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: &str,
    ) -> Option<&mut MediaSession> {
        self.session.as_mut().filter(|session| {
            &session.endpoint_id == endpoint_id && session.session_id == session_id
        })
    }

    fn end_session(&mut self, notice: &str) {
        if let Some(mut session) = self.session.take() {
            session.peer.close();
            self.notice(notice);
        }
    }

    fn cancel_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.effects.push(MediaEffect::CancelConsent {
                session_id: pending.session_id,
            });
        }
    }

    fn decline(&mut self, pending: PendingConsent, notice: &str) {
        self.notice(notice);
        self.send_close(
            pending.endpoint_id,
            pending.session_id,
            close_code::DECLINED,
            "the user did not allow microphone access",
        );
    }

    fn refuse(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: String,
        code: &str,
        message: &str,
    ) {
        self.send_close(endpoint_id.clone(), session_id, code, message);
    }

    fn send_close(
        &mut self,
        endpoint_id: ClientEndpointId,
        session_id: String,
        code: &str,
        message: &str,
    ) {
        self.effects.push(MediaEffect::Send(
            endpoint_id,
            MediaControl::Close(MediaClose::new(session_id, code, bounded(message))),
        ));
    }

    fn notice(&mut self, message: &str) {
        self.effects.push(MediaEffect::Notice(message.to_owned()));
    }
}

/// Keep peer and device text inside the protocol's detail bound.
fn bounded(text: &str) -> String {
    if text.len() <= MAX_MEDIA_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_MEDIA_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::api::schema::MediaSessionState;
    use crate::protocol::media::{
        MediaApplied, MediaDeviceControl, MediaInfo, MediaLevels, MediaOpenInitial, MediaPeerState,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum PeerCall {
        Start(String),
        Answer(String, String),
        Mute(String, bool),
        Control(String, MediaDeviceControl),
        Close(String),
    }

    type Calls = Arc<Mutex<Vec<PeerCall>>>;

    struct FakePeer {
        session_id: String,
        calls: Calls,
        command_error: Option<PeerCommandError>,
    }

    impl MediaPeer for FakePeer {
        fn apply_answer(&mut self, sdp: String) -> Result<(), PeerCommandError> {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Answer(self.session_id.clone(), sdp));
            self.command_error.map_or(Ok(()), Err)
        }

        fn set_muted(&mut self, muted: bool) -> Result<(), PeerCommandError> {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Mute(self.session_id.clone(), muted));
            self.command_error.map_or(Ok(()), Err)
        }

        fn apply_control(&mut self, control: MediaDeviceControl) -> Result<(), PeerCommandError> {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Control(self.session_id.clone(), control));
            self.command_error.map_or(Ok(()), Err)
        }

        fn close(&mut self) {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Close(self.session_id.clone()));
        }
    }

    fn media(mode: MediaMode, fail: bool) -> (ClientMedia, Calls) {
        let calls = Calls::default();
        let factory_calls = calls.clone();
        let factory: PeerFactory = Box::new(
            move |session_id: String,
                  _initial: PeerInitialState,
                  _sink: PeerEventSink|
                  -> Result<Box<dyn MediaPeer>, String> {
                factory_calls
                    .lock()
                    .unwrap()
                    .push(PeerCall::Start(session_id.clone()));
                if fail {
                    return Err("no microphone".to_owned());
                }
                Ok(Box::new(FakePeer {
                    session_id,
                    calls: factory_calls.clone(),
                    command_error: None,
                }) as Box<dyn MediaPeer>)
            },
        );
        let sink: PeerEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let mut media = ClientMedia::new(mode, factory, sink);
        media.peer_available = true;
        (media, calls)
    }

    fn local() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    fn remote() -> ClientEndpointId {
        ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        )
    }

    fn device_control(session_id: &str, control_id: u64) -> MediaDeviceControl {
        MediaDeviceControl {
            session_id: session_id.into(),
            control_id,
            input_muted: None,
            output_suppressed: None,
            levels: None,
        }
    }

    fn device_events(session_id: &str) -> [PeerEvent; 3] {
        [
            PeerEvent::Applied(MediaApplied {
                session_id: session_id.into(),
                control_id: 7,
                input_muted: true,
                output_suppressed: true,
            }),
            PeerEvent::Levels(MediaLevels {
                session_id: session_id.into(),
                input_peak: 0.25,
                output_peak: 0.5,
            }),
            PeerEvent::Info(MediaInfo {
                session_id: session_id.into(),
                version: "test-version".into(),
                commit: "test-commit".into(),
                pid: 42,
                input_device: "microphone".into(),
                output_device: "speaker".into(),
            }),
        ]
    }

    fn open(media: &mut ClientMedia, session_id: &str, pane_id: &str, now: Instant) {
        media.handle_server_control(
            &local(),
            MediaControl::Open(MediaOpen {
                session_id: session_id.into(),
                pane_id: pane_id.into(),
            }),
            true,
            |pane| (pane == "pane_1" || pane == "pane_2").then(|| format!("label {pane}")),
            now,
        );
    }

    /// Inject an event through the same single-generation boundary as the factory sink.
    fn emit(media: &mut ClientMedia, event: PeerEvent) {
        media.handle_peer_event(PeerEvent::Scoped {
            generation: media.peer_generation,
            event: Box::new(event),
        });
    }

    fn closes(effects: &[MediaEffect]) -> Vec<(String, Option<String>)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                MediaEffect::Send(_, MediaControl::Close(close)) => {
                    Some((close.session_id.clone(), close.code.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn refusal(media: &mut ClientMedia) -> Option<String> {
        let effects = media.take_effects();
        match closes(&effects).as_slice() {
            [(_, code)] => code.clone(),
            _ => None,
        }
    }

    #[test]
    fn open_from_an_endpoint_the_client_does_not_show_is_wrong_endpoint() {
        let (mut media, calls) = media(MediaMode::Auto, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        media.handle_server_control(
            &local(),
            MediaControl::Open(MediaOpen {
                session_id: "m1".into(),
                pane_id: "pane_1".into(),
            }),
            false,
            |_| Some("label".into()),
            now,
        );
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::WRONG_ENDPOINT)
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn open_without_a_build_peer_is_unsupported() {
        let (mut media, _) = media(MediaMode::Auto, false);
        media.peer_available = false;
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::UNSUPPORTED)
        );
    }

    #[test]
    fn open_with_media_off_is_disabled_with_a_notice() {
        let (mut media, calls) = media(MediaMode::Off, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        let effects = media.take_effects();
        assert_eq!(
            closes(&effects),
            vec![("m1".to_owned(), Some(close_code::DISABLED.to_owned()))]
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, MediaEffect::Notice(_))));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn open_for_a_pane_outside_the_view_is_not_viewed() {
        let (mut media, _) = media(MediaMode::Auto, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_9", now);
        open(&mut media, "m1", "pane_9", now);
        assert_eq!(refusal(&mut media).as_deref(), Some(close_code::NOT_VIEWED));
    }

    #[test]
    fn stale_or_other_pane_input_is_stale_input() {
        let (mut media, calls) = media(MediaMode::Auto, false);
        let start = Instant::now();
        media.note_pane_input(&local(), "pane_1", start);
        open(&mut media, "m1", "pane_1", start + Duration::from_secs(11));
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::STALE_INPUT)
        );

        media.note_pane_input(&local(), "pane_2", start);
        open(&mut media, "m2", "pane_1", start);
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::STALE_INPUT)
        );

        media.note_pane_input(&ClientEndpointId::Local, "pane_1", start);
        media.endpoint_gone(&local());
        open(&mut media, "m3", "pane_1", start);
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::STALE_INPUT)
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reloading_media_off_refuses_a_pending_prompt_and_a_late_accept_starts_nothing() {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        assert!(media
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, MediaEffect::AskConsent { .. })));

        media.set_mode(MediaMode::Off);
        let effects = media.take_effects();
        assert_eq!(
            closes(&effects),
            vec![("m1".to_owned(), Some(close_code::DISABLED.to_owned()))]
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            MediaEffect::CancelConsent { session_id } if session_id == "m1"
        )));

        media.consent("m1", true);
        assert!(media.take_effects().is_empty());
        assert!(calls.lock().unwrap().is_empty(), "no peer after media off");
    }

    #[test]
    fn accepting_after_media_turned_off_is_disabled_and_ask_still_works_otherwise() {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        media.take_effects();
        // A policy change that bypassed set_mode's cleanup must still win at the start boundary.
        media.mode = MediaMode::Off;
        media.consent("m1", true);
        assert_eq!(
            closes(&media.take_effects()),
            vec![("m1".to_owned(), Some(close_code::DISABLED.to_owned()))]
        );
        assert!(calls.lock().unwrap().is_empty());

        // Counterexample: with ask still in force, accepting starts the peer.
        media.mode = MediaMode::Ask;
        open(&mut media, "m2", "pane_1", now);
        media.take_effects();
        media.consent("m2", true);
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m2".into())]);
    }

    #[test]
    fn reloading_media_off_ends_a_live_call_and_releases_the_peer() {
        let (mut media, calls) = media(MediaMode::Auto, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        media.take_effects();
        media.set_mode(MediaMode::Ask);
        assert!(media.take_effects().is_empty(), "ask keeps the live call");

        media.set_mode(MediaMode::Off);
        assert_eq!(
            closes(&media.take_effects()),
            vec![("m1".to_owned(), Some(close_code::DISABLED.to_owned()))]
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![PeerCall::Start("m1".into()), PeerCall::Close("m1".into())]
        );
    }

    #[test]
    fn auto_starts_the_peer_and_forwards_its_offer_and_state() {
        let (mut media, calls) = media(MediaMode::Auto, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now + Duration::from_secs(9));
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert!(closes(&media.take_effects()).is_empty());

        emit(
            &mut media,
            PeerEvent::Offer {
                session_id: "m1".into(),
                sdp: "v=0".into(),
            },
        );
        emit(
            &mut media,
            PeerEvent::State {
                session_id: "m1".into(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            },
        );
        emit(
            &mut media,
            PeerEvent::Offer {
                session_id: "stale".into(),
                sdp: "v=0".into(),
            },
        );
        assert_eq!(
            media.take_effects(),
            vec![
                MediaEffect::Send(
                    local(),
                    MediaControl::Offer(MediaSdp {
                        session_id: "m1".into(),
                        sdp: "v=0".into(),
                    })
                ),
                MediaEffect::Send(
                    local(),
                    MediaControl::State(MediaStateUpdate {
                        session_id: "m1".into(),
                        state: MediaPeerState::Connected,
                        muted: false,
                        detail: None,
                    })
                ),
            ]
        );
    }

    #[test]
    fn ask_waits_for_consent_then_starts_and_remembers_it() {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        assert!(media.take_effects().contains(&MediaEffect::AskConsent {
            session_id: "m1".into(),
            pane_label: "label pane_1".into(),
        }));
        assert_eq!(media.deadline(), Some(now + MEDIA_CONSENT_TIMEOUT));
        assert!(calls.lock().unwrap().is_empty());

        media.consent("m1", true);
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert_eq!(media.deadline(), None);

        // Allowed once in this process: the next open starts at once and replaces m1.
        media.take_effects();
        open(&mut media, "m2", "pane_1", now);
        assert_eq!(
            closes(&media.take_effects()),
            vec![("m1".to_owned(), Some(close_code::REPLACED.to_owned()))]
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                PeerCall::Start("m1".into()),
                PeerCall::Close("m1".into()),
                PeerCall::Start("m2".into()),
            ]
        );
    }

    #[test]
    fn declined_or_unanswered_consent_is_declined() {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        media.take_effects();
        media.consent("m1", false);
        assert_eq!(refusal(&mut media).as_deref(), Some(close_code::DECLINED));

        open(&mut media, "m2", "pane_1", now);
        media.take_effects();
        media.tick(now + Duration::from_secs(29));
        assert!(media.take_effects().is_empty());
        media.tick(now + MEDIA_CONSENT_TIMEOUT);
        let effects = media.take_effects();
        assert!(effects.contains(&MediaEffect::CancelConsent {
            session_id: "m2".into()
        }));
        assert_eq!(
            closes(&effects),
            vec![("m2".to_owned(), Some(close_code::DECLINED.to_owned()))]
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn factory_failure_is_device_error() {
        let (mut media, _) = media(MediaMode::Auto, true);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::DEVICE_ERROR)
        );
    }

    fn started(mode: MediaMode) -> (ClientMedia, Calls) {
        started_on(mode, &local())
    }

    fn started_on(mode: MediaMode, endpoint_id: &ClientEndpointId) -> (ClientMedia, Calls) {
        let (mut media, calls) = media(mode, false);
        let now = Instant::now();
        media.note_pane_input(endpoint_id, "pane_1", now);
        media.handle_server_control(
            endpoint_id,
            MediaControl::Open(MediaOpen {
                session_id: "m1".into(),
                pane_id: "pane_1".into(),
            }),
            true,
            |_| Some("label pane_1".into()),
            now,
        );
        media.take_effects();
        (media, calls)
    }

    // smarty-voice#133: the server reopens a pane's live call for its client when nobody typed in the last 10 s.
    #[test]
    fn a_renewal_of_this_clients_own_call_on_the_pane_starts_without_fresh_input() {
        let (mut media, calls) = started(MediaMode::Auto);
        let later = Instant::now() + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        open(&mut media, "m2", "pane_1", later);
        let effects = media.take_effects();
        assert_eq!(
            closes(&effects),
            vec![("m1".to_owned(), Some(close_code::REPLACED.to_owned()))],
            "the old peer is replaced, and nothing is refused"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                PeerCall::Start("m1".into()),
                PeerCall::Close("m1".into()),
                PeerCall::Start("m2".into())
            ]
        );
    }

    #[test]
    fn stale_input_is_still_refused_for_another_pane_or_with_no_call() {
        let (mut calling, _) = started(MediaMode::Auto);
        let later = Instant::now() + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        open(&mut calling, "m2", "pane_2", later); // The live call is on pane_1.
        assert_eq!(
            refusal(&mut calling).as_deref(),
            Some(close_code::STALE_INPUT)
        );

        let (mut idle, calls) = media(MediaMode::Auto, false);
        idle.note_pane_input(&local(), "pane_1", Instant::now());
        open(&mut idle, "m3", "pane_1", later); // No call on this client.
        assert_eq!(refusal(&mut idle).as_deref(), Some(close_code::STALE_INPUT));
        assert!(calls.lock().unwrap().is_empty());
    }

    /// The broker's controls for its client, and the responses its caller got.
    fn to_client(
        actions: Vec<crate::server::media::MediaAction>,
    ) -> (Vec<MediaControl>, Vec<serde_json::Value>) {
        let (mut controls, mut responses) = (Vec::new(), Vec::new());
        for action in actions {
            match action {
                crate::server::media::MediaAction::Send { control, .. } => controls.push(control),
                crate::server::media::MediaAction::Respond {
                    respond_to,
                    response,
                } => {
                    responses.push(serde_json::from_str(&response).unwrap());
                    let _ = respond_to.send(response); // The caller waiting on pane.media_open.
                }
            }
        }
        (controls, responses)
    }

    /// The client's controls for the broker, in order.
    fn to_broker(client: &mut ClientMedia) -> Vec<MediaControl> {
        client
            .take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                MediaEffect::Send(_, control) => Some(control),
                _ => None,
            })
            .collect()
    }

    /// A live call through the real broker and client: opened on fresh input, offered, answered, connected.
    fn live_call_through_broker(
        now: Instant,
    ) -> (
        crate::server::media::MediaBroker,
        ClientMedia,
        Calls,
        String,
    ) {
        use crate::layout::PaneId;
        let mut broker = crate::server::media::MediaBroker::new();
        broker.client_connected(1, true);
        let (mut client, calls) = media(MediaMode::Auto, false);
        broker.note_pane_input(1, PaneId::from_raw(7), "pane_1", now);
        client.note_pane_input(&local(), "pane_1", now);
        let (tx, _rx) = std::sync::mpsc::channel();
        let (controls, _) =
            to_client(broker.open("r1".into(), tx, PaneId::from_raw(7), |_| true, now));
        for control in controls {
            client.handle_server_control(&local(), control, true, |_| Some("label".into()), now);
        }
        assert!(
            to_broker(&mut client).is_empty(),
            "the first open is accepted"
        );
        let old = calls
            .lock()
            .unwrap()
            .iter()
            .find_map(|call| match call {
                PeerCall::Start(id) => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        emit(
            &mut client,
            PeerEvent::Offer {
                session_id: old.clone(),
                sdp: "v=0".into(),
            },
        );
        for control in to_broker(&mut client) {
            to_client(broker.client_control(1, control, now));
        }
        broker.answer(&old, "v=0 answer".into()).unwrap();
        broker.client_control(
            1,
            MediaControl::State(MediaStateUpdate {
                session_id: old.clone(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            }),
            now,
        );
        (broker, client, calls, old)
    }

    // herdr#102 review: the broker's actual ordered controls, through the client, then back (smarty-voice#133).
    #[test]
    fn an_idle_renewal_hands_over_in_order_and_the_old_call_ends_only_after_the_client_accepts() {
        use crate::layout::PaneId;
        let now = Instant::now();
        let (mut broker, mut client, calls, old) = live_call_through_broker(now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, rx) = std::sync::mpsc::channel();
        let (controls, _) =
            to_client(broker.open("r2".into(), tx, PaneId::from_raw(7), |_| true, later));
        // 1. Only the new Open: the live call is not closed yet.
        let new = match controls.as_slice() {
            [MediaControl::OpenInitial(initial), MediaControl::Open(open)] => {
                assert_eq!(initial.session_id, open.session_id);
                assert!(!initial.input_muted && !initial.output_suppressed);
                open.session_id.clone()
            }
            other => panic!("expected only the new Open, got {other:?}"),
        };
        assert_eq!(
            broker.state(&old).unwrap().state,
            MediaSessionState::Connected
        );
        // 2. The client accepts it: a renewal of its own call on the pane needs no fresh input.
        for control in controls {
            client.handle_server_control(&local(), control, true, |_| Some("label".into()), later);
        }
        let accepted = to_broker(&mut client);
        assert!(
            accepted
                .iter()
                .all(|c| !matches!(c, MediaControl::Close(close) if close.session_id == new)),
            "not refused: {accepted:?}"
        );
        assert!(calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Start(new.clone())));
        for control in accepted {
            to_client(broker.client_control(1, control, later)); // Its close of the old peer, as replaced.
        }
        // 3. The client's offer (its acknowledgement) completes the handover; the caller gets the new offer.
        emit(
            &mut client,
            PeerEvent::Offer {
                session_id: new.clone(),
                sdp: "v=0 new".into(),
            },
        );
        for control in to_broker(&mut client) {
            to_client(broker.client_control(1, control, later));
        }
        let body: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(body["result"]["session_id"], new.as_str());
        assert_ne!(
            broker.state(&old).map(|view| view.state),
            Some(MediaSessionState::Connected)
        );
    }

    /// Delivers broker actions to the client, in order.
    fn deliver(
        client: &mut ClientMedia,
        actions: Vec<crate::server::media::MediaAction>,
        now: Instant,
    ) -> Vec<serde_json::Value> {
        let (controls, responses) = to_client(actions);
        for control in controls {
            client.handle_server_control(&local(), control, true, |_| Some("label".into()), now);
        }
        responses
    }

    // herdr#102 security pass, P1: a renewal of a muted call keeps it muted from the new peer's start.
    #[test]
    fn a_renewal_of_a_muted_call_starts_its_new_peer_muted() {
        use crate::layout::PaneId;
        let now = Instant::now();
        let (mut broker, mut client, calls, old) = live_call_through_broker(now);
        deliver(&mut client, broker.mute(&old, true).unwrap(), now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, _rx) = std::sync::mpsc::channel();
        deliver(
            &mut client,
            broker.open("r2".into(), tx, PaneId::from_raw(7), |_| true, later),
            later,
        );
        let calls = calls.lock().unwrap().clone();
        let start = calls
            .iter()
            .position(|call| matches!(call, PeerCall::Start(id) if *id != old))
            .expect("the new peer started");
        let PeerCall::Start(new) = &calls[start] else {
            unreachable!()
        };
        assert!(client.session.as_ref().unwrap().desired_input_muted);
        assert_eq!(client.session.as_ref().unwrap().session_id, *new);
        assert!(
            calls[start + 1..].is_empty(),
            "no after-start bootstrap setter"
        );
    }

    #[test]
    fn a_mute_asked_for_during_a_handover_reaches_the_new_peer() {
        use crate::layout::PaneId;
        let now = Instant::now();
        let (mut broker, mut client, calls, old) = live_call_through_broker(now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, _rx) = std::sync::mpsc::channel();
        deliver(
            &mut client,
            broker.open("r2".into(), tx, PaneId::from_raw(7), |_| true, later),
            later,
        );
        for control in to_broker(&mut client) {
            to_client(broker.client_control(1, control, later)); // The client's close of the old peer (replaced).
        }
        let new = calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|call| match call {
                PeerCall::Start(id) => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        // The caller mutes the call it knows (the old session) while the handover is under way.
        let actions = broker.mute(&old, true).unwrap_or_default();
        deliver(&mut client, actions, later);
        assert!(calls.lock().unwrap().contains(&PeerCall::Mute(new, true)));
    }

    // herdr#102 security pass, P2: ending the call cancels its renewal still being handed over.
    #[test]
    fn ending_the_call_during_its_handover_cancels_the_renewal() {
        use crate::layout::PaneId;
        let now = Instant::now();
        let (mut broker, mut client, calls, old) = live_call_through_broker(now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, rx) = std::sync::mpsc::channel();
        deliver(
            &mut client,
            broker.open("r2".into(), tx, PaneId::from_raw(7), |_| true, later),
            later,
        );
        let new = calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|call| match call {
                PeerCall::Start(id) => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        // The caller ends the call before the replacement's offer came back.
        deliver(&mut client, broker.close(&old, later), later);
        assert!(
            calls
                .lock()
                .unwrap()
                .contains(&PeerCall::Close(new.clone())),
            "the new peer is closed"
        );
        let body: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "media_refused");
        assert!(broker.state(&new).map(|view| view.state) != Some(MediaSessionState::Offered));
        // A late offer for the cancelled renewal changes nothing.
        let late = to_client(broker.client_control(
            1,
            MediaControl::Offer(MediaSdp {
                session_id: new.clone(),
                sdp: "v=0 late".into(),
            }),
            later,
        ));
        assert!(late.0.is_empty() && late.1.is_empty());
    }

    #[test]
    fn a_refused_idle_renewal_leaves_the_live_call_intact() {
        use crate::layout::PaneId;
        let now = Instant::now();
        let (mut broker, mut client, calls, old) = live_call_through_broker(now);
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(60);
        let (tx, rx) = std::sync::mpsc::channel();
        let (controls, _) =
            to_client(broker.open("r2".into(), tx, PaneId::from_raw(7), |_| true, later));
        // The client no longer shows the pane: it refuses the new session.
        for control in controls {
            client.handle_server_control(&local(), control, true, |_| None, later);
        }
        for control in to_broker(&mut client) {
            to_client(broker.client_control(1, control, later));
        }
        let body: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "media_refused");
        assert_eq!(
            broker.state(&old).unwrap().state,
            MediaSessionState::Connected,
            "the call goes on"
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .contains(&PeerCall::Close(old.clone())),
            "its peer was not closed"
        );
    }

    #[test]
    fn answer_and_mute_reach_the_owned_peer_only() {
        let (mut media, calls) = started(MediaMode::Auto);
        let now = Instant::now();
        let other = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let answer = |session_id: &str| {
            MediaControl::Answer(MediaSdp {
                session_id: session_id.into(),
                sdp: "answer".into(),
            })
        };
        media.handle_server_control(&local(), answer("m1"), true, |_| None, now);
        media.handle_server_control(&local(), answer("unknown"), true, |_| None, now);
        media.handle_server_control(&other, answer("m1"), true, |_| None, now);
        media.handle_server_control(
            &local(),
            MediaControl::Mute(crate::protocol::media::MediaMute {
                session_id: "m1".into(),
                muted: true,
            }),
            true,
            |_| None,
            now,
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                PeerCall::Start("m1".into()),
                PeerCall::Answer("m1".into(), "answer".into()),
                PeerCall::Mute("m1".into(), true),
            ]
        );
        assert!(media.take_effects().is_empty());
    }

    // Trust boundary: only the owning endpoint may reach the live peer. The native peer
    // owns audio acknowledgement; dispatch must preserve arrival order without inventing one.
    #[test]
    fn device_controls_reach_the_owned_peer_in_arrival_order_without_early_ack() {
        let owner = remote();
        let (mut media, calls) = started_on(MediaMode::Auto, &owner);
        let first = MediaDeviceControl {
            input_muted: Some(true),
            output_suppressed: Some(true),
            levels: Some(true),
            ..device_control("m1", 9)
        };
        let second = MediaDeviceControl {
            input_muted: Some(false),
            output_suppressed: Some(false),
            levels: Some(false),
            ..device_control("m1", 3)
        };
        for control in [&first, &second] {
            // An existing call remains controllable even when its endpoint is not shown.
            media.handle_server_control(
                &owner,
                MediaControl::Control(control.clone()),
                false,
                |_| None,
                Instant::now(),
            );
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                PeerCall::Start("m1".into()),
                PeerCall::Control("m1".into(), first),
                PeerCall::Control("m1".into(), second),
            ]
        );
        assert!(media.take_effects().is_empty(), "no speculative Applied");
    }

    #[test]
    fn device_controls_for_unknown_pending_and_closed_sessions_reply_closed() {
        let (mut media, calls) = started(MediaMode::Auto);
        let now = Instant::now();
        media.handle_server_control(
            &remote(),
            MediaControl::Control(device_control("unknown", 1)),
            true,
            |_| None,
            now,
        );
        assert_eq!(
            media.take_effects(),
            vec![MediaEffect::Send(
                remote(),
                MediaControl::Close(MediaClose::new(
                    "unknown",
                    close_code::CLOSED,
                    "this client has no live peer for that media session",
                )),
            )]
        );
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);

        media.handle_server_control(
            &local(),
            MediaControl::Close(MediaClose::new("m1", close_code::CLOSED, "done")),
            true,
            |_| None,
            now,
        );
        media.take_effects();
        media.handle_server_control(
            &local(),
            MediaControl::Control(device_control("m1", 2)),
            true,
            |_| None,
            now,
        );
        assert_eq!(refusal(&mut media).as_deref(), Some(close_code::CLOSED));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![PeerCall::Start("m1".into()), PeerCall::Close("m1".into())]
        );

        let (mut pending, calls) = started(MediaMode::Ask);
        pending.handle_server_control(
            &local(),
            MediaControl::Control(device_control("m1", 3)),
            true,
            |_| None,
            now,
        );
        assert!(matches!(
            pending.take_effects().as_slice(),
            [MediaEffect::Send(_, MediaControl::ControlFailed(failed))]
                if failed.control_id == Some(3) && failed.code == "awaiting_consent"
        ));
        assert!(pending.pending.is_some(), "no peer before consent");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn device_control_from_the_wrong_endpoint_cannot_reach_or_mute_the_peer() {
        let (mut media, calls) = started(MediaMode::Auto);
        media.handle_server_control(
            &remote(),
            MediaControl::Control(MediaDeviceControl {
                input_muted: Some(true),
                ..device_control("m1", 1)
            }),
            true,
            |_| None,
            Instant::now(),
        );
        assert_eq!(
            media.take_effects(),
            vec![MediaEffect::Send(
                remote(),
                MediaControl::Close(MediaClose::new(
                    "m1",
                    close_code::WRONG_ENDPOINT,
                    "this media session belongs to another server",
                )),
            )]
        );
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert!(!media.session.as_ref().unwrap().desired_input_muted);
    }

    #[test]
    fn current_peer_applied_levels_and_info_go_only_to_its_endpoint() {
        let owner = remote();
        let (mut media, calls) = started_on(MediaMode::Auto, &owner);
        for event in device_events("m1") {
            emit(&mut media, event);
        }
        assert_eq!(
            media.take_effects(),
            vec![
                MediaEffect::Send(
                    owner.clone(),
                    MediaControl::Applied(MediaApplied {
                        session_id: "m1".into(),
                        control_id: 7,
                        input_muted: true,
                        output_suppressed: true,
                    }),
                ),
                MediaEffect::Send(
                    owner.clone(),
                    MediaControl::Levels(MediaLevels {
                        session_id: "m1".into(),
                        input_peak: 0.25,
                        output_peak: 0.5,
                    }),
                ),
                MediaEffect::Send(
                    owner,
                    MediaControl::Info(MediaInfo {
                        session_id: "m1".into(),
                        version: "test-version".into(),
                        commit: "test-commit".into(),
                        pid: 42,
                        input_device: "microphone".into(),
                        output_device: "speaker".into(),
                    }),
                ),
            ]
        );
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
    }

    #[test]
    fn replaced_and_closed_peer_device_events_are_ignored() {
        let (mut media, _) = started(MediaMode::Auto);
        open(&mut media, "m2", "pane_1", Instant::now());
        media.take_effects();
        for event in device_events("m1") {
            emit(&mut media, event);
        }
        assert!(media.take_effects().is_empty(), "old peer was replaced");

        emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m2".into(),
                code: close_code::CLOSED,
                message: "done".into(),
            },
        );
        media.take_effects();
        for event in device_events("m2") {
            emit(&mut media, event);
        }
        assert!(media.take_effects().is_empty(), "current peer is closed");
    }

    // Data safety: a late acknowledgement must not unmute a replacement peer when the
    // latest input mute is still in flight, nor undo a later requested unmute.
    #[test]
    fn renewal_uses_latest_requested_input_mute_not_an_older_applied_event() {
        for desired in [true, false] {
            let (mut media, calls) = started(MediaMode::Auto);
            let now = Instant::now();
            for (control_id, muted) in [(1, !desired), (2, desired)] {
                media.handle_server_control(
                    &local(),
                    MediaControl::Control(MediaDeviceControl {
                        input_muted: Some(muted),
                        ..device_control("m1", control_id)
                    }),
                    true,
                    |_| None,
                    now,
                );
            }
            emit(
                &mut media,
                PeerEvent::Applied(MediaApplied {
                    session_id: "m1".into(),
                    control_id: 1,
                    input_muted: !desired,
                    output_suppressed: false,
                }),
            );
            // Omitting input_muted must retain its desired value.
            media.handle_server_control(
                &local(),
                MediaControl::Control(MediaDeviceControl {
                    output_suppressed: Some(true),
                    levels: Some(true),
                    ..device_control("m1", 3)
                }),
                true,
                |_| None,
                now,
            );
            open(
                &mut media,
                "m2",
                "pane_1",
                now + MEDIA_INPUT_WINDOW + Duration::from_secs(60),
            );
            assert_eq!(media.session.as_ref().unwrap().desired_input_muted, desired);
            let calls = calls.lock().unwrap();
            let start = calls
                .iter()
                .position(|call| *call == PeerCall::Start("m2".into()))
                .expect("renewal starts without fresh input");
            assert_eq!(calls[start - 1], PeerCall::Close("m1".into()));
            assert!(media.session.as_ref().unwrap().desired_output_suppressed);
            assert!(
                calls[start + 1..].is_empty(),
                "initial policy is constructor data"
            );
        }
    }

    #[test]
    fn server_close_releases_the_peer_once() {
        let (mut media, calls) = started(MediaMode::Auto);
        let close = || MediaControl::Close(MediaClose::new("m1", close_code::CLOSED, "done"));
        media.handle_server_control(&local(), close(), true, |_| None, Instant::now());
        media.handle_server_control(&local(), close(), true, |_| None, Instant::now());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![PeerCall::Start("m1".into()), PeerCall::Close("m1".into())]
        );
        assert!(media.session.is_none());
        assert!(closes(&media.take_effects()).is_empty());
    }

    #[test]
    fn server_close_cancels_a_pending_prompt() {
        let (mut media, _) = started(MediaMode::Ask);
        let close = MediaControl::Close(MediaClose::new("m1", close_code::CLOSED, "done"));
        media.handle_server_control(&local(), close, true, |_| None, Instant::now());
        assert_eq!(
            media.take_effects(),
            vec![MediaEffect::CancelConsent {
                session_id: "m1".into()
            }]
        );
        assert_eq!(media.deadline(), None);
    }

    #[test]
    fn endpoint_disconnect_closes_the_peer_without_a_message() {
        let (mut media, calls) = started(MediaMode::Auto);
        media.endpoint_gone(&local());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![PeerCall::Start("m1".into()), PeerCall::Close("m1".into())]
        );
        assert!(closes(&media.take_effects()).is_empty());
    }

    #[test]
    fn peer_closed_event_reports_its_code_and_drops_the_session() {
        let (mut media, _) = started(MediaMode::Auto);
        emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m1".into(),
                code: close_code::DEVICE_ERROR,
                message: "x".repeat(MAX_MEDIA_TEXT_BYTES + 10),
            },
        );
        let effects = media.take_effects();
        assert_eq!(
            closes(&effects),
            vec![("m1".to_owned(), Some(close_code::DEVICE_ERROR.to_owned()))]
        );
        assert!(media.session.is_none());
        let MediaEffect::Send(_, control) = &effects[0] else {
            panic!("close is sent first");
        };
        assert!(control.validate().is_ok());
    }

    #[derive(Clone, Default)]
    struct Recording {
        calls: Calls,
        initials: Arc<Mutex<Vec<(String, PeerInitialState)>>>,
        sinks: Arc<Mutex<Vec<PeerEventSink>>>,
        events: Arc<Mutex<Vec<PeerEvent>>>,
    }

    fn recording_media(
        mode: MediaMode,
        command_error: Option<PeerCommandError>,
    ) -> (ClientMedia, Recording) {
        let recording = Recording::default();
        let factory_recording = recording.clone();
        let factory: PeerFactory = Box::new(move |session_id, initial, sink| {
            factory_recording
                .calls
                .lock()
                .unwrap()
                .push(PeerCall::Start(session_id.clone()));
            factory_recording
                .initials
                .lock()
                .unwrap()
                .push((session_id.clone(), initial));
            factory_recording.sinks.lock().unwrap().push(sink);
            if session_id.starts_with("bad") {
                return Err("audio.open failed".to_owned());
            }
            Ok(Box::new(FakePeer {
                session_id,
                calls: factory_recording.calls.clone(),
                command_error,
            }) as Box<dyn MediaPeer>)
        });
        let events = recording.events.clone();
        let sink: PeerEventSink = Arc::new(move |event| {
            let events = events.clone();
            Box::pin(async move {
                events.lock().unwrap().push(event);
                Ok(())
            })
        });
        let mut media = ClientMedia::new(mode, factory, sink);
        media.peer_available = true;
        (media, recording)
    }

    fn open_for(
        media: &mut ClientMedia,
        endpoint: &ClientEndpointId,
        session_id: &str,
        pane_id: &str,
        initial: Option<PeerInitialState>,
        now: Instant,
    ) {
        let control = match initial {
            Some(initial) => MediaControl::OpenInitial(MediaOpenInitial {
                session_id: session_id.into(),
                pane_id: pane_id.into(),
                input_muted: initial.input_muted,
                output_suppressed: initial.output_suppressed,
            }),
            None => MediaControl::Open(MediaOpen {
                session_id: session_id.into(),
                pane_id: pane_id.into(),
            }),
        };
        media.handle_server_control(endpoint, control, true, |_| Some("pane".into()), now);
    }

    fn deliver_recorded(
        media: &mut ClientMedia,
        recording: &Recording,
        peer_index: usize,
        event: PeerEvent,
    ) {
        let sink = recording.sinks.lock().unwrap()[peer_index].clone();
        let mut future = sink(event);
        assert!(
            recording.events.lock().unwrap().is_empty(),
            "creation is not delivery"
        );
        assert_eq!(
            future
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
            std::task::Poll::Ready(Ok(()))
        );
        let event = recording.events.lock().unwrap().pop().unwrap();
        media.handle_peer_event(event);
    }

    fn protective() -> PeerInitialState {
        PeerInitialState {
            input_muted: true,
            output_suppressed: true,
        }
    }

    // C1 / privacy: the existing one-physical-peer boundary must remain explicit. This
    // constructor test is not native callback or physical failed-handover preservation proof.
    #[test]
    fn failed_successor_closes_source_before_factory_and_retains_initial_privacy() {
        let (mut media, recording) = recording_media(MediaMode::Auto, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open_for(
            &mut media,
            &local(),
            "source",
            "pane_1",
            Some(protective()),
            now,
        );
        media.take_effects();
        let later = now + MEDIA_INPUT_WINDOW + Duration::from_secs(1);
        open_for(
            &mut media,
            &local(),
            "bad-successor",
            "pane_1",
            Some(protective()),
            later,
        );
        assert_eq!(
            *recording.calls.lock().unwrap(),
            vec![
                PeerCall::Start("source".into()),
                PeerCall::Close("source".into()),
                PeerCall::Start("bad-successor".into())
            ]
        );
        assert_eq!(recording.initials.lock().unwrap()[1].1, protective());
        assert!(
            media.session.is_none(),
            "failed successor leaves no physical source"
        );
        assert_eq!(
            closes(&media.take_effects()),
            vec![
                ("source".into(), Some(close_code::REPLACED.into())),
                (
                    "bad-successor".into(),
                    Some(close_code::DEVICE_ERROR.into())
                ),
            ]
        );
        open_for(&mut media, &local(), "bad-successor", "pane_1", None, later);
        assert!(
            media.take_effects().is_empty(),
            "v1 alternate cannot retry failure"
        );
        assert_eq!(recording.initials.lock().unwrap().len(), 2);
        open_for(&mut media, &local(), "stale", "pane_1", None, later);
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::STALE_INPUT)
        );
        media.note_pane_input(&local(), "pane_1", later);
        open_for(&mut media, &local(), "safe", "pane_1", None, later);
        assert_eq!(recording.initials.lock().unwrap()[2].1, protective());
        // A complete false pair clears remembered protection even if that factory fails.
        open_for(
            &mut media,
            &local(),
            "bad-clear",
            "pane_1",
            Some(PeerInitialState::default()),
            later,
        );
        media.take_effects();
        open_for(&mut media, &local(), "bad-clear", "pane_1", None, later);
        assert!(media.take_effects().is_empty());
        assert_eq!(recording.initials.lock().unwrap().len(), 4);
        open_for(&mut media, &local(), "clear", "pane_1", None, later);
        assert_eq!(
            recording.initials.lock().unwrap()[4].1,
            PeerInitialState::default()
        );
        assert!(!recording
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| matches!(call, PeerCall::Mute(..))));
    }

    // C2 / old-server direction: observations never overwrite requested per-key safety.
    #[test]
    fn legacy_guard_survives_close_off_disconnect_and_explicit_false_patches_each_axis() {
        for initial in [
            PeerInitialState {
                input_muted: true,
                output_suppressed: false,
            },
            PeerInitialState {
                input_muted: false,
                output_suppressed: true,
            },
            protective(),
        ] {
            for ending in 0..4 {
                let (mut media, recording) = recording_media(MediaMode::Auto, None);
                let now = Instant::now();
                media.note_pane_input(&local(), "pane_1", now);
                open_for(&mut media, &local(), "m1", "pane_1", None, now);
                media.handle_server_control(
                    &local(),
                    MediaControl::Control(MediaDeviceControl {
                        input_muted: Some(initial.input_muted),
                        output_suppressed: Some(initial.output_suppressed),
                        ..device_control("m1", 1)
                    }),
                    true,
                    |_| None,
                    now,
                );
                emit(
                    &mut media,
                    PeerEvent::State {
                        session_id: "m1".into(),
                        state: MediaPeerState::Connected,
                        muted: false,
                        detail: None,
                    },
                );
                match ending {
                    0 => emit(
                        &mut media,
                        PeerEvent::Closed {
                            session_id: "m1".into(),
                            code: close_code::DEVICE_CHANGED,
                            message: "changed".into(),
                        },
                    ),
                    1 => media.handle_server_control(
                        &local(),
                        MediaControl::Close(MediaClose::new("m1", close_code::CLOSED, "caller")),
                        true,
                        |_| None,
                        now,
                    ),
                    2 => {
                        media.set_mode(MediaMode::Off);
                        media.set_mode(MediaMode::Auto);
                    }
                    _ => media.endpoint_gone(&local()),
                }
                media.take_effects();
                assert_eq!(media.legacy_intents[&(local(), "pane_1".into())], initial);
                media.note_pane_input(&local(), "pane_1", now);
                open_for(&mut media, &local(), "m2", "pane_1", None, now);
                assert_eq!(recording.initials.lock().unwrap()[1].1, initial);
                media.handle_server_control(
                    &local(),
                    MediaControl::Control(MediaDeviceControl {
                        input_muted: Some(false),
                        ..device_control("m2", 2)
                    }),
                    true,
                    |_| None,
                    now,
                );
                // A delayed older observation cannot relatch input or clear output.
                emit(
                    &mut media,
                    PeerEvent::Applied(MediaApplied {
                        session_id: "m2".into(),
                        control_id: 1,
                        input_muted: true,
                        output_suppressed: false,
                    }),
                );
                emit(
                    &mut media,
                    PeerEvent::Closed {
                        session_id: "m2".into(),
                        code: close_code::DEVICE_CHANGED,
                        message: "changed".into(),
                    },
                );
                open_for(&mut media, &local(), "m3", "pane_1", None, now);
                let after_input_false = PeerInitialState {
                    input_muted: false,
                    output_suppressed: initial.output_suppressed,
                };
                assert_eq!(recording.initials.lock().unwrap()[2].1, after_input_false);
                media.handle_server_control(
                    &local(),
                    MediaControl::Control(MediaDeviceControl {
                        output_suppressed: Some(false),
                        ..device_control("m3", 3)
                    }),
                    true,
                    |_| None,
                    now,
                );
                emit(
                    &mut media,
                    PeerEvent::Closed {
                        session_id: "m3".into(),
                        code: close_code::CLOSED,
                        message: "done".into(),
                    },
                );
                open_for(&mut media, &local(), "m4", "pane_1", None, now);
                assert_eq!(
                    recording.initials.lock().unwrap()[3].1,
                    PeerInitialState::default()
                );
                // New-server direction: full Initial(false,false) beats cached true/true.
                media.handle_server_control(
                    &local(),
                    MediaControl::Control(MediaDeviceControl {
                        input_muted: Some(true),
                        output_suppressed: Some(true),
                        ..device_control("m4", 4)
                    }),
                    true,
                    |_| None,
                    now,
                );
                open_for(
                    &mut media,
                    &local(),
                    "m5",
                    "pane_1",
                    Some(PeerInitialState::default()),
                    now,
                );
                assert_eq!(
                    recording.initials.lock().unwrap()[4].1,
                    PeerInitialState::default()
                );
                open_for(&mut media, &local(), "m5", "pane_1", None, now);
                assert_eq!(
                    recording.initials.lock().unwrap().len(),
                    5,
                    "same-ID alternate opens once"
                );
            }
        }
    }

    #[test]
    fn initial_v1_alternate_cannot_bypass_denial_off_stale_input_or_factory_error() {
        for scenario in 0..4 {
            let mode = match scenario {
                0 => MediaMode::Ask,
                1 => MediaMode::Off,
                _ => MediaMode::Auto,
            };
            let (mut media, recording) = recording_media(mode, None);
            let now = Instant::now();
            let session_id = if scenario == 3 { "bad-open" } else { "m1" };
            if scenario != 2 {
                media.note_pane_input(&local(), "pane_1", now);
            }
            open_for(
                &mut media,
                &local(),
                session_id,
                "pane_1",
                Some(protective()),
                now,
            );
            if scenario == 0 {
                media.consent(session_id, false);
            }
            assert!(!media.take_effects().is_empty());
            let attempts = recording.initials.lock().unwrap().len();
            open_for(&mut media, &local(), session_id, "pane_1", None, now);
            assert!(
                media.take_effects().is_empty(),
                "alternate has no second refusal or prompt"
            );
            assert_eq!(recording.initials.lock().unwrap().len(), attempts);
            assert_eq!(attempts, usize::from(scenario == 3));
            assert!(media.session.is_none());
            assert!(media.pending.is_none());
        }
    }

    // The guard reserves even false/false, and cannot evict a live or pending false key.
    #[test]
    fn legacy_guard_capacity_refuses_without_eviction_and_reclaims_only_unused_false() {
        for mode in [MediaMode::Auto, MediaMode::Ask] {
            let (mut media, recording) = recording_media(mode, None);
            let now = Instant::now();
            for index in 0..MAX_LEGACY_INTENTS {
                let pane = format!("pane_{index}");
                let initial = if index + 1 == MAX_LEGACY_INTENTS {
                    PeerInitialState::default()
                } else {
                    protective()
                };
                media.note_pane_input(&local(), &pane, now);
                open_for(
                    &mut media,
                    &local(),
                    &format!("m{index}"),
                    &pane,
                    Some(initial),
                    now,
                );
                media.take_effects();
            }
            let attempts = recording.initials.lock().unwrap().len();
            media.note_pane_input(&local(), "overflow", now);
            open_for(
                &mut media,
                &local(),
                "overflow",
                "overflow",
                Some(protective()),
                now,
            );
            assert_eq!(
                refusal(&mut media).as_deref(),
                Some(close_code::INTENT_CAPACITY)
            );
            assert_eq!(recording.initials.lock().unwrap().len(), attempts);
            assert_eq!(media.legacy_intents.len(), MAX_LEGACY_INTENTS);
            assert!(!media
                .legacy_intents
                .contains_key(&(local(), "overflow".into())));
            let last = format!("m{}", MAX_LEGACY_INTENTS - 1);
            assert!(
                media
                    .session
                    .as_ref()
                    .is_some_and(|session| session.session_id == last)
                    || media
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.session_id == last)
            );
            media.handle_server_control(
                &local(),
                MediaControl::Close(MediaClose::new(last, close_code::CLOSED, "done")),
                true,
                |_| None,
                now,
            );
            media.take_effects();
            open_for(
                &mut media,
                &local(),
                "next",
                "overflow",
                Some(protective()),
                now,
            );
            assert!(closes(&media.take_effects()).is_empty());
            assert_eq!(media.legacy_intents.len(), MAX_LEGACY_INTENTS);
            assert_eq!(
                media.legacy_intents[&(local(), "pane_0".into())],
                protective()
            );
            assert!(!media
                .legacy_intents
                .contains_key(&(local(), format!("pane_{}", MAX_LEGACY_INTENTS - 1))));
            // A full guard of protected keys refuses a new endpoint too, without stealing
            // another key's policy or closing the existing endpoint's live/pending target.
            media.note_pane_input(&remote(), "pane_0", now);
            open_for(&mut media, &remote(), "other", "pane_0", None, now);
            assert_eq!(
                refusal(&mut media).as_deref(),
                Some(close_code::INTENT_CAPACITY)
            );
        }
    }

    #[test]
    fn pending_consent_retains_privacy_but_terminally_rejects_device_controls_without_native() {
        let (mut media, recording) = recording_media(MediaMode::Ask, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open_for(
            &mut media,
            &local(),
            "m1",
            "pane_1",
            Some(protective()),
            now,
        );
        media.take_effects();
        media.handle_server_control(
            &remote(),
            MediaControl::Control(device_control("m1", 1)),
            true,
            |_| None,
            now,
        );
        assert_eq!(
            refusal(&mut media).as_deref(),
            Some(close_code::WRONG_ENDPOINT)
        );
        media.handle_server_control(
            &local(),
            MediaControl::Control(MediaDeviceControl {
                input_muted: Some(false),
                levels: Some(true),
                ..device_control("m1", 2)
            }),
            true,
            |_| None,
            now,
        );
        let effects = media.take_effects();
        assert!(
            matches!(effects.as_slice(), [MediaEffect::Send(endpoint, MediaControl::ControlFailed(failed))]
            if *endpoint == local() && failed.control_id == Some(2) && failed.code == "awaiting_consent")
        );
        assert!(media.pending.is_some());
        assert!(
            recording.calls.lock().unwrap().is_empty(),
            "no native work before yes"
        );
        media.handle_server_control(
            &local(),
            MediaControl::Mute(crate::protocol::media::MediaMute {
                session_id: "m1".into(),
                muted: true,
            }),
            true,
            |_| None,
            now,
        );
        assert!(media.take_effects().is_empty());
        assert_eq!(media.pending.as_ref().unwrap().initial, protective());
        media.consent("m1", true);
        assert_eq!(
            *recording.initials.lock().unwrap(),
            vec![("m1".into(), protective())]
        );
        assert_eq!(
            *recording.calls.lock().unwrap(),
            vec![PeerCall::Start("m1".into())]
        );
        assert!(!media.take_effects().iter().any(|effect| matches!(
            effect,
            MediaEffect::Send(_, MediaControl::Applied(_) | MediaControl::Levels(_))
        )));
    }

    #[test]
    fn peer_scopes_reject_unscoped_nested_and_equal_session_from_another_endpoint() {
        let (mut media, recording) = recording_media(MediaMode::Auto, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open_for(
            &mut media,
            &local(),
            "same",
            "pane_1",
            Some(protective()),
            now,
        );
        let old_generation = media.session.as_ref().unwrap().generation;
        media.note_pane_input(&local(), "pane_2", now);
        open_for(&mut media, &local(), "different", "pane_2", None, now);
        assert_eq!(
            recording.initials.lock().unwrap()[1].1,
            PeerInitialState::default(),
            "no cross-pane intent"
        );
        media.note_pane_input(&local(), "pane_1", now);
        open_for(
            &mut media,
            &local(),
            "same",
            "pane_1",
            Some(protective()),
            now,
        );
        assert_eq!(recording.initials.lock().unwrap()[2].1, protective());
        media.note_pane_input(&remote(), "pane_1", now);
        open_for(&mut media, &remote(), "same", "pane_1", None, now);
        assert_eq!(
            recording.initials.lock().unwrap()[3].1,
            PeerInitialState::default(),
            "equal session text is not a duplicate across endpoints or a policy key"
        );
        media.take_effects();
        let mut old_events = device_events("same").to_vec();
        old_events.extend([
            PeerEvent::Closed {
                session_id: "same".into(),
                code: close_code::CLOSED,
                message: "old".into(),
            },
            PeerEvent::Offer {
                session_id: "same".into(),
                sdp: "v=0".into(),
            },
            PeerEvent::State {
                session_id: "same".into(),
                state: MediaPeerState::Failed,
                muted: true,
                detail: None,
            },
            PeerEvent::ControlFailed(MediaControlFailed {
                session_id: "same".into(),
                control_id: Some(1),
                code: "control_timeout".into(),
                message: None,
            }),
        ]);
        for event in old_events {
            media.handle_peer_event(event.clone());
            deliver_recorded(&mut media, &recording, 0, event.clone());
            deliver_recorded(
                &mut media,
                &recording,
                3,
                PeerEvent::Scoped {
                    generation: old_generation,
                    event: Box::new(event),
                },
            );
        }
        assert!(media.take_effects().is_empty());
        assert_eq!(media.session.as_ref().unwrap().endpoint_id, remote());
        for event in device_events("same") {
            deliver_recorded(&mut media, &recording, 3, event);
        }
        assert!(media.take_effects().iter().all(
            |effect| matches!(effect, MediaEffect::Send(endpoint, _) if *endpoint == remote())
        ));
        assert_eq!(
            media.legacy_intents[&(remote(), "pane_1".into())],
            PeerInitialState::default()
        );
        // Markers are endpoint-local and are cleared on disconnect; protected memory is not.
        media.endpoint_gone(&local());
        assert!(!media.initial_alternates.contains_key(&local()));
        assert_eq!(
            media.legacy_intents[&(local(), "pane_1".into())],
            protective()
        );
    }

    #[test]
    fn old_wire_mute_guard_survives_dead_peer_until_owned_explicit_unmute() {
        use crate::protocol::media::{MEDIA_MUTE_KIND, MEDIA_OPEN_KIND};
        let (mut media, recording) = recording_media(MediaMode::Auto, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        let wire_open = |session_id: &str| {
            MediaControl::decode(
                MEDIA_OPEN_KIND,
                &format!(r#"{{"session_id":"{session_id}","pane_id":"pane_1"}}"#,),
            )
            .unwrap()
            .unwrap()
        };
        let wire_mute = |session_id: &str, muted: bool| {
            MediaControl::decode(
                MEDIA_MUTE_KIND,
                &format!(r#"{{"session_id":"{session_id}","muted":{muted}}}"#,),
            )
            .unwrap()
            .unwrap()
        };
        media.handle_server_control(
            &local(),
            wire_open("m1"),
            true,
            |_| Some("pane".into()),
            now,
        );
        media.handle_server_control(&local(), wire_mute("m1", true), true, |_| None, now);
        emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m1".into(),
                code: close_code::DEVICE_CHANGED,
                message: "changed".into(),
            },
        );
        // A dead or foreign session cannot use legacy Mute as authority to clear a key.
        media.handle_server_control(&local(), wire_mute("m1", false), true, |_| None, now);
        media.handle_server_control(
            &local(),
            wire_open("m2"),
            true,
            |_| Some("pane".into()),
            now,
        );
        assert!(recording.initials.lock().unwrap()[1].1.input_muted);
        media.handle_server_control(&remote(), wire_mute("m2", false), true, |_| None, now);
        assert!(media.legacy_intents[&(local(), "pane_1".into())].input_muted);
        media.handle_server_control(&local(), wire_mute("m2", false), true, |_| None, now);
        emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m2".into(),
                code: close_code::CLOSED,
                message: "done".into(),
            },
        );
        media.handle_server_control(
            &local(),
            wire_open("m3"),
            true,
            |_| Some("pane".into()),
            now,
        );
        assert_eq!(
            recording.initials.lock().unwrap()[2].1,
            PeerInitialState::default()
        );
    }

    #[test]
    fn new_server_v1_only_decoder_opens_once_only_for_unprotected_pair() {
        use crate::layout::PaneId;
        use crate::protocol::media::MEDIA_OPEN_KIND;

        // Pin the old decoder's load-bearing Open shape: unknown named kinds are ignored.
        let v1_opens = |controls: &[MediaControl]| -> Vec<MediaOpen> {
            controls
                .iter()
                .filter_map(|control| {
                    let crate::protocol::ServerMessage::EndpointControl { kind, data } =
                        control.server_message().unwrap()
                    else {
                        panic!("media uses the stable named JSON envelope")
                    };
                    (kind == MEDIA_OPEN_KIND).then(|| serde_json::from_str(&data).unwrap())
                })
                .collect()
        };
        let now = Instant::now();
        let mut ordinary = crate::server::media::MediaBroker::new();
        ordinary.client_connected(1, true);
        ordinary.note_pane_input(1, PaneId::from_raw(7), "pane_1", now);
        let (tx, _rx) = std::sync::mpsc::channel();
        let (controls, _) =
            to_client(ordinary.open("ordinary".into(), tx, PaneId::from_raw(7), |_| true, now));
        let old_opens = v1_opens(&controls);
        assert_eq!(old_opens.len(), 1);
        assert!(
            matches!(controls.as_slice(), [MediaControl::OpenInitial(initial), MediaControl::Open(open)]
            if !initial.input_muted && !initial.output_suppressed && initial.session_id == open.session_id && initial.pane_id == open.pane_id)
        );
        assert_eq!(old_opens[0].pane_id, "pane_1");
        for pair in [
            PeerInitialState {
                input_muted: true,
                output_suppressed: false,
            },
            PeerInitialState {
                input_muted: false,
                output_suppressed: true,
            },
            protective(),
        ] {
            let (mut broker, _client, _calls, old) = live_call_through_broker(now);
            broker
                .dispatch_control(
                    &old,
                    Some(pair.input_muted),
                    Some(pair.output_suppressed),
                    None,
                    now,
                )
                .unwrap();
            broker.close(&old, now);
            broker.note_pane_input(1, PaneId::from_raw(7), "pane_1", now);
            let (tx, _rx) = std::sync::mpsc::channel();
            let (controls, _) =
                to_client(broker.open("protected".into(), tx, PaneId::from_raw(7), |_| true, now));
            assert!(
                v1_opens(&controls).is_empty(),
                "an old client cannot open a protected pair"
            );
            assert!(
                matches!(controls.as_slice(), [MediaControl::OpenInitial(initial)]
                if initial.input_muted == pair.input_muted && initial.output_suppressed == pair.output_suppressed)
            );
        }
    }

    #[test]
    fn consent_bootstrap_uses_latest_same_key_request_not_older_prompt_snapshot() {
        let (mut media, recording) = recording_media(MediaMode::Auto, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open_for(&mut media, &local(), "source", "pane_1", None, now);
        media.set_mode(MediaMode::Ask);
        open_for(
            &mut media,
            &local(),
            "successor",
            "pane_1",
            Some(PeerInitialState::default()),
            now,
        );
        assert!(media.pending.is_some());
        assert_eq!(recording.initials.lock().unwrap().len(), 1);
        media.handle_server_control(
            &local(),
            MediaControl::Mute(crate::protocol::media::MediaMute {
                session_id: "source".into(),
                muted: true,
            }),
            true,
            |_| None,
            now,
        );
        media.consent("successor", true);
        assert!(recording.initials.lock().unwrap()[1].1.input_muted);
        assert!(media.session.as_ref().unwrap().desired_input_muted);
    }

    #[test]
    fn peer_generation_exhaustion_preserves_source_and_scoped_sink_propagates_delivery_error() {
        let (mut media, recording) = recording_media(MediaMode::Auto, None);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open_for(&mut media, &local(), "source", "pane_1", None, now);
        media.take_effects();
        media.peer_generation = u64::MAX;
        open_for(&mut media, &local(), "successor", "pane_1", None, now);
        assert_eq!(
            closes(&media.take_effects()),
            vec![("successor".into(), Some(close_code::CLOSED.into()))]
        );
        assert_eq!(
            *recording.calls.lock().unwrap(),
            vec![PeerCall::Start("source".into())]
        );
        assert_eq!(media.session.as_ref().unwrap().session_id, "source");
        let (mut failing, recorded) = recording_media(MediaMode::Auto, None);
        let sink: PeerEventSink =
            Arc::new(|_| Box::pin(async { Err(peer::PeerEventSinkError::ConsumerClosed) }));
        failing.sink = sink;
        failing.note_pane_input(&local(), "pane_1", now);
        open_for(&mut failing, &local(), "m1", "pane_1", None, now);
        let wrapped = recorded.sinks.lock().unwrap()[0].clone();
        let mut future = wrapped(device_events("m1")[0].clone());
        assert_eq!(
            future
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
            std::task::Poll::Ready(Err(peer::PeerEventSinkError::ConsumerClosed))
        );
    }

    #[test]
    fn rejected_peer_commands_close_only_the_owned_target_and_retain_policy() {
        for error in [PeerCommandError::Full, PeerCommandError::Closed] {
            for command in 0..3 {
                let (mut media, recording) = recording_media(MediaMode::Auto, Some(error));
                let now = Instant::now();
                media.note_pane_input(&local(), "pane_1", now);
                open_for(
                    &mut media,
                    &local(),
                    "m1",
                    "pane_1",
                    Some(protective()),
                    now,
                );
                media.take_effects();
                media.handle_server_control(
                    &remote(),
                    MediaControl::Control(device_control("m1", 7)),
                    true,
                    |_| None,
                    now,
                );
                assert_eq!(
                    refusal(&mut media).as_deref(),
                    Some(close_code::WRONG_ENDPOINT)
                );
                assert!(
                    media.session.is_some(),
                    "wrong endpoint did not reach/reject native"
                );
                let control = match command {
                    0 => MediaControl::Answer(MediaSdp {
                        session_id: "m1".into(),
                        sdp: "answer".into(),
                    }),
                    1 => MediaControl::Mute(crate::protocol::media::MediaMute {
                        session_id: "m1".into(),
                        muted: true,
                    }),
                    _ => MediaControl::Control(MediaDeviceControl {
                        input_muted: Some(true),
                        ..device_control("m1", 8)
                    }),
                };
                media.handle_server_control(&local(), control, true, |_| None, now);
                let expected = match error {
                    PeerCommandError::Full => close_code::CONTROL_QUEUE_FULL,
                    PeerCommandError::Closed => close_code::CLOSED,
                };
                let effects = media.take_effects();
                assert_eq!(closes(&effects), vec![("m1".into(), Some(expected.into()))]);
                assert!(effects
                    .iter()
                    .filter_map(|effect| match effect {
                        MediaEffect::Send(endpoint, _) => Some(endpoint),
                        _ => None,
                    })
                    .all(|endpoint| *endpoint == local()));
                assert!(media.session.is_none());
                assert_eq!(
                    recording.calls.lock().unwrap().last(),
                    Some(&PeerCall::Close("m1".into()))
                );
                assert_eq!(
                    media.legacy_intents[&(local(), "pane_1".into())],
                    protective()
                );
                deliver_recorded(&mut media, &recording, 0, device_events("m1")[0].clone());
                assert!(
                    media.take_effects().is_empty(),
                    "late outcome cannot reopen target"
                );
            }
        }
    }
}
