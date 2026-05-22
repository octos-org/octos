#!/usr/bin/env bash
# Ensure protocol-visible edits are paired with an explicit UI Protocol change
# request (UPCR) doc. Compares the current branch against a base ref
# (default: origin/main, falling back to main / origin/master / master) using
# `git diff --name-status --diff-filter=AMR <merge-base>..HEAD` so the check
# covers committed work — including changes the user split across multiple
# commits and rename-with-edit moves where the destination is the file we
# care about. Uncommitted changes (staged + unstaged + untracked) are folded
# in so the gate behaves correctly when run pre-commit. Whitespace-only diffs
# are exempted via `git diff -w --stat`.
#
# Coverage rules:
#   * Any change to a *.rs protocol file requires an added/modified
#     `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md` in the same diff
#     range. Editing the spec doc alone does NOT satisfy this because the
#     spec edit may be unrelated (typo fix, broken link, etc.).
#   * If the only protocol-visible change is the spec doc itself, the spec
#     change is self-coverage.
#   * `UPCR_ALLOW_NO_DOC=1` is a reviewer-override escape hatch.
#
# If neither a base ref nor a merge-base can be resolved, the gate refuses
# to run rather than silently checking only uncommitted state — that
# behaviour was the bypass closed by #717.

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
    if git rev-parse --verify --quiet "$UPCR_BASE_REF" >/dev/null; then
      printf '%s\n' "$UPCR_BASE_REF"
      return 0
    fi
    cat >&2 <<EOF
ui-protocol-upcr: UPCR_BASE_REF='$UPCR_BASE_REF' does not resolve in this checkout.
EOF
    return 2
  fi
  for candidate in origin/main main origin/master master; do
    if git rev-parse --verify --quiet "$candidate" >/dev/null; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

resolve_status=0
base_ref="$(resolve_base_ref)" || resolve_status=$?

merge_base=""
head_sha=""
if [ "$resolve_status" -eq 0 ] && [ -n "$base_ref" ]; then
  merge_base="$(git merge-base "$base_ref" HEAD 2>/dev/null || true)"
  head_sha="$(git rev-parse --verify --quiet HEAD 2>/dev/null || true)"
fi

# Treat merge_base == HEAD as "no committed work to inspect": HEAD..HEAD is
# empty, so the committed-range scan would silently miss any actual diffs.
# This is the bypass codex review #6 flagged. We distinguish two sub-cases:
#
#   * Uncommitted protocol changes exist in the working tree -> legitimate
#     pre-commit case (feature branch freshly created off main); proceed
#     with uncommitted-only checking.
#   * No uncommitted protocol changes either -> the gate cannot tell
#     whether the repo lacks a real base ref (shallow CI) or whether the
#     branch is genuinely identical to main. Fail loud so a stale base
#     ref doesn't silently bypass committed-but-not-yet-pushed work.
base_equals_head=0
if [ -n "$merge_base" ] && [ -n "$head_sha" ] && [ "$merge_base" = "$head_sha" ]; then
  base_equals_head=1
fi

if [ -z "$merge_base" ] || [ "$base_equals_head" -eq 1 ]; then
  # Probe the working tree for any uncommitted *trigger* changes only.
  # A stray untracked UPCR / template / spec edit must NOT mask a committed
  # protocol change here — that bypass was codex review #7. We're trying to
  # answer "is there real protocol work in the working tree that justifies
  # running in uncommitted-only mode?", and only protocol-code and spec-doc
  # files (the trigger set) can answer yes.
  uncommitted_probe="$(
    git status --porcelain --untracked-files=all -- \
      "${PROTOCOL_PATHS[@]}" "${PROTOCOL_GLOBS[@]}" "$SPEC_GLOB" \
      2>/dev/null || true
  )"
  if [ -z "$uncommitted_probe" ]; then
    if [ "${UPCR_ALLOW_NO_DOC:-0}" = "1" ]; then
      printf 'ui-protocol-upcr: no base ref available; allowed by reviewer override\n'
      exit 0
    fi
    cat >&2 <<'EOF'
ui-protocol-upcr: could not resolve a usable base ref for the diff. The gate
tried origin/main, main, origin/master, master, and any UPCR_BASE_REF
override; either none resolved or the resolved ref points to HEAD itself
(which would diff HEAD..HEAD = empty). Refusing to run with only stale
state because that lets committed protocol changes slip through CI.

Fix: fetch a real base (e.g. `git fetch --no-tags origin main`), or set
UPCR_BASE_REF=<sha-or-ref> that points to the actual target branch this
PR/branch is built against (e.g. the merge-base SHA against origin/main,
or the commit of the target branch tip). Do NOT use `HEAD~1` for branches
that contain multiple commits — the gate would diff only the last commit
and report no protocol-visible edits even when an earlier commit on the
branch touched protocol Rust. Use `HEAD~1` only as a single-commit-only
case. As a last resort, set UPCR_ALLOW_NO_DOC=1 for a documented
reviewer override.
EOF
    exit 2
  fi
  # Uncommitted protocol/UPCR changes exist; proceed in uncommitted-only mode.
  merge_base=""
fi

if [ -n "$merge_base" ]; then
  range="$merge_base..HEAD"
else
  range=""
fi

