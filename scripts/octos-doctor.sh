#!/usr/bin/env bash
# octos-doctor.sh — Diagnose octos installation and service health.
# Self-contained: no dependencies beyond a standard shell.
#
# Usage:
#   octos-doctor.sh
#   curl -fsSL https://github.com/octos-org/octos/releases/latest/download/octos-doctor.sh | bash
#
# Options:
#   --prefix DIR     Install prefix to check (default: ~/.octos/bin)
#   --data-dir DIR   Data directory to check (default: ~/.octos)

set -euo pipefail

PREFIX="${OCTOS_PREFIX:-$HOME/.octos/bin}"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"

needval() {
    # Ensure that option "$1" has a non-empty value in "$2".
    if [ "$#" -lt 2 ] || [ -z "${2-}" ]; then
        echo "Option $1 requires an argument" >&2
        exit 1
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            needval "$1" "${2-}"
            PREFIX="$2"
            shift 2
            ;;
        --data-dir)
            needval "$1" "${2-}"
            DATA_DIR="$2"
            shift 2
            ;;
        --help|-h)
            cat << 'HELPEOF'
octos-doctor.sh — Diagnose octos installation and service health.

Usage:
  octos-doctor.sh
  curl -fsSL .../octos-doctor.sh | bash

Options:
  --prefix DIR     Install prefix to check (default: ~/.octos/bin)
  --data-dir DIR   Data directory to check (default: ~/.octos)
HELPEOF
            exit 0
            ;;
        *)
            echo "Unknown option: $1"; exit 1 ;;
    esac
done

OS="$(uname -s)"
CAN_SUDO_LSOF=false

# ── Output helpers ──────────────────────────────────────────────────
section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
hint()    { echo "          -> $1"; }
DOCTOR_ISSUES=0
err()     { echo "    FAIL: $1"; DOCTOR_ISSUES=$((DOCTOR_ISSUES + 1)); }

if [ "$OS" = "Darwin" ] && command -v sudo >/dev/null 2>&1; then
    if [ -r /dev/tty ]; then
        if sudo -v 2>/dev/null; then
            CAN_SUDO_LSOF=true
        fi
    elif sudo -n true 2>/dev/null; then
        CAN_SUDO_LSOF=true
    fi
fi

# ── Platform helpers ────────────────────────────────────────────────

pkg_hint() {
    case "$OS" in
        Darwin)
            case "$1" in
                git)       echo "xcode-select --install" ;;
                node)      echo "brew install node" ;;
                chromium)  echo "brew install --cask google-chrome" ;;
                ffmpeg)    echo "brew install ffmpeg" ;;
            esac
            ;;
        Linux)
            case "$1" in
                git)       echo "sudo apt-get install -y git (or your package manager)" ;;
                node)      echo "curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt-get install -y nodejs" ;;
                chromium)  echo "sudo apt-get install -y chromium-browser" ;;
                ffmpeg)    echo "sudo apt-get install -y ffmpeg" ;;
                iproute2)  echo "sudo apt-get install -y iproute2" ;;
            esac
            ;;
        *)
            echo "(see your OS package manager)" ;;
    esac
}

svc_hint() {
    local action="$1" service="$2"
    case "$OS" in
        Darwin)
            local plist="/Library/LaunchDaemons/io.octos.${service}.plist"
            case "$action" in
                start)   echo "sudo launchctl load $plist" ;;
                stop)    echo "sudo launchctl unload $plist" ;;
                restart) echo "sudo launchctl unload $plist && sudo launchctl load $plist" ;;
                status)  echo "sudo launchctl print system/io.octos.${service}" ;;
            esac
            ;;
        Linux)
            local unit="$service"
            [ "$service" = "serve" ] && unit="octos-serve"
            case "$action" in
                start)   echo "sudo systemctl start $unit" ;;
                stop)    echo "sudo systemctl stop $unit" ;;
                restart) echo "sudo systemctl restart $unit" ;;
                status)  echo "sudo systemctl status $unit" ;;
            esac
            ;;
        *)
            echo "# service management not supported on $OS" ;;
    esac
}

