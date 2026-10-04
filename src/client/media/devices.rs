//! Client-local audio device discovery and selection.
//!
//! Keep selection independent of CPAL so direction, fallback and the default-only fast
//! path can be checked without hardware. Only native macOS builds link an audio host.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceNames {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

/// List the exact names accepted by the client's voice settings.
#[cfg(all(feature = "native-media", target_os = "macos"))]
pub(crate) fn list_devices() -> Result<DeviceNames, String> {
    selection::list_names(&cpal_devices::CpalDevices::new())
}

/// No audio enumeration backend is linked on other platforms or non-native builds.
#[cfg(not(all(feature = "native-media", target_os = "macos")))]
pub(crate) fn list_devices() -> Result<DeviceNames, String> {
    Err("audio device listing requires a macOS client built with native-media".to_owned())
}

#[cfg(any(test, all(feature = "native-media", target_os = "macos")))]
pub(super) mod selection {
    use super::DeviceNames;
    use crate::protocol::media::{MediaAudioDevice, MediaAudioDevices, MAX_MEDIA_TEXT_BYTES};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Direction {
        Input,
        Output,
    }

    impl Direction {
        fn label(self) -> &'static str {
            match self {
                Self::Input => "input",
                Self::Output => "output",
            }
        }
    }

    /// The only host operations selection needs. Enumeration is directional and only
    /// used when a name was requested; handles returned here are the ones audio opens.
    pub(crate) trait DeviceProvider {
        type Device;

        fn default_device(&self, direction: Direction) -> Option<Self::Device>;
        fn devices(&self, direction: Direction) -> Result<Vec<Self::Device>, String>;
        fn name(&self, device: &Self::Device) -> Result<String, String>;
    }

    pub(crate) struct SelectedDevices<D> {
        pub input: D,
        pub output: D,
        pub metadata: MediaAudioDevices,
    }

    fn validate_name(name: &str, direction: Direction) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err(format!(
                "{} audio device name must not be empty",
                direction.label()
            ));
        }
        if name.len() > MAX_MEDIA_TEXT_BYTES {
            return Err(format!(
                "{} audio device name exceeds {MAX_MEDIA_TEXT_BYTES} UTF-8 bytes",
                direction.label()
            ));
        }
        Ok(())
    }

    fn device_name<P: DeviceProvider>(
        provider: &P,
        device: &P::Device,
        direction: Direction,
    ) -> Result<String, String> {
        let name = provider.name(device)?;
        validate_name(&name, direction)?;
        Ok(name)
    }

    fn default_name<P: DeviceProvider>(
        provider: &P,
        device: &P::Device,
        direction: Direction,
    ) -> String {
        match device_name(provider, device, direction) {
            Ok(name) => name,
            Err(error) => {
                // Naming is optional metadata on the old default-only opening path.
                // "default" is a display placeholder, never a truncated/matched selector.
                // Keep any missing preference report even when its default name is unknown.
                tracing::warn!(direction = direction.label(), %error, "could not report default audio device name");
                "default".to_owned()
            }
        }
    }

    fn resolve<P: DeviceProvider>(
        provider: &P,
        direction: Direction,
        requested: Option<&str>,
    ) -> Result<(P::Device, MediaAudioDevice), String> {
        if let Some(requested) = requested {
            for device in provider.devices(direction)? {
                let name = match provider.name(&device) {
                    Ok(name) => name,
                    Err(error) => {
                        // One disappearing/unreadable unrelated device must not prevent
                        // a later exact match or the default-device fallback.
                        tracing::warn!(direction = direction.label(), %error, "could not read enumerated audio device name");
                        continue;
                    }
                };
                if name == requested {
                    return Ok((
                        device,
                        MediaAudioDevice {
                            name,
                            missing: None,
                        },
                    ));
                }
            }
        }
        // Unset means exactly the old default-only path: no host-wide enumeration.
        let device = provider
            .default_device(direction)
            .ok_or_else(|| format!("no default {} audio device", direction.label()))?;
        let name = default_name(provider, &device, direction);
        Ok((
            device,
            MediaAudioDevice {
                name,
                missing: requested.map(str::to_owned),
            },
        ))
    }

    pub(crate) fn select<P: DeviceProvider>(
        provider: &P,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<SelectedDevices<P::Device>, String> {
        // Reject invalid settings before opening or even looking up either device. Keep
        // names exact: trimming or truncation could silently select different hardware.
        for (direction, requested) in [(Direction::Input, input), (Direction::Output, output)] {
            if let Some(name) = requested {
                validate_name(name, direction)?;
            }
        }
        let (input, input_metadata) = resolve(provider, Direction::Input, input)?;
        let (output, output_metadata) = resolve(provider, Direction::Output, output)?;
        Ok(SelectedDevices {
            input,
            output,
            metadata: MediaAudioDevices {
                input: input_metadata,
                output: output_metadata,
            },
        })
    }

    pub(crate) fn list_names(provider: &impl DeviceProvider) -> Result<DeviceNames, String> {
        let names = |direction| {
            provider
                .devices(direction)?
                .iter()
                .map(|device| device_name(provider, device, direction))
                .collect::<Result<Vec<_>, String>>()
        };
        Ok(DeviceNames {
            input: names(Direction::Input)?,
            output: names(Direction::Output)?,
        })
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
pub(super) mod cpal_devices {
    use cpal::traits::{DeviceTrait, HostTrait};

    use super::selection::{DeviceProvider, Direction};

    pub(crate) struct CpalDevices(cpal::Host);

    impl CpalDevices {
        pub(crate) fn new() -> Self {
            Self(cpal::default_host())
        }
    }

    impl DeviceProvider for CpalDevices {
        type Device = cpal::Device;

        fn default_device(&self, direction: Direction) -> Option<Self::Device> {
            match direction {
                Direction::Input => self.0.default_input_device(),
                Direction::Output => self.0.default_output_device(),
            }
        }

        fn devices(&self, direction: Direction) -> Result<Vec<Self::Device>, String> {
            match direction {
                Direction::Input => self
                    .0
                    .input_devices()
                    .map(|devices| devices.collect())
                    .map_err(|error| format!("could not list input audio devices: {error}")),
                Direction::Output => self
                    .0
                    .output_devices()
                    .map(|devices| devices.collect())
                    .map_err(|error| format!("could not list output audio devices: {error}")),
            }
        }

        fn name(&self, device: &Self::Device) -> Result<String, String> {
            // CPAL 0.18 has no DeviceTrait::name. Description Display adds metadata;
            // use its exact name field for listing, matching and the opened-device report.
            device
                .description()
                .map(|description| description.name().to_owned())
                .map_err(|error| format!("could not read audio device name: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::selection::{self, DeviceProvider, Direction};
    use super::*;
    use crate::protocol::media::MAX_MEDIA_TEXT_BYTES;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Device {
        id: u8,
        name: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Lookup {
        Default(Direction),
        Enumerate(Direction),
    }

    struct Provider {
        input: Vec<Device>,
        output: Vec<Device>,
        default_input: Option<Device>,
        default_output: Option<Device>,
        lookups: RefCell<Vec<Lookup>>,
        enumeration_error: bool,
        name_error_for: Option<u8>,
    }

    impl Provider {
        fn new() -> Self {
            let input = Device {
                id: 1,
                name: "Microphone".into(),
            };
            let output = Device {
                id: 2,
                name: "Speaker".into(),
            };
            Self {
                input: vec![input.clone()],
                output: vec![output.clone()],
                default_input: Some(input),
                default_output: Some(output),
                lookups: RefCell::new(Vec::new()),
                enumeration_error: false,
                name_error_for: None,
            }
        }
    }

    impl DeviceProvider for Provider {
        type Device = Device;

        fn default_device(&self, direction: Direction) -> Option<Device> {
            self.lookups.borrow_mut().push(Lookup::Default(direction));
            match direction {
                Direction::Input => self.default_input.clone(),
                Direction::Output => self.default_output.clone(),
            }
        }

        fn devices(&self, direction: Direction) -> Result<Vec<Device>, String> {
            self.lookups.borrow_mut().push(Lookup::Enumerate(direction));
            if self.enumeration_error {
                return Err("enumeration failed".into());
            }
            Ok(match direction {
                Direction::Input => self.input.clone(),
                Direction::Output => self.output.clone(),
            })
        }

        fn name(&self, device: &Device) -> Result<String, String> {
            if self.name_error_for == Some(device.id) {
                return Err("description failed".into());
            }
            Ok(device.name.clone())
        }
    }

    #[test]
    fn unset_uses_only_defaults_even_when_enumeration_would_fail() {
        let mut provider = Provider::new();
        provider.enumeration_error = true;
        let selected = selection::select(&provider, None, None).expect("defaults");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.output.id, 2);
        assert_eq!(selected.metadata.input.name, "Microphone");
        assert_eq!(selected.metadata.output.name, "Speaker");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.missing, None);
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Default(Direction::Input),
                Lookup::Default(Direction::Output),
            ]
        );
    }

    #[test]
    fn named_present_uses_named_handles_without_looking_up_defaults() {
        let mut provider = Provider::new();
        provider.input.push(Device {
            id: 3,
            name: "USB mic".into(),
        });
        provider.output.push(Device {
            id: 4,
            name: "USB speaker".into(),
        });
        provider.default_input = None;
        provider.default_output = None;
        let selected = selection::select(&provider, Some("USB mic"), Some("USB speaker"))
            .expect("named devices need no defaults");
        assert_eq!(selected.input.id, 3);
        assert_eq!(selected.output.id, 4);
        assert_eq!(selected.metadata.input.name, "USB mic");
        assert_eq!(selected.metadata.output.name, "USB speaker");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.missing, None);
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Enumerate(Direction::Input),
                Lookup::Enumerate(Direction::Output),
            ]
        );
    }

    #[test]
    fn literal_default_is_an_exact_named_device_not_a_selection_sentinel() {
        let mut provider = Provider::new();
        provider.input.push(Device {
            id: 3,
            name: "default".into(),
        });
        provider.output.push(Device {
            id: 4,
            name: "default".into(),
        });
        let selected = selection::select(&provider, Some("default"), Some("default"))
            .expect("literal named devices");
        assert_eq!(selected.input.id, 3);
        assert_eq!(selected.output.id, 4);
        assert_eq!(selected.metadata.input.name, "default");
        assert_eq!(selected.metadata.output.name, "default");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.missing, None);
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Enumerate(Direction::Input),
                Lookup::Enumerate(Direction::Output),
            ]
        );
        assert_eq!(
            selection::list_names(&provider).expect("exact list").input,
            vec!["Microphone".to_owned(), "default".to_owned()]
        );
    }

    #[test]
    fn absent_names_fall_back_separately_and_report_requested_names() {
        let provider = Provider::new();
        // These names exist, but only in the opposite direction.
        let selected = selection::select(&provider, Some("Speaker"), Some("Microphone"))
            .expect("directional fallback");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.output.id, 2);
        assert_eq!(selected.metadata.input.name, "Microphone");
        assert_eq!(selected.metadata.output.name, "Speaker");
        assert_eq!(selected.metadata.input.missing.as_deref(), Some("Speaker"));
        assert_eq!(
            selected.metadata.output.missing.as_deref(),
            Some("Microphone")
        );
    }

    #[test]
    fn requesting_one_direction_does_not_enumerate_the_other() {
        let provider = Provider::new();
        let selected = selection::select(&provider, None, Some("Speaker")).expect("selection");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.missing, None);
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Default(Direction::Input),
                Lookup::Enumerate(Direction::Output),
            ]
        );
    }

    #[test]
    fn blank_or_oversized_requests_are_rejected_before_any_lookup() {
        let provider = Provider::new();
        for name in [
            String::new(),
            " \t\n".into(),
            "é".repeat(MAX_MEDIA_TEXT_BYTES / 2 + 1),
        ] {
            for (input, output) in [(Some(name.as_str()), None), (None, Some(name.as_str()))] {
                assert!(selection::select(&provider, input, output).is_err());
            }
        }
        assert!(provider.lookups.borrow().is_empty());
    }

    #[test]
    fn device_names_are_exact_and_utf8_byte_bounded_not_truncated() {
        let mut provider = Provider::new();
        let name = "é".repeat(MAX_MEDIA_TEXT_BYTES / 2);
        provider.default_input.as_mut().expect("input").name = name.clone();
        let selected = selection::select(&provider, None, None).expect("512-byte name");
        assert_eq!(selected.metadata.input.name, name);
        provider
            .default_input
            .as_mut()
            .expect("input")
            .name
            .push('é');
        let selected = selection::select(&provider, None, None).expect("default still works");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.metadata.input.name, "default");
        assert_eq!(selected.metadata.input.missing, None);
        provider.input[0].name = "x".repeat(MAX_MEDIA_TEXT_BYTES + 1);
        assert!(selection::list_names(&provider).is_err());

        provider.input[0].name = " Microphone ".into();
        let selected =
            selection::select(&provider, Some(" Microphone "), None).expect("exact name");
        assert_eq!(selected.metadata.input.name, " Microphone ");
        assert_eq!(selected.metadata.input.missing, None);
    }

    #[test]
    fn default_description_failure_does_not_fail_or_enumerate_audio() {
        let mut provider = Provider::new();
        provider.name_error_for = Some(1);
        provider.enumeration_error = true;
        let selected = selection::select(&provider, None, None).expect("defaults still work");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.output.id, 2);
        assert_eq!(selected.metadata.input.name, "default");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.name, "Speaker");
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Default(Direction::Input),
                Lookup::Default(Direction::Output),
            ]
        );

        provider.name_error_for = Some(2);
        let selected =
            selection::select(&provider, None, None).expect("output default still works");
        assert_eq!(selected.metadata.output.name, "default");
        assert_eq!(selected.metadata.output.missing, None);
    }

    #[test]
    fn blank_default_description_uses_display_placeholder_without_failing_audio() {
        let mut provider = Provider::new();
        provider.default_input.as_mut().expect("input").name.clear();
        provider.default_output.as_mut().expect("output").name = " \t".into();
        let selected = selection::select(&provider, None, None).expect("default still works");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.output.id, 2);
        assert_eq!(selected.metadata.input.name, "default");
        assert_eq!(selected.metadata.output.name, "default");
        assert_eq!(selected.metadata.input.missing, None);
        assert_eq!(selected.metadata.output.missing, None);
        // Discovery lists OS device names, not the metadata placeholder.
        assert_eq!(
            selection::list_names(&provider).expect("exact list").input,
            vec!["Microphone".to_owned()]
        );
    }

    #[test]
    fn missing_named_device_keeps_fallback_even_when_default_name_cannot_be_reported() {
        let mut provider = Provider::new();
        provider.input.clear();
        provider.name_error_for = Some(1);
        provider.default_output.as_mut().expect("output").name =
            "x".repeat(MAX_MEDIA_TEXT_BYTES + 1);
        let selected = selection::select(&provider, Some("Missing mic"), Some("Missing speaker"))
            .expect("defaults still work");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.output.id, 2);
        assert_eq!(selected.metadata.input.name, "default");
        assert_eq!(
            selected.metadata.input.missing.as_deref(),
            Some("Missing mic")
        );
        assert_eq!(selected.metadata.output.name, "default");
        assert_eq!(
            selected.metadata.output.missing.as_deref(),
            Some("Missing speaker")
        );
    }

    #[test]
    fn unreadable_unrelated_devices_do_not_hide_later_named_matches_or_defaults() {
        let mut provider = Provider::new();
        provider.name_error_for = Some(1);
        provider.input.push(Device {
            id: 3,
            name: "USB mic".into(),
        });
        let selected = selection::select(&provider, Some("USB mic"), None)
            .expect("later match despite unreadable device");
        assert_eq!(selected.input.id, 3);
        assert_eq!(selected.metadata.input.name, "USB mic");
        assert_eq!(selected.metadata.input.missing, None);

        provider.name_error_for = Some(3);
        let selected = selection::select(&provider, Some("Missing mic"), None)
            .expect("default despite unreadable unrelated device");
        assert_eq!(selected.input.id, 1);
        assert_eq!(selected.metadata.input.name, "Microphone");
        assert_eq!(
            selected.metadata.input.missing.as_deref(),
            Some("Missing mic")
        );
    }

    #[test]
    fn enumeration_failure_is_not_reported_as_missing_and_no_default_is_an_error() {
        let mut provider = Provider::new();
        provider.enumeration_error = true;
        assert_eq!(
            selection::select(&provider, Some("USB mic"), None)
                .err()
                .as_deref(),
            Some("enumeration failed")
        );
        assert_eq!(
            *provider.lookups.borrow(),
            vec![Lookup::Enumerate(Direction::Input)]
        );
        provider.enumeration_error = false;
        provider.default_output = None;
        assert!(selection::select(&provider, None, Some("missing")).is_err());
    }

    #[test]
    fn listing_keeps_input_and_output_names_separate() {
        let provider = Provider::new();
        assert_eq!(
            selection::list_names(&provider).expect("names"),
            DeviceNames {
                input: vec!["Microphone".into()],
                output: vec!["Speaker".into()],
            }
        );
        assert_eq!(
            *provider.lookups.borrow(),
            vec![
                Lookup::Enumerate(Direction::Input),
                Lookup::Enumerate(Direction::Output),
            ]
        );
    }

    #[test]
    #[cfg(not(all(feature = "native-media", target_os = "macos")))]
    fn unsupported_builds_return_a_clear_listing_error() {
        let error = list_devices().expect_err("unsupported");
        assert!(error.contains("macOS"));
        assert!(error.contains("native-media"));
    }
}
