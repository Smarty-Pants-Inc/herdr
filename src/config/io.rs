use std::path::{Path, PathBuf};

use tracing::warn;

use super::{model::LoadedConfig, Config, CONFIG_PATH_ENV_VAR};

const KNOWN_TOP_LEVEL_CONFIG_KEYS: &[&str] = &[
    "advanced",
    "experimental",
    "identity",
    "keys",
    "media",
    "onboarding",
    "remote",
    "server",
    "session",
    "terminal",
    "theme",
    "ui",
    "update",
    "voice",
    "worktrees",
];

pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    #[cfg(test)]
    return test_root().join("config").join(app_dir_name());
    #[cfg(not(test))]
    platform_config_dir()
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    #[cfg(test)]
    return test_root().join("state").join(app_dir_name());
    #[cfg(not(test))]
    platform_state_dir()
}

/// Unit tests never fall back to the user's real config or state dirs
/// (for example `~/.config/herdr/agent-detection`); tests that need a
/// specific root still set `XDG_CONFIG_HOME` / `XDG_STATE_HOME`.
#[cfg(test)]
fn test_root() -> PathBuf {
    std::env::temp_dir()
        .join("herdr-unit-tests")
        .join(std::process::id().to_string())
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
fn platform_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("APPDATA") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Roaming")
            .join(app_dir_name());
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(format!(".config/{}", app_dir_name()));
    }
    std::env::temp_dir().join(app_dir_name())
}

#[cfg(not(windows))]
#[cfg_attr(test, allow(dead_code))]
fn platform_config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".config/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(app_dir_name())
    }
}

#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
fn platform_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join(app_dir_name());
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Local")
            .join(app_dir_name());
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()));
    }
    std::env::temp_dir().join(format!("{}-state", app_dir_name()))
}

#[cfg(not(windows))]
#[cfg_attr(test, allow(dead_code))]
fn platform_state_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(format!(".local/state/{}", app_dir_name()))
    } else {
        std::env::temp_dir().join(format!("{}-state", app_dir_name()))
    }
}

/// Normalize UTF-8 byte-order marks in config text.
///
/// TOML tolerates a single BOM at the very start of the document, but a BOM at
/// the start of a later line makes the parser reject the whole file. A
/// line-oriented edit can displace a leading BOM into the middle of the file,
/// so drop line-start BOMs that the TOML parser actually rejects. A U+FEFF that
/// is valid string data is kept, because its parse error would not point at it.
fn normalize_utf8_bom(content: &str) -> String {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !content.contains('\u{feff}') {
        return content.to_owned();
    }

    let mut normalized = content.to_owned();
    while let Err(error) = normalized.parse::<toml::Value>() {
        let Some(span) = error.span() else {
            break;
        };
        if normalized.get(span.clone()) != Some("\u{feff}") {
            break;
        }
        normalized.replace_range(span, "");
    }
    normalized
}

pub(super) fn read_optional_config(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(normalize_utf8_bom(&content))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

impl super::VoiceConfig {
    /// Read host-local audio preferences without inspecting hardware or contacting a server.
    pub(crate) fn load_at(path: &Path) -> Result<Self, String> {
        let content = read_optional_config(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let Some(content) = content else {
            return Ok(Self::default());
        };
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct VoiceSettings {
            voice: super::VoiceConfig,
        }
        toml::from_str::<VoiceSettings>(&content)
            .map(|settings| settings.voice)
            .map_err(|error| format!("invalid config at {}: {error}", path.display()))
    }
}

impl Config {
    pub fn load() -> LoadedConfig {
        Self::load_at(&config_path())
    }

    pub(crate) fn load_at(path: &Path) -> LoadedConfig {
        let content = match read_optional_config(path) {
            Ok(Some(content)) => content,
            Ok(None) => {
                return LoadedConfig {
                    config: Self::default(),
                    diagnostics: Vec::new(),
                    invalid_sections: Vec::new(),
                };
            }
            Err(err) => {
                warn!(err = %err, "config read error, using defaults");
                return LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config read error: {err}; using defaults")],
                    invalid_sections: Vec::new(),
                };
            }
        };

        match deserialize_with_ignored::<Config, _>(toml::Deserializer::new(&content)) {
            Ok((config, ignored_keys)) => {
                let (unknown_sections, mut diagnostics) =
                    unknown_top_level_sections_from_str(&content);
                diagnostics.extend(unknown_config_key_diagnostics(
                    ignored_keys
                        .into_iter()
                        .filter(|path| {
                            !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                        })
                        .collect(),
                    None,
                ));
                diagnostics.extend(config.collect_diagnostics());
                LoadedConfig {
                    config,
                    diagnostics,
                    invalid_sections: Vec::new(),
                }
            }
            Err(err) => {
                warn!(err = %err, "config parse error, using defaults");
                LoadedConfig {
                    config: Self::default(),
                    diagnostics: vec![format!("config parse error: {err}; using defaults")],
                    invalid_sections: Vec::new(),
                }
            }
        }
    }
}

pub(super) fn resolve_config_relative_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    config_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(path)
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var(CONFIG_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    config_dir().join("config.toml")
}

pub fn config_diagnostic_summary(diagnostics: &[String]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }

    let target = config_path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml")
        .to_string();
    let read_error = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.starts_with("config read error:"));
    let impact = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("using defaults"))
    {
        if read_error {
            " unreadable; using defaults"
        } else {
            " invalid; using defaults"
        }
    } else if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("keeping current config"))
    {
        if read_error {
            " unreadable; keeping current config"
        } else {
            " invalid; keeping current config"
        }
    } else if diagnostics
        .iter()
        .all(|diagnostic| diagnostic.starts_with("unknown config key "))
    {
        " has unknown keys"
    } else {
        ""
    };

    Some(format!("{target}{impact}; herdr config check"))
}

