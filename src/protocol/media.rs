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

/// One audio device opened on the client, not a server-side selection request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAudioDevice {
    /// Actual opened device name, or display-only `default` for an unreportable OS default.
    /// This label is never a device selector.
    pub name: String,
    /// Requested device that was missing when the client used the default, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// The local devices actually opened for this offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAudioDevices {
    pub input: MediaAudioDevice,
    pub output: MediaAudioDevice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSdp {
    pub session_id: String,
    pub sdp: String,
    /// Offer-only device metadata. Older clients omit it; answers emit `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_devices: Option<MediaAudioDevices>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMute {
    pub session_id: String,
    pub muted: bool,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaControl {
    Open(MediaOpen),
    Offer(MediaSdp),
    Answer(MediaSdp),
    Mute(MediaMute),
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
            Self::State(_) => MEDIA_STATE_KIND,
            Self::Close(_) => MEDIA_CLOSE_KIND,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Open(open) => &open.session_id,
            Self::Offer(sdp) | Self::Answer(sdp) => &sdp.session_id,
            Self::Mute(mute) => &mute.session_id,
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
                    !sdp.sdp.is_empty()
                        && sdp.sdp.len() <= MAX_MEDIA_SDP_BYTES
                        && sdp.audio_devices.as_ref().is_none_or(|devices| {
                            [&devices.input, &devices.output].into_iter().all(|device| {
                                !device.name.is_empty()
                                    && text_ok(&device.name)
                                    && optional_ok(&device.missing)
                            })
                        })
                }
                Self::Mute(_) => true,
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

    fn audio_devices() -> MediaAudioDevices {
        MediaAudioDevices {
            input: MediaAudioDevice {
                name: "USB microphone".into(),
                missing: Some("Preferred microphone".into()),
            },
            output: MediaAudioDevice {
                name: "Headphones".into(),
                missing: None,
            },
        }
    }

    fn all_controls() -> Vec<MediaControl> {
        vec![
            MediaControl::Open(MediaOpen {
                session_id: "media_1".into(),
                pane_id: "w1:p2".into(),
            }),
            MediaControl::Offer(MediaSdp {
                session_id: "media_1".into(),
                sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".into(),
                audio_devices: Some(audio_devices()),
            }),
            MediaControl::Answer(MediaSdp {
                session_id: "media_1".into(),
                sdp: "v=0\r\n".into(),
                audio_devices: None,
            }),
            MediaControl::Mute(MediaMute {
                session_id: "media_1".into(),
                muted: true,
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

            // The answer and offer use the same two-string frozen envelope in either direction.
            let message = control.server_message().unwrap();
            let mut framed = Vec::new();
            crate::protocol::write_message(&mut framed, &message).unwrap();
            let decoded: ServerMessage = crate::protocol::read_message(
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
    fn sdp_device_metadata_is_optional_for_older_offers_and_answers() {
        for kind in [MEDIA_OFFER_KIND, MEDIA_ANSWER_KIND] {
            for data in [
                r#"{"session_id":"m","sdp":"v=0"}"#,
                r#"{"session_id":"m","sdp":"v=0","audio_devices":null}"#,
            ] {
                let control = MediaControl::decode(kind, data).unwrap().unwrap();
                let sdp = match &control {
                    MediaControl::Offer(sdp) | MediaControl::Answer(sdp) => sdp,
                    _ => panic!("expected SDP"),
                };
                assert!(sdp.audio_devices.is_none());
                assert_eq!(control.data().unwrap(), r#"{"session_id":"m","sdp":"v=0"}"#);
            }
        }
    }

    #[test]
    fn offer_metadata_has_the_exact_optional_json_shape() {
        let control = MediaControl::Offer(MediaSdp {
            session_id: "m".into(),
            sdp: "v=0".into(),
            audio_devices: Some(audio_devices()),
        });
        let data = serde_json::from_str::<serde_json::Value>(&control.data().unwrap()).unwrap();
        assert_eq!(
            data,
            serde_json::json!({
                "session_id": "m",
                "sdp": "v=0",
                "audio_devices": {
                    "input": {"name": "USB microphone", "missing": "Preferred microphone"},
                    "output": {"name": "Headphones"}
                }
            })
        );
        assert_eq!(
            MediaControl::decode(MEDIA_OFFER_KIND, &data.to_string()),
            Some(Ok(control))
        );
    }

    #[test]
    fn sdp_device_metadata_is_bounded_at_decode_in_both_directions() {
        for kind in [MEDIA_OFFER_KIND, MEDIA_ANSWER_KIND] {
            for input in [true, false] {
                for invalid in ["", &"x".repeat(MAX_MEDIA_TEXT_BYTES + 1), &"é".repeat(257)] {
                    let mut devices = audio_devices();
                    let device = if input {
                        &mut devices.input
                    } else {
                        &mut devices.output
                    };
                    device.name = invalid.to_owned();
                    let data = serde_json::to_string(&MediaSdp {
                        session_id: "m".into(),
                        sdp: "v=0".into(),
                        audio_devices: Some(devices),
                    })
                    .unwrap();
                    assert!(matches!(MediaControl::decode(kind, &data), Some(Err(_))));
                }
                let mut devices = audio_devices();
                let device = if input {
                    &mut devices.input
                } else {
                    &mut devices.output
                };
                device.missing = Some("x".repeat(MAX_MEDIA_TEXT_BYTES + 1));
                let data = serde_json::to_string(&MediaSdp {
                    session_id: "m".into(),
                    sdp: "v=0".into(),
                    audio_devices: Some(devices),
                })
                .unwrap();
                assert!(matches!(MediaControl::decode(kind, &data), Some(Err(_))));
            }
            // Bounds count UTF-8 bytes, not characters, and include both device directions.
            let devices = MediaAudioDevices {
                input: MediaAudioDevice {
                    name: "é".repeat(MAX_MEDIA_TEXT_BYTES / 2),
                    missing: Some("x".repeat(MAX_MEDIA_TEXT_BYTES)),
                },
                output: MediaAudioDevice {
                    name: "x".repeat(MAX_MEDIA_TEXT_BYTES),
                    missing: Some(String::new()),
                },
            };
            let data = serde_json::to_string(&MediaSdp {
                session_id: "m".into(),
                sdp: "v=0".into(),
                audio_devices: Some(devices),
            })
            .unwrap();
            assert!(matches!(MediaControl::decode(kind, &data), Some(Ok(_))));
            // Partial metadata is invalid: omission is supported only for the whole optional field.
            assert!(matches!(
                MediaControl::decode(
                    kind,
                    r#"{"session_id":"m","sdp":"v=0","audio_devices":{"input":{"name":"Mic"}}}"#
                ),
                Some(Err(_))
            ));
        }
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
            audio_devices: None,
        })
        .unwrap();
        assert!(matches!(
            MediaControl::decode(MEDIA_OFFER_KIND, &huge),
            Some(Err(_))
        ));
    }
}
