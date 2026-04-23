#!/usr/bin/env bash
# M4.1A release gate: live progress validation on the mini fleet.
#
# This script is the supervisor-facing release gate for issue #474. It proves
# that the full structured progress pipeline works end-to-end on a real
# canary: deep-research emits octos.harness.event.v1 events, the runtime sink
# folds them into durable task_status, the UI replays them through chat, and
# /api/sessions/:id/tasks + the SSE event stream both expose the same truth.
#
# The script is idempotent — two back-to-back runs produce identical
# decisions. It exits 0 when all assertions pass, or a non-zero code with a
# structured diagnostic on any failure (see DIAGNOSTIC.* entries).
#
# Usage:
#   ./scripts/validate-m4-1a-live.sh \
#       --base-url https://dspfac.crew.ominix.io \
#       --auth-token octos-admin-2026 \
#       [--profile dspfac] \
#       [--test-email dspfac@gmail.com] \
#       [--skip-e2e] \
#       [--e2e-only] \
#       [--timeout-seconds 600] \
#       [--output-dir /tmp/m4-1a-live-results]
#
# Environment overrides mirror the CLI flags:
#   OCTOS_TEST_URL, OCTOS_AUTH_TOKEN, OCTOS_PROFILE, OCTOS_TEST_EMAIL,
#   OCTOS_E2E_OUTPUT_ROOT, OCTOS_M4_1A_TIMEOUT

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURE_PATH="${ROOT}/e2e/fixtures/m4-1a-progress-expected.json"

BASE_URL="${OCTOS_TEST_URL:-}"
AUTH_TOKEN="${OCTOS_AUTH_TOKEN:-}"
PROFILE_ID="${OCTOS_PROFILE:-dspfac}"
TEST_EMAIL="${OCTOS_TEST_EMAIL:-dspfac@gmail.com}"
TIMEOUT_SECONDS="${OCTOS_M4_1A_TIMEOUT:-600}"
OUTPUT_DIR=""
SKIP_E2E=false
E2E_ONLY=false
VERBOSE=false

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BOLD=$'\033[1m'; RESET=$'\033[0m'

log() { printf "%s[%s]%s %s\n" "$BOLD" "$(date -u +%H:%M:%SZ)" "$RESET" "$*"; }
pass() { printf "  %s✓%s %s\n" "$GREEN" "$RESET" "$*"; }
fail() { printf "  %s✗%s %s\n" "$RED" "$RESET" "$*" >&2; }
warn() { printf "  %s!%s %s\n" "$YELLOW" "$RESET" "$*" >&2; }

usage() {
  cat <<'USAGE'
Usage: ./scripts/validate-m4-1a-live.sh --base-url <url> --auth-token <token> [options]

Required arguments:
  --base-url <url>        canary URL (e.g. https://dspfac.crew.ominix.io)
  --auth-token <token>    admin auth token used for API and Playwright runs

Optional arguments:
  --profile <id>          profile id (default: dspfac)
  --test-email <email>    login email used by Playwright helpers
  --timeout-seconds <n>   max seconds per deep-research observation
                          (default: 600, hard ceiling: 600)
  --output-dir <path>     write diagnostics and Playwright output here
  --skip-e2e              skip the Playwright spec portion
  --e2e-only              run ONLY the Playwright spec (no API probes)
  --verbose               emit extra diagnostics

Exit codes:
  0   all assertions passed
  1   generic failure (API probe or Playwright)
  2   missing prerequisite (curl, jq, base-url, auth-token)
  3   assertion failure with structured DIAGNOSTIC output
  4   timeout reached before the deep-research run reported ready
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-url) BASE_URL="$2"; shift 2 ;;
    --auth-token) AUTH_TOKEN="$2"; shift 2 ;;
    --profile) PROFILE_ID="$2"; shift 2 ;;
    --test-email) TEST_EMAIL="$2"; shift 2 ;;
    --timeout-seconds) TIMEOUT_SECONDS="$2"; shift 2 ;;
    --output-dir) OUTPUT_DIR="$2"; shift 2 ;;
    --skip-e2e) SKIP_E2E=true; shift ;;
    --e2e-only) E2E_ONLY=true; shift ;;
    --verbose) VERBOSE=true; shift ;;
    --help|-h) usage; exit 0 ;;
    *) fail "unknown argument: $1"; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$BASE_URL" ]]; then
  fail "missing --base-url (or OCTOS_TEST_URL)"
  exit 2
