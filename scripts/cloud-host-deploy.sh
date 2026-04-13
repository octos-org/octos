#!/usr/bin/env bash
# cloud-host-deploy.sh — Bootstrap a server as an Octos cloud/host relay.
# Supports Linux (systemd) and macOS (launchd).
# Reuses install.sh for octos serve, then provisions frps and Caddy.
#
# Usage:
#   ./scripts/cloud-host-deploy.sh
#   ./scripts/cloud-host-deploy.sh --domain octos.example.com --https --dns-provider cloudflare
#   ./scripts/cloud-host-deploy.sh --config ./cloud-bootstrap.env --non-interactive

set -eEuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SCRIPT="$ROOT_DIR/scripts/install.sh"
FRPS_SCRIPT="$ROOT_DIR/scripts/frp/setup-frps.sh"
CADDY_SCRIPT="$ROOT_DIR/scripts/frp/setup-caddy.sh"

VERSION="latest"
PREFIX="${OCTOS_PREFIX:-$HOME/.octos/bin}"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"
PORT="8080"
AUTH_TOKEN=""
FRPS_TOKEN="${FRPS_TOKEN:-}"
TUNNEL_DOMAIN="${TUNNEL_DOMAIN:-}"
FRPS_SERVER="${FRPS_SERVER:-}"
ENABLE_HTTPS="${ENABLE_HTTPS:-}"
DNS_PROVIDER="${DNS_PROVIDER:-}"
ENABLE_SMTP="${ENABLE_SMTP:-}"
SMTP_HOST="${SMTP_HOST:-}"
SMTP_PORT="${SMTP_PORT:-}"
SMTP_USERNAME="${SMTP_USERNAME:-}"
SMTP_FROM="${SMTP_FROM:-}"
SMTP_PASSWORD="${SMTP_PASSWORD:-}"
ALLOW_SELF_REGISTRATION="${ALLOW_SELF_REGISTRATION:-}"
INSTALL_DEPS=false
NONINTERACTIVE=false
DRY_RUN=false
UNINSTALL=false
PURGE=false
CONFIG_FILE=""
STATE_FILE=""

FRPS_BIND_PORT="${FRPS_BIND_PORT:-7000}"
FRPS_VHOST_HTTP_PORT="${FRPS_VHOST_HTTP_PORT:-8081}"
FRPS_VHOST_HTTPS_PORT="${FRPS_VHOST_HTTPS_PORT:-8443}"
FRPS_DASHBOARD_PORT="${FRPS_DASHBOARD_PORT:-7500}"
FRPS_SSH_PORT_START="${FRPS_SSH_PORT_START:-6001}"
FRPS_SSH_PORT_END="${FRPS_SSH_PORT_END:-6999}"

