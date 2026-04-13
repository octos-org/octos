# Testing Guide

## Quick Start

```bash
# Full local CI (mirrors GitHub Actions)
./scripts/ci.sh

# Fast iteration (skip clippy)
./scripts/ci.sh --quick

# Auto-fix formatting
./scripts/ci.sh --fix

# Memory-constrained machines
./scripts/ci.sh --serial
```

---

## Live Playwright On Mini 1

Use this for real deployed-server validation, not for local dev loops.

Reference host:

- web: `https://dspfac.crew.ominix.io`
- api: `https://dspfac.crew.ominix.io`

There are two separate Playwright surfaces:

1. `octos-web/tests`
   - real Chromium browser against the deployed UI
   - covers chat UX, uploads, slides, TTS, sessions, content panel, task widgets
2. `octos/e2e/tests`
   - request/API tests against the deployed backend
   - covers SSE integrity, UTF-8/CJK streaming, session persistence, file events, session APIs

### Browser Commands

Run from [/Users/yuechen/home/octos-web](/Users/yuechen/home/octos-web):

```bash
BASE_URL=https://dspfac.crew.ominix.io npx playwright test
```

Recommended split:

```bash
BASE_URL=https://dspfac.crew.ominix.io npx playwright test \
  tests/chat-fixes.spec.ts \
  tests/tts-file-delivery.spec.ts

BASE_URL=https://dspfac.crew.ominix.io npx playwright test \
  tests/adaptive-routing.spec.ts \
  tests/audio-autoplay.spec.ts \
  tests/audio-autoplay-history.spec.ts \
  tests/command-hints.spec.ts \
  tests/content-panel.spec.ts \
  tests/deep-research.spec.ts \
  tests/error-recovery.spec.ts \
  tests/file-upload.spec.ts \
  tests/queue-mode.spec.ts \
  tests/session-switching.spec.ts \
  tests/streaming-fidelity.spec.ts

BASE_URL=https://dspfac.crew.ominix.io \
API_BASE=https://dspfac.crew.ominix.io \
npx playwright test \
  tests/slides-api.spec.ts \
  tests/slides-stress.spec.ts

BASE_URL=https://dspfac.crew.ominix.io \
npx playwright test tests/slides-workflow.spec.ts
```

Critical note:

- `tests/slides-api.spec.ts` and `tests/slides-stress.spec.ts` default to `:3000` if `API_BASE` is not set
- if they fail with `ECONNREFUSED ... :3000`, that is a harness misconfiguration, not a product regression

### Backend/API Commands

Run from [/Users/yuechen/home/octos/e2e](/Users/yuechen/home/octos/e2e):

```bash
OCTOS_TEST_URL=https://dspfac.crew.ominix.io npx playwright test
```

To include privileged admin/tool regression checks:

```bash
OCTOS_TEST_URL=https://dspfac.crew.ominix.io \
OCTOS_AUTH_TOKEN=<real-admin-token> \
npx playwright test
```

Without `OCTOS_AUTH_TOKEN`, the admin shell / ffmpeg / privileged tool-chain checks are expected to skip.

### Auth Assumptions

Most browser tests already use the built-in e2e auth path from
[helpers.ts](/Users/yuechen/home/octos-web/tests/helpers.ts):

- token: `e2e-test-2026`
- profile: `dspfac`

### Failure Classification

Treat failures in these buckets:

- real regression
  - browser-facing file paths expose raw absolute server paths instead of `pf/...` or `up/...`
  - background task widgets stop reflecting real task state
  - SSE UTF-8/CJK corruption appears
  - session/file/task APIs fail consistently on the deployed host
- flaky/model-tool instability
  - upload/file-read fails once but rerun passes
  - long generation flow fails once and succeeds immediately on rerun
- harness/selector bug
  - `ECONNREFUSED ... :3000` in slides API/stress tests
  - `/help` failing because the test only waits for `assistant-message`
  - collect mode failing because the test expects multiple assistant bubbles instead of one merged response
  - `/new slides ...` failing only because no assistant bubble appears even though the session switched

On failure, inspect:

- `test-results/.../error-context.md`
- `test-results/.../test-failed-1.png`

### Current Known Weak Tests

As of April 12, 2026:

- `tests/slides-workflow.spec.ts`
  - `/help` currently renders via `cmd-feedback`, not `assistant-message`
  - `/new slides ...` currently creates/switches the session without an assistant bubble
- `tests/queue-mode.spec.ts`
  - the collect-mode assertion expects `>= 2` assistant bubbles, which conflicts with merged-response behavior
- `tests/file-upload.spec.ts`
  - CSV upload can still be intermittently flaky on rare runs

