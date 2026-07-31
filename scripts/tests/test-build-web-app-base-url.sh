#!/usr/bin/env bash
# Regression test for Git-for-Windows MSYS2 environment conversion in
# scripts/build-web-app.sh. Runs offline with a tiny npm fixture.

set -eEuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET="$REPO_ROOT/scripts/build-web-app.sh"

fixture="$(mktemp -d "${TMPDIR:-/tmp}/octos-web-base-url.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT

mkdir -p \
    "$fixture/scripts" \
    "$fixture/octos-web/node_modules" \
    "$fixture/crates/octos-cli/static"
cp "$TARGET" "$fixture/scripts/build-web-app.sh"

cat >"$fixture/octos-web/package.json" <<'JSON'
{
  "name": "octos-web-base-url-fixture",
  "private": true,
  "scripts": {
    "build": "node build.mjs"
  }
}
JSON

cat >"$fixture/octos-web/build.mjs" <<'JS'
import { mkdirSync, writeFileSync } from "node:fs";

const base = process.env.BASE_URL;
mkdirSync("dist/assets", { recursive: true });
writeFileSync("dist/assets/app.js", "");
writeFileSync("dist/assets/app.css", "");
writeFileSync(
  "dist/index.html",
  `<script type="module" src="${base}assets/app.js"></script>` +
    `<link rel="stylesheet" href="${base}assets/app.css">`,
);
JS

# On Windows, force the same native npm.cmd boundary used by release builds.
# On Unix, wrapping the regular npm executable keeps the fixture portable.
if command -v npm.cmd >/dev/null 2>&1; then
    real_npm="$(command -v npm.cmd)"
else
    real_npm="$(command -v npm)"
fi
mkdir -p "$fixture/bin"
{
    echo '#!/bin/sh'
    printf 'exec %q "$@"\n' "$real_npm"
} >"$fixture/bin/npm"
chmod +x "$fixture/bin/npm"

PATH="$fixture/bin:$PATH" "$BASH" "$fixture/scripts/build-web-app.sh"

embedded_index="$fixture/crates/octos-cli/static/web/index.html"
grep -Fq 'src="/app/assets/app.js"' "$embedded_index"
grep -Fq 'href="/app/assets/app.css"' "$embedded_index"

echo "OK: embedded web assets remain rooted at /app/"
