//! Media peer seam.
//!
//! The client media controller drives a peer only through [`MediaPeer`] and receives its
//! results as [`PeerEvent`]s. The control plane (binding, refusals, consent) therefore stays
//! testable without audio devices or WebRTC, and builds without the `native-media` feature
//! compile no media stack at all.

use std::sync::Arc;

use crate::protocol::media::MediaPeerState;

/// Asynchronous results from a peer. The peer calls the sink from its own threads.
// Only the native peer constructs these; builds without it have none outside tests.
#[cfg_attr(not(feature = "native-media"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerEvent {
    /// The complete local SDP offer (ICE gathering finished or timed out).
    Offer { session_id: String, sdp: String },
    /// A connection or mute state change.
    State {
        session_id: String,
        state: MediaPeerState,
        muted: bool,
        detail: Option<String>,
    },
    /// The peer ended on its own (device loss, ICE or DTLS failure). It has already released
    /// the microphone and speaker. `code` is one of `protocol::media::close_code`.
    Closed {
        session_id: String,
        code: &'static str,
        message: String,
    },
}

pub(crate) type PeerEventSink = Arc<dyn Fn(PeerEvent) + Send + Sync>;

/// One live media session. Every method returns promptly; failures arrive as events.
pub(crate) trait MediaPeer: Send {
    /// Apply the remote SDP answer.
    fn apply_answer(&mut self, sdp: String);
    /// Mute or unmute the local microphone. The peer confirms with a `State` event.
    fn set_muted(&mut self, muted: bool);
    /// Stop media and release the microphone and speaker. Idempotent. Dropping a peer
    /// must have the same effect.
    fn close(&mut self);
}

/// Starts a peer for one session. It returns at once; the offer arrives as `PeerEvent::Offer`.
pub(crate) type PeerFactory =
    Box<dyn Fn(String, PeerEventSink) -> Result<Box<dyn MediaPeer>, String> + Send>;

/// Whether this build contains a real media peer.
/// Only macOS has a native audio backend; other feature builds compile the peer for tests
/// but must not advertise a capability their factory always refuses.
pub(crate) const NATIVE_PEER_AVAILABLE: bool =
    cfg!(all(feature = "native-media", target_os = "macos"));

/// Endpoint hello capabilities this client build advertises.
pub(crate) fn advertised_capabilities() -> Vec<String> {
    if NATIVE_PEER_AVAILABLE {
        vec![crate::protocol::media::MEDIA_WEBRTC_CAPABILITY.to_owned()]
    } else {
        Vec::new()
    }
}

/// The peer factory for this build.
pub(crate) fn native_peer_factory() -> PeerFactory {
    #[cfg(feature = "native-media")]
    {
        Box::new(|session_id, sink| {
            super::native::start(session_id, sink).map(|peer| Box::new(peer) as Box<dyn MediaPeer>)
        })
    }
    #[cfg(not(feature = "native-media"))]
    {
        Box::new(|_, _| Err("this Herdr client was built without native media".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_advertised_only_where_the_native_peer_works() {
        let expected = cfg!(all(feature = "native-media", target_os = "macos"));
        assert_eq!(NATIVE_PEER_AVAILABLE, expected);
        assert_eq!(!advertised_capabilities().is_empty(), expected);
    }
}