### CI Recommendation

Yes, this should be part of release validation, but not as a strict per-PR gate.

Recommended split:

- normal CI:
  - `cargo fmt`
  - `cargo clippy`
  - unit/integration tests
  - deterministic local browser tests
- post-deploy / release-candidate CI on the test server:
  - backend `octos/e2e` Playwright against Mini 1
  - stable browser `octos-web/tests` subset against Mini 1
  - quarantine or separately report known-flaky specs

Good release-gate subset:

```bash
cd /Users/yuechen/home/octos/e2e
OCTOS_TEST_URL=https://dspfac.crew.ominix.io npx playwright test

cd /Users/yuechen/home/octos-web
BASE_URL=https://dspfac.crew.ominix.io npx playwright test \
  tests/chat-fixes.spec.ts \
  tests/tts-file-delivery.spec.ts \
  tests/adaptive-routing.spec.ts \
  tests/audio-autoplay.spec.ts \
  tests/audio-autoplay-history.spec.ts \
  tests/command-hints.spec.ts \
  tests/content-panel.spec.ts \
  tests/deep-research.spec.ts \
  tests/error-recovery.spec.ts \
  tests/session-switching.spec.ts \
  tests/streaming-fidelity.spec.ts

BASE_URL=https://dspfac.crew.ominix.io \
API_BASE=https://dspfac.crew.ominix.io \
npx playwright test \
  tests/slides-api.spec.ts \
  tests/slides-stress.spec.ts
```

Do not block releases on these until fixed:

- `tests/slides-workflow.spec.ts`
- `tests/queue-mode.spec.ts`
- `tests/file-upload.spec.ts` if it remains intermittent

## CI Pipeline

`scripts/ci.sh` runs the same checks as `.github/workflows/ci.yml` plus focused subsystem tests.

### Steps

| Step | Command | Flags |
|------|---------|-------|
| 1. Format | `cargo fmt --all -- --check` | `--fix` auto-fixes |
| 2. Clippy | `cargo clippy --workspace -- -D warnings` | `--quick` skips |
| 3. Workspace tests | `cargo test --workspace` | `--serial` for single-thread |
| 4. Focused groups | Per-subsystem tests (see below) | Always runs |

### Focused Test Groups

After the full workspace run, the CI script re-runs critical subsystems individually to surface failures clearly:

| Group | Crate | Test Filter | Count | What It Covers |
|-------|-------|-------------|-------|----------------|
| Adaptive routing | `octos-llm` | `adaptive::tests` | 19 | Off/Hedge/Lane modes, circuit breaker, failover, scoring, metrics, racing |
| Responsiveness | `octos-llm` | `responsiveness::tests` | 8 | Baseline learning, degradation detection, recovery, threshold boundaries |
| Session actor | `octos-cli` | `session_actor::tests` | 9 | Queue modes, speculative overflow, auto-escalation/deescalation |
| Session persistence | `octos-bus` | `session::tests` | 28 | JSONL storage, LRU eviction, fork, rewrite, timestamp sort |

Session actor tests always run single-threaded (`--test-threads=1`) because they spawn full actors with mock providers and can OOM under parallel execution.

---

## Feature Coverage

### Adaptive Routing (`crates/octos-llm/src/adaptive.rs` — 19 tests)

Tests the `AdaptiveRouter` which manages multiple LLM providers with metrics-driven selection.

#### Off Mode (static priority)

| Test | What It Verifies |
|------|-----------------|
| `test_selects_primary_on_cold_start` | Priority order on first call (no metrics yet) |
| `test_lane_changing_off_uses_priority_order` | Off mode ignores latency differences |
| `test_lane_changing_off_skips_circuit_broken` | Off mode still respects circuit breaker |
| `test_hedged_off_uses_single_provider` | Off mode uses priority, no racing |

#### Hedge Mode (provider racing)

| Test | What It Verifies |
|------|-----------------|
| `test_hedged_racing_picks_faster_provider` | Race 2 providers via `tokio::select!`, faster wins |
| `test_hedged_racing_survives_one_failure` | Falls back to alternate when primary racer fails |
| `test_hedge_single_provider_falls_through` | Hedge with 1 provider uses single-provider path |

#### Lane Mode (score-based selection)

| Test | What It Verifies |
|------|-----------------|
| `test_lane_mode_picks_best_by_score` | Switches to faster provider after metrics warm-up |

#### Circuit Breaker and Failover

| Test | What It Verifies |
|------|-----------------|
| `test_circuit_breaker_skips_degraded` | Skips provider after N consecutive failures |
| `test_failover_on_error` | Falls over to next provider when primary fails |
| `test_all_providers_fail` | Returns error when every provider fails |

