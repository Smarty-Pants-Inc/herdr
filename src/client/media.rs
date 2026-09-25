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
    close_code, MediaClose, MediaControl, MediaOpen, MediaSdp, MediaStateUpdate,
    MAX_MEDIA_TEXT_BYTES, MEDIA_INPUT_WINDOW,
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
    peer: Box<dyn MediaPeer>,
}

struct PendingConsent {
    endpoint_id: ClientEndpointId,
    session_id: String,
    deadline: Instant,
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
                    session.peer.set_muted(mute.muted);
                }
            }
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
            MediaControl::Offer(_) | MediaControl::State(_) => {}
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
        } = open;
        let duplicate = self
            .session
            .as_ref()
            .is_some_and(|session| session.session_id == session_id)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.session_id == session_id);
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
        if !fresh_input {
            return self.refuse(
                endpoint_id,
                session_id,
                close_code::STALE_INPUT,
                "this client did not just type into that pane",
            );
        }
        if self.mode == MediaMode::Auto || self.allowed {
            return self.start(endpoint_id.clone(), session_id);
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
            self.start(pending.endpoint_id, pending.session_id);
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
        let Some((endpoint_id, current)) = self
            .session
            .as_ref()
            .map(|session| (session.endpoint_id.clone(), session.session_id.clone()))
        else {
            return;
        };
        match event {
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

    fn start(&mut self, endpoint_id: ClientEndpointId, session_id: String) {
        if let Some(mut previous) = self.session.take() {
            previous.peer.close();
            self.send_close(
                previous.endpoint_id,
                previous.session_id,
                close_code::REPLACED,
                "a newer media session replaced this one",
            );
        }
        match (self.factory)(session_id.clone(), self.sink.clone()) {
            Ok(peer) => {
                self.session = Some(MediaSession {
                    endpoint_id,
                    session_id,
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
    use crate::protocol::media::MediaPeerState;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum PeerCall {
        Start(String),
        Answer(String, String),
        Mute(String, bool),
        Close(String),
    }

    type Calls = Arc<Mutex<Vec<PeerCall>>>;

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
            move |session_id: String, _sink: PeerEventSink| -> Result<Box<dyn MediaPeer>, String> {
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
        let mut media = ClientMedia::new(mode, factory, Arc::new(|_: PeerEvent| {}));
        media.peer_available = true;
        (media, calls)
    }

    fn local() -> ClientEndpointId {
        ClientEndpointId::Local
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

        media.handle_peer_event(PeerEvent::Offer {
            session_id: "m1".into(),
            sdp: "v=0".into(),
        });
        media.handle_peer_event(PeerEvent::State {
            session_id: "m1".into(),
            state: MediaPeerState::Connected,
            muted: false,
            detail: None,
        });
        media.handle_peer_event(PeerEvent::Offer {
            session_id: "stale".into(),
            sdp: "v=0".into(),
        });
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
        let (mut media, _) = started(MediaMode::Auto);
        media.handle_peer_event(PeerEvent::Closed {
            session_id: "m1".into(),
            code: close_code::DEVICE_ERROR,
            message: "x".repeat(MAX_MEDIA_TEXT_BYTES + 10),
        });
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
