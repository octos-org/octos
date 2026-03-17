# Adaptive Router — Test Architecture

> Fully automated test suite for `octos-llm` adaptive routing, failover, hedging, and QoS.

## Overview

```
                          ┌─────────────────────────┐
                          │     Test Pyramid         │
                          ├─────────────────────────┤
                          │  UX Integration Tests    │  ← Real LLM APIs (manual, #[ignore])
                          │  (ux_adaptive.rs)        │
                          ├─────────────────────────┤
                          │  Tool Call Integration   │  ← Multi-provider tool calling (#[ignore])
                          │  (tool_call_conversation)│
                          ├─────────────────────────┤
                          │  Unit Tests (adaptive)   │  ← MockProvider, deterministic, fast
                          │  Unit Tests (responsive) │
                          │  Unit Tests (config)     │
                          │  Unit Tests (provider)   │
                          └─────────────────────────┘
```

| Layer | Files | Tests | Requires API Keys | Runs in CI |
|-------|-------|------:|:-----------------:|:----------:|
| Unit — Adaptive Router | `src/adaptive.rs` | 23 | No | Yes |
| Unit — Responsiveness | `src/responsiveness.rs` | 8 | No | Yes |
| Unit — ChatConfig | `src/config.rs` | 9 | No | Yes |
| Unit — Provider utils | `src/provider.rs` | 5 | No | Yes |
| Integration — UX Adaptive | `tests/ux_adaptive.rs` | 8 | Yes | No |
| Integration — Tool Calls | `tests/tool_call_conversation.rs` | 19 | Yes | No |
| **Total** | | **72** | | |

---

## 1. Unit Tests — AdaptiveRouter (`src/adaptive.rs`)

### Mock Infrastructure

All unit tests use a `MockProvider` that implements `LlmProvider`:

```rust
struct MockProvider {
    name: &'static str,     // provider_name() return value
    model: &'static str,    // model_id() return value
    latency_ms: u64,        // simulated response time
    fail: bool,             // if true, returns Err
    error_msg: &'static str,// error message prefix
}
```

Key design decisions:
- **Fixed latencies** (not random) — tests are deterministic
- **`probe_probability: 0.0`** in most tests — disables stochastic probing
- **Each test creates its own router** — no shared state between tests

### Test Matrix

#### 1.1 Basic Routing (Cold Start)

| Test | Setup | Assertion |
|------|-------|-----------|
| `test_selects_primary_on_cold_start` | 2 providers, no prior calls | First call returns `"from-primary"` (priority 0 wins) |
| `test_scoring_cold_start_respects_priority` | 2 providers, sync scoring | `score(slot[0]) < score(slot[1])` — lower priority = lower score = preferred |
| `test_empty_router_panics` | 0 providers | `#[should_panic(expected = "at least one provider")]` |

**What this validates**: On first call with no metrics data, the router selects providers by insertion order (priority). This is the foundation — all other modes build on top of this default behavior.

#### 1.2 Failover & Circuit Breaker

```
                Normal Flow              Failover Flow
                ┌─────────┐              ┌─────────┐
  request ────► │ Primary │─── ok ──►    │ Primary │─── err ──┐
                └─────────┘              └─────────┘          │
                                         ┌─────────┐          │
                                         │Fallback │◄─────────┘
                                         └────┬────┘
                                              │ ok ──►
```

| Test | Setup | Behavior Verified |
|------|-------|-------------------|
| `test_failover_on_error` | Primary `fail=true` | Primary fails → automatic fallover → `"from-fallback"` |
| `test_circuit_breaker_skips_degraded` | `failure_threshold=1` | After 1 failure, circuit opens. Next call **skips** primary entirely |
| `test_all_providers_fail` | Both providers `fail=true` | Returns `Err` — no silent swallowing of errors |
| `test_lane_changing_off_skips_circuit_broken` | Off mode + `failure_threshold=1` | Even in Off mode (no lane changing), circuit-broken providers are bypassed |

**What this validates**: The circuit breaker is the safety net. After `failure_threshold` consecutive failures, a provider is marked "open" (degraded). Subsequent calls skip it entirely — no wasted latency on known-broken providers.

#### 1.3 Hedge Mode (Parallel Racing)

