//! Linux HID access for the verified Naga V2 HyperSpeed.
//!
//! Backups and status reads are read-only. Onboard profile writes, DPI/poll
//! changes, and recovery writes are explicit operations and verify hardware
//! readback before reporting success.

use crate::onboard_backup::{
    DeviceIdentity, OnboardBackup, RawButtonRecord, ReadFailure, ReadableSettings,
    SnapshotPayload, FORMAT_ID, FORMAT_VERSION,
};
use hidapi::{HidApi, HidDevice};
use std::{
    collections::{BTreeMap, BTreeSet},
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
const REPORT_ID: u8 = 0x00;
const NAGA_BUTTON_IDS: [u8; 21] = [
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b,
    0x0b, 0x0c, 0x01, 0x02, 0x03, 0x09, 0x0a, 0x34, 0x35,
];
const PAYLOAD_LEN: usize = 90;
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
const CMD_GET_BUTTON_IDS: u8 = 0x84;
const CMD_GET_DPI: u8 = 0x85;
const NOSTORE: u8 = 0x00;
const VARSTORE: u8 = 0x01;
const CMD_SET_DPI: u8 = 0x05;
const CMD_GET_MODE: u8 = 0x84;
const CMD_SET_MODE: u8 = 0x04;
const MODE_NORMAL: u8 = 0x00;
const MODE_CONFIRM_ATTEMPTS: u32 = 4;
const CMD_GET_BATTERY: u8 = 0x80;
const RESPONSE_POLL_ATTEMPTS: usize = 20;
const SETTLE: Duration = Duration::from_millis(35);
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
    report_id: u8,
    _exclusive: MutexGuard<'static, ()>,
}

fn is_target_hid_collection(interface_number: i32, usage_page: u16, usage: u16) -> bool {
    interface_number == INTERFACE
        && usage_page == USAGE_PAGE_GENERIC_DESKTOP
        && usage == USAGE_MOUSE
}

/// The receiver reports status 0x01 while a command is still being processed.
/// Poll GET_REPORT for that same command; never resend SET_REPORT while busy.
fn response_is_pending(status: u8) -> bool {
    status == COMMAND_STATUS_BUSY
}

/// Reject malformed frames before considering whether they belong to this request.
fn validate_response_frame(reply: &[u8; PAYLOAD_LEN]) -> Result<(), String> {
    if reply[88] != checksum(reply) {
        return Err("mouse returned an invalid HID response checksum".into());
    }
    if reply[2] != 0 || reply[3] != 0 || reply[4] != 0 || reply[89] != 0 {
        return Err("mouse returned an invalid HID response header".into());
    }
    if reply[5] > 80 {
        return Err(format!("mouse returned an invalid HID response length {}", reply[5]));
    }
    if !matches!(reply[0], 0x01..=0x05) {
        return Err(format!("mouse returned an invalid HID response status {:#04x}", reply[0]));
    }
    Ok(())
}

