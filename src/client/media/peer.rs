//! Media peer seam.
//!
//! The client media controller drives a peer only through [`MediaPeer`] and receives its
//! results as [`PeerEvent`]s. The control plane (binding, refusals, consent) therefore stays
//! testable without audio devices or WebRTC, and builds without the `native-media` feature
//! compile no media stack at all.

use std::sync::Arc;

use crate::config::VoiceConfig;
use crate::protocol::media::{MediaAudioDevices, MediaPeerState};

/// Asynchronous results from a peer. The peer calls the sink from its own threads.
// Only the native peer constructs these; builds without it have none outside tests.
#[cfg_attr(not(feature = "native-media"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerEvent {
    /// The complete local SDP offer (ICE gathering finished or timed out), with the
    /// devices successfully opened for this session, if the backend reports them.
    Offer {
        session_id: String,
        sdp: String,
        audio_devices: Option<MediaAudioDevices>,
    },
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
    Box<dyn FnMut(String, PeerEventSink) -> Result<Box<dyn MediaPeer>, String> + Send>;

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

/// The voice-relevant part of a live config load; kept separate for hardware-free tests.
#[cfg(any(test, feature = "native-media"))]
struct VoiceReload {
    voice: VoiceConfig,
    invalid_sections: Vec<String>,
    diagnostics: Vec<String>,
}

#[cfg(any(test, feature = "native-media"))]
fn apply_voice_reload(accepted: &mut VoiceConfig, reload: Result<VoiceReload, Vec<String>>) {
    match reload {
        Ok(loaded)
            if !loaded
                .invalid_sections
                .iter()
                .any(|section| section == "voice") =>
        {
            *accepted = loaded.voice;
        }
        Ok(loaded) => {
            tracing::warn!(diagnostics = ?loaded.diagnostics, "invalid client voice settings; keeping last accepted audio preferences");
        }
        Err(diagnostics) => {
            tracing::warn!(
                ?diagnostics,
                "could not reload client voice settings; keeping last accepted audio preferences"
            );
        }
    }
}

/// The peer factory for this build, starting with accepted client-local preferences.
pub(crate) fn native_peer_factory(voice: VoiceConfig) -> PeerFactory {
    #[cfg(feature = "native-media")]
    {
        let mut voice = voice;
        Box::new(move |session_id, sink| {
            // Reload only at a new call/renewal, never during an active stream. A bad
            // unrelated section does not invalidate voice; read/parse/voice errors keep
            // the last accepted preferences rather than silently opening defaults.
            let reload = crate::config::load_live_config().map(|loaded| VoiceReload {
                voice: loaded.config.voice,
                invalid_sections: loaded.invalid_sections,
                diagnostics: loaded.diagnostics,
            });
            apply_voice_reload(&mut voice, reload);
            super::native::start(session_id, sink, voice.clone())
                .map(|peer| Box::new(peer) as Box<dyn MediaPeer>)
        })
    }
    #[cfg(not(feature = "native-media"))]
    {
        let _ = voice;
        Box::new(|_, _| Err("this Herdr client was built without native media".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named_voice() -> VoiceConfig {
        VoiceConfig {
            input: Some("USB microphone".into()),
            output: Some("Headphones".into()),
        }
    }

    fn reload(voice: VoiceConfig, invalid: &[&str]) -> Result<VoiceReload, Vec<String>> {
        Ok(VoiceReload {
            voice,
            invalid_sections: invalid
                .iter()
                .map(|section| (*section).to_owned())
                .collect(),
            diagnostics: invalid
                .iter()
                .map(|section| format!("invalid {section} config"))
                .collect(),
        })
    }

    #[test]
    fn new_call_accepts_updated_voice_and_can_reselect_system_defaults() {
        let mut accepted = VoiceConfig::default();
        apply_voice_reload(&mut accepted, reload(named_voice(), &[]));
        assert_eq!(accepted, named_voice());
        apply_voice_reload(&mut accepted, reload(VoiceConfig::default(), &[]));
        assert_eq!(accepted, VoiceConfig::default());
    }

    #[test]
    fn invalid_unrelated_ui_section_does_not_discard_valid_voice_preferences() {
        let mut accepted = VoiceConfig::default();
        apply_voice_reload(&mut accepted, reload(named_voice(), &["ui"]));
        assert_eq!(accepted, named_voice());
    }

    #[test]
    fn invalid_voice_section_keeps_last_accepted_preferences_not_loader_defaults() {
        let mut accepted = named_voice();
        apply_voice_reload(
            &mut accepted,
            reload(VoiceConfig::default(), &["voice", "ui"]),
        );
        assert_eq!(accepted, named_voice());
    }

    #[test]
    fn failed_read_or_parse_keeps_last_accepted_voice_and_later_valid_reload_recovers() {
        let mut accepted = named_voice();
        for error in ["config read error", "config parse error"] {
            apply_voice_reload(&mut accepted, Err(vec![error.into()]));
            assert_eq!(accepted, named_voice());
        }
        let next = VoiceConfig {
            input: None,
            output: Some("External speaker".into()),
        };
        apply_voice_reload(&mut accepted, reload(next.clone(), &[]));
        assert_eq!(accepted, next);
    }

    #[test]
    fn capability_is_advertised_only_where_the_native_peer_works() {
        let expected = cfg!(all(feature = "native-media", target_os = "macos"));
        assert_eq!(NATIVE_PEER_AVAILABLE, expected);
        assert_eq!(!advertised_capabilities().is_empty(), expected);
    }
}
