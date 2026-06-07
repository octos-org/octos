#!/usr/bin/env bash
# Build the octos-web SPA into the octos-cli embed dir so `octos serve` can
# serve it same-origin at `/app` (no separate Caddy needed).
#
# octos-web is a git submodule at `octos-web/`. Its Vite config hardcodes
# `outDir: "dist"` and honours `BASE_URL` for the asset base + the React Router
# basename (`API_BASE` is "" so the app is same-origin by design). We build with
# `BASE_URL=/app/` then copy `dist/` into `crates/octos-cli/static/web/`, which
# `static_files.rs` embeds via rust-embed (`#[folder = "static/"]`).
#
# Mirrors scripts/build-dashboard.sh (admin SPA) and the swarm-app pattern.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WEB_DIR="$ROOT/octos-web"
OUT_DIR="$ROOT/crates/octos-cli/static/web"
BASE_PATH="/app/"

if [ ! -d "$WEB_DIR" ] || [ ! -f "$WEB_DIR/package.json" ]; then
    echo "error: octos-web submodule is not checked out at $WEB_DIR" >&2
    echo "  run: git submodule update --init octos-web" >&2
    exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
    echo "error: npm not found — install Node.js (https://nodejs.org) to build octos-web" >&2
    exit 1
fi

cd "$WEB_DIR"
if [ ! -d node_modules ]; then
    echo "Installing octos-web dependencies (npm ci)…"
    npm ci
fi

echo "Building octos-web (BASE_URL=$BASE_PATH) → $OUT_DIR"
BASE_URL="$BASE_PATH" npm run build

# Replace the embed dir atomically-ish: clear then copy the fresh dist.
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
cp -R "$WEB_DIR/dist/." "$OUT_DIR/"

echo "octos-web assets synced to $OUT_DIR (serve at /app)"