fn reply_matches(request: &[u8; PAYLOAD_LEN], reply: &[u8; PAYLOAD_LEN]) -> bool {
    if reply[1] != request[1] || reply[6..8] != request[6..8] {
        return false;
    }
    // Button reads share a command header. Confirm the bank and button too.
    // Reply byte 10 is not checked: it is metadata, not a layer echo.
    !(reply[0] == COMMAND_STATUS_OK
        && request[6] == CLASS_BUTTON
        && request[7] == CMD_GET_BUTTON
        && (reply[8] != request[8] || reply[9] != request[9]))
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

        validate_response_frame(&reply)?;
        if reply_matches(request, &reply) {
            match reply[0] {
                COMMAND_STATUS_OK => {
                    if reply[5] != request[5] {
                        return Err(format!(
                            "HID query {class:#04x}/{command:#04x} returned {} bytes of arguments; expected {}",
                            reply[5], request[5]
                        ));
                    }
                    return Ok(reply);
                }
                status if response_is_pending(status) => {}
                COMMAND_STATUS_NO_RESPONSE => {
                    let target = if class == CLASS_BUTTON && command == CMD_GET_BUTTON && request[5] >= 3 {
                        format!(" (stored bank {}, button {:#04x}, layer {})", request[8], request[9], request[10])
                    } else if class == CLASS_BUTTON && command == 0x0c && request[5] >= 3 {
                        format!(" (stored bank {}, button {:#04x}, layer {})", request[8], request[9], request[10])
                    } else if class == CLASS_BUTTON && command == CMD_GET_BUTTON_IDS {
                        " (reading the onboard button inventory)".to_string()
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "Mouse timeout (status 0x04) for HID command {class:#04x}/{command:#04x}{target}. Move the mouse to wake it, then retry."
                    ));
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
        let api = HidApi::new().map_err(|e| format!("HID initialization failed: {e}"))?;
        let matching_devices: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID)
            .collect();
        // OpenNaga's independently verified transport selects the USB mouse
        // collection (Usage Page 1, Usage 2) on interface zero, with report ID 0.
        let candidates: Vec<_> = matching_devices
            .iter()
            .copied()
            .filter(|d| is_target_hid_collection(d.interface_number(), d.usage_page(), d.usage()))
            .collect();
        if candidates.is_empty() {
            let found = matching_devices
                .iter()
                .map(|d| format!(
                    "interface {} usage-page {:#06x} usage {:#04x}",
                    d.interface_number(), d.usage_page(), d.usage()
                ))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "Razer Naga V2 HyperSpeed HID interface {INTERFACE} was not found. Detected HID collections: {}. Check the receiver connection and HID permissions.",
                if found.is_empty() { "none".into() } else { found }
            ));
        }

        let mut failures = Vec::new();
        for info in candidates {
            let description = format!(
                "usage-page {:#06x} usage {:#04x}",
                info.usage_page(), info.usage()
            );
            let exclusive = DEVICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let device = match api.open_path(info.path()) {
                Ok(device) => device,
                Err(error) => {
                    failures.push(format!("{description}: open failed: {error}"));
                    continue;
                }
            };
            if let Err(error) = device.set_blocking_mode(true) {
                failures.push(format!("{description}: blocking mode failed: {error}"));
                continue;
            }
            let mut candidate = Self {
                device,
                report_id: REPORT_ID,
                _exclusive: exclusive,
            };

            candidate.report_id = REPORT_ID;
            match candidate.command(CLASS_CONTROL, CMD_GET_MODE, &[0x00, 0x00]) {
                Ok(_) => return Ok(candidate),
                Err(error) => failures.push(format!(
                    "{description} report-id {REPORT_ID:#04x}: {error}"
                )),
            }
        }

        Err(format!(
            "No Razer Naga V2 HyperSpeed HID collection on interface {INTERFACE} completed the read-only vendor command probe. {}",
            failures.join("; ")
        ))
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
        output[0] = self.report_id;
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
        input[0] = self.report_id;
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
        self.raw_button_record_from(0x01, button_id, layer)
    }

    fn raw_button_record_from(&self, profile: u8, button_id: u8, layer: u8) -> Result<[u8; 10], String> {
        if profile > 1 || layer > 1 {
            return Err("only the active/stored profile banks and two button layers are supported".into());
        }
        let args = [profile, button_id, layer, 0, 0, 0, 0, 0, 0, 0];
        let reply = self.command(CLASS_BUTTON, CMD_GET_BUTTON, &args)?;
        let mut record = [0u8; 10];
        record.copy_from_slice(&reply[8..18]);
        if record[0] != profile || record[1] != button_id {
            return Err(format!("mouse returned profile/button {:#04x}/{:#04x} while {profile:#04x}/{button_id:#04x} was requested", record[0], record[1]));
        }
        // Reply byte 2 is receiver metadata, not an echo of the requested layer.
        record[2] = 0;
        Ok(record)
    }

    fn verify_active_mirror(&self, records: &[RawButtonRecord]) -> Result<(), String> {
        for record in records {
            let actual = self.raw_button_record_from(0x00, record.button_id, record.layer)
                .map_err(|error| format!(
                    "Could not verify active-bank button {:#04x} layer {}: {error}",
                    record.button_id, record.layer
                ))?;
            let mut expected = record.bytes;
            expected[0] = 0x00;
            expected[2] = 0x00;
            if actual != expected {
                return Err(format!("button {:#04x} layer {} did not match the active onboard profile after writing", record.button_id, record.layer));
            }
        }
        Ok(())
    }

    fn button_ids(&self) -> Result<BTreeSet<u8>, String> {
        let reply = self.command(CLASS_BUTTON, CMD_GET_BUTTON_IDS, &[0; 22])?;
        decode_button_ids(&reply)
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
        if record.layer > 1 || record.bytes[0] != 0x01
            || record.bytes[1] != record.button_id || record.bytes[2] > 1
            || usize::from(record.bytes[4]) > 5
        {
            return Err(format!("backup contains an invalid raw record for button {:#04x} layer {}", record.button_id, record.layer));
        }
        // Persist normalized bytes, setting the requested layer only in the SET.
        let expected = normalize_binding_bytes(record.bytes);
        let mut wire = expected;
        wire[2] = record.layer;
        self.command(CLASS_BUTTON, 0x0c, &wire)
            .map_err(|error| format!(
                "Could not write onboard button {:#04x} layer {}: {error}",
                record.button_id, record.layer
            ))?;
        let readback = self.raw_button_record(record.button_id, record.layer)
            .map_err(|error| format!(
                "Could not read back onboard button {:#04x} layer {}: {error}",
                record.button_id, record.layer
            ))?;
        if readback != expected {
            return Err(format!("button {:#04x} layer {} did not read back byte-for-byte after restore", record.button_id, record.layer));
        }
        Ok(())
    }

    pub fn create_backup(&self) -> Result<OnboardBackup, String> {
        // Read the receiver's actual button table and refuse unknown layouts.
        let ids = self.button_ids()?;

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
            || record.bytes[2] > 1
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
    device.button_ids()?;
    device.ensure_normal_mode()?;
    let mut ordered: Vec<_> = payload.records.iter().collect();
    ordered.sort_by_key(|record| std::cmp::Reverse(record.layer));
    for record in ordered {
        device.write_raw_record(record)?;
    }
    device.verify_active_mirror(&payload.records)?;
    device.set_mode(saved_mode)?;
    Ok(())
}

