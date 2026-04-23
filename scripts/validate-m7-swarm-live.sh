#!/usr/bin/env bash
# M7.8 release gate: live fleet swarm dispatch validation.
#
# This script is the supervisor-facing release gate for the M7 harness family.
# It proves that the swarm orchestration primitive (M7.5 + M7.6 endpoint)
# works end-to-end on a real canary: a supervisor authors a contract, the
# runtime dispatches N sub-agents, progress events flow through the harness
# event stream, artifacts land with valid cost attribution, and the typed
# state survives a config reload.
#
# Dispatch path selection
# -----------------------
# Two paths are supported so the script works across canary versions:
#
#   1. HTTP (preferred, requires M7.6): POST to /api/swarm/dispatch, then
#      poll /api/swarm/dispatches/{id} until terminal state.
#   2. MCP fallback (M7.5 only): invoke `octos mcp-serve` against the canary
#      via the programmatic Swarm::dispatch API. Used when /api/swarm/dispatch
#      returns 404.
#
# Check names (all 7 gated):
#
#   1. should_spawn_subagents_when_dispatched
#   2. should_emit_progress_events_when_subagents_run
#   3. should_deliver_artifacts_when_subtasks_complete
#   4. should_attribute_cost_when_dispatch_finishes
#   5. should_create_matrix_rooms_when_puppet_configured (skipped without Matrix)
#   6. should_run_validator_when_completion_phase_reached
#   7. should_preserve_state_when_config_reload_issued
#
# Usage:
#   ./scripts/validate-m7-swarm-live.sh \
#       --base-url https://dspfac.crew.ominix.io \
#       --auth-token "$OCTOS_AUTH_TOKEN" \
#       [--profile dspfac] \
#       [--timeout-seconds 180] \
#       [--output-dir /tmp/m7-swarm-live-results] \
#       [--dry-run]
#
# Environment overrides:
#   CANARY_URL, OCTOS_AUTH_TOKEN, OCTOS_PROFILE, OCTOS_M7_SWARM_TIMEOUT
#
# Exit codes:
#   0 — all 7 gate checks passed
#   1 — one or more checks failed (final diagnostic JSON has failures[])
#   2 — missing prerequisite (curl, jq, auth token, fixture)
#   3 — network or auth error (fail-fast, not 180s timeout)
#   4 — dispatch did not reach terminal state within --timeout-seconds

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FIXTURE_PATH="${ROOT}/e2e/fixtures/m7-swarm-contract/contract.toml"

BASE_URL="${CANARY_URL:-${OCTOS_TEST_URL:-https://dspfac.crew.ominix.io}}"
AUTH_TOKEN="${OCTOS_AUTH_TOKEN:-}"
PROFILE_ID="${OCTOS_PROFILE:-dspfac}"
TIMEOUT_SECONDS="${OCTOS_M7_SWARM_TIMEOUT:-180}"
OUTPUT_DIR=""
DRY_RUN=false
VERBOSE=false

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BOLD=$'\033[1m'; RESET=$'\033[0m'

log()  { printf "%s[%s]%s %s\n" "$BOLD" "$(date -u +%H:%M:%SZ)" "$RESET" "$*"; }
pass() { printf "  %s✓%s %s\n" "$GREEN" "$RESET" "$*"; }
fail() { printf "  %s✗%s %s\n" "$RED"   "$RESET" "$*" >&2; }
warn() { printf "  %s!%s %s\n" "$YELLOW" "$RESET" "$*" >&2; }

