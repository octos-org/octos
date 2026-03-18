#!/usr/bin/env bash
# setup-caddy.sh — Install Caddy on VPS with wildcard SSL via Cloudflare DNS challenge.
# Caddy terminates TLS for *.octos-cloud.org and reverse-proxies to frps vhost ports.
# Idempotent: safe to re-run.
#
# Usage:
#   CF_API_TOKEN=<token> TUNNEL_DOMAIN=octos-cloud.org ./setup-caddy.sh
#
# Environment:
#   CF_API_TOKEN    (required) Cloudflare API token with DNS edit permissions
#   TUNNEL_DOMAIN   (optional) Base domain (default: octos-cloud.org)
#   FRPS_VHOST_HTTP_PORT  (optional) frps HTTP vhost port (default: 8080)

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
CF_API_TOKEN="${CF_API_TOKEN:?'CF_API_TOKEN env var is required'}"
TUNNEL_DOMAIN="${TUNNEL_DOMAIN:-octos-cloud.org}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8080}"

echo "==> Setting up Caddy for *.${TUNNEL_DOMAIN}"

# ── Install Caddy with Cloudflare DNS module ──────────────────────────
# We need the custom build with the cloudflare DNS plugin
if command -v caddy &>/dev/null; then
    echo "    Caddy already installed: $(caddy version)"
else
    echo "    Installing Caddy with cloudflare DNS module..."
fi

# Always install/update to get the cloudflare DNS plugin
# Use xcaddy to build with the cloudflare plugin
if ! command -v xcaddy &>/dev/null; then
    echo "    Installing xcaddy..."
    # Install Go if not present (needed for xcaddy)
    if ! command -v go &>/dev/null; then
        echo "    Installing Go..."
        if command -v apt-get &>/dev/null; then
            sudo apt-get update -qq && sudo apt-get install -y -qq golang-go
        elif command -v dnf &>/dev/null; then
            sudo dnf install -y golang
        elif command -v yum &>/dev/null; then
            sudo yum install -y golang
        else
            echo "ERROR: Cannot install Go. Please install Go manually."
            exit 1
        fi
    fi
    go install github.com/caddyserver/xcaddy/cmd/xcaddy@latest
    export PATH="$PATH:$(go env GOPATH)/bin"
fi

echo "    Building Caddy with cloudflare DNS plugin..."
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

xcaddy build --with github.com/caddy-dns/cloudflare --output "${TMPDIR}/caddy"
sudo install -m 0755 "${TMPDIR}/caddy" /usr/local/bin/caddy

# Give caddy permission to bind low ports without root
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/caddy 2>/dev/null || true

echo "    Caddy installed: $(caddy version)"

# ── Write Caddyfile ───────────────────────────────────────────────────
sudo mkdir -p /etc/caddy
sudo tee /etc/caddy/Caddyfile > /dev/null << EOF
# Caddyfile — managed by setup-caddy.sh
# Wildcard SSL for *.${TUNNEL_DOMAIN} via Cloudflare DNS challenge.
# Reverse-proxies to frps vhost HTTP port.

*.${TUNNEL_DOMAIN} {
    tls {
        dns cloudflare {env.CF_API_TOKEN}
    }

    # Pass the original Host header so frps can route by subdomain
    reverse_proxy localhost:${FRPS_VHOST_HTTP_PORT} {
        header_up Host {host}
    }
}

# Root domain — serve a simple landing page or redirect
${TUNNEL_DOMAIN} {
    tls {
        dns cloudflare {env.CF_API_TOKEN}
    }
    respond "octos tunnel relay" 200
}
EOF

echo "    Wrote Caddyfile to /etc/caddy/Caddyfile"

# ── Create systemd service with Cloudflare token ──────────────────────
sudo mkdir -p /etc/caddy/env
sudo tee /etc/caddy/env/cloudflare > /dev/null << EOF
CF_API_TOKEN=${CF_API_TOKEN}
EOF
sudo chmod 600 /etc/caddy/env/cloudflare

# Create caddy user if doesn't exist
if ! id caddy &>/dev/null; then
    sudo useradd --system --home /var/lib/caddy --shell /usr/sbin/nologin caddy || true
fi
sudo mkdir -p /var/lib/caddy
sudo chown caddy:caddy /var/lib/caddy

sudo tee /etc/systemd/system/caddy.service > /dev/null << EOF
[Unit]
Description=Caddy reverse proxy with wildcard SSL
After=network.target frps.service

[Service]
Type=simple
User=caddy
Group=caddy
EnvironmentFile=/etc/caddy/env/cloudflare
ExecStart=/usr/local/bin/caddy run --config /etc/caddy/Caddyfile
ExecReload=/usr/local/bin/caddy reload --config /etc/caddy/Caddyfile
Restart=always
RestartSec=5
LimitNOFILE=65536

# Caddy data and config directories
Environment=XDG_DATA_HOME=/var/lib/caddy/data
Environment=XDG_CONFIG_HOME=/var/lib/caddy/config

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable caddy
sudo systemctl restart caddy

echo "==> Caddy is running"
echo "    Wildcard domain: *.${TUNNEL_DOMAIN}"
echo "    Proxying HTTPS → localhost:${FRPS_VHOST_HTTP_PORT} (frps vhost)"
echo ""
echo "==> DNS: Ensure these records exist in Cloudflare:"
echo "    A     ${TUNNEL_DOMAIN}     → $(curl -s ifconfig.me)"
echo "    A     *.${TUNNEL_DOMAIN}   → $(curl -s ifconfig.me)"
