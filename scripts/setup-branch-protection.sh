#!/usr/bin/env bash
#
# setup-branch-protection.sh — apply the repo settings and `main` branch
# protection that docs/CI_POLICY.md assumes.
#
# Dry-run by default: prints the gh commands without running them. Pass --apply
# to execute. Safe to re-run (the PUT is a full, idempotent config).
#
# Env overrides:
#   REPO=octos-org/octos  BRANCH=main  REVIEWS=2  CODEOWNER_REVIEWS=true
#
# NOTE: adding required status checks affects *every* open PR immediately. Only
# add a workflow to the list once it has run green on a real PR, otherwise PRs
# hang on "Expected — waiting for status to be reported".
#
set -euo pipefail

REPO="${REPO:-octos-org/octos}"
BRANCH="${BRANCH:-main}"
REVIEWS="${REVIEWS:-2}"
CODEOWNER_REVIEWS="${CODEOWNER_REVIEWS:-true}"

APPLY=0
[[ "${1:-}" == "--apply" ]] && APPLY=1

command -v gh >/dev/null || { echo "error: gh CLI not found" >&2; exit 1; }

run() {
  echo "+ $*"
  if [[ "$APPLY" == 1 ]]; then "$@"; else echo "  (dry-run)"; fi
}

echo "## 1. repository settings"
echo "# the single biggest cause of branch sprawl: stop keeping merged branches"
run gh api -X PATCH "repos/$REPO" -F delete_branch_on_merge=true

echo
echo "## 2. $BRANCH branch protection"
echo "# required check: the 'CI required' aggregate job in ci.yml"
echo "# reviews: $REVIEWS approving, dismiss stale, code-owner review=$CODEOWNER_REVIEWS"
echo "# plus: strict up-to-date, conversation resolution, no force-push, no delete"
run gh api -X PUT "repos/$REPO/branches/$BRANCH/protection" \
  -H "Accept: application/vnd.github+json" \
  -F "required_status_checks[strict]=true" \
  -F "required_status_checks[contexts][]=CI required" \
  -F "enforce_admins=false" \
  -F "required_pull_request_reviews[dismiss_stale_reviews]=true" \
  -F "required_pull_request_reviews[require_code_owner_reviews]=$CODEOWNER_REVIEWS" \
  -F "required_pull_request_reviews[required_approving_review_count]=$REVIEWS" \
  -F "required_conversation_resolution=true" \
  -F "allow_force_pushes=false" \
  -F "allow_deletions=false"

if [[ "$APPLY" != 1 ]]; then
  echo
  echo "(dry-run) re-run with --apply to make these changes."
fi