pub fn load_live_config() -> Result<LoadedConfig, Vec<String>> {
    let path = config_path();
    let content = match read_optional_config(&path) {
        Ok(Some(content)) => content,
        Ok(None) => {
            return Ok(LoadedConfig {
                config: Config::default(),
                diagnostics: Vec::new(),
                invalid_sections: Vec::new(),
            });
        }
        Err(err) => {
            return Err(vec![format!(
                "config read error: {err}; keeping current config"
            )]);
        }
    };
    load_live_config_from_str(&content)
}

fn load_live_config_from_str(content: &str) -> Result<LoadedConfig, Vec<String>> {
    let value = content
        .parse::<toml::Value>()
        .map_err(|err| vec![format!("config parse error: {err}; keeping current config")])?;
    let table = value.as_table().ok_or_else(|| {
        vec![
            "config parse error: top-level config must be a table; keeping current config"
                .to_string(),
        ]
    })?;

    let mut config = Config::default();
    let mut diagnostics = unknown_top_level_section_diagnostics(table);
    diagnostics.extend(unknown_top_level_config_key_diagnostics(table));
    let mut invalid_sections = Vec::new();

    if let Some(value) = table.get("onboarding") {
        match value.clone().try_into::<Option<bool>>() {
            Ok(onboarding) => config.onboarding = onboarding,
            Err(err) => diagnostics.push(format!(
                "invalid onboarding setting: {err}; keeping current onboarding state"
            )),
        }
    }

    if let Some(value) = table.get("media") {
        match value.clone().try_into::<super::model::MediaMode>() {
            Ok(media) => config.media = media,
            Err(err) => diagnostics.push(format!(
                "invalid media setting: {err}; expected \"off\", \"ask\" or \"auto\"; keeping current media mode"
            )),
        }
    }

    load_live_section(
        table,
        "voice",
        "voice config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.voice = section,
    );
    load_live_section(
        table,
        "identity",
        "identity config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.identity = section,
    );
    load_live_section(
        table,
        "theme",
        "theme config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.theme = section,
    );
    load_live_section(
        table,
        "keys",
        "keybinding config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.keys = section,
    );
    load_live_section(
        table,
        "terminal",
        "terminal config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.terminal = section,
    );
    load_live_section(
        table,
        "session",
        "session config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.session = section,
    );
    load_live_section(
        table,
        "server",
        "server config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.server = section,
    );
    load_live_section(
        table,
        "update",
        "update config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.update = section,
    );
    load_live_section(
        table,
        "ui",
        "ui config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.ui = section,
    );
    load_live_section(
        table,
        "advanced",
        "advanced config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.advanced = section,
    );
    load_live_section(
        table,
        "worktrees",
        "worktree config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.worktrees = section,
    );
    load_live_section(
        table,
        "experimental",
        "experimental config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.experimental = section,
    );
    load_live_section(
        table,
        "remote",
        "remote config",
        &mut diagnostics,
        &mut invalid_sections,
        |section| config.remote = section,
    );

    diagnostics.extend(config.theme.diagnostics());

    Ok(LoadedConfig {
        config,
        diagnostics,
        invalid_sections,
    })
}

fn unknown_top_level_sections_from_str(content: &str) -> (Vec<String>, Vec<String>) {
    let Ok(value) = content.parse::<toml::Value>() else {
        return (Vec::new(), Vec::new());
    };
    let Some(table) = value.as_table() else {
        return (Vec::new(), Vec::new());
    };

    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for (key, value) in table {
        if let Some(diagnostic) = unknown_top_level_section_diagnostic(key, value) {
            keys.push(key.clone());
            diagnostics.push(diagnostic);
        }
    }
    (keys, diagnostics)
}

