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
    # Sidecar exactly as bundle-release.sh writes it (#2514).
    (
        cd "$bundle_dir"
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "octos-bundle-$triple.tar.gz" > "octos-bundle-$triple.tar.gz.sha256"
        else
            shasum -a 256 "octos-bundle-$triple.tar.gz" > "octos-bundle-$triple.tar.gz.sha256"
        fi
    )
}

# Run the installer and assert it refused the bundle with a checksum
# diagnostic, leaving nothing installed (#2514).
run_corrupt_installer() {
    local workdir="$1" home_dir="$2" prefix="$3" output_file="$4" mock_bin="$5" bundle_dir="$6"

    mkdir -p "$home_dir"
    set +e
    (
        cd "$workdir"
        HOME="$home_dir" OCTOS_DOWNLOAD_URL="file://$bundle_dir" PATH="$mock_bin:$PATH" \
            bash "$INSTALLER" --prefix "$prefix" --version test
    ) >"$output_file" 2>&1
    local status=$?
    set -e

    if [ "$status" -eq 0 ]; then
        cat "$output_file" >&2
        fail "installer accepted a bundle that fails its checksum"
    fi
    grep -q "checksum MISMATCH" "$output_file" \
        || fail "corrupt bundle refused without a checksum diagnostic"
    [ ! -x "$prefix/octos" ] || fail "corrupt bundle was installed anyway"
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

# Mock curl so the installer's default download arm (no OCTOS_DOWNLOAD_URL)
# runs without the internet: GET bodies are served from $MOCK_HTTP_ROOT by
# URL basename. Only the installer's form is supported: curl -fsSL -o OUT URL.
# A missing source is a 404: exit non-zero, write no output file.
create_mock_curl() {
    local mock_bin="$1"
    cat >"$mock_bin/curl" <<'EOF'
#!/usr/bin/env bash
out=""
prev=""
for arg in "$@"; do
    [ "$prev" = "-o" ] && out="$arg"
    prev="$arg"
done
src="$MOCK_HTTP_ROOT/$(basename "${@: -1}")"
if [ ! -f "$src" ]; then
    exit 22
fi
[ -n "$out" ] || exit 2
cp "$src" "$out"
EOF
    chmod +x "$mock_bin/curl"
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
    create_mock_curl "$mock_bin"
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

    # ── Bundle checksum verification (#2514) ─────────────────────────
    # The happy paths above now install from a bundle WITH its sidecar;
    # assert the verification actually ran on one of them.
    grep -q "checksum verified" "$test_root/relative.out" \
        || fail "sidecar shipped next to the bundle was not verified"
    for leak in "$test_root/relative-bin"/*.tar.gz "$test_root/relative-bin"/*.sha256; do
        [ -e "$leak" ] && fail "download artifact leaked into the install prefix: $leak"
    done

    # A bundle whose sidecar does not match must be refused, never installed.
    local corrupt_dir="$test_root/corrupt"
    local corrupt_prefix="$test_root/corrupt-bin"
    mkdir -p "$corrupt_dir"
    create_fake_bundle "$corrupt_dir"
    printf '%064d  octos-bundle-%s.tar.gz\n' 0 "$(host_triple)" \
        > "$corrupt_dir/octos-bundle-$(host_triple).tar.gz.sha256"
    run_corrupt_installer "$test_root" "$test_root/home-corrupt" "$corrupt_prefix" \
        "$test_root/corrupt.out" "$mock_bin" "$corrupt_dir"

    # An UPPERCASE-hash sidecar verifies too: GNU sha256sum -c and macOS
    # shasum -c both accept uppercase hex (verified on both tools), matching
    # install.ps1's case-insensitive comparison.
    local upper_dir="$test_root/upper-sidecar"
    DOWNLOAD_BASE="file://$upper_dir"
    mkdir -p "$upper_dir"
    create_fake_bundle "$upper_dir"
    tr 'a-f' 'A-F' < "$upper_dir/octos-bundle-$(host_triple).tar.gz.sha256" \
        > "$upper_dir/octos-bundle-$(host_triple).tar.gz.sha256.up"
    mv "$upper_dir/octos-bundle-$(host_triple).tar.gz.sha256.up" \
        "$upper_dir/octos-bundle-$(host_triple).tar.gz.sha256"
    run_installer "$test_root" "$test_root/home-upper" "$test_root/upper-bin" \
        "$test_root/upper.out" "$mock_bin"
    [ -x "$test_root/upper-bin/octos" ] \
        || fail "uppercase sidecar aborted the install"
    grep -q "checksum verified" "$test_root/upper.out" \
        || fail "uppercase sidecar was not verified"

    # A missing sidecar (pre-rc.12 releases, air-gapped mirrors) only warns.
    local bare_dir="$test_root/bare"
    DOWNLOAD_BASE="file://$bare_dir"
    mkdir -p "$bare_dir"
    create_fake_bundle "$bare_dir"
    rm -f "$bare_dir"/*.sha256
    run_installer "$test_root" "$test_root/home-bare" "$test_root/bare-bin" \
        "$test_root/bare.out" "$mock_bin"
    [ -x "$test_root/bare-bin/octos" ] \
        || fail "missing sidecar aborted the install (pre-rc.12 releases must keep installing)"
    grep -q "skipping checksum verification" "$test_root/bare.out" \
        || fail "missing sidecar was not surfaced to the operator"

    # A sidecar that exists but does not parse (a 200-with-HTML mirror
    # answer) is treated like a missing one, not a mismatch.
    local html_dir="$test_root/html-sidecar"
    DOWNLOAD_BASE="file://$html_dir"
    mkdir -p "$html_dir"
    create_fake_bundle "$html_dir"
    printf '<html><body>404: not found</body></html>\n' \
        > "$html_dir/octos-bundle-$(host_triple).tar.gz.sha256"
    run_installer "$test_root" "$test_root/home-html" "$test_root/html-bin" \
        "$test_root/html.out" "$mock_bin"
    [ -x "$test_root/html-bin/octos" ] \
        || fail "unparseable sidecar aborted the install"
    grep -q "malformed.*skipping checksum verification" "$test_root/html.out" \
        || fail "unparseable sidecar was not surfaced to the operator"

    # A 0-byte sidecar (200-with-empty-body mirror) behaves the same.
    local empty_dir="$test_root/empty-sidecar"
    DOWNLOAD_BASE="file://$empty_dir"
    mkdir -p "$empty_dir"
    create_fake_bundle "$empty_dir"
    : > "$empty_dir/octos-bundle-$(host_triple).tar.gz.sha256"
    run_installer "$test_root" "$test_root/home-empty" "$test_root/empty-bin" \
        "$test_root/empty.out" "$mock_bin"
    [ -x "$test_root/empty-bin/octos" ] \
        || fail "empty sidecar aborted the install"
    grep -q "malformed.*skipping checksum verification" "$test_root/empty.out" \
        || fail "empty sidecar was not surfaced to the operator"

    # A CRLF sidecar (text-mode mirror) must still verify, not mismatch —
    # macOS shasum -c would otherwise look for a file named "...tar.gz\r".
    local crlf_dir="$test_root/crlf-sidecar"
    DOWNLOAD_BASE="file://$crlf_dir"
    mkdir -p "$crlf_dir"
    create_fake_bundle "$crlf_dir"
    printf '%s\r\n' "$(cat "$crlf_dir/octos-bundle-$(host_triple).tar.gz.sha256")" \
        > "$crlf_dir/octos-bundle-$(host_triple).tar.gz.sha256"
    run_installer "$test_root" "$test_root/home-crlf" "$test_root/crlf-bin" \
        "$test_root/crlf.out" "$mock_bin"
    [ -x "$test_root/crlf-bin/octos" ] \
        || fail "CRLF sidecar aborted the install"
    grep -q "checksum verified" "$test_root/crlf.out" \
        || fail "CRLF sidecar was not verified"

    # ── The default download arm: no OCTOS_DOWNLOAD_URL, both the bundle
    # and its sidecar are fetched over HTTP (mocked) and verified (#2514).
    local net_dir="$test_root/http-root"
    DOWNLOAD_BASE=""
    mkdir -p "$net_dir"
    create_fake_bundle "$net_dir"
    export MOCK_HTTP_ROOT="$net_dir"
    run_installer "$test_root" "$test_root/home-net" \
        "$test_root/net-bin" "$test_root/net.out" "$mock_bin"
    [ -x "$test_root/net-bin/octos" ] \
        || fail "network download arm did not install"
    grep -q "checksum verified" "$test_root/net.out" \
        || fail "network download arm did not verify the sidecar"

    # A mirror missing the sidecar (404 on <bundle>.sha256) warns and
    # still installs.
    local net404_dir="$test_root/http-root-404"
    DOWNLOAD_BASE=""
    mkdir -p "$net404_dir"
    create_fake_bundle "$net404_dir"
    rm -f "$net404_dir"/*.sha256
    export MOCK_HTTP_ROOT="$net404_dir"
    run_installer "$test_root" "$test_root/home-net404" \
        "$test_root/net404-bin" "$test_root/net404.out" "$mock_bin"
    [ -x "$test_root/net404-bin/octos" ] \
        || fail "network 404 sidecar aborted the install"
    grep -q "skipping checksum verification" "$test_root/net404.out" \
        || fail "network 404 sidecar was not surfaced to the operator"

    echo "install path tests passed"
}

main "$@"
