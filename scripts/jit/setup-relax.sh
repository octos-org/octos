#!/bin/sh
# Builds the patched Relax JIT that scripts/jit/octos-jit runs Octos with.
#
# Relax (https://github.com/makepad/relax, MIT) compiles Rust from source inside
# the running process and patches saved edits into it. Octos needs fixes that
# are not upstream yet. They live in scripts/jit/relax-patches and apply to one
# pinned Relax commit. This script fetches that commit, applies the fixes and
# builds the `relax` binary. It does not touch the Octos build or its target/.
#
#   OCTOS_RELAX_DIR   where the checkout and its build live
#                     (default: $HOME/.octos-jit/relax)
#   OCTOS_RELAX_REPO  repository to fetch the pinned commit from
#                     (default: https://github.com/makepad/relax.git)
#
# Relax reads its own core/alloc/std from the checkout it was built in, so the
# folder has to stay where it was built. See docs/JIT_DEV_MODE.md.
set -eu

PIN=442f13bc333ed5cb4b3d75de62693251acd39dd7
REPO="${OCTOS_RELAX_REPO:-https://github.com/makepad/relax.git}"
DEST="${OCTOS_RELAX_DIR:-$HOME/.octos-jit/relax}"
HERE="$(cd "$(dirname "$0")" && pwd)"
PATCHES="$HERE/relax-patches"
STAMP_NAME=.octos-jit-stamp

say() { echo "setup-relax: $*"; }
die() { echo "setup-relax: $*" >&2; exit 1; }

for tool in git cargo; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is not on PATH"
done
ls "$PATCHES"/*.patch >/dev/null 2>&1 || die "no patches in $PATCHES"

# The pin and the patch contents together name what a checkout was made from.
WANT="$PIN $(cat "$PATCHES"/*.patch | git hash-object --stdin)"
BIN="$DEST/target/release/relax"

if [ -e "$DEST" ]; then
  if [ ! -f "$DEST/$STAMP_NAME" ] || [ "$(cat "$DEST/$STAMP_NAME")" != "$WANT" ]; then
    die "$DEST exists but was not made from this pin and patch set.
Remove that folder, or point OCTOS_RELAX_DIR at another one, and run again."
  fi
  if [ -x "$BIN" ]; then
    say "up to date: $BIN"
    exit 0
  fi
  say "sources are in place, building"
else
  PARTIAL="$DEST.partial.$$"
  # Only ever removes the folder this run created.
  trap 'rm -rf "$PARTIAL"' EXIT INT TERM
  mkdir -p "$PARTIAL"
  say "fetching Relax $PIN"
  git -C "$PARTIAL" init -q
  git -C "$PARTIAL" remote add origin "$REPO"
  git -C "$PARTIAL" fetch -q --depth 1 origin "$PIN"
  git -C "$PARTIAL" checkout -q --detach FETCH_HEAD
  [ "$(git -C "$PARTIAL" rev-parse HEAD)" = "$PIN" ] || die "fetched commit is not the pin"
  for patch in "$PATCHES"/*.patch; do
    say "applying $(basename "$patch")"
    git -C "$PARTIAL" apply "$patch"
  done
  echo "$WANT" > "$PARTIAL/$STAMP_NAME"
  mkdir -p "$(dirname "$DEST")"
  mv "$PARTIAL" "$DEST"
  trap - EXIT INT TERM
fi

say "building (a few minutes the first time)"
(cd "$DEST" && cargo build --release -p relax-runner --bin relax)
[ -x "$BIN" ] || die "the build finished without $BIN"
say "built $BIN"
