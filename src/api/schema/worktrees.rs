use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorktreeListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub trust_repository: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorktreeCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub trust_repository: bool,
}

/// Project-checked creation, separate from the frozen legacy endpoint shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorktreeCreateProjectCheckedParams {
    #[serde(flatten)]
    pub params: WorktreeCreateParams,
    /// Explicit permission to change ownership of a surviving agent session.
    #[serde(default)]
    pub allow_project_change: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorktreeOpenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub trust_repository: bool,
}

/// Project-checked opening, separate from the frozen legacy endpoint shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorktreeOpenProjectCheckedParams {
    #[serde(flatten)]
    pub params: WorktreeOpenParams,
    /// Explicit permission to change ownership of a surviving agent session.
    #[serde(default)]
    pub allow_project_change: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeRemoveParams {
    pub workspace_id: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub trust_repository: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeSourceInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub source_checkout_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
    pub is_linked_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_workspace_id: Option<String>,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_project_change_permission_is_opt_in() {
        for permission in [None, Some(false), Some(true)] {
            let mut params = serde_json::json!({"branch": "worktree/test", "focus": true});
            if let Some(allow) = permission {
                params["allow_project_change"] = serde_json::json!(allow);
            }
            let create: WorktreeCreateProjectCheckedParams =
                serde_json::from_value(params.clone()).unwrap();
            let open: WorktreeOpenProjectCheckedParams = serde_json::from_value(params).unwrap();
            let expected = permission.unwrap_or(false);
            assert_eq!(create.allow_project_change, expected);
            assert_eq!(open.allow_project_change, expected);
            for serialized in [
                serde_json::to_value(create).unwrap(),
                serde_json::to_value(open).unwrap(),
            ] {
                assert_eq!(serialized["allow_project_change"], expected);
                assert_eq!(serialized["branch"], "worktree/test");
                assert_eq!(serialized["focus"], true);
                assert!(serialized.get("params").is_none());
            }
        }
    }

    #[test]
    fn legacy_worktree_schema_has_no_project_change_field() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(WorktreeCreateParams)).unwrap(),
            serde_json::to_value(schemars::schema_for!(WorktreeOpenParams)).unwrap(),
        ] {
            assert!(schema["properties"].get("allow_project_change").is_none());
        }
    }

    #[test]
    fn worktree_project_change_schema_is_optional_and_defaults_false() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(WorktreeCreateProjectCheckedParams))
                .unwrap(),
            serde_json::to_value(schemars::schema_for!(WorktreeOpenProjectCheckedParams)).unwrap(),
        ] {
            assert_eq!(
                schema["properties"]["allow_project_change"]["type"],
                "boolean"
            );
            assert_eq!(
                schema["properties"]["allow_project_change"]["default"],
                false
            );
            assert!(!schema["required"]
                .as_array()
                .is_some_and(|required| required
                    .iter()
                    .any(|field| field == "allow_project_change")));
        }
    }
}