fi
if [[ -z "$AUTH_TOKEN" ]]; then
  fail "missing --auth-token (or OCTOS_AUTH_TOKEN)"
  exit 2
fi

# Hard-cap the observation window at 10 minutes per deep-research run.
if [[ "$TIMEOUT_SECONDS" -gt 600 ]]; then
  TIMEOUT_SECONDS=600
fi

command -v curl >/dev/null 2>&1 || { fail "curl is required"; exit 2; }
command -v jq   >/dev/null 2>&1 || { fail "jq is required"; exit 2; }

if [[ ! -f "$FIXTURE_PATH" ]]; then
  fail "fixture missing: $FIXTURE_PATH"
  exit 2
fi

# Strip a trailing slash so curl URLs compose cleanly.
BASE_URL="${BASE_URL%/}"

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="${OCTOS_E2E_OUTPUT_ROOT:-${ROOT}/e2e/test-results-m4-1a-live}"
fi
mkdir -p "$OUTPUT_DIR"

DIAGNOSTIC_JSON="${OUTPUT_DIR}/diagnostic.json"
LAST_API_SNAPSHOT="${OUTPUT_DIR}/last-api-snapshot.json"
LAST_SSE_SNAPSHOT="${OUTPUT_DIR}/last-sse-snapshot.ndjson"

# Fixture-driven constants (kept in sync with e2e/fixtures/m4-1a-progress-expected.json).
REQUIRED_PHASES="$(jq -r '.phase_order[]' "$FIXTURE_PATH")"
TERMINAL_STATES="$(jq -r '.terminal_states[]' "$FIXTURE_PATH")"
ACTIVE_STATES="$(jq -r '.active_states[]' "$FIXTURE_PATH")"
LIFECYCLE_LADDER="$(jq -r '.lifecycle_states[]' "$FIXTURE_PATH")"
DEEP_RESEARCH_PROMPT="$(jq -r '.prompts.deep_research' "$FIXTURE_PATH")"
POLL_INTERVAL="$(jq -r '.limits.poll_interval_seconds' "$FIXTURE_PATH")"
MIN_EVENTS="$(jq -r '.limits.min_progress_events' "$FIXTURE_PATH")"
MAX_DUP_SESSIONS="$(jq -r '.limits.max_duplicate_sessions' "$FIXTURE_PATH")"

emit_diagnostic() {
  local kind="$1"
  local detail="$2"
  local curl_hint="${3:-}"
  jq -n \
    --arg kind "$kind" \
    --arg base_url "$BASE_URL" \
    --arg profile "$PROFILE_ID" \
    --arg detail "$detail" \
    --arg curl_hint "$curl_hint" \
    --arg timestamp "$(date -u +%FT%TZ)" \
    '{
       "diagnostic.kind": $kind,
       "diagnostic.base_url": $base_url,
       "diagnostic.profile": $profile,
       "diagnostic.detail": $detail,
       "diagnostic.curl_hint": $curl_hint,
       "diagnostic.timestamp": $timestamp
     }' > "$DIAGNOSTIC_JSON"
  printf "\n%sDIAGNOSTIC%s\n" "$BOLD" "$RESET" >&2
  cat "$DIAGNOSTIC_JSON" >&2
  printf "\n" >&2
}

curl_auth() {
  curl --silent --show-error --fail \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    "$@"
}

probe_health() {
  log "probing $BASE_URL for /api/sessions availability"
  local resp
  if ! resp="$(curl_auth "${BASE_URL}/api/sessions")"; then
    emit_diagnostic \
      "unreachable_base_url" \
      "Failed to read /api/sessions; canary may be offline or auth token invalid." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' ${BASE_URL}/api/sessions"
    exit 3
  fi
  if ! jq -e . >/dev/null 2>&1 <<<"$resp"; then
    emit_diagnostic \
      "invalid_sessions_response" \
      "/api/sessions did not return JSON. Response: ${resp:0:256}" \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' ${BASE_URL}/api/sessions"
    exit 3
  fi
  pass "/api/sessions reachable (${#resp} bytes)"
}

