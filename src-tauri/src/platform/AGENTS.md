# src-tauri/src/platform — OS-specific layer

## Purpose

Platform-specific implementations behind a shared interface: display/DPI, input synthesis details, foreground-window info, and shell/registry lookups. Windows remains supported; native Linux on CachyOS/KDE Plasma Wayland is a primary target.

## Ownership

- `mod.rs` — the platform-agnostic surface.
- `windows/` — `display.rs`, `input.rs`, `window.rs`, `shell.rs` (App Paths registry, `REG_EXPAND_SZ` expansion, path-traversal-guarded exe lookup), plus process/elevation helpers.
- `linux/` — `display.rs`, `window.rs`, `shell.rs`; KDE Wayland focus integration is owned by `linux/AGENTS.md`.

## Local Contracts

- Keep `#[cfg(target_os = ...)]` branches behind the shared `mod.rs` interface; callers stay platform-agnostic.
- Process-info fields (path/exe/elevated) are cached per `(hwnd, pid)`; window title is NOT cached (it changes for the same HWND, e.g. browser tabs).
- Registry/shell lookups sanitize inputs and never cross a privilege boundary.
- When Sidearm runs elevated, launch/open targets run with the shell (explorer) token via `shell::spawn_with_shell_token`; fail closed, never inherit the admin token.

## Work Guidance

(empty)

## Verification

- `cargo test` + `cargo clippy ... -D warnings`. Clippy runs against the Windows target; Linux-only branches compile on Linux CI.

## Child DOX Index

- `linux/AGENTS.md` — Linux platform implementation and KDE/GNOME Wayland integration.
