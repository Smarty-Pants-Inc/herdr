//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

/// Optional compile-time update manifest for fork builds.
pub fn update_manifest_url() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_UPDATE_MANIFEST_URL"))
}

/// Whether this build must use preview-style update selection.
pub fn uses_preview_update_manifest() -> bool {
    update_manifest_url().is_some() || is_preview()
}

/// Whether this published build may install preview updates from a running client.
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