start_deep_research() {
  # The backend expects a simple chat POST with the profile+auth headers.
  # We intentionally submit a fresh session label so the script is idempotent:
  # two back-to-back runs create sibling sessions and do not reuse state.
  local session_id="m4-1a-live-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  log "submitting deep research prompt to session $session_id"
  local body
  body="$(jq -n \
    --arg id "$session_id" \
    --arg msg "$DEEP_RESEARCH_PROMPT" \
    '{session_id: $id, message: $msg, stream: false}')"

  # Two possible endpoints exist across canary versions; prefer /api/chat.
  local http_code
  http_code="$(curl --silent --show-error --output "${OUTPUT_DIR}/submit.out" \
    -o "${OUTPUT_DIR}/submit.body" \
    -w '%{http_code}' \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    -X POST \
    --data "$body" \
    "${BASE_URL}/api/chat" || echo 000)"

  if [[ "$http_code" != "200" && "$http_code" != "202" ]]; then
    # Fall back to /api/sessions/:id/messages.
    http_code="$(curl --silent --show-error --output "${OUTPUT_DIR}/submit.out" \
      -o "${OUTPUT_DIR}/submit.body" \
      -w '%{http_code}' \
      -H 'Content-Type: application/json' \
      -H "Authorization: Bearer ${AUTH_TOKEN}" \
      -H "X-Profile-Id: ${PROFILE_ID}" \
      -X POST \
      --data "$(jq -n --arg msg "$DEEP_RESEARCH_PROMPT" '{content: $msg}')" \
      "${BASE_URL}/api/sessions/${session_id}/messages" || echo 000)"
  fi

  if [[ "$http_code" != "200" && "$http_code" != "202" ]]; then
    emit_diagnostic \
      "deep_research_submit_failed" \
      "POST to /api/chat and /api/sessions/${session_id}/messages both returned ${http_code}." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' -H 'Content-Type: application/json' -X POST --data '$(jq -rc . <<<"$body")' ${BASE_URL}/api/chat"
    exit 3
  fi

  pass "deep research submitted, session=${session_id}"
  printf "%s" "$session_id"
}

fetch_session_tasks() {
  local session_id="$1"
  local url="${BASE_URL}/api/sessions/${session_id}/tasks"
  local resp
  if ! resp="$(curl_auth "$url")"; then
    warn "failed to read tasks for $session_id"
    echo "[]"
    return 0
  fi
  if ! jq -e 'type == "array"' >/dev/null 2>&1 <<<"$resp"; then
    echo "[]"
    return 0
  fi
  echo "$resp"
}

is_active_task() {
  local task_json="$1"
  local lifecycle
  lifecycle="$(jq -r '.lifecycle_state // empty' <<<"$task_json" | tr '[:upper:]' '[:lower:]')"
  while IFS= read -r state; do
    [[ -z "$state" ]] && continue
    if [[ "$lifecycle" == "$state" ]]; then
      return 0
    fi
  done <<<"$ACTIVE_STATES"
  return 1
}

is_terminal_task() {
  local task_json="$1"
  local lifecycle
  lifecycle="$(jq -r '.lifecycle_state // empty' <<<"$task_json" | tr '[:upper:]' '[:lower:]')"
  while IFS= read -r state; do
    [[ -z "$state" ]] && continue
    if [[ "$lifecycle" == "$state" ]]; then
      return 0
    fi
  done <<<"$TERMINAL_STATES"
  return 1
}

is_deep_research_task() {
  local task_json="$1"
  local workflow
  workflow="$(jq -r '(.workflow_kind // .runtime_detail.workflow_kind // .tool_name // .label // "")' <<<"$task_json" | tr '[:upper:]' '[:lower:]')"
  case "$workflow" in
    *deep_research*|*"deep research"*|*deep_search*) return 0 ;;
    *) return 1 ;;
  esac
}

extract_phase() {
  local task_json="$1"
  jq -r '(.current_phase // .runtime_detail.current_phase // .runtime_detail.phase // "")' <<<"$task_json"
}

extract_progress() {
  local task_json="$1"
  jq -r '(.runtime_detail.progress // .progress // empty)' <<<"$task_json"
}

extract_lifecycle() {
  local task_json="$1"
  jq -r '.lifecycle_state // empty' <<<"$task_json" | tr '[:upper:]' '[:lower:]'
}

ladder_index() {
  local needle="$1"
  local ladder="$2"
  local idx=0
  while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    if [[ "$entry" == "$needle" ]]; then
      printf "%s" "$idx"
      return 0
    fi
    idx=$((idx + 1))
  done <<<"$ladder"
  printf "%s" "-1"
}

