#!/usr/bin/env bash
# T-0543: make rustup persistent on the repo-owned macOS pool.
#
# octos-mac runners set CARGO_HOME/RUSTUP_HOME to runner-local dirs
# (scripts/setup-macos-ci-runner.sh) that are NOT on PATH, so
# `command -v rustup` fails in every job and dtolnay/rust-toolchain
# re-downloads rustup-init each time. On macmini1 the download
# intermittently dies on a curl SSL error (exit 35), reddening main and
# merge-group runs (run 42576108004 build-macos-arm; #3050 security).
#
# This script, run before dtolnay/rust-toolchain on the self-hosted macOS
# pool, first reuses a previously installed rustup (runner CARGO_HOME/bin
# or the account's ~/.cargo/bin — only paths that actually exist are added
# to GITHUB_PATH); when none exists it performs the ONE download with
# bounded retries instead of failing the job on a transient curl error.
set -euo pipefail

if [ "${1:-}" = "--self-test" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/cargo/bin" "$tmp/out"
  touch "$tmp/cargo/bin/rustup"
  chmod +x "$tmp/cargo/bin/rustup"
  CARGO_HOME="$tmp/cargo" GITHUB_PATH="$tmp/out/path" bash "$0"
  grep -qx "$tmp/cargo/bin" "$tmp/out/path"
  # Account-default path is used when the runner-local one is absent.
  mkdir -p "$tmp/home/.cargo/bin" "$tmp/out2"
  touch "$tmp/home/.cargo/bin/rustup"
  chmod +x "$tmp/home/.cargo/bin/rustup"
  env -u CARGO_HOME HOME="$tmp/home" GITHUB_PATH="$tmp/out2/path" bash "$0"
  grep -qx "$tmp/home/.cargo/bin" "$tmp/out2/path"
  echo "ci-ensure-rustup self-test ok"
  exit 0
fi

candidates=()
[ -n "${CARGO_HOME:-}" ] && candidates+=("$CARGO_HOME/bin")
candidates+=("$HOME/.cargo/bin")

for dir in "${candidates[@]}"; do
  if [ -x "$dir/rustup" ]; then
    echo "$dir" >> "$GITHUB_PATH"
    echo "rustup found at $dir/rustup; reusing it, no download"
    exit 0
  fi
done

echo "no existing rustup under ${candidates[*]}; downloading once with bounded retries"
tmp="$(mktemp)"
# 5 attempts with all-errors retry covers transient curl SSL/connect
# failures without turning a hard outage into an hour-long hang.
if ! curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 --connect-timeout 30 \
  https://sh.rustup.rs -o "$tmp"; then
  echo "rustup-init download failed after 5 attempts" >&2
  exit 1
fi
sh "$tmp" -y --default-toolchain none --profile minimal
rm -f "$tmp"
echo "${CARGO_HOME:-$HOME/.cargo}/bin" >> "$GITHUB_PATH"
