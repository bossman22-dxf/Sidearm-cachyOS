//! Platform-specific window detection (Linux).
//!
//! Provides native foreground-window capture on KDE Plasma Wayland through a
//! KWin script and a private session-D-Bus service. GNOME Wayland uses its
//! focused-window extension; X11 falls back to `active-win-pos-rs` (XCB).

#![allow(unused_imports)]

use std::{
    path::Path,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock, mpsc},
    thread,
    time::Duration,
};

use crate::runtime::timestamp_millis;
use crate::window_capture::RawWindowCapture;

const KWIN_BUS_NAME: &str = "org.sidearm.Sidearm";
const KWIN_OBJECT_PATH: &str = "/org/sidearm/Focus";
const KWIN_PLUGIN_NAME: &str = "sidearm-focus-monitor";

static KDE_WINDOW_CACHE: OnceLock<Arc<Mutex<Option<RawWindowCapture>>>> = OnceLock::new();
static KDE_MONITOR_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct KWinFocusService {
    cache: Arc<Mutex<Option<RawWindowCapture>>>,
}

#[zbus::interface(name = "org.sidearm.Focus")]
impl KWinFocusService {
    fn window_activated(&self, pid: i32, title: String, resource_class: String) {
        if pid <= 0 {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }

        let pid = pid as u32;
        let process_path = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let exe = if !resource_class.is_empty() {
            resource_class.to_ascii_lowercase()
        } else {
            Path::new(&process_path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase()
        };
        let capture = RawWindowCapture {
            hwnd: String::new(),
            pid,
            exe,
            process_path,
            title,
            captured_at: timestamp_millis(),
            is_elevated: is_process_elevated(pid),
        };
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(capture);
    }
}

/// Start a small KWin script that pushes the active Wayland window to a private
/// session-bus service owned by Sidearm. This avoids polling the journal or
/// relying on XWayland's incomplete view of native Wayland windows.
pub(crate) fn start_kde_focus_monitor(config_dir: &Path) {
    if !is_kde_wayland() || KDE_MONITOR_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let script_dir = config_dir.join("kwin");
    let script_path = script_dir.join("sidearm-focus.js");
    if let Err(error) = std::fs::create_dir_all(&script_dir)
        .and_then(|()| std::fs::write(&script_path, KWIN_FOCUS_SCRIPT))
    {
        KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
        log::warn!("[window] Could not install the KWin focus script: {error}");
        return;
    }

    let cache = KDE_WINDOW_CACHE
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let service_cache = cache.clone();
    let service_started = thread::Builder::new()
        .name("kwin-focus-dbus".into())
        .spawn(move || {
            let result = zbus::blocking::connection::Builder::session()
                .and_then(|builder| builder.name(KWIN_BUS_NAME))
                .and_then(|builder| {
                    builder.serve_at(KWIN_OBJECT_PATH, KWinFocusService { cache: service_cache })
                })
                .and_then(|builder| builder.build());
            match result {
                Ok(connection) => {
                    let _ = ready_tx.send(Ok(()));
                    // Keep the service connection alive for the lifetime of Sidearm.
                    let _connection = connection;
                    loop {
                        thread::park_timeout(Duration::from_secs(3600));
                    }
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    log::warn!("[window] Could not start the KWin focus D-Bus service: {error}");
                }
            }
        });

    if let Err(error) = service_started {
        KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
        log::warn!("[window] Could not start the KWin focus service thread: {error}");
        return;
    }

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
            log::warn!("[window] KWin focus service failed: {error}");
            return;
        }
        Err(error) => {
            KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
            log::warn!("[window] KWin focus service did not become ready: {error}");
            return;
        }
    }

    if let Err(error) = load_kwin_focus_script(&script_path) {
        KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
        log::warn!("[window] Could not load the KWin focus script: {error}");
    } else {
        log::info!("[window] KWin Wayland focus monitor started.");
    }
}

