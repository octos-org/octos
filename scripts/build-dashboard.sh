#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DASHBOARD_DIR="$ROOT/dashboard"
OUT_DIR="$ROOT/crates/octos-cli/static/admin"

INSTALL_DEPS=false
while [ $# -gt 0 ]; do
    case "$1" in
        --install-deps) INSTALL_DEPS=true; shift ;;
        --help|-h)
            cat <<EOF
Usage: $(basename "$0") [--install-deps]

Builds the dashboard SPA into $OUT_DIR.

  --install-deps   Install Node.js (which provides npm) automatically when
                   missing. Uses the platform's package manager when one is
                   available (brew on macOS, apt/dnf/yum/pacman/apk on Linux),
                   and falls back to the official Node.js .pkg installer
                   (sudo) on macOS without Homebrew.
EOF
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Print the platform-appropriate command to install Node.js.
node_install_hint() {
    case "$(uname -s)" in
        Darwin)
            if command -v brew >/dev/null 2>&1; then
                echo "brew install node"
            else
                echo "Install Node.js from https://nodejs.org/en/download or install Homebrew first (https://brew.sh) and run 'brew install node'"
            fi
            ;;
        Linux)
            if command -v apt-get >/dev/null 2>&1; then
                echo "curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt-get install -y nodejs"
            elif command -v dnf >/dev/null 2>&1; then
                echo "sudo dnf install -y nodejs npm"
            elif command -v yum >/dev/null 2>&1; then
                echo "sudo yum install -y nodejs npm"
            elif command -v pacman >/dev/null 2>&1; then
                echo "sudo pacman -S --noconfirm nodejs npm"
            elif command -v apk >/dev/null 2>&1; then
                echo "sudo apk add nodejs npm"
            else
                echo "Install Node.js using your distro package manager, or from https://nodejs.org/en/download"
            fi
            ;;
        *)
            echo "Install Node.js from https://nodejs.org/en/download"
            ;;
    esac
}

# Direct-from-nodejs.org fallback for macOS without Homebrew. Downloads
# the official LTS .pkg installer and installs system-wide via
# `sudo installer`. Pinning a known-good LTS keeps this deterministic;
# bump as needed when the version is EOL'd.
install_node_macos_pkg() {
    local node_version="v22.12.0"
    local arch_name
    case "$(uname -m)" in
        arm64)         arch_name="arm64" ;;
        x86_64|amd64)  arch_name="x64" ;;
        *) echo "ERROR: unsupported macOS arch $(uname -m) for Node.js .pkg" >&2; return 1 ;;
    esac
    local url="https://nodejs.org/dist/${node_version}/node-${node_version}-darwin-${arch_name}.pkg"
    local pkg
    pkg=$(mktemp /tmp/node.XXXXXX.pkg)
    echo "==> Downloading Node.js ${node_version} (${arch_name}) from nodejs.org"
    if ! curl -fsSL --max-time 180 "$url" -o "$pkg"; then
        rm -f "$pkg"
        return 1
    fi
    echo "==> Installing Node.js (sudo will prompt)"
    if ! sudo installer -pkg "$pkg" -target /; then
        rm -f "$pkg"
        return 1
    fi
    rm -f "$pkg"
    # The .pkg lays down /usr/local/bin/node + /usr/local/bin/npm; both
    # should be on PATH already on a default macOS shell.
    command -v npm >/dev/null 2>&1
}

# Try to auto-install Node.js. Returns 0 only when npm is on PATH afterwards.
# Mirrors install.sh's pkg_hint approach so the prereqs stay consistent
# across the install scripts.
auto_install_node() {
    local cmd
    cmd="$(node_install_hint)"
    case "$cmd" in
        Install*)
            # No package manager hint available. On macOS without Homebrew
            # we can still install directly from the official .pkg — that
            # path needs sudo but doesn't require any prior tooling.
            if [ "$(uname -s)" = "Darwin" ]; then
                install_node_macos_pkg && return 0
            fi
            return 1
            ;;
    esac
    echo "==> Installing Node.js"
    echo "    $cmd"
    eval "$cmd"
    command -v npm >/dev/null 2>&1
}

if ! command -v npm >/dev/null 2>&1; then
    if [ "$INSTALL_DEPS" = true ]; then
        if ! auto_install_node; then
            echo "ERROR: --install-deps was set but Node.js could not be installed automatically" >&2
            echo "       Install it manually:" >&2
            echo "       $(node_install_hint)" >&2
            exit 1
        fi
    else
        echo "npm is required to build dashboard assets" >&2
        echo "" >&2
        echo "Install Node.js for your platform:" >&2
        echo "    $(node_install_hint)" >&2
        echo "" >&2
        echo "Or re-run this script with --install-deps to auto-install." >&2
        exit 1
    fi
fi

cd "$DASHBOARD_DIR"

if [ ! -d node_modules ]; then
    echo "Installing dashboard dependencies..."
    npm ci
fi

echo "Building dashboard into $OUT_DIR"
VITE_OUT_DIR="$OUT_DIR" VITE_BASE_PATH="/admin/" npm run build

echo "Dashboard assets synced to $OUT_DIR"
