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

use self::peer::{MediaPeer, PeerEvent, PeerEventSink, PeerFactory};
use super::endpoint::ClientEndpointId;
use crate::config::MediaMode;
use crate::protocol::media::{
    close_code, MediaClose, MediaControl, MediaEndOrigin, MediaEnded, MediaOpen, MediaSdp,
    MediaStateUpdate, MediaTeardownStuck, MAX_MEDIA_TEXT_BYTES, MEDIA_INPUT_WINDOW,
};

/// The consent prompt declines on its own after this long.
pub(super) const MEDIA_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Work for the client loop after a controller call.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    owner: u64,
    /// The pane this call is for, so a renewal of it can be recognised (smarty-voice#133).
    pane_id: String,
    /// The mute last applied to this call's peer: a renewal's new peer starts with it (herdr#102 security pass).
    muted: bool,
    peer: Box<dyn MediaPeer>,
}

struct PendingConsent {
    endpoint_id: ClientEndpointId,
    session_id: String,
    pane_id: String,
    deadline: Instant,
}

/// Receipt custody outlives the current prompt/peer. Session ids are unique only within
/// one endpoint connection; settled entries fence duplicates there, not on other endpoints.
struct MediaAttempt {
    owner: Option<u64>,
    endpoint_id: ClientEndpointId,
    generation: String,
    attempt: String,
    origin: Option<MediaEndOrigin>,
    request_id: Option<String>,
    started: bool,
    settled: bool,
}

pub(super) struct ClientMedia {
    mode: MediaMode,
    peer_available: bool,
    factory: PeerFactory,
    sink: PeerEventSink,
    /// This client's last pane input per endpoint: `(pane_id, when)`.
    last_input: HashMap<ClientEndpointId, (String, Instant)>,
    /// The user allowed media once in this client process.
    allowed: bool,
    pending: Option<PendingConsent>,
    session: Option<MediaSession>,
    attempts: HashMap<(ClientEndpointId, String), MediaAttempt>,
    /// Immutable factory invocation bindings, including legacy peers without receipts.
    owners: HashMap<u64, (ClientEndpointId, String)>,
    next_owner: u64,
    effects: Vec<MediaEffect>,
}

impl ClientMedia {
    pub(super) fn new(mode: MediaMode, factory: PeerFactory, sink: PeerEventSink) -> Self {
        Self {
            mode,
            peer_available: peer::NATIVE_PEER_AVAILABLE,
            factory,
            sink,
            last_input: HashMap::new(),
            allowed: false,
            pending: None,
            session: None,
            attempts: HashMap::new(),
            owners: HashMap::new(),
            next_owner: 1,
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
                self.handle_open(endpoint_id, open, endpoint_shown, pane_label, now);
            }
            MediaControl::Answer(answer) => {
                if let Some(session) = self.owned_session(endpoint_id, &answer.session_id) {
                    session.peer.apply_answer(answer.sdp);
                }
            }
            MediaControl::Mute(mute) => {
                if let Some(session) = self.owned_session(endpoint_id, &mute.session_id) {
                    session.muted = mute.muted;
                    session.peer.set_muted(mute.muted);
                }
            }
            MediaControl::Close(close) => {
                // Closing predecessors still own receipt custody. A stale or wrong endpoint
                // must not change the reason/correlation of a different attempt.
                if self
                    .attempts
                    .get(&(endpoint_id.clone(), close.session_id.clone()))
                    .is_some_and(|attempt| &attempt.endpoint_id == endpoint_id && !attempt.settled)
                {
                    self.mark_end(
                        endpoint_id,
                        &close.session_id,
                        close.origin.unwrap_or(MediaEndOrigin::Natural),
                        close.request_id,
                    );
                }
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
            | MediaControl::Ended(_)
            | MediaControl::TeardownStuck(_) => {}
        }
    }

