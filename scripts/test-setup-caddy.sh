#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/frp/setup-caddy.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

main() {
    grep -Fq 'sed_in_place() {' "$SCRIPT" \
        || fail "setup-caddy should define a portable in-place edit helper"

    grep -Fq 'sed_in_place "$TEST_CADDYFILE"' "$SCRIPT" \
        || fail "setup-caddy should use the portable edit helper for the token compatibility probe"

    grep -Fq 'sed_in_place /etc/caddy/Caddyfile' "$SCRIPT" \
        || fail "setup-caddy should use the portable edit helper for the final Caddyfile substitution"

    grep -Fq 'launchd_env_dict() {' "$SCRIPT" \
        || fail "setup-caddy should define a launchd environment helper for macOS"

    grep -Fq '${LAUNCHD_ENV_DICT}' "$SCRIPT" \
        || fail "setup-caddy should inject DNS provider environment variables into the macOS launchd plist"

    echo "setup-caddy tests passed"
}

main "$@"
