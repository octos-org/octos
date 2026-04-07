#!/usr/bin/env bash
# cloud-deploy.sh — Bootstrap a Linux VPS as an Octos cloud/host server.
# Reuses install.sh for octos serve, then provisions frps and Caddy.
#
# Usage:
#   ./scripts/cloud-deploy.sh
#   ./scripts/cloud-deploy.sh --domain octos.example.com --https --dns-provider cloudflare
#   ./scripts/cloud-deploy.sh --config ~/.octos/cloud-bootstrap.env --non-interactive

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SCRIPT="$ROOT_DIR/scripts/install.sh"
FRPS_SCRIPT="$ROOT_DIR/scripts/frp/setup-frps.sh"
CADDY_SCRIPT="$ROOT_DIR/scripts/frp/setup-caddy.sh"

VERSION="latest"
PREFIX="${OCTOS_PREFIX:-$HOME/.octos/bin}"
DATA_DIR="${OCTOS_HOME:-$HOME/.octos}"
PORT="8080"
AUTH_TOKEN=""
TUNNEL_DOMAIN="${TUNNEL_DOMAIN:-}"
FRPS_SERVER="${FRPS_SERVER:-}"
ENABLE_HTTPS="${ENABLE_HTTPS:-}"
DNS_PROVIDER="${DNS_PROVIDER:-}"
INSTALL_DEPS=false
NONINTERACTIVE=false
DRY_RUN=false
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

while [ $# -gt 0 ]; do
    case "$1" in
        --config)            needval "$@"; CONFIG_FILE="$2"; load_config_file "$2"; shift 2 ;;
        --version)           needval "$@"; VERSION="$2"; shift 2 ;;
        --prefix)            needval "$@"; PREFIX="$2"; shift 2 ;;
        --data-dir)          needval "$@"; DATA_DIR="$2"; shift 2 ;;
        --state-file)        needval "$@"; STATE_FILE="$2"; shift 2 ;;
        --port)              needval "$@"; PORT="$2"; shift 2 ;;
        --auth-token)        needval "$@"; AUTH_TOKEN="$2"; shift 2 ;;
        --domain)            needval "$@"; TUNNEL_DOMAIN="$2"; shift 2 ;;
        --frps-server)       needval "$@"; FRPS_SERVER="$2"; shift 2 ;;
        --https)             ENABLE_HTTPS=true; shift ;;
        --http-only)         ENABLE_HTTPS=false; shift ;;
        --dns-provider)      needval "$@"; DNS_PROVIDER="$2"; shift 2 ;;
        --install-deps)      INSTALL_DEPS=true; shift ;;
        --non-interactive|--yes) NONINTERACTIVE=true; shift ;;
        --dry-run)           DRY_RUN=true; shift ;;
        --help|-h)
            cat <<'HELPEOF'
cloud-deploy.sh — Bootstrap a Linux VPS as an Octos cloud/host server.

Usage:
  ./scripts/cloud-deploy.sh
  ./scripts/cloud-deploy.sh --domain octos.example.com --https --dns-provider cloudflare
  ./scripts/cloud-deploy.sh --config ~/.octos/cloud-bootstrap.env --non-interactive

Options:
  --config PATH          Source a shell-style config file for silent install
  --version TAG          octos release version passed to install.sh (default: latest)
  --prefix DIR           Binary install prefix (default: ~/.octos/bin)
  --data-dir DIR         Octos data dir and config home (default: ~/.octos)
  --state-file PATH      Persist rerun settings (default: <data-dir>/cloud-bootstrap.env)
  --port PORT            octos serve port behind Caddy (default: 8080)
  --auth-token TOKEN     Admin auth token for the dashboard
  --domain DOMAIN        Base public domain for signup and tenant subdomains
  --frps-server ADDR     Address tenants use to reach frps (default: same as --domain)
  --https                Enable HTTPS with wildcard certs via setup-caddy.sh
  --http-only            Force HTTP-only Caddy setup
  --dns-provider NAME    DNS provider for HTTPS: cloudflare, route53, digitalocean, godaddy
  --install-deps         Forward to install.sh to install missing runtime deps
  --non-interactive      Fail instead of prompting for missing values
  --dry-run              Write config files but print commands instead of executing them

