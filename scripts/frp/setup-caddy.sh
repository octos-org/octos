#!/usr/bin/env bash
# setup-caddy.sh — Install Caddy as reverse proxy for octos serve and frps (Linux/macOS).
# Supports HTTP-only mode (default) or HTTPS with wildcard certs via DNS challenge.
# Idempotent: safe to re-run.
#
# Usage:
#   ./setup-caddy.sh                                    # HTTP only
#   ./setup-caddy.sh --https --dns-provider cloudflare  # HTTPS with wildcard certs
#
# Environment:
#   TUNNEL_DOMAIN         (optional) Base domain (default: octos-cloud.org)
#   OCTOS_SERVE_PORT      (optional) octos serve port for apex site (default: 8080)
#   FRPS_VHOST_HTTP_PORT  (optional) frps HTTP vhost port for tenant subdomains (default: 8081)
#   CF_API_TOKEN          (required for --dns-provider cloudflare)
#
# DNS Providers:
#   cloudflare   — requires CF_API_TOKEN (Zone:DNS:Edit)
#   route53      — requires AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY
#   digitalocean — requires DO_AUTH_TOKEN
#   godaddy      — requires GODADDY_API_KEY + GODADDY_API_SECRET

set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────
TUNNEL_DOMAIN="${TUNNEL_DOMAIN:-octos-cloud.org}"
OCTOS_SERVE_PORT="${OCTOS_SERVE_PORT:-8080}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8081}"
ENABLE_HTTPS=false
DNS_PROVIDER=""

sed_in_place() {
    local file="$1"
    shift
    local tmp
    tmp=$(mktemp /tmp/caddy-sed.XXXXXX)
    sed "$@" "$file" >"$tmp"
    if [ -w "$file" ]; then
        cat "$tmp" >"$file"
    else
        sudo install -m 0644 "$tmp" "$file"
    fi
    rm -f "$tmp"
}

# ── Parse arguments ───────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --https)        ENABLE_HTTPS=true; shift ;;
        --dns-provider) DNS_PROVIDER="$2"; shift 2 ;;
        --domain)       TUNNEL_DOMAIN="$2"; shift 2 ;;
        --help|-h)
            sed -n '2,23s/^# //p' "$0"
            exit 0
            ;;
        *)              echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Validate HTTPS requirements
if [ "$ENABLE_HTTPS" = true ] && [ -z "$DNS_PROVIDER" ]; then
    echo "ERROR: --https requires --dns-provider <provider>"
    echo "Supported: cloudflare, route53, digitalocean, godaddy"
    exit 1
fi

echo "==> Setting up Caddy for ${TUNNEL_DOMAIN}"
echo "    Mode: $([ "$ENABLE_HTTPS" = true ] && echo "HTTPS (DNS: ${DNS_PROVIDER})" || echo "HTTP only")"

# ── Detect architecture ──────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  CADDY_ARCH="amd64" ;;
    aarch64) CADDY_ARCH="arm64" ;;
    arm64)   CADDY_ARCH="arm64" ;;
    *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# ── Map DNS provider to Caddy plugin ─────────────────────────────────
DNS_PLUGIN=""
DNS_ENV_LINE=""
DNS_CONFIG_BLOCK=""
if [ "$ENABLE_HTTPS" = true ]; then
    case "$DNS_PROVIDER" in
        cloudflare)
            DNS_PLUGIN="github.com/caddy-dns/cloudflare"
            [ -z "${CF_API_TOKEN:-}" ] && { echo "ERROR: CF_API_TOKEN not set"; exit 1; }
            DNS_ENV_LINE="Environment=CF_API_TOKEN=${CF_API_TOKEN}"
            DNS_CONFIG_BLOCK="dns cloudflare {env.CF_API_TOKEN}"
            ;;
        route53)
            DNS_PLUGIN="github.com/caddy-dns/route53"
            [ -z "${AWS_ACCESS_KEY_ID:-}" ] && { echo "ERROR: AWS_ACCESS_KEY_ID not set"; exit 1; }
            [ -z "${AWS_SECRET_ACCESS_KEY:-}" ] && { echo "ERROR: AWS_SECRET_ACCESS_KEY not set"; exit 1; }
            DNS_ENV_LINE="Environment=AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID}\nEnvironment=AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}"
            DNS_CONFIG_BLOCK="dns route53"
            ;;
        digitalocean)
            DNS_PLUGIN="github.com/caddy-dns/digitalocean"
            [ -z "${DO_AUTH_TOKEN:-}" ] && { echo "ERROR: DO_AUTH_TOKEN not set"; exit 1; }
            DNS_ENV_LINE="Environment=DO_AUTH_TOKEN=${DO_AUTH_TOKEN}"
            DNS_CONFIG_BLOCK="dns digitalocean {env.DO_AUTH_TOKEN}"
            ;;
        godaddy)
            DNS_PLUGIN="github.com/caddy-dns/godaddy"
            [ -z "${GODADDY_API_KEY:-}" ] && { echo "ERROR: GODADDY_API_KEY not set"; exit 1; }
            [ -z "${GODADDY_API_SECRET:-}" ] && { echo "ERROR: GODADDY_API_SECRET not set"; exit 1; }
            DNS_ENV_LINE="Environment=GODADDY_API_KEY=${GODADDY_API_KEY}\nEnvironment=GODADDY_API_SECRET=${GODADDY_API_SECRET}"
            DNS_CONFIG_BLOCK="dns godaddy {env.GODADDY_API_KEY} {env.GODADDY_API_SECRET}"
            ;;
        *)
            echo "ERROR: Unsupported DNS provider: $DNS_PROVIDER"
            echo "Supported: cloudflare, route53, digitalocean, godaddy"
            exit 1
            ;;
    esac
