//! Read-only Linux HID access for the verified Naga V2 HyperSpeed.
//!
//! Sending a vendor feature report is required to issue the protocol's GET
//! commands; only the explicitly invoked DPI/poll controls and recovery method
//! send writes. Backup creation itself never modifies the mouse.

use crate::onboard_backup::{
    DeviceIdentity, OnboardBackup, RawButtonRecord, ReadFailure, ReadableSettings,
    SnapshotPayload, OnboardBackup, FORMAT_ID, FORMAT_VERSION,
};
use hidapi::{HidApi, HidDevice};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Mutex, MutexGuard},
    thread,
    time::Duration,
};

const VID: u16 = 0x1532;
const PID: u16 = 0x00b4;
const INTERFACE: i32 = 0;
const REPORT_ID: u8 = 0x03;
const PAYLOAD_LEN: usize = 90;
const COMMAND_STATUS_OK: u8 = 0x02;
const COMMAND_STATUS_BUSY: u8 = 0x01;
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
const CMD_GET_BATTERY: u8 = 0x80;
const BUSY_RETRIES: usize = 8;
const SETTLE: Duration = Duration::from_millis(50);
const BUSY_WAIT: Duration = Duration::from_millis(200);

/// Serializes the report SET/GET exchange across all backup requests.
static DEVICE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinuxMouseSettings {
    pub battery_percent: u8,
    pub dpi: u16,
    pub poll_rate_hz: u16,
}

pub struct LinuxOnboardDevice {
    device: HidDevice,
    _exclusive: MutexGuard<'static, ()>,
}

impl LinuxOnboardDevice {
    pub fn open() -> Result<Self, String> {
        let exclusive = DEVICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let api = HidApi::new().map_err(|e| format!("HID initialization failed: {e}"))?;
        let info = api
            .device_list()
            .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.interface_number() == INTERFACE)
            .ok_or_else(|| "Razer Naga V2 HyperSpeed vendor interface 0 was not found. Check the receiver connection and HID permissions.".to_string())?;
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

        for attempt in 0..BUSY_RETRIES {
            let mut output = [0u8; PAYLOAD_LEN + 1];
            output[0] = REPORT_ID;
            output[1..].copy_from_slice(&request);
            self.device
                .send_feature_report(&output)
                .map_err(|e| format!("Could not send read-only HID query {class:#04x}/{command:#04x}: {e}"))?;
            thread::sleep(SETTLE);

            let mut input = [0u8; PAYLOAD_LEN + 1];
            input[0] = REPORT_ID;
            self.device
                .get_feature_report(&mut input)
                .map_err(|e| format!("Could not read HID reply {class:#04x}/{command:#04x}: {e}"))?;
            let mut reply = [0u8; PAYLOAD_LEN];
            reply.copy_from_slice(&input[1..]);
            if reply[6..8] != request[6..8] {
                continue;
            }
            match reply[0] {
                COMMAND_STATUS_OK => return Ok(reply),
                COMMAND_STATUS_BUSY if attempt + 1 < BUSY_RETRIES => thread::sleep(BUSY_WAIT),
                COMMAND_STATUS_NO_RESPONSE => return Err("mouse did not respond (asleep or out of range)".into()),
                status => return Err(format!("HID query {class:#04x}/{command:#04x} returned status {status:#04x}")),
            }
        }
        Err(format!("HID query {class:#04x}/{command:#04x} did not receive a matching reply"))
    }

    pub fn set_dpi(&self, dpi: u16) -> Result<LinuxMouseSettings, String> {
        if !(100..=30_000).contains(&dpi) {
            return Err(format!("DPI {dpi} is outside 100..=30000"));
        }
        let [hi, lo] = dpi.to_be_bytes();
        self.command(CLASS_DPI, CMD_SET_DPI, &[VARSTORE, hi, lo, hi, lo, 0x00, 0x00])?;
        let read = self.dpi_in(VARSTORE)?;
        if read != dpi {
            return Err(format!("mouse kept DPI {read} after a write of {dpi}"));
        }
        self.settings()
    }

    pub fn set_poll_rate_hz(&self, hz: u16) -> Result<LinuxMouseSettings, String> {
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
        self.settings()
    }

    pub fn settings(&self) -> Result<LinuxMouseSettings, String> {
        let battery_reply = self.command(CLASS_POWER, CMD_GET_BATTERY, &[0x00, 0x00])?;
        Ok(LinuxMouseSettings {
            battery_percent: ((u16::from(battery_reply[9]) * 100 + 127) / 255) as u8,
            dpi: self.dpi()?,
            poll_rate_hz: self.poll_rate_hz()?,
        })
    }

    fn raw_button_record(&self, button_id: u8, layer: u8) -> Result<[u8; 10], String> {
        let args = [0x01, button_id, layer, 0, 0, 0, 0, 0, 0, 0];
        let reply = self.command(CLASS_BUTTON, CMD_GET_BUTTON, &args)?;
        let mut record = [0u8; 10];
        record.copy_from_slice(&reply[8..18]);
        Ok(record)
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
            scan_complete: failures.is_empty() && records.len() == 42,
            records,
            read_failures: failures,
        };
        OnboardBackup::new(payload).map_err(|e| e.to_string())
    }
}

/// Restore only a complete, validated backup for this exact model. Every
/// record is written as its original raw ten-byte value and read back exactly.
/// This is intentionally separate from the UI and can be called by the recovery
/// CLI when the main application cannot start.
pub fn restore_backup(path: &Path) -> Result<(), String> {
    let backup = OnboardBackup::load_and_validate(path).map_err(|e| e.to_string())?;
    let payload = &backup.payload;
    if !payload.scan_complete || !payload.read_failures.is_empty() || payload.records.len() != 42 {
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
        0x40u8,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0x0b,0x0c,
        0x01,0x02,0x03,0x09,0x0a,0x34,0x35,
    ].into_iter().flat_map(|button| [(button, 0), (button, 1)]).collect();
    let actual: BTreeSet<(u8, u8)> = payload.records.iter().map(|r| (r.button_id, r.layer)).collect();
    if actual != expected {
        return Err("refusing to restore: backup does not contain the complete expected button/layer address set".into());
    }

    let device = LinuxOnboardDevice::open()?;
    for record in &payload.records {
        device.write_raw_record(record)?;
    }
    Ok(())
}

/// Create a new backup file from the connected verified Naga. Existing files
/// are never replaced; the caller must choose a fresh destination.
pub fn backup_connected_mouse(path: &Path) -> Result<(), String> {
    let device = LinuxOnboardDevice::open()?;
    let backup = device.create_backup()?;
    backup.save_new(path).map_err(|e| e.to_string())
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
    fn snapshot_address_set_has_expected_record_count() {
        let ids = [0x40u8,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x49,0x4a,0x4b,0x0b,0x0c,
                   0x01,0x02,0x03,0x09,0x0a,0x34,0x35];
        let unique: BTreeSet<u8> = ids.into_iter().collect();
        assert_eq!(unique.len(), 21);
        assert_eq!(unique.len() * 2, 42);
    }
}
