#!/usr/bin/env bash
# M7.8 release gate: live swarm dispatch validation on a canary.
#
# This gate is intentionally canary-only. It sets OCTOS_M7_SWARM_LIVE=1 before
# invoking Playwright, which keeps the spec skipped during default e2e runs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURE_PATH="${ROOT}/e2e/fixtures/m7-swarm-expected.json"

BASE_URL="${OCTOS_TEST_URL:-}"
AUTH_TOKEN="${OCTOS_AUTH_TOKEN:-}"
PROFILE_ID="${OCTOS_PROFILE:-dspfac}"
TEST_EMAIL="${OCTOS_TEST_EMAIL:-dspfac@gmail.com}"
OUTPUT_DIR=""
HEADED=false
LIST_ONLY=false
PLAYWRIGHT_ARGS=()

RED=$'\033[31m'
GREEN=$'\033[32m'
YELLOW=$'\033[33m'
BOLD=$'\033[1m'
RESET=$'\033[0m'

log() { printf "%s[%s]%s %s\n" "$BOLD" "$(date -u +%H:%M:%SZ)" "$RESET" "$*"; }
pass() { printf "  %s+%s %s\n" "$GREEN" "$RESET" "$*"; }
fail() { printf "  %sx%s %s\n" "$RED" "$RESET" "$*" >&2; }
warn() { printf "  %s!%s %s\n" "$YELLOW" "$RESET" "$*" >&2; }

usage() {
  cat <<'USAGE'
Usage: ./scripts/validate-m7-swarm-live.sh --base-url <url> --auth-token <token> [options]

Required arguments:
  --base-url <url>        canary URL, for example https://dspfac.crew.ominix.io
  --auth-token <token>    admin auth token used for API, WS, and Playwright

Optional arguments:
  --profile <id>          profile id (default: dspfac)
  --test-email <email>    login email used by Playwright helpers
  --output-dir <path>     write diagnostics and Playwright output here
  --headed                run Playwright headed
  --list                  list the five Playwright tests without live traffic
  --                      pass remaining arguments through to Playwright

Environment overrides mirror the CLI flags:
  OCTOS_TEST_URL, OCTOS_AUTH_TOKEN, OCTOS_PROFILE, OCTOS_TEST_EMAIL,
  OCTOS_M7_SWARM_OUTPUT_DIR

Exit codes:
  0   all assertions passed, or --list succeeded
  1   Playwright or npm failure
  2   missing prerequisite or invalid arguments
  3   refused host or fixture problem
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-url) BASE_URL="$2"; shift 2 ;;
    --auth-token) AUTH_TOKEN="$2"; shift 2 ;;
    --profile) PROFILE_ID="$2"; shift 2 ;;
    --test-email) TEST_EMAIL="$2"; shift 2 ;;
    --output-dir) OUTPUT_DIR="$2"; shift 2 ;;
    --headed) HEADED=true; shift ;;
    --list) LIST_ONLY=true; shift ;;
    --help|-h) usage; exit 0 ;;
    --) shift; PLAYWRIGHT_ARGS+=("$@"); break ;;
    *) fail "unknown argument: $1"; usage >&2; exit 2 ;;
  esac
done

if [[ ! -f "$FIXTURE_PATH" ]]; then
  fail "fixture missing: $FIXTURE_PATH"
  exit 3
fi

command -v node >/dev/null 2>&1 || { fail "node is required"; exit 2; }
command -v npm >/dev/null 2>&1 || { fail "npm is required"; exit 2; }

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="${OCTOS_M7_SWARM_OUTPUT_DIR:-${ROOT}/e2e/test-results-m7-swarm-live/$(date -u +%Y%m%dT%H%M%SZ)}"
fi
mkdir -p "$OUTPUT_DIR"

DIAGNOSTIC_JSON="${OUTPUT_DIR}/diagnostic.json"