#### Scoring and Metrics

| Test | What It Verifies |
|------|-----------------|
| `test_scoring_cold_start_respects_priority` | Cold-start scores follow config priority |
| `test_latency_samples_p95` | P95 calculation from circular buffer |
| `test_metrics_snapshot` | Latency/success/failure recorded correctly |
| `test_metrics_export_after_calls` | Export includes per-provider metrics |

#### Runtime Controls

| Test | What It Verifies |
|------|-----------------|
| `test_mode_switch_at_runtime` | Off → Hedge → Lane → Off switching |
| `test_qos_ranking_toggle` | QoS ranking toggle is orthogonal to mode |
| `test_adaptive_status_reports_correctly` | Status struct reflects current mode/count |
| `test_empty_router_panics` | Asserts at least 1 provider required |

### Responsiveness Observer (`crates/octos-llm/src/responsiveness.rs` — 8 tests)

Tests the latency tracker that drives auto-escalation.

#### Baseline Learning

| Test | What It Verifies |
|------|-----------------|
| `test_baseline_learning` | Baseline established from first 5 samples |
| `test_sample_count_tracking` | `sample_count()` returns correct value |

#### Degradation Detection

| Test | What It Verifies |
|------|-----------------|
| `test_degradation_detection` | 3 consecutive slow requests (> 3x baseline) trigger activation |
| `test_at_threshold_boundary_not_triggered` | Latency exactly at threshold is not "slow" |
| `test_no_false_trigger_before_baseline` | No activation before baseline is learned |

#### Recovery and Lifecycle

| Test | What It Verifies |
|------|-----------------|
| `test_recovery_detection` | 1 fast request after activation triggers deactivation |
| `test_multiple_activation_cycles` | Activate → deactivate → reactivate works |
| `test_window_caps_at_max_size` | Rolling window stays at 20 entries |

### Queue Modes and Session Actor (`crates/octos-cli/src/session_actor.rs` — 9 tests)

Tests the per-session actor that owns message processing, queue policies, and auto-protection.

**Mock infrastructure:** `DelayedMockProvider` — configurable delay + scripted FIFO responses. `setup_speculative_actor` / `setup_actor_with_mode` — builds minimal actor with chosen queue mode and optional adaptive router.

#### Queue Mode: Followup

| Test | What It Verifies |
|------|-----------------|
| `test_queue_mode_followup_sequential` | Each message processed individually — 3 messages produce 3 responses, all appear in session history separately |

#### Queue Mode: Collect

| Test | What It Verifies |
|------|-----------------|
| `test_queue_mode_collect_batches` | Messages queued during a slow LLM call are batched into a single combined prompt (`"msg2\n---\nQueued #1: msg3"`) |

#### Queue Mode: Steer

| Test | What It Verifies |
|------|-----------------|
| `test_queue_mode_steer_keeps_newest` | Older queued messages discarded, only newest processed — discarded message absent from session history |

#### Queue Mode: Speculative

| Test | What It Verifies |
|------|-----------------|
| `test_speculative_overflow_concurrent` | Overflow spawned as full agent task during slow primary (12s > 10s patience); both responses arrive; history sorted by timestamp |
| `test_speculative_within_patience_drops` | Overflow dropped when primary within patience (5s < 10s); only 1 response arrives |
| `test_speculative_handles_background_result` | `BackgroundResult` messages handled in the speculative `select!` loop without extra LLM calls |

#### Auto-Escalation / Deescalation

| Test | What It Verifies |
|------|-----------------|
| `test_auto_escalation_on_degradation` | 5 fast warmups (baseline 100ms) → 3 slow calls (400ms > 3x) → mode switches to Hedge + Speculative, user gets notification |
| `test_auto_deescalation_on_recovery` | 1 fast response after escalation → mode reverts to Off + Followup, router confirms Off |

#### Utility

| Test | What It Verifies |
|------|-----------------|
| `test_strip_think_tags` | `<think>...</think>` block removal from LLM output |

### Session Persistence (`crates/octos-bus/src/session.rs` — 28 tests)

Tests JSONL-backed session storage with LRU caching.

#### CRUD and Persistence

| Test | What It Verifies |
|------|-----------------|
| `test_session_manager_create_and_retrieve` | Create session, add messages, retrieve |
| `test_session_manager_persistence` | Messages survive manager restart (disk reload) |
| `test_session_manager_clear` | Clear deletes from memory and disk |

#### History and Ordering

| Test | What It Verifies |
|------|-----------------|
| `test_session_get_history` | Tail-slice returns last N messages |
| `test_session_get_history_all` | Returns all when fewer than max |
| `test_sort_by_timestamp_restores_order` | Restores chronological order after concurrent overflow writes |

