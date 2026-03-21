#!/usr/bin/env bash
# setup-caddy.sh — Install Caddy on VPS as reverse proxy to frps vhost.
# Uses standard Caddy (no plugins). HTTP-only or auto HTTPS via Let's Encrypt
# HTTP challenge (requires port 80 + DNS A records pointing to VPS).
# Idempotent: safe to re-run.
#
# Usage:
#   TUNNEL_DOMAIN=octos-cloud.org ./setup-caddy.sh
#
# Environment:
#   TUNNEL_DOMAIN         (optional) Base domain (default: octos-cloud.org)
#   FRPS_VHOST_HTTP_PORT  (optional) frps HTTP vhost port (default: 8080)

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
TUNNEL_DOMAIN="${TUNNEL_DOMAIN:-octos-cloud.org}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8080}"

echo "==> Setting up Caddy for ${TUNNEL_DOMAIN}"

# ── Install Caddy (standard binary, no plugins needed) ────────────────
if command -v caddy &>/dev/null; then
    echo "    Caddy already installed: $(caddy version)"
else
    echo "    Installing Caddy..."

    # Use Caddy's download API for a standard build
    ARCH=$(uname -m)
    case "$ARCH" in
        x86_64)  CADDY_ARCH="amd64" ;;
        aarch64) CADDY_ARCH="arm64" ;;
        arm64)   CADDY_ARCH="arm64" ;;
        *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac

    curl -fsSL "https://caddyserver.com/api/download?os=linux&arch=${CADDY_ARCH}" -o /tmp/caddy
    sudo install -m 0755 /tmp/caddy /usr/local/bin/caddy
    rm -f /tmp/caddy
fi

# Give caddy permission to bind low ports without root
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/caddy 2>/dev/null || true

echo "    Caddy: $(caddy version)"

# ── Write Caddyfile ───────────────────────────────────────────────────
sudo mkdir -p /etc/caddy
sudo tee /etc/caddy/Caddyfile > /dev/null << EOF
# Caddyfile — managed by setup-caddy.sh
# Reverse-proxies HTTP to frps vhost port.
# For HTTPS: point A records for *.${TUNNEL_DOMAIN} to this VPS,
# then change :80 to the domain and Caddy auto-provisions Let's Encrypt certs.

:80 {
    reverse_proxy localhost:${FRPS_VHOST_HTTP_PORT} {
        header_up Host {host}
    }
}
EOF

echo "    Wrote Caddyfile to /etc/caddy/Caddyfile"

# ── Create caddy user if doesn't exist ────────────────────────────────
if ! id caddy &>/dev/null; then
    sudo useradd --system --home /var/lib/caddy --shell /usr/sbin/nologin caddy || true
fi
sudo mkdir -p /var/lib/caddy
sudo chown caddy:caddy /var/lib/caddy

# ── Create systemd service ────────────────────────────────────────────
sudo tee /etc/systemd/system/caddy.service > /dev/null << EOF
[Unit]
Description=Caddy reverse proxy for octos tunnel
After=network.target frps.service

[Service]
Type=simple
User=caddy
Group=caddy
ExecStart=/usr/local/bin/caddy run --config /etc/caddy/Caddyfile
ExecReload=/usr/local/bin/caddy reload --config /etc/caddy/Caddyfile
Restart=always
RestartSec=5
LimitNOFILE=65536

Environment=XDG_DATA_HOME=/var/lib/caddy/data
Environment=XDG_CONFIG_HOME=/var/lib/caddy/config

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable caddy
sudo systemctl restart caddy

echo "==> Caddy is running"
echo "    Listening on :80 → localhost:${FRPS_VHOST_HTTP_PORT} (frps vhost)"
echo ""
echo "==> DNS: Point these A records to $(curl -s ifconfig.me):"
echo "    A     ${TUNNEL_DOMAIN}"
echo "    A     *.${TUNNEL_DOMAIN}"
echo ""
echo "==> To enable HTTPS later, edit /etc/caddy/Caddyfile:"
echo "    Replace ':80' with '*.${TUNNEL_DOMAIN}' and Caddy will auto-provision certs"
