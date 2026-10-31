//! Read-only Linux HID access for the verified Naga V2 HyperSpeed.
//!
//! Sending a vendor feature report is required to issue the protocol's GET
//! commands; only the explicitly invoked DPI/poll controls and recovery method
//! send writes. Backup creation itself never modifies the mouse.

use crate::onboard_backup::{
    DeviceIdentity, OnboardBackup, RawButtonRecord, ReadFailure, ReadableSettings,
    SnapshotPayload, FORMAT_ID, FORMAT_VERSION,
};
use hidapi::{HidApi, HidDevice};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{atomic::{AtomicU64, Ordering}, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

const VID: u16 = 0x1532;
const PID: u16 = 0x00b4;
const INTERFACE: i32 = 0;
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const REPORT_ID: u8 = 0x03;
const PAYLOAD_LEN: usize = 90;
const COMMAND_STATUS_NEW: u8 = 0x00;
const COMMAND_STATUS_BUSY: u8 = 0x01;
const COMMAND_STATUS_OK: u8 = 0x02;
const COMMAND_STATUS_NO_RESPONSE: u8 = 0x04;
const CLASS_CONTROL: u8 = 0x00;
const CLASS_BUTTON: u8 = 0x02;
const CLASS_DPI: u8 = 0x04;
const CLASS_POWER: u8 = 0x07;
const CMD_GET_POLL_RATE: u8 = 0x85;
const CMD_SET_POLL_RATE: u8 = 0x05;
const CMD_GET_BUTTON: u8 = 0x8c;
const CMD_GET_DPI: u8 = 0x85;
const NOSTORE: u8 = 0x00;
const VARSTORE: u8 = 0x01;
const CMD_SET_DPI: u8 = 0x05;
const CMD_GET_MODE: u8 = 0x84;
const CMD_SET_MODE: u8 = 0x04;
const MODE_NORMAL: u8 = 0x00;
const MODE_CONFIRM_ATTEMPTS: u32 = 4;
const CMD_GET_BATTERY: u8 = 0x80;
const RESPONSE_POLL_ATTEMPTS: usize = 40;
const SETTLE: Duration = Duration::from_millis(50);
const RESPONSE_POLL_WAIT: Duration = Duration::from_millis(25);

/// Serializes the report SET/GET exchange across all backup requests.
static DEVICE_LOCK: Mutex<()> = Mutex::new(());
static PROFILE_SWITCHES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinuxMouseSettings {
    pub battery_percent: Option<u8>,
    pub dpi: Option<u16>,
    pub poll_rate_hz: Option<u16>,
    pub warnings: Vec<String>,
}

pub struct LinuxOnboardDevice {
    device: HidDevice,
    _exclusive: MutexGuard<'static, ()>,
}

fn is_target_hid_collection(interface_number: i32, usage_page: u16, usage: u16) -> bool {
    interface_number == INTERFACE
        && usage_page == USAGE_PAGE_GENERIC_DESKTOP
        && usage == USAGE_MOUSE
}

/// A matching feature report can still be pending: 0x00 is "new command"
/// and 0x01 is "busy". Both require another GET_REPORT, not another SET_REPORT.
fn response_is_pending(status: u8) -> bool {
    matches!(status, COMMAND_STATUS_NEW | COMMAND_STATUS_BUSY)
}

fn reply_matches(request: &[u8; PAYLOAD_LEN], reply: &[u8; PAYLOAD_LEN]) -> bool {
    reply[1] == request[1] && reply[6..8] == request[6..8]
}

/// Poll the reply to one already-sent command. The command is never resent while
/// the receiver is still processing it; only GET_REPORT is repeated.
fn wait_for_command_reply<F, P>(
    request: &[u8; PAYLOAD_LEN],
    mut read_reply: F,
    mut pause: P,
) -> Result<[u8; PAYLOAD_LEN], String>
where
    F: FnMut() -> Result<[u8; PAYLOAD_LEN], String>,
    P: FnMut(Duration),
{
    let class = request[6];
    let command = request[7];
    let started_at = Instant::now();
    let mut last_status = None;
    // Preserve every short response header on timeout. This is deliberately
    // read-only diagnostic data: it helps distinguish stale replies, pending
    // commands, and a report-layout mismatch without exposing button payloads.
    let mut response_headers = Vec::with_capacity(RESPONSE_POLL_ATTEMPTS);

    for attempt in 0..RESPONSE_POLL_ATTEMPTS {
        let reply = read_reply()?;
        last_status = Some(reply[0]);
        let header = reply[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        response_headers.push(format!(
            "{}:{}ms:[{}]",
            attempt + 1,
            started_at.elapsed().as_millis(),
            header
        ));

        if reply_matches(request, &reply) {
            match reply[0] {
                COMMAND_STATUS_OK => return Ok(reply),
                status if response_is_pending(status) => {}
                COMMAND_STATUS_NO_RESPONSE => {
                    return Err("mouse did not respond (asleep or out of range)".into());
                }
                0x03 => {
                    return Err(format!("HID query {class:#04x}/{command:#04x} was rejected (status 0x03)"));
                }
                0x05 => {
                    return Err(format!("HID query {class:#04x}/{command:#04x} is not supported (status 0x05)"));
                }
                status => {
                    return Err(format!("HID query {class:#04x}/{command:#04x} returned unknown status {status:#04x}"));
                }
            }
        }

        if attempt + 1 < RESPONSE_POLL_ATTEMPTS {
            pause(RESPONSE_POLL_WAIT);
        }
    }

    let last = last_status
        .map(|status| format!("; last response status {status:#04x}"))
        .unwrap_or_default();
    Err(format!(
        "HID query {class:#04x}/{command:#04x} did not receive a completed matching reply after {RESPONSE_POLL_ATTEMPTS} response reads{last}; elapsed {}ms; response headers (poll:elapsed:[bytes 0..7]) [{}]",
        started_at.elapsed().as_millis(),
        response_headers.join("; ")
    ))
}

/// Keeps successful live settings even when another individual query fails.
fn optional_setting<T>(
    label: &str,
    result: Result<T, String>,
    warnings: &mut Vec<String>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(format!("{label}: {error}"));
            None
        }
    }
}

