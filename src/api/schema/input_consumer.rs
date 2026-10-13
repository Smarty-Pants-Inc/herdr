//! Local-only authenticated PTY consumer requests.
//!
//! Offsets are byte counts (exclusive ends): the first interval is `[0, cut)`.
//! Requests carry no credential: Herdr accepts cut and release only from the
//! enrolled process incarnation, and signs every cut answer (smarty-dev#2636 E′).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaneInputConsumerEnrollParams {
    pub pane_id: String,
    /// Fresh 256-bit consumer challenge as 64 lowercase hex characters; the
    /// answer's Ed25519 `sig` covers it.
    pub challenge: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputConsumerCutKind {
    Submit,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaneInputConsumerCutParams {
    pub epoch: String,
    pub seq: u64,
    pub token: String,
    /// Number of post-marker bytes through the end of this interval.
    pub cut: u64,
    pub digest: String,
    pub kind: InputConsumerCutKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaneInputConsumerReleaseParams {
    pub epoch: String,
}
