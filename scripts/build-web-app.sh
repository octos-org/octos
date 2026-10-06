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
# Package manager: octos-web migrated to pnpm (package.json
# `packageManager: pnpm@…`, pnpm-lock.yaml, NO package-lock.json — 2026-08,
# #2108), so this script installs with `pnpm install --frozen-lockfile`.
# Requires corepack or a standalone pnpm on PATH; Node 22+ images have both.
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

if ! command -v node >/dev/null 2>&1; then
    echo "error: node not found — install Node.js (https://nodejs.org) to build octos-web" >&2
    exit 1
fi
# corepack reads `packageManager` from package.json, so it must run INSIDE
# $WEB_DIR — from the repo root (as on the windows base-url CI job) it exits
# with "Couldn't find a project in the local directory".
cd "$WEB_DIR"
if ! command -v pnpm >/dev/null 2>&1; then
    # Corepack ships with Node >=16.10 and reads `packageManager` from
    # package.json, so this also pins the exact pnpm version the lockfile needs.
    if command -v corepack >/dev/null 2>&1; then
        corepack enable pnpm
        corepack prepare
    else
        echo "error: pnpm not found — octos-web is a pnpm workspace (no package-lock.json). Install pnpm or run 'corepack enable'." >&2
        exit 1
    fi
fi

if [ ! -d node_modules ]; then
    echo "Installing octos-web dependencies (pnpm install --frozen-lockfile)…"
    pnpm install --frozen-lockfile
fi

echo "Building octos-web (BASE_URL=$BASE_PATH) → $OUT_DIR"
# Git for Windows runs native npm/node through MSYS2. Without an exclusion,
# MSYS2 treats the POSIX-looking BASE_URL as a filesystem path and rewrites
# `/app/` to e.g. `C:/Program Files/Git/app/`, which Vite then bakes into every
# asset URL and the React Router basename.
msys2_env_conv_excl="${MSYS2_ENV_CONV_EXCL:-}"
if [ -n "$msys2_env_conv_excl" ]; then
    msys2_env_conv_excl="${msys2_env_conv_excl};BASE_URL"
else
    msys2_env_conv_excl="BASE_URL"
fi
MSYS2_ENV_CONV_EXCL="$msys2_env_conv_excl" BASE_URL="$BASE_PATH" npm run build

# Fail the release build before rust-embed can package a shell whose primary
# Vite assets point outside `/app/`. This is intentionally an output check,
# not just an environment check, so CI guards the artifact users receive.
INDEX_FILE="$WEB_DIR/dist/index.html"
if [ ! -f "$INDEX_FILE" ]; then
    echo "error: octos-web build did not produce $INDEX_FILE" >&2
    exit 1
fi
if ! grep -Fq "src=\"${BASE_PATH}assets/" "$INDEX_FILE" \
    || ! grep -Fq "href=\"${BASE_PATH}assets/" "$INDEX_FILE"; then
    echo "error: octos-web assets are not rooted at $BASE_PATH" >&2
    echo "  generated index references:" >&2
    grep -Eo '(src|href)="[^"]+"' "$INDEX_FILE" >&2 || true
    exit 1
fi

# Replace the embed dir atomically-ish: clear then copy the fresh dist.
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
cp -R "$WEB_DIR/dist/." "$OUT_DIR/"

echo "octos-web assets synced to $OUT_DIR (serve at /app)"