Config file format:
  Shell-style KEY=value entries, for example:
    TUNNEL_DOMAIN=octos.example.com
    FRPS_SERVER=relay.octos.example.com
    ENABLE_HTTPS=true
    DNS_PROVIDER=cloudflare
    CF_API_TOKEN=...
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
STATE_FILE="${STATE_FILE:-$DATA_DIR/cloud-bootstrap.env}"
STATE_FILE="$(normalize_path "$STATE_FILE")"

section() { echo ""; echo "==> $1"; }
ok()      { echo "    OK: $1"; }
warn()    { echo "    WARN: $1"; }
err()     { echo "    ERROR: $1" >&2; exit 1; }

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
        return 0
    fi
    if [ "$NONINTERACTIVE" = true ]; then
        if [ -n "$default_value" ]; then
            printf -v "$var_name" '%s' "$default_value"
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
}

prompt_yes_no() {
    local var_name="$1"
    local prompt="$2"
    local default_value="$3"
    local current="${!var_name:-}"
    if [ "$current" = true ] || [ "$current" = false ]; then
        return 0
    fi
    if [ "$NONINTERACTIVE" = true ]; then
        printf -v "$var_name" '%s' "$default_value"
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
}

export_dns_env() {
    [ -n "${CF_API_TOKEN:-}" ] && export CF_API_TOKEN
    [ -n "${AWS_ACCESS_KEY_ID:-}" ] && export AWS_ACCESS_KEY_ID
    [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] && export AWS_SECRET_ACCESS_KEY
    [ -n "${DO_AUTH_TOKEN:-}" ] && export DO_AUTH_TOKEN
    [ -n "${GODADDY_API_KEY:-}" ] && export GODADDY_API_KEY
    [ -n "${GODADDY_API_SECRET:-}" ] && export GODADDY_API_SECRET
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
                "$DETECTED_PROVIDER" "$DETECTED_MODEL" "$DETECTED_ENV" <<'PYEOF'
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

config_path.write_text(json.dumps(data, indent=2) + "\n")
PYEOF
        else
            err "python3 is required to update existing $config_path safely"
        fi
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
    chmod 600 "$config_path"
    ok "cloud config written to $config_path"
}

