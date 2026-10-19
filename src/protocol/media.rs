//! Client-local media effects.
//!
//! The server can ask one attached client to act as the WebRTC peer for a pane, so a
//! program in that pane (for example a voice agent) gets the user's local microphone and
//! speaker without a browser page. The server only carries SDP and control; media flows
//! directly between the client and the remote peer.
//!
//! Messages ride the stable endpoint `EndpointControl` channel as named, versioned JSON
//! controls. Clients and servers that do not know a kind ignore it, so no frozen codec or
//! enum changes. A client advertises support with [`MEDIA_WEBRTC_CAPABILITY`] in its
//! endpoint hello.
//!
//! Flow: server `media.open.v1` → client `media.offer.v1` (or `media.close.v1` as a
//! refusal) → server `media.answer.v1` → client `media.state.v1` updates. Either side
//! sends `media.close.v1`; the client releases the microphone when it closes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ClientMessage, ServerMessage};

/// Endpoint hello capability for clients built with a WebRTC media peer.
pub const MEDIA_WEBRTC_CAPABILITY: &str = "media.webrtc.v1";

/// Server → client: open a media session for a pane the client just typed into.
pub const MEDIA_OPEN_KIND: &str = "media.open.v1";
/// Client → server: the client's SDP offer.
pub const MEDIA_OFFER_KIND: &str = "media.offer.v1";
/// Server → client: the remote peer's SDP answer.
pub const MEDIA_ANSWER_KIND: &str = "media.answer.v1";
/// Server → client: mute or unmute the local microphone.
pub const MEDIA_MUTE_KIND: &str = "media.mute.v1";
/// Server → client: apply a device control and report it when the device has applied it.
pub const MEDIA_CONTROL_KIND: &str = "media.control.v1";
/// Client → server: a device control was applied at the device boundary.
pub const MEDIA_APPLIED_KIND: &str = "media.applied.v1";
/// Client → server: the latest opt-in audio levels.
pub const MEDIA_LEVELS_KIND: &str = "media.levels.v1";
/// Client → server: connected device and build information.
pub const MEDIA_INFO_KIND: &str = "media.info.v1";
/// Client → server: peer state changes (connecting, connected, muted, failed).
pub const MEDIA_STATE_KIND: &str = "media.state.v1";
/// Both directions: end the session. Sent by the client before an offer, it is a refusal.
pub const MEDIA_CLOSE_KIND: &str = "media.close.v1";

/// A client's input must have reached the pane within this window for the server to bind
/// a media session to that client, and for the client to accept it.
pub const MEDIA_INPUT_WINDOW: Duration = Duration::from_secs(10);

/// Upper bound for one SDP payload. Real audio offers are a few KiB.
pub const MAX_MEDIA_SDP_BYTES: usize = 64 * 1024;
/// Upper bound for identifiers, codes and short detail strings in media controls.
pub const MAX_MEDIA_TEXT_BYTES: usize = 512;