config_json_get() {
    local expr="$1"
    local config_path="$DATA_DIR/config.json"
    [ -f "$config_path" ] || return 1
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$config_path" "$expr" <<'PYEOF'
import json
import sys

config_path = sys.argv[1]
expr = sys.argv[2]
with open(config_path) as fh:
    data = json.load(fh)

cur = data
for part in expr.split("."):
    if not isinstance(cur, dict) or part not in cur:
        sys.exit(1)
    cur = cur[part]

if isinstance(cur, bool):
    print("true" if cur else "false")
elif cur is None:
    sys.exit(1)
else:
    print(cur)
PYEOF
    else
        return 1
    fi
}

is_service_active() {
    local service="$1"
    case "$OS" in
        Darwin)
            launchctl print "system/io.octos.${service}" >/dev/null 2>&1
            ;;
        Linux)
            local unit="$service"
            [ "$service" = "serve" ] && unit="octos-serve"
            systemctl is-active "$unit" >/dev/null 2>&1
            ;;
        *)
            return 1
            ;;
    esac
}

find_octos_serve_pid() {
    local pid=""
    pid=$(ps ax -o pid= -o command= 2>/dev/null | grep -E '(^|/| )octos( |$).* serve( |$)|(^| )octos serve( |$)' | grep -v grep | head -1 | awk '{print $1}' || true)
    printf '%s\n' "$pid"
}