fi

launchd_env_dict() {
    cat <<EOF
        <key>XDG_DATA_HOME</key>
        <string>/var/lib/caddy/data</string>
        <key>XDG_CONFIG_HOME</key>
        <string>/var/lib/caddy/config</string>
EOF

    case "$DNS_PROVIDER" in
        cloudflare)
            [ -n "${CF_API_TOKEN:-}" ] && cat <<EOF
        <key>CF_API_TOKEN</key>
        <string>${CF_API_TOKEN}</string>
EOF
            ;;
        route53)
            [ -n "${AWS_ACCESS_KEY_ID:-}" ] && cat <<EOF
        <key>AWS_ACCESS_KEY_ID</key>
        <string>${AWS_ACCESS_KEY_ID}</string>
EOF
            [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] && cat <<EOF
        <key>AWS_SECRET_ACCESS_KEY</key>
        <string>${AWS_SECRET_ACCESS_KEY}</string>
EOF
            ;;
        digitalocean)
            [ -n "${DO_AUTH_TOKEN:-}" ] && cat <<EOF
        <key>DO_AUTH_TOKEN</key>
        <string>${DO_AUTH_TOKEN}</string>
EOF
            ;;
        godaddy)
            [ -n "${GODADDY_API_KEY:-}" ] && cat <<EOF
        <key>GODADDY_API_KEY</key>
        <string>${GODADDY_API_KEY}</string>
EOF
            [ -n "${GODADDY_API_SECRET:-}" ] && cat <<EOF
        <key>GODADDY_API_SECRET</key>
        <string>${GODADDY_API_SECRET}</string>
EOF
            ;;
    esac
}

LAUNCHD_ENV_DICT="$(launchd_env_dict)"

# ── Install Caddy ────────────────────────────────────────────────────
install_caddy() {
    if [ "$ENABLE_HTTPS" = true ]; then
        # Build custom Caddy with DNS plugin using xcaddy
        echo "    Building Caddy with ${DNS_PROVIDER} DNS plugin..."
        if ! command -v xcaddy &>/dev/null; then
            if command -v go &>/dev/null; then
                go install github.com/caddyserver/xcaddy/cmd/xcaddy@latest
            else
                echo "ERROR: xcaddy requires Go. Install Go first: https://go.dev/dl/"
                exit 1
            fi
        fi
        XCADDY=$(command -v xcaddy)
        "$XCADDY" build --with "$DNS_PLUGIN" --output /tmp/caddy
        sudo install -m 0755 /tmp/caddy /usr/local/bin/caddy
        rm -f /tmp/caddy
    else
        # Standard Caddy binary (no plugins needed)
        echo "    Downloading standard Caddy..."
        CADDY_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
        curl -fsSL "https://caddyserver.com/api/download?os=${CADDY_OS}&arch=${CADDY_ARCH}" -o /tmp/caddy
        sudo install -m 0755 /tmp/caddy /usr/local/bin/caddy
        rm -f /tmp/caddy
    fi
}

NEEDS_INSTALL=false
if ! command -v caddy &>/dev/null; then
    NEEDS_INSTALL=true
