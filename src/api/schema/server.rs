use serde::{Deserialize, Serialize};

fn omp_maintenance_operation_id_schema(
    _generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 43,
        "maxLength": 43,
        "pattern": "^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$"
    })
}

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerOmpMaintenanceAcquireParams {
    #[schemars(schema_with = "omp_maintenance_operation_id_schema")]
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerOmpMaintenancePermitParams {
    #[schemars(schema_with = "omp_maintenance_operation_id_schema")]
    pub operation_id: String,
    pub session: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerOmpMaintenanceReleaseParams {
    #[schemars(schema_with = "omp_maintenance_operation_id_schema")]
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerOmpMaintenancePermit {
    pub session: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerOmpMaintenanceRoute {
    pub session: String,
    pub pane_id: String,
    pub omp_session_id: String,
    pub route_generation: u64,
    pub proof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerOmpMaintenanceStatus {
    pub schema: String,
    pub held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<ServerOmpMaintenancePermit>,
    pub route_count: usize,
    pub routes: Vec<ServerOmpMaintenanceRoute>,
    /// Monotonic revision of the host-wide public route membership set.
    #[serde(default)]
    pub route_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
    #[serde(default)]
    pub omp_maintenance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerBuildIdentity {
    pub channel: String,
    pub build_id: String,
    pub update_manifest_url: String,
}
