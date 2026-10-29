//! Portable, lossless container for data read from a mouse's onboard memory.
//!
//! This module deliberately does not talk to hardware. A future platform
//! transport must collect raw records and pass them here; no restore operation
//! is exposed until the device protocol has a separately verified raw writer.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
use tempfile::NamedTempFile;

pub const FORMAT_ID: &str = "sidearm-onboard-backup";
pub const FORMAT_VERSION: u32 = 1;
pub const BUTTON_RECORD_LEN: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceIdentity {
    pub vendor_id: u16,
    pub product_id: u16,
    pub model: String,
    pub interface: u8,
    pub protocol: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawButtonRecord {
    pub button_id: u8,
    pub layer: u8,
    /// Exact ten bytes returned by GET_BUTTON, including reserved/unknown data.
    pub bytes: [u8; BUTTON_RECORD_LEN],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadFailure {
    pub button_id: u8,
    pub layer: u8,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadableSettings {
    pub dpi: Option<u16>,
    pub poll_rate_hz: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotPayload {
    pub format: String,
    pub format_version: u32,
    pub sidearm_version: String,
    pub captured_at: String,
    pub device: DeviceIdentity,
    pub settings: ReadableSettings,
    pub records: Vec<RawButtonRecord>,
    /// Every requested address that could not be read. Missing records are
    /// never silently interpreted as empty/default records.
    pub read_failures: Vec<ReadFailure>,
    /// True only when the caller has completed its full, model-defined scan.
    pub scan_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OnboardBackup {
    pub payload: SnapshotPayload,
    /// Lowercase SHA-256 of the canonical serialized payload bytes.
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("backup file operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("backup checksum does not match its contents")]
    Checksum,
    #[error("backup format or version is not supported")]
    UnsupportedFormat,
    #[error("backup contains duplicate or invalid record addresses")]
    InvalidRecords,
    #[error("backup path already exists; choose a new filename to preserve the existing backup")]
    AlreadyExists,
}

impl OnboardBackup {
    pub fn new(payload: SnapshotPayload) -> Result<Self, BackupError> {
        let bytes = serde_json::to_vec(&payload)?;
        let sha256 = hex_digest(&bytes);
        let backup = Self { payload, sha256 };
        backup.validate()?;
        Ok(backup)
    }

    pub fn validate(&self) -> Result<(), BackupError> {
        if self.payload.format != FORMAT_ID || self.payload.format_version != FORMAT_VERSION {
            return Err(BackupError::UnsupportedFormat);
        }
        let mut seen = std::collections::BTreeSet::new();
        for record in &self.payload.records {
            if record.layer > 1 || !seen.insert((record.button_id, record.layer)) {
                return Err(BackupError::InvalidRecords);
            }
        }
        for failure in &self.payload.read_failures {
            if failure.layer > 1 || !seen.insert((failure.button_id, failure.layer)) {
                return Err(BackupError::InvalidRecords);
            }
        }
        if self.payload.scan_complete && !self.payload.read_failures.is_empty() {
            return Err(BackupError::InvalidRecords);
        }
        let bytes = serde_json::to_vec(&self.payload)?;
        if self.sha256 != hex_digest(&bytes) {
            return Err(BackupError::Checksum);
        }
        Ok(())
    }

    /// Validate and write without replacing an existing file. The temporary
    /// file is created in the destination directory so persist is same-volume.
    pub fn save_new(&self, path: &Path) -> Result<(), BackupError> {
        self.validate()?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        if path.exists() {
            return Err(BackupError::AlreadyExists);
        }
        let mut temp = NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer_pretty(&mut temp, self)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Err(BackupError::AlreadyExists),
            Err(error) => Err(BackupError::Io(error.error)),
        }
    }

    pub fn load_and_validate(path: &Path) -> Result<Self, BackupError> {
        let backup: Self = serde_json::from_slice(&fs::read(path)?)?;
        backup.validate()?;
        Ok(backup)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SnapshotPayload {
        SnapshotPayload {
            format: FORMAT_ID.into(),
            format_version: FORMAT_VERSION,
            sidearm_version: "test".into(),
            captured_at: "2026-10-30T00:00:00Z".into(),
            device: DeviceIdentity {
                vendor_id: 0x1532,
                product_id: 0x00b4,
                model: "Razer Naga V2 HyperSpeed".into(),
                interface: 0,
                protocol: "razer-command-class-v1".into(),
            },
            settings: ReadableSettings { dpi: Some(1600), poll_rate_hz: Some(1000) },
            records: vec![RawButtonRecord { button_id: 0x40, layer: 0, bytes: [1,2,3,4,5,6,7,8,9,10] }],
            read_failures: vec![ReadFailure { button_id: 0x41, layer: 0, reason: "no response".into() }],
            scan_complete: false,
        }
    }

    #[test]
    fn round_trip_preserves_unknown_raw_bytes_and_incomplete_scan() {
        let backup = OnboardBackup::new(sample()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mouse.sidearm-backup.json");
        backup.save_new(&path).unwrap();
        let loaded = OnboardBackup::load_and_validate(&path).unwrap();
        assert_eq!(loaded, backup);
        assert!(!loaded.payload.scan_complete);
        assert_eq!(loaded.payload.records[0].bytes, [1,2,3,4,5,6,7,8,9,10]);
    }

    #[test]
    fn refuses_to_overwrite_existing_backup() {
        let backup = OnboardBackup::new(sample()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known-good.json");
        fs::write(&path, b"keep me").unwrap();
        assert!(matches!(backup.save_new(&path), Err(BackupError::AlreadyExists)));
        assert_eq!(fs::read(path).unwrap(), b"keep me");
    }

    #[test]
    fn rejects_tampered_payload_and_duplicate_addresses() {
        let mut backup = OnboardBackup::new(sample()).unwrap();
        backup.payload.records[0].bytes[0] ^= 0xff;
        assert!(matches!(backup.validate(), Err(BackupError::Checksum)));

        let mut payload = sample();
        payload.read_failures.clear();
        payload.records.push(payload.records[0].clone());
        assert!(matches!(OnboardBackup::new(payload), Err(BackupError::InvalidRecords)));
    }

    #[test]
    fn complete_scan_cannot_contain_read_failures() {
        let mut payload = sample();
        payload.scan_complete = true;
        assert!(matches!(OnboardBackup::new(payload), Err(BackupError::InvalidRecords)));
    }
}
