#!/usr/bin/env bash
# setup-frps.sh — Install and configure frps (frp server) on a VPS.
# Idempotent: safe to re-run.
#
# Usage:
#   FRPS_TOKEN=<secret> ./setup-frps.sh
#   FRPS_TOKEN=<secret> FRPS_DASHBOARD_PASSWORD=<pw> ./setup-frps.sh
#
# Environment:
#   FRPS_TOKEN               (required) Auth token shared with frpc clients
#   FRPS_DASHBOARD_PASSWORD  (optional) Dashboard password (default: random)
#   FRPS_VERSION             (optional) frp version to install (default: 0.61.1)

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
FRPS_VERSION="${FRPS_VERSION:-0.61.1}"
FRPS_TOKEN="${FRPS_TOKEN:?'FRPS_TOKEN env var is required'}"
FRPS_DASHBOARD_PASSWORD="${FRPS_DASHBOARD_PASSWORD:-$(openssl rand -hex 16)}"
FRPS_BIND_PORT="${FRPS_BIND_PORT:-7000}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8080}"
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

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

TARBALL="frp_${FRPS_VERSION}_${OS}_${FRP_ARCH}.tar.gz"
URL="https://github.com/fatedier/frp/releases/download/v${FRPS_VERSION}/${TARBALL}"

echo "    Downloading ${URL}"
curl -fsSL -o "${TMPDIR}/${TARBALL}" "$URL"
tar -xzf "${TMPDIR}/${TARBALL}" -C "$TMPDIR"

sudo install -m 0755 "${TMPDIR}/frp_${FRPS_VERSION}_${OS}_${FRP_ARCH}/frps" "${INSTALL_DIR}/frps"
echo "    Installed frps to ${INSTALL_DIR}/frps"

# ── Write configuration ───────────────────────────────────────────────
sudo mkdir -p "$CONFIG_DIR"
sudo tee "$CONFIG_FILE" > /dev/null << EOF
# frps configuration — managed by setup-frps.sh
# Do not edit manually; re-run the script to update.

bindPort = ${FRPS_BIND_PORT}
vhostHTTPPort = ${FRPS_VHOST_HTTP_PORT}
vhostHTTPSPort = ${FRPS_VHOST_HTTPS_PORT}

auth.method = "token"
auth.token = "${FRPS_TOKEN}"

webServer.port = ${FRPS_DASHBOARD_PORT}
webServer.user = "admin"
webServer.password = "${FRPS_DASHBOARD_PASSWORD}"

# Allow SSH tunnel port range
allowPorts = [
  { start = ${FRPS_SSH_PORT_START}, end = ${FRPS_SSH_PORT_END} }
]

# Logging
log.to = "/var/log/frps.log"
log.level = "info"
log.maxDays = 7
EOF

echo "    Wrote config to ${CONFIG_FILE}"

# ── Create systemd service ────────────────────────────────────────────
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

echo "==> frps is running"
echo "    Control port: ${FRPS_BIND_PORT}"
echo "    vHost HTTP:   ${FRPS_VHOST_HTTP_PORT}"
echo "    vHost HTTPS:  ${FRPS_VHOST_HTTPS_PORT}"
echo "    Dashboard:    http://$(hostname -I | awk '{print $1}'):${FRPS_DASHBOARD_PORT}"
echo "    Dashboard pw: ${FRPS_DASHBOARD_PASSWORD}"
echo ""
echo "==> Firewall: ensure these ports are open:"
echo "    TCP ${FRPS_BIND_PORT}         (frp control)"
echo "    TCP ${FRPS_VHOST_HTTP_PORT}         (HTTP vhost)"
echo "    TCP ${FRPS_VHOST_HTTPS_PORT}        (HTTPS vhost)"
echo "    TCP ${FRPS_DASHBOARD_PORT}       (dashboard, admin only)"
echo "    TCP ${FRPS_SSH_PORT_START}-${FRPS_SSH_PORT_END}  (SSH tunnels, admin only)"
