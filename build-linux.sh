#!/usr/bin/env bash
# Sidearm Linux Build Script
# Usage: ./build-linux.sh [--clean]
#
# Builds the release binary + AppImage bundle.
# Run this on a Linux machine with the repo cloned.
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
TAURI_DIR="$PROJECT_DIR/src-tauri"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${CYAN}[build]${NC} $*"; }
warn() { echo -e "${YELLOW}[build]${NC} $*"; }
err()  { echo -e "${RED}[build]${NC} $*" >&2; }
ok()   { echo -e "${GREEN}[build]${NC} $*"; }

# ── Parse args ──────────────────────────────────────────────
CLEAN=false
for arg in "$@"; do
    case "$arg" in
        --clean) CLEAN=true ;;
        -h|--help)
            echo "Usage: ./build-linux.sh [--clean]"
            echo "  --clean  Force full recompile (cargo clean)"
            exit 0
            ;;
    esac
done

# ── Check prerequisites ────────────────────────────────────
log "Checking prerequisites..."

missing=()
for cmd in cargo rustc node npm; do
    if ! command -v "$cmd" &>/dev/null; then
        missing+=("$cmd")
    fi
done

if [ ${#missing[@]} -gt 0 ]; then
    err "Missing: ${missing[*]}"
    echo ""
    echo "  Install Rust:    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo "  Install Node.js: https://nodejs.org/ (or via nvm)"
    exit 1
fi

# Tauri v2 dependencies differ by distribution. Check installed packages
# without installing anything or upgrading the system.
missing_libs=()
if command -v pacman &>/dev/null; then
    log "Checking Arch/CachyOS build dependencies..."
    required_packages=(
        webkit2gtk-4.1 gtk3
        gcc make pkgconf
        curl wget file openssl
        appmenu-gtk-module libappindicator-gtk3 librsvg xdotool
        patchelf systemd-libs
    )
    for package in "${required_packages[@]}"; do
        if ! pacman -T "$package" &>/dev/null; then
            missing_libs+=("$package")
        fi
    done

    if [ ${#missing_libs[@]} -gt 0 ]; then
        err "Missing system packages: ${missing_libs[*]}"
        echo ""
        echo "  Install only the missing packages with:"
        echo "  sudo pacman -S --needed ${missing_libs[*]}"
        exit 1
    fi
elif command -v dpkg-query &>/dev/null; then
    log "Checking Debian/Ubuntu build dependencies..."
    required_packages=(
        build-essential curl wget file pkg-config
        libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev
        libssl-dev libudev-dev libayatana-appindicator3-dev
        librsvg2-dev patchelf
    )
    for package in "${required_packages[@]}"; do
        status="$(dpkg-query -W -f='${Status}' "$package" 2>/dev/null || true)"
        if [ "$status" != "install ok installed" ]; then
            missing_libs+=("$package")
        fi
    done

    if [ ${#missing_libs[@]} -gt 0 ]; then
        err "Missing system packages: ${missing_libs[*]}"
        echo ""
        echo "  Install with:"
        echo "  sudo apt install --no-install-recommends ${missing_libs[*]}"
        exit 1
    fi
else
    err "Unsupported package manager. Install the Tauri v2 Linux prerequisites for your distribution."
    exit 1
fi

ok "Prerequisites OK (cargo $(cargo --version 2>/dev/null | cut -d' ' -f2), node $(node --version 2>/dev/null))"

# ── Clean if requested ─────────────────────────────────────
if $CLEAN; then
    log "Cleaning build artifacts..."
    (cd "$TAURI_DIR" && cargo clean)
fi

# ── Install npm deps if needed ─────────────────────────────
if [ ! -d "$PROJECT_DIR/node_modules" ]; then
    log "Installing npm dependencies..."
    (cd "$PROJECT_DIR" && npm ci)
fi

# ── Build ──────────────────────────────────────────────────
log "Building Sidearm release AppImage..."
cd "$PROJECT_DIR"
npx tauri build --bundles appimage

echo ""
ok "Build complete! Output:"
echo ""

if [ -f "$TAURI_DIR/target/release/sidearm" ]; then
    size=$(du -h "$TAURI_DIR/target/release/sidearm" | cut -f1)
    ok "  Binary:   $TAURI_DIR/target/release/sidearm ($size)"
fi

for bundle in "$TAURI_DIR/target/release/bundle/appimage"/*.AppImage; do
    if [ -f "$bundle" ]; then
        size=$(du -h "$bundle" | cut -f1)
        ok "  AppImage: $bundle ($size)"
    fi
done