elif [ "$ENABLE_HTTPS" = true ]; then
    # Check if existing Caddy has the DNS plugin; also verify the token is accepted.
    # Older module versions reject newer token formats (cfut_/cfat_ prefixes).
    if ! caddy list-modules 2>/dev/null | grep -q "dns.providers.${DNS_PROVIDER}"; then
        echo "    Existing Caddy missing ${DNS_PROVIDER} DNS module, rebuilding..."
        NEEDS_INSTALL=true
    else
        echo "    Caddy has ${DNS_PROVIDER} DNS module, verifying token compatibility..."
        # Write a minimal test Caddyfile to check if the module accepts the token
        TEST_CADDYFILE=$(mktemp /tmp/caddy-test.XXXXXX)
        cat > "$TEST_CADDYFILE" << 'TESTEOF'
*.test.invalid {
    tls {
        dns cloudflare __TEST_TOKEN__
    }
    respond "ok"
}
TESTEOF
        # Substitute the actual token (from env) for the placeholder
        case "$DNS_PROVIDER" in
            cloudflare)    TEST_TOKEN="${CF_API_TOKEN}" ;;
            digitalocean)  TEST_TOKEN="${DO_AUTH_TOKEN}" ;;
            route53)       TEST_TOKEN="test" ;;
            godaddy)       TEST_TOKEN="${GODADDY_API_KEY}" ;;
        esac
        sed_in_place "$TEST_CADDYFILE" -e "s|__TEST_TOKEN__|${TEST_TOKEN}|g"
        if caddy validate --config "$TEST_CADDYFILE" 2>&1 | grep -qi "invalid\|error"; then
            echo "    Module rejects token format, rebuilding with latest version..."
            NEEDS_INSTALL=true
        else
            echo "    Caddy ${DNS_PROVIDER} module is compatible"
        fi
        rm -f "$TEST_CADDYFILE"
    fi
else
    echo "    Caddy already installed: $(caddy version)"
fi

if [ "$NEEDS_INSTALL" = true ]; then
    install_caddy
fi

# Give caddy permission to bind low ports without root
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/caddy 2>/dev/null || true

echo "    Caddy: $(caddy version)"

# ── Write Caddyfile ───────────────────────────────────────────────────
sudo mkdir -p /etc/caddy

ESCAPED_DOMAIN="${TUNNEL_DOMAIN//./\\.}"

if [ "$ENABLE_HTTPS" = true ]; then
    sudo tee /etc/caddy/Caddyfile > /dev/null << 'CADDYEOF'
# Caddyfile — managed by setup-caddy.sh
# HTTPS with wildcard cert via __DNS_PROVIDER__ DNS challenge.

# Main site: all requests proxied to octos serve (landing page embedded)
www.__DOMAIN__, __DOMAIN__ {
    reverse_proxy localhost:__SERVE_PORT__
}

# Tenant subdomains: HTTPS with wildcard cert
*.__DOMAIN__ {
    tls {
        __DNS_CONFIG_BLOCK__
    }
    reverse_proxy localhost:__FRPS_VHOST_PORT__ {
        header_up Host {host}
    }
}

# HTTP fallback: redirect tenant subdomains and the apex site to HTTPS
:80 {
    @tenant {
        not header_regexp Host ^(www\.)?__ESCAPED_DOMAIN__$
        not header_regexp Host ^[0-9]
    }
    handle @tenant {
        redir https://{host}{uri} permanent
    }
    handle {
        redir https://{host}{uri} permanent
    }
}
CADDYEOF
else
    sudo tee /etc/caddy/Caddyfile > /dev/null << 'CADDYEOF'
# Caddyfile — managed by setup-caddy.sh
# HTTP-only mode. To enable HTTPS, re-run with:
#   ./setup-caddy.sh --https --dns-provider cloudflare

www.__DOMAIN__, __DOMAIN__ {
    reverse_proxy localhost:__SERVE_PORT__
}

:80 {
    @tenant {
        not header_regexp Host ^(www\.)?__ESCAPED_DOMAIN__$
        not header_regexp Host ^[0-9]
    }
    handle @tenant {
        reverse_proxy localhost:__FRPS_VHOST_PORT__ {
            header_up Host {host}
        }
    }
    handle {
        reverse_proxy localhost:__SERVE_PORT__
    }
}
CADDYEOF
fi

# Substitute shell variables into the Caddyfile (Caddy placeholders like {host} are preserved)
sed_in_place /etc/caddy/Caddyfile \
    -e "s|__DOMAIN__|${TUNNEL_DOMAIN}|g" \
    -e "s|__ESCAPED_DOMAIN__|${ESCAPED_DOMAIN}|g" \
    -e "s|__SERVE_PORT__|${OCTOS_SERVE_PORT}|g" \
    -e "s|__FRPS_VHOST_PORT__|${FRPS_VHOST_HTTP_PORT}|g" \
    -e "s|__DNS_PROVIDER__|${DNS_PROVIDER}|g" \
    -e "s|__DNS_CONFIG_BLOCK__|${DNS_CONFIG_BLOCK}|g"

