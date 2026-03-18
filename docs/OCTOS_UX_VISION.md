# octos UX Vision

Octos's UX strategy: adopt OpenClaw's proven interaction patterns, then layer on octos's unique adaptive intelligence capabilities that OpenClaw lacks.

---

## Foundation: OpenClaw's 4 UX Principles

### 1. LLM-Instructed Background Tasks

When a user request triggers a long-running operation (deep research, multi-step tool chains, code generation), the agent spawns it as a background task and immediately returns the session to the user.

```
User: "Research the history of quantum computing and write a 5000-word report"
Agent: "Starting deep research in the background. I'll notify you when it's ready.
        You can keep chatting — ask me anything else."
        [Background task: research-quantum-computing | Status: running]
```

**Implementation (done):**
- `SpawnTool` supports `mode: "background"` — the LLM calls spawn with `mode: "background"` to run tasks without blocking the session
- Background tasks write results to session history on completion via `ActorMessage::BackgroundResult`
- User gets a notification in their channel when done (result injected as system message)

### 2. User Can Cancel

Any running operation — foreground, background, or queued — can be cancelled by the user at any time.

```
User: "stop"          // English
User: "停止"           // Chinese
User: "やめて"         // Japanese
User: "стоп"          // Russian
```

**Implementation (done):**
- `CancellationToken` propagation from session → agent run → subagents → tool calls
- 30+ multilingual abort trigger words across 9 languages (`octos-core/src/abort.rs`):
  - English: stop, abort, cancel, halt, interrupt, quit, enough
  - Chinese: 停, 停止, 取消, 停下, 别说了
  - Japanese: やめて, 止めて, ストップ
  - Russian: стоп, отмена, хватит
  - French: arrête, annuler
  - Spanish: detente, cancelar
  - Hindi: रुको, बंद करो
  - Arabic: توقف, قف
  - Korean: 멈춰, 중지
- On cancel: kills active LLM stream, abort spawned tasks, clear queue, acknowledge cancellation
- Cascade abort: parent cancellation propagates to all child subagents via `SubagentRegistry`
- Exact-match only (case-insensitive, trimmed) — avoids false positives from partial matches like "please stop talking about cats"

### 3. Queued Messages Answered Together

When the user sends multiple messages while the agent is busy, octos handles them according to the configured queue mode.

```
User: "Also check the Rust implementation"
User: "And compare performance with Go"
User: "Focus on async patterns specifically"

Agent receives (after current run completes, in collect mode):
  [Queued messages while agent was busy]
  ---
  Queued #1: Also check the Rust implementation
  ---
  Queued #2: And compare performance with Go
  ---
  Queued #3: Focus on async patterns specifically
```

**Queue modes (all implemented):**

| Mode | Behavior | When to Use |
|------|----------|-------------|
| **followup** | Process queued messages one at a time (FIFO) | Default. Safe, predictable |
| **collect** | All messages batched into one combined prompt | Rapid-fire addendums |
| **steer** | Keep only the latest message, discard older | Topic changes mid-flight |
| **interrupt** | Cancel current run, process new message immediately | Urgent corrections |
| **speculative** | Spawns primary call as tokio task; polls inbox concurrently. Overflow messages exceeding patience get immediate lightweight responses while the slow call continues | Best responsiveness |

Default: `followup`.

**Runtime switching:**
```
/queue                              → show current mode
/queue followup                     → switch to followup
/queue collect                      → switch to collect
/queue steer                        → switch to steer
/queue interrupt                    → switch to interrupt
/queue spec                         → switch to speculative (alias: speculative)
```

### 4. Streaming

Never leave the user staring at a blank screen. Output appears progressively as the LLM generates.

| Channel | Method | Throttle | Max Chars |
|---------|--------|----------|-----------|
| Discord | Edit existing message (PATCH) | 1200ms | 2000 |
| Slack | Native `chat.startStream` or edit fallback | 1000ms | 4000 |
| Telegram | Draft transport or edit message | 1000ms | 4096 |
| CLI | Direct stdout streaming | None | Unlimited |