check_listener() {
    local port="$1"
    local expected_name="$2"
    local label="$3"
    local owner=""
    local pid=""

    if command -v lsof >/dev/null 2>&1; then
        local line
        if [ "$OS" = "Darwin" ] && [ "$CAN_SUDO_LSOF" = true ]; then
            line=$(sudo lsof -i :"$port" -P -n 2>/dev/null | grep LISTEN | head -1 || true)
        else
            line=$(lsof -i :"$port" -P -n 2>/dev/null | grep LISTEN | head -1 || true)
        fi
        if [ -n "$line" ]; then
            owner=$(echo "$line" | awk '{print $1}')
            pid=$(echo "$line" | awk '{print $2}')
        fi
    elif command -v ss >/dev/null 2>&1; then
        local line
        line=$(ss -tlnp "( sport = :$port )" 2>/dev/null | tail -n +2 | head -1 || true)
        if [ -n "$line" ]; then
            owner=$(echo "$line" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p')
            pid=$(echo "$line" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')
        fi
    fi

    if [ -z "$owner" ]; then
        err "$label port $port is not listening"
        hint "Check: $(svc_hint status "$expected_name")"
        return
    fi

    if echo "$owner" | grep -qi "$expected_name"; then
        ok "$label port $port held by $owner${pid:+ (PID: $pid)}"
    else
        warn "$label port $port held by $owner${pid:+ (PID: $pid)}"
    fi
}

CLOUD_MODE=false
CLOUD_DOMAIN=""
FRPS_SERVER=""
SMTP_ENABLED=false
SMTP_PASSWORD_ENV=""
ALLOW_SELF_REGISTRATION="false"
CLOUD_HTTPS_ENABLED=false

# ══════════════════════════════════════════════════════════════════════
# ── Checks ───────────────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════

echo "octos doctor"
echo "============"

# ── Binary ───────────────────────────────────────────────────────
section "octos binary"

OCTOS_BIN="$PREFIX/octos"
if [ -f "$OCTOS_BIN" ]; then
    ok "found: $OCTOS_BIN"
    if "$OCTOS_BIN" --version &>/dev/null; then
        ok "version: $("$OCTOS_BIN" --version 2>&1 | head -1)"
    else
        err "binary exists but failed to run"
        if [ "$OS" = "Darwin" ]; then
            hint "Try: xattr -d com.apple.quarantine $OCTOS_BIN && codesign -s - $OCTOS_BIN"
        else
            hint "Try: chmod +x $OCTOS_BIN"
            hint "Check dependencies: ldd $OCTOS_BIN"
        fi
        hint "Or re-run install.sh"
    fi
else
    if command -v octos &>/dev/null; then
        FOUND="$(command -v octos)"
        warn "not found at $OCTOS_BIN, but found at $FOUND"
        hint "Set OCTOS_PREFIX or add $PREFIX to PATH"
    else
        err "octos binary not found"
        hint "Run install.sh to install"
    fi
fi

# ── Data directory ───────────────────────────────────────────────
section "Data directory"

if [ -d "$DATA_DIR" ]; then
    ok "found: $DATA_DIR"
        if [ -f "$DATA_DIR/config.json" ]; then
            ok "config.json exists"
            MODE="$(config_json_get mode 2>/dev/null || true)"
        if [ "$MODE" = "cloud" ]; then
            CLOUD_MODE=true
            CLOUD_DOMAIN="$(config_json_get tunnel_domain 2>/dev/null || true)"
            FRPS_SERVER="$(config_json_get frps_server 2>/dev/null || true)"
            SMTP_PASSWORD_ENV="$(config_json_get dashboard_auth.smtp.password_env 2>/dev/null || true)"
            SMTP_HOST_CFG="$(config_json_get dashboard_auth.smtp.host 2>/dev/null || true)"
            SMTP_PORT_CFG="$(config_json_get dashboard_auth.smtp.port 2>/dev/null || true)"
            SMTP_FROM_CFG="$(config_json_get dashboard_auth.smtp.from_address 2>/dev/null || true)"
            ALLOW_SELF_REGISTRATION="$(config_json_get dashboard_auth.allow_self_registration 2>/dev/null || echo false)"
            if [ -n "$SMTP_PASSWORD_ENV" ]; then
                SMTP_ENABLED=true
            fi
            ok "deployment mode: cloud"
            [ -n "$CLOUD_DOMAIN" ] && ok "tunnel domain: $CLOUD_DOMAIN"
            [ -n "$FRPS_SERVER" ] && ok "frps server: $FRPS_SERVER"
            if [ -f /etc/caddy/Caddyfile ] && grep -q 'tls {' /etc/caddy/Caddyfile 2>/dev/null; then
                CLOUD_HTTPS_ENABLED=true
            fi
        fi
    else
        warn "config.json missing"
        hint "Run: octos init"
    fi
else
    err "$DATA_DIR does not exist"
    hint "Run: octos init --defaults"
fi

# ── octos serve process ──────────────────────────────────────────
section "octos serve"

OCTOS_PID="$(find_octos_serve_pid)"
if [ -n "$OCTOS_PID" ]; then
    OCTOS_CMD=$(ps -p "$OCTOS_PID" -o args= 2>/dev/null || true)
    ok "running (PID: $OCTOS_PID)"
    echo "    CMD: $OCTOS_CMD"
else
    if is_service_active serve; then
        warn "service appears active but process match failed"
        hint "Check: $(svc_hint status serve)"
    else
        err "octos serve is not running"
        hint "Start: $(svc_hint start serve)"
        hint "Or manually: $PREFIX/octos serve --port 8080 --host 0.0.0.0"
    fi
fi

# ── Port 8080 ────────────────────────────────────────────────────
section "Port 8080"

PORT_CMD=""
PORT_PID=""
PORT_CHECK_AVAILABLE=false
if command -v lsof &>/dev/null; then
    PORT_CHECK_AVAILABLE=true
    if [ "$OS" = "Darwin" ] && [ "$CAN_SUDO_LSOF" = true ]; then
        PORT_OWNER=$(sudo lsof -i :8080 -P -n 2>/dev/null | grep LISTEN | head -1 || true)
    else
        PORT_OWNER=$(lsof -i :8080 -P -n 2>/dev/null | grep LISTEN | head -1 || true)
    fi
    if [ -n "$PORT_OWNER" ]; then
        PORT_CMD=$(echo "$PORT_OWNER" | awk '{print $1}')
        PORT_PID=$(echo "$PORT_OWNER" | awk '{print $2}')
    fi
elif command -v ss &>/dev/null; then
    PORT_CHECK_AVAILABLE=true
    PORT_OWNER=$(ss -tlnp 'sport = :8080' 2>/dev/null | tail -n +2 | head -1 || true)
    if [ -n "$PORT_OWNER" ]; then
        PORT_CMD=$(echo "$PORT_OWNER" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p')
        PORT_PID=$(echo "$PORT_OWNER" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')
    fi
elif command -v netstat &>/dev/null; then
    PORT_CHECK_AVAILABLE=true
    PORT_OWNER=$(netstat -tlnp 2>/dev/null | grep ':8080 ' | head -1 || true)
    if [ -n "$PORT_OWNER" ]; then
        PORT_PID=$(echo "$PORT_OWNER" | awk '{print $NF}' | cut -d/ -f1)
        PORT_CMD=$(echo "$PORT_OWNER" | awk '{print $NF}' | cut -d/ -f2)
    fi
else
    warn "cannot check port 8080 (none of lsof, ss, or netstat found)"
    _iproute_hint=$(pkg_hint iproute2)
    if [ -n "$_iproute_hint" ]; then
        hint "Install one: $_iproute_hint   # provides ss"
    fi
fi

if [ -n "$PORT_CMD" ]; then
    if echo "$PORT_CMD" | grep -qi octos; then
        ok "port 8080 held by octos (PID: $PORT_PID)"
        if [ -z "$OCTOS_PID" ]; then
            OCTOS_PID="$PORT_PID"
            OCTOS_CMD=$(ps -p "$OCTOS_PID" -o args= 2>/dev/null || true)
        fi
    else
        err "port 8080 held by $PORT_CMD (PID: $PORT_PID) — not octos"
        hint "Kill it: kill $PORT_PID"
        if [ "$OS" = "Darwin" ]; then
            hint "If it respawns, find its LaunchAgent/Daemon:"
            hint "  grep -rl '$PORT_CMD' ~/Library/LaunchAgents/ /Library/LaunchDaemons/ 2>/dev/null"
        fi
    fi
elif [ "$PORT_CHECK_AVAILABLE" = true ]; then
    if [ -n "$OCTOS_PID" ]; then
        err "octos serve is running but nothing is listening on 8080"
        hint "Check if it's bound to a different port: ps -p $OCTOS_PID -o args="
    else
        warn "nothing listening on port 8080"
    fi
fi

# ── Admin portal ─────────────────────────────────────────────────
section "Admin portal"

HTTP_CODE=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 3 http://localhost:8080/admin/ 2>/dev/null || echo "000")
case "$HTTP_CODE" in
    200)
        ok "http://localhost:8080/admin/ responds 200"
        ;;
    000|000000)
        err "connection failed (server not reachable on localhost:8080)"
        hint "Check 'octos serve' and 'Port 8080' sections above"
        ;;
    401|403)
        warn "responds $HTTP_CODE (auth required)"
        hint "Pass auth token: curl -H 'Authorization: Bearer <token>' http://localhost:8080/admin/"
        ;;
    404)
        err "responds 404 (admin route not found)"
        hint "Binary may be built without 'api' feature. Rebuild with: cargo build --features api"
        ;;
    *)
        warn "responds HTTP $HTTP_CODE"
        hint "Check logs: tail -20 $DATA_DIR/serve.log"
        ;;
