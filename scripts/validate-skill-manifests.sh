#!/usr/bin/env bash
#
# RFC-2 (issue #1291): validate every bundled `crates/app-skills/*/manifest.json`
# (plus any additional manifest paths passed on the command line) against the
# strict octos JSON schema validator.
#
# Backstory: on 2026-05-25 `mofa-slides v0.5.0` shipped with
# `input_schema.anyOf[]` branches that lacked `type`. Strict LLM-provider
# validators (Moonshot Kimi K2.6 et al.) reject this shape at request time
# and the failure surfaces deep inside the agent loop with no useful pointer
# back to the manifest. RFC-2 adds a parse-time validator AND this CI hook
# so the bug class is caught at PR review rather than in production.
#
# Usage
# -----
#   scripts/validate-skill-manifests.sh                # all bundled
#   scripts/validate-skill-manifests.sh foo/manifest.json bar/manifest.json
#
# Environment
# -----------
#   OCTOS_MANIFEST_VALIDATION   strict (default) | lenient | off
#
# Exits non-zero if any manifest fails validation. Builds the
# `validate_manifests` bin in release mode so subsequent invocations are
# fast — the binary is small and pure-Rust so this is cheap even in CI.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_SKILLS_DIR="$ROOT/crates/app-skills"

# Build the bin once. Use the workspace cargo flags the rest of CI uses
# so the artefact slots into Swatinem/rust-cache like every other test
# job's compile cache.
echo "[validate-skill-manifests] building validate_manifests bin..."
(cd "$ROOT" && cargo build -p octos-plugin --bin validate_manifests --quiet)

BIN="$ROOT/target/debug/validate_manifests"
if [ ! -x "$BIN" ]; then
    echo "validate_manifests bin not found at $BIN" >&2
    exit 2
fi

# Collect manifests. Default = every `crates/app-skills/*/manifest.json`.
# Extra paths on the command line append (e.g. for testing a vendor skill
# prior to install).
declare -a MANIFESTS=()
if [ "$#" -eq 0 ]; then
    for d in "$APP_SKILLS_DIR"/*; do
        manifest="$d/manifest.json"
        if [ -f "$manifest" ]; then
            MANIFESTS+=("$manifest")
        fi
    done
else
    MANIFESTS=("$@")
fi

if [ "${#MANIFESTS[@]}" -eq 0 ]; then
    echo "[validate-skill-manifests] no manifests found — nothing to do" >&2
    exit 0
fi

echo "[validate-skill-manifests] validating ${#MANIFESTS[@]} manifest(s)..."
"$BIN" "${MANIFESTS[@]}"