echo "    Wrote Caddyfile to /etc/caddy/Caddyfile"

# ── Create system service ─────────────────────────────────────────────
CADDY_BIN="$(command -v caddy)"

case "$(uname -s)" in
    Darwin)
        PLIST="/Library/LaunchDaemons/io.octos.caddy.plist"
        sudo tee "$PLIST" > /dev/null << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.caddy</string>
    <key>ProgramArguments</key>
    <array>
        <string>${CADDY_BIN}</string>
        <string>run</string>
        <string>--config</string>
        <string>/etc/caddy/Caddyfile</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/caddy.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/caddy.log</string>
    <key>EnvironmentVariables</key>
    <dict>
${LAUNCHD_ENV_DICT}
    </dict>
</dict>
</plist>
EOF
        sudo mkdir -p /var/lib/caddy
        sudo launchctl unload "$PLIST" 2>/dev/null || true
        sudo chown root:wheel "$PLIST"
        sudo chmod 644 "$PLIST"
        sudo launchctl load "$PLIST"
        ;;
    *)
        # Create caddy user if doesn't exist (Linux)
        if ! id caddy &>/dev/null; then
            sudo useradd --system --home /var/lib/caddy --shell /usr/sbin/nologin caddy || true
        fi
        sudo mkdir -p /var/lib/caddy
        sudo chown caddy:caddy /var/lib/caddy

        sudo tee /etc/systemd/system/caddy.service > /dev/null << EOF
[Unit]
Description=Caddy reverse proxy for octos tunnel
After=network.target frps.service

[Service]
Type=simple
User=caddy
Group=caddy
ExecStart=${CADDY_BIN} run --config /etc/caddy/Caddyfile
ExecReload=${CADDY_BIN} reload --config /etc/caddy/Caddyfile
Restart=always
RestartSec=5
LimitNOFILE=65536

Environment=XDG_DATA_HOME=/var/lib/caddy/data
Environment=XDG_CONFIG_HOME=/var/lib/caddy/config
$([ -n "$DNS_ENV_LINE" ] && echo -e "$DNS_ENV_LINE" || true)

[Install]
WantedBy=multi-user.target
EOF
        sudo systemctl daemon-reload
        sudo systemctl enable caddy
        sudo systemctl restart caddy
        ;;
esac

# ── Verify Caddy is running ──────────────────────────────────────────
echo ""
echo "==> Verifying Caddy..."
sleep 2
if pgrep -x caddy > /dev/null 2>&1; then
    echo "    Caddy is running"
else
    echo "    WARNING: Caddy failed to start. Check logs:"
    if [ "$(uname -s)" = "Darwin" ]; then
        echo "    tail -50 /var/log/caddy.log"
    else
        echo "    sudo journalctl -u caddy --no-pager -n 20"
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────
VPS_IP=$(curl -s ifconfig.me 2>/dev/null || echo "<VPS_IP>")

echo ""
echo "==> Caddy is running"
if [ "$ENABLE_HTTPS" = true ]; then
    echo "    HTTPS: ${TUNNEL_DOMAIN} → localhost:${OCTOS_SERVE_PORT} (octos serve)"
    echo "    HTTPS: *.${TUNNEL_DOMAIN} → localhost:${FRPS_VHOST_HTTP_PORT} (frps vhost)"
    echo "    DNS challenge: ${DNS_PROVIDER}"
    echo "    Certs: auto-provisioned via Let's Encrypt"
else
    echo "    HTTP: ${TUNNEL_DOMAIN} → localhost:${OCTOS_SERVE_PORT} (octos serve)"
    echo "    HTTP: *.${TUNNEL_DOMAIN} → localhost:${FRPS_VHOST_HTTP_PORT} (frps vhost)"
fi
echo ""
echo "==> DNS: Point these A records to ${VPS_IP}:"
echo "    A     ${TUNNEL_DOMAIN}       → ${VPS_IP}"
echo "    A     *.${TUNNEL_DOMAIN}     → ${VPS_IP}"
if [ "$ENABLE_HTTPS" = false ]; then
    echo ""
    echo "==> To enable HTTPS, re-run:"
    echo "    CF_API_TOKEN=<token> ./setup-caddy.sh --https --dns-provider cloudflare"
fi
