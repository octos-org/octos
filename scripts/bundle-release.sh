#!/usr/bin/env bash
# Bundle the release binaries + canonical model catalog into one archive.
#
# Single source of truth for WHAT ships in an octos release. Previously this
# list was copy-pasted across ci.yml, release.yml and release-dispatch.yml and
# had already drifted (skill-evolve was compiled but never bundled;
# model_catalog.json was only bundled by release-dispatch).
#
# Usage: scripts/bundle-release.sh <output.tar.gz|output.zip>
#
# Every binary is REQUIRED — a missing build output fails the step. The old
# inline loops used `cp ... 2>/dev/null || true`, which shipped incomplete
# bundles silently.
set -euo pipefail

BINARIES=(
  octos
  octos-sandbox
  news_fetch
  deep-search
  deep_crawl
  send_email
  account_manager
  voice
  clock
  weather
  smart_home
)

out="${1:?usage: bundle-release.sh <output.tar.gz|output.zip>}"
# Normalise to an absolute path — the archive is written from inside dist/.
case "$out" in
  /*) ;;
  *) out="$(pwd)/$out" ;;
esac

rm -rf dist
mkdir dist
missing=0
for b in "${BINARIES[@]}"; do
  src="target/release/$b"
  if [ ! -f "$src" ] && [ -f "target/release/$b.exe" ]; then
    src="target/release/$b.exe"
  fi
  if [ ! -f "$src" ]; then
    echo "error: missing build output: target/release/$b" >&2
    missing=1
    continue
  fi
  cp "$src" dist/
done
if [ "$missing" != 0 ]; then
  echo "error: refusing to bundle an incomplete release" >&2
  exit 1
fi

# Canonical model catalog (model-provisioning SSOT) ships next to the binary.
cp model_catalog.json dist/

case "$out" in
  *.tar.gz) (cd dist && tar czf "$out" *) ;;
  *.zip) (cd dist && 7z a "$out" ./*) ;;
  *) echo "error: unknown archive format: $out (want .tar.gz or .zip)" >&2; exit 1 ;;
esac

echo "bundled $out:"
case "$out" in
  *.tar.gz) tar tzf "$out" ;;
  *.zip) 7z l "$out" ;;
esac
