#!/usr/bin/env bash
# Local deployment for octos on macOS and Linux.
# Usage: ./scripts/local-tenant-deploy.sh [OPTIONS]
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
#
# Tunnel options (auto-enabled with --full, set up frpc to connect to VPS relay):
#   --no-tunnel              Skip frpc tunnel setup even in --full mode
#   --tenant-name NAME       Tenant subdomain (e.g. "alice")
#   --frps-token TOKEN       frps auth token
#   --frps-server ADDR       frps server address (default: 163.192.33.32)
#   --ssh-port PORT          SSH tunnel remote port (default: 6001)
#   --domain DOMAIN          Tunnel domain (default: octos-cloud.org)
#   --auth-token TOKEN       Dashboard auth token (default: auto-generated)
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

# Tunnel defaults
SKIP_TUNNEL=false
TENANT_NAME=""
FRPS_TOKEN=""
FRPS_SERVER="163.192.33.32"
SSH_PORT="6001"
AUTH_TOKEN=""
TUNNEL_DOMAIN="octos-cloud.org"

while [ $# -gt 0 ]; do
    case "$1" in
        --minimal)       MODE="minimal"; shift ;;
        --full)          MODE="full"; shift ;;
        --channels)      CHANNELS="${2:-}"; shift 2 ;;
        --no-skills)     BUILD_SKILLS=false; shift ;;
        --no-service)    SETUP_SERVICE=false; shift ;;
        --uninstall)     UNINSTALL=true; shift ;;
        --debug)         PROFILE="dev"; shift ;;
        --prefix)        PREFIX="${2:-$PREFIX}"; shift 2 ;;
        --no-tunnel)     SKIP_TUNNEL=true; shift ;;
        --tenant-name)   TENANT_NAME="$2"; shift 2 ;;
        --frps-token)    FRPS_TOKEN="$2"; shift 2 ;;
        --frps-server)   FRPS_SERVER="$2"; shift 2 ;;
        --ssh-port)      SSH_PORT="$2"; shift 2 ;;
        --auth-token)    AUTH_TOKEN="$2"; shift 2 ;;
        --domain)        TUNNEL_DOMAIN="$2"; shift 2 ;;
        --help|-h)
            sed -n '2,26s/^# //p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"; exit 1 ;;
    esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"

section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
err()     { echo "    ERROR: $1"; exit 1; }

