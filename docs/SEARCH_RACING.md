# Search Provider Racing — Improvement Plan

> Status: Planned. Low effort (~2-3 hours). Not blocking any current feature.

## Current Behavior

Search providers are called **sequentially** with early return on first success:

```
DDG → Exa → Brave → You.com → Perplexity
 ↑ free, tried first              ↑ paid, last resort
```

Each provider blocks until response/timeout before trying the next. If DDG is slow or returns no results, total latency stacks up.

## Proposed: Hedged Racing

Race the top 2 available providers concurrently. First good result wins, loser is cancelled.

```rust
// Race DDG (free) + best available paid provider
tokio::select! {
    r = ddg_search() => if r.is_good() { return r; },
    r = exa_search() => if r.is_good() { return r; },
}
// If both fail, fall through to remaining providers sequentially
```

### Why DDG + 1 Paid

- DDG is free and unlimited — racing it costs nothing
- If DDG wins (common case), no paid API quota burned
- If DDG is slow/empty, paid provider result arrives without waiting for DDG timeout

## Impact on Deep Research

Deep research pipelines already get implicit multi-provider coverage:

```
dynamic_parallel (6 workers, concurrent)
  Worker 1 → DDG ✓ (fast)
  Worker 2 → DDG ✗ → Exa ✓ (fallback)
  Worker 3 → DDG ✓
  ...
```

With racing, each worker would be faster on DDG failures (no sequential wait), but the overall improvement is marginal since most DDG calls succeed. Main win is **tail latency reduction** for the occasional slow/failed DDG call.

## Implementation Plan

| Task | File | Effort |
|------|------|--------|
| Add `tokio::select!` race logic | `crates/octos-agent/src/tools/web_search.rs` | 1 hr |
| Handle edge cases (both fail, partial results) | Same file | 0.5 hr |
| Config flag `search_racing: bool` (default: false) | `crates/octos-cli/src/config.rs` | 0.5 hr |
| Unit tests with mock providers | `crates/octos-agent/src/tools/web_search.rs` (#[cfg(test)]) | 0.5 hr |

## Trade-offs

| Pro | Con |
|-----|-----|
| Lower tail latency on DDG failures | 2x API calls when racing paid providers |
| More resilient search | Slightly more complex code |
| Pattern already proven in LLM `AdaptiveProvider` | Marginal gain for deep research (parallelism already at worker level) |

## Config Example

```json
{
  "search": {
    "racing": true,
    "race_providers": ["ddg", "exa"]
  }
}
```

## Reference

- LLM hedged racing: `crates/octos-llm/src/adaptive.rs` (lines 649-689)
- Current search failover: `crates/octos-agent/src/tools/web_search.rs` (lines 170-237)
- Deep search parallel fetch: `crates/octos-agent/src/tools/deep_search.rs` (line 133, `join_all`)
