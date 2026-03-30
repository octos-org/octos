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

# ── Output helpers ──────────────────────────────────────────────────
section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
hint()    { echo "          -> $1"; }
DOCTOR_ISSUES=0
err()     { echo "    FAIL: $1"; DOCTOR_ISSUES=$((DOCTOR_ISSUES + 1)); }

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

OCTOS_PID=$(pgrep -f "octos serve" 2>/dev/null | head -1 || true)
if [ -n "$OCTOS_PID" ]; then
    OCTOS_CMD=$(ps -p "$OCTOS_PID" -o args= 2>/dev/null || true)
    ok "running (PID: $OCTOS_PID)"
    echo "    CMD: $OCTOS_CMD"
else
    err "octos serve is not running"
    hint "Start: $(svc_hint start serve)"
    hint "Or manually: $PREFIX/octos serve --port 8080 --host 0.0.0.0"
fi

# ── Port 8080 ────────────────────────────────────────────────────
section "Port 8080"

PORT_CMD=""
PORT_PID=""
PORT_CHECK_AVAILABLE=false
if command -v lsof &>/dev/null; then
    PORT_CHECK_AVAILABLE=true
    PORT_OWNER=$(lsof -i :8080 -P -n 2>/dev/null | grep LISTEN | head -1 || true)
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
    000)
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

# ── frpc tunnel ──────────────────────────────────────────────────
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

# Check frpc logs for errors
if [ -f /var/log/frpc.log ]; then
    FRPC_ERRORS=$(tail -20 /var/log/frpc.log 2>/dev/null | grep -i "error\|failed\|refused" | tail -3)
    if [ -n "$FRPC_ERRORS" ]; then
        warn "recent frpc errors:"
        echo "$FRPC_ERRORS" | while read -r line; do echo "      $line"; done
        hint "Full log: tail -50 /var/log/frpc.log"
    fi
fi

# ── Remote access ────────────────────────────────────────────────
section "Remote access"

ADMIN_OK=false
[ "$HTTP_CODE" = "200" ] && ADMIN_OK=true

FRPC_OK=false
[ -n "$FRPC_PID" ] && FRPC_OK=true

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

# ── Serve logs ───────────────────────────────────────────────────
section "Recent serve logs"

SERVE_LOG="$DATA_DIR/serve.log"
if [ -f "$SERVE_LOG" ]; then
    SERVE_ERRORS=$(tail -30 "$SERVE_LOG" 2>/dev/null | grep -i "error\|panic\|Address already in use" | tail -5)
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
