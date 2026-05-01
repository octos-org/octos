#!/usr/bin/env bash
# promote-captured-fixture.sh — copy a captured live-soak fixture into the
# Layer 1 SPA reducer test corpus so it locks in the regression.
#
# Usage:
#   promote-captured-fixture.sh <source.json> <target-name>
#
# Example:
#   ./e2e/scripts/promote-captured-fixture.sh \
#       e2e/fixtures/captured/live-overflow-stress-2026-04-30_19-12-03-456.json \
#       overflow-stress-thread-binding
#
# Result:
#   crates/octos-web/src/state/__tests__/fixtures/captured/overflow-stress-thread-binding.fixture.json
#
# Idempotency:
#   - The default REFUSES to overwrite an existing target. This prevents
#     a re-run from silently destroying manual triage edits (assertions,
#     turn_id, cmid) the operator added to the promoted fixture. Script
#     exits 2 if target exists.
#   - Pass `--force` to overwrite (e.g. when re-promoting after pulling a
#     fresher capture before the target was triaged).
#   - Pass `--dry-run` to print what would happen without writing.
#
# Safety:
#   - Validates that the source file exists and is JSON-parseable.
#   - Validates that the source has a non-empty `events` array.
#   - Refuses to write outside the configured target dir (no `..` in name).
#
# After promotion, edit the JSON if needed (e.g. fill in turn_id / cmid
# fields that the live capture didn't carry; add an `assertions` block
# describing the bug class) — see e2e/fixtures/captured/README.md for the
# format and triage guidance.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="$REPO_ROOT/crates/octos-web/src/state/__tests__/fixtures/captured"

DRY_RUN=0
FORCE=0

POSITIONAL=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --force) FORCE=1; shift ;;
    --no-clobber)
      # Back-compat alias: --no-clobber is the default behaviour now.
      # Accept the flag silently so any wrapper scripts that pass it
      # continue to work.
      shift ;;
    -h|--help)
      sed -n '2,34p' "$0"
      exit 0
      ;;
    --) shift; while [[ $# -gt 0 ]]; do POSITIONAL+=("$1"); shift; done ;;
    -*)
      echo "promote: unknown flag: $1" >&2
      exit 64
      ;;
    *) POSITIONAL+=("$1"); shift ;;
  esac
done

if [[ ${#POSITIONAL[@]} -ne 2 ]]; then
  echo "Usage: $(basename "$0") [--dry-run] [--force] <source.json> <target-name>" >&2
  exit 64
fi

SRC="${POSITIONAL[0]}"
NAME="${POSITIONAL[1]}"

if [[ ! -f "$SRC" ]]; then
  echo "promote: source not found: $SRC" >&2
  exit 66
fi

# Disallow path-injection in the target name; we want a flat layout.
if [[ "$NAME" == *"/"* || "$NAME" == *".."* || "$NAME" == .* ]]; then
  echo "promote: target name must be a flat slug (no '/', no '..'): $NAME" >&2
  exit 65
fi

# Validate JSON + minimal structural sanity. Use node so we don't add a
# python dep; node is already in the e2e harness.
node - "$SRC" <<'NODE_VALIDATION' || { echo "promote: source failed validation" >&2; exit 65; }
const fs = require('fs');
const p = process.argv[2];
const txt = fs.readFileSync(p, 'utf-8');
let j;
try { j = JSON.parse(txt); } catch (e) {
  console.error('json parse error:', e.message);
  process.exit(1);
}
if (!Array.isArray(j.events)) {
  console.error('source is missing events[]');
  process.exit(1);
}
if (j.events.length === 0 && (!Array.isArray(j.raw_events) || j.raw_events.length === 0)) {
  console.error('source has zero events AND zero raw_events; nothing to promote');
  process.exit(1);
}
console.error(`source validates: ${j.events.length} normalized events, ${(j.raw_events || []).length} raw frames`);
NODE_VALIDATION

DEST="$TARGET_DIR/$NAME.fixture.json"

if [[ -e "$DEST" && $FORCE -eq 0 ]]; then
  echo "promote: target exists; refusing to overwrite without --force: $DEST" >&2
  echo "promote: re-run with --force to replace, or pick a different target name." >&2
  exit 2
fi

if [[ $DRY_RUN -eq 1 ]]; then
  echo "promote: DRY RUN"
  echo "  source: $SRC"
  echo "  target: $DEST"
  exit 0
fi

mkdir -p "$TARGET_DIR"
cp "$SRC" "$DEST"
echo "promote: wrote $DEST"
echo "promote: review the file and add an 'assertions' block before committing."
echo "promote: see e2e/fixtures/captured/README.md for triage guidance."
