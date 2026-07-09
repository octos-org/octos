#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$ROOT_DIR/scripts/install.sh"
DOWNLOAD_BASE=""

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

run_installer() {
    local workdir="$1"
    local home_dir="$2"
    local prefix="$3"
    local output_file="$4"
    local mock_bin="$5"

    mkdir -p "$home_dir"

    set +e
    (
        cd "$workdir"
        HOME="$home_dir" OCTOS_DOWNLOAD_URL="$DOWNLOAD_BASE" PATH="$mock_bin:$PATH" \
            bash "$INSTALLER" --prefix "$prefix" --version test
    ) >"$output_file" 2>&1
    local status=$?
    set -e

    if [ "$status" -ne 0 ] && ! grep -q "Operation not permitted" "$output_file"; then
        cat "$output_file" >&2
        fail "installer exited unexpectedly for prefix '$prefix'"
    fi
}

host_triple() {
    local os arch platform
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin) platform="apple-darwin" ;;
        Linux) platform="unknown-linux-gnu" ;;
        *) fail "unsupported test OS: $os" ;;
    esac
    case "$arch" in
        x86_64) echo "x86_64-$platform" ;;
        aarch64|arm64) echo "aarch64-$platform" ;;
        *) fail "unsupported test architecture: $arch" ;;
    esac
}

create_fake_bundle() {
    local bundle_dir="$1"
    local triple
    triple="$(host_triple)"
    mkdir -p "$bundle_dir/payload"
    cat >"$bundle_dir/payload/octos" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "$bundle_dir/payload/octos"
    tar -czf "$bundle_dir/octos-bundle-$triple.tar.gz" -C "$bundle_dir/payload" octos
}

create_mock_sudo() {
    local mock_bin="$1"
    mkdir -p "$mock_bin"
    cat >"$mock_bin/sudo" <<'EOF'
#!/usr/bin/env bash
echo "sudo: Operation not permitted" >&2
exit 1
EOF
    chmod +x "$mock_bin/sudo"
}

main() {
    local test_root
    test_root="$(mktemp -d /tmp/octos-install-paths.XXXXXX)"
    trap 'rm -rf "${test_root:-}"' EXIT
    local bundle_dir="$test_root/download"
    local mock_bin="$test_root/mock-bin"
    mkdir -p "$bundle_dir"
    create_fake_bundle "$bundle_dir"
    create_mock_sudo "$mock_bin"
    DOWNLOAD_BASE="file://$bundle_dir"

    local rel_workdir="$test_root/relative"
    mkdir -p "$rel_workdir"
    run_installer "$rel_workdir" "$test_root/home-rel" "./relative-bin" "$test_root/relative.out" "$mock_bin"
    if grep -q "invalid prefix" "$test_root/relative.out"; then
        fail "relative prefix was rejected"
    fi
    [ -x "$rel_workdir/relative-bin/octos" ] || fail "relative prefix did not install into the working directory"

    local tilde_workdir="$test_root/tilde"
    local tilde_home="$test_root/home-tilde"
    mkdir -p "$tilde_workdir"
    run_installer "$tilde_workdir" "$tilde_home" "~/tilde-bin" "$test_root/tilde.out" "$mock_bin"
    if grep -q "invalid prefix" "$test_root/tilde.out"; then
        fail "tilde prefix was rejected"
    fi
    [ -x "$tilde_home/tilde-bin/octos" ] || fail "tilde prefix did not expand to HOME"
    [ ! -e "$tilde_workdir/~/tilde-bin" ] || fail "tilde prefix was treated as a literal path"

    echo "install path tests passed"
}

main "$@"
