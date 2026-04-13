#!/usr/bin/env bash
# purge-tenant.sh — full removal of a registered tenant (profile + user + node + data).
#
# Usage:
#   purge-tenant.sh <node-name> [--force] [--data-dir <path>]
#
# Calls POST /api/admin/profiles/by-node/<node-name>/purge on the local octos serve.
# By default prompts for type-to-confirm. Use --force to skip the prompt.
#
# Config resolution mirrors the CLI:
#   1. <cwd>/.octos/config.json  (project-local)
#   2. <--data-dir>/config.json  (explicit override)
#   3. $OCTOS_HOME/config.json   (env var)
#   4. ~/.octos/config.json      (default)
set -euo pipefail

NODE_NAME="${1:-}"
FORCE=false
DATA_DIR_OVERRIDE=""

if [[ -z "$NODE_NAME" ]]; then
  echo "Usage: $0 <node-name> [--force] [--data-dir <path>]" >&2
  exit 2
fi

shift
while [[ $# -gt 0 ]]; do
  case "$1" in
    --force) FORCE=true; shift;;
    --data-dir) DATA_DIR_OVERRIDE="$2"; shift 2;;
    *) echo "Unknown flag: $1" >&2; exit 2;;
  esac
done

# Resolve data directory: --data-dir flag > OCTOS_HOME env var > ~/.octos
if [[ -n "$DATA_DIR_OVERRIDE" ]]; then
  DATA_DIR="$DATA_DIR_OVERRIDE"
elif [[ -n "${OCTOS_HOME:-}" ]]; then
  DATA_DIR="$OCTOS_HOME"
else
  DATA_DIR="$HOME/.octos"
fi

# Resolve config file following the same priority as the CLI:
#   1. project-local <cwd>/.octos/config.json
#   2. <data-dir>/config.json
CONFIG_FILE=""
CWD="$(pwd)"
if [[ -f "$CWD/.octos/config.json" ]]; then
  CONFIG_FILE="$CWD/.octos/config.json"
elif [[ -f "$DATA_DIR/config.json" ]]; then
  CONFIG_FILE="$DATA_DIR/config.json"
fi

# Resolve API base URL — default 127.0.0.1:8080 (serve host/port are CLI args, not in config)
API_HOST="127.0.0.1"
API_PORT="8080"
API_BASE="http://${API_HOST}:${API_PORT}"

# Auth token — mirrors `octos serve`'s own resolution (commands/serve.rs:109-114):
#   1. $OCTOS_AUTH_TOKEN         — the env var the server itself reads
#   2. config.json .auth_token   — the persisted fallback the server uses
# (Not auth.json — that's a provider OAuth credential store, not an admin token.)
AUTH_TOKEN="${OCTOS_AUTH_TOKEN:-}"
if [[ -z "$AUTH_TOKEN" ]] && [[ -n "$CONFIG_FILE" ]]; then
  if command -v jq >/dev/null 2>&1; then
    AUTH_TOKEN=$(jq -r '.auth_token // empty' "$CONFIG_FILE" 2>/dev/null || true)
  elif command -v python3 >/dev/null 2>&1; then
    AUTH_TOKEN=$(python3 -c "import json; print(json.load(open('$CONFIG_FILE')).get('auth_token',''))" 2>/dev/null || true)
  fi
fi

AUTH_HEADER=()
if [[ -n "$AUTH_TOKEN" ]]; then
  AUTH_HEADER=(-H "Authorization: Bearer $AUTH_TOKEN")
fi

echo "About to PURGE node: $NODE_NAME"
echo "  via: $API_BASE"
echo ""
echo "This will permanently remove the profile, user, node record, and all data."
echo "The same email and node name can be re-registered after this."
echo ""

if [[ "$FORCE" != true ]]; then
  read -r -p "Type the node name to confirm: " CONFIRM
  if [[ "$CONFIRM" != "$NODE_NAME" ]]; then
    echo "Confirmation mismatch — aborting." >&2
    exit 1
  fi
fi

URL="${API_BASE}/api/admin/profiles/by-node/${NODE_NAME}/purge"
RESPONSE=$(curl -sS -X POST ${AUTH_HEADER[@]+"${AUTH_HEADER[@]}"} "$URL" -w "\n%{http_code}")
HTTP_CODE=$(echo "$RESPONSE" | tail -n 1)
BODY=$(echo "$RESPONSE" | sed '$d')

if [[ "$HTTP_CODE" != "200" ]]; then
  echo "Purge failed (HTTP $HTTP_CODE):" >&2
  echo "$BODY" >&2
  exit 1
fi

echo ""
echo "Purge complete:"
if command -v jq >/dev/null 2>&1; then
  echo "$BODY" | jq .
else
  echo "$BODY"
fi