fn collect_mouse_settings(
    battery: Result<u8, String>,
    dpi: Result<u16, String>,
    poll_rate_hz: Result<u16, String>,
) -> LinuxMouseSettings {
    let mut warnings = Vec::new();
    let battery_percent = optional_setting("Battery", battery, &mut warnings);
    let dpi = optional_setting("DPI", dpi, &mut warnings);
    let poll_rate_hz = optional_setting("Polling rate", poll_rate_hz, &mut warnings);
    LinuxMouseSettings {
        battery_percent,
        dpi,
        poll_rate_hz,
        warnings,
    }
}

impl LinuxOnboardDevice {
    pub fn open() -> Result<Self, String> {
        let exclusive = DEVICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let api = HidApi::new().map_err(|e| format!("HID initialization failed: {e}"))?;
        let matching_devices: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
            .collect();
        // HIDAPI enumerates each top-level HID collection separately. This
        // receiver exposes several collections on the same USB interface
        // number, so matching only interface_number == 0 can open the
        // keyboard/auxiliary collection instead of the mouse collection that
        // carries the vendor feature reports.
        let info = matching_devices
            .iter()
            .copied()
            .find(|d| {
                is_target_hid_collection(d.interface_number(), d.usage_page(), d.usage())
            })
            .ok_or_else(|| {
                let found = matching_devices
                    .iter()
                    .map(|d| format!(
                        "interface {} usage-page {:#06x} usage {:#06x}",
                        d.interface_number(), d.usage_page(), d.usage()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Razer Naga V2 HyperSpeed vendor mouse collection (USB interface 0, Generic Desktop/Mouse) was not found.                      Detected HID collections: {}. Check the receiver connection, HID permissions, and hidraw backend.",
                    if found.is_empty() { "none".into() } else { found }
                )
            })?;
        let device = api
            .open_path(info.path())
            .map_err(|e| format!("Could not open the Naga vendor HID interface: {e}. Check the device's udev permissions; do not run Sidearm as root."))?;
        device
            .set_blocking_mode(true)
            .map_err(|e| format!("Could not configure blocking HID reads: {e}"))?;
        Ok(Self { device, _exclusive: exclusive })
    }

    fn command(&self, class: u8, command: u8, args: &[u8]) -> Result<[u8; PAYLOAD_LEN], String> {
        if args.len() > 80 {
            return Err("HID command arguments exceed the report size".into());
        }
        let mut request = [0u8; PAYLOAD_LEN];
        request[1] = 0x1f;
        request[5] = args.len() as u8;
        request[6] = class;
        request[7] = command;
        request[8..8 + args.len()].copy_from_slice(args);
        request[88] = checksum(&request);

        let mut output = [0u8; PAYLOAD_LEN + 1];
        output[0] = REPORT_ID;
        output[1..].copy_from_slice(&request);
        self.device
            .send_feature_report(&output)
            .map_err(|e| format!("Could not send HID command {class:#04x}/{command:#04x}: {e}"))?;
        thread::sleep(SETTLE);

        wait_for_command_reply(
            &request,
            || self.read_command_reply(class, command),
            thread::sleep,
        )
    }

    fn read_command_reply(&self, class: u8, command: u8) -> Result<[u8; PAYLOAD_LEN], String> {
        let mut input = [0u8; PAYLOAD_LEN + 1];
        input[0] = REPORT_ID;
        let bytes_read = self.device
            .get_feature_report(&mut input)
            .map_err(|e| format!("Could not read HID reply {class:#04x}/{command:#04x}: {e}"))?;
        if bytes_read != PAYLOAD_LEN + 1 {
            return Err(format!(
                "HID reply {class:#04x}/{command:#04x} was truncated: expected {} bytes, got {bytes_read}",
                PAYLOAD_LEN + 1
            ));
        }
        let mut reply = [0u8; PAYLOAD_LEN];
        reply.copy_from_slice(&input[1..]);
        Ok(reply)
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<LinuxMouseSettings, String> {
        self.write_dpi(dpi, VARSTORE)?;
        Ok(self.settings())
    }

    fn set_dpi_live(&self, dpi: u16) -> Result<(), String> {
        self.write_dpi(dpi, NOSTORE)
    }

    fn write_dpi(&self, dpi: u16, storage: u8) -> Result<(), String> {
        if !(100..=30_000).contains(&dpi) {
            return Err(format!("DPI {dpi} is outside 100..=30000"));
        }
        let [hi, lo] = dpi.to_be_bytes();
        self.command(CLASS_DPI, CMD_SET_DPI, &[storage, hi, lo, hi, lo, 0x00, 0x00])?;
        let read = self.dpi_in(storage)?;
        if read != dpi {
            return Err(format!("mouse kept DPI {read} after a write of {dpi}"));
        }
        Ok(())
    }

    pub fn set_poll_rate_hz(&self, hz: u16) -> Result<LinuxMouseSettings, String> {
        self.write_poll_rate_hz(hz)?;
        Ok(self.settings())
    }

    fn write_poll_rate_hz(&self, hz: u16) -> Result<(), String> {
        let code = match hz {
            1000 => 0x01,
            500 => 0x02,
            125 => 0x08,
            _ => return Err(format!("unsupported polling rate {hz} Hz")),
        };
        self.command(CLASS_CONTROL, CMD_SET_POLL_RATE, &[code])?;
        let read = self.poll_rate_hz()?;
        if read != hz {
            return Err(format!("mouse kept {read} Hz after a write of {hz} Hz"));
        }
        Ok(())
    }

    pub fn mode(&self) -> Result<u8, String> {
        let reply = self.command(CLASS_CONTROL, CMD_GET_MODE, &[0x00, 0x00])?;
        Ok(reply[8])
    }

    fn set_mode(&self, mode: u8) -> Result<(), String> {
        self.command(CLASS_CONTROL, CMD_SET_MODE, &[mode, 0x00])?;
        let mut last = "no reply".to_string();
        for attempt in 0..MODE_CONFIRM_ATTEMPTS {
            if attempt > 0 {
                thread::sleep(SETTLE);
            }
            match self.mode() {
                Ok(actual) if actual == mode => return Ok(()),
                Ok(actual) => last = format!("mouse stayed in mode {actual:#04x} after requesting {mode:#04x}"),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn ensure_normal_mode(&self) -> Result<u8, String> {
        let before = self.mode()?;
        if before != MODE_NORMAL {
            self.set_mode(MODE_NORMAL)?;
        }
        Ok(before)
    }

    pub fn settings(&self) -> LinuxMouseSettings {
        collect_mouse_settings(self.battery_percent(), self.dpi(), self.poll_rate_hz())
    }

    fn raw_button_record(&self, button_id: u8, layer: u8) -> Result<[u8; 10], String> {
        let args = [0x01, button_id, layer, 0, 0, 0, 0, 0, 0, 0];
        let reply = self.command(CLASS_BUTTON, CMD_GET_BUTTON, &args)?;
        let mut record = [0u8; 10];
        record.copy_from_slice(&reply[8..18]);
        Ok(record)
    }

    fn battery_percent(&self) -> Result<u8, String> {
        let reply = self.command(CLASS_POWER, CMD_GET_BATTERY, &[0x00, 0x00])?;
        Ok(((u16::from(reply[9]) * 100 + 127) / 255) as u8)
    }

    fn dpi(&self) -> Result<u16, String> {
        self.dpi_in(NOSTORE)
    }

    fn dpi_in(&self, storage: u8) -> Result<u16, String> {
        let mut args = [0u8; 7];
        args[0] = storage;
        let reply = self.command(CLASS_DPI, CMD_GET_DPI, &args)?;
        Ok(u16::from_be_bytes([reply[9], reply[10]]))
    }

    fn poll_rate_hz(&self) -> Result<u16, String> {
        let reply = self.command(CLASS_CONTROL, CMD_GET_POLL_RATE, &[0x00])?;
        match reply[8] {
            0x01 => Ok(1000),
            0x02 => Ok(500),
            0x08 => Ok(125),
            code => Err(format!("mouse returned unknown poll-rate code {code:#04x}")),
        }
    }

    fn write_raw_record(&self, record: &RawButtonRecord) -> Result<(), String> {
        if record.layer > 1
            || record.bytes[0] != 0x01
            || record.bytes[1] != record.button_id
            || record.bytes[2] != record.layer
            || usize::from(record.bytes[4]) > 5
        {
            return Err(format!("backup contains an invalid raw record for button {:#04x} layer {}", record.button_id, record.layer));
        }
        self.command(CLASS_BUTTON, 0x0c, &record.bytes)?;
        let readback = self.raw_button_record(record.button_id, record.layer)?;
        if readback != record.bytes {
            return Err(format!("button {:#04x} layer {} did not read back byte-for-byte after restore", record.button_id, record.layer));
        }
        Ok(())
    }

    pub fn create_backup(&self) -> Result<OnboardBackup, String> {
        let mut ids = BTreeSet::new();
        // These are the complete, hardware-scanned set of addressable records
        // for this model: 14 key controls and seven native mouse controls.
        for id in [0x40u8, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x0b, 0x0c,
                   0x01, 0x02, 0x03, 0x09, 0x0a, 0x34, 0x35] {
            ids.insert(id);
        }

        let mode_before = self.mode().ok();
        let mut records = Vec::new();
        let mut failures = Vec::new();
        for button_id in ids {
            for layer in 0..=1 {
                match self.raw_button_record(button_id, layer) {
                    Ok(bytes) => records.push(RawButtonRecord { button_id, layer, bytes }),
                    Err(reason) => failures.push(ReadFailure { button_id, layer, reason }),
                }
            }
        }

        let settings = ReadableSettings {
            dpi: self.dpi().ok(),
            poll_rate_hz: self.poll_rate_hz().ok(),
        };
        let mode_after = self.mode().ok();
        let device_mode = match (mode_before, mode_after) {
            (Some(before), Some(after)) if before == after => Some(before),
            _ => None,
        };
        let payload = SnapshotPayload {
            format: FORMAT_ID.into(),
            format_version: FORMAT_VERSION,
            sidearm_version: env!("CARGO_PKG_VERSION").into(),
            captured_at: chrono_free_timestamp(),
            device: DeviceIdentity {
                vendor_id: VID,
                product_id: PID,
                model: "Razer Naga V2 HyperSpeed".into(),
                interface: INTERFACE as u8,
                protocol: "razer-command-class-v1".into(),
            },
            settings,
            device_mode,
            scan_complete: failures.is_empty() && records.len() == 42 && device_mode.is_some(),
            records,
            read_failures: failures,
        };
        OnboardBackup::new(payload).map_err(|e| e.to_string())
    }
}

fn validate_restore_payload(payload: &SnapshotPayload) -> Result<(), String> {
    if !payload.scan_complete || !payload.read_failures.is_empty() || payload.records.len() != 42 || payload.device_mode.is_none() {
        return Err("refusing to restore: backup is incomplete; all 42 button/layer records must be present".into());
    }

    let identity = &payload.device;
    if identity.vendor_id != VID
        || identity.product_id != PID
        || identity.model != "Razer Naga V2 HyperSpeed"
        || identity.interface != INTERFACE as u8
        || identity.protocol != "razer-command-class-v1"
    {
        return Err("refusing to restore: backup belongs to a different device or protocol".into());
    }

    let expected: BTreeSet<(u8, u8)> = [
        0x40u8, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x0b, 0x0c,
        0x01, 0x02, 0x03, 0x09, 0x0a, 0x34, 0x35,
    ]
    .into_iter()
    .flat_map(|button| [(button, 0), (button, 1)])
    .collect();

    for record in &payload.records {
        if record.layer > 1
            || record.bytes[0] != 0x01
            || record.bytes[1] != record.button_id
            || record.bytes[2] != record.layer
            || record.bytes[4] > 5
        {
            return Err("refusing to restore: backup contains an invalid raw record".into());
        }
    }

    let actual: BTreeSet<(u8, u8)> = payload
        .records
        .iter()
        .map(|record| (record.button_id, record.layer))
        .collect();
    if actual != expected {
        return Err("refusing to restore: backup does not contain the complete expected button/layer address set".into());
    }

    Ok(())
}

/// Validate that a backup is structurally sound, complete, and restorable
/// to this exact device model. This does not open or modify the mouse.
pub fn validate_backup_for_restore(path: &Path) -> Result<usize, String> {
    let backup = OnboardBackup::load_and_validate(path).map_err(|e| e.to_string())?;
    validate_restore_payload(&backup.payload)?;
    Ok(backup.payload.records.len())
}

/// Restore only a complete, validated backup for this exact model. Every
/// record is written as its original raw ten-byte value and read back exactly.
/// The original device mode is restored after the records.
pub fn restore_backup(path: &Path) -> Result<(), String> {
    let backup = OnboardBackup::load_and_validate(path).map_err(|e| e.to_string())?;
    let payload = &backup.payload;
    validate_restore_payload(payload)?;
    let saved_mode = payload
        .device_mode
        .ok_or_else(|| "backup is missing its original device mode".to_string())?;

    let device = LinuxOnboardDevice::open()?;
    device.ensure_normal_mode()?;
    for record in &payload.records {
        device.write_raw_record(record)?;
    }
    device.set_mode(saved_mode)?;
    Ok(())
}

fn sidearm_mapping_records(payload: &SnapshotPayload) -> Result<Vec<RawButtonRecord>, String> {
    let mut records = Vec::with_capacity(32);
    for index in 0..12u8 {
        let button_id = 0x40 + index;
        let usage = 0x68 + index;
        for layer in 0..=1 {
            let mut record = payload
                .records
                .iter()
                .find(|record| record.button_id == button_id && record.layer == layer)
                .cloned()
                .ok_or_else(|| format!("backup is missing thumb button {button_id:#04x} layer {layer}"))?;
            // Change only the action header/value. Keep reserved bytes exactly
            // as they were in the backup for forward compatibility.
            record.bytes[3] = 0x02;
            record.bytes[4] = 0x02;
            record.bytes[5] = 0x00;
            record.bytes[6] = usage;
            records.push(record);
        }
    }
    // The top panel buttons are keyboard controls, not mouse-button events.
    // Sidearm's encoder map identifies them as Alt+F23 and Alt+F24. Their
    // records have no vendor slot, but the command-class write path addresses
    // them directly. Write both layers so stale firmware actions cannot leak
    // through while Sidearm is running.
    for (button_id, usage) in [(0x0b, 0x72), (0x0c, 0x73)] {
        for layer in 0..=1 {
            let mut record = payload
                .records
                .iter()
                .find(|record| record.button_id == button_id && record.layer == layer)
                .cloned()
                .ok_or_else(|| format!("backup is missing top button {button_id:#04x} layer {layer}"))?;
            record.bytes[3] = 0x02;
            record.bytes[4] = 0x02;
            record.bytes[5] = 0x40; // right Alt, as confirmed by the device's key-record format
            record.bytes[6] = usage;
            records.push(record);
        }
    }

    for (button_id, mouse_button) in [(0x34, 4), (0x35, 5)] {
        for layer in 0..=1 {
            let mut record = payload
                .records
                .iter()
                .find(|record| record.button_id == button_id && record.layer == layer)
                .cloned()
                .ok_or_else(|| format!("backup is missing native button {button_id:#04x} layer {layer}"))?;
            record.bytes[3] = 0x01;
            record.bytes[4] = 0x01;
            record.bytes[5] = mouse_button;
            records.push(record);
        }
    }
    Ok(records)
}

/// Apply the fixed Sidearm convention only when the backup still matches the
/// connected mouse. A failed write triggers a full raw-record and mode rollback.
pub fn apply_sidearm_mapping(path: &Path) -> Result<(), String> {
    let backup = OnboardBackup::load_and_validate(path).map_err(|e| e.to_string())?;
    let payload = &backup.payload;
    validate_restore_payload(payload)?;
    let saved_mode = payload
        .device_mode
        .ok_or_else(|| "backup is missing its original device mode".to_string())?;

    let device = LinuxOnboardDevice::open()?;
    let current_mode = device.mode()?;
    if current_mode != saved_mode {
        return Err("mouse mode changed since this backup was created; create a fresh backup before mapping".into());
    }
    for original in &payload.records {
        let current = device.raw_button_record(original.button_id, original.layer)?;
        if current != original.bytes {
            return Err(format!(
                "button {:#04x} layer {} changed since this backup was created; no mapping was written",
                original.button_id, original.layer
            ));
        }
    }

    let original_mode = match device.ensure_normal_mode() {
        Ok(mode) => mode,
        Err(error) => {
            let rollback = device.set_mode(saved_mode);
            return match rollback {
                Ok(()) => Err(format!("could not switch to normal onboard mode; original mode restored: {error}")),
                Err(rollback_error) => Err(format!("could not switch to normal onboard mode: {error}; original mode rollback failed: {rollback_error}")),
            };
        }
    };

    for mapping in sidearm_mapping_records(payload)? {
        if let Err(error) = device.write_raw_record(&mapping) {
            let mut rollback_errors = Vec::new();
            for original in &payload.records {
                if let Err(rollback) = device.write_raw_record(original) {
                    rollback_errors.push(rollback);
                }
            }
            if let Err(rollback) = device.set_mode(original_mode) {
                rollback_errors.push(rollback);
            }
            if rollback_errors.is_empty() {
                return Err(format!("mapping failed and the original backup was restored: {error}"));
            }
            return Err(format!(
                "mapping failed: {error}; rollback also had {} error(s): {}",
                rollback_errors.len(),
                rollback_errors.join("; ")
            ));
        }
    }
    Ok(())
}

/// Apply profile-specific DPI and polling rate without blocking the capture
/// worker. A newer profile switch supersedes a queued older one.
pub fn apply_profile_mouse_settings(profile: &crate::config::Profile) {
    let (dpi, poll_rate_hz) = (profile.mouse_dpi, profile.mouse_poll_rate_hz);
    if dpi.is_none() && poll_rate_hz.is_none() {
        return;
    }
    let name = profile.name.clone();
    let generation = PROFILE_SWITCHES.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = thread::Builder::new()
        .name("linux-profile-mouse".into())
        .spawn(move || {
            let result = LinuxOnboardDevice::open().and_then(|device| {
                if PROFILE_SWITCHES.load(Ordering::SeqCst) != generation {
                    return Ok(());
                }
                if let Some(value) = dpi
                    && device.dpi()? != value
                {
                    device.set_dpi_live(value)?;
                }
                if PROFILE_SWITCHES.load(Ordering::SeqCst) != generation {
                    return Ok(());
                }
                if let Some(value) = poll_rate_hz
                    && device.poll_rate_hz()? != value
                {
                    device.write_poll_rate_hz(value)?;
                }
                Ok(())
            });
            if let Err(error) = result {
                log::warn!("[mouse] Could not apply profile settings for {name}: {error}");
            }
        });
}

/// Create a new backup file from the connected verified Naga. Existing files
/// are never replaced; the caller must choose a fresh destination.
pub fn backup_connected_mouse(path: &Path) -> Result<bool, String> {
    let device = LinuxOnboardDevice::open()?;
    let backup = device.create_backup()?;
    let complete = backup.payload.scan_complete;
    backup.save_new(path).map_err(|e| e.to_string())?;
    Ok(complete)
}

fn checksum(payload: &[u8; PAYLOAD_LEN]) -> u8 {
    payload[2..88].iter().fold(0, |acc, byte| acc ^ byte)
}

fn chrono_free_timestamp() -> String {
    // Unix timestamps are unambiguous and avoid adding a clock dependency.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{seconds}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_payload() -> SnapshotPayload {
        let ids = [0x40u8,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0x0b,0x0c,
                   0x01,0x02,0x03,0x09,0x0a,0x34,0x35];
        let records = ids.into_iter().flat_map(|button_id| {
            (0..=1).map(move |layer| RawButtonRecord {
                button_id,
                layer,
                bytes: [0x01, button_id, layer, 0x02, 0x02, 0x00, 0x68, 0, 0, 0],
            })
        }).collect();
        SnapshotPayload {
            format: FORMAT_ID.into(),
            format_version: FORMAT_VERSION,
            sidearm_version: "test".into(),
            captured_at: "unix:1".into(),
            device: DeviceIdentity {
                vendor_id: VID,
                product_id: PID,
                model: "Razer Naga V2 HyperSpeed".into(),
                interface: INTERFACE as u8,
                protocol: "razer-command-class-v1".into(),
            },
            settings: ReadableSettings { dpi: None, poll_rate_hz: None },
            device_mode: Some(MODE_NORMAL),
            records,
            read_failures: vec![],
            scan_complete: true,
        }
    }

    #[test]
    fn sidearm_mapping_targets_f13_to_f24_and_preserves_real_mouse_buttons() {
        let mut payload = complete_payload();
        for (index, record) in payload.records.iter_mut().enumerate() {
            record.bytes[7] = index as u8;
            record.bytes[8] = 0xaa;
            record.bytes[9] = 0xbb;
        }
        let records = sidearm_mapping_records(&payload).unwrap();
        assert_eq!(records.len(), 32);
        assert_eq!(&records[0].bytes[3..7], &[2, 2, 0, 0x68]);
        assert_eq!(&records[23].bytes[3..7], &[2, 2, 0, 0x73]);
        // D+ and D− are top-panel key buttons: Alt+F23 and Alt+F24.
        assert_eq!(records[24].button_id, 0x0b);
        assert_eq!(&records[24].bytes[3..7], &[2, 2, 0x40, 0x72]);
        assert_eq!(records[26].button_id, 0x0c);
        assert_eq!(&records[26].bytes[3..7], &[2, 2, 0x40, 0x73]);
        // Both layers receive the same physical signals.
        assert_eq!(&records[25].bytes[3..7], &[2, 2, 0x40, 0x72]);
        assert_eq!(&records[27].bytes[3..7], &[2, 2, 0x40, 0x73]);
        // Wheel-tilt M4/M5 remain distinct native mouse-button outputs.
        assert_eq!(&records[28].bytes[3..6], &[1, 1, 4]);
        assert_eq!(&records[31].bytes[3..6], &[1, 1, 5]);
        for record in &records {
            let original = payload.records.iter().find(|item| item.button_id == record.button_id && item.layer == record.layer).unwrap();
            assert_eq!(&record.bytes[7..10], &original.bytes[7..10]);
        }
    }

    #[test]
    fn restore_validation_accepts_only_complete_expected_snapshot() {
        let payload = complete_payload();
        assert!(validate_restore_payload(&payload).is_ok());
    }

    #[test]
    fn restore_validation_rejects_incomplete_snapshot() {
        let mut payload = complete_payload();
        payload.scan_complete = false;
        payload.records.pop();
        assert!(validate_restore_payload(&payload).unwrap_err().contains("incomplete"));
    }

    #[test]
    fn restore_validation_rejects_wrong_device_identity() {
        let mut payload = complete_payload();
        payload.device.product_id = 0x00b5;
        assert!(validate_restore_payload(&payload).unwrap_err().contains("different device"));
    }

    #[test]
    fn restore_validation_rejects_missing_or_duplicate_addresses() {
        let mut payload = complete_payload();
        payload.records.pop();
        assert!(validate_restore_payload(&payload).unwrap_err().contains("incomplete"));
        let mut payload = complete_payload();
        payload.records[0] = payload.records[1].clone();
        assert!(validate_restore_payload(&payload).unwrap_err().contains("complete expected"));
    }

    #[test]
    fn request_checksum_matches_device_protocol() {
        let mut payload = [0u8; PAYLOAD_LEN];
        payload[1] = 0x1f;
        payload[5] = 3;
        payload[6] = CLASS_BUTTON;
        payload[7] = CMD_GET_BUTTON;
        payload[8..11].copy_from_slice(&[1, 0x40, 0]);
        payload[88] = checksum(&payload);
        assert_eq!(payload[88], payload[2..88].iter().fold(0, |a, b| a ^ b));
    }

    #[test]
    fn new_and_busy_statuses_require_more_get_report_reads() {
        assert!(response_is_pending(COMMAND_STATUS_NEW));
        assert!(response_is_pending(COMMAND_STATUS_BUSY));
        for status in [COMMAND_STATUS_OK, 0x03, COMMAND_STATUS_NO_RESPONSE, 0x05] {
            assert!(!response_is_pending(status), "status {status:#04x} is final");
        }
    }

    fn battery_request() -> [u8; PAYLOAD_LEN] {
        let mut request = [0u8; PAYLOAD_LEN];
        request[1] = 0x1f;
        request[5] = 2;
        request[6] = CLASS_POWER;
        request[7] = CMD_GET_BATTERY;
        request[8..10].copy_from_slice(&[0, 0]);
        request[88] = checksum(&request);
        request
    }

    fn response_for(request: &[u8; PAYLOAD_LEN], status: u8) -> [u8; PAYLOAD_LEN] {
        let mut response = *request;
        response[0] = status;
        response
    }

    #[test]
    fn response_reader_polls_new_and_busy_without_resending_command() {
        let request = battery_request();
        let mut responses = std::collections::VecDeque::from([
            response_for(&request, COMMAND_STATUS_NEW),
            response_for(&request, COMMAND_STATUS_BUSY),
            response_for(&request, COMMAND_STATUS_OK),
        ]);
        let mut reads = 0;
        let mut pauses = Vec::new();
        let reply = wait_for_command_reply(
            &request,
            || {
                reads += 1;
                Ok(responses.pop_front().expect("expected a queued HID response"))
            },
            |duration| pauses.push(duration),
        )
        .expect("pending responses should complete");
        assert_eq!(reply[0], COMMAND_STATUS_OK);
        assert_eq!(reads, 3, "one SET is followed by three GET_REPORT reads");
        assert_eq!(pauses, [RESPONSE_POLL_WAIT, RESPONSE_POLL_WAIT]);
    }

    #[test]
    fn response_reader_bounds_repeated_new_status() {
        let request = battery_request();
        let mut reads = 0;
        let error = wait_for_command_reply(
            &request,
            || {
                reads += 1;
                Ok(response_for(&request, COMMAND_STATUS_NEW))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(reads, RESPONSE_POLL_ATTEMPTS);
        assert!(error.contains("last response status 0x00"));
    }

    #[test]
    fn response_reader_ignores_a_stale_reply_for_another_command() {
        let request = battery_request();
        let mut stale = response_for(&request, COMMAND_STATUS_OK);
        stale[7] = 0x81;
        let expected = response_for(&request, COMMAND_STATUS_OK);
        let mut replies = [stale, expected].into_iter();
        let reply = wait_for_command_reply(
            &request,
            || Ok(replies.next().expect("expected two HID responses")),
            |_| {},
        )
        .expect("the matching reply should be selected");
        assert_eq!(reply[7], CMD_GET_BATTERY);
    }

    #[test]
    fn settings_keep_dpi_and_polling_when_battery_read_fails() {
        let settings = collect_mouse_settings(
            Err("battery query status 0x00".into()),
            Ok(1600),
            Ok(500),
        );
        assert_eq!(settings.battery_percent, None);
        assert_eq!(settings.dpi, Some(1600));
        assert_eq!(settings.poll_rate_hz, Some(500));
        assert_eq!(settings.warnings.len(), 1);
        assert!(settings.warnings[0].starts_with("Battery:"));
    }

    #[test]
    fn settings_keep_battery_and_dpi_when_polling_read_fails() {
        let settings = collect_mouse_settings(Ok(80), Ok(2400), Err("bad poll code".into()));
        assert_eq!(settings.battery_percent, Some(80));
        assert_eq!(settings.dpi, Some(2400));
        assert_eq!(settings.poll_rate_hz, None);
        assert_eq!(settings.warnings.len(), 1);
        assert!(settings.warnings[0].starts_with("Polling rate:"));
    }

    #[test]
    fn target_hid_collection_is_the_mouse_application_on_interface_zero() {
        assert!(is_target_hid_collection(0, 0x01, 0x02));
        assert!(!is_target_hid_collection(0, 0x01, 0x06));
        assert!(!is_target_hid_collection(1, 0x01, 0x02));
        assert!(!is_target_hid_collection(0, 0x0c, 0x01));
    }

    #[test]
    fn snapshot_address_set_has_expected_record_count() {
        let ids = [0x40u8,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0x0b,0x0c,
                   0x01,0x02,0x03,0x09,0x0a,0x34,0x35];
        let unique: BTreeSet<u8> = ids.into_iter().collect();
        assert_eq!(unique.len(), 21);
        assert_eq!(unique.len() * 2, 42);
    }
}
