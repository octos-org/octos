#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLOUD_DEPLOY="$ROOT_DIR/scripts/cloud-host-deploy.sh"

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
ENABLE_SMTP=true
SMTP_HOST=smtp.example.com
SMTP_PORT=465
SMTP_USERNAME=noreply@example.com
SMTP_FROM=noreply@example.com
SMTP_PASSWORD=test-smtp-password
ALLOW_SELF_REGISTRATION=true
AUTH_TOKEN=test-auth-token
FRPS_TOKEN=test-shared-frps-token
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
    grep -q '"dashboard_auth"' "$data_dir/config.json" || fail "config.json missing dashboard_auth"
    grep -q '"host": "smtp.example.com"' "$data_dir/config.json" || fail "config.json missing SMTP host"
    grep -q '"password_env": "SMTP_PASSWORD"' "$data_dir/config.json" || fail "config.json missing SMTP password env"
    grep -q '"allow_self_registration": true' "$data_dir/config.json" || fail "config.json missing allow_self_registration"

    [ -f "$state_file" ] || fail "state file was not written"
    grep -q '^ENABLE_HTTPS=true$' "$state_file" || fail "state file missing ENABLE_HTTPS"
    grep -q '^DNS_PROVIDER=cloudflare$' "$state_file" || fail "state file missing DNS_PROVIDER"
    grep -q '^FRPS_TOKEN=test-shared-frps-token$' "$state_file" || fail "state file missing FRPS_TOKEN"
    if grep -q '^ENABLE_SMTP=' "$state_file"; then
        fail "state file should not store SMTP settings"
    fi
    if grep -q '^SMTP_HOST=' "$state_file"; then
        fail "state file should not store SMTP settings"
    fi

    local rerun_out="$test_root/rerun.out"
    CF_API_TOKEN=test-cloudflare-token SMTP_PASSWORD=test-smtp-password bash "$CLOUD_DEPLOY" \
        --non-interactive \
        --dry-run \
        --data-dir "$data_dir" \
        --prefix "$prefix" \
        --state-file "$state_file" \
        >"$rerun_out" 2>&1
    grep -q "loaded previous settings from $state_file" "$rerun_out" \
        || fail "rerun should load settings from the existing state file"
    grep -q -- '--auth-token test-auth-token' "$rerun_out" \
        || fail "rerun should reuse the saved auth token as the default in non-interactive mode"
    grep -q 'FRPS_TOKEN=\*\*\*' "$rerun_out" \
        || fail "rerun should reuse the saved shared FRPS token for setup-frps.sh"
    grep -q 'scripts/frp/setup-caddy.sh --https --dns-provider cloudflare --domain octos.example.com' "$rerun_out" \
        || fail "rerun should reuse the saved HTTPS settings as defaults in non-interactive mode"
    grep -q 'SMTP_HOST=smtp.example.com' "$rerun_out" \
        || fail "rerun should reuse SMTP settings from config.json in non-interactive mode"

    grep -q 'scripts/install.sh' "$output_file" || fail "dry run did not include install.sh"
    grep -q -- '--auth-token test-auth-token' "$output_file" || fail "dry run did not include auth token"
    grep -q 'scripts/frp/setup-frps.sh' "$output_file" || fail "dry run did not include setup-frps.sh"
    grep -q 'FRPS_TOKEN=\*\*\*' "$output_file" || fail "dry run did not include shared FRPS token env for setup-frps.sh"
    grep -q 'scripts/frp/setup-caddy.sh --https --dns-provider cloudflare --domain octos.example.com' "$output_file" || fail "dry run did not include expected setup-caddy.sh command"
    grep -q 'SMTP_HOST=smtp.example.com' "$output_file" || fail "dry run did not include SMTP env for install.sh"

    # ── Test: missing SMTP_PASSWORD fails early with clear message ───────
    local no_smtp_secret_config="$test_root/no-smtp-secret.env"
    cat >"$no_smtp_secret_config" <<'EOF'
