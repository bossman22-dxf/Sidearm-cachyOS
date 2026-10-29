# Linux Port: Hardware Safety, Backup/Recovery, and Accessibility

This document defines the safety and usability requirements for the native Linux port. The primary target is CachyOS (Arch Linux) with KDE Plasma on Wayland, using the Razer Naga V2 HyperSpeed (USB ID 1532:00b4).

## Non-negotiable safety rule

No experimental write to mouse non-volatile/onboard memory is permitted until the backup and restore path has been implemented, reviewed, and tested against a non-destructive/read-only protocol path.

The app must never assume that a successful USB/HID write means the device accepted or persisted the requested configuration. A write is successful only after the device's documented acknowledgement and a read-back verification (or an explicitly documented protocol limitation).

## Backup format and lifecycle

Implement a versioned, portable backup format. It must:

- Store a schema version, application version, device vendor/product IDs, device model/protocol identifier, capture timestamp, and the exact profile/settings payloads that were successfully read.
- Preserve unknown fields and raw protocol data where possible. Do not silently discard settings the current app does not understand.
- Record which settings were actually captured and which were unavailable. Never represent an incomplete backup as complete.
- Be written atomically: write a temporary file, flush/sync it, validate it, then rename it into place.
- Use restrictive file permissions (0600) and a user-owned configuration/data directory. Never require root merely to create or inspect a backup.
- Be independently exportable/importable; do not make recovery depend on the app's database or current UI state.
- Include a checksum for corruption detection. A checksum is not a signature and must not be described as one.
- Keep previous backups rather than overwriting the only known-good copy. Make backup creation easy to repeat before every write operation.

Suggested extension: `.sidearm-backup.json`. The format should be documented and stable.

## Restore/recovery behaviour

Provide two clearly separate operations:

1. **Validate backup** — parse, schema-check, verify checksum, confirm target device identity/protocol compatibility, and show exactly which settings are present and absent. This operation must not write to the device.
2. **Restore to mouse** — require an explicit confirmation that names the target device and backup timestamp; write settings in a safe order; check each device acknowledgement; read back and compare where supported; report partial failure precisely.

Never attempt to restore a backup made for a different model or incompatible protocol revision without an explicit, strongly worded override. Prefer refusing the operation over guessing.

A failed restore must not trigger repeated blind writes. Preserve the backup and provide a diagnostic report. If the protocol supports a safe reset or recovery mode, document it separately; do not invent one.

The recovery path must be usable even if normal profile loading or the main UI is broken. Provide a small CLI command (for example, `sidearm backup create`, `sidearm backup validate <file>`, `sidearm backup restore <file>`) using the same reviewed hardware library. The exact command syntax can be finalized with the implementation.

## Hardware access and privilege boundaries

- Use Linux HID access through a narrowly scoped backend; do not run the whole application as root.
- Prefer `hidraw` or an appropriate Linux HID library for vendor-specific feature reports, after confirming the device's actual interfaces and report descriptors.
- Coordinate with OpenRazer: detect competing access, explain the conflict, and fail safely. Do not unbind kernel drivers or stop services automatically.
- Provide a udev rule/package guidance only if required. Grant access only to the exact vendor/product/interface needed; avoid broad world-writable device permissions.
- Keep read-only discovery separate from write-capable operations in the API.
- Log protocol metadata and errors, but never log sensitive user data or silently claim success.

## Keyboard accessibility requirements

All user-facing workflows must be operable without a mouse:

- Logical and predictable Tab/Shift+Tab order; no keyboard traps.
- Visible focus indicator in all themes and states.
- Enter/Space activate controls appropriately; Escape closes dialogs/popovers without losing unsaved work silently.
- Every interactive element has an accessible name, role, and state; icon-only controls require labels/tooltips.
- Device selection, profile selection, mapping assignment, backup creation, backup validation, restore confirmation, and error recovery are keyboard-operable.
- Do not rely on drag-and-drop, hover-only information, colour alone, or pointer-only gestures.
- Provide keyboard alternatives for reorder/reassign operations.
- Dialogs move focus into the dialog, constrain focus while modal, and return focus to the invoking control when closed.
- Announce asynchronous device detection, backup progress, write/read-back progress, and errors to assistive technology.
- Respect system text scaling and high-contrast settings where the UI framework supports them.

## Test and release gates

Before enabling any onboard write in a release:

1. Unit tests for backup serialization, schema migration, checksum, atomic writes, malformed files, truncated files, unknown fields, and device mismatch.
2. Hardware protocol tests using mocked HID transport, including timeouts, disconnects, malformed replies, partial writes, and read-back mismatch.
3. Linux CI compile/test on x86_64; test the actual CachyOS packaging path separately.
4. Accessibility tests for keyboard-only completion of every core workflow, plus automated accessibility checks where available.
5. A read-only hardware discovery test on the target Naga V2 HyperSpeed.
6. A backup created from the user's untouched current mouse configuration, then validated offline.
7. Only after all above: controlled write tests, one setting at a time, with restore immediately available.

## Current known target-specific context

The Naga V2 HyperSpeed is USB ID `1532:00b4`. Linux currently exposes multiple HID/input interfaces for it. Its `device_mode` sysfs attribute has previously been observed to switch between `00 00` and `03 00`; this is a driver-mode setting, not proof of onboard profile data. Do not conflate this sysfs workaround with the mouse's onboard configuration backup.

The desired initial mapping is:
- Twelve thumb buttons: F13–F24.
- Separate M4: genuine mouse button 4.
- Separate M5: genuine mouse button 5.

These are target requirements, not assumptions about the current onboard state. The backup must capture the actual current state before any mapping changes.
