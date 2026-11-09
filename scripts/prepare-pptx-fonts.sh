#!/usr/bin/env bash
# Source before compiling the native PPTX runtime. Downloads only at build time.
# Use an existing clean pinned checkout for offline builds.
set -euo pipefail
pptx_font_rev=37827adc41fd165f9a9234ac40f94dbad353aa69
# Keep source inputs outside Cargo's target directory: rust-cache may prune or
# replace that directory after this script runs. Hosted jobs get a private temp
# directory; local builds can reuse the ignored scratchpad checkout.
pptx_font_root="${CRAFT_FONTS_DIR:-${RUNNER_TEMP:-scratchpad}/octos-pptx-fonts-$pptx_font_rev}"
if command -v cygpath >/dev/null 2>&1; then
  pptx_font_root="$(cygpath -u "$pptx_font_root")"
fi
if [ ! -d "$pptx_font_root/.git" ]; then
  if [ -e "$pptx_font_root" ]; then
    echo 'error: font input must be a clean pinned git checkout' >&2
    return 1 2>/dev/null || exit 1
  fi
  mkdir -p "$pptx_font_root"
  git -C "$pptx_font_root" init -q
  git -C "$pptx_font_root" fetch -q --depth=1 https://github.com/storytold/craft-fonts.git "$pptx_font_rev"
  git -C "$pptx_font_root" checkout -q --detach FETCH_HEAD
fi
if [ "$(git -C "$pptx_font_root" rev-parse HEAD)" != "$pptx_font_rev" ] || \
   [ -n "$(git -C "$pptx_font_root" status --porcelain --untracked-files=all)" ]; then
  echo 'error: font input revision or clean-tree verification failed' >&2
  return 1 2>/dev/null || exit 1
fi
export CRAFT_FONTS_DIR="$(cd "$pptx_font_root" && pwd)"
if command -v cygpath >/dev/null 2>&1; then
  export CRAFT_FONTS_DIR="$(cygpath -m "$CRAFT_FONTS_DIR")"
fi
export CRAFT_FONTS_REQUIRED=1
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "CRAFT_FONTS_DIR=$CRAFT_FONTS_DIR" >> "$GITHUB_ENV"
  echo 'CRAFT_FONTS_REQUIRED=1' >> "$GITHUB_ENV"
fi
unset pptx_font_rev pptx_font_root
