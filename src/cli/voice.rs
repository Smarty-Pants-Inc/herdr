use std::path::Path;

use crate::client::media::devices::DeviceNames;
use crate::config::VoiceConfig;

#[derive(Debug, Default, PartialEq, Eq)]
struct DevicesOptions {
    input: Option<String>,
    output: Option<String>,
    default_input: bool,
    default_output: bool,
}

impl DevicesOptions {
    fn from_matches(matches: &clap::ArgMatches) -> Self {
        Self {
            input: matches.get_one::<String>("input").cloned(),
            output: matches.get_one::<String>("output").cloned(),
            default_input: matches.get_flag("default-input"),
            default_output: matches.get_flag("default-output"),
        }
    }

    fn changes_preferences(&self) -> bool {
        self.input.is_some() || self.output.is_some() || self.default_input || self.default_output
    }
}

pub(super) fn run_voice_command(args: &[String]) -> std::io::Result<i32> {
    let matches = match super::spec::parse_leaf_args(&["voice"], args) {
        Ok(matches) => matches,
        Err(error) => {
            crate::platform::begin_cli_output();
            error.print()?;
            return Ok(error.exit_code());
        }
    };
    let Some(matches) = matches.subcommand_matches("devices") else {
        return Ok(2);
    };
    let options = DevicesOptions::from_matches(matches);
    match devices_at(
        &options,
        &crate::config::config_path(),
        crate::client::media::devices::list_devices,
    ) {
        Ok(output) => {
            print!("{output}");
            Ok(0)
        }
        Err(error) => {
            eprintln!("{error}");
            Ok(1)
        }
    }
}

/// One direction's change from the command line: a name, a reset to default, or none (herdr#127 r2).
fn change(name: &Option<String>, reset: bool) -> crate::config::DeviceChange<'_> {
    match name {
        Some(name) => Some(Some(name.as_str())),
        None if reset => Some(None),
        None => None,
    }
}

fn devices_at(
    options: &DevicesOptions,
    path: &Path,
    list_devices: impl FnOnce() -> Result<DeviceNames, String>,
) -> Result<String, String> {
    if options.changes_preferences() {
        // herdr#127 r2: only the directions this command changed, applied to the file as it is at the save.
        VoiceConfig::save_changes_at(
            path,
            change(&options.input, options.default_input),
            change(&options.output, options.default_output),
        )?;
        let config = VoiceConfig::load_at(path)?;
        return Ok(format!(
            "{}Saved native voice device preferences. The next call uses these choices; active calls keep their current devices. No restart is needed.\n",
            preferences_summary(path, &config)
        ));
    }

    let config = VoiceConfig::load_at(path)?;
    let mut output = preferences_summary(path, &config);
    let devices = list_devices()
        .map_err(|error| format!("{output}Native voice device listing is unavailable: {error}"))?;
    output.push_str("Input devices:\n");
    append_names(&mut output, &devices.input);
    output.push_str("Output devices:\n");
    append_names(&mut output, &devices.output);
    Ok(output)
}

fn preferences_summary(path: &Path, config: &VoiceConfig) -> String {
    fn choice(name: &Option<String>) -> String {
        match name {
            Some(name) => format!("{name:?}"),
            None => "system default (no saved name)".to_string(),
        }
    }
    format!(
        "Config: {}\nSaved input: {}\nSaved output: {}\n",
        path.display(),
        choice(&config.input),
        choice(&config.output),
    )
}

