#!/usr/bin/env bash
# bootstrap-tenant.sh — Full admin onboarding: create tenant + deploy octos + frpc
# to a Mac Mini, resulting in a working dashboard at {subdomain}.octos-cloud.org.
#
# Run from the admin machine (where octos repo is checked out).
# Idempotent: safe to re-run.
#
# Usage:
#   ./scripts/frp/bootstrap-tenant.sh <name> <user@host> [options]
#
# Examples:
#   ./scripts/frp/bootstrap-tenant.sh alice cloud@10.0.1.50 --password s3cret
#   ./scripts/frp/bootstrap-tenant.sh bob cloud@192.168.1.100 --key ~/.ssh/id_ed25519
#
# Options:
#   --password <pw>       SSH password auth (requires sshpass)
#   --key <keyfile>       SSH key auth
#   --serve-port <port>   octos serve port on Mini (default: 8080)
#   --domain <domain>     Base tunnel domain (default: octos-cloud.org)
#   --server <addr>       frps VPS address (default: 163.192.33.32)
#   --frps-port <port>    frps control port (default: 7000)
#   --frps-token <tok>    per-tenant tunnel token (default: read from ~/home/orcl-vps/frps-token.txt)
#   --auth-token <tok>    Dashboard auth token (default: auto-generated)
#   --skip-build          Skip local cargo build (use existing binaries)
#   --skip-tenant         Skip tenant creation (already exists)
#   --data-dir <path>     Local octos data dir for tenant store (default: ~/.octos)

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────
SERVE_PORT=8080
DOMAIN="octos-cloud.org"
FRPS_SERVER="163.192.33.32"
FRPS_PORT=7000
FRPS_TOKEN=""
AUTH_TOKEN=""
SSH_AUTH_TYPE=""
SSH_AUTH_VAL=""
SKIP_BUILD=false
SKIP_TENANT=false
LOCAL_DATA_DIR=""
FRPC_VERSION="0.65.0"
PLIST_LABEL="io.octos.serve"
PLIST_FRPC="io.octos.frpc"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Parse arguments ───────────────────────────────────────────────────
if [ $# -lt 2 ]; then
    echo "Usage: $0 <tenant-name> <user@host> [options]"
    echo ""
    echo "Options:"
    echo "  --password <pw>       SSH password auth"
    echo "  --key <keyfile>       SSH key auth"
    echo "  --serve-port <port>   octos serve port (default: 8080)"
    echo "  --domain <domain>     Tunnel domain (default: octos-cloud.org)"
    echo "  --server <addr>       frps address (default: 163.192.33.32)"
    echo "  --frps-token <tok>    per-tenant tunnel token"
    echo "  --auth-token <tok>    Dashboard auth token"
    echo "  --skip-build          Use existing binaries"
    echo "  --skip-tenant         Tenant already created"
    exit 1
fi

TENANT_NAME="$1"
SSH_TARGET="$2"
shift 2

while [ $# -gt 0 ]; do
    case "$1" in
        --password)     SSH_AUTH_TYPE="password"; SSH_AUTH_VAL="$2"; shift 2 ;;
        --key)          SSH_AUTH_TYPE="key"; SSH_AUTH_VAL="$2"; shift 2 ;;
        --serve-port)   SERVE_PORT="$2"; shift 2 ;;
        --domain)       DOMAIN="$2"; shift 2 ;;
        --server)       FRPS_SERVER="$2"; shift 2 ;;
        --frps-port)    FRPS_PORT="$2"; shift 2 ;;
        --frps-token)   FRPS_TOKEN="$2"; shift 2 ;;
        --auth-token)   AUTH_TOKEN="$2"; shift 2 ;;
        --skip-build)   SKIP_BUILD=true; shift ;;
        --skip-tenant)  SKIP_TENANT=true; shift ;;
        --data-dir)     LOCAL_DATA_DIR="$2"; shift 2 ;;
        *)              echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Resolve frps token
if [ -z "$FRPS_TOKEN" ]; then
    TOKEN_FILE="$HOME/home/orcl-vps/frps-token.txt"
    if [ -f "$TOKEN_FILE" ]; then
        FRPS_TOKEN=$(cat "$TOKEN_FILE")
    else
        echo "ERROR: No --frps-token and $TOKEN_FILE not found"
        exit 1
    fi
fi

# Generate dashboard auth token if not provided
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN=$(openssl rand -hex 32)
fi