observe_until_terminal() {
  local session_id="$1"
  local deadline=$(( $(date +%s) + TIMEOUT_SECONDS ))
  local phase_sequence=()
  local lifecycle_sequence=()
  local progress_samples=()
  local sample_count=0
  local last_snapshot=""
  local last_phase=""
  local last_lifecycle=""
  local terminal_reached=0

  while (( $(date +%s) < deadline )); do
    last_snapshot="$(fetch_session_tasks "$session_id")"
    printf "%s\n" "$last_snapshot" > "$LAST_API_SNAPSHOT"

    local deep_task
    deep_task="$(jq -c '.[] | select((.workflow_kind // .runtime_detail.workflow_kind // .tool_name // .label // "" | ascii_downcase | test("deep_research|deep search|deep_search")))' <<<"$last_snapshot" | head -n1)"

    if [[ -n "$deep_task" ]]; then
      sample_count=$((sample_count + 1))
      local phase lifecycle progress
      phase="$(extract_phase "$deep_task")"
      lifecycle="$(extract_lifecycle "$deep_task")"
      progress="$(extract_progress "$deep_task")"

      if [[ -n "$phase" && "$phase" != "$last_phase" ]]; then
        phase_sequence+=("$phase")
        last_phase="$phase"
      fi
      if [[ -n "$lifecycle" && "$lifecycle" != "$last_lifecycle" ]]; then
        lifecycle_sequence+=("$lifecycle")
        last_lifecycle="$lifecycle"
      fi
      if [[ -n "$progress" && "$progress" != "null" ]]; then
        progress_samples+=("$progress")
      fi

      if $VERBOSE; then
        log "poll #$sample_count phase=$phase lifecycle=$lifecycle progress=$progress"
      fi

      if is_terminal_task "$deep_task"; then
        terminal_reached=1
        break
      fi
    elif $VERBOSE; then
      log "poll #$((sample_count + 1)) no deep_research task yet"
    fi

    sleep "$POLL_INTERVAL"
  done

  # Emit observations as JSON for downstream assertions.
  jq -n \
    --arg session "$session_id" \
    --arg terminal "$terminal_reached" \
    --argjson samples "$sample_count" \
    --argjson phases "$(printf '%s\n' "${phase_sequence[@]+${phase_sequence[@]}}" | jq -R -s 'split("\n") | map(select(length>0))')" \
    --argjson lifecycles "$(printf '%s\n' "${lifecycle_sequence[@]+${lifecycle_sequence[@]}}" | jq -R -s 'split("\n") | map(select(length>0))')" \
    --argjson progress "$(printf '%s\n' "${progress_samples[@]+${progress_samples[@]}}" | jq -R -s 'split("\n") | map(select(length>0)) | map(tonumber)')" \
    --argjson final_snapshot "$last_snapshot" \
    '{
       "session_id": $session,
       "terminal_reached": ($terminal == "1"),
       "sample_count": $samples,
       "phase_sequence": $phases,
       "lifecycle_sequence": $lifecycles,
       "progress_samples": $progress,
       "final_snapshot": $final_snapshot
     }'
}

capture_sse_snapshot() {
  local session_id="$1"
  local stream_url="${BASE_URL}/api/sessions/${session_id}/events/stream"
  log "capturing SSE snapshot from $stream_url"
  : > "$LAST_SSE_SNAPSHOT"
  # --max-time bounds the curl window; we intentionally error-tolerant because
  # the stream may legitimately be empty between task transitions.
  curl --silent --show-error \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    -H 'Accept: text/event-stream' \
    --max-time 20 \
    "$stream_url" > "$LAST_SSE_SNAPSHOT" 2>/dev/null || true
  pass "SSE snapshot written to $LAST_SSE_SNAPSHOT"
}

