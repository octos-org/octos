#!/usr/bin/env bash
# Test scripts/bundle-release.sh: the bundle must ship with a checksum sidecar
# (`<archive>.sha256`, standard `sha256sum -c` format) so downstream installers
# (octoscode auto-provision) can verify the download instead of silently
# skipping verification (#1929).
#
# Runs against a fake target/release tree — no real build outputs needed.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE="$ROOT_DIR/scripts/bundle-release.sh"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# Same cross-runner reality as the bundle script itself: Linux/Windows runners
# ship sha256sum, macOS ships shasum.
checksum_verify() {
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && sha256sum -c "$2.sha256")
    else
        (cd "$1" && shasum -a 256 -c "$2.sha256")
    fi
}

make_fake_release_tree() {
    # Keep this list in sync with BINARIES in bundle-release.sh: the fake tree
    # must offer every binary the bundle script requires (a missing one fails
    # the bundle step, which the script itself enforces loudly).
    local dir="$1"
    mkdir -p "$dir/target/release"
    for b in octos octos-sandbox news_fetch deep-search deep_crawl send_email \
        account_manager voice clock weather smart_home; do
        echo "fake $b" >"$dir/target/release/$b"
    done
    echo '{}' >"$dir/model_catalog.json"
}

assert_checksum_sidecar() {
    local archive="$1" sidecar="$1.sha256" dir name
    dir="$(dirname "$archive")"
    name="$(basename "$archive")"

    [ -f "$archive" ] || fail "missing bundle archive: $archive"
    [ -f "$sidecar" ] || fail "missing checksum sidecar: $sidecar"

    # Standard `sha256sum -c` line: "<64 lowercase hex>  <filename>". Checking
    # with the tool itself proves both the format and the value at once.
    checksum_verify "$dir" "$name" >/dev/null 2>&1 \
        || fail "sidecar does not verify against $name: $(cat "$sidecar")"
}

main() {
    WORK_DIR="$(mktemp -d /tmp/octos-bundle-release-test.XXXXXX)"
    trap 'rm -rf "$WORK_DIR"' EXIT

    make_fake_release_tree "$WORK_DIR"
    cd "$WORK_DIR"

    bash "$BUNDLE" test-bundle-aarch64-apple-darwin.tar.gz \
        || fail "bundle-release.sh failed for the tar.gz case"
    assert_checksum_sidecar test-bundle-aarch64-apple-darwin.tar.gz

    if command -v 7z >/dev/null 2>&1; then
        bash "$BUNDLE" test-bundle-x86_64-pc-windows-msvc.zip \
            || fail "bundle-release.sh failed for the zip case"
        assert_checksum_sidecar test-bundle-x86_64-pc-windows-msvc.zip
        echo "PASS: bundle-release.sh ships a matching .sha256 sidecar (tar.gz + zip)"
    else
        echo "SKIP: 7z not found — zip case not exercised (tar.gz sidecar passed)"
    fi
}

main "$@"
