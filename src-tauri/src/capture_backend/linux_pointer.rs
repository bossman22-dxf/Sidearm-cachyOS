//! Linux pointer capture for the Razer Naga V2 HyperSpeed.
//!
//! The Naga reports wheel tilt as native Mouse 4 / Mouse 5 button events,
//! not REL_HWHEEL. This module grabs only the matching pointer collection,
//! mirrors its normal key/relative events through uinput, and routes the two
//! tilt buttons through Sidearm's existing encoded-key resolver. All unrelated
//! pointer input is passed through unchanged.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc,
    },
    thread,
    time::Duration,
};

use evdev::{
    AttributeSet, InputEvent, KeyCode, MiscCode, PropType, RelativeAxisCode, SwitchCode,
    uinput::VirtualDevice,
};

use crate::{
    config::{AppConfig, Layer},
    runtime,
};

use super::{CAPTURE_BACKEND_NAME, EncodedKeyEvent};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_REPORT: u16 = 0;
const KEY_PRESS: i32 = 1;
const KEY_RELEASE: i32 = 0;
const KEY_REPEAT: i32 = 2;

fn pointer_device_path() -> Option<PathBuf> {
    evdev::enumerate().find_map(|(path, device)| {
        let name = device.name().unwrap_or_default().to_ascii_lowercase();
        if !name.contains("razer") || !name.contains("naga v2 hyperspeed") {
            return None;
        }

        let has_side_buttons = device.supported_keys().is_some_and(|keys| {
            keys.contains(KeyCode::BTN_SIDE) && keys.contains(KeyCode::BTN_EXTRA)
        });
        let has_pointer_axes = device.supported_relative_axes().is_some_and(|axes| {
            axes.contains(RelativeAxisCode::REL_X)
                && axes.contains(RelativeAxisCode::REL_Y)
        });

        // This must be the pointer collection, not the separate keyboard HID
        // collection used by F13-F24 capture.
        (has_side_buttons && has_pointer_axes
            && device.supported_absolute_axes().is_none())
            .then_some(path)
    })
}

fn create_pointer_mirror(source: &evdev::Device) -> Result<VirtualDevice, String> {
    let keys: AttributeSet<KeyCode> = source
        .supported_keys()
        .map(|supported| supported.iter().collect())
        .unwrap_or_default();

    let mut axes: AttributeSet<RelativeAxisCode> = source
        .supported_relative_axes()
        .map(|supported| supported.iter().collect())
        .unwrap_or_default();
    // The tilt translation uses horizontal-wheel events even when the
    // physical pointer collection exposes only BTN_SIDE / BTN_EXTRA.
    axes.insert(RelativeAxisCode::REL_HWHEEL);

    let properties: AttributeSet<PropType> = source.properties().iter().collect();
    let mut builder = VirtualDevice::builder()
        .map_err(|e| format!("Could not create Naga pointer mirror: {e}"))?
        .name("Sidearm Naga Pointer")
        .input_id(source.input_id())
        .with_keys(&keys)
        .map_err(|e| format!("Could not copy Naga pointer buttons: {e}"))?
        .with_relative_axes(&axes)
        .map_err(|e| format!("Could not copy Naga pointer axes: {e}"))?
        .with_properties(&properties)
        .map_err(|e| format!("Could not copy Naga pointer properties: {e}"))?;

    if let Some(source_switches) = source.supported_switches() {
        let switches: AttributeSet<SwitchCode> = source_switches.iter().collect();
        builder = builder
            .with_switches(&switches)
            .map_err(|e| format!("Could not copy Naga pointer switches: {e}"))?;
    }

    if let Some(source_misc) = source.misc_properties() {
        let misc: AttributeSet<MiscCode> = source_misc.iter().collect();
        builder = builder
            .with_msc(&misc)
            .map_err(|e| format!("Could not copy Naga pointer scan codes: {e}"))?;
    }

    builder
        .build()
        .map_err(|e| format!("Could not create Naga pointer mirror: {e}"))
}

fn side_button_hotkey(code: u16) -> Option<(&'static str, &'static str)> {
    match code {
        code if code == KeyCode::BTN_SIDE.0 => Some(("mouse_4", "Alt+F13")),
        code if code == KeyCode::BTN_EXTRA.0 => Some(("mouse_5", "Alt+F14")),
        _ => None,
    }
}

fn is_bound_in_standard_layer(config: &AppConfig, control_id: &str) -> bool {
    config.bindings.iter().any(|binding| {
        binding.enabled
            && binding.layer == Layer::Standard
            && binding.control_id.as_str() == control_id
    })
}