esac

# ── Service configuration ────────────────────────────────────────
section "Service configuration"

case "$OS" in
    Darwin)
        PLIST="/Library/LaunchDaemons/io.octos.serve.plist"
        if [ -f "$PLIST" ]; then
            ok "LaunchDaemon plist exists: $PLIST"
            if pgrep -f "octos serve" &>/dev/null; then
                ok "service appears loaded (process running)"
            else
                warn "plist exists but service does not appear to be running"
                hint "Check: $(svc_hint status serve)"
                hint "Load:  $(svc_hint start serve)"
            fi
        else
            warn "no LaunchDaemon plist found"
            hint "Re-run install.sh to set up the service"
        fi

        # Check for legacy/conflicting plists
        LEGACY_FOUND=false
        for p in \
            "$HOME/Library/LaunchAgents/io.octos.octos-serve.plist" \
            "$HOME/Library/LaunchAgents/io.octos.serve.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.crew-serve.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.ominix-api.plist" \
            "$HOME/Library/LaunchAgents/io.ominix.octos-serve.plist"; do
            if [ -f "$p" ]; then
                err "legacy plist found: $p"
                hint "Remove: launchctl unload '$p' && rm -f '$p'"
                LEGACY_FOUND=true
            fi
        done
        if [ "$LEGACY_FOUND" = false ]; then
            ok "no legacy/conflicting plists"
        fi
        ;;

    Linux)
        UNIT="/etc/systemd/system/octos-serve.service"
        if [ -f "$UNIT" ]; then
            ok "systemd unit exists: $UNIT"
            if systemctl is-active octos-serve &>/dev/null; then
                ok "service is active"
            else
                warn "service is not active"
                hint "Start: $(svc_hint start serve)"
                hint "Check: $(svc_hint status serve)"
            fi
        else
            warn "no systemd unit found"
            hint "Re-run install.sh to set up the service"
        fi
        ;;
    *)
        warn "service configuration check not supported on $OS"
        ;;