```
                    Hedge Mode
                ┌─────────────┐
  request ────► │  select_for │
                │   _hedge()  │
                └──────┬──────┘
                       │
              ┌────────┴────────┐
              ▼                 ▼
        ┌──────────┐     ┌──────────┐
        │ Primary  │     │Alternate │  (different provider_name, cheapest)
        │  200ms   │     │   10ms   │
        └────┬─────┘     └────┬─────┘
             │                │
             │   select!      │ ◄── first to complete wins
             │   {            │
             └───────┬────────┘
                     ▼
              winner response
```

| Test | Setup | Behavior Verified |
|------|-------|-------------------|
| `test_hedged_racing_picks_faster_provider` | Primary 200ms, Fallback 10ms | Gets fast response, total time <150ms (not 200ms) |
| `test_hedged_racing_survives_one_failure` | Primary `fail=true`, Fallback 10ms | Failing racer doesn't kill the race — fallback still wins |
| `test_hedged_off_uses_single_provider` | Hedge=Off, Primary 50ms, Fallback 1ms | No racing — always uses primary (priority order) |
| `test_hedge_single_provider_falls_through` | 1 provider only, Hedge=On | No alternate available → graceful fallback to single-provider path |
| `should_skip_hedge_when_all_providers_same_name` | 2x "moonshot" providers | Won't race a provider against itself (same API backend) |
| `should_hedge_with_different_provider_names` | 2x moonshot + 1x deepseek | Picks deepseek as alternate (different name), deepseek wins at 10ms |

**What this validates**: Hedge mode fires two requests in parallel using `tokio::select!`. The first response wins. This cuts tail latency — if one provider is slow or times out, the other covers. The test also ensures we don't waste money racing the same backend against itself.

#### 1.4 Lane Mode (Dynamic Provider Switching)

```
                    Lane Mode

  Warm-up (Off mode, 5 calls):
    Primary ████████████ 50ms avg
    Fallback (cold, no data)

  Switch to Lane mode:
    score(primary)  = 0.3*(50/100) + 0 + 0.2*(0/2) = 0.15 + 0.10 = 0.25
    score(fallback) = cold_start     = 0.2*(1/2)    = 0.10
                                                       ^^^^ lower = better
    Lane picks: fallback ✓
```

| Test | Setup | Behavior Verified |
|------|-------|-------------------|
| `test_lane_changing_off_uses_priority_order` | Off mode, Primary 50ms, Fallback 1ms | After 5 warm-up calls showing primary is slow, Off mode **still** uses primary |
| `test_lane_mode_picks_best_by_score` | Same setup, then switch to Lane | Lane mode switches to faster fallback based on score |

**What this validates**: Lane mode uses the scoring formula to dynamically route to the best provider. Off mode is the control — it always uses priority order regardless of performance metrics.

#### 1.5 Scoring Formula

```
score = weight_latency    * norm_latency      (default 0.3)
      + weight_error_rate * error_rate         (default 0.3)
      + weight_priority   * norm_priority      (default 0.2)
      + weight_cost       * norm_cost          (default 0.2)
```

Where:
- `norm_latency = latency_ema_ms / latency_threshold_ms` (capped at 1.0)
- `error_rate = failures / total_calls`
- `norm_priority = slot_index / num_slots`
- `norm_cost = slot_cost_per_m / max_cost_across_slots`

On **cold start** (no calls yet): `score = weight_priority * norm_priority + weight_cost * norm_cost`

Lower score = better provider.

#### 1.6 Late Failure Reporting

| Test | Setup | Behavior Verified |
|------|-------|-------------------|
| `should_record_failure_on_report_late_failure` | `failure_threshold=2` | `report_late_failure()` increments consecutive failures. After 2 calls, circuit opens |
| `should_failover_after_late_failure_opens_circuit` | `failure_threshold=1` | After 1 late failure, next `chat()` routes to fallback |

**What this validates**: Sometimes a response arrives but is later determined to be bad (e.g., truncated, malformed). `report_late_failure()` lets callers retroactively penalize a provider, eventually tripping the circuit breaker.

#### 1.7 Metrics & Observability

| Test | What it checks |
|------|---------------|
| `test_metrics_snapshot` | After 1 call: `success_count=1`, `failure_count=0`, `latency_ema_ms > 0` |
| `test_metrics_export_after_calls` | After 3 calls in Off mode: primary has `success_count=3`, fallback has `0` |
| `test_latency_samples_p95` | Ring buffer of 64 slots, pushes 1-100, p95 ≈ 95-97 |
| `test_adaptive_status_reports_correctly` | `adaptive_status()` reflects mode and provider count |
| `test_qos_ranking_toggle` | QoS ranking flag is independent of routing mode |