/// Map the Default profile onto every addressable Naga record on both layers.
///
/// Sidearm's profile resolver uses configured action bindings, while the mouse
/// can store only a small set of native keyboard/mouse actions. We map those
/// actions directly and preserve the freshly-read onboard assignments for
/// controls with no Default-profile binding (left/right click and wheel up/down).
fn sidearm_mapping_records(
    payload: &SnapshotPayload,
    config: &crate::config::AppConfig,
) -> Result<Vec<RawButtonRecord>, String> {
    use crate::config::Layer;

    let profile = config.profiles.iter()
        .find(|profile| profile.id == "default")
        .ok_or_else(|| "config has no Default profile".to_string())?;
    if !profile.enabled {
        return Err("the Default profile is disabled; enable it before writing the mouse".into());
    }

    // All 21 hardware controls found in the complete Naga V2 scan.
    const BUTTONS: [(u8, &str); 21] = [
        (0x40, "thumb_01"), (0x41, "thumb_02"), (0x42, "thumb_03"),
        (0x43, "thumb_04"), (0x44, "thumb_05"), (0x45, "thumb_06"),
        (0x46, "thumb_07"), (0x47, "thumb_08"), (0x48, "thumb_09"),
        (0x49, "thumb_10"), (0x4a, "thumb_11"), (0x4b, "thumb_12"),
        (0x0b, "top_aux_01"), (0x0c, "top_aux_02"),
        (0x01, "mouse_left"), (0x02, "mouse_right"), (0x03, "wheel_click"),
        (0x09, "wheel_up"), (0x0a, "wheel_down"),
        (0x34, "mouse_4"), (0x35, "mouse_5"),
    ];

    let mut records = Vec::with_capacity(42);
    for (button_id, control_id) in BUTTONS {
        for layer in 0..=1 {
            let mut record = payload.records.iter()
                .find(|record| record.button_id == button_id && record.layer == layer)
                .cloned()
                .ok_or_else(|| format!("backup is missing {control_id} ({button_id:#04x}) layer {layer}"))?;
            record.bytes = normalize_binding_bytes(record.bytes);

            // Hypershift-specific assignments win when present. Otherwise the
            // Default standard action is copied to layer 1, preventing stale
            // Synapse/Hypershift records from resurfacing on the mouse.
            let target_layer = if layer == 0 { Layer::Standard } else { Layer::Hypershift };
            let binding = config.bindings.iter().find(|binding| {
                binding.profile_id == "default"
                    && binding.layer == target_layer
                    && binding.control_id.as_str() == control_id
            }).or_else(|| {
                if layer == 1 {
                    config.bindings.iter().find(|binding| {
                        binding.profile_id == "default"
                            && binding.layer == Layer::Standard
                            && binding.control_id.as_str() == control_id
                    })
                } else {
                    None
                }
            });

            if let Some(binding) = binding.filter(|binding| binding.enabled) {
                if binding.trigger_mode.is_some()
                    || binding.chord_partner.is_some()
                    || binding.throttle_ms.is_some()
                {
                    return Err(format!(
                        "Default binding for {control_id} uses a trigger, chord, or throttle that onboard memory cannot represent"
                    ));
                }
                let action = config.actions.iter()
                    .find(|action| action.id == binding.action_id)
                    .ok_or_else(|| format!("Default binding for {control_id} references missing action {}", binding.action_id))?;
                if !action.conditions.is_empty() {
                    return Err(format!("Default action {} is conditional and cannot be stored onboard", action.display_name));
                }
                let bytes = default_action_bytes(action.action_type, &action.payload)
                    .map_err(|error| format!("Default action {} for {control_id}: {error}", action.display_name))?;
                record.bytes[3..10].copy_from_slice(&bytes);
            }
            records.push(record);
        }
    }

    if records.len() != 42 {
        return Err(format!("expected 42 Default-profile records, built {}", records.len()));
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.layer));
    Ok(records)
}