esac

if [ "$CLOUD_MODE" = true ]; then
    # ── Cloud services ──────────────────────────────────────────────
    section "Cloud services"

    for service in frps caddy; do
        case "$OS" in
            Darwin)
                plist="/Library/LaunchDaemons/io.octos.${service}.plist"
                if [ -f "$plist" ]; then
                    ok "${service} plist exists: $plist"
                else
                    err "${service} plist missing: $plist"
                    hint "Re-run cloud-host-deploy.sh"
                fi
                ;;
            Linux)
                unit="/etc/systemd/system/${service}.service"
                if [ -f "$unit" ]; then
                    ok "${service} unit exists: $unit"
                else
                    err "${service} unit missing: $unit"
                    hint "Re-run cloud-host-deploy.sh"
                fi
                ;;
        esac

        if is_service_active "$service"; then
            ok "${service} service is active"
        else
            err "${service} service is not active"
            hint "Check: $(svc_hint status "$service")"
            hint "Start: $(svc_hint start "$service")"
        fi
    done

    # ── Cloud ports ────────────────────────────────────────────────
    section "Cloud ports"
    check_listener 7000 frps "frps control"
    check_listener 8081 frps "frps vhost HTTP"
    check_listener 7500 frps "frps dashboard"
    check_listener 80 caddy "caddy HTTP"
    if [ "$CLOUD_HTTPS_ENABLED" = true ]; then
        check_listener 443 caddy "caddy HTTPS"
    fi

    # ── Cloud routing ──────────────────────────────────────────────
    section "Cloud routing"
    APEX_HTTP_CODE=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1/ 2>/dev/null || echo "000")
    if [ "$APEX_HTTP_CODE" != "000" ] && [ "$APEX_HTTP_CODE" != "000000" ]; then
        ok "http://127.0.0.1/ responds $APEX_HTTP_CODE"
    else
        err "http://127.0.0.1/ is not reachable through Caddy"
        hint "Check caddy logs and service state"
    fi

    if [ -n "$CLOUD_DOMAIN" ]; then
        TENANT_HTTP_CODE=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 3 -H "Host: test.$CLOUD_DOMAIN" http://127.0.0.1/ 2>/dev/null || echo "000")
        if [ "$TENANT_HTTP_CODE" != "000" ] && [ "$TENANT_HTTP_CODE" != "000000" ]; then
            ok "tenant routing via Host header responds $TENANT_HTTP_CODE"
        else
            err "tenant routing via Caddy/frps is not reachable locally"
            hint "Try: curl -H 'Host: test.$CLOUD_DOMAIN' http://127.0.0.1/"
        fi

        if [ "$CLOUD_HTTPS_ENABLED" = true ]; then
            HTTPS_CODE=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 "https://127.0.0.1/" -H "Host: $CLOUD_DOMAIN" 2>/dev/null || echo "000")
            if [ "$HTTPS_CODE" != "000" ] && [ "$HTTPS_CODE" != "000000" ]; then
                ok "HTTPS listener responds locally with Host: $CLOUD_DOMAIN ($HTTPS_CODE)"
            else
                warn "local HTTPS check failed"
                hint "Verify DNS, certificate issuance, and caddy logs"
            fi
        fi
    fi

    # ── SMTP OTP config ────────────────────────────────────────────
    section "SMTP OTP"
    if [ "$SMTP_ENABLED" = true ]; then
        ok "dashboard_auth.smtp configured"
        [ -n "${SMTP_HOST_CFG:-}" ] && ok "SMTP host: $SMTP_HOST_CFG"
        [ -n "${SMTP_PORT_CFG:-}" ] && ok "SMTP port: $SMTP_PORT_CFG"
        [ -n "${SMTP_FROM_CFG:-}" ] && ok "SMTP from: $SMTP_FROM_CFG"
        ok "allow_self_registration: $ALLOW_SELF_REGISTRATION"

        if [ -n "$SMTP_PASSWORD_ENV" ]; then
            case "$OS" in
                Darwin)
                    if [ -f /Library/LaunchDaemons/io.octos.serve.plist ] && grep -q "<key>${SMTP_PASSWORD_ENV}</key>" /Library/LaunchDaemons/io.octos.serve.plist 2>/dev/null; then
                        ok "octos serve plist includes $SMTP_PASSWORD_ENV"
                    else
                        err "octos serve plist does not include $SMTP_PASSWORD_ENV"
                        hint "Re-run cloud-host-deploy.sh to rebuild the service with SMTP env"
                    fi
                    ;;
                Linux)
                    if [ -f /etc/systemd/system/octos-serve.service ] && grep -q "Environment=\"${SMTP_PASSWORD_ENV}=" /etc/systemd/system/octos-serve.service 2>/dev/null; then
                        ok "octos serve unit includes $SMTP_PASSWORD_ENV"
                    else
                        err "octos serve unit does not include $SMTP_PASSWORD_ENV"
                        hint "Re-run cloud-host-deploy.sh to rebuild the service with SMTP env"
                    fi
                    ;;
            esac
        fi
        if [ -f "$DATA_DIR/serve.log" ]; then
            SMTP_AUTH_ERRORS=$(grep -i 'send_otp failed.*535\|send_otp failed.*authentication failed' "$DATA_DIR/serve.log" 2>/dev/null | tail -1 || true)
            if [ -n "$SMTP_AUTH_ERRORS" ]; then
                err "recent SMTP authentication failure detected in serve.log"
                hint "Verify SMTP_USERNAME / SMTP_PASSWORD and reload io.octos.serve"
            fi
        fi
    else
        warn "dashboard_auth.smtp not configured"
        hint "OTP codes will be logged to the console instead of emailed"
    fi