#### 1.8 Runtime Controls

| Test | Behavior Verified |
|------|-------------------|
| `test_mode_switch_at_runtime` | `set_mode()` cycles Off → Hedge → Lane → Off atomically |
| `test_qos_ranking_toggle` | `set_qos_ranking()` toggleable independently of mode |

---

## 2. Unit Tests — ResponsivenessObserver (`src/responsiveness.rs`)

The ResponsivenessObserver detects when a provider is degrading (slow responses) and triggers hedge/lane activation.

```
  State Machine:

  [Inactive] ──── 3 consecutive slow ────► [Active]
       ▲                                       │
       └──── latency returns to normal ────────┘

  Thresholds:
  - Baseline: rolling average of last 20 samples (needs ≥5 to learn)
  - Slow: latency > baseline × 3.0
  - Activation: 3 consecutive slow responses
  - Deactivation: 1 normal response while active
```

| Test | Scenario | Assertion |
|------|----------|-----------|
| `baseline_after_5_samples` | Record 5 × 100ms | `baseline() = Some(100ms)` |
| `detect_degradation` | 5 × 100ms (baseline), then 3 × 400ms | `should_activate() = true` |
| `detect_recovery` | Degrade → activate → record 100ms | `should_deactivate() = true` |
| `no_false_positive_before_baseline` | Only 2 samples (100ms + 10000ms) | `should_activate() = false` (need ≥5 for baseline) |
| `window_capped_at_20` | Record 30 values | `sample_count() = 20` |
| `multiple_activation_cycles` | baseline → degrade → activate → recover → deactivate → degrade → activate | Full state machine cycle works repeatedly |
| `boundary_exactly_at_threshold` | 5 × 100ms, then 300ms (= 100 × 3.0 exactly) | `should_activate() = false` (must be **strictly** greater) |
| `sample_count_tracking` | Record 3 values | `sample_count() = 3` |

---

## 3. Unit Tests — ChatConfig (`src/config.rs`)

| Test | What it validates |
|------|-------------------|
| `defaults` | `ChatConfig::default()` has expected values (temperature=0.0, tool_choice=Auto) |
| `tool_choice_default` | `ToolChoice::default() = Auto` |
| `serde_roundtrip` | ChatConfig → JSON → ChatConfig preserves all fields |
| `skip_serializing_none` | None fields omitted from JSON output |
| `tool_choice_specific_serde` | `Specific { name: "search" }` serializes correctly |
| `tool_choice_none_serde` | `ToolChoice::None` roundtrips |
| `response_format_json_object` | `type = "json_object"` serialization |
| `response_format_json_schema` | Schema name, JSON schema, strict flag all preserved |
| `response_format_skip_none` | `response_format: None` omitted from JSON |

---

## 4. Unit Tests — Provider Utilities (`src/provider.rs`)

| Test | What it validates |
|------|-------------------|
| `truncate_short_error` | Body < 200 chars: unchanged |
| `truncate_exact_200` | Body = 200 chars: unchanged |
| `truncate_long_error` | Body = 500 chars: truncated to 200 + `"... (500 bytes total)"` |
| `truncate_empty` | Empty string stays empty |
| `http_client_build` | `build_http_client(30, 10)` succeeds without panic |

---

## 5. Integration Tests — UX Adaptive (`tests/ux_adaptive.rs`)

### Provider Setup

```rust
fn kimi() -> Arc<dyn LlmProvider> {
    // KIMI_API_KEY → OpenAIProvider("kimi-k2.5", base_url="https://api.moonshot.ai/v1")
}
fn deepseek() -> Arc<dyn LlmProvider> {
    // DEEPSEEK_API_KEY → OpenAIProvider("deepseek-chat", base_url="https://api.deepseek.com/v1")
}
```

All tests are `#[ignore]` — run manually:
```bash
cargo test -p octos-llm --test ux_adaptive -- --ignored --nocapture
```

### Test Scenarios

#### 5.1 Single Provider Smoke

| Test | Provider | Query | Assertion |
|------|----------|-------|-----------|
| `test_kimi_responds` | Kimi K2.5 | "What is 7*8?" | Response contains "56" |
| `test_deepseek_responds` | DeepSeek Chat | "Capital of France?" | Response contains "paris" |