/// Unload the KWin script before Sidearm exits so it does not keep calling
/// a D-Bus service that no longer exists.
pub(crate) fn stop_kde_focus_monitor() {
    if !KDE_MONITOR_STARTED.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(connection) = zbus::blocking::Connection::session()
        && let Ok(proxy) = zbus::blocking::Proxy::new(
            &connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
    {
        let _ = proxy.call::<_, _, ()>("unloadScript", &KWIN_PLUGIN_NAME);
    }
    KDE_MONITOR_STARTED.store(false, Ordering::SeqCst);
    if let Some(cache) = KDE_WINDOW_CACHE.get() {
        *cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn is_kde_wayland() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    desktop.split(':').any(|part| part == "kde" || part == "plasma")
        || std::env::var_os("KDE_SESSION_VERSION").is_some()
        || std::env::var_os("KDE_FULL_SESSION").is_some()
}

fn load_kwin_focus_script(script_path: &Path) -> Result<(), String> {
    use zbus::blocking::{Connection, Proxy};

    let connection = Connection::session().map_err(|e| format!("session D-Bus unavailable: {e}"))?;
    let proxy = Proxy::new(
        &connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .map_err(|e| format!("KWin scripting D-Bus interface unavailable: {e}"))?;

    let _ = proxy.call::<_, _, ()>("unloadScript", &KWIN_PLUGIN_NAME);
    let args = (script_path.to_string_lossy().into_owned(), KWIN_PLUGIN_NAME.to_string());
    let script_id: i32 = proxy
        .call("loadScript", &args)
        .map_err(|e| format!("KWin refused to load Sidearm's focus script: {e}"))?;
    if script_id < 0 {
        return Err("KWin returned an invalid script ID for Sidearm's focus monitor".into());
    }
    let object_path = format!("/Scripting/Script{script_id}");
    let script_proxy = Proxy::new(
        &connection,
        "org.kde.KWin",
        object_path.as_str(),
        "org.kde.kwin.Script",
    )
    .map_err(|e| format!("KWin script object unavailable: {e}"))?;
    match script_proxy.call::<_, _, ()>("run", &()) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = proxy.call::<_, _, ()>("unloadScript", &KWIN_PLUGIN_NAME);
            Err(format!("KWin refused to start Sidearm's focus script: {error}"))
        }
    }
}

const KWIN_FOCUS_SCRIPT: &str = r#"
var sidearmActiveWindow = null;
function sidearmPublishWindow(win) {
    callDBus(
        "org.sidearm.Sidearm",
        "/org/sidearm/Focus",
        "org.sidearm.Focus",
        "WindowActivated",
        win && win.pid ? win.pid : 0,
        win && win.caption ? win.caption : "",
        win && win.resourceClass ? win.resourceClass : ""
    );
}
function sidearmCaptionChanged() {
    if (workspace.activeWindow === sidearmActiveWindow) {
        sidearmPublishWindow(sidearmActiveWindow);
    }
}
function sidearmWindowActivated(win) {
    if (sidearmActiveWindow && sidearmActiveWindow.captionChanged) {
        sidearmActiveWindow.captionChanged.disconnect(sidearmCaptionChanged);
    }
    sidearmActiveWindow = win;
    if (sidearmActiveWindow && sidearmActiveWindow.captionChanged) {
        sidearmActiveWindow.captionChanged.connect(sidearmCaptionChanged);
    }
    sidearmPublishWindow(sidearmActiveWindow);
}
workspace.windowActivated.connect(sidearmWindowActivated);
sidearmWindowActivated(workspace.activeWindow);
"#;

/// Call the Sidearm GNOME Shell extension D-Bus `Get` method and parse the JSON
/// payload it returns (gdbus wraps it as `('{...}',)`). Shared by
/// `try_gnome_dbus` and `try_gnome_dbus_geometry`.
fn fetch_focused_window_json() -> Option<serde_json::Value> {
    let output = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "com.sidearm.FocusedWindow",
            "--object-path",
            "/com/sidearm/FocusedWindow",
            "--method",
            "com.sidearm.FocusedWindow.Get",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // gdbus output format: ('{"pid":1234,"title":"...","wm_class":"...","x":0,"y":0,"width":800,"height":600}',)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_str = stdout.trim().strip_prefix("('")?.strip_suffix("',)")?;
    serde_json::from_str(json_str).ok()
}

/// Try to get the focused window via the Sidearm GNOME Shell extension D-Bus
/// interface (`com.sidearm.FocusedWindow`). Returns `None` if the extension is
/// unavailable or the call fails.
fn try_gnome_dbus() -> Option<RawWindowCapture> {
    let v = fetch_focused_window_json()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    if pid == 0 {
        return None;
    }

    let title = v.get("title")?.as_str().unwrap_or_default().to_owned();
    let wm_class = v.get("wm_class")?.as_str().unwrap_or_default().to_owned();

    // Resolve process path via /proc/<pid>/exe
    let proc_exe_path = format!("/proc/{pid}/exe");
    let process_path = std::fs::read_link(&proc_exe_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let exe = if !wm_class.is_empty() {
        wm_class.to_ascii_lowercase()
    } else {
        Path::new(&process_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
    };

    let is_elevated = is_process_elevated(pid);

    Some(RawWindowCapture {
        hwnd: String::new(),
        pid,
        exe,
        process_path,
        title,
        captured_at: timestamp_millis(),
        is_elevated,
    })
}

/// Capture information about the current foreground window.
///
/// KDE Wayland reads the active window pushed by KWin scripting; GNOME Wayland
/// uses the Sidearm GNOME extension; X11 uses the XCB active-window provider.
pub(crate) fn capture_foreground_window() -> Result<RawWindowCapture, String> {
    // KDE Wayland does not expose active-window details through a readable
    // D-Bus property. The KWin script pushes changes into Sidearm's session
    // service, so return that cached native Wayland window directly.
    if is_kde_wayland() {
        let capture = KDE_WINDOW_CACHE.get().and_then(|cache| {
            cache.lock().unwrap_or_else(|e| e.into_inner()).clone()
        });
        return capture.ok_or_else(|| {
            "KWin Wayland focus monitor is not ready; check Sidearm's KWin script and session D-Bus connection.".into()
        });
    }

    // GNOME Wayland uses its own focused-window extension.
    if std::env::var("WAYLAND_DISPLAY").is_ok()
        && let Some(capture) = try_gnome_dbus()
    {
        return Ok(capture);
    }

    // Fallback to X11/XCB
    let window = active_win_pos_rs::get_active_window()
        .map_err(|e| format!("Failed to get active window: {e:?}"))?;

    let pid = window.process_id as u32;
    let title = window.title;
    let window_id = window.window_id;

    // Resolve process path via /proc/<pid>/exe symlink
    let proc_exe_path = format!("/proc/{pid}/exe");
    let process_path = std::fs::read_link(&proc_exe_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Extract lowercase basename for exe matching
    let exe = Path::new(&process_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let is_elevated = is_process_elevated(pid);

    Ok(RawWindowCapture {
        hwnd: window_id,
        pid,
        exe,
        process_path,
        title,
        captured_at: timestamp_millis(),
        is_elevated,
    })
}

/// Check whether a target process is running with elevated privileges (root).
///
/// Reads `/proc/<pid>/status` and parses the `Uid:` line.
/// The effective UID is the second field. If it is 0, the process is root.
fn is_process_elevated(pid: u32) -> bool {
    let status_path = format!("/proc/{pid}/status");
    match std::fs::read_to_string(&status_path) {
        Ok(content) => parse_effective_uid_is_root(&content),
        Err(e) => elevated_on_read_error(&e),
    }
}

/// Fail-safe interpretation of a `/proc/<pid>/status` read error.
///
/// Mirrors the Windows-side fail-safe (window.rs `is_process_elevated`):
/// a permission error (e.g. `/proc` mounted with `hidepid=2`) means we
/// genuinely cannot tell, so assume elevated — under-warning is worse than
/// over-warning, since input injection into an elevated window would fail
/// regardless. Only a vanished process (`NotFound`) is treated as not-elevated.
/// See finding F044.
fn elevated_on_read_error(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
}

/// Parse the effective UID from `/proc/<pid>/status` contents and report whether
/// it is root (0). Returns false when no `Uid:` line is present.
fn parse_effective_uid_is_root(content: &str) -> bool {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // Fields: real, effective, saved-set, filesystem
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if let Some(effective_uid) = fields.get(1) {
                return *effective_uid == "0";
            }
        }
    }

    false
}

/// Check whether the current process itself is running elevated (root).
///
/// Reads `/proc/self/status` and parses the effective UID.
pub(crate) fn is_current_process_elevated() -> bool {
    is_process_elevated(std::process::id())
}

/// Check whether the current foreground window is fullscreen.
///
/// On Wayland, queries the Sidearm GNOME Shell D-Bus extension for window
/// geometry. Falls back to `active-win-pos-rs` (XCB) on X11. Returns false
/// on any error -- the Tauri window manager will clip to screen regardless.
pub(crate) fn is_foreground_fullscreen() -> bool {
    // Try GNOME Wayland D-Bus extension first
    if std::env::var("WAYLAND_DISPLAY").is_ok()
        && let Some((x, y, w, h)) = try_gnome_dbus_geometry()
    {
        if x != 0 || y != 0 {
            return false;
        }
        return w >= 1024 && h >= 768;
    }

    // Fallback to X11/XCB
    let window = match active_win_pos_rs::get_active_window() {
        Ok(w) => w,
        Err(_) => return false,
    };

    let pos = window.position;
    let win_x = pos.x;
    let win_y = pos.y;
    let win_w = pos.width;
    let win_h = pos.height;

    if win_x != 0.0 || win_y != 0.0 {
        return false;
    }

    win_w >= 1024.0 && win_h >= 768.0
}

/// Query window geometry from the GNOME Shell D-Bus extension.
/// Returns (x, y, width, height) or None.
fn try_gnome_dbus_geometry() -> Option<(i64, i64, i64, i64)> {
    let v = fetch_focused_window_json()?;

    let x = v.get("x")?.as_i64()?;
    let y = v.get("y")?.as_i64()?;
    let w = v.get("width")?.as_i64()?;
    let h = v.get("height")?.as_i64()?;
    Some((x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    // F044: a permission error must be treated as "elevated" (fail-safe),
    // matching the Windows-side behaviour, so a window we cannot inspect (e.g.
    // hidepid=2) still triggers the elevation warning.
    #[test]
    fn permission_denied_is_treated_as_elevated() {
        let err = Error::from(ErrorKind::PermissionDenied);
        assert!(
            elevated_on_read_error(&err),
            "permission denied must conservatively report elevated"
        );
    }

    // A vanished process (status file gone) is NOT elevated.
    #[test]
    fn not_found_is_not_elevated() {
        let err = Error::from(ErrorKind::NotFound);
        assert!(
            !elevated_on_read_error(&err),
            "a missing process must report not-elevated"
        );
    }

    #[test]
    fn effective_uid_root_is_elevated() {
        let status = "Name:\tbash\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n";
        assert!(parse_effective_uid_is_root(status));
    }

    #[test]
    fn effective_uid_nonroot_is_not_elevated() {
        let status = "Name:\tbash\nUid:\t1000\t1000\t1000\t1000\n";
        assert!(!parse_effective_uid_is_root(status));
    }

    #[test]
    fn missing_uid_line_is_not_elevated() {
        let status = "Name:\tbash\nState:\tR (running)\n";
        assert!(!parse_effective_uid_is_root(status));
    }
}
