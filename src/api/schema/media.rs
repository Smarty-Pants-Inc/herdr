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

/// Requires `media_control` capability. Controls are ordered per session in
/// admission order and may be submitted while the named session is `Offered`,
/// before `media.answer`. Omitted fields retain desired intent. The API response
/// is held until this named session acknowledges the matching control; a
/// successor's acknowledgement cannot settle the source request. Success is
/// `media_controls_applied` with `session_id`, the outer API `request_id`, the
/// applied `input_muted` and `output_suppressed` values, and the current `epoch`.
/// A 5-second `media_timeout` is uncertain: application may still occur, and the
/// session stays open. If the named session ends first, the response is
/// `media_control_failed` with its causal `close_code`; a successor's application
/// is reported in its own `media.controls_applied` event with the successor's
/// session ID. Before a session reaches `Offered` (while opening/pending),
/// or for an unknown, stale, or epoch-mismatched target, the error is
/// `media_session_not_found`. Exhausting the 32 unresolved target-device
/// control slots returns `media_control_failed` with a clear message and no
/// `close_code`, without ending the live session.
/// Desired input-mute and output-suppression intent survives session close and
/// renewal until explicitly changed or the pane is removed from the live
/// topology; it is not persisted across server restart. New and renewed client
/// sessions install both desired gates before `audio.open`. Renewal closes the
/// old peer before opening its successor; if successor `audio.open` fails, the
/// call ends without audio and the voice caller must renew. Applied
/// acknowledgements never overwrite newer desired intent. Legacy `media.mute` is
/// dispatch-only. An optional epoch mismatch returns `media_session_not_found`
/// without an effect; omission retains legacy behavior.
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