fn unknown_top_level_section_diagnostics(
    table: &toml::map::Map<String, toml::Value>,
) -> Vec<String> {
    table
        .iter()
        .filter_map(|(key, value)| unknown_top_level_section_diagnostic(key, value))
        .collect()
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    if KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key) {
        return None;
    }

    let header = if value.is_table() {
        format!("[{key}]")
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        format!("[[{key}]]")
    } else {
        return None;
    };

    if key == "toast" {
        Some(format!(
            "unknown config section {header}; did you mean [ui.toast]? ignoring section"
        ))
    } else {
        Some(format!("unknown config section {header}; ignoring section"))
    }
}

fn unknown_top_level_config_key_diagnostics(
    table: &toml::map::Map<String, toml::Value>,
) -> Vec<String> {
    let paths = table
        .iter()
        .filter(|(key, value)| {
            !KNOWN_TOP_LEVEL_CONFIG_KEYS.contains(&key.as_str())
                && unknown_top_level_section_diagnostic(key, value).is_none()
        })
        .map(|(key, _)| vec![ConfigKeyPathSegment::Key(key.clone())])
        .collect();
    unknown_config_key_diagnostics(paths, None)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConfigKeyPathSegment {
    Key(String),
    Index(usize),
}

fn config_key_path(path: &serde_ignored::Path<'_>) -> Vec<ConfigKeyPathSegment> {
    fn visit(path: &serde_ignored::Path<'_>, segments: &mut Vec<ConfigKeyPathSegment>) {
        match path {
            serde_ignored::Path::Root => {}
            serde_ignored::Path::Seq { parent, index } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Index(*index));
            }
            serde_ignored::Path::Map { parent, key } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Key(key.clone()));
            }
            serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => visit(parent, segments),
        }
    }

    let mut segments = Vec::new();
    visit(path, &mut segments);
    segments
}

fn format_config_key_path(path: &[ConfigKeyPathSegment]) -> String {
    path.iter()
        .map(|segment| match segment {
            ConfigKeyPathSegment::Key(key)
                if !key.is_empty()
                    && key.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                    }) =>
            {
                key.clone()
            }
            ConfigKeyPathSegment::Key(key) => toml::Value::String(key.clone()).to_string(),
            ConfigKeyPathSegment::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn unknown_config_key_diagnostics(
    paths: Vec<Vec<ConfigKeyPathSegment>>,
    section: Option<&str>,
) -> Vec<String> {
    let mut paths: Vec<Vec<ConfigKeyPathSegment>> = paths
        .into_iter()
        .map(|mut path| {
            if let Some(section) = section {
                path.insert(0, ConfigKeyPathSegment::Key(section.to_string()));
            }
            path
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            format!(
                "unknown config key {}; ignoring key",
                format_config_key_path(&path)
            )
        })
        .collect()
}

fn deserialize_with_ignored<'de, T, D>(
    deserializer: D,
) -> Result<(T, Vec<Vec<ConfigKeyPathSegment>>), D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        ignored.push(config_key_path(&path));
    })?;
    Ok((value, ignored))
}

fn load_live_section<T>(
    table: &toml::map::Map<String, toml::Value>,
    section: &'static str,
    label: &str,
    diagnostics: &mut Vec<String>,
    invalid_sections: &mut Vec<String>,
    apply: impl FnOnce(T),
) where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = table.get(section) else {
        return;
    };

    match deserialize_with_ignored(value.clone()) {
        Ok((section_config, ignored_keys)) => {
            diagnostics.extend(unknown_config_key_diagnostics(ignored_keys, Some(section)));
            apply(section_config);
        }
        Err(err) => {
            diagnostics.push(format!(
                "invalid {label}: {err}; keeping current {section} settings"
            ));
            invalid_sections.push(section.to_string());
        }
    }
}

pub(crate) fn upsert_top_level_bool(content: &str, key: &str, value: bool) -> String {
    let replacement = format!("{key} = {value}");
    let mut lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
    let mut in_section = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = true;
            continue;
        }
        if in_section {
            continue;
        }
        if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
            *line = replacement.clone();
            return lines.join("\n") + "\n";
        }
    }

    if lines.is_empty() {
        format!("{replacement}\n")
    } else {
        format!("{replacement}\n{}\n", lines.join("\n").trim_end())
    }
}

/// Write a key = value pair in a TOML section (creates section if missing).
pub fn upsert_section_value(content: &str, section: &str, key: &str, value: &str) -> String {
    upsert_section_raw(content, section, key, value)
}

