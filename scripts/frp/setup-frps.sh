#!/usr/bin/env bash
# setup-frps.sh — Install and configure frps (frp server) on a VPS.
# Idempotent: safe to re-run.
#
# Usage:
#   ./setup-frps.sh
#   FRPS_DASHBOARD_PASSWORD=<pw> ./setup-frps.sh
#
# Environment:
#   FRPS_DASHBOARD_PASSWORD  (optional) Dashboard password (default: random)
#   FRPS_VERSION             (optional) frp version to install (default: 0.65.0)
#   OCTOS_SERVE_PORT         (optional) octos serve port for auth plugin (default: 8080)

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
FRPS_VERSION="${FRPS_VERSION:-0.65.0}"
FRPS_DASHBOARD_PASSWORD="${FRPS_DASHBOARD_PASSWORD:-$(openssl rand -hex 16)}"
OCTOS_SERVE_PORT="${OCTOS_SERVE_PORT:-8080}"
FRPS_BIND_PORT="${FRPS_BIND_PORT:-7000}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8081}"
FRPS_VHOST_HTTPS_PORT="${FRPS_VHOST_HTTPS_PORT:-8443}"
FRPS_DASHBOARD_PORT="${FRPS_DASHBOARD_PORT:-7500}"
FRPS_SSH_PORT_START="${FRPS_SSH_PORT_START:-6001}"
FRPS_SSH_PORT_END="${FRPS_SSH_PORT_END:-6999}"

INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/frp"
CONFIG_FILE="${CONFIG_DIR}/frps.toml"

# ── Detect architecture ──────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  FRP_ARCH="amd64" ;;
    aarch64) FRP_ARCH="arm64" ;;
    arm64)   FRP_ARCH="arm64" ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

OS=$(uname -s | tr '[:upper:]' '[:lower:]')

echo "==> Installing frps v${FRPS_VERSION} (${OS}/${FRP_ARCH})"

# ── Download and install frps binary ──────────────────────────────────
if [ -f "${INSTALL_DIR}/frps" ]; then
    CURRENT_VERSION=$("${INSTALL_DIR}/frps" --version 2>/dev/null || echo "unknown")
    echo "    frps already installed (version: ${CURRENT_VERSION})"
    if [ "$CURRENT_VERSION" = "$FRPS_VERSION" ]; then
        echo "    Already at target version, skipping download"
    else
        echo "    Upgrading from ${CURRENT_VERSION} to ${FRPS_VERSION}"
    fi
fi

FRP_TMPDIR=$(mktemp -d /tmp/frps-install.XXXXXX)
trap 'rm -rf "$FRP_TMPDIR"' EXIT

TARBALL="frp_${FRPS_VERSION}_${OS}_${FRP_ARCH}.tar.gz"
URL="https://github.com/fatedier/frp/releases/download/v${FRPS_VERSION}/${TARBALL}"

echo "    Downloading ${URL}"
curl -fsSL -o "${FRP_TMPDIR}/${TARBALL}" "$URL"
tar -xzf "${FRP_TMPDIR}/${TARBALL}" -C "$FRP_TMPDIR"

sudo mkdir -p "${INSTALL_DIR}"
sudo cp "${FRP_TMPDIR}/frp_${FRPS_VERSION}_${OS}_${FRP_ARCH}/frps" "${INSTALL_DIR}/frps"
sudo chmod 0755 "${INSTALL_DIR}/frps"
echo "    Installed frps to ${INSTALL_DIR}/frps"

# ── Write configuration ───────────────────────────────────────────────
sudo mkdir -p "$CONFIG_DIR"
sudo tee "$CONFIG_FILE" > /dev/null << EOF
# frps configuration — managed by setup-frps.sh
# Do not edit manually; re-run the script to update.

bindPort = ${FRPS_BIND_PORT}
vhostHTTPPort = ${FRPS_VHOST_HTTP_PORT}
vhostHTTPSPort = ${FRPS_VHOST_HTTPS_PORT}
custom404Page = "${CONFIG_DIR}/404.html"

webServer.port = ${FRPS_DASHBOARD_PORT}
webServer.user = "admin"
webServer.password = "${FRPS_DASHBOARD_PASSWORD}"