Validates: Basic connectivity, correct response parsing, token usage tracking, latency measurement.

#### 5.2 Hedge Mode (Real Network)

| Test | What it does |
|------|-------------|
| `test_hedge_mode_races_two_providers` | Races Kimi vs DeepSeek on "Tallest mountain?" — verifies response contains "everest", checks `adaptive_status()` |
| `test_hedge_mode_3_queries_builds_metrics` | 3 arithmetic queries, validates `metrics_snapshots()` shows `success_count > 0` for at least one provider |

Validates: Real-world hedging latency, metrics accumulation across multiple calls.

#### 5.3 Lane Mode (Real Network)

| Test | What it does |
|------|-------------|
| `test_lane_mode_selects_best_provider` | 3 sequential queries in Lane mode, prints per-provider metrics (success count, latency EMA) |

Validates: Lane mode convergence on real providers with real latency variance.

#### 5.4 Failover

| Test | What it does |
|------|-------------|
| `test_failover_from_broken_to_working` | Broken provider (invalid API key "sk-INVALID") + working DeepSeek, `failure_threshold=1` |

Assertions:
- Response contains "10" (from DeepSeek answering "5+5?")
- Broken provider has `failure_count > 0` in metrics

Validates: Real HTTP error handling (401 from Moonshot), automatic failover to next provider.

#### 5.5 Multi-Turn Context

```
  Turn 1: "Remember: secret code is BLUE42. Acknowledge briefly."
  Turn 2: [history + turn 1 response] + "What was the secret code?"
  Assert: Response contains "blue42" or "blue 42"
```

Validates: Conversation history is correctly passed through the provider, multi-turn context is preserved.

#### 5.6 ResponsivenessObserver (Real Latency)

| Test | What it does |
|------|-------------|
| `test_responsiveness_baseline_learning` | 6 queries to DeepSeek, each latency recorded to observer |