# ── SSH helper ────────────────────────────────────────────────────────
ssh_cmd() {
    case "$SSH_AUTH_TYPE" in
        password)
            sshpass -p "$SSH_AUTH_VAL" ssh -o StrictHostKeyChecking=no "$SSH_TARGET" "$@" ;;
        key)
            ssh -i "$SSH_AUTH_VAL" -o StrictHostKeyChecking=no "$SSH_TARGET" "$@" ;;
        *)
            ssh -o StrictHostKeyChecking=no "$SSH_TARGET" "$@" ;;
    esac
}

scp_cmd() {
    case "$SSH_AUTH_TYPE" in
        password)
            sshpass -p "$SSH_AUTH_VAL" scp -o StrictHostKeyChecking=no "$@" ;;
        key)
            scp -i "$SSH_AUTH_VAL" -o StrictHostKeyChecking=no "$@" ;;
        *)
            scp -o StrictHostKeyChecking=no "$@" ;;
    esac
}

echo "============================================================"
echo "  Bootstrapping tenant: ${TENANT_NAME}"
echo "  Target: ${SSH_TARGET}"
echo "  Dashboard: http://${TENANT_NAME}.${DOMAIN}"
echo "============================================================"

# ── Step 1: Create tenant ─────────────────────────────────────────────
if [ "$SKIP_TENANT" = false ]; then
    echo ""
    echo "==> Step 1: Creating tenant..."
    DATA_DIR_ARGS=""
    if [ -n "$LOCAL_DATA_DIR" ]; then
        DATA_DIR_ARGS="--data-dir $LOCAL_DATA_DIR"
    fi
    # shellcheck disable=SC2086
    cargo run -p octos-cli --quiet -- admin create-tenant \
        --name "$TENANT_NAME" \
        --domain "$DOMAIN" \
        --server "$FRPS_SERVER" \
        --port "$FRPS_PORT" \
        --local-port "$SERVE_PORT" \
        --auth-token "$AUTH_TOKEN" \
        $DATA_DIR_ARGS 2>&1 || {
        echo "    Tenant may already exist, continuing..."
    }
else
    echo ""
    echo "==> Step 1: Skipping tenant creation (--skip-tenant)"
fi

# ── Step 2: Build octos binaries ──────────────────────────────────────
BINARIES=(octos news_fetch deep-search deep_crawl send_email account_manager clock weather)

if [ "$SKIP_BUILD" = false ]; then
    echo ""
    echo "==> Step 2: Building octos (release, all features)..."
    (cd "$REPO_ROOT" && cargo build --release -p octos-cli \
        --features "api,telegram,whatsapp,feishu,twilio,wecom" 2>&1 | tail -3)

    # Also build app-skills
    (cd "$REPO_ROOT" && cargo build --release \
        -p news -p deep-search -p deep-crawl -p send-email -p account-manager \
        -p clock -p weather 2>&1 | tail -3)
    echo "    Build complete"
else
    echo ""
    echo "==> Step 2: Skipping build (--skip-build)"
fi

# ── Step 3: Verify SSH access ─────────────────────────────────────────
echo ""
echo "==> Step 3: Verifying SSH access..."
read -r REMOTE_HOME REMOTE_OS REMOTE_ARCH <<< "$(ssh_cmd 'echo $HOME $(uname -s) $(uname -m)')"
echo "    Connected: ${SSH_TARGET} (${REMOTE_OS}/${REMOTE_ARCH}, home=${REMOTE_HOME})"

RBIN="${REMOTE_HOME}/.cargo/bin"
RDATA="${REMOTE_HOME}/.octos"

# ── Step 4: Upload binaries ───────────────────────────────────────────
echo ""
echo "==> Step 4: Uploading binaries..."
ssh_cmd "mkdir -p ${RBIN} ${RDATA}"

for bin in "${BINARIES[@]}"; do
    LOCAL_BIN="${REPO_ROOT}/target/release/${bin}"
    if [ -f "$LOCAL_BIN" ]; then
        echo "    Uploading ${bin}..."
        scp_cmd "$LOCAL_BIN" "${SSH_TARGET}:/tmp/${bin}.new"
        ssh_cmd "mv /tmp/${bin}.new '${RBIN}/${bin}' && chmod +x '${RBIN}/${bin}'"
        # codesign on macOS
        if [ "$REMOTE_OS" = "Darwin" ]; then
            ssh_cmd "codesign --force -s - '${RBIN}/${bin}' 2>/dev/null || true"
        fi
    fi