write_state_file() {
    mkdir -p "$(dirname "$STATE_FILE")"
    cat >"$STATE_FILE" <<EOF
# Generated by cloud-deploy.sh
VERSION=$VERSION
PREFIX=$PREFIX
DATA_DIR=$DATA_DIR
PORT=$PORT
AUTH_TOKEN=$AUTH_TOKEN
TUNNEL_DOMAIN=$TUNNEL_DOMAIN
FRPS_SERVER=$FRPS_SERVER
ENABLE_HTTPS=$ENABLE_HTTPS
DNS_PROVIDER=$DNS_PROVIDER
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

run_cmd() {
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

run_install() {
    section "Installing octos serve"
    local cmd=("$INSTALL_SCRIPT" --version "$VERSION" --prefix "$PREFIX" --port "$PORT" --auth-token "$AUTH_TOKEN")
    [ "$INSTALL_DEPS" = true ] && cmd+=(--install-deps)
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: OCTOS_HOME=%q' "$DATA_DIR"
        printf ' %q' "${cmd[@]}"
        printf '\n'
    else
        OCTOS_HOME="$DATA_DIR" "${cmd[@]}"
    fi
}

run_setup_frps() {
    section "Installing frps"
    if [ "$DRY_RUN" = true ]; then
        printf '    DRY RUN: TUNNEL_DOMAIN=%q OCTOS_SERVE_PORT=%q FRPS_BIND_PORT=%q FRPS_VHOST_HTTP_PORT=%q FRPS_VHOST_HTTPS_PORT=%q FRPS_DASHBOARD_PORT=%q FRPS_SSH_PORT_START=%q FRPS_SSH_PORT_END=%q %q\n' \
            "$TUNNEL_DOMAIN" "$PORT" "$FRPS_BIND_PORT" "$FRPS_VHOST_HTTP_PORT" "$FRPS_VHOST_HTTPS_PORT" \
            "$FRPS_DASHBOARD_PORT" "$FRPS_SSH_PORT_START" "$FRPS_SSH_PORT_END" "$FRPS_SCRIPT"
    else
        TUNNEL_DOMAIN="$TUNNEL_DOMAIN" \
        OCTOS_SERVE_PORT="$PORT" \
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

if [ "$(uname -s)" != "Linux" ]; then
    if [ "$DRY_RUN" = true ]; then
        warn "dry-run mode on non-Linux host; real cloud bootstrap requires Linux/systemd"
    else
        err "cloud host bootstrap currently supports Linux/systemd hosts only. Run this on a Linux VPS. Use --dry-run locally to preview the commands."
    fi
fi

[ -f "$INSTALL_SCRIPT" ] || err "missing install script: $INSTALL_SCRIPT"
[ -f "$FRPS_SCRIPT" ] || err "missing frps setup script: $FRPS_SCRIPT"
[ -f "$CADDY_SCRIPT" ] || err "missing Caddy setup script: $CADDY_SCRIPT"

section "Collecting configuration"
prompt_value TUNNEL_DOMAIN "Base domain for signup and tenant subdomains"
prompt_value FRPS_SERVER "Address tenants use to reach frps" "$TUNNEL_DOMAIN"
prompt_yes_no ENABLE_HTTPS "Enable HTTPS with wildcard certificates via Caddy DNS challenge" false
if [ "$ENABLE_HTTPS" = true ]; then
    prompt_value DNS_PROVIDER "DNS provider (cloudflare, route53, digitalocean, godaddy)"
fi
if [ -z "$AUTH_TOKEN" ]; then
    AUTH_TOKEN="$(openssl rand -hex 32)"
fi

validate "domain" "$TUNNEL_DOMAIN" '[a-zA-Z0-9.-]+'
validate "frps-server" "$FRPS_SERVER" '[a-zA-Z0-9.:-]+'
validate "port" "$PORT" '[0-9]+'
validate "auth-token" "$AUTH_TOKEN" '[a-zA-Z0-9._-]+'
validate "frps-bind-port" "$FRPS_BIND_PORT" '[0-9]+'
validate "frps-vhost-http-port" "$FRPS_VHOST_HTTP_PORT" '[0-9]+'
validate "frps-vhost-https-port" "$FRPS_VHOST_HTTPS_PORT" '[0-9]+'
validate "frps-dashboard-port" "$FRPS_DASHBOARD_PORT" '[0-9]+'
validate "frps-ssh-port-start" "$FRPS_SSH_PORT_START" '[0-9]+'
validate "frps-ssh-port-end" "$FRPS_SSH_PORT_END" '[0-9]+'
case "$ENABLE_HTTPS" in
    true|false) ;;
    *) err "ENABLE_HTTPS must be true or false" ;;
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
echo "    octos serve port:    $PORT"
echo "    frps bind port:      $FRPS_BIND_PORT"
echo "    frps vhost HTTP:     $FRPS_VHOST_HTTP_PORT"
echo "    frps dashboard port: $FRPS_DASHBOARD_PORT"
echo "    HTTPS:               $ENABLE_HTTPS"
if [ "$ENABLE_HTTPS" = true ]; then
    echo "    DNS provider:        $DNS_PROVIDER"
fi
echo "    Data dir:            $DATA_DIR"
echo "    Prefix:              $PREFIX"
if [ "$DRY_RUN" = false ] && [ "$NONINTERACTIVE" = false ]; then
    echo ""
    echo "    Press Enter to continue, or Ctrl+C to abort."
    read -r < /dev/tty
fi

section "Writing local state"
write_cloud_config
write_state_file

run_install
run_setup_frps
run_setup_caddy

section "Complete"
echo "    Octos config:  $DATA_DIR/config.json"
echo "    Bootstrap cfg: $STATE_FILE"
echo "    Landing page:  http://$TUNNEL_DOMAIN/"
if [ "$ENABLE_HTTPS" = true ]; then
    echo "    HTTPS target:  https://$TUNNEL_DOMAIN/"
fi
