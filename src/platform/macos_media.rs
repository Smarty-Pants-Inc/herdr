//! CoreAudio device identity and default-device change policy for native media.
//!
//! CPAL's default-output monitor reports DeviceChanged after automatic rerouting,
//! or DeviceNotAvailable when no replacement default exists.
//! Its input stream instead pins a HAL device and exposes no default-input event,
//! so the peer invokes the identity check below off callbacks at most once a second.
//! We close rather than claim that a call transparently reopened different devices.

use cpal::traits::{DeviceTrait, HostTrait};

pub(crate) fn device_name(device: &cpal::Device) -> Result<String, String> {
    device
        .description()
        .map(|description| description.name().to_owned())
        .map_err(|error| format!("audio device name: {error}"))
}

pub(crate) fn default_input_check(
    input: &cpal::Device,
) -> Result<Box<dyn FnMut() -> bool>, String> {
    let original = input
        .id()
        .map_err(|error| format!("microphone identity: {error}"))?;
    let host = cpal::default_host();
    Ok(Box::new(move || {
        host.default_input_device()
            .and_then(|device| device.id().ok())
            .as_ref()
            != Some(&original)
    }))
}

pub(crate) fn stream_device_changed(error: &cpal::Error) -> bool {
    error.kind() == cpal::ErrorKind::DeviceChanged
}

/// Only for streams opened from the host's default output device.
pub(crate) fn default_output_stream_device_changed(error: &cpal::Error) -> bool {
    // CoreAudio's default-output monitor pauses on no default and reports DeviceNotAvailable.
    matches!(
        error.kind(),
        cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::DeviceNotAvailable
    )
}