/// Select only records whose normalized function bytes differ from the live onboard
/// snapshot. A changed Hypershift assignment also schedules that control's
/// standard-layer record: on this receiver the normal-layer SET clears the
/// layer-status flag left by a Hypershift SET, even when the standard mapping
/// itself is unchanged. This mirrors OpenNaga's write ordering.
fn changed_mapping_records(
    current: &[RawButtonRecord],
    targets: &[RawButtonRecord],
) -> Result<Vec<RawButtonRecord>, String> {
    let mut current_by_address = BTreeMap::new();
    for record in current {
        let address = (record.button_id, record.layer);
        if record.layer > 1 || current_by_address.insert(address, record).is_some() {
            return Err("live onboard snapshot contains duplicate or invalid button/layer records".into());
        }
    }

    let mut target_by_address = BTreeMap::new();
    for record in targets {
        let address = (record.button_id, record.layer);
        if record.layer > 1 || target_by_address.insert(address, record).is_some() {
            return Err("Default profile contains duplicate or invalid button/layer records".into());
        }
        if !current_by_address.contains_key(&address) {
            return Err(format!(
                "live onboard snapshot is missing button {:#04x} layer {}",
                record.button_id, record.layer
            ));
        }
    }

    let hypershift_changed: BTreeSet<u8> = targets.iter()
        .filter(|target| target.layer == 1
            && current_by_address.get(&(target.button_id, target.layer))
                .is_some_and(|before| before.bytes != target.bytes))
        .map(|target| target.button_id)
        .collect();

    let mut changed: Vec<RawButtonRecord> = targets.iter()
        .filter(|target| {
            target.layer == 1
                && current_by_address.get(&(target.button_id, target.layer))
                    .is_some_and(|before| before.bytes != target.bytes)
        })
        .cloned()
        .collect();

    changed.extend(targets.iter().filter(|target| {
        target.layer == 0
            && (hypershift_changed.contains(&target.button_id)
                || current_by_address.get(&(target.button_id, target.layer))
                    .is_some_and(|before| before.bytes != target.bytes))
    }).cloned());

    // SET layer 1 first; the corresponding layer 0 write follows to clear the
    // receiver's layer-status flag.
    changed.sort_by_key(|record| std::cmp::Reverse(record.layer));
    Ok(changed)
}

/// Seven action bytes stored after the profile/button/layer address in GET/SET_BUTTON.
fn default_action_bytes(
    action_type: crate::config::ActionType,
    payload: &crate::config::ActionPayload,
) -> Result<[u8; 7], String> {
    use crate::config::{ActionPayload, ActionType, MouseActionKind};

    match (action_type, payload) {
        (ActionType::Shortcut, ActionPayload::Shortcut(shortcut)) => {
            let key = crate::hotkeys::parse_primary_key(&shortcut.key)?;
            let usage = hid_usage_for_vk(key.code)
                .ok_or_else(|| format!("key {} has no onboard HID usage", key.display_name))?;
            let modifier = u8::from(shortcut.ctrl)
                | (u8::from(shortcut.shift) << 5)
                | (u8::from(shortcut.alt) << 6)
                | (u8::from(shortcut.win) << 3);
            Ok([0x02, 0x02, modifier, usage, 0, 0, 0])
        }
        (ActionType::MouseAction, ActionPayload::MouseAction(mouse)) => {
            if mouse.ctrl || mouse.shift || mouse.alt || mouse.win {
                return Err("modified mouse-button actions are not supported onboard".into());
            }
            let value = match mouse.action {
                MouseActionKind::LeftClick => 1,
                MouseActionKind::RightClick => 2,
                MouseActionKind::MiddleClick => 3,
                MouseActionKind::MouseBack | MouseActionKind::ScrollLeft => 4,
                MouseActionKind::MouseForward | MouseActionKind::ScrollRight => 5,
                _ => return Err(format!("mouse action {:?} cannot be stored onboard", mouse.action)),
            };
            Ok([0x01, 0x01, value, 0, 0, 0, 0])
        }
        _ => Err(format!("{} actions cannot be stored in onboard memory", action_type.as_str())),
    }
}

/// USB HID keyboard-page usage corresponding to the parsed Windows virtual key.
fn hid_usage_for_vk(vk: u16) -> Option<u8> {
    let vk = vk as u8;
    Some(match vk {
        0x41..=0x5a => 0x04 + (vk - 0x41),
        0x31..=0x39 => 0x1e + (vk - 0x31),
        0x30 => 0x27,
        0x70..=0x7b => 0x3a + (vk - 0x70),
        0x7c..=0x87 => 0x68 + (vk - 0x7c),
        0x0d => 0x28, 0x1b => 0x29, 0x08 => 0x2a, 0x09 => 0x2b,
        0x20 => 0x2c, 0xbd => 0x2d, 0xbb => 0x2e,
        0xdb => 0x2f, 0xdd => 0x30, 0xdc => 0x31,
        0xba => 0x33, 0xde => 0x34, 0xc0 => 0x35,
        0xbc => 0x36, 0xbe => 0x37, 0xbf => 0x38,
        0x14 => 0x39, 0x2c => 0x46, 0x91 => 0x47, 0x13 => 0x48,
        0x2d => 0x49, 0x24 => 0x4a, 0x21 => 0x4b, 0x2e => 0x4c,
        0x23 => 0x4d, 0x22 => 0x4e, 0x27 => 0x4f, 0x25 => 0x50,
        0x28 => 0x51, 0x26 => 0x52, 0x5d => 0x65,
        _ => return None,
    })
}