detect_provider_defaults() {
    if [ -n "${OPENAI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="openai"; DETECTED_MODEL="gpt-4.1-mini"; DETECTED_ENV="OPENAI_API_KEY"
    elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
        DETECTED_PROVIDER="anthropic"; DETECTED_MODEL="claude-sonnet-4-20250514"; DETECTED_ENV="ANTHROPIC_API_KEY"
    elif [ -n "${GEMINI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="gemini"; DETECTED_MODEL="gemini-2.5-flash"; DETECTED_ENV="GEMINI_API_KEY"
    elif [ -n "${DEEPSEEK_API_KEY:-}" ]; then
        DETECTED_PROVIDER="deepseek"; DETECTED_MODEL="deepseek-chat"; DETECTED_ENV="DEEPSEEK_API_KEY"
    elif [ -n "${KIMI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="moonshot"; DETECTED_MODEL="kimi-k2.5"; DETECTED_ENV="KIMI_API_KEY"
    elif [ -n "${DASHSCOPE_API_KEY:-}" ]; then
        DETECTED_PROVIDER="dashscope"; DETECTED_MODEL="qwen3.5-plus"; DETECTED_ENV="DASHSCOPE_API_KEY"
    else
        DETECTED_PROVIDER="openai"; DETECTED_MODEL="gpt-4.1-mini"; DETECTED_ENV="OPENAI_API_KEY"
    fi
}

write_runtime_config() {
    local config_path="$DATA_DIR/config.json"
    local mode="local"
    detect_provider_defaults

    if { [ -n "$CLI_FEATURES" ] && [ "$SKIP_TUNNEL" = false ]; } || [ -n "$TENANT_NAME" ] || [ -n "$FRPS_TOKEN" ]; then
        mode="tenant"
    fi

    if [ -f "$config_path" ] && command -v python3 >/dev/null 2>&1; then
        python3 - "$config_path" "$mode" "$TUNNEL_DOMAIN" "$FRPS_SERVER" \
            "$DETECTED_PROVIDER" "$DETECTED_MODEL" "$DETECTED_ENV" <<'PYEOF'
import json
import pathlib
import sys

config_path = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
tunnel_domain = sys.argv[3]
frps_server = sys.argv[4]
provider = sys.argv[5]
model = sys.argv[6]
api_key_env = sys.argv[7]

data = {}
if config_path.exists():
    with config_path.open() as fh:
        data = json.load(fh)

data.setdefault("provider", provider)
data.setdefault("model", model)
data.setdefault("api_key_env", api_key_env)
data["mode"] = mode
if mode == "tenant":
    data["tunnel_domain"] = tunnel_domain
    data["frps_server"] = frps_server
else:
    data.pop("tunnel_domain", None)
    data.pop("frps_server", None)

config_path.write_text(json.dumps(data, indent=2) + "\n")
PYEOF
    elif [ ! -f "$config_path" ]; then
        local extra_config=""
        if [ "$mode" = "tenant" ]; then
            extra_config=$(cat <<EOF
,
  "tunnel_domain": "$TUNNEL_DOMAIN",
  "frps_server": "$FRPS_SERVER"
EOF
)
        fi
        cat > "$config_path" <<EOF
{
  "provider": "$DETECTED_PROVIDER",
  "model": "$DETECTED_MODEL",
  "api_key_env": "$DETECTED_ENV",
  "mode": "$mode"$extra_config
}
EOF
    else
        warn "python3 not found; leaving existing $config_path unchanged"
        return 0
    fi

    chmod 600 "$config_path"
    ok "runtime config: $config_path"
}

# ── Uninstall ─────────────────────────────────────────────────────────
if [ "$UNINSTALL" = true ]; then
    section "Uninstalling octos"

    # Stop and remove service
    echo "    (sudo is needed to remove the system service)"
    case "$OS" in
        Darwin)
            sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist 2>/dev/null || true
            sudo rm -f /Library/LaunchDaemons/io.octos.serve.plist
            # Also clean up legacy LaunchAgent if present
            launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist 2>/dev/null || true
            rm -f ~/Library/LaunchAgents/io.octos.octos-serve.plist
            ok "launchd service removed"
            ;;
        Linux)
            sudo systemctl stop octos-serve.service 2>/dev/null || true
            sudo systemctl disable octos-serve.service 2>/dev/null || true
            sudo rm -f /etc/systemd/system/octos-serve.service
            sudo systemctl daemon-reload 2>/dev/null || true
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

# ── Build dashboard ──────────────────────────────────────────────────
if [ -n "$CLI_FEATURES" ] && [[ "$CLI_FEATURES" == *"api"* ]]; then
    section "Building admin dashboard"
    if command -v npm &>/dev/null; then
        (cd "$ROOT/dashboard" && npm install && npm run build)
        ok "dashboard built to crates/crew-cli/static/admin/"
    else
        err "npm not found. Required to build the admin dashboard. Install Node.js first."
    fi
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

mkdir -p "$DATA_DIR"/{profiles,memory,sessions,skills,logs,research,history}
write_runtime_config
[ ! -f "$DATA_DIR/.gitignore" ] && cat > "$DATA_DIR/.gitignore" << 'EOF'
# Ignore task state and database files
tasks/
sessions/
*.redb
EOF
[ ! -f "$DATA_DIR/AGENTS.md" ] && printf '# Agent Instructions\n\nCustomize agent behavior and guidelines here.\n' > "$DATA_DIR/AGENTS.md"
[ ! -f "$DATA_DIR/SOUL.md" ]   && printf '# Soul — Who You Are\n\n## Core Principles\n\n- Help, don'\''t perform. Skip filler phrases — just do the thing.\n- Be resourceful. Come back with answers, not questions.\n- Have a voice. You can disagree and suggest alternatives.\n- Match the medium. Telegram gets concise replies. CLI gets detail.\n\n## Trust & Safety\n\n- Private things stay private.\n- External actions need care. Internal actions are yours.\n- Never send half-finished replies to messaging channels.\n' > "$DATA_DIR/SOUL.md"
[ ! -f "$DATA_DIR/USER.md" ]   && printf '# User Info\n\nAdd your information and preferences here.\n' > "$DATA_DIR/USER.md"
ok "data dir ready: $DATA_DIR"

# ── Service setup ─────────────────────────────────────────────────────
if [ "$SETUP_SERVICE" = true ] && [ -n "$CLI_FEATURES" ]; then
    section "Setting up background service"

    OCTOS_BIN="$PREFIX/octos"

    # Generate auth token if not provided
    if [ -z "$AUTH_TOKEN" ]; then
        AUTH_TOKEN=$(openssl rand -hex 32)
        echo "    Generated auth token: ${AUTH_TOKEN:0:8}..."
        echo "    (save this — needed to access the dashboard)"
    fi

    PLIST_LABEL="io.octos.serve"

    case "$OS" in
        Darwin)
            # Clean up any legacy LaunchAgent before installing LaunchDaemon
            for LEGACY_PLIST in \
                "$HOME/Library/LaunchAgents/io.octos.octos-serve.plist" \
                "$HOME/Library/LaunchAgents/io.octos.serve.plist" \
                "$HOME/Library/LaunchAgents/io.ominix.crew-serve.plist"; do
                if [ -f "$LEGACY_PLIST" ]; then
                    launchctl unload "$LEGACY_PLIST" 2>/dev/null || true
                    rm -f "$LEGACY_PLIST"
                    ok "removed legacy plist: $(basename "$LEGACY_PLIST")"
                fi
            done
            # launchd daemon (runs as root, survives logout)
            PLIST_FILE="/Library/LaunchDaemons/${PLIST_LABEL}.plist"

            # Write plist to temp file first, then sudo move it
            PLIST_TMP=$(mktemp /tmp/io.octos.serve.plist.XXXXXX)
            cat > "$PLIST_TMP" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>$OCTOS_BIN</string>
        <string>serve</string>
        <string>--port</string>
        <string>8080</string>
        <string>--host</string>
        <string>0.0.0.0</string>
        <string>--auth-token</string>
        <string>$AUTH_TOKEN</string>
    </array>
    <key>UserName</key>
    <string>$(whoami)</string>
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
        <key>HOME</key>
        <string>$HOME</string>
        <key>OCTOS_DATA_DIR</key>
        <string>$DATA_DIR</string>
        <key>OCTOS_AUTH_TOKEN</key>
        <string>$AUTH_TOKEN</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>$HOME</string>