validate_observation() {
  local observation_json="$1"
  local session_id="$2"

  local terminal sample_count
  terminal="$(jq -r '.terminal_reached' <<<"$observation_json")"
  sample_count="$(jq -r '.sample_count' <<<"$observation_json")"

  if [[ "$terminal" != "true" ]]; then
    emit_diagnostic \
      "task_did_not_reach_terminal" \
      "deep_research task did not reach a terminal lifecycle state within ${TIMEOUT_SECONDS}s. sample_count=${sample_count}." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
    return 4
  fi

  if (( sample_count < MIN_EVENTS )); then
    emit_diagnostic \
      "too_few_progress_samples" \
      "expected >=${MIN_EVENTS} progress samples, observed ${sample_count}. Likely cause: task completed before the sink emitted per-phase updates, or the deep-search emitter regressed." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
    return 3
  fi

  # Required phases
  local phases_json
  phases_json="$(jq -c '.phase_sequence' <<<"$observation_json")"
  local required
  required="$(jq -r '.required_phases[] | select(.must_appear) | .phase' "$FIXTURE_PATH")"
  while IFS= read -r needed; do
    [[ -z "$needed" ]] && continue
    if ! jq -e --arg n "$needed" 'any(. == $n)' >/dev/null 2>&1 <<<"$phases_json"; then
      emit_diagnostic \
        "required_phase_missing" \
        "required phase \"${needed}\" not observed. phase_sequence=${phases_json}." \
        "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks | jq '.[].runtime_detail'"
      return 3
    fi
  done <<<"$required"
  pass "all required phases observed: $(jq -c . <<<"$phases_json")"

  # Monotonic phase order
  local prev_index=-1
  while IFS= read -r phase; do
    [[ -z "$phase" ]] && continue
    local idx
    idx="$(ladder_index "$phase" "$REQUIRED_PHASES")"
    if [[ "$idx" != "-1" ]]; then
      if (( idx < prev_index )); then
        emit_diagnostic \
          "phase_sequence_not_monotonic" \
          "phase \"${phase}\" appeared out of order (ladder expected ${REQUIRED_PHASES//$'\n'/ -> }). Observed sequence: ${phases_json}." \
          "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
        return 3
      fi
      prev_index=$idx
    fi
  done < <(jq -r '.[]' <<<"$phases_json")
  pass "phase sequence is monotonic along the canonical ladder"

  # Monotonic lifecycle transitions
  local lifecycles_json
  lifecycles_json="$(jq -c '.lifecycle_sequence' <<<"$observation_json")"
  prev_index=-1
  while IFS= read -r state; do
    [[ -z "$state" ]] && continue
    local idx
    idx="$(ladder_index "$state" "$LIFECYCLE_LADDER")"
    if [[ "$idx" != "-1" ]]; then
      if (( idx < prev_index )); then
        emit_diagnostic \
          "lifecycle_regressed" \
          "lifecycle_state \"${state}\" regressed. sequence=${lifecycles_json}. canonical ladder: ${LIFECYCLE_LADDER//$'\n'/ -> }." \
          "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
        return 3
      fi
      prev_index=$idx
    fi
  done < <(jq -r '.[]' <<<"$lifecycles_json")
  pass "lifecycle_state transitions are monotonic"

  # Progress values bounded
  local progress_json
  progress_json="$(jq -c '.progress_samples' <<<"$observation_json")"
  local bad_progress
  bad_progress="$(jq -c '[.[] | select(. < 0 or . > 1)]' <<<"$progress_json")"
  if [[ "$bad_progress" != "[]" ]]; then
    emit_diagnostic \
      "progress_out_of_range" \
      "progress values must be in [0, 1]. out_of_range=${bad_progress}. all=${progress_json}." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks | jq '.[].runtime_detail.progress'"
    return 3
  fi
  pass "progress samples stay within [0.0, 1.0]"

  # Duplicate-session guard
  local deep_tasks_count unique_count
  deep_tasks_count="$(jq '[.final_snapshot[] | select((.workflow_kind // .runtime_detail.workflow_kind // .tool_name // .label // "" | ascii_downcase | test("deep_research|deep_search")))] | length' <<<"$observation_json")"
  unique_count="$(jq '[.final_snapshot[] | select((.workflow_kind // .runtime_detail.workflow_kind // .tool_name // .label // "" | ascii_downcase | test("deep_research|deep_search"))) | (.id // .tool_call_id // .started_at)] | unique | length' <<<"$observation_json")"
  if (( deep_tasks_count - unique_count > 0 )); then
    emit_diagnostic \
      "duplicate_research_sessions" \
      "observed ${deep_tasks_count} deep_research tasks with only ${unique_count} unique ids in session ${session_id}." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
    return 3
  fi
  if (( deep_tasks_count > 1 + MAX_DUP_SESSIONS )); then
    emit_diagnostic \
      "excess_research_sessions" \
      "more deep_research tasks (${deep_tasks_count}) than allowed (${MAX_DUP_SESSIONS}+1) for a single prompt." \
      "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/tasks"
    return 3
  fi
  pass "no duplicate deep_research sessions"

  # Cross-session bleed check: pick the newest sibling session if any and
  # confirm it does not surface deep_research.
  local sibling_id
  sibling_id="$(curl_auth "${BASE_URL}/api/sessions" \
    | jq -r --arg origin "$session_id" '[.[] | select(.id != $origin)] | sort_by(.message_count) | reverse | .[0].id // empty')"
  if [[ -n "$sibling_id" ]]; then
    local sibling_tasks
    sibling_tasks="$(fetch_session_tasks "$sibling_id")"
    local bleed_count
    bleed_count="$(jq '[.[] | select((.workflow_kind // .runtime_detail.workflow_kind // .tool_name // .label // "" | ascii_downcase | test("deep_research|deep_search")))] | length' <<<"$sibling_tasks")"
    if (( bleed_count > 0 )); then
      emit_diagnostic \
        "cross_session_progress_bleed" \
        "sibling session ${sibling_id} surfaced ${bleed_count} deep_research tasks that belong to ${session_id}." \
        "curl -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${sibling_id}/tasks"
      return 3
    fi
    pass "sibling session ${sibling_id} has no deep_research bleed"
  else
    warn "no sibling sessions available for bleed check"
  fi

  return 0
}

