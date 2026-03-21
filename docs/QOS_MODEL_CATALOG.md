# QoS & Model Catalog Architecture

> Single source of truth for model selection, scoring, and failover across the entire system.

## Single Source of Truth: `model_catalog.json`

```
┌─────────────────────────┐     startup      ┌──────────────────┐
│ ~/.octos/                │ ───seed─────────→│  AdaptiveRouter  │
│   model_catalog.json    │                  │  (in-memory)     │
│ (system baseline:       │                  │                  │
│  benchmarks, costs,     │                  │  Live QoS:       │
│  context windows)       │                  │  throughput EMA   │
└─────────────────────────┘                  │  error rate EMA   │
                                             │  score            │
                                             └────────┬─────────┘
                                                      │ every 30s
                                                      ▼
                                             ┌──────────────────┐
                                             │ {profile_data}/  │
                                             │ model_catalog.json│ ← baseline + live QoS blended
                                             │ (single source   │
                                             │  of truth)       │
                                             └────────┬─────────┘
                                                      │ score field
                              ┌────────────────┬──────┴──────┬───────────────┐
                              ▼                ▼             ▼               ▼
                        Pipeline Guard   FallbackProvider  Dashboard    Admin API
                        (DOT model       (failover         (display)   (/model-limits)
                         assignment)      ranking)
```

### Schema

```json
{
  "updated_at": "2026-03-20T06:00:00Z",
  "models": [
    {
      "provider": "minimax/MiniMax-M2.7",
      "type": "strong",
      "stability": 1.0,
      "tool_avg_ms": 1807,
      "p95_ms": 2370,
      "score": 0.15,
      "cost_in": 0.15,
      "cost_out": 1.50,
      "ds_output": 7848,
      "context_window": 1000000,
      "max_output": 65536
    }
  ]
}
```

| Field | Description | Updated by |
|-------|-------------|------------|
| `provider` | `provider_name/model_id` | Static |
| `type` | `"strong"` or `"fast"` | Static baseline |
| `stability` | Success rate 0-1 | Blended: baseline × (1-w) + live × w |
| `tool_avg_ms` | Average latency | Blended: baseline × (1-w) + live × w |
| `p95_ms` | P95 latency | Blended: baseline × (1-w) + live × w |
| `score` | **Unified score (lower = better)** | AdaptiveRouter every 30s |
| `cost_in` | $/1M input tokens | Static |
| `cost_out` | $/1M output tokens | Static |
| `ds_output` | Benchmark output chars | Static (from synthesize test) |
| `context_window` | Max context tokens | Static |
| `max_output` | Max output tokens | Static |

### Blending Formula

Live QoS blends into baseline using EMA weight:

```
weight = min(1.0, total_calls / 10)
blended = baseline × (1 - weight) + live × weight
```

- Cold start (0 calls): 100% baseline from benchmarks
- After 10 calls: 100% live data
- Gradual transition prevents cold-start zeros from destroying benchmark data

## Scoring

### Formula

```
score = 0.35 × stability_penalty
      + 0.30 × quality_penalty
      + 0.20 × throughput_penalty
      + 0.15 × cost_penalty
```

**Lower score = better provider.**

| Factor | Weight | Metric | Why |
|--------|--------|--------|-----|
| **Stability** | 35% | `1 - blended_success_rate` | Does it complete without errors? |
| **Quality** | 30% | `1 - (ds_output × stability / max_across_providers)` | Does it produce good output? |
| **Throughput** | 20% | `1 - (tokens_per_second / max_across_providers)` | How fast per output token? Task-normalized. |
| **Cost** | 15% | `cost_per_m / max_cost_across_providers` | How cheap? |

### Why no raw latency?

Raw latency depends on task complexity — prompt length, tool count, output length. A model that takes 60s to write a 10K-char report is not "slow." It's doing more work.

**Throughput** (output tokens per second) normalizes for this. A model producing 100 tok/s is genuinely faster than 40 tok/s, regardless of absolute latency.

## Consumers

All consumers read the `score` field from `model_catalog.json`. No consumer computes its own ranking.

### 1. Pipeline Guard (DOT model assignment)