pub fn upsert_section_bool(content: &str, section: &str, key: &str, value: bool) -> String {
    upsert_section_raw(content, section, key, &value.to_string())
}

pub fn remove_section_key(content: &str, section: &str, key: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let structural = toml_structural_lines(content);
    let mut result = Vec::new();
    let mut i = 0;
    let mut in_section = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if let Some(name) = structural[i]
            .then(|| toml_table_header_name(toml_line_without_comment(trimmed)))
            .flatten()
        {
            in_section = !trimmed.starts_with("[[") && name == section;
            result.push(line.to_string());
            i += 1;
            continue;
        }

        if structural[i] && in_section && toml_line_assigns_key(trimmed, key) {
            let value_part = toml_line_without_comment(line);
            if value_part.len() < line.len() {
                result.push(line[value_part.len()..].to_string());
            }
            i += 1;
            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    result.join("\n") + "\n"
}

pub fn remove_keybinding_config_sections(content: &str) -> (String, bool) {
    let mut result = Vec::new();
    let mut removed = false;
    let mut skipping_key_section = false;
    let mut in_table = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(table_name) = toml_table_header_name(trimmed) {
            in_table = true;
            skipping_key_section = is_keys_table_name(table_name);
            if skipping_key_section {
                removed = true;
                continue;
            }
        } else if skipping_key_section || (!in_table && is_top_level_keys_assignment(trimmed)) {
            removed = true;
            continue;
        }

        result.push(line.to_string());
    }

    let mut updated = result.join("\n");
    if content.ends_with('\n') || !updated.is_empty() {
        updated.push('\n');
    }
    (updated, removed)
}

fn toml_table_header_name(trimmed: &str) -> Option<&str> {
    let trimmed = trimmed.trim();
    if let Some(name) = trimmed
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
    {
        return Some(name.trim());
    }
    trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .map(str::trim)
}

fn is_keys_table_name(name: &str) -> bool {
    name == "keys" || name.starts_with("keys.")
}

fn is_top_level_keys_assignment(trimmed: &str) -> bool {
    trimmed.starts_with("keys ") || trimmed.starts_with("keys=") || trimmed.starts_with("keys.")
}