**Implementation:**
- `EditAdapter` / `StreamingAdapter` traits from the Channel Adapter Pattern
- Throttled updates prevent API rate limits
- Delayed start (250ms debounce) avoids flicker for fast operations
- Final flush ensures complete output delivery
- `<think>` tag stripping: `strip_think_from_buffer()` in `stream_reporter.rs` removes `<think>...</think>` blocks before flushing to users (handles partial/unclosed tags during streaming)

---

## octos Differentiators

These capabilities are unique to octos and don't exist in OpenClaw.

### 5. Adaptive Routing with Exclusive Modes

octos's adaptive routing system uses an `AdaptiveMode` enum with three mutually exclusive strategies, plus an orthogonal QoS toggle. This replaces the previous independent boolean toggles.

```rust
// crates/octos-llm/src/adaptive.rs
pub enum AdaptiveMode {
    Off = 0,    // Static priority, failover only on circuit-broken
    Hedge = 1,  // Race 2 providers, take winner, cancel loser
    Lane = 2,   // Score-based single-provider selection
}
```

Stored as `AtomicU8` for lock-free runtime switching.

#### Mode: Off (Default)

Static priority order. The router tries providers in the order they were configured. Failover happens only when a provider's circuit breaker opens (N consecutive failures, default: 3).

```
Provider A (primary) → if circuit-broken → Provider B → if circuit-broken → Provider C
```

#### Mode: Hedge (Hedged Racing)