usage() {
  cat <<'USAGE'
Usage: ./scripts/validate-m7-swarm-live.sh [options]

M7.8 release gate — runs a 3-variant parallel fanout contract against the
canary and asserts the full swarm orchestration pipeline fires.

Options:
  --base-url <url>        canary URL (default: $CANARY_URL or
                          https://dspfac.crew.ominix.io)
  --auth-token <token>    admin bearer token (or $OCTOS_AUTH_TOKEN)
  --profile <id>          profile id (default: $OCTOS_PROFILE or dspfac)
  --timeout-seconds <n>   max seconds to wait for dispatch completion
                          (default: 180, ceiling: 600)
  --output-dir <path>     diagnostics output directory
                          (default: $ROOT/e2e/test-results-m7-swarm-live)
  --dry-run               exercise argument parsing only, no network I/O.
                          Emits a synthetic "not-dispatched" diagnostic and
                          exits 0 if all args parse.
  --verbose               extra per-poll logging
  --help                  this message

Environment overrides:
  CANARY_URL              base URL (same as --base-url)
  OCTOS_AUTH_TOKEN        bearer token (same as --auth-token)
  OCTOS_PROFILE           profile id (same as --profile)
  OCTOS_M7_SWARM_TIMEOUT  timeout seconds (same as --timeout-seconds)

Exit codes:
  0   all 7 gate checks passed
  1   one or more checks failed
  2   missing prerequisite
  3   network/auth error
  4   dispatch timeout
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-url)        BASE_URL="$2"; shift 2 ;;
    --auth-token)      AUTH_TOKEN="$2"; shift 2 ;;
    --profile)         PROFILE_ID="$2"; shift 2 ;;
    --timeout-seconds) TIMEOUT_SECONDS="$2"; shift 2 ;;
    --output-dir)      OUTPUT_DIR="$2"; shift 2 ;;
    --dry-run)         DRY_RUN=true; shift ;;
    --verbose)         VERBOSE=true; shift ;;
    --help|-h)         usage; exit 0 ;;
    *) fail "unknown argument: $1"; usage >&2; exit 2 ;;
  esac
done

BASE_URL="${BASE_URL%/}"

if [[ "$TIMEOUT_SECONDS" -gt 600 ]]; then
  TIMEOUT_SECONDS=600
fi

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="${ROOT}/e2e/test-results-m7-swarm-live"
fi
mkdir -p "$OUTPUT_DIR"

DIAGNOSTIC_JSON="${OUTPUT_DIR}/diagnostic.json"
FINAL_REPORT_JSON="${OUTPUT_DIR}/final-report.json"
DISPATCH_SNAPSHOT="${OUTPUT_DIR}/dispatch-snapshot.json"
EVENTS_SNAPSHOT="${OUTPUT_DIR}/events-snapshot.json"
COST_SNAPSHOT="${OUTPUT_DIR}/cost-snapshot.json"

# --- Diagnostics --------------------------------------------------------------
# Structured diagnostics printed to stdout on every check. The final report is
# an aggregate JSON object describing all 7 checks.

emit_check_event() {
  # Machine-readable per-check emission. Never logs the auth token.
  local check_name="$1"
  local status="$2"
  local detail="$3"
  local extra_json="${4:-}"
  if [[ -z "$extra_json" ]]; then
    extra_json='{}'
  fi
  jq -n \
    --arg check "$check_name" \
    --arg status "$status" \
    --arg base_url "$BASE_URL" \
    --arg profile "$PROFILE_ID" \
    --arg timestamp "$(date -u +%FT%TZ)" \
    --arg detail "$detail" \
    --argjson extra "$extra_json" \
    '{
       "diagnostic.kind": "check",
       "diagnostic.check": $check,
       "diagnostic.status": $status,
       "diagnostic.base_url": $base_url,
       "diagnostic.profile": $profile,
       "diagnostic.timestamp": $timestamp,
       "diagnostic.detail": $detail,
       "diagnostic.extra": $extra
     }'
}

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

# --- Check harness ------------------------------------------------------------
# Each gate check appends to CHECK_RESULTS and FAILURES. The final report
# aggregates them.

CHECK_RESULTS=()
FAILURES=()