/// Build the optional four-control swap by exchanging action payloads.
fn sidearm_swap_records(payload: &SnapshotPayload) -> Result<Vec<RawButtonRecord>, String> {
    let mut records = Vec::with_capacity(8);
    for layer in 0..=1 {
        for (top_id, tilt_id) in [(0x0b, 0x34), (0x0c, 0x35)] {
            let top = payload.records.iter().find(|r| r.button_id == top_id && r.layer == layer)
                .ok_or_else(|| format!("backup is missing top button {top_id:#04x} layer {layer}"))?;
            let tilt = payload.records.iter().find(|r| r.button_id == tilt_id && r.layer == layer)
                .ok_or_else(|| format!("backup is missing wheel-tilt control {tilt_id:#04x} layer {layer}"))?;
            let mut new_top = top.clone();
            let mut new_tilt = tilt.clone();
            new_top.bytes = normalize_binding_bytes(new_top.bytes);
            new_tilt.bytes = normalize_binding_bytes(new_tilt.bytes);
            new_top.bytes[3..10].copy_from_slice(&tilt.bytes[3..10]);
            new_tilt.bytes[3..10].copy_from_slice(&top.bytes[3..10]);
            records.push(new_top);
            records.push(new_tilt);
        }
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.layer));
    Ok(records)
}

/// Write a bounded set from an unchanged snapshot. Each write is read back;
/// failures roll back only attempted targets. Success leaves normal onboard mode.
fn apply_mapping_records<F>(path: &Path, build: F) -> Result<(), String>
where
    F: FnOnce(&SnapshotPayload) -> Result<Vec<RawButtonRecord>, String>,
{
    // Keep the validated backup as the user's recovery file and receiver-model
    // check, but do not use its old button records as the mapping baseline.
    // Compare the requested profile with what is stored in the mouse right now.
    let backup = OnboardBackup::load_and_validate(path).map_err(|e| e.to_string())?;
    let payload = &backup.payload;
    validate_restore_payload(payload)?;
    let device = LinuxOnboardDevice::open()?;
    let button_ids = device.button_ids()
        .map_err(|error| format!("Could not read the onboard button inventory: {error}"))?;
    if device.mode().map_err(|error| format!("Could not read the mouse mode: {error}"))? != MODE_NORMAL {
        return Err("Turn off driver mode before saving to the mouse.".into());
    }

    let mut current_records = Vec::with_capacity(42);
    for button_id in button_ids {
        for layer in 0..=1 {
            current_records.push(RawButtonRecord {
                button_id,
                layer,
                bytes: device.raw_button_record(button_id, layer).map_err(|error| format!(
                    "Could not read the current stored assignment for button {button_id:#04x} layer {layer}: {error}"
                ))?,
            });
        }
    }

    // Build the target from the current Default profile. Unassigned controls
    // retain their current native assignment; explicit Default bindings are
    // authoritative and replace any differing onboard assignment.
    let mut current_payload = payload.clone();
    current_payload.records = current_records.clone();
    let mut targets = build(&current_payload)?;
    targets.sort_by_key(|record| std::cmp::Reverse(record.layer));
    let mappings = changed_mapping_records(&current_records, &targets)?;
    let differing_records = targets.iter().filter(|target| {
        current_records.iter().any(|current| {
            current.button_id == target.button_id
                && current.layer == target.layer
                && current.bytes != target.bytes
        })
    }).count();
    log::info!(
        "[onboard] Default-profile sync: {differing_records} of {} records differ; {} SET operations planned (including required normal-layer cleanup)",
        targets.len(),
        mappings.len()
    );

    let current_by_address: BTreeMap<(u8, u8), &RawButtonRecord> = current_records.iter()
        .map(|record| ((record.button_id, record.layer), record))
        .collect();
    let originals: Vec<&RawButtonRecord> = mappings.iter().map(|mapping| {
        current_by_address.get(&(mapping.button_id, mapping.layer)).copied()
            .ok_or_else(|| format!(
                "live onboard snapshot is missing original button {:#04x} layer {}",
                mapping.button_id, mapping.layer
            ))
    }).collect::<Result<_, _>>()?;

    let mut failure = None;
    let mut attempted_originals: Vec<RawButtonRecord> = Vec::with_capacity(mappings.len());
    for (mapping, original) in mappings.iter().zip(originals.iter()) {
        // A timed-out SET may still have reached flash, so include the current
        // record in rollback before sending the command.
        attempted_originals.push((**original).clone());
        if let Err(error) = device.write_raw_record(mapping) {
            failure = Some(error);
            break;
        }
    }
    if failure.is_none() {
        // Verify every requested target, not just the records that needed a
        // SET. This also confirms the unchanged parts of the active mirror.
        failure = device.verify_active_mirror(&targets).err();
    }

    if let Some(error) = failure {
        let mut rollback_errors = Vec::new();
        // Restore only records whose SET was attempted. If a Hypershift SET
        // may have reached flash, also write that same control's normal layer
        // to clear the receiver's layer-status flag before considering rollback
        // complete. This avoids touching unrelated, unattempted controls.
        let mut rollback_records = attempted_originals;
        let shifted_ids: BTreeSet<u8> = rollback_records.iter()
            .filter(|record| record.layer == 1)
            .map(|record| record.button_id)
            .collect();
        let mut rollback_addresses: BTreeSet<(u8, u8)> = rollback_records.iter()
            .map(|record| (record.button_id, record.layer))
            .collect();
        for button_id in shifted_ids {
            if rollback_addresses.insert((button_id, 0)) {
                if let Some(original) = current_by_address.get(&(button_id, 0)) {
                    rollback_records.push((**original).clone());
                } else {
                    rollback_errors.push(format!(
                        "missing original normal-layer record for button {button_id:#04x} during rollback"
                    ));
                }
            }
        }
        rollback_records.sort_by_key(|record| std::cmp::Reverse(record.layer));
        for record in &rollback_records {
            if let Err(e) = device.write_raw_record(record) { rollback_errors.push(e); }
        }
        if rollback_errors.is_empty()
            && let Err(e) = device.verify_active_mirror(&current_records)
        {
            rollback_errors.push(e);
        }
        if rollback_errors.is_empty() {
            return Err(format!("mapping failed and the original live onboard records were restored: {error}"));
        }
        return Err(format!("mapping failed: {error}; rollback also had {} error(s): {}",
            rollback_errors.len(), rollback_errors.join("; ")));
    }

    Ok(())
}

