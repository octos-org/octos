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

# Case analysis for the diff range:
#
#   * resolve_base_ref failed entirely (no origin/main, no main, no
#     UPCR_BASE_REF) -> we cannot reason about committed work at all.
#     Fail loud unless UPCR_ALLOW_NO_DOC=1 (closes codex #1).
#
#   * merge_base resolves but equals HEAD -> normal for a freshly-created
#     feature branch where the only relevant work is staged or unstaged.
#     Fall through in uncommitted-only mode and set `strict_uncommitted=1`,
#     which disables the spec-only self-coverage fallback. Without that
#     guard, a stray dirty spec edit could mask a committed protocol-code
#     change that lives in HEAD itself (closes codex #6/#8/#10).
#
#   * merge_base != HEAD -> normal committed-range scan.
base_equals_head=0
if [ -n "$merge_base" ] && [ -n "$head_sha" ] && [ "$merge_base" = "$head_sha" ]; then
  base_equals_head=1
fi

strict_uncommitted=0
# Treat both (a) no base ref resolvable and (b) base ref resolved but
# `git merge-base` returned empty as the same failure: we cannot diff
# committed work against the target branch. Case (b) hits on shallow PR
# checkouts that fetched origin/main as a separate depth-1 tip with no
# shared ancestor, or when UPCR_BASE_REF points at an unrelated history.
# Without this guard the script silently falls into uncommitted-only mode
# and lets committed protocol changes bypass the gate (codex review #11).
if [ "$resolve_status" -ne 0 ] || [ -z "$base_ref" ] \
    || { [ -n "$base_ref" ] && [ -z "$merge_base" ]; }; then
  if [ "${UPCR_ALLOW_NO_DOC:-0}" = "1" ]; then
    printf 'ui-protocol-upcr: no base ref available; allowed by reviewer override\n'
    exit 0
  fi
  cat >&2 <<'EOF'
ui-protocol-upcr: could not resolve a usable merge-base for the diff. The gate
tried origin/main, main, origin/master, master, and any UPCR_BASE_REF
override; either no base ref resolved, or `git merge-base` found no common
ancestor between the base ref and HEAD (typical in shallow PR checkouts that
fetched the base as a depth-1 tip without shared history). Refusing to run
because that lets committed protocol changes slip through CI.

Fix: fetch a real base with full history (e.g. `git fetch --no-tags --unshallow
origin main`, or `git fetch --depth=N origin main` deep enough to reach the
merge-base), or set UPCR_BASE_REF=<sha-or-ref> to a commit that is actually an
ancestor of HEAD. As a last resort, set UPCR_ALLOW_NO_DOC=1 for a documented
reviewer override.
EOF
  exit 2
fi

if [ "$base_equals_head" -eq 1 ]; then
  # Pre-commit / freshly-created-branch flow: we'll run in uncommitted-only
  # mode IFF there's actual uncommitted trigger work to inspect. If the
  # working tree is clean and merge_base == HEAD, we cannot tell whether
  # the branch is genuinely identical to main (no work, nothing to gate)
  # or whether a stale base ref is hiding committed protocol edits. Fall
  # back to a probe of uncommitted trigger paths; if the probe is empty,
  # fail loud. This is the careful split between codex #6 (close the
  # silent-empty-diff bypass when there's no uncommitted work either) and
  # codex #10 (don't reject legitimate pre-commit staged-trigger work).
  pre_commit_probe="$(
    git status --porcelain --untracked-files=all -- \
      "${PROTOCOL_PATHS[@]}" "${PROTOCOL_GLOBS[@]}" "$SPEC_GLOB" \
      2>/dev/null || true
  )"
  if [ -z "$pre_commit_probe" ]; then
    if [ "${UPCR_ALLOW_NO_DOC:-0}" = "1" ]; then
      printf 'ui-protocol-upcr: base ref resolves to HEAD; allowed by reviewer override\n'
      exit 0
    fi
    cat >&2 <<'EOF'
ui-protocol-upcr: the resolved base ref points to HEAD and the working tree
has no uncommitted protocol/spec edits. The committed-range diff (HEAD..HEAD)
is empty by construction, which can hide committed protocol changes on a
stale single-branch CI checkout. Refusing to run.

Fix: fetch a real base (e.g. `git fetch --no-tags origin main`), or set
UPCR_BASE_REF=<sha-or-ref> that points to the actual target branch this
PR/branch is built against. As a last resort, set UPCR_ALLOW_NO_DOC=1 for a
documented reviewer override.
EOF
    exit 2
  fi
  # Uncommitted trigger work exists; proceed in strict uncommitted-only mode.
  # `strict_uncommitted` disables the spec-only self-coverage fallback so
  # a stray dirty spec can't mask a committed protocol change in HEAD.
  merge_base=""
  strict_uncommitted=1
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

# Emit paths that are currently slated for deletion in the working tree
# (staged or unstaged) for the given path-specs. Used in coverage mode to
# subtract paths that the user is removing from the branch's coverage set
# — closes codex review #9 / #717 where committing a UPCR and then staging
# its deletion would still report UPCR coverage.
uncommitted_deletions() {
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
    if [[ "$status_code" == R* ]]; then
      path="${path##* -> }"
    fi
    case "$status_code" in
      "D "|" D"|"DD"|"AD"|"MD")
        printf '%s\n' "$path"
        ;;
    esac
  done <<<"$entries"
}

collect_names() {
  local range="$1"
  local mode="$2"
  shift 2
  local merged
  merged="$(
    {
      diff_range_names "$range" "$mode" "$@"
      uncommitted_names "$mode" "$@"
    } | awk 'NF && !seen[$0]++'
  )"
  if [ "$mode" != "coverage" ] || [ -z "$merged" ]; then
    printf '%s\n' "$merged"
    return 0
  fi
  # In coverage mode, subtract any path that is currently being deleted in
  # the working tree even if it appears in the committed AMR range. A
  # branch that committed a UPCR and then staged its deletion no longer
  # has that UPCR as coverage at the tip of the branch.
  local pending_deletes
  pending_deletes="$(uncommitted_deletions "$@")"
  if [ -z "$pending_deletes" ]; then
    printf '%s\n' "$merged"
    return 0
  fi
  awk -v deletes="$pending_deletes" '
    BEGIN {
      n = split(deletes, arr, "\n")
      for (i = 1; i <= n; i++) {
        if (arr[i] != "") del[arr[i]] = 1
      }
    }
    NF && !(($0) in del)
  ' <<<"$merged"
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
# is an add/modify AND we have a real committed-range diff. In strict
# uncommitted-only mode (merge_base == HEAD) the spec-only shortcut is
# disabled, because a stray dirty spec edit could otherwise mask a
# committed protocol-code change that lives in HEAD itself (codex #8/#10).
if [ "$strict_uncommitted" -eq 0 ] \
    && [ -z "$code_changes" ] && [ -n "$spec_changes" ]; then
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
