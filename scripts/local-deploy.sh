#!/usr/bin/env bash
# Local deployment for octos on macOS and Linux.
# Usage: ./scripts/local-deploy.sh [OPTIONS]
#
# Options:
#   --minimal          CLI + chat only (no channels, no dashboard)
#   --full             All channels + dashboard + app-skills
#   --channels LIST    Comma-separated channels (telegram,discord,slack,whatsapp,feishu,email,twilio,wecom)
#   --no-skills        Skip building app-skills
#   --no-service       Skip launchd/systemd service setup
#   --uninstall        Remove binaries and service files
#   --debug            Build in debug mode (faster, larger binary)
#   --prefix DIR       Install prefix (default: ~/.cargo/bin)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ── Defaults ──────────────────────────────────────────────────────────
MODE="minimal"
CHANNELS=""
BUILD_SKILLS=true
SETUP_SERVICE=true
UNINSTALL=false
PROFILE="release"
PREFIX="${CARGO_HOME:-$HOME/.cargo}/bin"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"

for arg in "$@"; do
    case "$arg" in
        --minimal)     MODE="minimal" ;;
        --full)        MODE="full" ;;
        --channels)    shift; CHANNELS="${1:-}" ;;
        --no-skills)   BUILD_SKILLS=false ;;
        --no-service)  SETUP_SERVICE=false ;;
        --uninstall)   UNINSTALL=true ;;
        --debug)       PROFILE="dev" ;;
        --prefix)      shift; PREFIX="${1:-$PREFIX}" ;;
        --help|-h)
            sed -n '2,14s/^# //p' "$0"
            exit 0
            ;;
    esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"

section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
err()     { echo "    ERROR: $1"; exit 1; }

# ── Uninstall ─────────────────────────────────────────────────────────
if [ "$UNINSTALL" = true ]; then
    section "Uninstalling octos"

    # Stop and remove service
    case "$OS" in
        Darwin)
            launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist 2>/dev/null || true
            rm -f ~/Library/LaunchAgents/io.octos.octos-serve.plist
            ok "launchd service removed"
            ;;
        Linux)
            systemctl --user stop octos-serve.service 2>/dev/null || true
            systemctl --user disable octos-serve.service 2>/dev/null || true
            rm -f ~/.config/systemd/user/octos-serve.service
            systemctl --user daemon-reload 2>/dev/null || true
            ok "systemd service removed"
            ;;
    esac

    # Remove binaries
    BINS=(octos news_fetch deep-search deep_crawl send_email account_manager clock weather)
    for bin in "${BINS[@]}"; do
        rm -f "$PREFIX/$bin"
    done
    ok "binaries removed from $PREFIX"

    echo ""
    echo "Binaries and service files removed."
    echo "Data directory ($DATA_DIR) was NOT removed. Delete manually if desired:"
    echo "  rm -rf $DATA_DIR"
    exit 0
fi

# ── Prerequisites ─────────────────────────────────────────────────────
section "Checking prerequisites"

# Rust toolchain
if ! command -v cargo &>/dev/null; then
    err "Rust not found. Install from https://rustup.rs"
fi
RUST_VER=$(rustc --version | awk '{print $2}')
ok "Rust $RUST_VER ($ARCH)"

# Platform-specific deps
case "$OS" in
    Darwin)
        ok "macOS $(sw_vers -productVersion 2>/dev/null || echo 'unknown')"
        ;;
    Linux)
        ok "Linux $(uname -r)"
        if ! command -v pkg-config &>/dev/null; then
            warn "pkg-config not found (may be needed for some features)"
        fi
        ;;
    *)
        err "Unsupported OS: $OS (use WSL2 on Windows)"
        ;;
esac

# Optional deps
if command -v node &>/dev/null; then
    ok "Node.js $(node --version) (for WhatsApp bridge, pptxgenjs)"
else
    warn "Node.js not found (optional: WhatsApp bridge, pptxgenjs)"
fi

if command -v ffmpeg &>/dev/null; then
    ok "ffmpeg found (for media skills)"
else
    warn "ffmpeg not found (optional: media skills)"
fi

# ── Resolve features ─────────────────────────────────────────────────
section "Resolving build configuration"

CLI_FEATURES=""
case "$MODE" in
    minimal)
        CLI_FEATURES=""
        BUILD_SKILLS=false
        echo "    Mode: minimal (CLI + chat only)"
        ;;
    full)
        CLI_FEATURES="api,telegram,discord,slack,whatsapp,feishu,email,twilio,wecom"
        echo "    Mode: full (all channels + dashboard + skills)"
        ;;
esac

# Override with explicit --channels
if [ -n "$CHANNELS" ]; then
    if [ -n "$CLI_FEATURES" ]; then
        CLI_FEATURES="$CLI_FEATURES,$CHANNELS"
    else
        CLI_FEATURES="$CHANNELS"
    fi
fi

# Always include api if any channel is set (needed for dashboard)
if [ -n "$CLI_FEATURES" ] && [[ "$CLI_FEATURES" != *"api"* ]]; then
    CLI_FEATURES="api,$CLI_FEATURES"
fi

if [ -n "$CLI_FEATURES" ]; then
    echo "    Features: $CLI_FEATURES"
else
    echo "    Features: (none — CLI only)"
fi