</dict>
</plist>
EOF
            echo "    (sudo is needed to install and start the system service)"
            sudo launchctl unload "$PLIST_FILE" 2>/dev/null || true
            sudo mv "$PLIST_TMP" "$PLIST_FILE"
            sudo chown root:wheel "$PLIST_FILE"
            sudo chmod 644 "$PLIST_FILE"
            ok "LaunchDaemon plist written to $PLIST_FILE"

            # Start service
            sudo launchctl load "$PLIST_FILE"
            ok "octos serve started via launchd"
            ;;

        Linux)
            # systemd system unit (runs as current user, survives logout)
            UNIT_FILE="/etc/systemd/system/octos-serve.service"

            UNIT_TMP=$(mktemp /tmp/octos-serve.service.XXXXXX)
            cat > "$UNIT_TMP" << EOF
[Unit]
Description=octos serve (dashboard + gateway)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$(whoami)
ExecStart=$OCTOS_BIN serve --port 8080 --host 0.0.0.0 --auth-token $AUTH_TOKEN
Restart=on-failure
RestartSec=5
Environment=HOME=$HOME
Environment=OCTOS_DATA_DIR=$DATA_DIR
Environment=OCTOS_AUTH_TOKEN=$AUTH_TOKEN
Environment=PATH=$PREFIX:/usr/local/bin:/usr/bin:/bin
WorkingDirectory=$HOME