#### LRU Cache

| Test | What It Verifies |
|------|-----------------|
| `test_eviction_keeps_max_sessions` | Cache respects capacity limit |
| `test_evicted_session_reloads_from_disk` | Evicted sessions reload on access |
| `test_with_max_sessions_clamps_zero` | Capacity clamped to minimum 1 |

#### Concurrency

| Test | What It Verifies |
|------|-----------------|
| `test_concurrent_sessions` | Multiple sessions don't interfere |
| `test_concurrent_session_processing` | 10 parallel tasks don't corrupt sessions |

#### Fork and Rewrite

| Test | What It Verifies |
|------|-----------------|
| `test_fork_creates_child` | Fork copies last N messages with parent link |
| `test_fork_persists_to_disk` | Forked session survives restart |
| `test_session_rewrite` | Atomic write-then-rename after mutation |

#### Multi-Session (Topics)

| Test | What It Verifies |
|------|-----------------|
| `test_list_sessions_for_chat` | Lists all topic sessions for a chat |
| `test_session_topic_persists` | Topic survives restart |
| `test_update_summary` | Summary update persists |
| `test_active_session_store` | Active topic switching and go-back |
| `test_active_session_store_persistence` | Active topic survives restart |
| `test_validate_topic_name` | Rejects invalid characters and lengths |

#### Filename Encoding

| Test | What It Verifies |
|------|-----------------|
| `test_truncated_session_keys_no_collision` | Long keys with hash suffix don't collide |
| `test_decode_filename` | Percent-encoded filenames decode correctly |
| `test_list_sessions_returns_decoded_keys` | `list_sessions()` returns human-readable keys |
| `test_short_key_no_hash_suffix` | Short keys don't get hash suffix |

#### Safety Limits

| Test | What It Verifies |
|------|-----------------|
| `test_load_rejects_oversized_file` | Files over 10 MB refused |
| `test_append_respects_file_size_limit` | Append skips when file at 10 MB limit |
| `test_load_rejects_future_schema_version` | Rejects unknown schema versions |
| `test_purge_stale_sessions` | Deletes sessions older than N days |

---

## Known Gaps

| Area | Why Not Tested |
|------|---------------|
| **Interrupt queue mode** | Same codepath as Steer — covered by `test_queue_mode_steer_keeps_newest` |
| **Probe/canary requests** | Disabled in all tests via `probe_probability: 0.0` for determinism |
| **Streaming (`chat_stream`)** | No mock streaming infrastructure; streaming tested manually |
| **Session compaction** | Called in actor tests but output not verified (would need LLM mock for summarization) |
| **Live provider integration** | Requires API keys; 1 test exists but marked `#[ignore]` |
| **Channel-specific routing** | Covered by channel crate tests, not part of this subsystem |
| **⬆️ Earlier task marker** | Primary response gets "⬆️ Earlier task completed:" prefix when overflow was served; not directly asserted in tests (would need to inspect outbound content after a slow primary + fast overflow race) |
| **Overflow agent tool execution** | `serve_overflow` spawns a full `agent.process_message_tracked()` with tool access; current tests use `DelayedMockProvider` which returns canned responses without tool calls |

---

## Running Individual Tests

```bash
# Single test
cargo test -p octos-llm --lib adaptive::tests::test_hedged_racing_picks_faster_provider

# One subsystem
cargo test -p octos-llm --lib adaptive::tests

# Session actor (always single-threaded)
cargo test -p octos-cli session_actor::tests -- --test-threads=1

# With output
cargo test -p octos-cli session_actor::tests -- --test-threads=1 --nocapture
```

## GitHub Actions CI

`.github/workflows/ci.yml` runs on push/PR to `main`:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`

The local `scripts/ci.sh` is a superset — it runs the same three steps plus focused subsystem groups. If CI passes locally, it passes on GitHub.

**Runner:** `macos-14` (ARM64). Private repo with 2000 free minutes/month (10x multiplier for macOS runners = ~200 effective minutes).

---

## Files

| File | What |
|------|------|
| `scripts/ci.sh` | Local CI script (this document) |
| `scripts/pre-release.sh` | Full release smoke tests (build, E2E, skill binaries) |
| `.github/workflows/ci.yml` | GitHub Actions CI |
| `crates/octos-llm/src/adaptive.rs` | Adaptive router + 19 tests |
| `crates/octos-llm/src/responsiveness.rs` | Responsiveness observer + 8 tests |
| `crates/octos-cli/src/session_actor.rs` | Session actor + 9 tests |
| `crates/octos-bus/src/session.rs` | Session persistence + 28 tests |
