#!/usr/bin/env bash
# Setup frp SERVER (frps) on macOS or Linux
# Usage: ./setup-frps.sh <auth-token>
# Example: ./setup-frps.sh my-secret-token-123

set -euo pipefail

FRP_VERSION="0.61.1"
TOKEN="${1:-}"

if [ -z "$TOKEN" ]; then
  echo "Usage: $0 <auth-token>"
  echo "  auth-token: shared secret between server and client (pick a strong random string)"
  exit 1
fi

OS=$(uname -s)
ARCH=$(uname -m)

echo "==> Installing frps v${FRP_VERSION} on ${OS}..."

case "$OS" in
  Darwin)
    case "$ARCH" in
      x86_64)  ARCH_NAME="amd64" ;;
      arm64)   ARCH_NAME="arm64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    OS_NAME="darwin"
    ;;
  Linux)
    case "$ARCH" in
      x86_64)  ARCH_NAME="amd64" ;;
      aarch64) ARCH_NAME="arm64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    OS_NAME="linux"
    ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

[ -d /usr/local/bin ] || sudo mkdir -p /usr/local/bin

cd /tmp
curl -fsSL "https://github.com/fatedier/frp/releases/download/v${FRP_VERSION}/frp_${FRP_VERSION}_${OS_NAME}_${ARCH_NAME}.tar.gz" -o frps.tar.gz
tar xzf frps.tar.gz
sudo cp "frp_${FRP_VERSION}_${OS_NAME}_${ARCH_NAME}/frps" /usr/local/bin/
rm -rf frps.tar.gz "frp_${FRP_VERSION}_${OS_NAME}_${ARCH_NAME}"

echo "==> frps installed at /usr/local/bin/frps"

echo "==> Writing config to /etc/frp/frps.toml..."
sudo mkdir -p /etc/frp
sudo tee /etc/frp/frps.toml > /dev/null <<EOF
bindPort = 7000
auth.method = "token"
auth.token = "${TOKEN}"

vhostHTTPPort = 80
vhostHTTPSPort = 443
EOF

# ── Platform-specific service setup ─────────────────────────────────
if [ "$OS" = "Darwin" ]; then
  PLIST_LABEL="com.frp.server"
  PLIST_PATH="/Library/LaunchDaemons/${PLIST_LABEL}.plist"

  # Stop existing if loaded
  if sudo launchctl list 2>/dev/null | grep -q "$PLIST_LABEL"; then
    echo "==> Stopping existing frps service..."
    sudo launchctl unload "$PLIST_PATH" 2>/dev/null || true
  fi

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
        <string>/usr/local/bin/frps</string>
        <string>-c</string>
        <string>/etc/frp/frps.toml</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>/var/log/frps.stdout.log</string>

    <key>StandardErrorPath</key>
    <string>/var/log/frps.stderr.log</string>

    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
EOF

  sudo chown root:wheel "$PLIST_PATH"
  sudo chmod 644 "$PLIST_PATH"

  echo "==> Starting frps..."
  sudo launchctl load "$PLIST_PATH"
  sleep 2

  STATUS=$(sudo launchctl list | grep "$PLIST_LABEL" || echo "not found")
  PID=$(echo "$STATUS" | awk '{print $1}')

  if [ "$PID" != "-" ] && [ "$PID" != "not" ]; then
    echo "==> frps is running (PID: ${PID})"
  else
    echo "==> frps may have failed. Check: tail -20 /var/log/frps.stderr.log"
  fi

  echo ""
  echo "==> Useful commands:"
  echo "    sudo launchctl list | grep frp        # check status"
  echo "    tail -f /var/log/frps.stderr.log      # watch logs"
  echo "    sudo launchctl stop ${PLIST_LABEL}    # stop"
  echo "    sudo launchctl start ${PLIST_LABEL}   # start"
  echo "    sudo launchctl unload ${PLIST_PATH}   # disable"

else
  # Linux — use systemd
  echo "==> Creating systemd service..."
  sudo tee /etc/systemd/system/frps.service > /dev/null <<'EOF'
[Unit]
Description=frp server
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/frps -c /etc/frp/frps.toml
Restart=always
RestartSec=10
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
EOF

  echo "==> Opening firewall ports (7000, 80, 443)..."
  if command -v ufw &>/dev/null; then
    sudo ufw allow 7000/tcp
    sudo ufw allow 80/tcp
    sudo ufw allow 443/tcp
  elif command -v firewall-cmd &>/dev/null; then
    sudo firewall-cmd --permanent --add-port=7000/tcp
    sudo firewall-cmd --permanent --add-port=80/tcp
    sudo firewall-cmd --permanent --add-port=443/tcp
    sudo firewall-cmd --reload
  else
    echo "  (no ufw or firewalld found — make sure ports 7000, 80, 443 are open)"
  fi

  echo "==> Starting frps..."
  sudo systemctl daemon-reload
  sudo systemctl enable frps
  sudo systemctl restart frps
  sudo systemctl status frps --no-pager
fi

echo ""
echo "==> Done! frps is running."
echo "    Control port: 7000"
echo "    HTTP vhost:   80"
echo "    HTTPS vhost:  443"
echo "    Auth token:   ${TOKEN}"
echo ""
echo "Next: run setup-frpc.sh on your client Mac."
