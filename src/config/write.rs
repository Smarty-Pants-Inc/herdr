#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    ToastDelivery(super::ToastDelivery),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
        }
    }

    pub(crate) fn apply(self, content: &str) -> String {
        match self {
            Self::Theme(name) => {
                let content =
                    super::upsert_section_value(content, "theme", "name", &format!("\"{name}\""));
                super::upsert_section_bool(&content, "theme", "auto_switch", false)
            }
            Self::StatusIndicators(style) => super::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            ),
            Self::Sound(enabled) => {
                super::upsert_section_bool(content, "ui.sound", "enabled", enabled)
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Herdr => "\"herdr\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
        }
    }
}

impl super::VoiceConfig {
    /// Save names in the existing host-local config, preserving unrelated settings and comments.
    pub(crate) fn save_at(&self, path: &std::path::Path) -> Result<(), String> {
        self.validate()?;
        update_file_at_checked(path, "voice devices", |content| {
            let mut expected = content.parse::<toml::Value>().map_err(|error| {
                format!(
                    "invalid config at {}: {error}; leaving it unchanged",
                    path.display()
                )
            })?;
            let root = expected
                .as_table_mut()
                .ok_or_else(|| "config must be a table; leaving config unchanged".to_string())?;
            let mut updated = content.to_owned();
            for (key, name) in [("input", &self.input), ("output", &self.output)] {
                if let Some(name) = name {
                    let table = root
                        .entry("voice".to_string())
                        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                        .as_table_mut()
                        .ok_or_else(|| {
                            "voice config must be a table; leaving config unchanged".to_string()
                        })?;
                    table.insert(key.to_string(), toml::Value::String(name.clone()));
                    // Keep a one-line basic string, including TOML's additional DEL restriction.
                    let literal = serde_json::to_string(name)
                        .map_err(|error| format!("failed to encode voice device name: {error}"))?
                        .replace('\u{7f}', "\\u007f");
                    updated = super::upsert_section_value(&updated, "voice", key, &literal);
                } else {
                    if let Some(value) = root.get_mut("voice") {
                        let table = value.as_table_mut().ok_or_else(|| {
                            "voice config must be a table; leaving config unchanged".to_string()
                        })?;
                        table.remove(key);
                    }
                    updated = super::remove_section_key(&updated, "voice", key);
                }
            }
            let actual = updated.parse::<toml::Value>().map_err(|error| {
                format!("could not safely edit voice devices: {error}; leaving config unchanged")
            })?;
            if actual != expected {
                return Err("could not safely edit [voice] without changing unrelated settings; use input/output keys in a [voice] table; leaving config unchanged".to_string());
            }
            Ok(updated)
        })
    }
}

pub(crate) fn update_file_at(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    update_file_at_checked(path, description, |content| Ok(update(content)))
}

