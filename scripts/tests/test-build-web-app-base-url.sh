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
  "packageManager": "pnpm@10.0.0",
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

# The build script installs via pnpm (octos-web is a pnpm workspace, no
# package-lock.json). Provide a stub pnpm that proxies to the same real
# npm boundary as before — `pnpm install` is skipped because the fixture
# ships a pre-populated node_modules, so only the `pnpm run build`
# invocation reaches npm here, which runs it as `npm exec build`.
if command -v npm.cmd >/dev/null 2>&1; then
    real_npm="$(command -v npm.cmd)"
else
    real_npm="$(command -v npm)"
fi
mkdir -p "$fixture/bin"
for tool in npm pnpm; do
    {
        echo '#!/bin/sh'
        printf 'exec %q "$@"\n' "$real_npm"
    } >"$fixture/bin/$tool"
    chmod +x "$fixture/bin/$tool"
done

PATH="$fixture/bin:$PATH" "$BASH" "$fixture/scripts/build-web-app.sh"

embedded_index="$fixture/crates/octos-cli/static/web/index.html"
grep -Fq 'src="/app/assets/app.js"' "$embedded_index"
grep -Fq 'href="/app/assets/app.css"' "$embedded_index"

echo "OK: embedded web assets remain rooted at /app/"
