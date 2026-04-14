#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/frp/setup-frps.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

main() {
    grep -Fq 'auth.method = "token"' "$SCRIPT" \
        || fail "setup-frps.sh should keep auth.method = token (plugin rewrites privilege_key)"

    grep -Fq 'auth.token = ""' "$SCRIPT" \
        || fail "setup-frps.sh must leave auth.token empty so the plugin can rewrite privilege_key to md5(\"\" + ts)"

    grep -Fq 'ops = ["Login", "NewProxy"]' "$SCRIPT" \
        || fail "setup-frps.sh must route Login AND NewProxy through the Octos plugin"

    if grep -Eq 'FRPS_TOKEN=.*openssl rand' "$SCRIPT"; then
        fail "setup-frps.sh should not generate a shared FRPS token (per-tenant tokens only)"
    fi

    echo "setup-frps tests passed"
}

main "$@"