needval() {
    if [ $# -lt 2 ] || case "$2" in -*) true ;; *) false ;; esac; then
        echo "ERROR: $1 requires a value" >&2
        exit 1
    fi
}

normalize_path() {
    local path="$1"
    case "$path" in
        "~")
            printf '%s\n' "$HOME"
            ;;
        "~/"*)
            printf '%s/%s\n' "$HOME" "${path#"~/"}"
            ;;
        /*)
            printf '%s\n' "$path"
            ;;
        *)
            printf '%s/%s\n' "$PWD" "$path"
            ;;
    esac
}

load_config_file() {
    local path="$1"
    [ -f "$path" ] || { echo "ERROR: config file not found: $path" >&2; exit 1; }
    # Admin-owned bootstrap file. Source for silent installs and reruns.
    # shellcheck disable=SC1090
    . "$path"
}

CURRENT_STEP=""
trap 'echo ""; echo "    FAILED${CURRENT_STEP:+ during: $CURRENT_STEP}" >&2; echo "    The deploy did not complete. Fix the error above and re-run." >&2' ERR

section() { CURRENT_STEP="$1"; echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
err()     { echo "    ERROR: $1" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --config)            needval "$@"; CONFIG_FILE="$2"; load_config_file "$2"; shift 2 ;;
        --version)           needval "$@"; VERSION="$2"; shift 2 ;;
        --prefix)            needval "$@"; PREFIX="$2"; shift 2 ;;
        --data-dir)          needval "$@"; DATA_DIR="$2"; shift 2 ;;
        --state-file)        needval "$@"; STATE_FILE="$2"; shift 2 ;;
        --port)              needval "$@"; PORT="$2"; shift 2 ;;
        --auth-token)        needval "$@"; AUTH_TOKEN="$2"; shift 2 ;;
        --frps-token)        needval "$@"; FRPS_TOKEN="$2"; shift 2 ;;
        --domain)            needval "$@"; TUNNEL_DOMAIN="$2"; shift 2 ;;
        --frps-server)       needval "$@"; FRPS_SERVER="$2"; shift 2 ;;
        --https)             ENABLE_HTTPS=true; shift ;;
        --http-only)         ENABLE_HTTPS=false; shift ;;
        --dns-provider)      needval "$@"; DNS_PROVIDER="$2"; shift 2 ;;
        --smtp)              ENABLE_SMTP=true; shift ;;
        --no-smtp)           ENABLE_SMTP=false; shift ;;
        --install-deps)      INSTALL_DEPS=true; shift ;;
        --uninstall)         UNINSTALL=true; shift ;;
        --purge)             PURGE=true; shift ;;
        --non-interactive|--yes) NONINTERACTIVE=true; shift ;;
        --dry-run)           DRY_RUN=true; shift ;;
        --help|-h)
            cat <<'HELPEOF'
cloud-host-deploy.sh — Bootstrap a server (Linux or macOS) as an Octos cloud/host relay.

Usage:
  ./scripts/cloud-host-deploy.sh
  ./scripts/cloud-host-deploy.sh --domain octos.example.com --https --dns-provider cloudflare
  ./scripts/cloud-host-deploy.sh --config ./cloud-bootstrap.env --non-interactive

Options:
  --config PATH          Source a shell-style config file for silent install
  --version TAG          octos release version passed to install.sh (default: latest)
  --prefix DIR           Binary install prefix (default: ~/.octos/bin)
  --data-dir DIR         Octos data dir and config home (default: ~/.octos)
  --state-file PATH      Persist rerun settings (default: ./cloud-bootstrap.env)
  --port PORT            octos serve port behind Caddy (default: 8080)
  --auth-token TOKEN     Admin auth token for the dashboard
  --frps-token TOKEN     Shared FRPS auth token for all tenant tunnels
  --domain DOMAIN        Base public domain for signup and tenant subdomains
  --frps-server ADDR     Address tenants use to reach frps (default: same as --domain)
  --https                Enable HTTPS with wildcard certs via setup-caddy.sh
  --http-only            Force HTTP-only Caddy setup
  --dns-provider NAME    DNS provider for HTTPS: cloudflare, route53, digitalocean, godaddy
  --smtp                 Configure SMTP for dashboard OTP emails
  --no-smtp              Disable SMTP for dashboard OTP emails
  --install-deps         Forward to install.sh to install missing runtime deps
  --uninstall            Remove octos serve, frps, and Caddy host services/config
  --purge                Delete the data dir only (preserves bootstrap state)
  --non-interactive      Fail instead of prompting for missing values
  --dry-run              Write config files but print commands instead of executing them

Config file format:
  Shell-style KEY=value entries, for example:
    TUNNEL_DOMAIN=octos.example.com
    FRPS_SERVER=relay.octos.example.com
    ENABLE_HTTPS=true
    DNS_PROVIDER=cloudflare
    CF_API_TOKEN=...
    ENABLE_SMTP=true
    SMTP_HOST=smtp.gmail.com
    SMTP_PORT=465
    SMTP_USERNAME=your-email@gmail.com
    SMTP_FROM=your-email@gmail.com
    # Export SMTP_PASSWORD in your shell before running the script
HELPEOF
            exit 0
            ;;
        *)
            echo "ERROR: unknown option: $1" >&2
            exit 1
            ;;
    esac
done

PREFIX="$(normalize_path "$PREFIX")"
DATA_DIR="$(normalize_path "$DATA_DIR")"
STATE_FILE="${STATE_FILE:-$PWD/cloud-bootstrap.env}"
STATE_FILE="$(normalize_path "$STATE_FILE")"

# Auto-load previous state file on re-runs (unless --config was already given)
if [ -z "$CONFIG_FILE" ] && [ -f "$STATE_FILE" ]; then
    load_config_file "$STATE_FILE"
    ok "loaded previous settings from $STATE_FILE"
fi

validate() {
    local name="$1" value="$2" pattern="$3"
    if ! printf '%s' "$value" | grep -qE "^${pattern}\$"; then
        err "invalid $name: '$value' (must match ${pattern})"
    fi
}

prompt_value() {
    local var_name="$1"
    local prompt="$2"
    local default_value="${3:-}"
    local current="${!var_name:-}"
    if [ -n "$current" ]; then
        default_value="$current"
    fi
    if [ "$NONINTERACTIVE" = true ]; then
        if [ -n "$default_value" ]; then
            printf -v "$var_name" '%s' "$default_value"
            checkpoint_state_file
            return 0
        fi
        err "missing required value for $var_name"
    fi
    if [ -n "$default_value" ]; then
        printf "    %s [%s]: " "$prompt" "$default_value"
    else
        printf "    %s: " "$prompt"
    fi
    local answer=""
    read -r answer < /dev/tty
    if [ -z "$answer" ]; then
        answer="$default_value"
    fi
    [ -n "$answer" ] || err "$var_name is required"
    printf -v "$var_name" '%s' "$answer"
    checkpoint_state_file
}

prompt_yes_no() {
    local var_name="$1"
    local prompt="$2"
    local default_value="$3"
    local current="${!var_name:-}"
    if [ "$current" = true ] || [ "$current" = false ]; then
        default_value="$current"
    fi
    if [ "$NONINTERACTIVE" = true ]; then
        printf -v "$var_name" '%s' "$default_value"
        checkpoint_state_file
        return 0
    fi
    local hint="y/N"
    if [ "$default_value" = true ]; then
        hint="Y/n"
    fi
    printf "    %s [%s]: " "$prompt" "$hint"
    local answer=""
    read -r answer < /dev/tty
    case "$answer" in
        y|Y|yes|YES) printf -v "$var_name" 'true' ;;
        n|N|no|NO)   printf -v "$var_name" 'false' ;;
        "")          printf -v "$var_name" '%s' "$default_value" ;;
        *)           err "please answer yes or no" ;;
    esac
    checkpoint_state_file
}

prompt_secret() {
    local var_name="$1"
    local prompt="$2"
    local current="${!var_name:-}"
    if [ -n "$current" ]; then
        checkpoint_state_file
        return 0
    fi
    if [ "$NONINTERACTIVE" = true ]; then
        err "missing required secret for $var_name"
    fi
    printf "    %s: " "$prompt"
    local answer=""
    read -rs answer < /dev/tty
    printf '\n'
    [ -n "$answer" ] || err "$var_name is required"
    printf -v "$var_name" '%s' "$answer"
    checkpoint_state_file
}

export_dns_env() {
    [ -n "${CF_API_TOKEN:-}" ] && export CF_API_TOKEN || true
    [ -n "${AWS_ACCESS_KEY_ID:-}" ] && export AWS_ACCESS_KEY_ID || true
    [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] && export AWS_SECRET_ACCESS_KEY || true
    [ -n "${DO_AUTH_TOKEN:-}" ] && export DO_AUTH_TOKEN || true
    [ -n "${GODADDY_API_KEY:-}" ] && export GODADDY_API_KEY || true
    [ -n "${GODADDY_API_SECRET:-}" ] && export GODADDY_API_SECRET || true
}

export_smtp_env() {
    [ -n "${SMTP_HOST:-}" ] && export SMTP_HOST || true
    [ -n "${SMTP_PORT:-}" ] && export SMTP_PORT || true
    [ -n "${SMTP_USERNAME:-}" ] && export SMTP_USERNAME || true
    [ -n "${SMTP_FROM:-}" ] && export SMTP_FROM || true
    [ -n "${SMTP_PASSWORD:-}" ] && export SMTP_PASSWORD || true
}

load_smtp_defaults_from_config() {
    local config_path="$DATA_DIR/config.json"
    [ -f "$config_path" ] || return 0
    command -v python3 >/dev/null 2>&1 || return 0

    local smtp_values=""
    smtp_values="$(python3 - "$config_path" <<'PYEOF'
import json
import sys

config_path = sys.argv[1]
with open(config_path) as fh:
    data = json.load(fh)

dashboard_auth = data.get("dashboard_auth")
if not isinstance(dashboard_auth, dict):
    sys.exit(0)

smtp = dashboard_auth.get("smtp")
if not isinstance(smtp, dict):
    sys.exit(0)

def emit(key, value):
    if value is None:
        value = ""
    print(f"{key}={value}")

emit("ENABLE_SMTP", "true")
emit("SMTP_HOST", smtp.get("host", ""))
emit("SMTP_PORT", smtp.get("port", ""))
emit("SMTP_USERNAME", smtp.get("username", ""))
emit("SMTP_FROM", smtp.get("from_address", ""))
emit("ALLOW_SELF_REGISTRATION", "true" if dashboard_auth.get("allow_self_registration") else "false")
PYEOF
    )"

    [ -n "$smtp_values" ] || return 0
    while IFS='=' read -r key value; do
        case "$key" in
            ENABLE_SMTP) [ -n "${ENABLE_SMTP:-}" ] || ENABLE_SMTP="$value" ;;
            SMTP_HOST) [ -n "${SMTP_HOST:-}" ] || SMTP_HOST="$value" ;;
            SMTP_PORT) [ -n "${SMTP_PORT:-}" ] || SMTP_PORT="$value" ;;
            SMTP_USERNAME) [ -n "${SMTP_USERNAME:-}" ] || SMTP_USERNAME="$value" ;;
            SMTP_FROM) [ -n "${SMTP_FROM:-}" ] || SMTP_FROM="$value" ;;
            ALLOW_SELF_REGISTRATION)
                [ -n "${ALLOW_SELF_REGISTRATION:-}" ] || ALLOW_SELF_REGISTRATION="$value"
                ;;
        esac
    done <<EOF
$smtp_values
EOF
}

detect_provider_defaults() {
    if [ -n "${OPENAI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="openai"; DETECTED_MODEL="gpt-4.1-mini"; DETECTED_ENV="OPENAI_API_KEY"
    elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
        DETECTED_PROVIDER="anthropic"; DETECTED_MODEL="claude-sonnet-4-20250514"; DETECTED_ENV="ANTHROPIC_API_KEY"
    elif [ -n "${GEMINI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="gemini"; DETECTED_MODEL="gemini-2.5-flash"; DETECTED_ENV="GEMINI_API_KEY"
    elif [ -n "${DEEPSEEK_API_KEY:-}" ]; then
        DETECTED_PROVIDER="deepseek"; DETECTED_MODEL="deepseek-chat"; DETECTED_ENV="DEEPSEEK_API_KEY"
    elif [ -n "${KIMI_API_KEY:-}" ]; then
        DETECTED_PROVIDER="moonshot"; DETECTED_MODEL="kimi-k2.5"; DETECTED_ENV="KIMI_API_KEY"
    elif [ -n "${DASHSCOPE_API_KEY:-}" ]; then
        DETECTED_PROVIDER="dashscope"; DETECTED_MODEL="qwen3.5-plus"; DETECTED_ENV="DASHSCOPE_API_KEY"
    else
        DETECTED_PROVIDER="openai"; DETECTED_MODEL="gpt-4.1-mini"; DETECTED_ENV="OPENAI_API_KEY"
    fi
}

write_cloud_config() {
    local config_path="$DATA_DIR/config.json"
    mkdir -p "$DATA_DIR"
    detect_provider_defaults

    if [ -f "$config_path" ]; then
        if command -v python3 >/dev/null 2>&1; then
            python3 - "$config_path" "$TUNNEL_DOMAIN" "$FRPS_SERVER" "$AUTH_TOKEN" \
                "$DETECTED_PROVIDER" "$DETECTED_MODEL" "$DETECTED_ENV" \
                "$ENABLE_SMTP" "$SMTP_HOST" "$SMTP_PORT" "$SMTP_USERNAME" "$SMTP_FROM" "$ALLOW_SELF_REGISTRATION" <<'PYEOF'
import json
import pathlib
import sys

config_path = pathlib.Path(sys.argv[1])
tunnel_domain = sys.argv[2]
frps_server = sys.argv[3]
auth_token = sys.argv[4]
provider = sys.argv[5]
model = sys.argv[6]
api_key_env = sys.argv[7]
enable_smtp = sys.argv[8].lower() == "true"
smtp_host = sys.argv[9]
smtp_port = int(sys.argv[10]) if sys.argv[10] else 465
smtp_username = sys.argv[11]
smtp_from = sys.argv[12]
allow_self_registration = sys.argv[13].lower() == "true"

data = {}
if config_path.exists():
    with config_path.open() as fh:
        data = json.load(fh)

data.setdefault("provider", provider)
data.setdefault("model", model)
data.setdefault("api_key_env", api_key_env)
data["mode"] = "cloud"
data["tunnel_domain"] = tunnel_domain
data["frps_server"] = frps_server
data["auth_token"] = auth_token

dashboard_auth = data.get("dashboard_auth")
if enable_smtp:
    if not isinstance(dashboard_auth, dict):
        dashboard_auth = {}
    dashboard_auth["smtp"] = {
        "host": smtp_host,
        "port": smtp_port,
        "username": smtp_username,
        "password_env": "SMTP_PASSWORD",
        "from_address": smtp_from,
    }
    dashboard_auth["session_expiry_hours"] = dashboard_auth.get("session_expiry_hours", 24)
    dashboard_auth["allow_self_registration"] = allow_self_registration
    data["dashboard_auth"] = dashboard_auth
elif isinstance(dashboard_auth, dict) and "smtp" in dashboard_auth:
    dashboard_auth.pop("smtp", None)
    if dashboard_auth:
        data["dashboard_auth"] = dashboard_auth
    else:
        data.pop("dashboard_auth", None)

config_path.write_text(json.dumps(data, indent=2) + "\n")
PYEOF
        else
            err "python3 is required to update existing $config_path safely"
        fi
    else
        if [ "$ENABLE_SMTP" = true ]; then
            cat >"$config_path" <<EOF
{
  "provider": "$DETECTED_PROVIDER",
  "model": "$DETECTED_MODEL",
  "api_key_env": "$DETECTED_ENV",
  "mode": "cloud",
  "tunnel_domain": "$TUNNEL_DOMAIN",
  "frps_server": "$FRPS_SERVER",
  "auth_token": "$AUTH_TOKEN",
  "dashboard_auth": {
    "smtp": {
      "host": "$SMTP_HOST",
      "port": $SMTP_PORT,
      "username": "$SMTP_USERNAME",
      "password_env": "SMTP_PASSWORD",
      "from_address": "$SMTP_FROM"
    },
    "session_expiry_hours": 24,
    "allow_self_registration": $ALLOW_SELF_REGISTRATION
  }
}
EOF
        else
            cat >"$config_path" <<EOF
{
  "provider": "$DETECTED_PROVIDER",
  "model": "$DETECTED_MODEL",
  "api_key_env": "$DETECTED_ENV",
  "mode": "cloud",
  "tunnel_domain": "$TUNNEL_DOMAIN",
  "frps_server": "$FRPS_SERVER",
  "auth_token": "$AUTH_TOKEN"
}
EOF
        fi
    fi
    chmod 600 "$config_path"
    ok "cloud config written to $config_path"
}

write_state_file() {
    mkdir -p "$(dirname "$STATE_FILE")"
    cat >"$STATE_FILE" <<EOF
# Generated by cloud-host-deploy.sh
VERSION=$VERSION
PREFIX=$PREFIX
DATA_DIR=$DATA_DIR
PORT=$PORT
AUTH_TOKEN=$AUTH_TOKEN
FRPS_TOKEN=$FRPS_TOKEN
TUNNEL_DOMAIN=$TUNNEL_DOMAIN
FRPS_SERVER=$FRPS_SERVER
ENABLE_HTTPS=$ENABLE_HTTPS
DNS_PROVIDER=$DNS_PROVIDER
ENABLE_SMTP=$ENABLE_SMTP
SMTP_HOST=$SMTP_HOST
SMTP_PORT=$SMTP_PORT
SMTP_USERNAME=$SMTP_USERNAME
SMTP_FROM='$SMTP_FROM'
ALLOW_SELF_REGISTRATION=$ALLOW_SELF_REGISTRATION
FRPS_BIND_PORT=$FRPS_BIND_PORT
FRPS_VHOST_HTTP_PORT=$FRPS_VHOST_HTTP_PORT
FRPS_VHOST_HTTPS_PORT=$FRPS_VHOST_HTTPS_PORT
FRPS_DASHBOARD_PORT=$FRPS_DASHBOARD_PORT
FRPS_SSH_PORT_START=$FRPS_SSH_PORT_START
FRPS_SSH_PORT_END=$FRPS_SSH_PORT_END
EOF
    chmod 600 "$STATE_FILE"
    ok "state file written to $STATE_FILE"
}

checkpoint_state_file() {
    mkdir -p "$(dirname "$STATE_FILE")"
    cat >"$STATE_FILE" <<EOF
# Generated by cloud-host-deploy.sh
VERSION=$VERSION
PREFIX=$PREFIX
DATA_DIR=$DATA_DIR
PORT=$PORT
AUTH_TOKEN=$AUTH_TOKEN
FRPS_TOKEN=$FRPS_TOKEN
TUNNEL_DOMAIN=$TUNNEL_DOMAIN
FRPS_SERVER=$FRPS_SERVER
ENABLE_HTTPS=$ENABLE_HTTPS
DNS_PROVIDER=$DNS_PROVIDER
ENABLE_SMTP=$ENABLE_SMTP
SMTP_HOST=$SMTP_HOST
SMTP_PORT=$SMTP_PORT
SMTP_USERNAME=$SMTP_USERNAME
SMTP_FROM='$SMTP_FROM'
ALLOW_SELF_REGISTRATION=$ALLOW_SELF_REGISTRATION
FRPS_BIND_PORT=$FRPS_BIND_PORT
FRPS_VHOST_HTTP_PORT=$FRPS_VHOST_HTTP_PORT
FRPS_VHOST_HTTPS_PORT=$FRPS_VHOST_HTTPS_PORT
FRPS_DASHBOARD_PORT=$FRPS_DASHBOARD_PORT
FRPS_SSH_PORT_START=$FRPS_SSH_PORT_START
FRPS_SSH_PORT_END=$FRPS_SSH_PORT_END
EOF
    chmod 600 "$STATE_FILE"
}

run_cmd() {
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

run_cmd_best_effort() {
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@" 2>/dev/null || true
    fi
}

run_install() {
    section "Installing octos serve"
    local cmd=("$INSTALL_SCRIPT" --version "$VERSION" --prefix "$PREFIX" --port "$PORT" --auth-token "$AUTH_TOKEN")
    [ "$INSTALL_DEPS" = true ] && cmd+=(--install-deps)
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: OCTOS_HOME=%q' "$DATA_DIR"
        if [ "$ENABLE_SMTP" = true ]; then
            printf ' SMTP_HOST=%q SMTP_PORT=%q SMTP_USERNAME=%q SMTP_FROM=%q SMTP_PASSWORD=***' \
                "$SMTP_HOST" "$SMTP_PORT" "$SMTP_USERNAME" "$SMTP_FROM"
        fi
        printf ' %q' "${cmd[@]}"
        printf '\n'
    else
        [ "$ENABLE_SMTP" = true ] && export_smtp_env
        OCTOS_HOME="$DATA_DIR" FRPS_TOKEN="$FRPS_TOKEN" "${cmd[@]}"
    fi
}

run_setup_frps() {
    section "Installing frps"
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: TUNNEL_DOMAIN=%q OCTOS_SERVE_PORT=%q FRPS_TOKEN=*** FRPS_BIND_PORT=%q FRPS_VHOST_HTTP_PORT=%q FRPS_VHOST_HTTPS_PORT=%q FRPS_DASHBOARD_PORT=%q FRPS_SSH_PORT_START=%q FRPS_SSH_PORT_END=%q %q\n' \
            "$TUNNEL_DOMAIN" "$PORT" "$FRPS_BIND_PORT" "$FRPS_VHOST_HTTP_PORT" "$FRPS_VHOST_HTTPS_PORT" \
            "$FRPS_DASHBOARD_PORT" "$FRPS_SSH_PORT_START" "$FRPS_SSH_PORT_END" "$FRPS_SCRIPT"
    else
        TUNNEL_DOMAIN="$TUNNEL_DOMAIN" \
        OCTOS_SERVE_PORT="$PORT" \
        FRPS_TOKEN="$FRPS_TOKEN" \
        FRPS_BIND_PORT="$FRPS_BIND_PORT" \
        FRPS_VHOST_HTTP_PORT="$FRPS_VHOST_HTTP_PORT" \
        FRPS_VHOST_HTTPS_PORT="$FRPS_VHOST_HTTPS_PORT" \
        FRPS_DASHBOARD_PORT="$FRPS_DASHBOARD_PORT" \
        FRPS_SSH_PORT_START="$FRPS_SSH_PORT_START" \
        FRPS_SSH_PORT_END="$FRPS_SSH_PORT_END" \
        "$FRPS_SCRIPT"
    fi
}

run_setup_caddy() {
    section "Installing Caddy"
    local cmd=("$CADDY_SCRIPT")
    if [ "$ENABLE_HTTPS" = true ]; then
        cmd+=(--https --dns-provider "$DNS_PROVIDER")
    fi
    cmd+=(--domain "$TUNNEL_DOMAIN")

    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: TUNNEL_DOMAIN=%q OCTOS_SERVE_PORT=%q FRPS_VHOST_HTTP_PORT=%q' \
            "$TUNNEL_DOMAIN" "$PORT" "$FRPS_VHOST_HTTP_PORT"
        printf ' %q' "${cmd[@]}"
        printf '\n'
    else
        export_dns_env
        TUNNEL_DOMAIN="$TUNNEL_DOMAIN" \
        OCTOS_SERVE_PORT="$PORT" \
        FRPS_VHOST_HTTP_PORT="$FRPS_VHOST_HTTP_PORT" \
        "${cmd[@]}"
    fi
}

run_install_uninstall() {
    section "Removing octos serve"
    local cmd=("$INSTALL_SCRIPT" --prefix "$PREFIX" --uninstall)
    if [ -n "$PORT" ]; then
        cmd+=(--port "$PORT")
    fi
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: OCTOS_HOME=%q' "$DATA_DIR"
        printf ' %q' "${cmd[@]}"
        printf '\n'
    else
        OCTOS_HOME="$DATA_DIR" INSTALL_SUPPRESS_DATA_DIR_HINT=1 "${cmd[@]}"
    fi
}

run_uninstall_frps() {
    section "Removing frps"
    case "$OS" in
        Darwin)
            run_cmd_best_effort sudo launchctl unload /Library/LaunchDaemons/io.octos.frps.plist
            run_cmd_best_effort sudo rm -f /Library/LaunchDaemons/io.octos.frps.plist
            ;;
        Linux)
            run_cmd_best_effort sudo systemctl stop frps.service
            run_cmd_best_effort sudo systemctl disable frps.service
            run_cmd_best_effort sudo rm -f /etc/systemd/system/frps.service
            run_cmd_best_effort sudo systemctl daemon-reload
            ;;
    esac
    run_cmd_best_effort sudo rm -f /usr/local/bin/frps
    run_cmd_best_effort sudo rm -rf /etc/frp
    run_cmd_best_effort sudo rm -f /var/log/frps.log
}

run_uninstall_caddy() {
    section "Removing Caddy host service"
    case "$OS" in
        Darwin)
            run_cmd_best_effort sudo launchctl unload /Library/LaunchDaemons/io.octos.caddy.plist
            run_cmd_best_effort sudo rm -f /Library/LaunchDaemons/io.octos.caddy.plist
            ;;
        Linux)
            run_cmd_best_effort sudo systemctl stop caddy.service
            run_cmd_best_effort sudo systemctl disable caddy.service
            run_cmd_best_effort sudo rm -f /etc/systemd/system/caddy.service
            run_cmd_best_effort sudo systemctl daemon-reload
            ;;
    esac
    run_cmd_best_effort sudo rm -f /etc/caddy/Caddyfile
    run_cmd_best_effort sudo rmdir /etc/caddy
    run_cmd_best_effort sudo rm -f /var/log/caddy.log
}

run_host_purge() {
    if [ "$DRY_RUN" = false ]; then
        section "Checking sudo access"
        if ! sudo -v 2>/dev/null; then
            err "sudo access is required to remove local state under $DATA_DIR."
        fi
        ok "sudo credentials cached"
    fi

    section "Purging local state"
    run_cmd_best_effort sudo rm -rf "$DATA_DIR"

    section "Complete"
    if [ "$UNINSTALL" = true ]; then
        echo "    Removed host services for octos serve, frps, and Caddy."
    else
        echo "    Purged local state only."
        echo "    Preserved installed services and binaries."
    fi
    echo "    Purged data dir:    $DATA_DIR"
    echo "    Preserved bootstrap state: $STATE_FILE"
}

run_host_uninstall() {
    if [ "$DRY_RUN" = false ]; then
        section "Checking sudo access"
        if ! sudo -v 2>/dev/null; then
            err "sudo access is required to remove system services (frps, Caddy, octos serve)."
        fi
        ok "sudo credentials cached"
    fi

    run_install_uninstall
    [ "$DRY_RUN" = true ] || sudo -v 2>/dev/null || true
    run_uninstall_frps
    [ "$DRY_RUN" = true ] || sudo -v 2>/dev/null || true
    run_uninstall_caddy

    if [ "$PURGE" = false ]; then
        section "Complete"
        echo "    Removed host services for octos serve, frps, and Caddy."
        echo "    Preserved data dir: $DATA_DIR"
        echo "    Preserved bootstrap state: $STATE_FILE"
        echo "    To remove them too, re-run with:"
        echo "      bash scripts/cloud-host-deploy.sh --uninstall --purge"
    fi
}

load_smtp_defaults_from_config

OS="$(uname -s)"
case "$OS" in
    Linux|Darwin) ;;
    *)
        if [ "$DRY_RUN" = true ]; then
            warn "dry-run mode on unsupported OS ($OS); cloud bootstrap supports Linux and macOS"
        else
            err "cloud host bootstrap supports Linux and macOS only (detected: $OS)"
        fi
        ;;
esac

[ -f "$INSTALL_SCRIPT" ] || err "missing install script: $INSTALL_SCRIPT"
[ -f "$FRPS_SCRIPT" ] || err "missing frps setup script: $FRPS_SCRIPT"
[ -f "$CADDY_SCRIPT" ] || err "missing Caddy setup script: $CADDY_SCRIPT"

if [ "$UNINSTALL" = true ]; then
    run_host_uninstall
    if [ "$PURGE" = true ]; then
        run_host_purge
    fi
    exit 0
fi

if [ "$PURGE" = true ]; then
    run_host_purge
    exit 0
fi

section "Collecting configuration"
prompt_value TUNNEL_DOMAIN "Base domain for signup and tenant subdomains"
prompt_value FRPS_SERVER "Address tenants use to reach frps" "frps.${TUNNEL_DOMAIN}"
if [ -z "$FRPS_TOKEN" ]; then
    FRPS_TOKEN="$(openssl rand -hex 32)"
fi
prompt_secret FRPS_TOKEN "Shared FRPS auth token for tenant tunnels"
prompt_yes_no ENABLE_HTTPS "Enable HTTPS with wildcard certificates via Caddy DNS challenge" false
if [ "$ENABLE_HTTPS" = true ]; then
    prompt_value DNS_PROVIDER "DNS provider (cloudflare, route53, digitalocean, godaddy)"
    # Validate DNS credentials early so we don't fail silently after frps install
    case "${DNS_PROVIDER:-}" in
        cloudflare)
            [ -n "${CF_API_TOKEN:-}" ] || err "CF_API_TOKEN is required for cloudflare HTTPS. Export it and re-run." ;;
        route53)
            [ -n "${AWS_ACCESS_KEY_ID:-}" ] || err "AWS_ACCESS_KEY_ID is required for route53 HTTPS."
            [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] || err "AWS_SECRET_ACCESS_KEY is required for route53 HTTPS." ;;
        digitalocean)
            [ -n "${DO_AUTH_TOKEN:-}" ] || err "DO_AUTH_TOKEN is required for digitalocean HTTPS." ;;
        godaddy)
            [ -n "${GODADDY_API_KEY:-}" ] || err "GODADDY_API_KEY is required for godaddy HTTPS."
            [ -n "${GODADDY_API_SECRET:-}" ] || err "GODADDY_API_SECRET is required for godaddy HTTPS." ;;
    esac
fi
prompt_yes_no ENABLE_SMTP "Configure SMTP for dashboard OTP emails" false
if [ "$ENABLE_SMTP" = true ]; then
    prompt_value SMTP_HOST "SMTP host" "smtp.gmail.com"
    prompt_value SMTP_PORT "SMTP port" "465"
    prompt_value SMTP_USERNAME "SMTP username"
    prompt_value SMTP_FROM "SMTP from address" "$SMTP_USERNAME"
    prompt_yes_no ALLOW_SELF_REGISTRATION "Allow self-registration via email OTP" false
    [ -n "${SMTP_PASSWORD:-}" ] || err "SMTP_PASSWORD is required for SMTP. Export it and re-run."
fi
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN="$(openssl rand -hex 32)"
fi

validate "domain" "$TUNNEL_DOMAIN" '[a-zA-Z0-9.-]+'
validate "frps-server" "$FRPS_SERVER" '[a-zA-Z0-9.:-]+'
validate "port" "$PORT" '[0-9]+'
validate "auth-token" "$AUTH_TOKEN" '[a-zA-Z0-9._-]+'
validate "frps-token" "$FRPS_TOKEN" '[a-zA-Z0-9._-]+'
validate "frps-bind-port" "$FRPS_BIND_PORT" '[0-9]+'
validate "frps-vhost-http-port" "$FRPS_VHOST_HTTP_PORT" '[0-9]+'
validate "frps-vhost-https-port" "$FRPS_VHOST_HTTPS_PORT" '[0-9]+'
validate "frps-dashboard-port" "$FRPS_DASHBOARD_PORT" '[0-9]+'
validate "frps-ssh-port-start" "$FRPS_SSH_PORT_START" '[0-9]+'
validate "frps-ssh-port-end" "$FRPS_SSH_PORT_END" '[0-9]+'
if [ "$ENABLE_SMTP" = true ]; then
    validate "smtp-host" "$SMTP_HOST" '[a-zA-Z0-9.-]+'
    validate "smtp-port" "$SMTP_PORT" '[0-9]+'
    validate "smtp-from" "$SMTP_FROM" '([^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+|[^"\\<>[:space:]][^"\\<>]*[^"\\<>[:space:]] <[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+>)'
fi
case "$ENABLE_HTTPS" in
    true|false) ;;
    *) err "ENABLE_HTTPS must be true or false" ;;
esac
case "$ENABLE_SMTP" in
    true|false) ;;
    *) err "ENABLE_SMTP must be true or false" ;;
esac
case "$ALLOW_SELF_REGISTRATION" in
    true|false) ;;
    *) err "ALLOW_SELF_REGISTRATION must be true or false" ;;
esac
if [ -n "$DNS_PROVIDER" ]; then
    case "$DNS_PROVIDER" in
        cloudflare|route53|digitalocean|godaddy) ;;
        *) err "unsupported DNS provider: $DNS_PROVIDER" ;;
    esac
fi

section "Configuration summary"
echo "    Domain:              $TUNNEL_DOMAIN"
echo "    frps server:         $FRPS_SERVER"
echo "    shared frps token:   ${FRPS_TOKEN:0:8}..."
echo "    octos serve port:    $PORT"
echo "    frps bind port:      $FRPS_BIND_PORT"
echo "    frps vhost HTTP:     $FRPS_VHOST_HTTP_PORT"
echo "    frps dashboard port: $FRPS_DASHBOARD_PORT"
echo "    HTTPS:               $ENABLE_HTTPS"
if [ "$ENABLE_HTTPS" = true ]; then
    echo "    DNS provider:        $DNS_PROVIDER"
fi
echo "    SMTP:                $ENABLE_SMTP"
if [ "$ENABLE_SMTP" = true ]; then
    echo "    SMTP host:           $SMTP_HOST"
    echo "    SMTP port:           $SMTP_PORT"
    echo "    SMTP username:       $SMTP_USERNAME"
    echo "    SMTP from:           $SMTP_FROM"
    echo "    Self-registration:   $ALLOW_SELF_REGISTRATION"
fi
echo "    Data dir:            $DATA_DIR"
echo "    Prefix:              $PREFIX"
if [ "$DRY_RUN" = false ] && [ "$NONINTERACTIVE" = false ]; then
    echo ""
    echo "    Press Enter to continue, or Ctrl+C to abort."
    read -r < /dev/tty
fi

if [ "$DRY_RUN" = false ]; then
    section "Checking sudo access"
    if ! sudo -v 2>/dev/null; then
        err "sudo access is required to install system services (frps, Caddy, octos serve). Run with sudo privileges or ensure your user is in the sudoers file."
    fi
    ok "sudo credentials cached"
fi

section "Writing local state"
write_cloud_config
write_state_file

run_install
# Refresh sudo credentials between long-running steps (macOS default timeout is 5 min)
[ "$DRY_RUN" = true ] || sudo -v 2>/dev/null || true
run_setup_frps
[ "$DRY_RUN" = true ] || sudo -v 2>/dev/null || true
run_setup_caddy

section "Complete"
echo "    Octos config:  $DATA_DIR/config.json"
echo "    Bootstrap cfg: $STATE_FILE"
echo "    Landing page:  http://$TUNNEL_DOMAIN/"
if [ "$ENABLE_HTTPS" = true ]; then
    echo "    HTTPS target:  https://$TUNNEL_DOMAIN/"
fi