fi

if [ "$CLOUD_MODE" = true ]; then
    # ── Cloud access ───────────────────────────────────────────────
    section "Cloud access"

    ADMIN_OK=false
    [ "$HTTP_CODE" = "200" ] && ADMIN_OK=true

    if [ "$ADMIN_OK" = true ] && is_service_active frps && is_service_active caddy; then
        ok "admin portal works locally and cloud relay services are running"
        if [ -n "$CLOUD_DOMAIN" ]; then
            echo "    Public URL: https://$CLOUD_DOMAIN"
        fi
    elif [ "$ADMIN_OK" = false ]; then
        err "admin portal is not responding locally — fix octos serve first (see above)"
        hint "Cloud relay depends on the local server working first"
    else
        err "admin portal works locally but frps/caddy are not both healthy"
        hint "Check the 'Cloud services', 'Cloud ports', and 'Cloud routing' sections above"
    fi
else
    # ── frpc tunnel ────────────────────────────────────────────────
    section "frpc tunnel"

    TENANT=""
    if [ -f /usr/local/bin/frpc ]; then
        ok "frpc installed: $(/usr/local/bin/frpc --version 2>/dev/null || echo 'unknown version')"
    else
        warn "frpc not installed (tunnel not configured)"
        hint "Re-run install.sh with tunnel options, or use --no-tunnel if not needed"
    fi

    FRPC_PID=$(pgrep -x frpc 2>/dev/null || true)
    if [ -n "$FRPC_PID" ]; then
        ok "frpc running (PID: $FRPC_PID)"
    else
        if [ -f /usr/local/bin/frpc ]; then
            err "frpc installed but not running"
            hint "Start: $(svc_hint start frpc)"
        fi
    fi

    if [ -f /etc/frp/frpc.toml ]; then
        ok "frpc config: /etc/frp/frpc.toml"
        TENANT=$(grep 'customDomains' /etc/frp/frpc.toml 2>/dev/null | head -1 | sed 's/.*\["\(.*\)"\].*/\1/')
        if [ -n "$TENANT" ]; then
            echo "    Tunnel: https://$TENANT"
        fi
        if grep -q 'CHANGE_ME' /etc/frp/frpc.toml 2>/dev/null; then
            warn "frpc config contains placeholder token (CHANGE_ME)"
            hint "Update: sudo nano /etc/frp/frpc.toml"
            hint "Or re-run: bash install.sh --tenant-name <name> --frps-token <token>"
        fi
    elif [ -f /usr/local/bin/frpc ]; then
        warn "frpc installed but no config at /etc/frp/frpc.toml"
        hint "Re-run install.sh with --tenant-name and --frps-token"
    fi

    if [ -f /var/log/frpc.log ]; then
        FRPC_ERRORS=$(tail -20 /var/log/frpc.log 2>/dev/null | grep -i "error\|failed\|refused" | tail -3)
        if [ -n "$FRPC_ERRORS" ]; then
            warn "recent frpc errors:"
            echo "$FRPC_ERRORS" | while read -r line; do echo "      $line"; done
            hint "Full log: tail -50 /var/log/frpc.log"
        fi
    fi

    # ── Remote access ──────────────────────────────────────────────
    section "Remote access"

    ADMIN_OK=false
    [ "$HTTP_CODE" = "200" ] && ADMIN_OK=true

    FRPC_OK=false
    [ -n "${FRPC_PID:-}" ] && FRPC_OK=true

    if [ "$ADMIN_OK" = true ] && [ "$FRPC_OK" = true ]; then
        ok "admin portal works locally and frpc tunnel is running"
        if [ -n "$TENANT" ]; then
            echo "    Remote URL: https://$TENANT"
        fi
    elif [ "$ADMIN_OK" = true ] && [ "$FRPC_OK" = false ]; then
        err "admin portal works locally but frpc is NOT running — remote access is down"
        if [ ! -f /usr/local/bin/frpc ]; then
            hint "frpc was never installed. Set up the tunnel:"
            hint "  Re-run install.sh with --tenant-name <name> --frps-token <token>"
        elif [ ! -f /etc/frp/frpc.toml ]; then
            hint "frpc is installed but not configured"
            hint "  Re-run install.sh with --tenant-name <name> --frps-token <token>"
        else
            hint "frpc is installed and configured but the process is not running"
            hint "  Start: $(svc_hint start frpc)"
        fi
    elif [ "$ADMIN_OK" = false ]; then
        err "admin portal is not responding locally — fix octos serve first (see above)"
        hint "Remote access depends on the local server working first"
    fi
