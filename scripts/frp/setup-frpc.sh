#!/usr/bin/env bash
# Setup frp CLIENT (frpc) on macOS with LaunchDaemon
# Usage: ./setup-frpc.sh <vps-ip> <auth-token> <domain> [local-port]
# Example: ./setup-frpc.sh 1.2.3.4 my-secret-token-123 crewrs.hagency.org 8080

set -euo pipefail

FRP_VERSION="0.61.1"
VPS_IP="${1:-}"
TOKEN="${2:-}"
DOMAIN="${3:-}"
LOCAL_PORT="${4:-8080}"

if [ -z "$VPS_IP" ] || [ -z "$TOKEN" ] || [ -z "$DOMAIN" ]; then
  echo "Usage: $0 <vps-ip> <auth-token> <domain> [local-port]"
  echo ""
  echo "  vps-ip:     public IP of the VPS running frps"
  echo "  auth-token: same token used in setup-frps.sh"
  echo "  domain:     domain name pointing to the VPS (e.g. crewrs.hagency.org)"
  echo "  local-port: local service port to expose (default: 8080)"
  exit 1
fi

PLIST_LABEL="com.frp.client"
PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"
CONF_DIR="/etc/frp"
LOG_DIR="/var/log"

# ── Stop existing service if loaded ──────────────────────────────────
if sudo launchctl list 2>/dev/null | grep -q "$PLIST_LABEL"; then
  echo "==> Stopping existing frpc service..."
  sudo launchctl unload "$PLIST_PATH" 2>/dev/null || true
fi

# ── Install frpc binary ─────────────────────────────────────────────
echo "==> Installing frpc v${FRP_VERSION}..."
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  ARCH_NAME="amd64" ;;
  arm64)   ARCH_NAME="arm64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

[ -d /usr/local/bin ] || sudo mkdir -p /usr/local/bin

cd /tmp
curl -fsSL "https://github.com/fatedier/frp/releases/download/v${FRP_VERSION}/frp_${FRP_VERSION}_darwin_${ARCH_NAME}.tar.gz" -o frpc.tar.gz
tar xzf frpc.tar.gz
sudo cp "frp_${FRP_VERSION}_darwin_${ARCH_NAME}/frpc" /usr/local/bin/
rm -rf frpc.tar.gz "frp_${FRP_VERSION}_darwin_${ARCH_NAME}"

echo "==> frpc installed at /usr/local/bin/frpc"

# ── Generate self-signed cert for tunnel ─────────────────────────────
echo "==> Generating self-signed certificate..."
sudo mkdir -p "$CONF_DIR"
sudo openssl req -x509 -nodes -days 3650 -newkey rsa:2048 \
  -keyout "${CONF_DIR}/server.key" \
  -out "${CONF_DIR}/server.crt" \
  -subj "/CN=${DOMAIN}" 2>/dev/null

# ── Write frpc config ───────────────────────────────────────────────
echo "==> Writing config to ${CONF_DIR}/frpc.toml..."
sudo tee "${CONF_DIR}/frpc.toml" > /dev/null <<EOF
serverAddr = "${VPS_IP}"
serverPort = 7000
auth.method = "token"
auth.token = "${TOKEN}"

[[proxies]]
name = "crew-https"
type = "https"
customDomains = ["${DOMAIN}"]

[proxies.plugin]
type = "https2http"
localAddr = "127.0.0.1:${LOCAL_PORT}"
crtPath = "${CONF_DIR}/server.crt"
keyPath = "${CONF_DIR}/server.key"
hostHeaderRewrite = "127.0.0.1"

[[proxies]]
name = "crew-http"
type = "http"
customDomains = ["${DOMAIN}"]
localIP = "127.0.0.1"
localPort = ${LOCAL_PORT}
EOF

# ── Verify config by doing a dry run ────────────────────────────────
echo "==> Verifying config..."
if ! /usr/local/bin/frpc verify -c "${CONF_DIR}/frpc.toml" 2>/dev/null; then
  echo "  (verify command not available, skipping — will test on start)"
fi

# ── Create LaunchDaemon ─────────────────────────────────────────────
echo "==> Creating LaunchDaemon at ${PLIST_PATH}..."
sudo tee "$PLIST_PATH" > /dev/null <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/frpc</string>
        <string>-c</string>
        <string>${CONF_DIR}/frpc.toml</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>${LOG_DIR}/frpc.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>${LOG_DIR}/frpc.stderr.log</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
EOF

sudo chown root:wheel "$PLIST_PATH"
sudo chmod 644 "$PLIST_PATH"

# ── Start service ───────────────────────────────────────────────────
echo "==> Starting frpc..."
sudo launchctl load "$PLIST_PATH"

# Wait a moment for it to start
sleep 2

# ── Verify ──────────────────────────────────────────────────────────
echo "==> Checking status..."
STATUS=$(sudo launchctl list | grep "$PLIST_LABEL" || echo "not found")
PID=$(echo "$STATUS" | awk '{print $1}')

if [ "$PID" != "-" ] && [ "$PID" != "not" ]; then
  echo ""
  echo "==> frpc is running (PID: ${PID})"
else
  echo ""
  echo "==> frpc may have failed to start. Check logs:"
  echo "    tail -20 ${LOG_DIR}/frpc.stderr.log"
fi

echo ""
echo "==> Setup complete!"
echo "    VPS:        ${VPS_IP}"
echo "    Domain:     ${DOMAIN}"
echo "    Local port: ${LOCAL_PORT}"
echo "    Config:     ${CONF_DIR}/frpc.toml"
echo "    Logs:       ${LOG_DIR}/frpc.stderr.log"
echo ""
echo "==> Make sure DNS A record for ${DOMAIN} points to ${VPS_IP}"
echo ""
echo "==> Useful commands:"
echo "    sudo launchctl list | grep frp        # check status"
echo "    tail -f ${LOG_DIR}/frpc.stderr.log    # watch logs"
echo "    sudo launchctl stop ${PLIST_LABEL}    # stop"
echo "    sudo launchctl start ${PLIST_LABEL}   # start"
echo "    sudo launchctl unload ${PLIST_PATH}   # disable"
