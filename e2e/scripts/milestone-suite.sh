#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SCRIPT_PATH="${SCRIPT_DIR}/$(basename "$0")"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$ROOT"

AUTH_TOKEN="${OCTOS_AUTH_TOKEN:-octos-admin-2026}"
PROFILE_ID="${OCTOS_PROFILE:-dspfac}"
TEST_EMAIL="${OCTOS_TEST_EMAIL:-dspfac@gmail.com}"

CREW_URL="${OCTOS_CREW_URL:-https://dspfac.crew.ominix.io}"
BOT_URL="${OCTOS_BOT_URL:-https://dspfac.bot.ominix.io}"
OCEAN_URL="${OCTOS_OCEAN_URL:-https://dspfac.ocean.ominix.io}"
OUTPUT_ROOT="${OCTOS_E2E_OUTPUT_ROOT:-test-results/milestone}"
PLAYWRIGHT_EXTRA_ARGS="${OCTOS_PLAYWRIGHT_ARGS:-}"

usage() {
  cat <<'EOF'
Usage: ./e2e/scripts/milestone-suite.sh <suite>

Canonical milestone E2E suites:
  crew-core           live browser + session recovery + web client
  bot-runtime         runtime regression + tool-use regression
  ocean-deliverables  slides/site + refactor capabilities + coding hardcases
  all                 run every milestone live suite in sequence

Environment overrides:
  OCTOS_AUTH_TOKEN        auth token for live browser runs
  OCTOS_PROFILE           profile id (default: dspfac)
  OCTOS_TEST_EMAIL        login email used by helpers
  OCTOS_CREW_URL          base URL for crew suite
  OCTOS_BOT_URL           base URL for bot suite
  OCTOS_OCEAN_URL         base URL for ocean suite
  OCTOS_E2E_OUTPUT_ROOT   output root for Playwright results
  OCTOS_PLAYWRIGHT_ARGS   extra args appended to every playwright invocation
EOF
}

ensure_deps() {
  if [ ! -d node_modules/@playwright/test ]; then
    npm ci
  fi
}

run_suite() {
  local suite="$1"
  local base_url="$2"
  shift 2
  local output_dir="${OUTPUT_ROOT}/${suite}"
  mkdir -p "$output_dir"

  local extra_args=()
  if [ -n "$PLAYWRIGHT_EXTRA_ARGS" ]; then
    # shellcheck disable=SC2206
    extra_args=($PLAYWRIGHT_EXTRA_ARGS)
  fi

  env \
    OCTOS_TEST_URL="$base_url" \
    OCTOS_AUTH_TOKEN="$AUTH_TOKEN" \
    OCTOS_PROFILE="$PROFILE_ID" \
    OCTOS_TEST_EMAIL="$TEST_EMAIL" \
    npx playwright test "$@" --reporter=line --output="$output_dir" "${extra_args[@]}"
}

SUITE="${1:-}"
case "$SUITE" in
  crew-core)
    ensure_deps
    run_suite "crew-core" "$CREW_URL" \
      tests/live-browser.spec.ts \
      tests/session-recovery.spec.ts \
      tests/web-client.spec.ts
    ;;
  bot-runtime)
    ensure_deps
    run_suite "bot-runtime" "$BOT_URL" \
      tests/runtime-regression.spec.ts \
      tests/tool-use-regression.spec.ts
    ;;
  ocean-deliverables)
    ensure_deps
    run_suite "ocean-deliverables" "$OCEAN_URL" \
      tests/live-slides-site.spec.ts \
      tests/refactor-capabilities.spec.ts \
      tests/coding-hardcases.spec.ts
    ;;
  all)
    ensure_deps
    "$SCRIPT_PATH" crew-core
    "$SCRIPT_PATH" bot-runtime
    "$SCRIPT_PATH" ocean-deliverables
    ;;
  --help|-h|"")
    usage
    ;;
  *)
    echo "Unknown suite: $SUITE" >&2
    usage >&2
    exit 2
    ;;
esac
