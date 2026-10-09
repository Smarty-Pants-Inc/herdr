//! Local registered-channel JSON contracts. These are not binary endpoint codecs.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentRegisterSelfParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub session_generation: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub draft_guard: bool,
    /// Installed by the direct local transport, never deserialized from caller data.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) transport: Option<crate::api::agent_channel::RegistrationTransport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentChannelInfoParams {
    pub target: String,
}

/// A read-only observation, never authority for a later prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDraftStateParams {
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentPromptGuardedParams {
    pub target: String,
    pub text: String,
    pub expected_terminal: String,
    pub expected_registration_epoch: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub if_draft_empty: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_timeout"
    )]
    #[schemars(with = "u64")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub allow_cross_pane: bool,
}

fn deserialize_timeout<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdmissionStatus {
    Accepted,
    Queued,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdmissionReason {
    NoSession,
    SessionChanged,
    PayloadMismatch,
    ShuttingDown,
    AdmissionRefused,
    Unsupported,
    Unknown,
    DraftPresent,
    UiHold,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionAck {
    #[serde(rename = "type")]
    pub kind: String,
    pub registration_epoch: String,
    pub request_id: String,
    pub session_generation: String,
    pub status: AdmissionStatus,
    #[serde(default)]
    pub reason: Option<AdmissionReason>,
    #[serde(default)]
    pub duplicate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftHold {
    Dialog,
    Custom,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftStateUnknownReason {
    Unregistered,
    Unsupported,
    Timeout,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentDraftStateResult {
    Known {
        empty: bool,
        chars: u64,
        hold: Option<DraftHold>,
    },
    Unknown {
        reason: DraftStateUnknownReason,
    },
}

// Separate closed shapes enforce exact known/unknown fields, including a required
// (possibly null) hold. No text or arbitrary caller data can enter the result.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum DraftStateAck {
    Known(DraftStateKnownAck),
    Unknown(DraftStateUnknownAck),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftStateKnownAck {
    #[serde(rename = "type")]
    pub kind: String,
    pub registration_epoch: String,
    pub request_id: String,
    pub session_generation: String,
    pub empty: bool,
    pub chars: u64,
    #[serde(deserialize_with = "deserialize_hold")]
    pub hold: Option<DraftHold>,
}

fn deserialize_hold<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DraftHold>, D::Error> {
    Option::<DraftHold>::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftStateUnknownAck {
    #[serde(rename = "type")]
    pub kind: String,
    pub registration_epoch: String,
    pub request_id: String,
    pub session_generation: String,
    pub unknown: bool,
}
