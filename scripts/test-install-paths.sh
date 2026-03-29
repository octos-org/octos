#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$ROOT_DIR/scripts/install.sh"
DOWNLOAD_BASE="file://$ROOT_DIR"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

run_installer() {
    local workdir="$1"
    local home_dir="$2"
    local prefix="$3"
    local output_file="$4"

    mkdir -p "$home_dir"

    set +e
    (
        cd "$workdir"
        HOME="$home_dir" OCTOS_DOWNLOAD_URL="$DOWNLOAD_BASE" \
            bash "$INSTALLER" --prefix "$prefix" --version test --no-tunnel
    ) >"$output_file" 2>&1
    local status=$?
    set -e

    if [ "$status" -ne 0 ] && ! grep -q "Operation not permitted" "$output_file"; then
        cat "$output_file" >&2
        fail "installer exited unexpectedly for prefix '$prefix'"
    fi
}

main() {
    local test_root
    test_root="$(mktemp -d /tmp/octos-install-paths.XXXXXX)"
    trap 'rm -rf "${test_root:-}"' EXIT

    local rel_workdir="$test_root/relative"
    mkdir -p "$rel_workdir"
    run_installer "$rel_workdir" "$test_root/home-rel" "./relative-bin" "$test_root/relative.out"
    if grep -q "invalid prefix" "$test_root/relative.out"; then
        fail "relative prefix was rejected"
    fi
    [ -x "$rel_workdir/relative-bin/octos" ] || fail "relative prefix did not install into the working directory"

    local tilde_workdir="$test_root/tilde"
    local tilde_home="$test_root/home-tilde"
    mkdir -p "$tilde_workdir"
    run_installer "$tilde_workdir" "$tilde_home" "~/tilde-bin" "$test_root/tilde.out"
    if grep -q "invalid prefix" "$test_root/tilde.out"; then
        fail "tilde prefix was rejected"
    fi
    [ -x "$tilde_home/tilde-bin/octos" ] || fail "tilde prefix did not expand to HOME"
    [ ! -e "$tilde_workdir/~/tilde-bin" ] || fail "tilde prefix was treated as a literal path"

    echo "install path tests passed"
}

main "$@"
