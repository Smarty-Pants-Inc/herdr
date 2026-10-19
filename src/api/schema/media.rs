use serde::{Deserialize, Serialize};

// Client-local media sessions (see `crate::protocol::media`). `pane.media_open` accepts
// an optional caller handle; the other methods address one session.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaOpenParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), pattern(r"^[\x20-\x7E]+(?![\s\S])"))]
    pub open_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaSessionTarget {
    pub session_id: String,
}

/// Select exactly one `session_id` or caller-selected `open_id` to close.
///
/// Closing an unknown `open_id` is idempotent and records a cancellation tombstone for up to
/// 120 seconds. At most 256 unknown-id tombstones are retained; the media `expire` pass prunes
/// elapsed ones. A later open with that id is refused rather than creating a client session.
/// An optional epoch must match the resolved session; a mismatch returns
/// media_session_not_found without closing it. Omission retains legacy behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(extend(
    "oneOf" = [
        {
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"]
        },
        {
            "properties": { "open_id": { "type": "string" } },
            "required": ["open_id"]
        }
    ]
))]
pub struct MediaCloseParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), pattern(r"^[\x20-\x7E]+(?![\s\S])"))]
    pub open_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
}

/// Dispatch the SDP answer immediately, without waiting for Connected. The session
/// must connect within 15 seconds after the answer or close with negotiation_timeout.
/// An optional epoch must match the session or return media_session_not_found;
/// omission retains legacy behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAnswerParams {
    pub session_id: String,
    pub sdp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
}

/// Requires media_control capability. Controls are ordered per session in admission
/// order and allowed before the answer (Offered). Omitted fields retain desired intent.
/// The response is held until the matching client control_id is applied, returning
/// media_controls_applied with request_id equal to the outer API id and current epoch.
/// A 5-second media_timeout is uncertain: application may still occur, and the session
/// stays open. Session end returns media_control_failed with a causal close_code.
/// Both desired fields survive device_changed, close and renewal; renewed capture
/// starts pre-muted when input mute intent is true. Legacy media.mute is dispatch-only.
/// An optional epoch mismatch returns media_session_not_found without an effect;
/// omission retains legacy behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaControlParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_suppressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaMuteParams {
    pub session_id: String,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionState {
    Opening,
    Offered,
    Connecting,
    Connected,
    Failed,
    Closed,
}