# Emit destination paths from `git diff --name-status` in the committed
# range. The second argument selects between two filters:
#
#   * trigger  -> AMRD: Added / Modified / Renamed / Deleted. Removal of a
#                 protocol file is itself a protocol-visible event and must
#                 be gated (codex review #4 / #717).
#   * coverage -> AMR  only: a *deleted* UPCR or spec file is NOT valid
#                 coverage for a protocol-code change (codex review #5).
#
# For renames the trailing column is the destination, which is what we want
# (we care about the file that lives in the working tree at HEAD, not the
# pre-rename path). Whitespace-only changes are filtered by re-running
# `git diff -w --stat` on each candidate and dropping those with empty stat.
diff_range_names() {
  local range="$1"
  local mode="$2"
  shift 2
  if [ -z "$range" ]; then
    return 0
  fi
  local filter
  case "$mode" in
    trigger)  filter="AMRD" ;;
    coverage) filter="AMR"  ;;
    *)
      echo "diff_range_names: unknown mode '$mode'" >&2
      return 2
      ;;
  esac
  local raw
  raw="$(git diff --name-status --diff-filter="$filter" "$range" -- "$@" 2>/dev/null || true)"
  if [ -z "$raw" ]; then
    return 0
  fi
  local line code name
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    code="$(printf '%s' "$line" | awk '{print $1}')"
    case "$code" in
      R*)
        # Format: "R<score><TAB>old/path<TAB>new/path" — destination is what
        # lives in the working tree, so that's what consumers see.
        name="$(printf '%s' "$line" | awk '{print $3}')"
        ;;
      *)
        name="$(printf '%s' "$line" | awk '{print $2}')"
        ;;
    esac
    [ -z "$name" ] && continue
    case "${code:0:1}" in
      R|D)
        # Pure rename or deletion is itself a protocol-visible event;
        # `git diff -w --stat` on a deleted path is meaningless and a
        # rename-only delta can come back empty under `-w`. Short-circuit.
        printf '%s\n' "$name"
        ;;
      *)
        local stat
        stat="$(git diff -w --stat "$range" -- "$name" 2>/dev/null || true)"
        if [ -n "$stat" ]; then
          printf '%s\n' "$name"
        fi
        ;;
    esac
  done <<<"$raw"
}

# Uncommitted change names (staged + unstaged + untracked) for matching paths.
#
# $1 = mode: "trigger" includes deletions, "coverage" excludes them.
uncommitted_names() {
  local mode="$1"
  shift
  case "$mode" in
    trigger|coverage) ;;
    *)
      echo "uncommitted_names: unknown mode '$mode'" >&2
      return 2
      ;;
  esac
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
      "??"|"A "|"AM"|" A"|R*)
        # Adds and renames count for both modes.
        printf '%s\n' "$path"
        ;;
      "D "|" D"|"DD"|"AD"|"MD")
        # Deletions count for trigger only — a deleted UPCR is not coverage.
        if [ "$mode" = "trigger" ]; then
          printf '%s\n' "$path"
        fi
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
  local mode="$2"
  shift 2
  {
    diff_range_names "$range" "$mode" "$@"
    uncommitted_names "$mode" "$@"
  } | awk 'NF && !seen[$0]++'
}

# Split changes into code-level (Rust source) and spec-doc buckets so the
# coverage rules can treat them differently. Codex review #3 (#717): an
# unrelated spec edit must not satisfy the gate for a code-level diff.
# Trigger mode includes deletions so removing a protocol file is gated.
code_changes="$(collect_names "$range" trigger "${PROTOCOL_PATHS[@]}" "${PROTOCOL_GLOBS[@]}")"
spec_changes="$(collect_names "$range" trigger "$SPEC_GLOB")"

if [ -z "$code_changes" ] && [ -z "$spec_changes" ]; then
  printf 'ui-protocol-upcr: no protocol-visible edits detected\n'
  exit 0
fi

# Coverage mode for UPCR docs: only added or modified entries count. A
# deletion of an old UPCR is NOT coverage for a fresh protocol change
# (codex review #5 / #717).
upcr_changes="$(collect_names "$range" coverage "$UPCR_GLOB" "$UPCR_TEMPLATE")"

# Verify the matched UPCR file looks like a real UPCR-YYYY-NNN doc.
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

# No UPCR doc. A spec-only edit can self-cover, but only if the spec change
# is an add/modify; a deletion-only spec change is just a protocol-visible
# removal and still needs an explicit UPCR.
if [ -z "$code_changes" ] && [ -n "$spec_changes" ]; then
  spec_coverage="$(collect_names "$range" coverage "$SPEC_GLOB")"
  if [ -n "$spec_coverage" ]; then
    printf 'ui-protocol-upcr: spec-only edit, treated as self-coverage\n'
    exit 0
  fi
fi

if [ "${UPCR_ALLOW_NO_DOC:-0}" = "1" ]; then
  printf 'ui-protocol-upcr: protocol edits allowed by reviewer override\n'
  exit 0
fi

cat >&2 <<EOF
ui-protocol-upcr: protocol-visible edits require a UPCR document.

Detected code-level protocol changes (range: ${range:-uncommitted-only}):
EOF
while IFS= read -r _change; do
  [ -z "$_change" ] && continue
  printf '  %s\n' "$_change" >&2
done <<<"$code_changes"
if [ -n "$spec_changes" ]; then
  cat >&2 <<EOF

Spec-doc changes are also present, but a spec edit alone does NOT satisfy the
gate when Rust protocol surfaces have changed — it could be an unrelated fix.
Add an explicit UPCR doc that references this change.
EOF
fi
cat >&2 <<'EOF'

Add or update docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md in the same
branch, or set UPCR_ALLOW_NO_DOC=1 only for a documented reviewer override.
EOF
exit 1