fn append_names(output: &mut String, names: &[String]) {
    if names.is_empty() {
        output.push_str("  (none)\n");
    }
    for name in names {
        // Quote names so control characters from the host cannot act as terminal commands.
        output.push_str(&format!("  {name:?}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(options: &[&str]) -> DevicesOptions {
        let args = options
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        DevicesOptions::from_matches(
            &crate::cli::spec::parse_leaf_args(&["voice", "devices"], &args).unwrap(),
        )
    }

    /// herdr#127 r2: a concurrent writer holds the config lock and saves both names; this command, which changed
    /// one direction (a name, then a reset to default), keeps that writer's other direction.
    fn concurrent_save_keeps_the_other_direction(command: &[&str], expect_input: Option<&str>) {
        let dir = std::env::temp_dir().join(format!(
            "herdr-voice-cli-race-{}-{}",
            std::process::id(),
            command.join("")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[voice]\ninput = \"old mic\"\noutput = \"old speaker\"\n",
        )
        .unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("toml.lock"))
            .unwrap();
        lock.lock().unwrap(); // the other writer is mid-save,
        let options = parse(command);
        let saving = {
            let path = path.clone();
            std::thread::spawn(move || devices_at(&options, &path, || unreachable!()))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        // and publishes its whole state: the input it read, and its new output.
        std::fs::write(
            &path,
            "[voice]\ninput = \"old mic\"\noutput = \"new speaker\"\n",
        )
        .unwrap();
        lock.unlock().unwrap();
        saving.join().unwrap().unwrap();
        let saved = VoiceConfig::load_at(&path).unwrap();
        assert_eq!(saved.input.as_deref(), expect_input, "{command:?}");
        assert_eq!(saved.output.as_deref(), Some("new speaker"), "{command:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_concurrent_output_edit_survives_an_input_save_and_an_input_reset() {
        concurrent_save_keeps_the_other_direction(&["--input", "new mic"], Some("new mic"));
        concurrent_save_keeps_the_other_direction(&["--default-input"], None);
    }

    #[test]
    fn voice_devices_set_and_reset_without_enumerating_hardware() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-cli-save-{}", std::process::id()));
        let path = dir.join("config.toml");
        let output = devices_at(
            &parse(&["--input", "default", "--output", "unplugged speaker"]),
            &path,
            || panic!("saving names must not enumerate hardware"),
        )
        .unwrap();
        assert!(output.contains(&path.display().to_string()));
        assert!(output.contains("Saved input: \"default\""));
        assert!(output.contains("The next call uses these choices"));
        assert!(output.contains("active calls keep their current devices"));
        assert!(output.contains("No restart is needed"));
        assert_eq!(
            VoiceConfig::load_at(&path).unwrap().input.as_deref(),
            Some("default")
        );

        devices_at(&parse(&["--default-input"]), &path, || {
            panic!("reset must not enumerate hardware")
        })
        .unwrap();
        let restarted = VoiceConfig::load_at(&path).unwrap();
        assert_eq!(restarted.input, None);
        assert_eq!(restarted.output.as_deref(), Some("unplugged speaker"));
        devices_at(&parse(&["--default-output"]), &path, || {
            panic!("reset must not enumerate hardware")
        })
        .unwrap();
        assert_eq!(VoiceConfig::load_at(&path).unwrap(), VoiceConfig::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_devices_lists_names_persisted_choices_and_config_path() {
        let dir = std::env::temp_dir().join(format!("herdr-voice-cli-list-{}", std::process::id()));
        let path = dir.join("config.toml");
        VoiceConfig {
            input: Some("missing mic".to_string()),
            output: None,
        }
        .save_at(&path)
        .unwrap();
        let output = devices_at(&parse(&[]), &path, || {
            Ok(DeviceNames {
                input: vec!["USB microphone".to_string(), "mic\x1b[2J".to_string()],
                output: vec![],
            })
        })
        .unwrap();
        assert!(output.contains(&path.display().to_string()));
        assert!(output.contains("Saved input: \"missing mic\""));
        assert!(output.contains("Saved output: system default (no saved name)"));
        assert!(output.contains("Input devices:\n  \"USB microphone\""));
        assert!(output.contains("Output devices:\n  (none)"));
        assert!(!output.contains('\x1b'));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn voice_devices_unsupported_listing_reports_local_choices_without_writing() {
        let path = std::env::temp_dir().join(format!(
            "herdr-voice-cli-unsupported-{}/config.toml",
            std::process::id()
        ));
        let error = devices_at(&parse(&[]), &path, || {
            Err("native audio is not supported on this host".to_string())
        })
        .unwrap_err();
        assert!(error.contains("Native voice device listing is unavailable"));
        assert!(error.contains("native audio is not supported on this host"));
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("Saved input: system default (no saved name)"));
        assert!(!path.exists());
    }

    #[test]
    fn voice_devices_machine_routing_is_rejected_before_profile_or_connection() {
        for prefix in [
            vec!["--machine", "not-a-real-machine"],
            vec!["--machine=not-a-real-machine"],
        ] {
            let mut args = vec!["herdr"];
            args.extend(prefix);
            args.extend(["voice", "devices", "--input", "mic"]);
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            let outcome = crate::cli::maybe_run_machine(&args).unwrap().unwrap();
            assert!(matches!(outcome, crate::cli::CommandOutcome::Handled(2)));
        }
    }
}