    fn handle_open(
        &mut self,
        endpoint_id: &ClientEndpointId,
        open: MediaOpen,
        endpoint_shown: bool,
        pane_label: impl FnOnce(&str) -> Option<String>,
        now: Instant,
    ) {
        let MediaOpen {
            session_id,
            pane_id,
            generation,
            attempt,
        } = open;
        let duplicate = self
            .attempts
            .contains_key(&(endpoint_id.clone(), session_id.clone()))
            || self.session.as_ref().is_some_and(|session| {
                &session.endpoint_id == endpoint_id && session.session_id == session_id
            })
            || self.pending.as_ref().is_some_and(|pending| {
                &pending.endpoint_id == endpoint_id && pending.session_id == session_id
            });
        if duplicate {
            return;
        }
        if let (Some(generation), Some(attempt)) = (generation, attempt) {
            self.attempts.insert(
                (endpoint_id.clone(), session_id.clone()),
                MediaAttempt {
                    owner: None,
                    endpoint_id: endpoint_id.clone(),
                    generation,
                    attempt,
                    origin: None,
                    request_id: None,
                    started: false,
                    settled: false,
                },
            );
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
        if self.mode == MediaMode::Auto || self.allowed {
            return self.start(endpoint_id.clone(), session_id, pane_id);
        }
        // `&mut self` is the opener guard: consent, close and start are serialized
        // by the client loop. Removing the prompt seals it before publishing completion.
        // A newer request supersedes an unanswered prompt.
        if let Some(previous) = self.pending.take() {
            self.mark_end(
                &previous.endpoint_id,
                &previous.session_id,
                MediaEndOrigin::Replaced,
                None,
            );
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
            self.start(pending.endpoint_id, pending.session_id, pending.pane_id);
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
        let PeerEvent::Scoped { owner, event } = event else {
            // Bare events have no endpoint or factory lineage and cannot adopt custody.
            return;
        };
        let Some((endpoint_id, current)) = self.owners.get(&owner).cloned() else {
            return;
        };
        let event_session = match event.as_ref() {
            PeerEvent::Offer { session_id, .. }
            | PeerEvent::State { session_id, .. }
            | PeerEvent::Closed { session_id, .. }
            | PeerEvent::Teardown { session_id, .. } => session_id,
            PeerEvent::Scoped { .. } => return,
        };
        if event_session != &current {
            return;
        }
        // A replaced peer retains its original receipt custody, but never the successor's.
        if let PeerEvent::Teardown {
            acquired, success, ..
        } = *event
        {
            if self
                .attempts
                .get(&(endpoint_id.clone(), current.clone()))
                .is_some_and(|attempt| attempt.started && attempt.owner == Some(owner))
            {
                self.complete_attempt(&endpoint_id, &current, acquired, success);
            }
            self.owners.remove(&owner);
            return;
        }
        if !self.session.as_ref().is_some_and(|session| {
            session.owner == owner
                && session.endpoint_id == endpoint_id
                && session.session_id == current
        }) {
            return;
        }
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
            PeerEvent::Closed {
                session_id,
                code,
                message,
            } if session_id == current => {
                // Closed is not join evidence. Initiate idempotent close/reaping and
                // retain the attempt independently until Teardown arrives.
                self.mark_end(&endpoint_id, &session_id, MediaEndOrigin::Natural, None);
                if let Some(mut session) = self.session.take() {
                    session.peer.close();
                }
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
        // Disconnect is unknown, never completion. Clear custody before closing peers
        // or prompts, so neither synchronous cleanup nor queued teardown can send proof.
        self.attempts
            .retain(|_, attempt| &attempt.endpoint_id != endpoint_id);
        self.owners
            .retain(|_, (endpoint, _)| endpoint != endpoint_id);
        self.effects.retain(
            |effect| !matches!(effect, MediaEffect::Send(endpoint, _) if endpoint == endpoint_id),
        );
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

    fn start(&mut self, endpoint_id: ClientEndpointId, session_id: String, pane_id: String) {
        // A policy reload from Ask to Auto can leave an older prompt outstanding.
        // Seal that opener too before starting the successor.
        if let Some(previous) = self.pending.take() {
            self.mark_end(
                &previous.endpoint_id,
                &previous.session_id,
                MediaEndOrigin::Replaced,
                None,
            );
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
        // This serialized transition happens before scheduling any factory opener.
        if let Some(attempt) = self
            .attempts
            .get_mut(&(endpoint_id.clone(), session_id.clone()))
        {
            if attempt.settled || attempt.origin.is_some() {
                return;
            }
            attempt.started = true;
        }
        // A renewal of the same call keeps its mute from the first sample (closed before the new peer opens).
        let keep_muted = self.session.as_ref().is_some_and(|previous| {
            previous.muted && previous.endpoint_id == endpoint_id && previous.pane_id == pane_id
        });
        if let Some(mut previous) = self.session.take() {
            let origin = if previous.endpoint_id == endpoint_id && previous.pane_id == pane_id {
                MediaEndOrigin::Replaced
            } else {
                MediaEndOrigin::Natural
            };
            self.mark_end(&previous.endpoint_id, &previous.session_id, origin, None);
            previous.peer.close();
            self.send_close(
                previous.endpoint_id,
                previous.session_id,
                close_code::REPLACED,
                "a newer media session replaced this one",
            );
        }
        // Never recycle a local token, even after disconnect or a factory error.
        let owner = self.next_owner;
        let Some(next_owner) = owner.checked_add(1) else {
            self.notice("Voice call failed: media peer identities exhausted");
            return;
        };
        self.next_owner = next_owner;
        self.owners
            .insert(owner, (endpoint_id.clone(), session_id.clone()));
        if let Some(attempt) = self
            .attempts
            .get_mut(&(endpoint_id.clone(), session_id.clone()))
        {
            attempt.owner = Some(owner);
        }
        let sink = self.sink.clone();
        let scoped_sink: PeerEventSink = std::sync::Arc::new(move |event| {
            sink(PeerEvent::Scoped {
                owner,
                event: Box::new(event),
            });
        });
        match (self.factory)(session_id.clone(), scoped_sink) {
            Ok(mut peer) => {
                if keep_muted {
                    peer.set_muted(true);
                }
                self.session = Some(MediaSession {
                    endpoint_id,
                    session_id,
                    owner,
                    pane_id,
                    muted: keep_muted,
                    peer,
                });
                self.notice("Voice call started");
            }
            Err(error) => {
                // A generic factory Err does not prove no worker/opener was scheduled.
                // Preserve custody for its teardown; never infer acquired:false here.
                self.notice(&format!("Voice call failed: {error}"));
                self.send_close(endpoint_id, session_id, close_code::DEVICE_ERROR, &error);
            }
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
                session_id: pending.session_id.clone(),
            });
            self.complete_attempt(&pending.endpoint_id, &pending.session_id, false, true);
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

    fn mark_end(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: &str,
        origin: MediaEndOrigin,
        request_id: Option<String>,
    ) {
        if let Some(attempt) = self
            .attempts
            .get_mut(&(endpoint_id.clone(), session_id.to_owned()))
        {
            if !attempt.settled && attempt.origin.is_none() {
                attempt.origin = Some(origin);
                attempt.request_id = if origin == MediaEndOrigin::Requested {
                    request_id
                } else {
                    None
                };
            }
        }
    }

    /// The only completion path: callers either positively sealed a never-started
    /// attempt under serialized ownership, or received the native post-join result.
    fn complete_attempt(
        &mut self,
        endpoint_id: &ClientEndpointId,
        session_id: &str,
        acquired: bool,
        success: bool,
    ) {
        let Some(attempt) = self
            .attempts
            .get_mut(&(endpoint_id.clone(), session_id.to_owned()))
        else {
            return;
        };
        if attempt.settled {
            return;
        }
        attempt.settled = true;
        let control = if success {
            MediaControl::Ended(MediaEnded {
                session_id: session_id.to_owned(),
                generation: attempt.generation.clone(),
                attempt: attempt.attempt.clone(),
                origin: attempt.origin.unwrap_or(MediaEndOrigin::Natural),
                request_id: attempt.request_id.clone(),
                acquired,
            })
        } else {
            MediaControl::TeardownStuck(MediaTeardownStuck {
                session_id: session_id.to_owned(),
                generation: attempt.generation.clone(),
                attempt: attempt.attempt.clone(),
            })
        };
        self.effects
            .push(MediaEffect::Send(attempt.endpoint_id.clone(), control));
    }

    fn send_close(
        &mut self,
        endpoint_id: ClientEndpointId,
        session_id: String,
        code: &str,
        message: &str,
    ) {
        // Client-first refusals and local policy/device ends are natural unless a
        // server close or explicit successor transition already supplied the reason.
        self.mark_end(&endpoint_id, &session_id, MediaEndOrigin::Natural, None);
        self.effects.push(MediaEffect::Send(
            endpoint_id.clone(),
            MediaControl::Close(MediaClose::new(session_id.clone(), code, bounded(message))),
        ));
        if self
            .attempts
            .get(&(endpoint_id.clone(), session_id.clone()))
            .is_some_and(|attempt| !attempt.started)
        {
            self.complete_attempt(&endpoint_id, &session_id, false, true);
        }
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
    use crate::protocol::media::MediaPeerState;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum PeerCall {
        Start(String),
        Answer(String, String),
        Mute(String, bool),
        Close(String),
    }

    #[derive(Clone, Default)]
    struct Calls {
        calls: Arc<Mutex<Vec<PeerCall>>>,
        sinks: Arc<Mutex<Vec<(String, PeerEventSink)>>>,
        events: Arc<Mutex<Vec<PeerEvent>>>,
    }

    impl std::ops::Deref for Calls {
        type Target = Mutex<Vec<PeerCall>>;
        fn deref(&self) -> &Self::Target {
            &self.calls
        }
    }

    impl Calls {
        fn emit(&self, media: &mut ClientMedia, event: PeerEvent) {
            let sid = match &event {
                PeerEvent::Offer { session_id, .. }
                | PeerEvent::State { session_id, .. }
                | PeerEvent::Closed { session_id, .. }
                | PeerEvent::Teardown { session_id, .. } => session_id,
                PeerEvent::Scoped { .. } => panic!("factory fixtures emit bare native events"),
            };
            let sinks = self.sinks.lock().unwrap();
            // Unknown/stale session fixtures still use a real factory sink, which must
            // reject the mismatched wire id instead of bypassing owner checks.
            let sink = sinks
                .iter()
                .rev()
                .find(|(id, _)| id == sid)
                .or_else(|| sinks.last())
                .expect("a factory was invoked")
                .1
                .clone();
            drop(sinks);
            sink(event);
            for event in std::mem::take(&mut *self.events.lock().unwrap()) {
                media.handle_peer_event(event);
            }
        }
    }

    struct FakePeer {
        session_id: String,
        calls: Calls,
    }

    impl MediaPeer for FakePeer {
        fn apply_answer(&mut self, sdp: String) {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Answer(self.session_id.clone(), sdp));
        }

        fn set_muted(&mut self, muted: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(PeerCall::Mute(self.session_id.clone(), muted));
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
            move |session_id: String, sink: PeerEventSink| -> Result<Box<dyn MediaPeer>, String> {
                factory_calls
                    .sinks
                    .lock()
                    .unwrap()
                    .push((session_id.clone(), sink));
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
                }) as Box<dyn MediaPeer>)
            },
        );
        let events = calls.events.clone();
        let mut media = ClientMedia::new(
            mode,
            factory,
            Arc::new(move |event| {
                events.lock().unwrap().push(event);
            }),
        );
        media.peer_available = true;
        (media, calls)
    }

    fn local() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    #[derive(Default, Clone)]
    struct PeerHarness {
        sinks: Arc<Mutex<Vec<PeerEventSink>>>,
        events: Arc<Mutex<Vec<PeerEvent>>>,
    }

    impl PeerHarness {
        fn controller(&self) -> ClientMedia {
            let sinks = self.sinks.clone();
            let calls = Calls::default();
            let factory: PeerFactory = Box::new(move |session_id, sink| {
                sinks.lock().unwrap().push(sink);
                Ok(Box::new(FakePeer {
                    session_id,
                    calls: calls.clone(),
                }))
            });
            let events = self.events.clone();
            let mut media = ClientMedia::new(
                MediaMode::Auto,
                factory,
                Arc::new(move |event| {
                    events.lock().unwrap().push(event);
                }),
            );
            media.peer_available = true;
            media
        }

        fn emit(&self, media: &mut ClientMedia, index: usize, event: PeerEvent) {
            let sink = self.sinks.lock().unwrap()[index].clone();
            sink(event);
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            for event in events {
                media.handle_peer_event(event);
            }
        }
    }

    fn other_endpoint() -> ClientEndpointId {
        ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        )
    }

    fn bound_open(media: &mut ClientMedia, endpoint: &ClientEndpointId, sid: &str, identity: &str) {
        let now = Instant::now();
        media.note_pane_input(endpoint, "pane_1", now);
        media.handle_server_control(endpoint, receipt_control("media.open.v1", serde_json::json!({
            "session_id": sid, "pane_id": "pane_1",
            "generation": format!("generation:{identity}"), "attempt": format!("attempt:{identity}"),
        })), true, |_| Some("pane".into()), now);
    }

    fn teardown(sid: &str) -> PeerEvent {
        PeerEvent::Teardown {
            session_id: sid.into(),
            acquired: true,
            success: true,
        }
    }

    #[test]
    fn receipt_370_old_factory_sink_cannot_complete_reused_id_after_disconnect() {
        let harness = PeerHarness::default();
        let mut media = harness.controller();
        bound_open(&mut media, &local(), "m", "A");
        media.endpoint_gone(&local());
        bound_open(&mut media, &other_endpoint(), "m", "B");
        assert_eq!(harness.sinks.lock().unwrap().len(), 2);
        media.take_effects();
        harness.emit(&mut media, 0, teardown("m"));
        assert_no_completion(&media.take_effects());
        for event in [
            PeerEvent::Offer {
                session_id: "m".into(),
                sdp: "old A".into(),
            },
            PeerEvent::State {
                session_id: "m".into(),
                state: MediaPeerState::Connected,
                muted: true,
                detail: None,
            },
            PeerEvent::Closed {
                session_id: "m".into(),
                code: close_code::DEVICE_ERROR,
                message: "old A".into(),
            },
        ] {
            harness.emit(&mut media, 0, event);
            assert!(media.take_effects().is_empty(), "old A cannot mutate B");
        }
        assert_eq!(
            media.session.as_ref().unwrap().endpoint_id,
            other_endpoint()
        );
        assert!(
            !media
                .attempts
                .get(&(other_endpoint(), "m".into()))
                .unwrap()
                .settled
        );
        harness.emit(&mut media, 1, teardown("m"));
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(
                other_endpoint(),
                serde_json::json!({
                    "session_id": "m", "generation": "generation:B", "attempt": "attempt:B",
                    "origin": "natural", "acquired": true,
                })
            )]
        );
        harness.emit(&mut media, 1, teardown("m"));
        assert!(media.take_effects().is_empty());
    }

    #[test]
    fn receipt_370_reconnect_invalidates_old_owner_even_on_same_endpoint() {
        let harness = PeerHarness::default();
        let mut media = harness.controller();
        bound_open(&mut media, &local(), "m", "A");
        media.endpoint_gone(&local());
        bound_open(&mut media, &local(), "m", "B");
        media.take_effects();
        harness.emit(&mut media, 0, teardown("m"));
        assert!(media.take_effects().is_empty());
        assert!(!media.attempts.get(&(local(), "m".into())).unwrap().settled);
        assert_eq!(media.session.as_ref().unwrap().session_id, "m");
        harness.emit(&mut media, 1, teardown("m"));
        let receipts = completion_controls(&media.take_effects(), "media.ended.v1");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].0, local());
        assert_eq!(receipts[0].1["generation"], "generation:B");
        assert_eq!(receipts[0].1["attempt"], "attempt:B");
    }

    #[test]
    fn receipt_370_unscoped_events_cannot_adopt_live_owner() {
        let harness = PeerHarness::default();
        let mut media = harness.controller();
        bound_open(&mut media, &local(), "m", "A");
        media.take_effects();
        for event in [
            PeerEvent::Offer {
                session_id: "m".into(),
                sdp: "bare".into(),
            },
            PeerEvent::State {
                session_id: "m".into(),
                state: MediaPeerState::Connected,
                muted: true,
                detail: None,
            },
            PeerEvent::Closed {
                session_id: "m".into(),
                code: close_code::DEVICE_ERROR,
                message: "bare".into(),
            },
            teardown("m"),
        ] {
            media.handle_peer_event(event);
            assert!(media.take_effects().is_empty());
        }
        assert_eq!(media.session.as_ref().unwrap().session_id, "m");
        assert!(!media.attempts.get(&(local(), "m".into())).unwrap().settled);
        harness.emit(&mut media, 0, teardown("m"));
        let receipts = completion_controls(&media.take_effects(), "media.ended.v1");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].1["attempt"], "attempt:A");
    }

    #[test]
    fn receipt_370_live_cross_endpoint_reused_ids_have_independent_custody() {
        let harness = PeerHarness::default();
        let mut media = harness.controller();
        bound_open(&mut media, &local(), "m", "A");
        bound_open(&mut media, &other_endpoint(), "m", "B");
        assert_eq!(
            harness.sinks.lock().unwrap().len(),
            2,
            "duplicate suppression is endpoint-local"
        );
        media.take_effects();
        harness.emit(&mut media, 0, teardown("m"));
        let effects = media.take_effects();
        let receipts = completion_controls(&effects, "media.ended.v1");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].0, local());
        assert_eq!(receipts[0].1["attempt"], "attempt:A");
        assert_eq!(
            media.session.as_ref().unwrap().endpoint_id,
            other_endpoint()
        );
        harness.emit(&mut media, 1, teardown("m"));
        let receipts = completion_controls(&media.take_effects(), "media.ended.v1");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].0, other_endpoint());
        assert_eq!(receipts[0].1["attempt"], "attempt:B");
    }

    #[test]
    fn receipt_370_replaced_same_endpoint_factory_sink_completes_original_identity() {
        let harness = PeerHarness::default();
        let mut media = harness.controller();
        bound_open(&mut media, &local(), "old", "A");
        bound_open(&mut media, &local(), "new", "B");
        media.take_effects();
        // Neither the predecessor's genuine id nor a mislabelled successor id may
        // publish or close the successor through that predecessor's captured sink.
        for sid in ["old", "new"] {
            for event in [
                PeerEvent::Offer {
                    session_id: sid.into(),
                    sdp: "late old".into(),
                },
                PeerEvent::State {
                    session_id: sid.into(),
                    state: MediaPeerState::Connected,
                    muted: true,
                    detail: None,
                },
                PeerEvent::Closed {
                    session_id: sid.into(),
                    code: close_code::DEVICE_ERROR,
                    message: "late old".into(),
                },
            ] {
                harness.emit(&mut media, 0, event);
                assert!(media.take_effects().is_empty());
                assert_eq!(media.session.as_ref().unwrap().session_id, "new");
            }
        }
        harness.emit(&mut media, 0, teardown("new"));
        assert!(media.take_effects().is_empty());
        harness.emit(&mut media, 0, teardown("old"));
        let receipts = completion_controls(&media.take_effects(), "media.ended.v1");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].0, local());
        assert_eq!(receipts[0].1["session_id"], "old");
        assert_eq!(receipts[0].1["generation"], "generation:A");
        assert_eq!(receipts[0].1["attempt"], "attempt:A");
        assert_eq!(receipts[0].1["origin"], "replaced");
        assert_eq!(media.session.as_ref().unwrap().session_id, "new");
    }

    fn open(media: &mut ClientMedia, session_id: &str, pane_id: &str, now: Instant) {
        media.handle_server_control(
            &local(),
            MediaControl::Open(MediaOpen {
                session_id: session_id.into(),
                pane_id: pane_id.into(),
                generation: None,
                attempt: None,
            }),
            true,
            |pane| (pane == "pane_1" || pane == "pane_2").then(|| format!("label {pane}")),
            now,
        );
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

    // #370 exercises the real named-control JSON boundary so receipt metadata is not
    // accidentally lost between decoding, consent and the teardown effect.
    fn receipt_control(kind: &str, data: serde_json::Value) -> MediaControl {
        MediaControl::decode(kind, &data.to_string())
            .expect("known media control")
            .expect("valid receipt-bearing control")
    }

    fn receipt_open(media: &mut ClientMedia, session_id: &str, now: Instant) {
        media.handle_server_control(
            &local(),
            receipt_control(
                "media.open.v1",
                serde_json::json!({
                    "session_id": session_id,
                    "pane_id": "pane_1",
                    "generation": format!("generation:{session_id}"),
                    "attempt": format!("attempt:{session_id}"),
                }),
            ),
            true,
            |_| Some("label pane_1".into()),
            now,
        );
    }

    fn receipt_close(media: &mut ClientMedia, session_id: &str, origin: &str) {
        let mut data = serde_json::json!({
            "session_id": session_id,
            "code": "closed",
            "origin": origin,
        });
        if origin == "requested" {
            data["request_id"] = serde_json::json!("request:close-370");
        }
        media.handle_server_control(
            &local(),
            receipt_control("media.close.v1", data),
            true,
            |_| None,
            Instant::now(),
        );
    }

    fn completion_controls(
        effects: &[MediaEffect],
        kind: &str,
    ) -> Vec<(ClientEndpointId, serde_json::Value)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                MediaEffect::Send(endpoint, control) if control.kind() == kind => {
                    let crate::protocol::ClientMessage::EndpointControl { data, .. } =
                        control.client_message().expect("encode client control")
                    else {
                        panic!("media completion must use the endpoint control envelope");
                    };
                    Some((endpoint.clone(), serde_json::from_str(&data).unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    fn expected_receipt(session_id: &str, origin: &str, acquired: bool) -> serde_json::Value {
        let mut data = serde_json::json!({
            "session_id": session_id,
            "generation": format!("generation:{session_id}"),
            "attempt": format!("attempt:{session_id}"),
            "origin": origin,
            "acquired": acquired,
        });
        if origin == "requested" {
            data["request_id"] = serde_json::json!("request:close-370");
        }
        data
    }

    fn assert_receipt_close_pending_consent(origin: &str) {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        receipt_open(&mut media, "m1", now);
        assert!(media.take_effects().contains(&MediaEffect::AskConsent {
            session_id: "m1".into(),
            pane_label: "label pane_1".into(),
        }));
        assert!(calls.lock().unwrap().is_empty(), "consent holds the opener");

        receipt_close(&mut media, "m1", origin);
        let effects = media.take_effects();
        assert!(effects.contains(&MediaEffect::CancelConsent {
            session_id: "m1".into(),
        }));
        assert_eq!(media.deadline(), None);
        // An already queued UI answer cannot start an opener after the close fence.
        media.consent("m1", true);
        assert!(
            calls.lock().unwrap().is_empty(),
            "late consent cannot acquire"
        );
        assert!(
            media.take_effects().is_empty(),
            "late consent cannot send again"
        );
        assert_eq!(
            completion_controls(&effects, "media.ended.v1"),
            vec![(local(), expected_receipt("m1", origin, false))],
            "sealed pending consent needs exactly one never-acquired receipt"
        );
        receipt_close(&mut media, "m1", origin);
        media.consent("m1", true);
        assert!(completion_controls(&media.take_effects(), "media.ended.v1").is_empty());
    }

    #[test]
    fn receipt_370_close_pending_consent_seals_opener_before_late_accept() {
        assert_receipt_close_pending_consent("cancelled");
    }

    #[test]
    fn receipt_370_requested_close_pending_consent_preserves_request_id() {
        assert_receipt_close_pending_consent("requested");
    }

    #[test]
    fn receipt_370_replaced_pending_consent_completes_only_the_old_attempt() {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        receipt_open(&mut media, "m1", now);
        media.take_effects();
        receipt_open(&mut media, "m2", now);
        let effects = media.take_effects();
        assert!(effects.contains(&MediaEffect::CancelConsent {
            session_id: "m1".into()
        }));
        assert!(effects.contains(&MediaEffect::AskConsent {
            session_id: "m2".into(),
            pane_label: "label pane_1".into(),
        }));
        media.consent("m1", true);
        assert!(
            calls.lock().unwrap().is_empty(),
            "old prompt answer is sealed"
        );
        assert!(media.take_effects().is_empty());
        assert_eq!(
            completion_controls(&effects, "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "replaced", false))],
            "replacement must preserve the predecessor identity, not complete m2"
        );
        media.consent("m2", true);
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m2".into())]);
    }

    fn receipt_started() -> (ClientMedia, Calls) {
        let (mut media, calls) = media(MediaMode::Auto, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        receipt_open(&mut media, "m1", now);
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert!(completion_controls(&media.take_effects(), "media.ended.v1").is_empty());
        (media, calls)
    }

    fn assert_no_completion(effects: &[MediaEffect]) {
        assert!(
            completion_controls(effects, "media.ended.v1").is_empty(),
            "close admission, peer Closed and a held teardown are not receipts: {effects:?}"
        );
        assert!(completion_controls(effects, "media.teardown_stuck.v1").is_empty());
    }

    // The fake peer's close records cancellation but deliberately does not emit Teardown.
    // The test holds completion at the real peer-event seam until the explicit join result.
    #[test]
    fn receipt_370_requested_close_waits_for_held_teardown_and_sends_once() {
        let (mut media, calls) = receipt_started();
        receipt_close(&mut media, "m1", "requested");
        assert!(calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Close("m1".into())));
        assert_no_completion(&media.take_effects());
        receipt_close(&mut media, "m1", "requested");
        calls.emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m1".into(),
                code: close_code::DEVICE_ERROR,
                message: "late error".into(),
            },
        );
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "unknown".into(),
                acquired: true,
                success: true,
            },
        );
        assert_no_completion(&media.take_effects());

        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "requested", true))],
            "late Closed must not replace the requested origin/correlation"
        );
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert!(
            media.take_effects().is_empty(),
            "duplicate join result cannot send twice"
        );
    }

    #[test]
    fn receipt_370_replaced_running_peer_retains_old_attempt_until_teardown() {
        let (mut media, calls) = receipt_started();
        let now = Instant::now();
        receipt_open(&mut media, "m2", now);
        assert!(calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Close("m1".into())));
        assert!(calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Start("m2".into())));
        assert_no_completion(&media.take_effects());
        // m1 is no longer current, but its join result is still authoritative for m1.
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "replaced", true))]
        );
        assert_eq!(media.session.as_ref().unwrap().session_id, "m2");
        // Nor may m1's late offer mutate the successor or restart its predecessor.
        calls.emit(
            &mut media,
            PeerEvent::Offer {
                session_id: "m1".into(),
                sdp: "late".into(),
            },
        );
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert!(media.take_effects().is_empty());
    }

    #[test]
    fn receipt_370_natural_closed_waits_for_join_after_current_session_is_removed() {
        let (mut media, calls) = receipt_started();
        calls.emit(
            &mut media,
            PeerEvent::Closed {
                session_id: "m1".into(),
                code: close_code::DEVICE_ERROR,
                message: "device lost".into(),
            },
        );
        let effects = media.take_effects();
        assert_eq!(
            closes(&effects),
            vec![("m1".into(), Some(close_code::DEVICE_ERROR.into()))]
        );
        assert_no_completion(&effects);
        assert!(media.session.is_none());
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "natural", true))]
        );
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert!(media.take_effects().is_empty());
    }

    #[test]
    fn receipt_370_failed_join_is_diagnostic_only_not_never_acquired() {
        let (mut media, calls) = receipt_started();
        receipt_close(&mut media, "m1", "requested");
        assert_no_completion(&media.take_effects());
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: false,
                success: false,
            },
        );
        let effects = media.take_effects();
        assert!(
            completion_controls(&effects, "media.ended.v1").is_empty(),
            "missing peer/acquisition information on a failed join is not never-acquired proof"
        );
        assert_eq!(
            completion_controls(&effects, "media.teardown_stuck.v1"),
            vec![(
                local(),
                serde_json::json!({
                    "session_id": "m1", "generation": "generation:m1", "attempt": "attempt:m1",
                })
            )]
        );
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: false,
                success: false,
            },
        );
        assert!(
            media.take_effects().is_empty(),
            "one diagnostic per attempt"
        );
    }

    #[test]
    fn receipt_370_wrong_endpoint_and_stale_controls_do_not_close_successor() {
        let (mut media, calls) = receipt_started();
        let other = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let now = Instant::now();
        let close = receipt_control(
            "media.close.v1",
            serde_json::json!({
                "session_id": "m1", "origin": "requested", "request_id": "wrong:endpoint",
            }),
        );
        media.handle_server_control(&other, close, true, |_| None, now);
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert!(media.take_effects().is_empty());
        receipt_open(&mut media, "m2", now);
        media.take_effects();
        receipt_close(&mut media, "m1", "requested");
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "replaced", true))]
        );
        assert_eq!(media.session.as_ref().unwrap().session_id, "m2");
        assert!(!calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Close("m2".into())));
    }

    #[test]
    fn receipt_370_disconnect_drops_custody_without_delivering_completion() {
        let (mut media, calls) = receipt_started();
        media.endpoint_gone(&local());
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_no_completion(&media.take_effects());
        assert!(media.attempts.is_empty());
        assert!(calls
            .lock()
            .unwrap()
            .contains(&PeerCall::Close("m1".into())));

        let (mut pending, _) = media_for_disconnect();
        receipt_close(&mut pending, "m1", "cancelled");
        // Even already-queued proof cannot be delivered onto a disconnected endpoint.
        pending.endpoint_gone(&local());
        assert_no_completion(&pending.take_effects());
        assert!(pending.attempts.is_empty());
    }

    fn media_for_disconnect() -> (ClientMedia, Calls) {
        let (mut media, calls) = media(MediaMode::Ask, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        receipt_open(&mut media, "m1", now);
        media.take_effects();
        (media, calls)
    }

    #[test]
    fn receipt_370_factory_error_is_not_positive_never_acquired_evidence() {
        let (mut media, calls) = media(MediaMode::Auto, true);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        receipt_open(&mut media, "m1", now);
        assert_no_completion(&media.take_effects());
        receipt_close(&mut media, "m1", "requested");
        assert_no_completion(&media.take_effects());
        calls.emit(
            &mut media,
            PeerEvent::Teardown {
                session_id: "m1".into(),
                acquired: true,
                success: true,
            },
        );
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m1", "natural", true))]
        );
    }

    #[test]
    fn receipt_370_refused_renewal_preserves_live_predecessor() {
        let (mut media, calls) = receipt_started();
        media.handle_server_control(
            &local(),
            receipt_control(
                "media.open.v1",
                serde_json::json!({
                    "session_id": "m2", "pane_id": "pane_1",
                    "generation": "generation:m2", "attempt": "attempt:m2",
                }),
            ),
            true,
            |_| None,
            Instant::now(),
        );
        assert_eq!(*calls.lock().unwrap(), vec![PeerCall::Start("m1".into())]);
        assert_eq!(
            completion_controls(&media.take_effects(), "media.ended.v1"),
            vec![(local(), expected_receipt("m2", "natural", false))]
        );
        assert_eq!(media.session.as_ref().unwrap().session_id, "m1");
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
                generation: None,
                attempt: None,
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

        calls.emit(
            &mut media,
            PeerEvent::Offer {
                session_id: "m1".into(),
                sdp: "v=0".into(),
            },
        );
        calls.emit(
            &mut media,
            PeerEvent::State {
                session_id: "m1".into(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            },
        );
        calls.emit(
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
        let (mut media, calls) = media(mode, false);
        let now = Instant::now();
        media.note_pane_input(&local(), "pane_1", now);
        open(&mut media, "m1", "pane_1", now);
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
                crate::server::media::MediaAction::Publish(_) => {}
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
        calls.emit(
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
            [MediaControl::Open(open)] => open.session_id.clone(),
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
        calls.emit(
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
        assert_eq!(
            calls.get(start + 1),
            Some(&PeerCall::Mute(new.clone(), true)),
            "muted right after it starts: {calls:?}"
        );
        assert!(!calls[start..].contains(&PeerCall::Mute(new.clone(), false)));
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
        let (mut media, calls) = started(MediaMode::Auto);
        calls.emit(
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
}
