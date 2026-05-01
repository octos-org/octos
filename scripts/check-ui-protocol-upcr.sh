#!/usr/bin/env bash
# Ensure protocol-visible edits are paired with an explicit UI Protocol change request.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

protocol_changes="$(
  git status --porcelain --untracked-files=all -- \
    crates/octos-core/src/ui_protocol.rs \
    crates/octos-cli/src/api/ui_protocol.rs \
    crates/octos-cli/src/api/ui_protocol_*.rs \
    api/OCTOS_UI_PROTOCOL_V1_SPEC_*.md \
    2>/dev/null || true
)"

if [ -z "$protocol_changes" ]; then
  printf 'ui-protocol-upcr: no protocol-visible edits detected\n'
  exit 0
fi

upcr_changes="$(
  git status --porcelain --untracked-files=all -- \
    docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md \
    docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md \
    2>/dev/null || true
)"

spec_changes="$(
  git status --porcelain --untracked-files=all -- \
    api/OCTOS_UI_PROTOCOL_V1_SPEC_*.md \
    2>/dev/null || true
)"

if [ -n "$upcr_changes" ]; then
  printf 'ui-protocol-upcr: protocol edits have UPCR coverage\n'
  exit 0
fi

if [ -n "$spec_changes" ]; then
  printf 'ui-protocol-upcr: protocol edits have protocol spec coverage\n'
  exit 0
fi

if [ "${UPCR_ALLOW_NO_DOC:-0}" = "1" ]; then
  printf 'ui-protocol-upcr: protocol edits allowed by reviewer override\n'
  exit 0
fi

cat >&2 <<'EOF'
ui-protocol-upcr: protocol-visible edits require a UPCR document.

Add or update docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md, or set
UPCR_ALLOW_NO_DOC=1 only for a documented reviewer override.
EOF
printf '%s\n' "$protocol_changes" >&2
exit 1
