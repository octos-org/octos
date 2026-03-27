#!/usr/bin/env bash
# install.sh — Install octos from pre-built binaries on a fresh machine.
# Self-contained: no repo clone, Rust, or Node.js needed.
#
# Usage:
#   curl -fsSL https://octos-cloud.org/install.sh | bash
#   curl -fsSL https://octos-cloud.org/install.sh | bash -s -- --tenant-name alice --frps-token <token>
#
# Options:
#   --tenant-name NAME       Tenant subdomain (e.g. "alice")
#   --frps-token TOKEN       frps auth token
#   --frps-server ADDR       frps server address (default: 163.192.33.32)
#   --ssh-port PORT          SSH tunnel remote port (default: 6001)
#   --auth-token TOKEN       Dashboard auth token (default: auto-generated)
#   --domain DOMAIN          Tunnel domain (default: octos-cloud.org)
#   --version TAG            Release version to install (default: latest)
#   --prefix DIR             Install prefix (default: ~/.octos/bin)
#   --no-tunnel              Skip frpc tunnel setup
#   --uninstall              Remove octos and frpc services and binaries

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────
GITHUB_REPO="octos-org/octos"
VERSION="latest"
PREFIX="${OCTOS_PREFIX:-$HOME/.octos/bin}"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"
FRPC_VERSION="0.61.1"

TENANT_NAME=""
FRPS_TOKEN=""
FRPS_SERVER="163.192.33.32"
SSH_PORT="6001"
AUTH_TOKEN=""
TUNNEL_DOMAIN="octos-cloud.org"
SKIP_TUNNEL=false
UNINSTALL=false

# ── Parse arguments ───────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --tenant-name)   TENANT_NAME="$2"; shift 2 ;;
        --frps-token)    FRPS_TOKEN="$2"; shift 2 ;;
        --frps-server)   FRPS_SERVER="$2"; shift 2 ;;
        --ssh-port)      SSH_PORT="$2"; shift 2 ;;
        --auth-token)    AUTH_TOKEN="$2"; shift 2 ;;
        --domain)        TUNNEL_DOMAIN="$2"; shift 2 ;;
        --version)       VERSION="$2"; shift 2 ;;
        --prefix)        PREFIX="$2"; shift 2 ;;
        --no-tunnel)     SKIP_TUNNEL=true; shift ;;
        --uninstall)     UNINSTALL=true; shift ;;
        --help|-h)
            sed -n '2,20s/^# //p' "$0"
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

# ── Uninstall ─────────────────────────────────────────────────────────
if [ "$UNINSTALL" = true ]; then
    section "Uninstalling octos"

    echo "    (sudo is needed to remove system services)"
    case "$OS" in
        Darwin)
            sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist 2>/dev/null || true
            sudo rm -f /Library/LaunchDaemons/io.octos.serve.plist
            sudo launchctl unload /Library/LaunchDaemons/io.octos.frpc.plist 2>/dev/null || true
            sudo rm -f /Library/LaunchDaemons/io.octos.frpc.plist
            # Clean up legacy LaunchAgents
            launchctl unload ~/Library/LaunchAgents/io.octos.octos-serve.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.octos.serve.plist 2>/dev/null || true
            launchctl unload ~/Library/LaunchAgents/io.octos.frpc.plist 2>/dev/null || true
            rm -f ~/Library/LaunchAgents/io.octos.*.plist
            ok "launchd services removed"
            ;;
        Linux)
            sudo systemctl stop octos-serve.service 2>/dev/null || true
            sudo systemctl disable octos-serve.service 2>/dev/null || true
            sudo rm -f /etc/systemd/system/octos-serve.service
            sudo systemctl stop frpc.service 2>/dev/null || true
            sudo systemctl disable frpc.service 2>/dev/null || true
            sudo rm -f /etc/systemd/system/frpc.service
            sudo systemctl daemon-reload 2>/dev/null || true
            ok "systemd services removed"
            ;;
    esac

    rm -rf "$PREFIX"
    sudo rm -f /usr/local/bin/frpc
    sudo rm -rf /etc/frp
    ok "binaries and config removed"

    echo ""
    echo "    Data directory ($DATA_DIR) was NOT removed. Delete manually if desired:"
    echo "      rm -rf $DATA_DIR"
    exit 0
fi

# ── Detect platform ──────────────────────────────────────────────────
section "Detecting platform"

case "$OS" in
    Darwin) PLATFORM="apple-darwin" ;;
    Linux)  PLATFORM="unknown-linux-gnu" ;;
    *)      err "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
    x86_64)        TRIPLE="x86_64-${PLATFORM}" ; FRP_ARCH="amd64" ;;
    aarch64|arm64) TRIPLE="aarch64-${PLATFORM}" ; FRP_ARCH="arm64" ;;
    *)             err "Unsupported architecture: $ARCH" ;;