TUNNEL_DOMAIN=octos.example.com
FRPS_SERVER=relay.octos.example.com
ENABLE_SMTP=true
SMTP_HOST=smtp.example.com
SMTP_PORT=465
SMTP_USERNAME=noreply@example.com
SMTP_FROM=noreply@example.com
AUTH_TOKEN=test-auth-token
EOF

    local no_smtp_secret_out="$test_root/no-smtp-secret.out"
    set +e
    bash "$CLOUD_DEPLOY" \
        --config "$no_smtp_secret_config" \
        --non-interactive \
        --data-dir "$data_dir-nosmtp" \
        --prefix "$prefix-nosmtp" \
        --state-file "$state_file-nosmtp" \
        >"$no_smtp_secret_out" 2>&1
    local no_smtp_secret_status=$?
    set -e
    [ "$no_smtp_secret_status" -ne 0 ] || fail "missing SMTP_PASSWORD should fail"
    grep -q 'missing required secret for SMTP_PASSWORD' "$no_smtp_secret_out" \
        || fail "missing SMTP_PASSWORD should produce a clear error message"

    local mock_bin="$test_root/mock-bin"
    mkdir -p "$mock_bin"
    cat >"$mock_bin/uname" <<'EOF'
#!/usr/bin/env bash
echo FreeBSD
EOF
    chmod +x "$mock_bin/uname"

    local error_file="$test_root/cloud-deploy-error.out"
    set +e
    PATH="$mock_bin:$PATH" bash "$CLOUD_DEPLOY" \
        --config "$config_file" \
        --non-interactive \
        --data-dir "$data_dir-err" \
        --prefix "$prefix-err" \
        --state-file "$state_file-err" \
        >"$error_file" 2>&1
    local status=$?
    set -e
    [ "$status" -ne 0 ] || fail "unsupported OS run should fail without --dry-run"
    grep -q 'cloud host bootstrap supports Linux and macOS only (detected: FreeBSD)' "$error_file" \
        || fail "unsupported OS failure did not explain the supported platforms"

    # ── Test: missing CF_API_TOKEN fails early with clear message ────────
    local no_token_config="$test_root/no-token.env"
    cat >"$no_token_config" <<'EOF'
TUNNEL_DOMAIN=octos.example.com
FRPS_SERVER=relay.octos.example.com
ENABLE_HTTPS=true
DNS_PROVIDER=cloudflare
AUTH_TOKEN=test-auth-token
EOF

    local no_token_out="$test_root/no-token.out"
    set +e
    bash "$CLOUD_DEPLOY" \
        --config "$no_token_config" \
        --non-interactive \
        --data-dir "$data_dir-notoken" \
        --prefix "$prefix-notoken" \
        --state-file "$state_file-notoken" \
        >"$no_token_out" 2>&1
    local no_token_status=$?
    set -e
    [ "$no_token_status" -ne 0 ] || fail "missing CF_API_TOKEN should fail"
    grep -q 'CF_API_TOKEN is required' "$no_token_out" \
        || fail "missing CF_API_TOKEN should produce a clear error message"

    # ── Test: export_dns_env does not abort under set -e ──────────────
    # Source the deploy script's export_dns_env in a set -e shell with no tokens set.
    # Before the fix, this would exit 1 silently.
    set +e
    bash -c '
        set -euo pipefail
        eval "$(sed -n "/^export_dns_env()/,/^}/p" "'"$CLOUD_DEPLOY"'")"
        export_dns_env
        echo "export_dns_env_survived"
    ' >"$test_root/dns-env.out" 2>&1
    local dns_env_status=$?
    set -e
    [ "$dns_env_status" -eq 0 ] || fail "export_dns_env should not abort when no tokens are set (exit $dns_env_status)"
    grep -q 'export_dns_env_survived' "$test_root/dns-env.out" \
        || fail "export_dns_env should complete without aborting"

    # ── Test: ERR trap is inherited into helper functions ───────────
    grep -q '^set -eEuo pipefail$' "$CLOUD_DEPLOY" \
        || fail "deploy script should enable errtrace so ERR trap fires inside helper functions"
    grep -q 'trap.*FAILED.*ERR' "$CLOUD_DEPLOY" \
        || fail "deploy script should have an ERR trap for failure reporting"
    grep -q 'CURRENT_STEP' "$CLOUD_DEPLOY" \
        || fail "deploy script should track CURRENT_STEP for ERR trap context"

    echo "cloud deploy tests passed"
}

main "$@"