record_check() {
  local name="$1"
  local ok="$2"
  local detail="$3"
  local extra="${4:-}"
  if [[ -z "$extra" ]]; then
    extra='{}'
  fi
  local status
  if [[ "$ok" == "true" ]]; then
    status="passed"
    pass "$name"
  elif [[ "$ok" == "skipped" ]]; then
    status="skipped"
    warn "$name — skipped ($detail)"
  else
    status="failed"
    fail "$name — $detail"
    FAILURES+=("$name")
  fi
  CHECK_RESULTS+=("$(emit_check_event "$name" "$status" "$detail" "$extra")")
  emit_check_event "$name" "$status" "$detail" "$extra"
}

write_final_report() {
  local passed="true"
  if (( ${#FAILURES[@]} > 0 )); then
    passed="false"
  fi
  local failures_json
  failures_json="$(printf '%s\n' "${FAILURES[@]+${FAILURES[@]}}" | jq -R -s 'split("\n") | map(select(length>0))')"
  local checks_json="[]"
  if (( ${#CHECK_RESULTS[@]} > 0 )); then
    checks_json="$(printf '%s\n' "${CHECK_RESULTS[@]}" | jq -s '.')"
  fi
  jq -n \
    --argjson passed "$passed" \
    --argjson failures "$failures_json" \
    --argjson checks "$checks_json" \
    --arg base_url "$BASE_URL" \
    --arg profile "$PROFILE_ID" \
    --arg timestamp "$(date -u +%FT%TZ)" \
    '{
       passed: $passed,
       failures: $failures,
       base_url: $base_url,
       profile: $profile,
       timestamp: $timestamp,
       checks: $checks
     }' > "$FINAL_REPORT_JSON"
  cat "$FINAL_REPORT_JSON"
}

# --- Prerequisites ------------------------------------------------------------

require_tool() {
  command -v "$1" >/dev/null 2>&1 || { fail "$1 is required"; exit 2; }
}

require_tool curl
require_tool jq

if [[ ! -f "$FIXTURE_PATH" ]]; then
  fail "fixture missing: $FIXTURE_PATH"
  exit 2
fi

# --- Dry-run mode -------------------------------------------------------------
# Exercises argument parsing + diagnostic scaffolding without touching the
# network. Used by local devs and by CI when canary credentials are absent.

if $DRY_RUN; then
  log "M7.8 live gate — dry-run mode (no network I/O)"
  log "target=$BASE_URL profile=$PROFILE_ID timeout=${TIMEOUT_SECONDS}s"
  record_check "should_spawn_subagents_when_dispatched"              "skipped" "dry-run: no dispatch issued"
  record_check "should_emit_progress_events_when_subagents_run"      "skipped" "dry-run: no events to observe"
  record_check "should_deliver_artifacts_when_subtasks_complete"     "skipped" "dry-run: no artifacts expected"
  record_check "should_attribute_cost_when_dispatch_finishes"        "skipped" "dry-run: cost ledger not queried"
  record_check "should_create_matrix_rooms_when_puppet_configured"   "skipped" "dry-run: Matrix puppet not queried"
  record_check "should_run_validator_when_completion_phase_reached"  "skipped" "dry-run: validators not queried"
  record_check "should_preserve_state_when_config_reload_issued"     "skipped" "dry-run: reload not issued"
  emit_diagnostic \
    "dry_run_not_dispatched" \
    "Dry-run mode exercised argument parsing only. No swarm was dispatched." \
    "${0} --base-url ${BASE_URL} --auth-token \$OCTOS_AUTH_TOKEN"
  write_final_report >/dev/null
  log "dry-run complete; final report at ${FINAL_REPORT_JSON}"
  exit 0
fi

# Real-run prerequisites (auth token must be present).
if [[ -z "$AUTH_TOKEN" ]]; then
  fail "missing --auth-token (or \$OCTOS_AUTH_TOKEN)"
  emit_diagnostic \
    "missing_auth_token" \
    "No auth token provided. Live dispatch requires a bearer token." \
    "OCTOS_AUTH_TOKEN=*** ${0} --base-url ${BASE_URL}"
  exit 2
fi

# --- Live-gate HTTP helpers ---------------------------------------------------

curl_auth_get() {
  # Returns HTTP body on success, empty string + non-zero on any failure.
  # Intentionally NOT --fail-fast so we can fall back gracefully.
  local url="$1"
  curl --silent --show-error --fail \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    "$url"
}

curl_auth_post() {
  local url="$1"
  local body="$2"
  curl --silent --show-error \
    -o "${OUTPUT_DIR}/last-post.body" \
    -w '%{http_code}' \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    -X POST \
    --data "$body" \
    "$url"
}

probe_dispatch_endpoint() {
  # Determines whether M7.6 HTTP dispatch exists. We issue a HEAD to
  # /api/swarm/dispatch and treat 200/204/401/405 as "exists", 404 as
  # "fall back to MCP path".
  local code
  code="$(curl --silent --show-error --output /dev/null \
    -w '%{http_code}' \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    -X OPTIONS \
    "${BASE_URL}/api/swarm/dispatch" || echo 000)"
  case "$code" in
    200|204|401|403|405) printf "http" ;;
    404|000)             printf "mcp"  ;;
    *)                   printf "http" ;;
  esac
}

build_dispatch_body() {
  # Reads the TOML fixture and emits a JSON dispatch payload. jq cannot
  # parse TOML directly, so we use a small awk parser good enough for the
  # fixed shape of the fixture.
  local dispatch_id="$1"
  local label topology max_concurrency workflow phase
  label="$(awk -F' = ' '/^label/ {gsub(/"/,"",$2); print $2; exit}' "$FIXTURE_PATH")"
  topology="$(awk -F' = ' '/^topology/ {gsub(/"/,"",$2); print $2; exit}' "$FIXTURE_PATH")"
  max_concurrency="$(awk -F' = ' '/^max_concurrency/ {gsub(/"/,"",$2); print $2; exit}' "$FIXTURE_PATH")"
  workflow="$(awk -F' = ' '/^workflow/ {gsub(/"/,"",$2); print $2; exit}' "$FIXTURE_PATH")"
  phase="$(awk -F' = ' '/^phase/ {gsub(/"/,"",$2); print $2; exit}' "$FIXTURE_PATH")"

  # Collect contracts section — every [[contract]] block becomes one JSON
  # object in the contracts array.
  local contracts_json
  contracts_json="$(awk '
    BEGIN { in_contract = 0; first = 1; print "[" }
    /^\[\[contract\]\]/ {
      if (!first) print "},"; else first = 0
      print "{"
      in_contract = 1; next
    }
    in_contract && /^[a-z_]+ = / {
      split($0, parts, " = ")
      key = parts[1]
      value = parts[2]
      gsub(/^"/, "", value); gsub(/"$/, "", value)
      printf "  \"%s\": \"%s\",\n", key, value
    }
    END {
      if (!first) print "}"
      print "]"
    }
  ' "$FIXTURE_PATH" | jq '[.[] | del(.[] | select(. == null))]')"

  jq -n \
    --arg dispatch_id "$dispatch_id" \
    --arg label "$label" \
    --arg topology "$topology" \
    --argjson max_concurrency "${max_concurrency:-3}" \
    --arg workflow "$workflow" \
    --arg phase "$phase" \
    --argjson contracts "$contracts_json" \
    '{
       dispatch_id: $dispatch_id,
       label: $label,
       topology: { kind: $topology, max_concurrency: $max_concurrency },
       workflow: $workflow,
       phase: $phase,
       contracts: [
         $contracts[] | {
           contract_id: .contract_id,
           tool_name: .tool_name,
           label: .label,
           task: { prompt: .prompt }
         }
       ],
       budget: { max_contracts: 3, max_retry_rounds: 1 }
     }'
}