esac

ok "$OS $ARCH ($TRIPLE)"

# ── Check / install runtime dependencies ─────────────────────────────
section "Checking runtime dependencies"

# git — required for skill installation
if command -v git &>/dev/null; then
    ok "git $(git --version | awk '{print $3}')"
else
    case "$OS" in
        Darwin)
            echo "    Installing Xcode Command Line Tools (provides git)..."
            xcode-select --install 2>/dev/null || true
            echo "    Follow the dialog to complete installation, then re-run this script."
            exit 1
            ;;
        Linux)
            err "git not found. Install with: sudo apt-get install -y git (or your package manager)"
            ;;
    esac
fi

# Node.js / npm — needed for skills with package.json (e.g. WhatsApp bridge, pptxgenjs)
if command -v node &>/dev/null; then
    ok "Node.js $(node --version)"
else
    warn "Node.js not found (recommended for some skills)"
    echo "    Install from https://nodejs.org or:"
    case "$OS" in
        Darwin) echo "      brew install node" ;;
        Linux)  echo "      curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt-get install -y nodejs" ;;
    esac
fi

# Chromium / Chrome — needed for browser tool and deep-crawl skill
CHROME_FOUND=false
for chrome_bin in "google-chrome" "google-chrome-stable" "chromium-browser" "chromium"; do
    if command -v "$chrome_bin" &>/dev/null; then
        ok "Browser: $chrome_bin"
        CHROME_FOUND=true
        break
    fi
done
if [ "$CHROME_FOUND" = false ] && [ "$OS" = "Darwin" ]; then
    # Check macOS app bundles
    for app in "/Applications/Google Chrome.app" "/Applications/Chromium.app"; do
        if [ -d "$app" ]; then
            ok "Browser: $app"
            CHROME_FOUND=true
            break
        fi
    done
fi
if [ "$CHROME_FOUND" = false ]; then
    warn "Chromium/Chrome not found (recommended for browser tool and deep-crawl skill)"
    case "$OS" in
        Darwin) echo "      brew install --cask google-chrome" ;;
        Linux)  echo "      sudo apt-get install -y chromium-browser" ;;
    esac
fi

# ffmpeg — optional, for media/voice skills
if command -v ffmpeg &>/dev/null; then
    ok "ffmpeg found"
else
    warn "ffmpeg not found (optional: media/voice skills)"
fi

# ── Resolve release version ──────────────────────────────────────────
section "Resolving release"

if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
    if [ -z "$VERSION" ]; then
        err "Could not determine latest release. Specify --version explicitly."
    fi
fi
ok "version: $VERSION"

# ── Download and install octos ────────────────────────────────────────
section "Installing octos"

TARBALL="octos-bundle-${TRIPLE}.tar.gz"
DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}/${TARBALL}"

echo "    Downloading $TARBALL..."
INSTALL_TMP=$(mktemp -d /tmp/octos-install.XXXXXX)
trap 'rm -rf "$INSTALL_TMP"' EXIT

if ! curl -fsSL -o "${INSTALL_TMP}/${TARBALL}" "$DOWNLOAD_URL"; then
    err "Download failed. Check that release $VERSION has a binary for $TRIPLE."
fi

tar -xzf "${INSTALL_TMP}/${TARBALL}" -C "$INSTALL_TMP"

