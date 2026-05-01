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
                   missing, using the platform's package manager. Mirrors
                   install.sh's --install-deps for source-checkout builds.
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

# Try to auto-install Node.js. Returns 0 only when npm is on PATH afterwards.
# Mirrors install.sh's pkg_hint approach so the prereqs stay consistent
# across the install scripts.
auto_install_node() {
    local cmd
    cmd="$(node_install_hint)"
    case "$cmd" in
        Install*) return 1 ;;  # The hint started with "Install ..." → no auto path.
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