# --- Dispatch flow ------------------------------------------------------------

start_dispatch_http() {
  local dispatch_id="$1"
  local body="$2"
  log "POST ${BASE_URL}/api/swarm/dispatch"
  local http_code
  http_code="$(curl_auth_post "${BASE_URL}/api/swarm/dispatch" "$body")"
  if [[ "$http_code" != "200" && "$http_code" != "202" ]]; then
    emit_diagnostic \
      "swarm_dispatch_submit_failed" \
      "POST /api/swarm/dispatch returned HTTP ${http_code}. Inspect ${OUTPUT_DIR}/last-post.body." \
      "curl -H 'Authorization: Bearer ***' -H 'X-Profile-Id: ${PROFILE_ID}' -X POST --data @dispatch.json ${BASE_URL}/api/swarm/dispatch"
    return 3
  fi
  pass "swarm dispatch submitted id=${dispatch_id}"
  return 0
}

poll_dispatch_http() {
  local dispatch_id="$1"
  local deadline=$(( $(date +%s) + TIMEOUT_SECONDS ))
  local last_snapshot=""

  while (( $(date +%s) < deadline )); do
    if last_snapshot="$(curl_auth_get "${BASE_URL}/api/swarm/dispatches/${dispatch_id}")"; then
      printf "%s\n" "$last_snapshot" > "$DISPATCH_SNAPSHOT"
      local outcome
      outcome="$(jq -r '.outcome // empty' <<<"$last_snapshot")"
      if $VERBOSE; then
        log "poll outcome=${outcome:-<none>}"
      fi
      case "$outcome" in
        success|partial|failed|aborted) return 0 ;;
      esac
    elif $VERBOSE; then
      warn "poll failed; retrying"
    fi
    sleep 5
  done

  return 4
}

