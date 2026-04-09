#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/local-tenant-deploy.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

main() {
    grep -Fq 'mkdir -p "$DATA_DIR"/{profiles,memory,sessions,skills,logs,research,history}' "$SCRIPT" \
        || fail "local tenant deploy should create the runtime data-dir structure"

    grep -Fq 'write_runtime_config() {' "$SCRIPT" \
        || fail "local tenant deploy should define a runtime config writer"

    grep -Fq 'local config_path="$DATA_DIR/config.json"' "$SCRIPT" \
        || fail "local tenant deploy should target DATA_DIR/config.json"

    grep -Fq 'data["mode"] = mode' "$SCRIPT" \
        || fail "local tenant deploy should persist deployment mode into config.json"

    grep -Fq '"mode": "$mode"$extra_config' "$SCRIPT" \
        || fail "local tenant deploy should write deployment mode in the shell fallback config path"

    grep -Fq 'config_path.write_text(json.dumps(data, indent=2) + "\n")' "$SCRIPT" \
        || fail "local tenant deploy should rewrite config.json when python3 is available"

    grep -Fq 'cat > "$config_path" <<EOF' "$SCRIPT" \
        || fail "local tenant deploy should create config.json when it does not exist"

    grep -Fq 'write_runtime_config' "$SCRIPT" \
        || fail "local tenant deploy should invoke the runtime config writer during initialization"

    grep -Fq '"tunnel_domain": "$TUNNEL_DOMAIN"' "$SCRIPT" \
        || fail "local tenant deploy should persist tunnel domain when tenant mode is configured"

    grep -Fq '"frps_server": "$FRPS_SERVER"' "$SCRIPT" \
        || fail "local tenant deploy should persist frps server when tenant mode is configured"

    if grep -Fq 'home/orcl-vps/frps-token.txt' "$SCRIPT"; then
        fail "local tenant deploy still references the old hardcoded shared-token path"
    fi

    grep -Fq 'shared frps auth token from your operator or cloud host' "$SCRIPT" \
        || fail "local tenant deploy should prompt for the shared FRPS token source"

    grep -Fq '#   --purge            Delete the data dir' "$SCRIPT" \
        || fail "local tenant deploy should document the --purge option"

    grep -Fq 'run_purge_data() {' "$SCRIPT" \
        || fail "local tenant deploy should define a standalone purge helper"

    grep -Fq 'sudo rm -rf "$DATA_DIR"' "$SCRIPT" \
        || fail "local tenant deploy should delete the data dir during purge"

    grep -Fq 'bash scripts/local-tenant-deploy.sh --uninstall --purge' "$SCRIPT" \
        || fail "local tenant deploy should direct users to rerun with --uninstall --purge"

    grep -Fq 'Installed binaries and services were preserved.' "$SCRIPT" \
        || fail "local tenant deploy should explain standalone purge preserves binaries and services"

    grep -Fq 'io.octos.frpc.plist' "$SCRIPT" \
        || fail "local tenant deploy uninstall should remove the macOS frpc service"

    grep -Fq 'frpc.service' "$SCRIPT" \
        || fail "local tenant deploy uninstall should remove the Linux frpc service"

    grep -Fq 'sudo rm -f /etc/frp/frpc.toml' "$SCRIPT" \
        || fail "local tenant deploy uninstall should remove the frpc config"

    grep -Fq 'sudo rm -f /var/log/frpc.log' "$SCRIPT" \
        || fail "local tenant deploy uninstall should remove the frpc log"

    echo "local tenant deploy tests passed"
}

main "$@"