fn update_file_at_checked(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> Result<String, String>,
) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create config directory: {error}"))?;
    }
    let content = match super::io::read_optional_config(path) {
        Ok(Some(content)) => content,
        Ok(None) => String::new(),
        Err(error) => {
            return Err(format!(
                "failed to read config before saving {description}: {error}"
            ));
        }
    };
    let updated = update(&content)?;
    crate::integration::config_file::write_config(path, updated)
        .map_err(|error| format!("failed to save {description}: {error}"))
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        edit.apply(content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_names_persist_across_explicit_path_restart_and_clear_independently() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "# preserve me\nmedia = 'off'\n[voice] # this host\ninput = 'old mic' # mic note\noutput = 'old speaker' # speaker note\n[ui] # other settings\nmouse_capture = false\n";
        std::fs::write(&path, original).unwrap();
        let names = super::super::VoiceConfig {
            input: Some("USB \"microphone\" \\ name\n\t# 日本語".to_string()),
            output: Some("default".to_string()),
        };
        names.save_at(&path).unwrap();

        let restarted = super::super::Config::load_at(&path);
        assert!(
            restarted.diagnostics.is_empty(),
            "{:?}",
            restarted.diagnostics
        );
        assert_eq!(restarted.config.voice, names);
        assert_eq!(restarted.config.media, super::super::MediaMode::Off);
        assert!(!restarted.config.ui.mouse_capture);
        let written = std::fs::read_to_string(&path).unwrap();
        for comment in [
            "# preserve me",
            "# this host",
            "# mic note",
            "# speaker note",
            "# other settings",
        ] {
            assert!(written.contains(comment), "lost {comment}: {written}");
        }

        let cleared_input = super::super::VoiceConfig {
            input: None,
            ..names
        };
        cleared_input.save_at(&path).unwrap();
        assert_eq!(
            super::super::VoiceConfig::load_at(&path).unwrap(),
            cleared_input
        );
        let restarted = super::super::Config::load_at(&path);
        assert_eq!(restarted.config.voice.input, None);
        assert_eq!(restarted.config.voice.output.as_deref(), Some("default"));

        super::super::VoiceConfig::default().save_at(&path).unwrap();
        let restarted = super::super::Config::load_at(&path);
        assert_eq!(restarted.config.voice, super::super::VoiceConfig::default());
        assert!(restarted.diagnostics.is_empty());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# mic note"));
        assert!(written.contains("# speaker note"));
        assert!(written.contains("mouse_capture = false"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exact_del_device_names_persist_when_both_directions_are_saved() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-del-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut names = super::super::VoiceConfig {
            input: Some("Mic\u{7f}".to_owned()),
            output: None,
        };
        names.save_at(&path).unwrap();
        assert_eq!(super::super::VoiceConfig::load_at(&path).unwrap(), names);
        // A valid pre-existing escaped name must not prevent changing the other direction.
        std::fs::write(&path, "[voice]\ninput = \"Mic\\u007f\"\n").unwrap();
        names = super::super::VoiceConfig::load_at(&path).unwrap();
        names.output = Some("Speakers".to_owned());
        names.save_at(&path).unwrap();
        assert_eq!(super::super::VoiceConfig::load_at(&path).unwrap(), names);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_save_creates_config_without_hardware_and_rejects_unsafe_edits() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-create-{}", std::process::id()));
        let path = dir.join("nested/config.toml");
        let names = super::super::VoiceConfig {
            input: Some("unplugged microphone".to_string()),
            output: None,
        };
        names.save_at(&path).unwrap();
        assert_eq!(super::super::Config::load_at(&path).config.voice, names);
        for original in [
            "[voice\ninput = 'bad'\n",
            "voice = { input = 'old' } # keep inline\n",
            "[voice]\ninput = '''old\nmultiline mic'''\n",
        ] {
            std::fs::write(&path, original).unwrap();
            assert!(names.save_at(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_save_rejects_invalid_names_before_creating_or_changing_config() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-invalid-{}", std::process::id()));
        let path = dir.join("config.toml");
        let limit = crate::protocol::media::MAX_MEDIA_TEXT_BYTES;
        for invalid in [
            String::new(),
            " \t\n".to_string(),
            "é".repeat(limit / 2 + 1),
        ] {
            let names = super::super::VoiceConfig {
                input: None,
                output: Some(invalid),
            };
            assert!(names.save_at(&path).is_err());
            assert!(!path.exists());
            assert!(!dir.exists());
        }
    }

    // Native audio reloads these settings at call start on macOS. Unix publication
    // must never expose the empty or partial file produced by an in-place write.
    #[cfg(unix)]
    #[test]
    fn voice_concurrent_readers_observe_only_complete_old_or_new_preferences() {
        use std::sync::{Arc, Barrier};
        let dir = std::env::temp_dir().join(format!("herdr-voice-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, format!("# {}\n", "unrelated comments ".repeat(512))).unwrap();
        let old = super::super::VoiceConfig {
            input: Some("old microphone".to_string()),
            output: Some("old speakers".to_string()),
        };
        let new = super::super::VoiceConfig {
            input: Some("new microphone".to_string()),
            output: Some("new speakers".to_string()),
        };
        old.save_at(&path).unwrap();
        let old_content = std::fs::read_to_string(&path).unwrap();
        new.save_at(&path).unwrap();
        let new_content = std::fs::read_to_string(&path).unwrap();
        old.save_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old_content);
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let reader_barrier = Arc::clone(&barrier);
            let path = &path;
            let old = &old;
            let new = &new;
            let old_content = &old_content;
            let new_content = &new_content;
            scope.spawn(move || {
                reader_barrier.wait();
                for _ in 0..2048 {
                    let content = std::fs::read_to_string(path).unwrap();
                    assert!(
                        content == *old_content || content == *new_content,
                        "partial config became visible"
                    );
                    let loaded = super::super::VoiceConfig::load_at(path).unwrap();
                    assert!(
                        loaded == *old || loaded == *new,
                        "partial preferences became visible: {loaded:?}"
                    );
                }
            });
            // Start both bounded loops together; no wall-clock timing assertions.
            barrier.wait();
            for _ in 0..32 {
                new.save_at(path).unwrap();
                old.save_at(path).unwrap();
            }
        });
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "staging files leaked"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn voice_save_preserves_symlink_target_permissions_and_unrelated_settings() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("herdr-voice-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("preferences.toml");
        let path = dir.join("config.toml");
        std::fs::write(&target, "# user config\n[ui]\nmouse_capture = false\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink("preferences.toml", &path).unwrap();
        let names = super::super::VoiceConfig {
            input: Some("USB microphone".to_string()),
            output: None,
        };
        names.save_at(&path).unwrap();
        assert_eq!(
            std::fs::read_link(&path).unwrap(),
            std::path::Path::new("preferences.toml")
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(super::super::VoiceConfig::load_at(&path).unwrap(), names);
        assert!(
            !super::super::Config::load_at(&target)
                .config
                .ui
                .mouse_capture
        );
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .contains("# user config"));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "staging files leaked"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_missing_config_uses_system_defaults_without_creating_a_file() {
        let path = std::env::temp_dir().join(format!(
            "herdr-voice-missing-{}/config.toml",
            std::process::id()
        ));
        assert!(!path.exists());
        assert_eq!(
            super::super::VoiceConfig::load_at(&path).unwrap(),
            super::super::VoiceConfig::default()
        );
        assert!(!path.exists());
        let config: super::super::Config =
            toml::from_str("[voice]\ninput = 'system default'\n").unwrap();
        assert_eq!(config.voice.input.as_deref(), Some("system default"));
        assert_eq!(config.voice.output, None);
        assert_eq!(
            toml::from_str::<super::super::VoiceConfig>(&toml::to_string(&config.voice).unwrap())
                .unwrap(),
            config.voice
        );
    }

    #[test]
    fn update_file_at_does_not_move_a_leading_bom_into_the_file() {
        let dir = std::env::temp_dir().join(format!("herdr-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();

        update_file_at(&path, "onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            !written.contains('\u{feff}'),
            "unexpected BOM in {written:?}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_ok(),
            "written config is not valid TOML: {written:?}"
        );
    }
}