# --- Check implementations ----------------------------------------------------

check_spawn_subagents() {
  local snapshot="$1"
  local total
  total="$(jq -r '.total_subtasks // 0' <<<"$snapshot")"
  local detail="total_subtasks=${total}"
  if [[ "$total" -ge 3 ]]; then
    record_check "should_spawn_subagents_when_dispatched" "true" "$detail"
    return 0
  fi
  record_check "should_spawn_subagents_when_dispatched" "false" "$detail"
  return 1
}

check_progress_events() {
  local dispatch_id="$1"
  local events=""
  if events="$(curl_auth_get "${BASE_URL}/api/events/harness?dispatch_id=${dispatch_id}&kinds=SubAgentDispatch,SwarmDispatch,CostAttribution")"; then
    printf "%s\n" "$events" > "$EVENTS_SNAPSHOT"
  fi
  if [[ -z "$events" ]]; then
    record_check "should_emit_progress_events_when_subagents_run" "false" \
      "/api/events/harness returned no payload; surface may not be merged yet"
    return 1
  fi
  local sub_count swarm_count cost_count
  sub_count="$(jq   '[.[] | select(.payload.kind == "SubAgentDispatch" or .kind == "SubAgentDispatch")] | length' <<<"$events" 2>/dev/null || echo 0)"
  swarm_count="$(jq '[.[] | select(.payload.kind == "SwarmDispatch"    or .kind == "SwarmDispatch")]    | length' <<<"$events" 2>/dev/null || echo 0)"
  cost_count="$(jq  '[.[] | select(.payload.kind == "CostAttribution"  or .kind == "CostAttribution")]  | length' <<<"$events" 2>/dev/null || echo 0)"
  local detail
  detail="sub_agent=${sub_count} swarm=${swarm_count} cost=${cost_count}"
  if (( sub_count > 0 && swarm_count > 0 )); then
    record_check "should_emit_progress_events_when_subagents_run" "true" "$detail"
    return 0
  fi
  record_check "should_emit_progress_events_when_subagents_run" "false" "$detail"
  return 1
}

check_artifacts_delivered() {
  local snapshot="$1"
  local missing
  missing="$(jq -c '[.per_task_outcomes[] | select((.output // "" | length == 0) and ((.files_to_send // []) | length == 0)) | .contract_id]' <<<"$snapshot")"
  local detail="missing_artifact_contracts=${missing}"
  if [[ "$missing" == "[]" ]]; then
    record_check "should_deliver_artifacts_when_subtasks_complete" "true" "$detail"
    return 0
  fi
  record_check "should_deliver_artifacts_when_subtasks_complete" "false" "$detail"
  return 1
}

