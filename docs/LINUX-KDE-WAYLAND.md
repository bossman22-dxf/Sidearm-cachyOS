# KDE Plasma Wayland integration

Sidearm uses KWin's scripting interface for foreground-window identity on KDE Wayland. XCB cannot reliably identify native Wayland windows, so KDE Wayland does not silently fall back to XCB.

## How it works

1. At startup, Sidearm creates `kwin/sidearm-focus.js` under its application config directory.
2. Sidearm registers a private session-D-Bus service named `org.sidearm.Sidearm`.
3. Sidearm asks KWin's `org.kde.kwin.Scripting` interface to load and run the script.
4. The script listens for `workspace.windowActivated` and active-window title changes, then sends the PID, caption, and resource class to Sidearm.
5. Sidearm resolves the executable path from `/proc/<pid>/exe` and feeds the shared profile resolver.
6. On a clean exit, Sidearm unloads the script. A later launch also unloads any stale copy before loading the current one.

The integration does not edit `kwinrc`, install a persistent KWin plugin, require `qdbus`/ `kdotool`, or need root access.

## Manual verification on KDE Wayland

- Start Sidearm from a KDE Wayland session and check the application log for `KWin Wayland focus monitor started`.
- Switch between a native Wayland application (for example Dolphin or Konsole), a Wayland browser, and an XWayland application.
- Confirm the active profile follows the foreground process.
- Change a window title (for example, switch browser tabs) and confirm the title refreshes without changing the process profile.
- Minimize and restore windows, switch virtual desktops, and close the active window.
- Exit Sidearm normally and confirm the KWin script is unloaded; restart Sidearm and confirm it loads again.

If the log reports that the KWin focus monitor is not ready, inspect Sidearm's session-D-Bus/KWin startup errors. Do not work around it by running Sidearm as root or by adding broad window-management permissions.