Fire each LLM request to 2 providers simultaneously. Take the winner, cancel the loser. Both record metrics on completion (loser's metrics not recorded since the future is dropped).

```
                    ┌─→ Provider A ─→ ✅ Response (winner, 1.2s)
User request ──────┤
                    └─→ Provider B ─→ 🚫 Cancelled (loser, still running at 1.2s)
```

**Implementation (`hedged_chat` in `adaptive.rs`):**
1. Primary selected by priority order (same as Off mode)
2. Alternate selected as the best non-primary, non-circuit-broken provider by score
3. `tokio::select!` races both `try_chat()` futures
4. Winner's result returned immediately; loser's future dropped (TCP connection closed at next `.await`)
5. If winner fails, loser retried sequentially as fallback
6. Cost: ~1.5× a single call (double input tokens, single output)

#### Mode: Lane (Score-Based Lane Changing)

Dynamically pick the best single provider based on a composite score:

```
score = weight_latency   × normalized_latency_ema
      + weight_error_rate × error_rate
      + weight_priority   × priority_order

Default weights: 0.4 latency, 0.4 error_rate, 0.2 priority
```

The provider with the lowest score is selected. No racing, no doubled cost. The system smoothly shifts traffic to healthier providers as metrics change.

#### QoS Ranking (Orthogonal)

An independent toggle that factors response quality into scoring decisions. When enabled, quality signals influence provider selection alongside latency/errors.

```
/adaptive qos on    → enable quality-weighted scoring
/adaptive qos off   → disable (latency + errors only)
```

#### Runtime Commands

```
/adaptive              → show current mode, current provider, per-provider metrics
/adaptive off          → static priority (failover only)
/adaptive hedge        → hedged racing (aliases: race, circuit)
/adaptive lane         → score-based lane changing
/adaptive qos on|off   → toggle QoS ranking (orthogonal to mode)
```

**Status display example:**
```
**Adaptive Routing**
  mode:        hedge
  qos ranking: on
  current:     anthropic

**Providers**
  anthropic (claude-sonnet-4-6): latency=1200ms ok=45 err=1 ✅
  openai (gpt-4o): latency=800ms ok=38 err=0 ✅
  gemini (gemini-2.0-flash): latency=2400ms ok=12 err=5 ⛔ OPEN
```

### 6. Concurrent Speculative Overflow

When `QueueMode::Speculative` is active with an `AdaptiveRouter`, the session actor spawns the primary agent call as a separate tokio task and polls the inbox concurrently via `tokio::select!`. If a new user message arrives while the primary call is still running AND the patience threshold has been exceeded, the overflow message gets an immediate lightweight response — no waiting for the slow call to finish.

**How it works — truly concurrent:**

```
Timeline:
  t=0s    User sends message A
            → save A to session history (pre-save for context integrity)
            → spawn agent call as tokio::spawn task
            → enter select! loop (polls agent task + inbox concurrently)
  t=5s    User sends message B → arrives in inbox via select!
            → elapsed (5s) < patience (10s) → dropped (within patience)
  t=15s   User sends message C → arrives in inbox via select!
            → elapsed (15s) > patience (10s) → serve_overflow() called IMMEDIATELY
            → reads fresh session history (includes A + A's partial results)
            → lightweight router.chat() with no tools → response sent to user
  t=18s   User sends message D → arrives in inbox
            → elapsed (18s) > patience (10s) → serve_overflow() called
            → reads fresh history (now includes A, C, C's response)
            → lightweight router.chat() → response sent to user
  t=45s   Agent task completes message A
            → full response (with tool results) delivered to user
            → responsiveness baseline updated
```

**Architecture — how concurrent execution is achieved:**

The key challenge was `&mut self` borrowing: the agent call needs `&self` access, but polling the inbox also requires `&mut self`. This is solved by:

1. **Interior-mutable reporter**: `Agent.reporter` uses `RwLock<Arc<dyn ProgressReporter>>` so `set_reporter()` takes `&self` instead of `&mut self`
2. **Arc-wrapped Agent**: The agent is `Arc<Agent>`, enabling `tokio::spawn` of the agent call as a separate task
3. **Pre-saved user message**: The primary user message is saved to session history BEFORE spawning, so overflow calls see it in context
4. **History deduplication**: When saving the primary call's `conv_response.messages`, the first user message is skipped (already pre-saved)

```rust
// Simplified flow in process_inbound_speculative:
let agent = Arc::clone(&self.agent);
let mut agent_task = tokio::spawn(async move {
    agent.process_message_tracked(&content, &history, media, &tracker).await
});

loop {
    tokio::select! {
        result = &mut agent_task => break result,
        msg = self.inbox.recv() => {
            if started.elapsed() > patience {
                self.serve_overflow(&msg, max_history).await;
            }
            // within patience → message dropped (drain_queue already ran)
        }
    }
}
```

**Patience threshold:**
- `2 × responsiveness baseline`, minimum 10 seconds
- If baseline not yet established (< 5 samples): 30 seconds
- Baseline learned from first 5 LLM requests (average latency)
- Typical baseline: ~2-5s → patience = 10s (clamped to minimum)

**`serve_overflow()` — the lightweight responder:**
1. Reads fresh session history (includes pre-saved primary message + any prior overflow responses)
2. Appends the overflow user message
3. Calls `router.chat(&messages, &[], &config)` — no tools, uses the adaptive router (may hedge-race)
4. Saves user + assistant messages to session history
5. Sends response to user immediately

**Key design decisions:**
1. **Truly concurrent** — Overflow responses are served DURING the primary call, not after. The `tokio::select!` loop polls both the agent task and inbox simultaneously
2. **History chaining** — Each overflow call reads fresh session history, so message D sees C's response in context
3. **No tools** — Overflow responses use `router.chat()` with empty tool specs (lightweight, fast, no side effects)
4. **All responses delivered** — The original slow response AND all overflow responses go to the user
5. **Context integrity** — Pre-saving the primary user message ensures overflow calls have full conversation context
6. **Within-patience drop** — Messages arriving before patience is exceeded are silently dropped (same as followup mode after drain_queue)
7. **Timestamp-sorted history** — After the primary call completes, `session.sort_by_timestamp()` restores chronological order. During concurrent execution, overflow messages are written to history before the primary's tool calls/results (which have earlier timestamps from actual execution time). The stable sort reorders them correctly, then `rewrite()` persists the sorted order to disk
8. **Background task compatibility** — `BackgroundResult` messages are handled in the select loop, so background tasks completing during a slow primary call are injected immediately (not blocked)

**Requirements for speculative mode to activate:**
- `gateway.queue_mode` must be `"speculative"` in config (or set via `/queue spec` at runtime)
- `adaptive_router` must exist (requires `fallback_models` with at least 1 entry in config)
- Both conditions checked at dispatch time: `self.queue_mode == QueueMode::Speculative && self.adaptive_router.is_some()`

**Validated:** 13/14 tests pass in end-to-end LLM test with deepseek + gemini-flash (OpenRouter). 4 overflow events served concurrently during slow multi-tool calls. Context integrity maintained at 7/7 recall after 2 overflow rounds.

### Provider Count Guardrails

When hedge or lane mode is enabled with fewer than 2 providers, the system warns the user:

```
/adaptive hedge
→ "⚠️ Only 1 provider configured — hedge needs ≥2 to race. Currently behaves like off mode."
```

With 2+ providers, the confirmation shows the count:
```
/adaptive hedge
→ "Adaptive mode: hedge (race 2 of 4 providers, take winner)"
```

This prevents confusion when users enable racing but only have one provider configured.

### Context Window Integrity

**Critical for hedge/lane mode:** When the agent processes a request involving tool calls (web search, file I/O, shell commands), the full chain of messages — user input, assistant reasoning, tool calls, tool results, and final response — must be persisted in session history. Otherwise, subsequent turns lose context about what tools were used and what they returned.

**Implementation:**
- `ConversationResponse` carries a `messages: Vec<Message>` field with ALL messages generated during processing (not just the final content string)
- `session_actor.rs` saves every message from `conv_response.messages` to session history (replacing the old approach of saving only user + assistant messages)
- This ensures hedge mode works correctly: even though different providers may win different rounds, the session history always reflects the complete tool-call chain from whichever provider won

**Verified:** 24/24 context integrity tests pass across all modes (off, hedge, lane) and mode switches, including multi-round conversations with web search, file I/O, shell commands, and memory operations.

### 7. Auto-Escalation via ResponsivenessObserver

The `ResponsivenessObserver` (`octos-llm/src/responsiveness.rs`) monitors LLM response latencies and automatically escalates/de-escalates adaptive protection.

```rust
pub struct ResponsivenessObserver {
    window: VecDeque<Duration>,     // Rolling window (capacity: 20)
    baseline: Option<Duration>,      // Learned from first 5 samples
    degradation_threshold: f64,      // 3.0× baseline
    consecutive_slow: u32,           // Counter
    slow_trigger: u32,               // 3 consecutive slow → activate
    active: bool,                    // Whether auto-protection is on
}
```

**Lifecycle:**

```
Normal:     baseline=100ms, requests at ~100ms → no action
Degradation: 3 requests at 400ms (>300ms = 3× baseline) → should_activate() = true
             → session_actor sets AdaptiveMode::Hedge + QueueMode::Speculative
Recovery:    1 request at 100ms → consecutive_slow resets to 0 → should_deactivate() = true
             → session_actor restores AdaptiveMode::Off + QueueMode::Followup
```

**Safety:**
- No action before baseline is established (needs 5 samples)
- Single normal request immediately resets degradation counter
- Auto-protection flag prevents re-triggering while already active

### 8. Long-Running Skill Status Emission

Skills like deep research, code generation, and multi-step analysis emit detailed progress updates so users always know what's happening.

```
User: "Do a deep research on WebAssembly's impact on server-side computing"

Agent: 🔍 Starting deep research...
       ├─ Phase 1/4: Gathering sources (12 found)
       ├─ Phase 2/4: Reading and analyzing...
       │  ├─ [1/12] WebAssembly spec overview ✓
       │  ├─ [2/12] WASI proposal analysis ✓
       │  └─ [3/12] Benchmark comparisons... (reading)
       ├─ Phase 3/4: Synthesizing findings
       └─ Phase 4/4: Writing report
```

**Status protocol:**

```rust
pub enum ToolProgress {
    Started { tool_name: String, total_phases: u32 },
    PhaseStarted { phase: u32, description: String },
    StepCompleted { phase: u32, step: u32, total_steps: u32, description: String },
    Completed { summary: String },
    Failed { error: String, phase: u32 },
}
```

*Status: ToolProgress enum defined but no consumers yet. Dashboard API not implemented.*

---

## How It All Fits Together

A typical interaction showing all capabilities working in concert:

```
10:00:00  User: "Deep research on Rust async runtimes, compare tokio vs async-std vs smol"
10:00:01  Agent: Spawns background research task [#1]          → (1) Background task
10:00:01  Agent: "Starting deep research in background..."     → (4) Streaming
10:00:15  User: "Also what about glommio?"                     → (3) Queued
10:00:20  User: "And monoio"                                   → (3) Queued, batched with previous
10:00:21  Agent: Processes both queued messages together        → (3) Collect mode
10:00:22  Agent: "Added glommio and monoio to the research."

10:01:30  [Provider latency spikes: claude-sonnet-4-6 → 12s avg]
10:01:30  ResponsivenessObserver: degradation detected          → (7) Auto-escalation
10:01:31  AdaptiveRouter: mode → Hedge, queue → Speculative
10:01:31  Agent: "⚡ Detected slow responses, enabling auto-failover"

10:02:00  User: "Deep dive on tokio internals — compare schedulers, I/O drivers, timers"
10:02:01  Agent: Hedged race, spawns as tokio task             → (5) Hedge + (6) Speculative
10:02:12  User: "Quick — what's the syntax for tokio::select?"
10:02:12  Agent: Patience exceeded (12s > 10s baseline×2)      → (6) serve_overflow()
10:02:13  Agent: Immediate answer via lightweight router call   → User unblocked!
10:02:45  Agent: Deep dive completes, full answer delivered     → Primary task done

10:03:30  User: "stop the research"                            → (2) Cancel
10:03:30  Agent: Cancels background task #1                    → (2) Cascade abort
10:03:31  Agent: "Research cancelled. Here's what I have so far: [partial results]"

10:03:45  [claude-sonnet-4-6 latency recovers to 1.8s]
10:03:45  ResponsivenessObserver: recovery detected            → (7) Auto-deactivation
10:03:46  AdaptiveRouter: mode → Off, queue → Followup
```

---

## Interaction Matrix: Routing Mode × Queue Mode

Any routing mode can be combined with any queue mode. The modes are independent axes:

| | Followup | Collect | Steer | Interrupt | Speculative |
|---|---|---|---|---|---|
| **Off** | Default setup | Batch overflow | Latest only | Cancel+process | Overflow→router (priority order) |
| **Hedge** | Race each msg | Race batched | Race latest | Cancel+race | Overflow→race |
| **Lane** | Score-select each | Score-select batch | Score-select latest | Cancel+score-select | Overflow→score-select |

The auto-escalation system sets both axes simultaneously: `Hedge + Speculative` on degradation, `Off + Followup` on recovery.

---

## Configuration

### Config File (`config.json`)

```json
{
  "fallback_models": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "api_key_env": "OPENAI_API_KEY"
    }
  ],
  "adaptive_routing": {
    "enabled": true,
    "mode": "off",
    "qos_ranking": false,
    "failure_threshold": 3,
    "latency_threshold_ms": 10000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60
  },
  "gateway": {
    "queue_mode": "followup"
  }
}
```

**Important:** `fallback_models` must have at least 1 entry for the `AdaptiveRouter` to be created. Without it, hedge mode and speculative overflow are unavailable (the system falls back to single-provider + normal queue mode). The primary provider is configured via the top-level `provider`/`model` fields.
```

### Runtime Override

Both `adaptive_routing.mode` and `gateway.queue_mode` can be changed at runtime via slash commands. Runtime changes do not persist across gateway restarts.

---

## Implementation Status

| Feature | Status | Files |
|---------|--------|-------|
| Background tasks (spawn) | ✅ Done | `octos-agent/src/tools/spawn.rs`, `session_actor.rs` |
| Multilingual abort | ✅ Done | `octos-core/src/abort.rs` (30+ triggers, 9 languages) |
| Queue modes (5) | ✅ Done | `config.rs` (QueueMode), `session_actor.rs` (drain_queue) |
| Slash commands (/adaptive, /queue) | ✅ Done | `session_actor.rs` (try_handle_command) |
| AdaptiveMode enum | ✅ Done | `octos-llm/src/adaptive.rs` (Off/Hedge/Lane as AtomicU8) |
| Hedged racing | ✅ Done | `adaptive.rs` (hedged_chat, tokio::select!) |
| Lane changing (score-based) | ✅ Done | `adaptive.rs` (select_provider, score()) |
| QoS ranking toggle | ✅ Done | `adaptive.rs` (AtomicBool, orthogonal) |
| ResponsivenessObserver | ✅ Done | `octos-llm/src/responsiveness.rs` |
| Auto-escalation/recovery | ✅ Done | `session_actor.rs` (Hedge+Speculative / Off+Followup) |
| Concurrent speculative overflow | ✅ Done | `session_actor.rs` (process_inbound_speculative, serve_overflow, tokio::spawn + select!) |
| System prompt documentation | ✅ Done | `prompts/gateway_default.txt` |
| ToolProgress enum | ⚠️ Defined | `octos-agent/src/tools/mod.rs` (no consumers) |
| Dashboard API | ❌ Not started | — |
| Hot-reload settings | ❌ Not started | — |

---

## Comparison: octos vs OpenClaw UX

| Capability | OpenClaw | octos (current) |
|-----------|----------|-------------------|
| Background tasks | ✅ Subagent spawn | ✅ SpawnTool with `mode: "background"` |
| User cancel | ✅ 30+ triggers | ✅ 30+ triggers, 9 languages, cascade abort |
| Queue modes | ✅ 6 modes | ✅ 5 modes (followup/collect/steer/interrupt/speculative) |
| Streaming | ✅ Edit-in-place | ✅ Edit-in-place via `ChannelStreamReporter` (think tag stripping) |
| Adaptive routing | ❌ | ✅ Off/Hedge/Lane + QoS (exclusive mode enum) |
| Hedged racing | ❌ | ✅ tokio::select! race, winner/loser, metrics |
| Quality ranking | ❌ | ✅ QoS toggle (orthogonal) |
| Auto-escalation | ❌ | ✅ ResponsivenessObserver (3× baseline, 3 consecutive) |
| Concurrent speculative overflow | ❌ | ✅ tokio::spawn + select! loop, serves overflow during slow calls |
| Slash commands | ✅ /adaptive /queue | ✅ /adaptive /queue (runtime mode switching) |
| Skill status emission | ❌ | ⚠️ ToolProgress defined, no consumers |

**octos's edge:** Adaptive intelligence. OpenClaw has better UX polish today, but octos combines that polish with runtime intelligence — the system observes, learns, and adapts without operator intervention.

---

## Related Documents

- [OPENCLAW_UX_DESIGN.md](./OPENCLAW_UX_DESIGN.md) — OpenClaw's UX patterns (what we're adopting)
- [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) — Full cross-pollination analysis
- [CHANNEL_ADAPTER_PATTERN.md](./CHANNEL_ADAPTER_PATTERN.md) — Channel decomposition (enables streaming/editing)
- [PROVIDER_RACING.md](./PROVIDER_RACING.md) — Provider racing design (now implemented as hedge mode)
- [SLACK_REFERENCE_ARCHITECTURE.md](./SLACK_REFERENCE_ARCHITECTURE.md) — Slack feature reference
