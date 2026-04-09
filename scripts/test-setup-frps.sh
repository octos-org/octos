#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/frp/setup-frps.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

main() {
    grep -Fq 'FRPS_TOKEN="${FRPS_TOKEN:-$(openssl rand -hex 32)}"' "$SCRIPT" \
        || fail "setup-frps.sh should define a shared FRPS token default"

    grep -Fq 'auth.method = "token"' "$SCRIPT" \
        || fail "setup-frps.sh should enable native FRPS token auth"

    grep -Fq 'auth.token = "${FRPS_TOKEN}"' "$SCRIPT" \
        || fail "setup-frps.sh should write the shared FRPS token into frps.toml"

    grep -Fq 'ops = ["NewProxy"]' "$SCRIPT" \
        || fail "setup-frps.sh should only use the Octos plugin for NewProxy authorization"

    if grep -Fq 'ops = ["Login", "NewProxy"]' "$SCRIPT"; then
        fail "setup-frps.sh should not route Login through the Octos plugin anymore"
    fi

    echo "setup-frps tests passed"
}

main "$@"
