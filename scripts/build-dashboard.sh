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
# the official universal .pkg installer and installs system-wide via
# `sudo installer`.
#
# nodejs.org ships a single universal .pkg per release at
# `node-v${version}.pkg` (no arch suffix). The arch-suffixed variants
# listed in `index.json::files` (e.g. `osx-x64-pkg`) are not always
# present at the URL pattern that name implies, and there is no
# `-darwin-arm64.pkg` at all — Apple Silicon-only is shipped as a
# tarball. The unprefixed .pkg is universal (Intel + Apple Silicon)
# and installs cleanly on both, so we use that.
#
# Version is fetched from index.json so the latest LTS is always used.
# A hardcoded fallback is kept for offline/restricted hosts where the
# index lookup fails.
install_node_macos_pkg() {
    local fallback_version="v24.15.0"
    local node_version
    # Discover the latest LTS via index.json. Pure bash (no jq/python
    # dependency): split the array entries by `},{`, take the first
    # record whose `"lts"` field is a non-false string, extract its
    # version.
    node_version=$(curl -fsSL --max-time 30 https://nodejs.org/dist/index.json 2>/dev/null \
        | sed 's/},{/},\
{/g' \
        | grep -m1 '"lts":"[A-Z]' \
        | grep -oE '"v[0-9]+\.[0-9]+\.[0-9]+"' \
        | head -1 \
        | tr -d '"')
    if [ -z "$node_version" ]; then
        echo "    Could not query nodejs.org for the latest LTS; falling back to ${fallback_version}"
        node_version="$fallback_version"
    fi
    local url="https://nodejs.org/dist/${node_version}/node-${node_version}.pkg"
    local pkg
    pkg=$(mktemp /tmp/node.XXXXXX.pkg)
    echo "==> Downloading Node.js ${node_version} (universal .pkg) from nodejs.org"
    if ! curl -fsSL --max-time 180 "$url" -o "$pkg"; then
        echo "ERROR: Node.js .pkg download failed (${url})" >&2
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
