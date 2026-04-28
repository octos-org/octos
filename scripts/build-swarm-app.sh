#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$ROOT/swarm-app"
OUT_DIR="$ROOT/crates/octos-cli/static/swarm"

if ! command -v npm >/dev/null 2>&1; then
    echo "npm is required to build swarm-app assets" >&2
    exit 1
fi

cd "$APP_DIR"

if [ ! -d node_modules ]; then
    echo "Installing swarm-app dependencies..."
    npm ci
fi

echo "Building swarm-app into $OUT_DIR"
VITE_OUT_DIR="$OUT_DIR" VITE_BASE_PATH="/swarm/" npm run build

echo "swarm-app assets synced to $OUT_DIR"