/// Apply the Default profile to all 21 Naga controls and both onboard layers.
pub fn apply_sidearm_mapping(path: &Path, config: &crate::config::AppConfig) -> Result<(), String> {
    apply_mapping_records(path, |payload| sidearm_mapping_records(payload, config))
}


/// Apply Sidearm's shipped Default profile from the recovery utility.
///
/// The standalone recovery binary intentionally uses the built-in seed config:
/// it has no app handle from which to resolve the user's config directory.
pub fn apply_default_profile_mapping(path: &Path) -> Result<(), String> {
    let config = crate::config::default_seed_config();
    apply_sidearm_mapping(path, &config)
}

/// Swap the top auxiliary and wheel-tilt actions on both layers. Writes exactly
/// eight records (four physical controls across two layers).
pub fn swap_auxiliary_and_tilt_directions(path: &Path) -> Result<(), String> {
    apply_mapping_records(path, sidearm_swap_records)
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

fn normalize_binding_bytes(mut bytes: [u8; 10]) -> [u8; 10] {
    bytes[2] = 0;
    bytes
}

fn decode_button_ids(reply: &[u8; PAYLOAD_LEN]) -> Result<BTreeSet<u8>, String> {
    if reply[5] != 22 {
        return Err(format!("mouse returned a {}-byte button list; expected 22", reply[5]));
    }
    let count = usize::from(reply[8]);
    if count == 0 || count >= 22 {
        return Err(format!("mouse returned an invalid button count {count}"));
    }
    let mut ids = BTreeSet::new();
    for button_id in &reply[9..9 + count] {
        if *button_id == 0 || !ids.insert(*button_id) {
            return Err("mouse returned a button list containing zero or duplicate IDs".into());
        }
    }
    let expected: BTreeSet<u8> = NAGA_BUTTON_IDS.into_iter().collect();
    if ids != expected {
        return Err(format!("mouse button layout differs from the supported 21-control Naga layout (received {} IDs)", ids.len()));
    }
    Ok(ids)
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
                bytes: [0x01, button_id, 0x00, 0x02, 0x02, 0x00, 0x68, 0, 0, 0],
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
    fn default_profile_mapping_writes_all_21_controls_on_both_layers() {
        let payload = complete_payload();
        let config = crate::config::default_seed_config();
        let records = sidearm_mapping_records(&payload, &config).unwrap();
        assert_eq!(records.len(), 42);
        assert!(records.windows(2).all(|pair| pair[0].layer >= pair[1].layer),
            "Hypershift records must be written before the normal layer");
        for button_id in [
            0x40u8, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b,
            0x0b, 0x0c, 0x01, 0x02, 0x03, 0x09, 0x0a, 0x34, 0x35,
        ] {
            for layer in 0..=1 {
                assert!(records.iter().any(|record| record.button_id == button_id && record.layer == layer));
            }
        }

        let record = |button_id, layer| records.iter()
            .find(|record| record.button_id == button_id && record.layer == layer)
            .unwrap();
        // Default thumb 1 is keyboard "1", with the same action in both layers.
        assert_eq!(&record(0x40, 0).bytes[3..7], &[0x02, 0x02, 0x00, 0x1e]);
        assert_eq!(&record(0x40, 1).bytes[3..7], &[0x02, 0x02, 0x00, 0x1e]);
        // Top Aux 1/2 are browser Back/Forward; wheel click stays middle click.
        assert_eq!(&record(0x0b, 0).bytes[3..6], &[0x01, 0x01, 4]);
        assert_eq!(&record(0x0c, 0).bytes[3..6], &[0x01, 0x01, 5]);
        assert_eq!(&record(0x03, 0).bytes[3..6], &[0x01, 0x01, 3]);
        // The default wheel-tilt actions remain mouse buttons 4/5.
        assert_eq!(&record(0x34, 0).bytes[3..6], &[0x01, 0x01, 4]);
        assert_eq!(&record(0x35, 0).bytes[3..6], &[0x01, 0x01, 5]);
        // Left/right click and wheel up/down have no Default-profile assignment;
        // their backup records are included in the full rewrite unchanged.
        for button_id in [0x01, 0x02, 0x09, 0x0a] {
            for layer in 0..=1 {
                assert_eq!(record(button_id, layer).bytes,
                    payload.records.iter().find(|r| r.button_id == button_id && r.layer == layer).unwrap().bytes);
            }
        }
    }

    #[test]
    fn default_profile_rejects_actions_the_mouse_cannot_store_before_writes() {
        let payload = complete_payload();
        let mut config = crate::config::default_seed_config();
        let binding = config.bindings.iter().find(|binding|
            binding.profile_id == "default" && binding.control_id.as_str() == "thumb_01"
        ).unwrap();
        let action_id = binding.action_id.clone();
        let action = config.actions.iter_mut().find(|action| action.id == action_id).unwrap();
        action.action_type = crate::config::ActionType::Disabled;
        action.payload = crate::config::ActionPayload::Disabled(crate::config::DisabledActionPayload {});
        let error = sidearm_mapping_records(&payload, &config).unwrap_err();
        assert!(error.contains("cannot be stored in onboard memory"));
    }

    #[test]
    fn mapping_diff_skips_unchanged_records_and_schedules_layer_cleanup() {
        let payload = complete_payload();
        let current = payload.records.clone();
        let mut targets = current.clone();

        // A standard-only change produces one SET.
        let standard = targets.iter_mut()
            .find(|record| record.button_id == 0x41 && record.layer == 0)
            .unwrap();
        standard.bytes[6] ^= 0x01;

        // A Hypershift change produces its SET plus a standard-layer SET for
        // the same button, even though that standard binding is unchanged.
        let hypershift = targets.iter_mut()
            .find(|record| record.button_id == 0x40 && record.layer == 1)
            .unwrap();
        hypershift.bytes[6] ^= 0x01;

        targets.sort_by_key(|record| std::cmp::Reverse(record.layer));
        let changed = changed_mapping_records(&current, &targets).unwrap();
        assert_eq!(changed.len(), 3);
        assert_eq!((changed[0].button_id, changed[0].layer), (0x40, 1));
        assert_eq!((changed[1].button_id, changed[1].layer), (0x40, 0));
        assert_eq!((changed[2].button_id, changed[2].layer), (0x41, 0));
    }

    #[test]
    fn mapping_diff_returns_empty_when_default_profile_matches_live_memory() {
        let payload = complete_payload();
        let changed = changed_mapping_records(&payload.records, &payload.records).unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn mapping_diff_rejects_duplicate_target_addresses() {
        let payload = complete_payload();
        let mut targets = vec![payload.records[0].clone()];
        targets.push(payload.records[0].clone());
        let error = changed_mapping_records(&payload.records, &targets).unwrap_err();
        assert!(error.contains("duplicate"));
    }

    #[test]
    fn swap_changes_only_auxiliary_and_wheel_tilt_actions_on_both_layers() {
        let mut payload = complete_payload();
        for record in &mut payload.records {
            record.bytes[3] = 0x10 + record.button_id;
            record.bytes[4] = 1;
            record.bytes[5] = record.layer;
            record.bytes[6] = 0x55;
            record.bytes[7] = 0x66;
            record.bytes[8] = 0x77;
            record.bytes[9] = 0x88;
        }
        let swapped = sidearm_swap_records(&payload).unwrap();
        assert_eq!(swapped.len(), 8);
        assert!(swapped.windows(2).all(|pair| pair[0].layer >= pair[1].layer));
        for layer in 0..=1 {
            for (top, tilt) in [(0x0b, 0x34), (0x0c, 0x35)] {
                let a = payload.records.iter().find(|r| r.button_id == top && r.layer == layer).unwrap();
                let b = payload.records.iter().find(|r| r.button_id == tilt && r.layer == layer).unwrap();
                let new_a = swapped.iter().find(|r| r.button_id == top && r.layer == layer).unwrap();
                let new_b = swapped.iter().find(|r| r.button_id == tilt && r.layer == layer).unwrap();
                assert_eq!(&new_a.bytes[3..10], &b.bytes[3..10]);
                assert_eq!(&new_b.bytes[3..10], &a.bytes[3..10]);
                assert_eq!(&new_a.bytes[..3], &a.bytes[..3]);
                assert_eq!(&new_b.bytes[..3], &b.bytes[..3]);
            }
        }
        assert!(payload.records.iter().filter(|r| ![0x0b, 0x0c, 0x34, 0x35].contains(&r.button_id))
            .all(|r| !swapped.iter().any(|w| w.button_id == r.button_id && w.layer == r.layer)));
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
    fn only_busy_status_requires_an_additional_get_report() {
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
    fn response_reader_polls_busy_without_resending_command() {
        let request = battery_request();
        let mut responses = std::collections::VecDeque::from([
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
        assert_eq!(reads, 2, "one SET is followed by GET_REPORT polling");
        assert_eq!(pauses, [RESPONSE_POLL_WAIT]);
    }

    #[test]
    fn response_reader_identifies_timed_out_button_and_layer() {
        let mut request = [0u8; PAYLOAD_LEN];
        request[1] = 0x1f;
        request[5] = 10;
        request[6] = CLASS_BUTTON;
        request[7] = CMD_GET_BUTTON;
        request[8..18].copy_from_slice(&[0x01, 0x40, 0x01, 0, 0, 0, 0, 0, 0, 0]);
        request[88] = checksum(&request);
        let error = wait_for_command_reply(
            &request,
            || Ok(response_for(&request, COMMAND_STATUS_NO_RESPONSE)),
            |_| {},
        ).unwrap_err();
        assert!(error.contains("HID command 0x02/0x8c"));
        assert!(error.contains("stored bank 1, button 0x40, layer 1"));
        assert!(error.contains("status 0x04"));
    }

    #[test]
    fn response_reader_rejects_unknown_zero_status() {
        let request = battery_request();
        let mut reads = 0;
        let error = wait_for_command_reply(
            &request,
            || {
                reads += 1;
                Ok(response_for(&request, 0x00))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(reads, 1);
        assert!(error.contains("invalid HID response status 0x00"));
    }

    #[test]
    fn response_reader_ignores_a_stale_reply_for_another_command() {
        let request = battery_request();
        let mut stale = response_for(&request, COMMAND_STATUS_OK);
        stale[7] = 0x81;
        stale[88] = checksum(&stale);
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
    fn target_hid_collection_matches_open_naga_usage_and_interface() {
        assert!(is_target_hid_collection(0, 0x01, 0x02));
        assert!(!is_target_hid_collection(1, 0x01, 0x02));
        assert!(!is_target_hid_collection(0, 0xff00, 0x02));
        assert!(!is_target_hid_collection(0, 0x01, 0x01));
    }

    #[test]
    fn snapshot_address_set_has_expected_record_count() {
        let ids = [0x40u8,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0x0b,0x0c,
                   0x01,0x02,0x03,0x09,0x0a,0x34,0x35];
        let unique: BTreeSet<u8> = ids.into_iter().collect();
        assert_eq!(unique.len(), 21);
        assert_eq!(unique.len() * 2, 42);
    }

    #[test]
    fn button_id_decoder_requires_the_verified_21_control_layout() {
        let mut reply = [0u8; PAYLOAD_LEN];
        reply[5] = 22;
        reply[8] = 21;
        reply[9..30].copy_from_slice(&NAGA_BUTTON_IDS);
        assert_eq!(decode_button_ids(&reply).unwrap().len(), 21);
        reply[29] = 0x7f;
        assert!(decode_button_ids(&reply).is_err());
    }

    #[test]
    fn layer_address_is_separate_from_normalized_binding_bytes() {
        let stored = [0x01, 0x40, 0x00, 0x02, 0x02, 0, 0x1e, 0, 0, 0];
        let expected = normalize_binding_bytes(stored);
        let mut wire = expected;
        wire[2] = 1;
        assert_eq!(expected[2], 0);
        assert_eq!(wire[2], 1);
    }

    #[test]
    fn restore_validation_accepts_legacy_layer_reply_metadata() {
        let mut payload = complete_payload();
        for record in &mut payload.records {
            if record.layer == 1 { record.bytes[2] = 1; }
        }
        assert!(validate_restore_payload(&payload).is_ok());
    }

    #[test]
    fn reply_match_rejects_stale_button_id_and_profile() {
        let mut request = [0u8; PAYLOAD_LEN];
        request[1] = 0x1f;
        request[5] = 10;
        request[6] = CLASS_BUTTON;
        request[7] = CMD_GET_BUTTON;
        request[8..18].copy_from_slice(&[1, 0x40, 1, 0, 0, 0, 0, 0, 0, 0]);
        let mut reply = response_for(&request, COMMAND_STATUS_OK);
        assert!(reply_matches(&request, &reply));
        reply[9] = 0x41;
        assert!(!reply_matches(&request, &reply));
        reply[9] = 0x40;
        reply[8] = 0;
        assert!(!reply_matches(&request, &reply));
    }
}
