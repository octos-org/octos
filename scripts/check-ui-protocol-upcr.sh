#!/usr/bin/env bash
# Ensure protocol-visible edits are paired with an explicit UI Protocol change
# request (UPCR) doc or a protocol spec revision. Compares the current branch
# against a base ref (default: origin/main, falling back to main) using
# `git diff --name-only --diff-filter=AM` so the check covers committed work,
# including changes the user split across multiple commits. Uncommitted
# changes (staged + unstaged + untracked) are folded in so the gate behaves
# correctly when run pre-commit. Whitespace-only diffs are exempted via
# `git diff -w --stat`.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PROTOCOL_PATHS=(
  "crates/octos-core/src/ui_protocol.rs"
  "crates/octos-cli/src/api/ui_protocol.rs"
)
PROTOCOL_GLOBS=(
  "crates/octos-cli/src/api/ui_protocol_*.rs"
)
SPEC_GLOB="api/OCTOS_UI_PROTOCOL_V1_SPEC_*.md"
UPCR_GLOB="docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md"
UPCR_TEMPLATE="docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_TEMPLATE.md"

# Resolve a base ref for the merge-base diff. Allow override via UPCR_BASE_REF.
resolve_base_ref() {
  if [ -n "${UPCR_BASE_REF:-}" ]; then
    printf '%s\n' "$UPCR_BASE_REF"
    return 0
  fi
  for candidate in origin/main main origin/master master; do
    if git rev-parse --verify --quiet "$candidate" >/dev/null; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

base_ref=""
if base_ref="$(resolve_base_ref)"; then
  merge_base="$(git merge-base "$base_ref" HEAD 2>/dev/null || true)"
else
  merge_base=""
fi

# Names that changed between merge-base and HEAD (committed work),
# restricted to Added/Modified (filters out renames/deletes whose new path
# is what we actually care about). Whitespace-only changes are stripped via
# `git diff -w --stat` -> empty stat means no semantic change.
diff_range_names() {
  local range="$1"
  shift || true
  if [ -z "$range" ]; then
    return 0
  fi
  # First collect candidate names by AM filter, then re-confirm via -w --stat.
  local candidates
  candidates="$(git diff --name-only --diff-filter=AM "$range" -- "$@" 2>/dev/null || true)"
  if [ -z "$candidates" ]; then
    return 0
  fi
  local name
  while IFS= read -r name; do
    [ -z "$name" ] && continue
    local stat
    stat="$(git diff -w --stat "$range" -- "$name" 2>/dev/null || true)"
    if [ -n "$stat" ]; then
      printf '%s\n' "$name"
    fi
  done <<<"$candidates"
}

# Uncommitted change names (staged + unstaged + untracked) for matching paths.
# Uses `git status --porcelain` so we still gate pre-commit work, but the
# whitespace filter only applies to tracked changes (untracked files are by
# definition new content).
uncommitted_names() {
  local entries
  entries="$(git status --porcelain --untracked-files=all -- "$@" 2>/dev/null || true)"
  if [ -z "$entries" ]; then
    return 0
  fi
  local line status_code path
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    status_code="${line:0:2}"
    path="${line:3}"
    # Handle renames: "R  old -> new" — take the new path.
    if [[ "$status_code" == R* ]]; then
      path="${path##* -> }"
    fi
    case "$status_code" in
      "??"|"A "|"AM"|" A")
        printf '%s\n' "$path"
        ;;
      *)
        # Tracked modification — drop if whitespace-only.
        local stat
        stat="$(git diff -w --stat HEAD -- "$path" 2>/dev/null || true)"
        local stat_cached
        stat_cached="$(git diff -w --cached --stat -- "$path" 2>/dev/null || true)"
        if [ -n "$stat" ] || [ -n "$stat_cached" ]; then
          printf '%s\n' "$path"
        fi
        ;;
    esac
  done <<<"$entries"
}

collect_names() {
  local range="$1"
  shift
  {
    diff_range_names "$range" "$@"
    uncommitted_names "$@"
  } | awk 'NF && !seen[$0]++'
}

range=""
if [ -n "$merge_base" ]; then
  range="$merge_base..HEAD"
fi

protocol_changes="$(collect_names "$range" "${PROTOCOL_PATHS[@]}" "${PROTOCOL_GLOBS[@]}" "$SPEC_GLOB")"

if [ -z "$protocol_changes" ]; then
  printf 'ui-protocol-upcr: no protocol-visible edits detected\n'
  exit 0
fi

upcr_changes="$(collect_names "$range" "$UPCR_GLOB" "$UPCR_TEMPLATE")"
spec_changes="$(collect_names "$range" "$SPEC_GLOB")"

# Sanity-check: at least one UPCR file looks like a real UPCR-YYYY-NNN doc
# (i.e. matches docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md). The collect
# step already restricts to that glob, but we re-verify the name pattern here
# so a stray match (e.g. someone renaming the template) can't satisfy the
# gate by accident.
upcr_real=""
while IFS= read -r name; do
  [ -z "$name" ] && continue
  case "$name" in
    docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md)
      upcr_real="$name"
      break
      ;;
  esac
done <<<"$upcr_changes"

if [ -n "$upcr_real" ]; then
  printf 'ui-protocol-upcr: protocol edits have UPCR coverage (%s)\n' "$upcr_real"
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

cat >&2 <<EOF
ui-protocol-upcr: protocol-visible edits require a UPCR document.

Detected protocol changes (range: ${range:-uncommitted only}):
EOF
printf '  %s\n' $protocol_changes >&2
cat >&2 <<'EOF'

Add or update docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md (or revise the
api/OCTOS_UI_PROTOCOL_V1_SPEC_*.md spec) in the same branch, or set
UPCR_ALLOW_NO_DOC=1 only for a documented reviewer override.
EOF
exit 1