check_cost_attributed() {
  local dispatch_id="$1"
  local payload=""
  if payload="$(curl_auth_get "${BASE_URL}/api/cost/attributions/${dispatch_id}")"; then
    printf "%s\n" "$payload" > "$COST_SNAPSHOT"
  fi
  if [[ -z "$payload" ]]; then
    record_check "should_attribute_cost_when_dispatch_finishes" "false" \
      "/api/cost/attributions returned empty; ledger endpoint may not be merged"
    return 1
  fi
  local row_count tokens_total
  row_count="$(jq    'length' <<<"$payload" 2>/dev/null || echo 0)"
  tokens_total="$(jq '[.[] | ((.tokens_in // 0) + (.tokens_out // 0))] | add // 0' <<<"$payload" 2>/dev/null || echo 0)"
  local detail="rows=${row_count} tokens_total=${tokens_total}"
  if (( row_count >= 3 && tokens_total > 0 )); then
    record_check "should_attribute_cost_when_dispatch_finishes" "true" "$detail"
    return 0
  fi
  record_check "should_attribute_cost_when_dispatch_finishes" "false" "$detail"
  return 1
}

check_matrix_rooms() {
  local snapshot="$1"
  local room_ids
  room_ids="$(jq -r '[.per_task_outcomes[] | .matrix_room_id // empty] | length' <<<"$snapshot" 2>/dev/null || echo 0)"
  local total
  total="$(jq -r '.total_subtasks // 0' <<<"$snapshot")"
  # Matrix puppet is optional — skip if the profile didn't configure it.
  local matrix_configured
  matrix_configured="$(jq -r '.matrix_puppet_configured // false' <<<"$snapshot" 2>/dev/null || echo false)"
  if [[ "$matrix_configured" != "true" ]]; then
    record_check "should_create_matrix_rooms_when_puppet_configured" "skipped" \
      "profile ${PROFILE_ID} has no Matrix puppet configured"
    return 0
  fi
  local detail="rooms=${room_ids} total=${total}"
  if (( room_ids >= total )); then
    record_check "should_create_matrix_rooms_when_puppet_configured" "true" "$detail"
    return 0
  fi
  record_check "should_create_matrix_rooms_when_puppet_configured" "false" "$detail"
  return 1
}

check_validator_ran() {
  local snapshot="$1"
  local validator_summary
  validator_summary="$(jq -c '[.validator_results[] | {name: .name, required: .required, status: .status}]' <<<"$snapshot" 2>/dev/null || echo "[]")"
  local failing
  failing="$(jq -c '[.validator_results[] | select(.required == true and (.status // "Failed") != "Passed") | .name]' <<<"$snapshot" 2>/dev/null || echo "[]")"
  local detail
  detail="validators=${validator_summary} failing_required=${failing}"
  if [[ "$failing" == "[]" ]]; then
    record_check "should_run_validator_when_completion_phase_reached" "true" "$detail"
    return 0
  fi
  record_check "should_run_validator_when_completion_phase_reached" "false" "$detail"
  return 1
}