# Restrict remotely exposed TCP ports to the configured SSH range.
allowPorts = [
  { start = ${FRPS_SSH_PORT_START}, end = ${FRPS_SSH_PORT_END} }
]

# Logging
log.to = "/var/log/frps.log"
log.level = "info"
log.maxDays = 7

# Per-tenant token auth — plugin verifies md5(token+timestamp) on Login,
# then cross-checks tenant ownership on NewProxy.
auth.method = "token"
auth.token = ""

[[httpPlugins]]
name = "octos-auth"
addr = "127.0.0.1:${OCTOS_SERVE_PORT}"
path = "/api/internal/frps-auth"
ops = ["Login", "NewProxy"]
EOF

# Install custom 404 page
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/404.html" ]; then
    sudo cp "$SCRIPT_DIR/404.html" "${CONFIG_DIR}/404.html"
    echo "    Installed custom 404 page"
else
    # Inline fallback if script is run standalone without the repo
    sudo tee "${CONFIG_DIR}/404.html" > /dev/null << 'HTMLEOF'
<!DOCTYPE html>
<html><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1.0">
<title>Octos Cloud</title>
<style>*{margin:0;padding:0}body{font-family:system-ui,sans-serif;background:#0a0a0f;color:#e4e4ef;min-height:100vh;display:flex;align-items:center;justify-content:center;text-align:center}
h1{font-size:24px;margin:24px 0 12px}p{color:#8888a0;font-size:16px;margin-bottom:32px}
a{background:#6366f1;color:#fff;text-decoration:none;padding:12px 32px;border-radius:8px;font-size:15px}</style></head>
<body><div><div style="font-size:64px">&#x1F419;</div><h1>This subdomain is not active</h1>
<p>This subdomain is not claimed or the node is currently offline.</p>
<a href="https://octos-cloud.org">Go to Octos Cloud</a></div></body></html>
HTMLEOF
    echo "    Created inline 404 page"
fi

echo "    Wrote config to ${CONFIG_FILE}"

# ── Create system service ─────────────────────────────────────────────
case "$(uname -s)" in
    Darwin)
        PLIST="/Library/LaunchDaemons/io.octos.frps.plist"
        sudo tee "$PLIST" > /dev/null << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.frps</string>
    <key>ProgramArguments</key>
    <array>
        <string>${INSTALL_DIR}/frps</string>
        <string>-c</string>
        <string>${CONFIG_FILE}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/frps.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/frps.log</string>
</dict>
</plist>
EOF
        sudo launchctl unload "$PLIST" 2>/dev/null || true
        sudo chown root:wheel "$PLIST"
        sudo chmod 644 "$PLIST"
        sudo launchctl load "$PLIST"
        ;;
    *)
        sudo tee /etc/systemd/system/frps.service > /dev/null << EOF
[Unit]
Description=frps tunnel relay server
After=network.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/frps -c ${CONFIG_FILE}
Restart=always
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
        sudo systemctl daemon-reload
        sudo systemctl enable frps
        sudo systemctl restart frps
        ;;
esac

echo "==> frps is running"
echo "    Control port: ${FRPS_BIND_PORT}"
echo "    vHost HTTP:   ${FRPS_VHOST_HTTP_PORT}"
echo "    vHost HTTPS:  ${FRPS_VHOST_HTTPS_PORT}"
echo "    Dashboard:    http://localhost:${FRPS_DASHBOARD_PORT}"
echo "    Dashboard pw: ${FRPS_DASHBOARD_PASSWORD}"
echo ""
echo "==> Firewall: ensure these ports are open:"
echo "    TCP ${FRPS_BIND_PORT}         (frp control)"
echo "    TCP ${FRPS_VHOST_HTTP_PORT}         (HTTP vhost)"
echo "    TCP ${FRPS_VHOST_HTTPS_PORT}        (HTTPS vhost)"
echo "    TCP ${FRPS_DASHBOARD_PORT}       (dashboard, admin only)"
echo "    TCP ${FRPS_SSH_PORT_START}-${FRPS_SSH_PORT_END}  (SSH tunnels, admin only)"