done
echo "    Binaries uploaded"

# ── Step 5: Initialize octos data directory ───────────────────────────
echo ""
echo "==> Step 5: Initializing octos data..."
ssh_cmd "mkdir -p ${RDATA}/{profiles,memory,sessions,skills,logs,research,history}"

# Write a minimal config.json if it doesn't exist (user configures via dashboard)
ssh_cmd "test -f ${RDATA}/config.json || cat > ${RDATA}/config.json" << 'EOF'
{}
EOF
echo "    Data directory initialized"

# ── Step 6: Install frpc ─────────────────────────────────────────────
echo ""
echo "==> Step 6: Installing frpc..."

case "$REMOTE_ARCH" in
    x86_64)       FRP_ARCH="amd64" ;;
    aarch64|arm64) FRP_ARCH="arm64" ;;
    *)            echo "Unsupported arch: $REMOTE_ARCH"; exit 1 ;;
esac

FRP_OS=$(echo "$REMOTE_OS" | tr '[:upper:]' '[:lower:]')
FRPC_INSTALLED=$(ssh_cmd "/usr/local/bin/frpc --version 2>/dev/null || echo ''")

if [ -z "$FRPC_INSTALLED" ]; then
    echo "    Downloading frpc v${FRPC_VERSION}..."
    TARBALL="frp_${FRPC_VERSION}_${FRP_OS}_${FRP_ARCH}.tar.gz"
    ssh_cmd "
        cd /tmp
        curl -fsSL -o frp.tar.gz \
            https://github.com/fatedier/frp/releases/download/v${FRPC_VERSION}/${TARBALL}
        tar -xzf frp.tar.gz
        sudo install -m 0755 frp_${FRPC_VERSION}_${FRP_OS}_${FRP_ARCH}/frpc /usr/local/bin/frpc
        rm -rf frp.tar.gz frp_${FRPC_VERSION}_${FRP_OS}_${FRP_ARCH}
    "
    echo "    frpc installed"
else
    echo "    frpc already installed (v${FRPC_INSTALLED})"
fi

# ── Step 7: Write frpc config ─────────────────────────────────────────
echo ""
echo "==> Step 7: Writing frpc config..."

# Get SSH port from tenant JSON file
TENANT_JSON="${LOCAL_DATA_DIR:-$HOME/.octos}/tenants/${TENANT_NAME}.json"
if [ -f "$TENANT_JSON" ]; then
    SSH_PORT=$(python3 -c "import json; print(json.load(open('$TENANT_JSON'))['ssh_port'])" 2>/dev/null || echo "6001")
else
    SSH_PORT="6001"
    echo "    WARNING: tenant JSON not found at $TENANT_JSON, defaulting SSH port to 6001"
fi

ssh_cmd "sudo mkdir -p /etc/frp && sudo tee /etc/frp/frpc.toml > /dev/null" << EOF
# frpc config for ${TENANT_NAME}.${DOMAIN}
# Managed by bootstrap-tenant.sh

serverAddr = "${FRPS_SERVER}"
serverPort = ${FRPS_PORT}

auth.method = "token"
auth.token = ""
metadatas.token = "${FRPS_TOKEN}"

log.to = "/var/log/frpc.log"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "${TENANT_NAME}-web"
type = "http"
localPort = ${SERVE_PORT}
customDomains = ["${TENANT_NAME}.${DOMAIN}"]

[[proxies]]
name = "${TENANT_NAME}-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = ${SSH_PORT}
EOF
echo "    frpc config written"

# ── Step 8: Create launchd/systemd services ───────────────────────────
echo ""
echo "==> Step 8: Creating services..."

if [ "$REMOTE_OS" = "Darwin" ]; then
    # --- octos serve launchd plist ---
    ssh_cmd "mkdir -p ~/Library/LaunchAgents"
    ssh_cmd "cat > ~/Library/LaunchAgents/${PLIST_LABEL}.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${RBIN}/octos</string>
        <string>serve</string>
        <string>--port</string>
        <string>${SERVE_PORT}</string>
        <string>--host</string>
        <string>0.0.0.0</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
    <key>StandardErrorPath</key>
    <string>/dev/null</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>${RBIN}:${REMOTE_HOME}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
        <key>HOME</key>
        <string>${REMOTE_HOME}</string>
        <key>OCTOS_DATA_DIR</key>
        <string>${RDATA}</string>
        <key>OCTOS_AUTH_TOKEN</key>
        <string>${AUTH_TOKEN}</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>${REMOTE_HOME}</string>
