//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_PREVIEW_UPDATE_MANIFEST_URL: &str = "https://herdr.dev/preview.json";

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

#[cfg(unix)]
pub fn commit() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_COMMIT"))
}

pub fn omp_build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_OMP_BUILD_ID"))
}

pub fn omp_commit() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_OMP_COMMIT"))
}

pub fn omp_tree() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_OMP_TREE"))
}

pub fn omp_version() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_OMP_VERSION"))
}

/// Optional compile-time update manifest for fork builds.
pub fn update_manifest_url() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_UPDATE_MANIFEST_URL"))
}

pub fn effective_update_manifest_url() -> Option<&'static str> {
    update_manifest_url().or_else(|| is_preview().then_some(DEFAULT_PREVIEW_UPDATE_MANIFEST_URL))
}

pub fn server_build_identity() -> Option<crate::api::schema::ServerBuildIdentity> {
    Some(crate::api::schema::ServerBuildIdentity {
        channel: channel().to_string(),
        build_id: build_id()?.to_string(),
        update_manifest_url: effective_update_manifest_url()?.to_string(),
    })
}

/// Whether this build must use preview-style update selection.
pub fn uses_preview_update_manifest() -> bool {
    update_manifest_url().is_some() || is_preview()
}

/// Whether this published build may install preview updates from a running client.
#[cfg(not(windows))]
pub fn client_auto_update_enabled() -> bool {
    matches!(
        non_empty(option_env!("HERDR_BUILD_AUTO_UPDATE")),
        Some("1" | "true")
    )
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }
}