run_e2e_specs() {
  log "running Playwright live progress gate"
  if [[ ! -d "$ROOT/e2e/node_modules/@playwright/test" ]]; then
    log "installing e2e npm dependencies"
    (cd "$ROOT/e2e" && npm ci --silent --no-audit --no-fund) \
      || { fail "npm ci failed"; exit 1; }
  fi
  local pw_output="${OUTPUT_DIR}/playwright"
  mkdir -p "$pw_output"
  if ! (
    cd "$ROOT/e2e" && \
    env \
      OCTOS_TEST_URL="$BASE_URL" \
      OCTOS_AUTH_TOKEN="$AUTH_TOKEN" \
      OCTOS_PROFILE="$PROFILE_ID" \
      OCTOS_TEST_EMAIL="$TEST_EMAIL" \
      npx playwright test tests/live-progress-gate.spec.ts \
        --reporter=line \
        --output="$pw_output"
  ); then
    emit_diagnostic \
      "playwright_failed" \
      "Playwright live-progress-gate specs failed. Inspect ${pw_output} for traces/screenshots." \
      "cd ${ROOT}/e2e && OCTOS_TEST_URL=${BASE_URL} OCTOS_AUTH_TOKEN=*** OCTOS_PROFILE=${PROFILE_ID} npx playwright test tests/live-progress-gate.spec.ts"
    exit 3
  fi
  pass "Playwright live-progress-gate passed"
}

main() {
  log "M4.1A live validation gate starting"
  log "target=$BASE_URL profile=$PROFILE_ID timeout=${TIMEOUT_SECONDS}s output=$OUTPUT_DIR"

  if ! $E2E_ONLY; then
    probe_health
    local session_id
    session_id="$(start_deep_research)"

    local observation
    observation="$(observe_until_terminal "$session_id")"
    printf "%s\n" "$observation" > "${OUTPUT_DIR}/observation.json"

    validate_observation "$observation" "$session_id"
    local rc=$?
    if (( rc != 0 )); then
      exit "$rc"
    fi

    capture_sse_snapshot "$session_id"
    if [[ -s "$LAST_SSE_SNAPSHOT" ]]; then
      pass "SSE stream produced data ($(wc -c < "$LAST_SSE_SNAPSHOT" | tr -d ' ') bytes)"
    else
      warn "SSE snapshot empty; retrying once"
      sleep "$POLL_INTERVAL"
      capture_sse_snapshot "$session_id"
      if [[ ! -s "$LAST_SSE_SNAPSHOT" ]]; then
        emit_diagnostic \
          "sse_stream_empty" \
          "SSE endpoint produced no data within 20s twice in a row." \
          "curl -N -H 'Authorization: Bearer ${AUTH_TOKEN}' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/sessions/${session_id}/events/stream"
        exit 3
      fi
    fi
  fi

  if ! $SKIP_E2E; then
    run_e2e_specs
  fi

  log "all M4.1A live gate assertions passed"
  pass "release gate green"
}

main "$@"
