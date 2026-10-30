# sidearm-recover — Standalone command-line utility

## Purpose

A standalone executable that must remain usable independently of the Tauri window, especially recovery when the main UI cannot start.

## Ownership

- `main.rs` — Linux onboard-memory backup, validation, explicitly confirmed raw-record restoration, and explicitly confirmed standard mapping.

## Local Contracts

- Never require root. Report HID permission failures and direct the operator to narrow udev permissions.
- Restore and mapping commands must require explicit interactive confirmation and must validate device identity, snapshot completeness, checksum, device mode, and every raw record before the first write.
- A mapping operation must preserve the original device mode in the backup, switch to normal/onboard mode before writing, and attempt a full raw-record plus mode rollback if any mapping write fails.
- Never replace an existing backup file.
- Keep recovery operations independent of frontend state and configuration loading.

## Work Guidance

- Keep command syntax and examples in `docs/LINUX-ONBOARD-RECOVERY.md`.

## Verification

- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`

## Child DOX Index

- None.
