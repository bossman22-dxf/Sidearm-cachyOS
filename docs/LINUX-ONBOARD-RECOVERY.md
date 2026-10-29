# Linux onboard-memory backup and recovery

## Scope

The native Linux backup path currently targets only the verified Razer Naga V2 HyperSpeed receiver (USB VID:PID `1532:00b4`, vendor HID interface 0). It reads 21 known addressable button IDs across layers 0 and 1, preserving each ten-byte record exactly. Other Razer models and Bluetooth are not supported by this path.

The backup also records live DPI and polling rate when those reads succeed. The current restore path restores button records only; it does not restore DPI, polling rate, battery state, or device mode.

## Safety

- Backup and validation are read-only with respect to onboard memory.
- A backup is restorable only if all 42 button/layer addresses were read and the device identity and protocol match.
- Restore writes each original raw record and immediately verifies an exact byte-for-byte read-back. It stops on the first failure; a failed restore may therefore be partial. Keep the original backup file unchanged.
- The standalone recovery utility asks the operator to type `RESTORE` before it writes anything.
- Never run Sidearm as root. If HID access is denied, configure a narrow user-session udev permission for this exact VID:PID; do not use world-writable HID nodes.
- No Linux onboard button-mapping editor is enabled yet. Do not add one until backup/restore has been tested on the actual mouse.

## Build and use

From the repository root in Fish:

```fish
cargo build --manifest-path src-tauri/Cargo.toml --bin sidearm-recover
```

Create a read-only backup:

```fish
cargo run --manifest-path src-tauri/Cargo.toml --bin sidearm-recover -- --backup-onboard-memory "$HOME/naga-v2-hyperspeed.sidearm-backup.json"
```

Validate it without touching the mouse:

```fish
cargo run --manifest-path src-tauri/Cargo.toml --bin sidearm-recover -- --validate-onboard-backup "$HOME/naga-v2-hyperspeed.sidearm-backup.json"
```

Restore only after the backup has been validated and the operator intends to replace the connected mouse's onboard button records:

```fish
cargo run --manifest-path src-tauri/Cargo.toml --bin sidearm-recover -- --restore-onboard-backup "$HOME/naga-v2-hyperspeed.sidearm-backup.json"
```

The restore utility requires the exact confirmation word `RESTORE`.

## Current verification boundary

Automated protocol/container tests and Linux CI must pass before considering the implementation build-ready. Hardware read, backup, and restore must still be tested on the actual Naga V2 HyperSpeed receiver. A successful build alone does not establish hardware compatibility or safe restoration.