mkdir -p "$PREFIX"
for bin in "$INSTALL_TMP"/*; do
    [ -f "$bin" ] || continue
    cp "$bin" "$PREFIX/"
    chmod +x "$PREFIX/$(basename "$bin")"
done
ok "binaries installed to $PREFIX"

# Sign on macOS
if [ "$OS" = "Darwin" ]; then
    for bin in "$PREFIX"/*; do
        codesign -s - "$bin" 2>/dev/null || true
    done
    ok "binaries signed (ad-hoc)"
fi

# Add to PATH if needed
if ! echo "$PATH" | grep -q "$PREFIX"; then
    warn "$PREFIX is not in your PATH"
    echo "    Add this to your shell profile:"
    echo "      export PATH=\"$PREFIX:\$PATH\""
fi

# ── Initialize octos workspace ────────────────────────────────────────
section "Initializing octos"

# Temporarily add PREFIX to PATH so octos init can run
export PATH="$PREFIX:$PATH"

if [ ! -d "$DATA_DIR" ]; then
    "$PREFIX/octos" init --defaults 2>/dev/null || "$PREFIX/octos" init 2>/dev/null || true
    ok "workspace initialized via octos init"
else
    ok "$DATA_DIR already exists (skipping init)"
fi

# Ensure required subdirectories exist (in case init didn't create them all)
mkdir -p "$DATA_DIR"/{profiles,memory,sessions,skills,logs,research,history}
if [ ! -f "$DATA_DIR/config.json" ]; then
    echo '{}' > "$DATA_DIR/config.json"
fi
ok "data directory: $DATA_DIR"

# ── Generate auth token ──────────────────────────────────────────────
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN=$(openssl rand -hex 32)
fi

# ── Set up octos serve as system service ──────────────────────────────
section "Setting up octos serve"

OCTOS_BIN="$PREFIX/octos"
PLIST_LABEL="io.octos.serve"

case "$OS" in
    Darwin)
        # Clean up legacy LaunchAgents (old names that conflict with port 8080)
        for LEGACY in \
            "$HOME/Library/LaunchAgents/io.octos.octos-serve.plist" \
            "$HOME/Library/LaunchAgents/io.octos.serve.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.crew-serve.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.ominix-api.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.octos-serve.plist"; do
            if [ -f "$LEGACY" ]; then
                launchctl unload "$LEGACY" 2>/dev/null || true
                rm -f "$LEGACY"
            fi
        done

        PLIST_FILE="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
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
        sudo launchctl load "$PLIST_FILE"
        ok "octos serve started via launchd"
        ;;

    Linux)
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
        echo "    (sudo is needed to install and start the system service)"
        sudo mv "$UNIT_TMP" "$UNIT_FILE"
        sudo systemctl daemon-reload
        sudo systemctl enable octos-serve
        sudo systemctl restart octos-serve
        ok "octos serve started via systemd"
        ;;
esac

# ── Verify octos serve ────────────────────────────────────────────────
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

# ── Tunnel setup (frpc) ──────────────────────────────────────────────
if [ "$SKIP_TUNNEL" = false ]; then
    section "Tunnel setup"

    # Prompt for missing inputs
    if [ -z "$TENANT_NAME" ]; then
        echo ""
        echo "    Enter the tenant subdomain (e.g. 'alice' for alice.${TUNNEL_DOMAIN}):"
        printf "    > "
        read -r TENANT_NAME < /dev/tty
        [ -z "$TENANT_NAME" ] && err "Tenant name is required for tunnel setup"
    fi

    if [ -z "$FRPS_TOKEN" ]; then
        TOKEN_FILE="$HOME/home/orcl-vps/frps-token.txt"
        if [ -f "$TOKEN_FILE" ]; then
            FRPS_TOKEN=$(cat "$TOKEN_FILE")
            echo "    frps token loaded from $TOKEN_FILE"
        else
            echo ""
            echo "    Enter the frps auth token (provided by your admin):"
            printf "    > "
            read -r FRPS_TOKEN < /dev/tty
            [ -z "$FRPS_TOKEN" ] && err "frps token is required for tunnel setup"
        fi
    fi

    # ── Confirm before proceeding ─────────────────────────────────────
    echo ""
    echo "    Tunnel configuration:"
    echo "      Tenant:       ${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "      frps server:  ${FRPS_SERVER}:7000"
    echo "      frps token:   ${FRPS_TOKEN:0:8}..."
    echo "      SSH port:     ${SSH_PORT}"
    echo "      Local port:   8080"
    echo ""
    echo "    Press Enter to continue, or Ctrl+C to abort."
    read -r < /dev/tty

    # ── Install frpc ──────────────────────────────────────────────────
    if [ ! -f /usr/local/bin/frpc ]; then
        echo "    Installing frpc v${FRPC_VERSION}..."

        case "$FRP_ARCH" in
            amd64|arm64) ;; # ok
            *) err "Unsupported frpc architecture: $FRP_ARCH" ;;
        esac

        FRP_OS=$(echo "$OS" | tr '[:upper:]' '[:lower:]')
        FRP_TARBALL="frp_${FRPC_VERSION}_${FRP_OS}_${FRP_ARCH}.tar.gz"
        FRP_URL="https://github.com/fatedier/frp/releases/download/v${FRPC_VERSION}/${FRP_TARBALL}"
        FRP_TMP=$(mktemp -d /tmp/frpc-install.XXXXXX)

        curl -fsSL -o "${FRP_TMP}/${FRP_TARBALL}" "$FRP_URL"
        tar -xzf "${FRP_TMP}/${FRP_TARBALL}" -C "$FRP_TMP"

        echo "    (sudo is needed to install frpc to /usr/local/bin)"
        sudo mkdir -p /usr/local/bin
        sudo cp "${FRP_TMP}/frp_${FRPC_VERSION}_${FRP_OS}_${FRP_ARCH}/frpc" /usr/local/bin/frpc
        sudo chmod 0755 /usr/local/bin/frpc
        rm -rf "$FRP_TMP"
        ok "frpc installed"
    else
        ok "frpc already installed ($(/usr/local/bin/frpc --version 2>/dev/null || echo 'unknown'))"
    fi

    # ── Write frpc config ─────────────────────────────────────────────
    FRPC_CONF_TMP=$(mktemp /tmp/frpc.toml.XXXXXX)
    cat > "$FRPC_CONF_TMP" << EOF
serverAddr = "${FRPS_SERVER}"
serverPort = 7000
auth.method = "token"
auth.token = "${FRPS_TOKEN}"
log.to = "/var/log/frpc.log"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "${TENANT_NAME}-web"
type = "http"
localPort = 8080
customDomains = ["${TENANT_NAME}.${TUNNEL_DOMAIN}"]

[[proxies]]
name = "${TENANT_NAME}-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = ${SSH_PORT}
EOF
    echo "    (sudo is needed to write config to /etc/frp)"
    sudo mkdir -p /etc/frp
    sudo mv "$FRPC_CONF_TMP" /etc/frp/frpc.toml
    sudo chmod 644 /etc/frp/frpc.toml
    ok "frpc config written to /etc/frp/frpc.toml"

    # ── Create frpc system service ────────────────────────────────────
    echo "    (sudo is needed to install the frpc system service)"
    case "$OS" in
        Darwin)
            FRPC_PLIST="/Library/LaunchDaemons/io.octos.frpc.plist"
            FRPC_PLIST_TMP=$(mktemp /tmp/io.octos.frpc.plist.XXXXXX)
            cat > "$FRPC_PLIST_TMP" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.frpc</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/frpc</string>
        <string>-c</string>
        <string>/etc/frp/frpc.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/frpc.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/frpc.log</string>
</dict>
</plist>
EOF
            # Clean up legacy LaunchAgent
            launchctl unload "$HOME/Library/LaunchAgents/io.octos.frpc.plist" 2>/dev/null || true
            rm -f "$HOME/Library/LaunchAgents/io.octos.frpc.plist"

            sudo launchctl unload "$FRPC_PLIST" 2>/dev/null || true
            sudo mv "$FRPC_PLIST_TMP" "$FRPC_PLIST"
            sudo chown root:wheel "$FRPC_PLIST"
            sudo chmod 644 "$FRPC_PLIST"
            sudo launchctl load "$FRPC_PLIST"
            ok "frpc started via launchd"
            ;;

        Linux)
            FRPC_UNIT="/etc/systemd/system/frpc.service"
            FRPC_UNIT_TMP=$(mktemp /tmp/frpc.service.XXXXXX)
            cat > "$FRPC_UNIT_TMP" << EOF
[Unit]
Description=frpc tunnel client for octos
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/frpc -c /etc/frp/frpc.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
            sudo mv "$FRPC_UNIT_TMP" "$FRPC_UNIT"
            sudo systemctl daemon-reload
            sudo systemctl enable frpc
            sudo systemctl restart frpc
            ok "frpc started via systemd"
            ;;
    esac

    # ── Verify tunnel ─────────────────────────────────────────────────
    section "Verifying tunnel"
    sleep 3
    if pgrep -x frpc > /dev/null 2>&1; then
        ok "frpc is running (PID: $(pgrep -x frpc))"
    else
        warn "frpc does not appear to be running"
        echo "    Check logs: tail -f /var/log/frpc.log"
    fi

    if curl -sf --max-time 3 "http://localhost:8080/api/status" > /dev/null 2>&1; then
        ok "octos serve is running on port 8080"
    else
        warn "octos serve is not responding on port 8080 (tunnel will retry once it starts)"
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────
section "Installation complete!"
echo ""
echo "    Binary:     $PREFIX/octos"
echo "    Data dir:   $DATA_DIR"
echo "    Config:     $DATA_DIR/config.json"
echo "    Auth token: $AUTH_TOKEN"
echo "    Logs:       tail -f $DATA_DIR/serve.log"
echo ""
echo "  Next steps:"
echo "    1. Set your API key:  export ANTHROPIC_API_KEY=sk-..."
echo "    2. Start chatting:    octos chat"
echo "    3. Open dashboard:    http://localhost:8080/admin/"
if [ -n "$TENANT_NAME" ]; then
    echo ""
    echo "  Tunnel:"
    echo "    Dashboard:  https://${TENANT_NAME}.${TUNNEL_DOMAIN}"
    echo "    SSH access: ssh -p ${SSH_PORT} $(whoami)@${TUNNEL_DOMAIN}"
fi
echo ""
echo "  Manage services:"
if [ "$OS" = "Darwin" ]; then
    echo "    Status:  sudo launchctl print system/io.octos.serve"
    echo "    Stop:    sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist"
    echo "    Start:   sudo launchctl load /Library/LaunchDaemons/io.octos.serve.plist"
else
    echo "    Status:  sudo systemctl status octos-serve"
    echo "    Stop:    sudo systemctl stop octos-serve"
    echo "    Start:   sudo systemctl start octos-serve"
fi
echo ""