/// Close and refusal codes shared by the client, the server and API callers.
pub mod close_code {
    /// The pane is not in the layout the client is showing.
    pub const NOT_VIEWED: &str = "not_viewed";
    /// The client's last input to the pane is older than the input window.
    pub const STALE_INPUT: &str = "stale_input";
    /// The client's `media` setting is `off`.
    pub const DISABLED: &str = "disabled";
    /// The user declined the consent prompt, or did not answer in time.
    pub const DECLINED: &str = "declined";
    /// The client has no media peer (built without `native-media`).
    pub const UNSUPPORTED: &str = "unsupported";
    /// The request did not come from the server the client is showing.
    pub const WRONG_ENDPOINT: &str = "wrong_endpoint";
    /// The audio device or WebRTC peer failed.
    pub const DEVICE_ERROR: &str = "device_error";
    /// A default audio device changed; the caller must renew the session.
    // Emitted by the optional native-media client; other builds still expose its code.
    #[allow(dead_code)]
    pub const DEVICE_CHANGED: &str = "device_changed";
    /// The API caller or the user ended the session.
    pub const CLOSED: &str = "closed";
    /// The client or its connection went away.
    pub const DISCONNECTED: &str = "disconnected";
    /// The pane closed.
    pub const PANE_CLOSED: &str = "pane_closed";
    /// The client did not send an offer in time.
    pub const TIMEOUT: &str = "timeout";
    /// A newer session for the same pane replaced this one.
    pub const REPLACED: &str = "replaced";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaOpen {
    pub session_id: String,
    /// Pane id exactly as the client sent it in its last pane input.
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSdp {
    pub session_id: String,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMute {
    pub session_id: String,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDeviceControl {
    pub session_id: String,
    pub control_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_suppressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaApplied {
    pub session_id: String,
    pub control_id: u64,
    pub input_muted: bool,
    pub output_suppressed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaLevels {
    pub session_id: String,
    pub input_peak: f32,
    pub output_peak: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub session_id: String,
    pub version: String,
    pub commit: String,
    pub pid: u32,
    pub input_device: String,
    pub output_device: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPeerState {
    Connecting,
    Connected,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStateUpdate {
    pub session_id: String,
    pub state: MediaPeerState,
    #[serde(default)]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaClose {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl MediaClose {
    pub fn new(session_id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            code: Some(code.to_owned()),
            message: Some(message.into()),
        }
    }
}

/// One decoded media control, in either direction.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaControl {
    Open(MediaOpen),
    Offer(MediaSdp),
    Answer(MediaSdp),
    Mute(MediaMute),
    Control(MediaDeviceControl),
    Applied(MediaApplied),
    Levels(MediaLevels),
    Info(MediaInfo),
    State(MediaStateUpdate),
    Close(MediaClose),
}

impl MediaControl {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Open(_) => MEDIA_OPEN_KIND,
            Self::Offer(_) => MEDIA_OFFER_KIND,
            Self::Answer(_) => MEDIA_ANSWER_KIND,
            Self::Mute(_) => MEDIA_MUTE_KIND,
            Self::Control(_) => MEDIA_CONTROL_KIND,
            Self::Applied(_) => MEDIA_APPLIED_KIND,
            Self::Levels(_) => MEDIA_LEVELS_KIND,
            Self::Info(_) => MEDIA_INFO_KIND,
            Self::State(_) => MEDIA_STATE_KIND,
            Self::Close(_) => MEDIA_CLOSE_KIND,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Open(open) => &open.session_id,
            Self::Offer(sdp) | Self::Answer(sdp) => &sdp.session_id,
            Self::Mute(mute) => &mute.session_id,
            Self::Control(control) => &control.session_id,
            Self::Applied(applied) => &applied.session_id,
            Self::Levels(levels) => &levels.session_id,
            Self::Info(info) => &info.session_id,
            Self::State(state) => &state.session_id,
            Self::Close(close) => &close.session_id,
        }
    }

    /// True for every versioned media kind, including ones this build cannot decode.
    pub fn is_media_kind(kind: &str) -> bool {
        kind.starts_with("media.")
    }

    /// Decode one named control. `None` means the kind is not a media kind this build knows;
    /// callers ignore it. `Some(Err)` means a known kind carried an invalid payload.
    pub fn decode(kind: &str, data: &str) -> Option<Result<Self, String>> {
        fn parse<T: for<'de> Deserialize<'de>>(kind: &str, data: &str) -> Result<T, String> {
            serde_json::from_str(data).map_err(|error| format!("invalid {kind}: {error}"))
        }
        let decoded = match kind {
            MEDIA_OPEN_KIND => parse(kind, data).map(Self::Open),
            MEDIA_OFFER_KIND => parse(kind, data).map(Self::Offer),
            MEDIA_ANSWER_KIND => parse(kind, data).map(Self::Answer),
            MEDIA_MUTE_KIND => parse(kind, data).map(Self::Mute),
            MEDIA_CONTROL_KIND => parse(kind, data).map(Self::Control),
            MEDIA_APPLIED_KIND => parse(kind, data).map(Self::Applied),
            MEDIA_LEVELS_KIND => parse(kind, data).map(Self::Levels),
            MEDIA_INFO_KIND => parse(kind, data).map(Self::Info),
            MEDIA_STATE_KIND => parse(kind, data).map(Self::State),
            MEDIA_CLOSE_KIND => parse(kind, data).map(Self::Close),
            _ => return None,
        };
        Some(decoded.and_then(|control| control.validate().map(|()| control)))
    }

    /// Bound payload sizes at the trust boundary.
    pub fn validate(&self) -> Result<(), String> {
        let text_ok = |value: &str| value.len() <= MAX_MEDIA_TEXT_BYTES;
        let optional_ok = |value: &Option<String>| value.as_deref().is_none_or(text_ok);
        let session_ok = !self.session_id().is_empty() && text_ok(self.session_id());
        let valid = session_ok
            && match self {
                Self::Open(open) => !open.pane_id.is_empty() && text_ok(&open.pane_id),
                Self::Offer(sdp) | Self::Answer(sdp) => {
                    !sdp.sdp.is_empty() && sdp.sdp.len() <= MAX_MEDIA_SDP_BYTES
                }
                Self::Mute(_) => true,
                Self::Control(_) => true,
                Self::Applied(_) => true,
                Self::Levels(levels) => {
                    levels.input_peak.is_finite()
                        && levels.output_peak.is_finite()
                        && (0.0..=1.0).contains(&levels.input_peak)
                        && (0.0..=1.0).contains(&levels.output_peak)
                }
                Self::Info(info) => {
                    text_ok(&info.version)
                        && text_ok(&info.commit)
                        && text_ok(&info.input_device)
                        && text_ok(&info.output_device)
                }
                Self::State(state) => optional_ok(&state.detail),
                Self::Close(close) => optional_ok(&close.code) && optional_ok(&close.message),
            };
        if valid {
            Ok(())
        } else {
            Err(format!("{} payload is out of bounds", self.kind()))
        }
    }

    fn data(&self) -> serde_json::Result<String> {
        match self {
            Self::Open(open) => serde_json::to_string(open),
            Self::Offer(sdp) | Self::Answer(sdp) => serde_json::to_string(sdp),
            Self::Mute(mute) => serde_json::to_string(mute),
            Self::Control(control) => serde_json::to_string(control),
            Self::Applied(applied) => serde_json::to_string(applied),
            Self::Levels(levels) => serde_json::to_string(levels),
            Self::Info(info) => serde_json::to_string(info),
            Self::State(state) => serde_json::to_string(state),
            Self::Close(close) => serde_json::to_string(close),
        }
    }

    pub fn server_message(&self) -> serde_json::Result<ServerMessage> {
        Ok(ServerMessage::EndpointControl {
            kind: self.kind().to_owned(),
            data: self.data()?,
        })
    }

    pub fn client_message(&self) -> serde_json::Result<ClientMessage> {
        Ok(ClientMessage::EndpointControl {
            kind: self.kind().to_owned(),
            data: self.data()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_controls() -> Vec<MediaControl> {
        vec![
            MediaControl::Open(MediaOpen {
                session_id: "media_1".into(),
                pane_id: "w1:p2".into(),
            }),
            MediaControl::Offer(MediaSdp {
                session_id: "media_1".into(),
                sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".into(),
            }),
            MediaControl::Answer(MediaSdp {
                session_id: "media_1".into(),
                sdp: "v=0\r\n".into(),
            }),
            MediaControl::Mute(MediaMute {
                session_id: "media_1".into(),
                muted: true,
            }),
            MediaControl::Control(MediaDeviceControl {
                session_id: "media_1".into(),
                control_id: 7,
                input_muted: Some(true),
                output_suppressed: Some(false),
                levels: Some(true),
            }),
            MediaControl::Applied(MediaApplied {
                session_id: "media_1".into(),
                control_id: 7,
                input_muted: true,
                output_suppressed: false,
            }),
            MediaControl::Levels(MediaLevels {
                session_id: "media_1".into(),
                input_peak: 0.25,
                output_peak: 0.75,
            }),
            MediaControl::Info(MediaInfo {
                session_id: "media_1".into(),
                version: "0.1.0".into(),
                commit: "abc123".into(),
                pid: 42,
                input_device: "mic".into(),
                output_device: "speaker".into(),
            }),
            MediaControl::State(MediaStateUpdate {
                session_id: "media_1".into(),
                state: MediaPeerState::Connected,
                muted: false,
                detail: None,
            }),
            MediaControl::Close(MediaClose::new("media_1", close_code::DECLINED, "no")),
        ]
    }

    #[test]
    fn media_controls_round_trip_through_both_directions() {
        for control in all_controls() {
            let ServerMessage::EndpointControl { kind, data } = control.server_message().unwrap()
            else {
                panic!("media controls are endpoint controls");
            };
            assert_eq!(kind, control.kind());
            assert_eq!(
                MediaControl::decode(&kind, &data),
                Some(Ok(control.clone()))
            );

            let ClientMessage::EndpointControl { kind, data } = control.client_message().unwrap()
            else {
                panic!("media controls are endpoint controls");
            };
            assert_eq!(
                MediaControl::decode(&kind, &data),
                Some(Ok(control.clone()))
            );

            // The bincode frame an old peer receives is the frozen EndpointControl envelope.
            let message = control.client_message().unwrap();
            let mut framed = Vec::new();
            crate::protocol::write_message(&mut framed, &message).unwrap();
            let decoded: ClientMessage = crate::protocol::read_message(
                &mut framed.as_slice(),
                crate::protocol::MAX_FRAME_SIZE,
            )
            .unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn media_kinds_are_versioned_and_unknown_kinds_are_ignored() {
        for control in all_controls() {
            assert!(control.kind().starts_with("media."));
            assert!(control.kind().ends_with(".v1"));
        }
        assert_eq!(MediaControl::decode("media.open.v2", "{}"), None);
        assert_eq!(MediaControl::decode("endpoint.health.ping.v1", ""), None);
        assert!(MediaControl::is_media_kind("media.open.v2"));
    }

    #[test]
    fn media_controls_tolerate_added_fields_and_unknown_states() {
        let decoded = MediaControl::decode(
            MEDIA_STATE_KIND,
            r#"{"session_id":"m","state":"reconnecting","muted":true,"future":1}"#,
        );
        assert_eq!(
            decoded,
            Some(Ok(MediaControl::State(MediaStateUpdate {
                session_id: "m".into(),
                state: MediaPeerState::Unknown,
                muted: true,
                detail: None,
            })))
        );
        let decoded = MediaControl::decode(MEDIA_CLOSE_KIND, r#"{"session_id":"m"}"#);
        assert_eq!(
            decoded,
            Some(Ok(MediaControl::Close(MediaClose {
                session_id: "m".into(),
                code: None,
                message: None,
            })))
        );
    }

    #[test]
    fn media_controls_reject_invalid_payloads() {
        assert!(matches!(
            MediaControl::decode(MEDIA_OFFER_KIND, "not json"),
            Some(Err(_))
        ));
        assert!(matches!(
            MediaControl::decode(MEDIA_OFFER_KIND, r#"{"session_id":"","sdp":"v=0"}"#),
            Some(Err(_))
        ));
        let huge = serde_json::to_string(&MediaSdp {
            session_id: "m".into(),
            sdp: "a".repeat(MAX_MEDIA_SDP_BYTES + 1),
        })
        .unwrap();
        assert!(matches!(
            MediaControl::decode(MEDIA_OFFER_KIND, &huge),
            Some(Err(_))
        ));
    }

    #[test]
    fn media_levels_require_finite_peaks_in_range() {
        for value in ["NaN", "Infinity", "-Infinity", "-0.01", "1.01"] {
            let data = format!(
                r#"{{"session_id":"m","input_peak":{},"output_peak":0.5}}"#,
                value
            );
            assert!(matches!(
                MediaControl::decode(MEDIA_LEVELS_KIND, &data),
                Some(Err(_)),
            ));
        }
        assert!(MediaControl::Levels(MediaLevels {
            session_id: "m".into(),
            input_peak: f32::NAN,
            output_peak: 0.5,
        })
        .validate()
        .is_err());
        assert!(MediaControl::Levels(MediaLevels {
            session_id: "m".into(),
            input_peak: 0.5,
            output_peak: f32::INFINITY,
        })
        .validate()
        .is_err());
    }

    #[test]
    fn media_info_device_names_are_bounded_by_utf8_bytes() {
        let huge = serde_json::to_string(&MediaInfo {
            session_id: "m".into(),
            version: "0.1.0".into(),
            commit: "abc".into(),
            pid: 42,
            input_device: "界".repeat(MAX_MEDIA_TEXT_BYTES),
            output_device: "speaker".into(),
        })
        .unwrap();
        assert!(matches!(
            MediaControl::decode(MEDIA_INFO_KIND, &huge),
            Some(Err(_))
        ));
    }
}