fi

# ── Serve logs ───────────────────────────────────────────────────
section "Recent serve logs"

SERVE_LOG="$DATA_DIR/serve.log"
if [ -f "$SERVE_LOG" ]; then
    SERVE_ERRORS=$(tail -30 "$SERVE_LOG" 2>/dev/null | grep -i "error\|panic\|Address already in use\|send_otp failed" | tail -5)
    if [ -n "$SERVE_ERRORS" ]; then
        warn "recent errors in serve.log:"
        echo "$SERVE_ERRORS" | while read -r line; do echo "      $line"; done
    else
        ok "no recent errors in serve.log"
    fi
    echo "    Last 3 lines:"
    tail -3 "$SERVE_LOG" 2>/dev/null | while read -r line; do echo "      $line"; done
else
    warn "serve.log not found at $SERVE_LOG"
fi

# ── Runtime dependencies ─────────────────────────────────────────
section "Runtime dependencies"

command -v git &>/dev/null && ok "git $(git --version | awk '{print $3}')" || warn "git not found"
command -v node &>/dev/null && ok "Node.js $(node --version)" || warn "Node.js not found (optional)"
command -v ffmpeg &>/dev/null && ok "ffmpeg found" || warn "ffmpeg not found (optional)"

CHROME_FOUND=false
for chrome_bin in "google-chrome" "google-chrome-stable" "chromium-browser" "chromium"; do
    if command -v "$chrome_bin" &>/dev/null; then
        ok "Browser: $chrome_bin"
        CHROME_FOUND=true
        break
    fi
done
if [ "$CHROME_FOUND" = false ] && [ "$OS" = "Darwin" ]; then
    for app in "/Applications/Google Chrome.app" "/Applications/Chromium.app"; do
        if [ -d "$app" ]; then
            ok "Browser: $app"
            CHROME_FOUND=true
            break
        fi
    done
fi
[ "$CHROME_FOUND" = false ] && warn "Chromium/Chrome not found (optional)"

# ── Summary ──────────────────────────────────────────────────────
section "Summary"
if [ "$DOCTOR_ISSUES" -eq 0 ]; then
    echo "    All checks passed. Everything looks healthy."
else
    echo "    Found $DOCTOR_ISSUES issue(s). Review the hints above to fix them."
fi
echo ""