Hook binary: `pipeline-guard` runs before every `run_pipeline` tool call.

**What it does:**
- Reads `{profile_data}/pipeline_models.json` (filtered catalog)
- If missing: filters system catalog by profile's configured models, saves filtered copy
- Splits models into STRONG pool and FAST pool by `type` field
- Sorts each pool by `score` ascending (best first)
- Injects `model=` on all DOT nodes:
  - `dynamic_parallel` / search nodes → round-robin from FAST pool
  - Everything else (analyze, synthesize) → round-robin from STRONG pool
  - `planner_model` → round-robin from STRONG pool
- Random PID-based start index so concurrent pipelines get different models

**LLM writes no model attributes.** The guard injects them all.

### 2. FallbackProvider (pipeline node failover)

Wraps each pipeline node's primary provider with fallbacks.

**What it does:**
- `ProviderRouter.compatible_fallbacks(key)` returns fallback list sorted by `score`
- On primary failure: records cooldown (60s), tries next best fallback
- Cooled-down models are skipped until cooldown expires
- After 60s: model eligible again (transient errors recover)

### 3. AdaptiveRouter (session-level routing)

Handles the parent agent's LLM calls (not pipeline nodes).

**What it does:**
- Computes `score` from live metrics + baseline (the formula above)
- Writes `score` to `model_catalog.json` every 30s
- **Lane mode**: picks lowest-score provider
- **Hedge mode**: races two providers, both accumulate metrics
- **Circuit breaker**: N consecutive failures → provider excluded
- **Probe**: 10% chance to test stale providers for recovery

### 4. Dashboard / Admin API

`GET /api/admin/model-limits` returns the runtime catalog directly.

## Failure Handling

| Layer | Mechanism | Recovery |
|-------|-----------|----------|
| AdaptiveRouter (session) | Circuit breaker: N consecutive failures | Probe: 10% chance to retry stale provider |
| FallbackProvider (pipeline) | Cooldown: 60s timed exclusion | Auto-recovery after 60s |
| Pipeline Guard | No runtime tracking | Re-reads catalog on each invocation |

Both mechanisms temporarily exclude failed providers and auto-recover. Different timings because:
- Session calls are interactive → need fast recovery via probing
- Pipeline calls are batch → can wait 60s for cooldown to expire

## Files

| File | Owner | Purpose |
|------|-------|---------|
| `~/.octos/model_catalog.json` | Human / benchmark script | System baseline (all models, benchmark data) |
| `{profile_data}/model_catalog.json` | AdaptiveRouter (every 30s) | Profile runtime (baseline + live QoS blended) |
| `{profile_data}/pipeline_models.json` | Pipeline Guard | Filtered catalog (only profile's models) |

`provider_metrics.json` is **removed** — all data flows through `model_catalog.json`.

`model_limits.json` is **removed** — context windows and max output tokens live in the catalog. Runtime lookups via `context::seed_from_catalog()` and `pricing::seed_pricing_catalog()`.

## Example: 5 Concurrent Deep Research Pipelines

```
Pipeline 1: search=deepseek-chat, planner=MiniMax-M2.7, analyze=glm-5, synthesize=qwen3.5-plus
Pipeline 2: search=minimaxai/minimax-m2.5, planner=glm-5, analyze=qwen3.5-plus, synthesize=MiniMax-M2.7
Pipeline 3: search=moonshotai/kimi-k2.5, planner=qwen3.5-plus, analyze=MiniMax-M2.7, synthesize=glm-5
Pipeline 4: search=gemini-2.5-flash, planner=MiniMax-M2.7, analyze=glm-5, synthesize=qwen3.5-plus
Pipeline 5: search=gpt-4.1-mini, planner=glm-5, analyze=qwen3.5-plus, synthesize=MiniMax-M2.7
```

- 5 different FAST models for search → no single provider rate-limited
- 3 STRONG models rotate across analyze/synthesize → load distributed
- Random start index ensures different assignments per pipeline
- If MiniMax-M2.7 fails in Pipeline 1 → 60s cooldown → Pipelines 2-5 skip it for fallback
