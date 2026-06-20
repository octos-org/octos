#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-/private/tmp/octos-post-eviction-replay-target}"

cd "$repo_root"
export CARGO_TARGET_DIR="$target_dir"

cargo test \
  -p octos-cli \
  --features api \
  ledger_post_eviction_validation_path_replays_from_persisted_jsonl \
  -- --nocapture