/// Find a trailing comment without confusing a # inside a quoted string for one.
fn toml_line_without_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if quote == Some('"') && escaped {
            escaped = false;
            continue;
        }
        match (quote, ch) {
            (Some('"'), '\\') => escaped = true,
            (Some(current), ch) if current == ch => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

/// Lines beginning inside a multiline TOML string, array or inline table are data, not table headers or keys
/// (a commented array row such as `["terminal_title"] # note` is not a header, herdr#127 r1).
/// Track lexical quote state and bracket depth across lines without changing the original contents.
fn toml_structural_lines(content: &str) -> Vec<bool> {
    let mut quote: Option<(u8, bool)> = None;
    let mut depth = 0usize;
    content
        .lines()
        .map(|line| {
            let structural = quote.is_none() && depth == 0;
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                match quote {
                    None if bytes[i] == b'#' => break,
                    None if matches!(bytes[i], b'[' | b'{') => {
                        depth += 1;
                        i += 1;
                    }
                    None if matches!(bytes[i], b']' | b'}') => {
                        depth = depth.saturating_sub(1);
                        i += 1;
                    }
                    None if matches!(bytes[i], b'\'' | b'"') => {
                        let ch = bytes[i];
                        let multiline = bytes[i..].starts_with(&[ch; 3]);
                        quote = Some((ch, multiline));
                        i += if multiline { 3 } else { 1 };
                    }
                    Some((b'"', _)) if bytes[i] == b'\\' => i += 2,
                    Some((ch, true)) if bytes[i..].starts_with(&[ch; 3]) => {
                        // Four/five closing quotes include one/two literal quote characters.
                        while i < bytes.len() && bytes[i] == ch {
                            i += 1;
                        }
                        quote = None;
                    }
                    Some((ch, false)) if bytes[i] == ch => {
                        quote = None;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            if quote.is_some_and(|(_, multiline)| !multiline) {
                quote = None;
            }
            structural
        })
        .collect()
}

fn toml_line_assigns_key(line: &str, key: &str) -> bool {
    let Some((name, _)) = line.split_once('=') else {
        return false;
    };
    let name = name.trim();
    name == key || name == format!("\"{key}\"") || name == format!("'{key}'")
}

fn upsert_section_raw(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let assignment = format!("{key} = {value}");
    let lines: Vec<&str> = content.lines().collect();
    let structural = toml_structural_lines(content);
    let mut result = Vec::new();
    let mut i = 0;
    let mut found_section = false;
    let mut inserted = false;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if structural[i]
            && !trimmed.starts_with("[[")
            && toml_table_header_name(toml_line_without_comment(trimmed)) == Some(section)
        {
            found_section = true;
            result.push(line.to_string());
            i += 1;

            while i < lines.len() {
                let current = lines[i];
                let current_trimmed = current.trim();
                if structural[i]
                    && toml_table_header_name(toml_line_without_comment(current_trimmed)).is_some()
                {
                    if !inserted {
                        result.push(assignment.clone());
                        inserted = true;
                    }
                    break;
                }

                if structural[i] && toml_line_assigns_key(current_trimmed, key) {
                    let value_part = toml_line_without_comment(current);
                    let comment = &current[value_part.len()..];
                    let indentation = &current[..current.len() - current.trim_start().len()];
                    result.push(if comment.is_empty() {
                        format!("{indentation}{assignment}")
                    } else {
                        format!("{indentation}{assignment} {comment}")
                    });
                    inserted = true;
                } else {
                    result.push(current.to_string());
                }
                i += 1;
            }

            continue;
        }

        result.push(line.to_string());
        i += 1;
    }

    if !found_section {
        if !result.is_empty() && !result.last().is_some_and(|line| line.trim().is_empty()) {
            result.push(String::new());
        }
        result.push(header);
        result.push(assignment);
    } else if !inserted {
        result.push(assignment);
    }

    result.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_top_level_bool_replaces_existing_value() {
        let content = "onboarding = true\n[keys]\nprefix = \"ctrl+b\"\n";
        let updated = upsert_top_level_bool(content, "onboarding", false);
        assert!(updated.contains("onboarding = false"));
        assert!(!updated.contains("onboarding = true"));
    }

    #[test]
    fn upsert_section_bool_adds_missing_section() {
        let updated = upsert_section_bool("", "ui.toast", "enabled", true);
        assert!(updated.contains("[ui.toast]"));
        assert!(updated.contains("enabled = true"));
    }

    #[test]
    fn remove_section_key_ignores_array_rows_that_look_like_headers() {
        let content =
            "[voice]\ndevices = [\n  [\"ui\"] # note\n]\ninput = \"a\"\n[ui]\ninput = \"x\"\n";
        let updated = remove_section_key(content, "voice", "input");
        let parsed = updated.parse::<toml::Value>().unwrap();
        assert!(parsed["voice"].get("input").is_none(), "{updated}");
        assert_eq!(parsed["ui"]["input"].as_str(), Some("x"));
        assert!(updated.contains("[\"ui\"] # note"), "{updated}");
    }

    #[test]
    fn remove_section_key_removes_matching_key_from_section() {
        let content =
            "[ui.toast]\nenabled = true\ndelivery = \"herdr\"\n[ui.sound]\nenabled = true\n";
        let updated = remove_section_key(content, "ui.toast", "enabled");
        assert!(!updated.contains("[ui.toast]\nenabled = true"));
        assert!(updated.contains("delivery = \"herdr\""));
        assert!(updated.contains("[ui.sound]\nenabled = true"));
    }

    #[test]
    fn section_edits_preserve_comments_and_respect_commented_headers() {
        let content = "# host settings\n[ voice ] # local audio\n\t'input'\t= 'old # mic' # keep this\n[ui] # unrelated\ninput = 'not audio'\n";
        let updated = upsert_section_value(content, "voice", "input", "\"new # mic\"");
        assert!(updated.contains("# host settings\n[ voice ] # local audio"));
        assert!(updated.contains("\tinput = \"new # mic\" # keep this"));
        assert!(updated.contains("[ui] # unrelated\ninput = 'not audio'"));
        let cleared = remove_section_key(&updated, "voice", "input");
        assert!(cleared.contains("# keep this"));
        assert!(cleared.contains("input = 'not audio'"));
        assert_eq!(
            cleared.parse::<toml::Value>().unwrap()["voice"]
                .as_table()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn settings_edits_preserve_commented_headers_inside_multiline_commands() {
        for delimiter in ["'''", "\"\"\""] {
            let content = format!(
                "[[keys.command]]\nkey = 'prefix+alt+p'\ntype = 'shell'\ncommand = {delimiter}\ncat <<'EOF'\n[ui.sound] # sample text, not a table\n enabled = true\nEOF\n{delimiter}\n"
            );
            let original: toml::Value = content.parse().unwrap();
            let updated = crate::config::ConfigEdit::Sound(false).apply(&content);
            let updated: toml::Value = updated.parse().unwrap();
            assert_eq!(updated["keys"], original["keys"], "changed a command");
            assert_eq!(updated["ui"]["sound"]["enabled"].as_bool(), Some(false));
        }
    }

    #[test]
    fn section_upsert_and_remove_preserve_multiline_values_in_real_sections() {
        for delimiter in ["'''", "\"\"\""] {
            let content = format!(
                "[voice]\nnotes = {delimiter}\n[voice] # literal header\ninput = 'sample text'\n[ui] # also literal\n{delimiter}\ninput = 'actual mic'\noutput = 'speaker'\n[ui] # real table\nmouse_capture = false\n"
            );
            let original: toml::Value = content.parse().unwrap();
            let updated = upsert_section_value(&content, "voice", "input", "'new mic'");
            let parsed: toml::Value = updated.parse().unwrap();
            assert_eq!(parsed["voice"]["notes"], original["voice"]["notes"]);
            assert_eq!(parsed["voice"]["input"].as_str(), Some("new mic"));
            assert_eq!(parsed["ui"], original["ui"]);
            let removed: toml::Value = remove_section_key(&updated, "voice", "input")
                .parse()
                .unwrap();
            assert!(removed["voice"].get("input").is_none());
            assert_eq!(removed["voice"]["notes"], original["voice"]["notes"]);
            assert_eq!(removed["voice"]["output"], original["voice"]["output"]);
        }
    }

    #[test]
    fn section_edits_do_not_close_multiline_basic_strings_at_escaped_quotes() {
        let content = r#"[[keys.command]]
key = 'prefix+alt+p'
type = 'shell'
command = """
\"""
[ui.sound] # still inside the command
enabled = true
"""
"#;
        let original: toml::Value = content.parse().unwrap();
        let updated: toml::Value = crate::config::ConfigEdit::Sound(false)
            .apply(content)
            .parse()
            .unwrap();
        assert_eq!(updated["keys"], original["keys"]);
        assert_eq!(updated["ui"]["sound"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn section_edits_ignore_hashes_inside_escaped_strings() {
        let content = "[voice]\ninput = \"old \\\"# mic\" # comment\n";
        let updated = upsert_section_value(content, "voice", "input", "\"new\"");
        assert_eq!(updated, "[voice]\ninput = \"new\" # comment\n");
    }

    #[test]
    fn config_diagnostic_summary_uses_compact_actionable_banner() {
        let diagnostics = vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
            "five".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unknown_keys_compactly() {
        let diagnostics = vec![
            "unknown config key ui.mouse_captur; ignoring key".to_string(),
            "unknown config key keys.new_tabb; ignoring key".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml has unknown keys; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_keeps_mixed_diagnostics_generic() {
        let diagnostics = vec![
            "invalid ui config: invalid type: string; keeping current ui settings".to_string(),
            "unknown config key keys.new_tabb; ignoring key".to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_default_fallback() {
        let diagnostics = vec![
            "config parse error: TOML parse error at line 33, column 8\n   |\n33 | type = \"popup\"\n   |        ^^^^^^^\nunknown variant `popup`; using defaults"
                .to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml invalid; using defaults; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_unreadable_config_impact() {
        let startup = vec!["config read error: permission denied; using defaults".to_string()];
        assert_eq!(
            config_diagnostic_summary(&startup).as_deref(),
            Some("config.toml unreadable; using defaults; herdr config check")
        );

        let reload =
            vec!["config read error: permission denied; keeping current config".to_string()];
        assert_eq!(
            config_diagnostic_summary(&reload).as_deref(),
            Some("config.toml unreadable; keeping current config; herdr config check")
        );
    }

    #[test]
    fn config_diagnostic_summary_reports_retained_live_config() {
        let diagnostics = vec![
            "config parse error: TOML parse error at line 7, column 4; keeping current config"
                .to_string(),
        ];

        assert_eq!(
            config_diagnostic_summary(&diagnostics).as_deref(),
            Some("config.toml invalid; keeping current config; herdr config check")
        );
    }

    #[test]
    fn config_loaders_report_unreadable_path() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path =
            std::env::temp_dir().join(format!("herdr-config-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let startup = Config::load();
        assert!(startup
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("config read error")
                && diagnostic.contains("using defaults")));

        let reload = load_live_config().unwrap_err();
        assert!(reload.iter().any(|diagnostic| {
            diagnostic.contains("config read error")
                && diagnostic.contains("keeping current config")
        }));

        std::env::remove_var(CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn load_live_config_recognizes_client_identity() {
        let loaded = load_live_config_from_str("[identity]\nname = \"Alice\"\n").unwrap();
        assert_eq!(loaded.config.identity.name.as_deref(), Some("Alice"));
        assert!(loaded.diagnostics.is_empty());
        let loaded = load_live_config_from_str("[identity]\nname = 42\n").unwrap();
        assert!(loaded
            .invalid_sections
            .iter()
            .any(|section| section == "identity"));
    }

    #[test]
    fn load_live_config_parses_media_mode_and_reports_invalid_values() {
        let loaded = load_live_config_from_str("media = \"off\"\n").unwrap();
        assert_eq!(loaded.config.media, crate::config::MediaMode::Off);
        assert!(loaded.diagnostics.is_empty());

        let loaded = load_live_config_from_str("media = \"sometimes\"\n").unwrap();
        assert_eq!(loaded.config.media, crate::config::MediaMode::Ask);
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.starts_with("invalid media setting")));
    }

    #[test]
    fn load_live_config_parses_voice_names_and_reports_unknown_keys() {
        let loaded = load_live_config_from_str(
            "[voice]\ninput = 'USB microphone'\noutput = 'default'\nextra = true\n",
        )
        .unwrap();
        assert_eq!(loaded.config.voice.input.as_deref(), Some("USB microphone"));
        assert_eq!(loaded.config.voice.output.as_deref(), Some("default"));
        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config key voice.extra; ignoring key"]
        );
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_rejects_invalid_voice_section_without_losing_siblings() {
        let loaded =
            load_live_config_from_str("[voice]\ninput = 42\n[server]\nheadless_cols = 160\n")
                .unwrap();
        assert_eq!(loaded.config.voice, super::super::VoiceConfig::default());
        assert_eq!(loaded.config.server.headless_cols, 160);
        assert_eq!(loaded.invalid_sections, vec!["voice"]);
        assert!(loaded.diagnostics[0].starts_with("invalid voice config:"));
    }

    #[test]
    fn load_live_config_accepts_voice_when_an_unrelated_section_is_invalid() {
        let loaded = load_live_config_from_str(
            "[voice]\ninput = 'Built-in Microphone'\noutput = 'Built-in Output'\n[ui]\nmouse_capture = 'invalid'\n",
        )
        .unwrap();
        assert_eq!(
            loaded.config.voice.input.as_deref(),
            Some("Built-in Microphone")
        );
        assert_eq!(
            loaded.config.voice.output.as_deref(),
            Some("Built-in Output")
        );
        assert_eq!(loaded.invalid_sections, vec!["ui"]);
    }

    #[test]
    fn load_live_config_marks_oversized_voice_names_invalid() {
        let name = "é".repeat(crate::protocol::media::MAX_MEDIA_TEXT_BYTES / 2 + 1);
        let content = format!("[voice]\noutput = {name:?}\n");
        let loaded = load_live_config_from_str(&content).unwrap();
        assert_eq!(loaded.config.voice, super::super::VoiceConfig::default());
        assert_eq!(loaded.invalid_sections, vec!["voice"]);
        assert!(loaded.diagnostics[0].contains("UTF-8 bytes"));
    }

    #[test]
    fn load_live_config_parses_session_section() {
        let loaded = load_live_config_from_str(
            r#"
[session]
resume_agents_on_restore = true
"#,
        )
        .unwrap();

        assert!(loaded.config.session.resume_agents_on_restore);
        assert!(loaded.diagnostics.is_empty());
        assert!(loaded.invalid_sections.is_empty());
    }

    #[test]
    fn load_live_config_warns_about_unknown_theme_names() {
        let loaded = load_live_config_from_str(
            r#"
[theme]
name = "catppucin"
"#,
        )
        .unwrap();

        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(loaded.diagnostics[0].contains("theme.name = \"catppucin\""));
    }

    #[test]
    fn load_live_config_warns_about_unknown_top_level_sections() {
        let loaded = load_live_config_from_str(
            r#"
[toast]
delivery = "system"

[ui.toast]
delivery = "herdr"
"#,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [toast]; did you mean [ui.toast]? ignoring section"]
        );
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Herdr
        );
    }

    #[test]
    fn load_live_config_warns_about_unknown_keys_and_applies_known_siblings() {
        let loaded = load_live_config_from_str(
            r##"
plugin = []

[theme.custom]
accentt = "#ffffff"

[advanced]
scrollback_lines = 42

[keys]
fullscreen = "prefix+z"
new_tabb = "prefix+t"

[[keys.command]]
key = "prefix+g"
command = "git status"
descrption = "status"

[ui]
mouse_capture = false
mouse_captur = true
"foo.bar" = true
"foo.?.bar" = false

[ui.toast]
enabled = true
delivry = "system"

[ui.sidebar.agents.rows_by_agent]
claude = [["terminal_title"]]
"##,
        )
        .unwrap();

        assert_eq!(
            loaded.diagnostics,
            vec![
                "unknown config key plugin; ignoring key",
                "unknown config key theme.custom.accentt; ignoring key",
                "unknown config key keys.command.0.descrption; ignoring key",
                "unknown config key keys.new_tabb; ignoring key",
                "unknown config key ui.\"foo.?.bar\"; ignoring key",
                "unknown config key ui.\"foo.bar\"; ignoring key",
                "unknown config key ui.mouse_captur; ignoring key",
                "unknown config key ui.toast.delivry; ignoring key",
            ]
        );
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(loaded.config.advanced.scrollback_limit_bytes, 42);
        assert!(!loaded.config.ui.mouse_capture);
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::Herdr
        );
        assert!(loaded
            .config
            .keybinds()
            .zoom
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+z"));
    }

    #[test]
    fn load_live_config_accepts_legacy_agent_panel_scope_without_warning() {
        let loaded = load_live_config_from_str(
            r#"
[ui]
agent_panel_scope = "current"
agent_panel_sort = "priority"
"#,
        )
        .unwrap();

        assert!(loaded.diagnostics.is_empty());
        assert!(loaded.invalid_sections.is_empty());
        assert_eq!(
            loaded.config.ui.agent_panel_sort,
            super::super::AgentPanelSortConfig::Priority
        );
    }

    #[test]
    fn load_live_config_discards_ignored_keys_from_an_invalid_section() {
        let loaded = load_live_config_from_str(
            r#"
[ui]
mouse_capture = "yes"
mouse_captur = true
"#,
        )
        .unwrap();

        assert_eq!(loaded.diagnostics.len(), 1);
        assert!(loaded.diagnostics[0].contains("invalid ui config"));
        assert!(!loaded.diagnostics[0].starts_with("unknown config key"));
        assert_eq!(loaded.invalid_sections, vec!["ui"]);
    }

    #[test]
    fn startup_config_accepts_legacy_agent_panel_scope_without_warning() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-config-legacy-agent-panel-scope-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[ui]\nagent_panel_scope = \"all\"\n").unwrap();
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let loaded = Config::load();

        std::env::remove_var(CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn startup_config_load_warns_about_unknown_top_level_sections() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-config-unknown-section-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[[plugin]]
id = "example"

[ui.toast]
delivery = "system"
"#,
        )
        .unwrap();
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let loaded = Config::load();

        assert_eq!(
            loaded.diagnostics,
            vec!["unknown config section [[plugin]]; ignoring section"]
        );
        assert_eq!(
            loaded.config.ui.toast.delivery,
            super::super::ToastDelivery::System
        );

        std::env::remove_var(CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remove_keybinding_config_sections_removes_keys_tables_only() {
        let content = r#"onboarding = false

[theme]
name = "catppuccin"

[keys]
prefix = "ctrl+a"
new_tab = "c"

[[keys.command]]
key = "g"
command = "lazygit"

[keys.indexed]
tabs = "ctrl"

[ui]
mouse_capture = false
"#;

        let (updated, removed) = remove_keybinding_config_sections(content);

        assert!(removed);
        assert!(updated.contains("onboarding = false"));
        assert!(updated.contains("[theme]\nname = \"catppuccin\""));
        assert!(updated.contains("[ui]\nmouse_capture = false"));
        assert!(!updated.contains("[keys]"));
        assert!(!updated.contains("[[keys.command]]"));
        assert!(!updated.contains("[keys.indexed]"));
        assert!(toml::from_str::<toml::Value>(&updated).is_ok());
    }

    #[test]
    fn remove_keybinding_config_sections_reports_noop_without_keys() {
        let content = "[ui]\nmouse_capture = true\n";
        let (updated, removed) = remove_keybinding_config_sections(content);
        assert!(!removed);
        assert_eq!(updated, content);
    }

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}onboarding = false\n[terminal]\n";
        assert_eq!(
            normalize_utf8_bom(content),
            "onboarding = false\n[terminal]\n"
        );
    }

    #[test]
    fn normalize_utf8_bom_recovers_from_a_displaced_mid_file_bom() {
        let content = "onboarding = false\n\u{feff}[terminal]\ndefault_shell = \"pwsh.exe\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(
            normalized,
            "onboarding = false\n[terminal]\ndefault_shell = \"pwsh.exe\"\n"
        );
        assert!(normalized.parse::<toml::Value>().is_ok());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(content.parse::<toml::Value>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(content.parse::<toml::Value>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(content.parse::<toml::Value>().is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn config_load_recovers_from_a_mid_file_bom() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-config-mid-file-bom-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            b"onboarding = false\n\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();
        std::env::set_var(CONFIG_PATH_ENV_VAR, &path);

        let loaded = Config::load();

        std::env::remove_var(CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_file(path);

        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(loaded.config.terminal.default_shell, "pwsh.exe");
    }
}
