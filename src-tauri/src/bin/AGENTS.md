# src-tauri/src/bin — Standalone command-line utilities

## Purpose

Small executables that must remain usable independently of the Tauri window, especially recovery when the main UI cannot start.

## Ownership

- `sidearm-recover.rs` — Linux onboard-memory backup, validation, and explicitly confirmed raw-record restoration.

## Local Contracts

- Never require root. Report HID permission failures and direct the operator to narrow udev permissions.
- Restore commands must require explicit interactive confirmation and must validate device identity, snapshot completeness, checksum, and every raw record before the first write.
- Never replace an existing backup file.
- Keep recovery operations independent of frontend state and configuration loading.

## Work Guidance

- Keep command syntax and examples in `docs/LINUX-ONBOARD-RECOVERY.md`.

## Verification

- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`

## Child DOX Index

- None.