# ── Build ─────────────────────────────────────────────────────────────
section "Building octos"

INSTALL_FLAG=""
BUILD_FLAG=""
if [ "$PROFILE" = "dev" ]; then
    INSTALL_FLAG="--debug"
    BUILD_FLAG=""
else
    # cargo install defaults to --release; passing it explicitly errors on newer Rust
    INSTALL_FLAG=""
    BUILD_FLAG="--release"
fi

if [ -n "$CLI_FEATURES" ]; then
    echo "    cargo install octos-cli with features: $CLI_FEATURES"
    cargo install --path crates/octos-cli --features "$CLI_FEATURES" $INSTALL_FLAG
else
    echo "    cargo install octos-cli (no extra features)"
    cargo install --path crates/octos-cli $INSTALL_FLAG
fi
ok "octos binary installed to $PREFIX/octos"

# App-skills
if [ "$BUILD_SKILLS" = true ]; then
    section "Building app-skills"
    SKILL_CRATES=(news_fetch deep-search deep-crawl send-email account-manager clock weather)
    for crate in "${SKILL_CRATES[@]}"; do
        echo "    Building $crate..."
        cargo build $BUILD_FLAG -p "$crate" 2>&1 | tail -1
    done

    # Copy skill binaries to install prefix
    SKILL_BINS=(news_fetch deep-search deep_crawl send_email account_manager clock weather)
    BIN_DIR="target/release"
    [ "$PROFILE" = "dev" ] && BIN_DIR="target/debug"
    for bin in "${SKILL_BINS[@]}"; do
        if [ -f "$BIN_DIR/$bin" ]; then
            cp "$BIN_DIR/$bin" "$PREFIX/$bin"
        fi
    done
    ok "app-skill binaries copied to $PREFIX"

    # Sign on macOS
    if [ "$OS" = "Darwin" ]; then
        for bin in "${SKILL_BINS[@]}"; do
            codesign -s - "$PREFIX/$bin" 2>/dev/null || true
        done
        ok "binaries signed (ad-hoc)"
    fi
fi

# ── Initialize ────────────────────────────────────────────────────────
section "Initializing octos workspace"

if [ ! -d "$DATA_DIR" ]; then
    "$PREFIX/octos" init --defaults 2>/dev/null || "$PREFIX/octos" init 2>/dev/null || true
    ok "created $DATA_DIR"
else
    ok "$DATA_DIR already exists (skipping init)"
fi

# ── Service setup ─────────────────────────────────────────────────────
if [ "$SETUP_SERVICE" = true ] && [ -n "$CLI_FEATURES" ]; then
    section "Setting up background service"

    OCTOS_BIN="$PREFIX/octos"

    case "$OS" in
        Darwin)
            # launchd plist
            PLIST_DIR="$HOME/Library/LaunchAgents"
            PLIST_FILE="$PLIST_DIR/io.octos.octos-serve.plist"
            mkdir -p "$PLIST_DIR"

            cat > "$PLIST_FILE" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.octos-serve</string>
    <key>ProgramArguments</key>
    <array>
        <string>$OCTOS_BIN</string>
        <string>serve</string>
        <string>--port</string>
        <string>8080</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$DATA_DIR/serve.log</string>
    <key>StandardErrorPath</key>
    <string>$DATA_DIR/serve.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$PREFIX:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>OCTOS_HOME</key>
        <string>$DATA_DIR</string>
    </dict>
</dict>
</plist>
EOF
            ok "launchd plist written to $PLIST_FILE"
            echo "    To start:  launchctl load $PLIST_FILE"
            echo "    To stop:   launchctl unload $PLIST_FILE"
            echo "    Logs:      tail -f $DATA_DIR/serve.log"
            ;;

        Linux)
            # systemd user unit
            UNIT_DIR="$HOME/.config/systemd/user"
            UNIT_FILE="$UNIT_DIR/octos-serve.service"
            mkdir -p "$UNIT_DIR"

            cat > "$UNIT_FILE" << EOF
[Unit]
Description=octos serve (dashboard + gateway)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$OCTOS_BIN serve --port 8080
Restart=on-failure
RestartSec=5
Environment=OCTOS_HOME=$DATA_DIR
Environment=PATH=$PREFIX:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=default.target
EOF
            systemctl --user daemon-reload
            ok "systemd unit written to $UNIT_FILE"
            echo "    To start:  systemctl --user start octos-serve"
            echo "    To enable: systemctl --user enable octos-serve"
            echo "    To stop:   systemctl --user stop octos-serve"
            echo "    Logs:      journalctl --user -u octos-serve -f"
            ;;
    esac
else
    if [ "$SETUP_SERVICE" = true ]; then
        echo ""
        echo "    Service setup skipped (no features enabled — use --full or --channels)"
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────
section "Deployment complete"
echo ""
echo "    Binary:     $PREFIX/octos"
echo "    Data dir:   $DATA_DIR"
echo "    Config:     $DATA_DIR/config.json"
echo ""
echo "  Next steps:"
echo "    1. Set your API key:  export ANTHROPIC_API_KEY=sk-..."
echo "    2. Start chatting:    octos chat"
if [ -n "$CLI_FEATURES" ]; then
    echo "    3. Start dashboard:   octos serve"
    echo "    4. Open browser:      http://localhost:8080/admin/"
fi
echo ""
