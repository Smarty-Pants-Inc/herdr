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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAnswerParams {
    pub session_id: String,
    pub sdp: String,
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