</dict>
</plist>
EOF
    echo "    octos serve plist written"

    # --- frpc launchd plist ---
    ssh_cmd "cat > ~/Library/LaunchAgents/${PLIST_FRPC}.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_FRPC}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/frpc</string>
        <string>-c</string>
        <string>/etc/frp/frpc.toml</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/frpc.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/frpc.log</string>
</dict>
</plist>
EOF
    echo "    frpc plist written"

    # --- Start services ---
    echo "    Starting services..."
    ssh_cmd "launchctl unload ~/Library/LaunchAgents/${PLIST_LABEL}.plist 2>/dev/null || true"
    ssh_cmd "launchctl unload ~/Library/LaunchAgents/${PLIST_FRPC}.plist 2>/dev/null || true"
    sleep 1
    ssh_cmd "launchctl load ~/Library/LaunchAgents/${PLIST_FRPC}.plist"
    ssh_cmd "launchctl load ~/Library/LaunchAgents/${PLIST_LABEL}.plist"
    echo "    launchd services started"

else
    # --- Linux: systemd ---
    ssh_cmd "sudo tee /etc/systemd/system/octos-serve.service > /dev/null" << EOF
[Unit]
Description=octos serve dashboard
After=network.target

[Service]
Type=simple
User=$(echo "$SSH_TARGET" | cut -d@ -f1)
Environment=HOME=${REMOTE_HOME}
Environment=PATH=${RBIN}:${REMOTE_HOME}/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=OCTOS_DATA_DIR=${RDATA}
Environment=OCTOS_AUTH_TOKEN=${AUTH_TOKEN}
ExecStart=${RBIN}/octos serve --port ${SERVE_PORT} --host 0.0.0.0
Restart=always
RestartSec=5
WorkingDirectory=${REMOTE_HOME}

[Install]
WantedBy=multi-user.target
EOF

    ssh_cmd "sudo tee /etc/systemd/system/frpc.service > /dev/null" << EOF
[Unit]
Description=frpc tunnel client
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/frpc -c /etc/frp/frpc.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

    ssh_cmd "sudo systemctl daemon-reload"
    ssh_cmd "sudo systemctl enable frpc octos-serve"
    ssh_cmd "sudo systemctl restart frpc octos-serve"
    echo "    systemd services started"
fi

# ── Step 9: Verify ────────────────────────────────────────────────────
echo ""
echo "==> Step 9: Verifying..."
sleep 3

# Check local octos serve
if ssh_cmd "curl -sf --max-time 3 http://localhost:${SERVE_PORT}/api/status" > /dev/null 2>&1; then
    echo "    octos serve: RUNNING on port ${SERVE_PORT}"
else
    echo "    octos serve: starting up (check ${RDATA}/logs/serve.\$(date +%F).log)"
fi

# Check frpc
if ssh_cmd "pgrep -x frpc" > /dev/null 2>&1; then
    echo "    frpc: RUNNING"
else
    echo "    frpc: starting up (check /tmp/frpc.log)"
fi

# Check tunnel from outside
sleep 2
if curl -sf --max-time 5 -H "Host: ${TENANT_NAME}.${DOMAIN}" "http://${FRPS_SERVER}/" > /dev/null 2>&1; then
    echo "    tunnel: CONNECTED"
else
    echo "    tunnel: connecting... (may take a few seconds)"
fi

echo ""
echo "============================================================"
echo "  Tenant ${TENANT_NAME} bootstrapped!"
echo ""
echo "  Dashboard:  http://${TENANT_NAME}.${DOMAIN}"
echo "  Auth token: ${AUTH_TOKEN}"
echo "  SSH tunnel: ssh -p ${SSH_PORT} $(echo "$SSH_TARGET" | cut -d@ -f1)@${FRPS_SERVER}"
echo "  Direct SSH: ssh ${SSH_TARGET}"
echo ""
echo "  Next steps:"
echo "  1. Create profiles via dashboard or API"
echo "  2. Add LLM API keys to profiles"
echo "  3. Enable channels (Telegram, Feishu, etc.)"
echo "============================================================"