write_diagnostic() {
  local status="$1"
  local kind="$2"
  local detail="$3"
  local exit_code="${4:-0}"
  node - "$DIAGNOSTIC_JSON" "$status" "$kind" "$detail" "$exit_code" "$BASE_URL" "$PROFILE_ID" <<'NODE'
const fs = require('node:fs');
const [path, status, kind, detail, exitCode, baseUrl, profile] = process.argv.slice(2);
fs.mkdirSync(require('node:path').dirname(path), { recursive: true });
fs.writeFileSync(path, JSON.stringify({
  schema: 'octos.swarm.m7.live_gate.diagnostic.v1',
  status,
  kind,
  detail,
  issue: 511,
  base_url: baseUrl || null,
  profile,
  session_id: null,
  exit_code: Number(exitCode || 0),
  timestamp: new Date().toISOString(),
}, null, 2) + '\n');
NODE
}

if [[ "$LIST_ONLY" == false ]]; then
  if [[ -z "$BASE_URL" ]]; then
    fail "missing --base-url (or OCTOS_TEST_URL)"
    exit 2
  fi
  if [[ -z "$AUTH_TOKEN" ]]; then
    fail "missing --auth-token (or OCTOS_AUTH_TOKEN)"
    exit 2
  fi
fi

BASE_URL="${BASE_URL%/}"
if [[ "$BASE_URL" == *"dspfac.ocean.ominix.io"* ]]; then
  write_diagnostic "failed" "disallowed_host" "M7.8 live swarm gate refuses to run against mini5 / dspfac.ocean.ominix.io" 3
  fail "refusing mini5 / dspfac.ocean.ominix.io; use a canary such as dspfac.crew.ominix.io"
  exit 3
fi

if [[ ! -d "${ROOT}/e2e/node_modules/@playwright/test" ]]; then
  log "installing e2e npm dependencies"
  (cd "$ROOT/e2e" && npm ci)
fi

export OCTOS_TEST_URL="$BASE_URL"
export OCTOS_AUTH_TOKEN="$AUTH_TOKEN"
export OCTOS_PROFILE="$PROFILE_ID"
export OCTOS_TEST_EMAIL="$TEST_EMAIL"
export OCTOS_M7_SWARM_OUTPUT_DIR="$OUTPUT_DIR"
export OCTOS_M7_SWARM_DIAGNOSTICS="$DIAGNOSTIC_JSON"
export PLAYWRIGHT_OUTPUT_DIR="${OUTPUT_DIR}/playwright"

if [[ "$LIST_ONLY" == true ]]; then
  log "listing M7.8 live swarm gate tests"
  (cd "$ROOT/e2e" && npx playwright test --list tests/swarm-dispatch-gate.spec.ts)
  pass "listed M7.8 Playwright tests"
  exit 0
fi

export OCTOS_M7_SWARM_LIVE=1

PLAYWRIGHT_CMD=(npx playwright test --workers=1 tests/swarm-dispatch-gate.spec.ts --reporter=line)
if [[ "$HEADED" == true ]]; then
  PLAYWRIGHT_CMD+=(--headed)
fi
PLAYWRIGHT_CMD+=("${PLAYWRIGHT_ARGS[@]}")

log "running M7.8 live swarm gate against ${BASE_URL}"
set +e
(cd "$ROOT/e2e" && "${PLAYWRIGHT_CMD[@]}")
rc=$?
set -e

if [[ "$rc" -ne 0 ]]; then
  if [[ ! -f "$DIAGNOSTIC_JSON" ]]; then
    write_diagnostic "failed" "playwright_failed" "Playwright M7.8 swarm gate failed before writing a specific diagnostic" "$rc"
  fi
  fail "M7.8 live swarm gate failed; diagnostic: $DIAGNOSTIC_JSON"
  exit "$rc"
fi

if [[ ! -f "$DIAGNOSTIC_JSON" ]]; then
  write_diagnostic "passed" "m7_swarm_gate_passed" "M7.8 live swarm gate passed" 0
fi

pass "M7.8 live swarm gate passed"
pass "diagnostic: $DIAGNOSTIC_JSON"
