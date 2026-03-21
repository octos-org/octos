#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DASHBOARD_DIR="$ROOT/dashboard"
OUT_DIR="$ROOT/crates/octos-cli/static/admin"

if ! command -v npm >/dev/null 2>&1; then
    echo "npm is required to build dashboard assets" >&2
    exit 1
fi

cd "$DASHBOARD_DIR"

if [ ! -d node_modules ]; then
    echo "Installing dashboard dependencies..."
    npm ci
fi

echo "Building dashboard into $OUT_DIR"
VITE_OUT_DIR="$OUT_DIR" VITE_BASE_PATH="/admin/" npm run build

echo "Dashboard assets synced to $OUT_DIR"
