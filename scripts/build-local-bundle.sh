#!/usr/bin/env bash
# Build + bundle a release tarball for the host platform, named so that
# scripts/install.sh auto-detects it and installs from the local build
# instead of downloading from GitHub Releases.
#
# Usage:
#   ./scripts/build-local-bundle.sh                    # build + bundle only
#   ./scripts/build-local-bundle.sh --install          # + run installer
#   ./scripts/build-local-bundle.sh --skip-dashboard   # skip npm/vite build
#   ./scripts/build-local-bundle.sh --install-deps     # auto-install Node.js
#                                                       # for the dashboard build
#                                                       # (forwarded to install.sh too)
#   ./scripts/build-local-bundle.sh --install --tunnel --tenant-name alice ...
#
# Environment (defaults mirror .github/workflows/ci.yml):
#   FEATURES      cargo features for octos-cli
#   SKILL_CRATES  -p args for skill crate builds
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SKIP_DASHBOARD=false
INSTALL_DEPS=false
PASSTHRU=()
for arg in "$@"; do
    case "$arg" in
        --skip-dashboard) SKIP_DASHBOARD=true ;;
        --install-deps)   INSTALL_DEPS=true; PASSTHRU+=("$arg") ;;
        *)                PASSTHRU+=("$arg") ;;
    esac
done
if [ ${#PASSTHRU[@]} -gt 0 ]; then
    set -- "${PASSTHRU[@]}"
else
    set --
fi

# ── Detect host TRIPLE (mirrors install.sh:1215-1225) ────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin) PLATFORM="apple-darwin" ;;
    Linux)  PLATFORM="unknown-linux-gnu" ;;
    *)      echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac
case "$ARCH" in
    x86_64)        TRIPLE="x86_64-${PLATFORM}" ;;
    aarch64|arm64) TRIPLE="aarch64-${PLATFORM}" ;;
    *)             echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac
echo "==> Target: $TRIPLE"

# ── Ensure cargo is on PATH ───────────────────────────────────────────
# Source-build hosts need the Rust toolchain. install.sh's --install-deps
# only covers runtime deps for release-binary installs; for the
# build-from-source path we install rustup here when the operator opted
# in via --install-deps. Otherwise, fail fast with a clear hint instead
# of letting milestone-ci.sh die on `cargo: command not found`.
ensure_cargo() {
    # rustup installs to ~/.cargo/bin which may not be on PATH yet for
    # this shell (e.g. fresh install). Try to source the env script too.
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi
    if [ "$INSTALL_DEPS" != true ]; then
        echo "ERROR: cargo (Rust toolchain) is required to build from source." >&2
        echo "       Install rustup: https://rustup.rs" >&2
        echo "       Or rerun with --install-deps to auto-install." >&2
        return 1
    fi
    echo "==> Installing rustup (Rust toolchain)"
    # Official rustup installer; -y skips confirmation, --default-toolchain
    # stable picks the right channel without forcing a specific version.
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    command -v cargo >/dev/null 2>&1
}
ensure_cargo || exit 1

# ── Dashboard (embedded SPA — must be built before cargo, since ─────
#    rust_embed bakes crates/octos-cli/static/admin/ into the binary) ─
if [ "$SKIP_DASHBOARD" = true ]; then
    echo "==> Skipping dashboard build (--skip-dashboard)"
else
    DASHBOARD_ARGS=()
    [ "$INSTALL_DEPS" = true ] && DASHBOARD_ARGS+=(--install-deps)
    ./scripts/build-dashboard.sh "${DASHBOARD_ARGS[@]}"
fi

# ── Build (delegates to milestone-ci.sh release-bundle) ──────────────
./scripts/milestone-ci.sh release-bundle

# ── Bundle (same binary list as .github/workflows/ci.yml:179-182) ────
TARBALL="octos-bundle-${TRIPLE}.tar.gz"
rm -rf dist && mkdir dist
for b in octos news_fetch deep-search deep_crawl send_email account_manager \
         voice clock weather; do
    cp "target/release/$b" dist/ 2>/dev/null || true
done
(cd dist && tar czf "../scripts/${TARBALL}" ./*)
echo "==> Wrote scripts/${TARBALL}"

# ── Optional: chain into installer ───────────────────────────────────
if [ "${1:-}" = "--install" ]; then
    shift
    exec ./scripts/install.sh "$@"
fi

echo ""
echo "Next: ./scripts/install.sh"