Assertions:
- `baseline().is_some()` after 6 samples
- `should_activate() = false` (normal latencies don't trigger degradation)
- `sample_count() = 6`

---

## 6. Integration Tests — Tool Call Conversation (`tests/tool_call_conversation.rs`)

### Tool Definitions

8 tools with realistic JSON schemas:

| Tool | Parameters | Purpose |
|------|-----------|---------|
| `get_weather` | location, unit (enum), include_forecast (bool) | Simple enum + boolean params |
| `search_web` | query, num_results, language | String + integer params |
| `execute_code` | language (enum), code, timeout | Code execution simulation |
| `read_file` | path, encoding (optional), line_range (optional object) | Nested optional objects |
| `write_file` | path, content, create_dirs (bool) | File write simulation |
| `create_task` | title, description, priority (enum), tags (array), assignee (optional) | Arrays + optional strings |
| `send_message` | recipient, subject, body, cc (array, optional) | Optional arrays |
| `database_query` | query, database, parameters (array of objects) | Nested array of objects |

### Multi-Turn Conversation Flow

```
  User: "I need help with a few things..."
    ↓
  Assistant: [tool_calls: get_weather, search_web, ...]
    ↓
  Tool Results: [weather JSON, search JSON, ...]
    ↓
  Assistant: "Based on the results..." [may call more tools]
    ↓
  Tool Results: [...]
    ↓
  Assistant: Final synthesized response
```

### Provider Coverage

| Provider | Models Tested | API Type |
|----------|--------------|----------|
| DashScope | qwen3-coder-flash, qwen3.5-plus | OpenAI-compatible |
| OpenAI | gpt-4o, gpt-4o-mini | Native OpenAI |
| DeepSeek | deepseek-chat | OpenAI-compatible |
| Kimi/Moonshot | kimi-k2.5 | OpenAI-compatible |
| MiniMax | MiniMax-M1, MiniMax-M2.5 | OpenAI-compatible |
| NVIDIA NIM | deepseek-v3.2, qwen3.5-397b, glm5, kimi-k2.5, minimax-m2.5 | OpenAI-compatible |
| Z.AI | glm-4.7 | Anthropic-compatible |
| Google Gemini | gemini-2.5-flash, gemini-3-flash-preview, gemini-3.1-pro-preview | Native Gemini |
| Local OminiX | qwen3.5-27b | OpenAI-compatible |

### Aggregate Test

`test_all_providers_tool_call_conversation` runs all available providers in parallel using `tokio::JoinSet`, collecting:
- Pass/fail status
- Latency per provider
- Token usage (input/output)
- Error messages on failure

Output:
```
=== Multi-Provider Tool Call Results ===
  ✓ dashscope/qwen3-coder-flash  (2.3s, 450in/380out)
  ✓ openai/gpt-4o                (1.8s, 520in/290out)
  ✗ nvidia/glm5                  Error: 500 Internal Server Error
=== 12/14 passed ===
```

### AdaptiveRouter Integration

`test_adaptive_router_with_real_providers`:
- Picks 2+ available providers (checks for API key env vars)
- Creates `AdaptiveRouter` with real providers
- Runs 5 requests through the router
- Validates `metrics_snapshots()` shows latency EMA, p95, success/failure counts

---

## 7. How to Run

```bash
# All unit tests (fast, no API keys needed)
cargo test -p octos-llm

# Only adaptive router unit tests
cargo test -p octos-llm -- adaptive::tests

# UX integration tests (requires KIMI_API_KEY + DEEPSEEK_API_KEY)
cargo test -p octos-llm --test ux_adaptive -- --ignored --nocapture

# Tool call integration tests (requires various API keys)
cargo test -p octos-llm --test tool_call_conversation -- --ignored --nocapture

# Single specific test
cargo test -p octos-llm -- test_hedged_racing_picks_faster_provider

# With debug logging
RUST_LOG=debug cargo test -p octos-llm -- --nocapture
```

---

## 8. Determinism Strategy

Unit tests are designed to be **100% deterministic**:

| Technique | Why |
|-----------|-----|
| Fixed `latency_ms` on MockProvider | No randomness in timing |
| `probe_probability: 0.0` | Disables stochastic probing that randomly tests degraded providers |
| Independent router per test | No state leakage between tests |
| Atomic counters with Relaxed ordering | Lock-free, no ordering-dependent races |
| `tokio::time::sleep` for latency simulation | Consistent, controllable delays |

---

## 9. Coverage Gaps

| Area | Status | Notes |
|------|--------|-------|
| Streaming responses (`chat_stream`) | Not unit-tested | Only tested via integration tests with real providers |
| Probe logic (stochastic recovery) | Not directly tested | Disabled in unit tests for determinism |
| Cost-aware scoring | Not yet tested | New feature — `cost_per_m` in scoring formula needs dedicated tests |
| Concurrent request stress | Not tested | Atomics are correct by design but no load test |
| Timeout behavior | Not tested | No test for provider timeout → failover |
| Status callback firing | Not tested | `set_status_callback()` effects not asserted |

---

## 10. Architecture Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│                        AdaptiveRouter                            │
│                                                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                      │
│  │  Slot 0  │  │  Slot 1  │  │  Slot N  │   (priority order)   │
│  │ provider │  │ provider │  │ provider │                      │
│  │ metrics  │  │ metrics  │  │ metrics  │                      │
│  │ priority │  │ priority │  │ priority │                      │
│  │ cost_per_m│  │ cost_per_m│  │ cost_per_m│                      │
│  └──────────┘  └──────────┘  └──────────┘                      │
│       │              │              │                            │
│       ▼              ▼              ▼                            │
│  ┌────────────────────────────────────┐                         │
│  │         Routing Decision           │                         │
│  │                                    │                         │
│  │  Off:   priority order             │                         │
│  │  Lane:  best score()               │                         │
│  │  Hedge: race primary vs cheapest   │                         │
│  └────────────────────────────────────┘                         │
│       │                                                          │
│       ▼                                                          │
│  ┌────────────────────────────────────┐                         │
│  │       Circuit Breaker Check        │                         │
│  │  consecutive_failures >= threshold │                         │
│  │  → skip provider, try next         │                         │
│  └────────────────────────────────────┘                         │
│       │                                                          │
│       ▼                                                          │
│  ┌────────────────────────────────────┐                         │
│  │        Metrics Recording           │                         │
│  │  latency_ema, p95, success/fail    │                         │
│  │  consecutive_failures tracking     │                         │
│  └────────────────────────────────────┘                         │
│       │                                                          │
│       ▼                                                          │
│  ┌────────────────────────────────────┐                         │
│  │     ResponsivenessObserver         │                         │
│  │  baseline learning (20-sample)     │                         │
│  │  degradation detection (3× slow)   │                         │
│  │  recovery detection                │                         │
│  └────────────────────────────────────┘                         │
└──────────────────────────────────────────────────────────────────┘
```
