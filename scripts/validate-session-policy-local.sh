#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/private/tmp/octos-session-sandbox-target}"

echo "==> protocol: session sandbox capability and validation"
cargo test -p octos-core session_open_params_topic_cwd_and_sandbox_are_additive_and_round_trip -- --nocapture
cargo test -p octos-cli --features api session_sandbox_ -- --nocapture

echo "==> runtime: same profile, distinct per-session sandbox policies"
cargo test -p octos-cli --features api bootstrap_with_explicit_sandbox_overrides_are_per_session -- --nocapture

echo "session sandbox local validation passed"