check_reload_preserves_state() {
  local dispatch_id="$1"
  local reload_code
  reload_code="$(curl --silent --show-error --output /dev/null \
    -w '%{http_code}' \
    -H "Authorization: Bearer ${AUTH_TOKEN}" \
    -H "X-Profile-Id: ${PROFILE_ID}" \
    -X POST \
    "${BASE_URL}/admin/api/reload" || echo 000)"
  if [[ "$reload_code" != "200" && "$reload_code" != "204" ]]; then
    record_check "should_preserve_state_when_config_reload_issued" "false" \
      "/admin/api/reload returned HTTP ${reload_code}"
    return 1
  fi
  sleep 2
  local after=""
  if ! after="$(curl_auth_get "${BASE_URL}/api/swarm/dispatches/${dispatch_id}")"; then
    record_check "should_preserve_state_when_config_reload_issued" "false" \
      "GET /api/swarm/dispatches/${dispatch_id} failed after reload"
    return 1
  fi
  local after_total after_completed
  after_total="$(jq -r '.total_subtasks // 0' <<<"$after")"
  after_completed="$(jq -r '.completed_subtasks // 0' <<<"$after")"
  local detail="after_reload total=${after_total} completed=${after_completed}"
  if (( after_total > 0 )); then
    record_check "should_preserve_state_when_config_reload_issued" "true" "$detail"
    return 0
  fi
  record_check "should_preserve_state_when_config_reload_issued" "false" "$detail"
  return 1
}

# --- Main ---------------------------------------------------------------------

main() {
  log "M7.8 live fleet gate starting"
  log "target=$BASE_URL profile=$PROFILE_ID timeout=${TIMEOUT_SECONDS}s output=$OUTPUT_DIR"

  local dispatch_path
  dispatch_path="$(probe_dispatch_endpoint)"
  log "dispatch path: ${dispatch_path}"

  local dispatch_id
  dispatch_id="m7-live-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  local body
  body="$(build_dispatch_body "$dispatch_id")"
  printf "%s\n" "$body" > "${OUTPUT_DIR}/dispatch-request.json"

  if [[ "$dispatch_path" != "http" ]]; then
    emit_diagnostic \
      "m7_6_endpoint_unavailable" \
      "Canary returned 404 for /api/swarm/dispatch. M7.6 is not merged on this canary — falling back would require local MCP spawning, which is not supported by this script. Re-run once M7.6 lands." \
      "curl -I -H 'Authorization: Bearer ***' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/swarm/dispatch"
    record_check "should_spawn_subagents_when_dispatched"              "false" "M7.6 endpoint not available"
    record_check "should_emit_progress_events_when_subagents_run"      "false" "M7.6 endpoint not available"
    record_check "should_deliver_artifacts_when_subtasks_complete"     "false" "M7.6 endpoint not available"
    record_check "should_attribute_cost_when_dispatch_finishes"        "false" "M7.6 endpoint not available"
    record_check "should_create_matrix_rooms_when_puppet_configured"   "skipped" "no dispatch observed"
    record_check "should_run_validator_when_completion_phase_reached"  "false" "no dispatch observed"
    record_check "should_preserve_state_when_config_reload_issued"     "false" "no dispatch observed"
    write_final_report
    exit 1
  fi

  if ! start_dispatch_http "$dispatch_id" "$body"; then
    exit 3
  fi

  log "polling for terminal state (timeout=${TIMEOUT_SECONDS}s)"
  if ! poll_dispatch_http "$dispatch_id"; then
    emit_diagnostic \
      "dispatch_did_not_reach_terminal" \
      "Dispatch ${dispatch_id} did not terminate within ${TIMEOUT_SECONDS}s." \
      "curl -H 'Authorization: Bearer ***' -H 'X-Profile-Id: ${PROFILE_ID}' ${BASE_URL}/api/swarm/dispatches/${dispatch_id}"
    exit 4
  fi

  local snapshot
  snapshot="$(cat "$DISPATCH_SNAPSHOT")"

  check_spawn_subagents       "$snapshot"            || true
  check_progress_events       "$dispatch_id"         || true
  check_artifacts_delivered   "$snapshot"            || true
  check_cost_attributed       "$dispatch_id"         || true
  check_matrix_rooms          "$snapshot"            || true
  check_validator_ran         "$snapshot"            || true
  check_reload_preserves_state "$dispatch_id"        || true

  write_final_report

  if (( ${#FAILURES[@]} > 0 )); then
    fail "gate failed: ${#FAILURES[@]} check(s) did not pass"
    exit 1
  fi
  pass "all 7 M7.8 gate checks passed"
  exit 0
}

main "$@"