fn send_button_event(
    event_tx: &SyncSender<EncodedKeyEvent>,
    encoded_key: &str,
    value: i32,
) -> bool {
    if value != KEY_PRESS && value != KEY_RELEASE && value != KEY_REPEAT {
        return false;
    }

    event_tx
        .send(EncodedKeyEvent {
            encoded_key: encoded_key.to_owned(),
            backend: CAPTURE_BACKEND_NAME.to_owned(),
            received_at: runtime::timestamp_millis(),
            is_repeat: value == KEY_REPEAT,
            is_key_up: value == KEY_RELEASE,
            is_hypershift: false,
        })
        .is_ok()
}

fn emit_pointer_batch(
    mirror: &mut VirtualDevice,
    batch: &mut Vec<InputEvent>,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    mirror
        .emit(batch)
        .map_err(|e| format!("Could not forward Naga pointer events: {e}"))?;
    batch.clear();
    Ok(())
}

pub(super) fn run_capture_loop(
    stop_flag: Arc<AtomicBool>,
    event_tx: SyncSender<EncodedKeyEvent>,
    config: AppConfig,
) {
    'hotplug: loop {
        if stop_flag.load(Ordering::SeqCst) {
            return;
        }

        let Some(path) = pointer_device_path() else {
            thread::sleep(Duration::from_secs(2));
            continue;
        };

        let mut source = match evdev::Device::open(&path) {
            Ok(device) => device,
            Err(error) => {
                log::warn!("[capture] Could not open Naga pointer collection {}: {error}", path.display());
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        if let Err(error) = source.set_nonblocking(true) {
            log::warn!("[capture] Could not make Naga pointer collection non-blocking: {error}");
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        let mut mirror = match create_pointer_mirror(&source) {
            Ok(device) => device,
            Err(error) => {
                // Never grab the physical mouse unless its full normal pointer
                // input can be forwarded safely.
                log::warn!("[capture] Wheel-tilt remapping unavailable: {error}");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // Let the desktop enumerate the mirror before hiding the physical node.
        thread::sleep(Duration::from_millis(200));
        if let Err(error) = source.grab() {
            log::warn!("[capture] Could not grab Naga pointer collection; leaving mouse input untouched: {error}");
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        log::info!(
            "[capture] Naga pointer collection captured and mirrored from {}",
            path.display()
        );

        let mut device_lost = false;
        let mut batch = Vec::with_capacity(32);

        while !stop_flag.load(Ordering::SeqCst) {
            let events: Vec<InputEvent> = match source.fetch_events() {
                Ok(events) => events.collect(),
                Err(error) => {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    if error.raw_os_error() == Some(19) {
                        log::warn!("[capture] Naga pointer disconnected; waiting for reconnect.");
                        device_lost = true;
                        break;
                    }
                    log::warn!("[capture] Naga pointer read failed: {error}");
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            for event in events {
                let event_type = event.event_type().0;

                if event_type == EV_SYN {
                    if event.code() == SYN_REPORT
                        && let Err(error) = emit_pointer_batch(&mut mirror, &mut batch)
                    {
                        log::error!("[capture] {error}");
                    }
                    continue;
                }

                if event_type == EV_KEY
                    && let Some((control_id, encoded_key)) = side_button_hotkey(event.code())
                    && is_bound_in_standard_layer(&config, control_id)
                    && send_button_event(&event_tx, encoded_key, event.value())
                {
                    // The physical Back/Forward button must not escape as well
                    // as the configured horizontal-scroll action.
                    continue;
                }

                batch.push(event);
            }

            if let Err(error) = emit_pointer_batch(&mut mirror, &mut batch) {
                log::error!("[capture] {error}");
                break;
            }
        }

        let _ = source.ungrab();
        drop(mirror);

        if stop_flag.load(Ordering::SeqCst) {
            return;
        }
        if !device_lost {
            // A read/forwarding error: avoid a tight reopen loop.
            thread::sleep(Duration::from_secs(2));
        }
        continue 'hotplug;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_tilt_buttons_resolve_to_horizontal_scroll_encoder_signals() {
        assert_eq!(side_button_hotkey(KeyCode::BTN_SIDE.0), Some(("mouse_4", "Alt+F13")));
        assert_eq!(side_button_hotkey(KeyCode::BTN_EXTRA.0), Some(("mouse_5", "Alt+F14")));
        assert_eq!(side_button_hotkey(KeyCode::BTN_LEFT.0), None);
    }

    #[test]
    fn only_enabled_standard_bindings_are_intercepted() {
        let mut config = crate::config::default_seed_config();
        assert!(is_bound_in_standard_layer(&config, "mouse_4"));
        assert!(is_bound_in_standard_layer(&config, "mouse_5"));

        config.bindings.retain(|binding| {
            binding.control_id.as_str() != "mouse_4" && binding.control_id.as_str() != "mouse_5"
        });
        assert!(!is_bound_in_standard_layer(&config, "mouse_4"));
        assert!(!is_bound_in_standard_layer(&config, "mouse_5"));
    }
}
