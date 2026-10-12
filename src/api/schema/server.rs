use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    /// Refuse the handoff unless the server handling it has this process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_source_pid: Option<u32>,
    /// Refuse the handoff unless the server handling it owns the API socket
    /// with this inode and that socket is the one at the public path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_socket_inode: Option<u64>,
    /// `server.live_handoff_pull` only: the token the importer that sent the
    /// request presents on the handoff socket. The source spawns no replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerSshAgentRegisterParams {
    /// Absolute remote-host agent socket. Registration lasts until this API connection closes.
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
    /// Stable client-owned endpoint generation supported by this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_protocol_generation: Option<u32>,
    /// Whether this server supports explicit client-shell surface interest.
    #[serde(default)]
    pub surface_interest: bool,
    /// Whether this server supports endpoint health probes.
    #[serde(default)]
    pub health_check: bool,
    /// Supports connection-scoped `server.ssh_agent.register` on the local JSON API.
    #[serde(default)]
    pub ssh_agent_registration: bool,
    /// Supports `server.live_handoff_guarded`, which refuses an unexpected
    /// source server and a replacement that cannot report its sockets.
    #[serde(default)]
    pub guarded_live_handoff: bool,
    /// Supports the fail-closed `expected_terminal` guard on `agent.start` and `pane.send_input`.
    #[serde(default)]
    pub expected_terminal_guard: bool,
    /// Supports the local JSON session-checked prompt, text and key methods.
    #[serde(default)]
    pub expected_agent_session_guard: bool,
    /// Supports the local `pane.input_consumer.*` methods (Linux servers only).
    #[serde(default)]
    pub input_consumer: bool,
}