[Install]
WantedBy=multi-user.target
EOF
            echo "    (sudo is needed to install the system service)"
            sudo mv "$UNIT_TMP" "$UNIT_FILE"
            sudo systemctl daemon-reload
            sudo systemctl enable octos-serve
            sudo systemctl restart octos-serve
            ok "octos serve started via systemd"
            ;;
    esac

    # ── Verify octos is responding ────────────────────────────────────
    section "Verifying octos serve"
    RETRIES=10
    while [ $RETRIES -gt 0 ]; do
        if curl -sf --max-time 2 http://localhost:8080/admin/ > /dev/null 2>&1; then
            ok "octos serve is running on http://localhost:8080"
            break
        fi
        RETRIES=$((RETRIES - 1))
        sleep 1
    done
    if [ $RETRIES -eq 0 ]; then
        warn "octos serve did not respond within 10 seconds"
        echo "    Check logs: tail -f $DATA_DIR/serve.log"
    fi
else
    if [ "$SETUP_SERVICE" = true ]; then
        echo ""
        echo "    Service setup skipped (no features enabled — use --full or --channels)"
    fi
fi

# ── Tunnel setup (optional) ───────────────────────────────────────────
if [ -n "$CLI_FEATURES" ] && [ "$SKIP_TUNNEL" = false ]; then
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

    section "Tunnel setup — collecting configuration"

    # ── Prompt for missing required inputs ────────────────────────────
    if [ -z "$TENANT_NAME" ]; then
        echo ""
        echo "    Enter the tenant subdomain (e.g. 'alice' for alice.${TUNNEL_DOMAIN}):"
        printf "    > "
        read -r TENANT_NAME < /dev/tty
        [ -z "$TENANT_NAME" ] && err "Tenant name is required for tunnel setup"
    fi

    if [ -z "$FRPS_TOKEN" ]; then
        echo ""
        echo "    Enter the frps auth token from your registration email or setup command:"
        printf "    > "
        read -r FRPS_TOKEN < /dev/tty
        [ -z "$FRPS_TOKEN" ] && err "frps token is required for tunnel setup"
    fi

    # ── Show summary before proceeding ────────────────────────────────
    section "Tunnel setup — summary"
    echo ""
    echo "    Tenant:       ${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "    frps server:  ${FRPS_SERVER}:7000"
    echo "    frps token:   ${FRPS_TOKEN:0:8}..."
    echo "    SSH port:     ${SSH_PORT}"
    echo "    Local port:   8080"
    echo ""
    echo "    Press Enter to continue, or Ctrl+C to abort."
    read -r < /dev/tty

    # ── Run setup-frpc.sh locally ─────────────────────────────────────
    section "Setting up frpc tunnel"

    FRPC_ARGS=("$TENANT_NAME" "$FRPS_TOKEN")
    FRPC_ARGS+=(--server "$FRPS_SERVER")
    FRPC_ARGS+=(--domain "$TUNNEL_DOMAIN")
    FRPC_ARGS+=(--local-port 8080)
    FRPC_ARGS+=(--ssh-port "$SSH_PORT")

    "$SCRIPT_DIR/frp/setup-frpc.sh" "${FRPC_ARGS[@]}"
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
    echo "    3. Open browser:      http://localhost:8080/admin/"
    echo ""
    echo "  Auth token:   $AUTH_TOKEN"
    echo "  Logs:         tail -f $DATA_DIR/serve.log"
    case "$OS" in
        Darwin)
            echo "  Status:       sudo launchctl print system/io.octos.serve"
            echo "  Stop:         sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist"
            echo "  Start:        sudo launchctl load /Library/LaunchDaemons/io.octos.serve.plist"
            ;;
        Linux)
            echo "  Status:       sudo systemctl status octos-serve"
            echo "  Stop:         sudo systemctl stop octos-serve"
            echo "  Start:        sudo systemctl start octos-serve"
            ;;
    esac
fi
if [ -n "$TENANT_NAME" ]; then
    echo ""
    echo "  Tunnel:"
    echo "    Dashboard: https://${TENANT_NAME}.${TUNNEL_DOMAIN}"
fi
echo ""
