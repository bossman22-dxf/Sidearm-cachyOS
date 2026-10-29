# src-tauri/src/platform/linux — Linux platform implementation

## Purpose

Native Linux platform support, with CachyOS and KDE Plasma on Wayland as a primary target. This layer provides foreground-window identity, shell opening, display geometry, and Linux-specific integration behind the shared platform interface.

## Ownership

- `display.rs` — monitor resolution and OSD placement using Linux display metadata.
- `shell.rs` — opening files, URLs, and folders through the desktop session.
- `window.rs` — foreground-window capture, process identity/elevation, and the KWin/GNOME Wayland bridges.

## Local Contracts

- KDE Wayland foreground capture uses a KWin script loaded at Sidearm startup. The script sends active-window changes to Sidearm's private session-D-Bus service; unload it on clean shutdown.
- Keep the script under Sidearm's app config directory. Do not edit `kwinrc`, install a persistent third-party KWin plugin, or require `kdotool`.
- On KDE Wayland, do not silently fall back to XCB when the KWin monitor is unavailable; XCB cannot reliably identify native Wayland windows. Return a clear error instead.
- GNOME Wayland retains its focused-window D-Bus extension path. X11 retains the XCB fallback.
- Resolve process paths from `/proc/<pid>/exe`; never infer an executable from a window title.
- Never run Sidearm as root to obtain process or window access.

## Work Guidance

- Preserve the shared `RawWindowCapture` contract so profile resolution stays platform-agnostic.
- Keep the KWin script event-driven; do not poll the journal or reload the script on every foreground-window query.

## Verification

- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path src-tauri/Cargo.toml`
- Manual KDE Plasma Wayland smoke test: switch between native Wayland and XWayland apps, change a browser tab title, minimize/restore windows, and confirm profile switching follows the active process.
- Manual GNOME Wayland and X11 regression checks.

## Child DOX Index

- None.
