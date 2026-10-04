//! Connection-local identity, never supplied by the client protocol.
//!
//! Resolve once in the accept/handshake worker, not on the input/render path.
//! A principal identifies an authenticated, configured SSH transport, not physical
//! human presence. Local/shared-UID clients deliberately have no principal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Principal {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClientIdentity {
    pub peer_pid: Option<u32>,
    pub uid: Option<u32>,
    pub principal: Option<Principal>,
}

/// Missing/untrusted configuration, unavailable evidence, or an ambiguous match
/// leaves `principal` absent. Peer PID/UID are diagnostic facts, NOT authority.
pub(crate) fn resolve_client_identity(stream: &crate::ipc::LocalStream) -> ClientIdentity {
    crate::platform::resolve_client_identity(stream)
}
