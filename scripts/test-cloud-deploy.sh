#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLOUD_DEPLOY="$ROOT_DIR/scripts/cloud-deploy.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

main() {
    local test_root
    test_root="$(mktemp -d /tmp/octos-cloud-deploy.XXXXXX)"
    trap 'rm -rf "${test_root:-}"' EXIT

    local config_file="$test_root/cloud.env"
    local data_dir="$test_root/home/.octos"
    local prefix="$test_root/home/.octos/bin"
    local state_file="$test_root/cloud-bootstrap.env"
    local output_file="$test_root/cloud-deploy.out"

    cat >"$config_file" <<'EOF'
TUNNEL_DOMAIN=octos.example.com
FRPS_SERVER=relay.octos.example.com
ENABLE_HTTPS=true
DNS_PROVIDER=cloudflare
CF_API_TOKEN=test-cloudflare-token
AUTH_TOKEN=test-auth-token
EOF

    bash "$CLOUD_DEPLOY" \
        --config "$config_file" \
        --non-interactive \
        --dry-run \
        --data-dir "$data_dir" \
        --prefix "$prefix" \
        --state-file "$state_file" \
        >"$output_file" 2>&1

    [ -f "$data_dir/config.json" ] || fail "cloud config.json was not created"
    grep -q '"mode": "cloud"' "$data_dir/config.json" || fail "config.json missing cloud mode"
    grep -q '"tunnel_domain": "octos.example.com"' "$data_dir/config.json" || fail "config.json missing tunnel_domain"
    grep -q '"frps_server": "relay.octos.example.com"' "$data_dir/config.json" || fail "config.json missing frps_server"

    [ -f "$state_file" ] || fail "state file was not written"
    grep -q '^ENABLE_HTTPS=true$' "$state_file" || fail "state file missing ENABLE_HTTPS"
    grep -q '^DNS_PROVIDER=cloudflare$' "$state_file" || fail "state file missing DNS_PROVIDER"

    grep -q 'scripts/install.sh' "$output_file" || fail "dry run did not include install.sh"
    grep -q -- '--auth-token test-auth-token' "$output_file" || fail "dry run did not include auth token"
    grep -q 'scripts/frp/setup-frps.sh' "$output_file" || fail "dry run did not include setup-frps.sh"
    grep -q 'scripts/frp/setup-caddy.sh --https --dns-provider cloudflare --domain octos.example.com' "$output_file" || fail "dry run did not include expected setup-caddy.sh command"

    if [ "$(uname -s)" != "Linux" ]; then
        local error_file="$test_root/cloud-deploy-error.out"
        set +e
        bash "$CLOUD_DEPLOY" \
            --config "$config_file" \
            --non-interactive \
            --data-dir "$data_dir-err" \
            --prefix "$prefix-err" \
            --state-file "$state_file-err" \
            >"$error_file" 2>&1
        local status=$?
        set -e
        [ "$status" -ne 0 ] || fail "non-linux host run should fail without --dry-run"
        grep -q 'Run this on a Linux VPS. Use --dry-run locally to preview the commands.' "$error_file" \
            || fail "non-linux failure did not explain how to proceed"
    fi

    echo "cloud deploy tests passed"
}

main "$@"
