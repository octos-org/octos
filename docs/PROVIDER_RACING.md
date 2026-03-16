# Provider Racing & Speculative Queue Design

Race two LLM providers concurrently. Return whichever responds first. Cancel the loser. Serve concurrent user messages without blocking.

**Status: Implemented** — `AdaptiveMode::Hedge` inside `AdaptiveRouter` for provider racing, `QueueMode::Speculative` inside `SessionActor` for concurrent message handling.

---

## Motivation

When the adaptive router can't confidently pick a winner (top two providers are close in score), or when the user flags a task as urgent, racing both providers halves the worst-case latency at the cost of doubled input tokens.

---

## Implementation (Actual)

### Integrated in AdaptiveRouter (`adaptive.rs`)

The racing logic is part of `AdaptiveRouter` rather than a standalone wrapper. This was chosen because:
1. `hedged_chat` needs access to per-provider metrics for alternate selection and recording
2. No extra cloning of messages/tools (shared references)
3. Single atomic mode check determines the code path

```rust
// crates/octos-llm/src/adaptive.rs

pub enum AdaptiveMode {
    Off = 0,    // Static priority, failover on circuit-broken
    Hedge = 1,  // Hedged racing (this document)
    Lane = 2,   // Score-based lane changing
}

impl AdaptiveRouter {
    // Main entry point — checks mode and dispatches
    async fn chat(&self, messages, tools, config) -> Result<ChatResponse> {
        let (primary_idx, _) = self.select_provider();

        if self.mode() == AdaptiveMode::Hedge {
            if let Some(result) = self.hedged_chat(primary_idx, messages, tools, config).await {
                return result;
            }
            // Only one provider available — fall through to single-provider path
        }

        // Single-provider path (Off or Lane mode, or only 1 provider)
        self.try_chat(primary_idx, messages, tools, config).await
        // ... with failover chain on error
    }

    async fn hedged_chat(&self, primary_idx, messages, tools, config)
        -> Option<Result<ChatResponse>>
    {
        // 1. Pick alternate: best non-primary, non-circuit-broken by score
        let alternate_idx = self.slots.iter().enumerate()
            .filter(|(i, s)| *i != primary_idx && !s.metrics.is_circuit_open(threshold))
            .map(|(i, s)| (i, self.score(s)))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(i, _)| i)?;  // None if no alternate → caller falls through

        // 2. Race both providers via tokio::select!
        tokio::select! {
            result = self.try_chat(primary_idx, ...) => {
                // Primary finished first
                if result.is_ok() { return Some(result); }
                // Primary failed → retry alternate sequentially
                Some(self.try_chat(alternate_idx, ...).await)
            }
            result = self.try_chat(alternate_idx, ...) => {
                // Alternate finished first
                if result.is_ok() { return Some(result); }
                // Alternate failed → retry primary sequentially
                Some(self.try_chat(primary_idx, ...).await)
            }
        }
    }
}
```

### Cancellation: Why Dropping Works

When `tokio::select!` picks a winner, the loser's future is dropped. Inside `try_chat`, the HTTP request's `reqwest` future is dropped at its next `.await` point, closing the TCP connection. The provider stops receiving tokens.

- **Winner's metrics**: Recorded inside `try_chat` before returning (latency, success/failure)
- **Loser's metrics**: NOT recorded (future dropped mid-flight) — correct behavior, we only score completed requests
- **Input tokens**: Charged by both providers (prompt already sent) — unavoidable cost of racing
- **Output tokens**: Only the winner generates output

### Provider Selection for Racing

Primary and alternate are chosen differently:

| Role | Selection Method |
|------|-----------------|
| Primary | Priority order (same as Off mode) — first non-circuit-broken provider |
| Alternate | Best score among remaining non-circuit-broken providers |

This means in Hedge mode, the primary is always your configured preferred provider, while the alternate is the best-performing backup.

---

## Configuration

Racing is controlled via the `AdaptiveMode` enum, not a separate config block.

### Config File

**Prerequisite:** At least one entry in `fallback_models` is required for the `AdaptiveRouter` to be created. Without fallback models, hedge mode is unavailable.

```json
{
  "provider": "deepseek",
  "model": "deepseek-chat",
  "api_key_env": "DEEPSEEK_API_KEY",
  "fallback_models": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "api_key_env": "OPENAI_API_KEY"
    }
  ],
  "adaptive_routing": {
    "enabled": true,
    "mode": "hedge"
  },
  "gateway": {
    "queue_mode": "speculative"
  }
}
```

### Runtime Toggle

```
/adaptive hedge     → enable hedged racing
/adaptive off       → disable (static priority)
/adaptive lane      → switch to lane changing instead
```

Aliases for hedge: `race`, `circuit`

---

## Cost Analysis

| Scenario | Input Cost | Output Cost | Net |
|----------|-----------|-------------|-----|
| A wins in 2s, B dropped | 2× input | 1× output (A only) | ~1.5× normal |
| B wins in 1s, A dropped | 2× input | 1× output (B only) | ~1.5× normal |
| A fails, B succeeds | 2× input | 1× output (B only) | ~1.5× normal |
| Both fail | 2× input | 0× output | 2× wasted |

**Rule of thumb**: Racing costs ~1.5× a single call (double input, single output). Worth it when latency matters more than cost.

---

## Auto-Escalation

The `ResponsivenessObserver` can automatically enable hedge mode when it detects sustained latency degradation:

```
Normal (baseline 100ms):
  → AdaptiveMode::Off, QueueMode::Followup

Degradation (3 consecutive requests at 3× baseline):
  → Auto-escalate to AdaptiveMode::Hedge + QueueMode::Speculative

Recovery (1 normal request):
  → Auto-restore to AdaptiveMode::Off + QueueMode::Followup
```

This means users get racing protection automatically when they need it most, without manual intervention.

### Speculative Overflow During Hedge

When auto-escalation activates `Hedge + Speculative`, the system provides truly concurrent overflow handling:

1. Primary agent call spawned as `tokio::spawn` task (separate future)
2. `tokio::select!` loop polls both the agent task and the session inbox
3. If a new message arrives while the primary is running, `serve_overflow()` fires immediately — no patience gate, every overflow is served
4. Overflow spawns a **full agent task** (`agent.process_message_tracked()`) with complete tool access — not a lightweight router call
5. Overflow gets a **pre-primary history snapshot** (see below) and saves results (tool calls, tool results, assistant reply) back to the session
6. Overflow responses delivered to user while the slow primary call continues in background
7. When the primary finishes after overflow already responded, its reply is prefixed with "⬆️ Earlier task completed:" so the user knows it's a delayed result

This ensures users are never blocked waiting for a slow multi-tool call to finish — overflow gets the same capabilities as the primary (web search, code execution, etc.), not just a quick LLM-only answer.

---

## Speculative Queue Implementation

### Architecture

`QueueMode::Speculative` enables concurrent message processing within a single session. Multiple user messages can be "in flight" simultaneously, sharing the same conversation context and LLM agent.

```
User sends msg A (long task, e.g., deep research)
    ↓
SessionActor::process_inbound_speculative()
    ├── Saves user msg A to session history
    ├── Snapshots history → overflow_history
    ├── Spawns primary agent task (tokio::spawn)
    └── Enters select! loop:
            ↓                          ↓
        agent_task completes     User sends msg B
            ↓                          ↓
        break loop              serve_overflow(B, overflow_history)
                                    ↓
                                Spawns concurrent agent task
                                    ↓
                                Sends response to user immediately
```

### Overflow History Snapshot

The overflow task receives a snapshot of the conversation from **before** the primary task started:

```rust
// session_actor.rs — process_inbound_speculative()

// Primary agent gets history WITHOUT the last user message
// (process_message_tracked re-adds it internally)
let history_for_agent = history[..history.len() - 1].to_vec();

// Overflow gets the same base — prior context WITHOUT the primary
// user message. If overflow sees the primary question, the LLM
// answers both the primary and overflow questions together.
let overflow_history = history_for_agent.clone();

// Later, when overflow arrives:
self.serve_overflow(&message, &overflow_history);
```

**Why a snapshot, not the live session?**
- The primary task may be mid-execution with pending tool calls. Including those in overflow's context would make the LLM try to re-answer the primary question.
- The snapshot **excludes** the primary user message — if the overflow LLM sees both the primary question and the overflow question, it answers both, producing duplicate/contaminated output.
- The snapshot provides full conversation context (user identity, preferences, prior exchanges) — not empty history.

### Message Integrity Under Concurrency

When multiple agent tasks run concurrently, they write to the same session. This creates three integrity problems:

#### Problem 1: Interleaved Tool Call Groups

OpenAI-compatible APIs require strict ordering: `assistant(tool_calls) → tool(result)*`. But concurrent writes interleave:

```
Actual write order to session:
  [Assistant A: tool_calls=[tc1]]     ← primary starts tool
  [User B: "what's the weather?"]     ← overflow message arrives
  [Tool: tc1 result]                  ← primary's tool finishes
```

The LLM sees a user message between a tool_call and its result — API error or hallucination.

**Fix: `repair_message_order()`** (`agent.rs`) — called before every LLM call. Gathers scattered tool results back to their parent assistant by matching `tool_call_id` to the assistant's `tool_calls[].id`:

```rust
fn repair_message_order(messages: &mut Vec<Message>) {
    // For each assistant with tool_calls:
    //   1. Count contiguous tool results already in place
    //   2. Scan forward for scattered results matching expected IDs
    //   3. Move them to the insertion point (right after assistant)
    // User/system messages between groups are NOT displaced —
    // they stay in place to preserve concurrent conversation threads.
}
```

Key design: user messages are **never displaced**. Only tool results are gathered to their parent. This preserves concurrent conversation threads instead of disrupting them.

#### Problem 2: Orphaned Tool Calls / Results

Session compaction or crashes can leave tool_calls without results, or tool results without a parent assistant.

**Fix: `repair_tool_pairs()`** (`agent.rs`) — called after `repair_message_order()` before every LLM call:

```rust
fn repair_tool_pairs(messages: &mut Vec<Message>) {
    // 1. Collect all tool_call IDs from assistant messages
    // 2. Collect all tool_call_ids from Tool result messages
    // 3. Find matched pairs (ID in both sets)
    // 4. Strip tool_calls from assistants with ANY unmatched ID
    //    (replace content with "[Called tools: X, Y]")
    // 5. Remove Tool results whose call ID has no parent
}
```

#### Problem 3: Compaction Splits Tool Groups

Session compaction summarizes older messages when count > threshold (default 40). A naive count-based split can cut between an assistant's tool_calls and their results:

```
Before compaction (42 messages, keep_recent=10):
  ... [summarize these 32] | [keep these 10] ...
  If message 32 is a tool result and 31 is the assistant → broken pair
```

**Fix:** Tool-pair-aware boundary in `compaction.rs`:

```rust
let mut to_summarize = total - config.keep_recent;

// Walk backwards past tool results to find the group start
while to_summarize > 0
    && session.messages[to_summarize].role == MessageRole::Tool
{
    to_summarize -= 1;
}

// If the previous message is an assistant with tool_calls
// whose results are in the "recent" half, include it too
if to_summarize > 0 {
    let prev = &session.messages[to_summarize - 1];
    if prev.role == MessageRole::Assistant
        && prev.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
    {
        to_summarize -= 1;
    }
}
```

### Message Repair Pipeline

Before every LLM call, the agent runs this pipeline on the message history:

```
trim_to_context_window()      ← Truncate oldest messages if over token limit
        ↓
normalize_system_messages()   ← Merge/position system messages
        ↓
repair_message_order()        ← Gather scattered tool results to parent
        ↓
repair_tool_pairs()           ← Remove orphaned calls/results
        ↓
LLM call                     ← Clean, valid message sequence
```

This makes the system self-healing: even if concurrent writes corrupt the session on disk, the next LLM call sees a clean message sequence.

### Overflow Task Lifecycle

```rust
// session_actor.rs — serve_overflow()

fn serve_overflow(&self, msg: &InboundMessage, pre_primary_history: &[Message]) {
    tokio::spawn(async move {
        // 1. Save user message to session history
        mgr.add_message(&session_key, user_msg).await;

        // 2. Run full agent with pre-primary history snapshot
        let result = agent.process_message_tracked(
            &content,
            &pre_primary_history,  // NOT empty, NOT live session
            vec![],                // no media for overflow
            &tracker,
        ).await;

        // 3. Save assistant response + tool calls to session
        for msg in conv_response.messages {
            mgr.add_message(&session_key, msg).await;
        }

        // 4. Send response to user
        out_tx.send(OutboundMessage { content: reply, .. }).await;
    });
}
```

### Delayed Primary Response

When the primary finishes after an overflow has already responded:

```rust
// session_actor.rs — process_inbound_speculative() post-processing

if overflow_served {
    // User already got a response from overflow.
    // Prefix primary's reply so user knows it's delayed.
    reply = format!("⬆️ Earlier task completed:\n\n{}", reply);
}
```

---

## Provider Count Guardrails

Hedge mode requires at least 2 non-circuit-broken providers to race. When only 1 provider is configured, the system:

1. Falls through to the single-provider path (behaves like Off mode)
2. Warns the user via the `/adaptive hedge` command response:
   ```
   ⚠️ Only 1 provider configured — hedge needs ≥2 to race. Currently behaves like off mode.
   ```

With 2+ providers, the response confirms the count:
```
Adaptive mode: hedge (race 2 of 4 providers, take winner)
```

---

## Context Window Integrity

**Critical:** In hedge mode, different providers may win different conversation rounds. The session history must preserve the complete tool-call chain from whichever provider won each round.

**Problem:** Previously, `ConversationResponse` only returned the final content string. Tool calls, tool results, and intermediate assistant reasoning were discarded. This meant subsequent turns couldn't reference previous tool results.

**Solution:** `ConversationResponse` now carries `messages: Vec<Message>` — the full sequence of messages generated during processing. `session_actor.rs` persists ALL of them to session history, not just user + assistant.

**Why this matters for hedge mode specifically:**
- Round 1: Provider A wins, runs `web_search("tokio")` → result saved to history
- Round 2: Provider B wins, sees the full history including A's tool call and result
- Without this fix, Provider B would only see "here's info about tokio" without knowing a search was performed

**Note on tool execution:** In hedge mode, the loser is cancelled via `tokio::select!` drop. The loser never gets to execute tools — only the winner's tool calls run. This is correct behavior: tools have side effects (file writes, web requests), so only one provider should execute them.

### History Ordering During Concurrent Overflow

When speculative overflow is active, concurrent writes can cause out-of-order messages in session history:

```
Write order (append to session JSONL):
  [User A]                         ← pre-saved at t=0
  [User B overflow]                ← overflow written at t=15
  [Assistant B]                    ← overflow response at t=16
  [Assistant A: tool_calls=[tc1]]  ← primary saved at t=45
  [Tool: tc1 result]               ← primary saved at t=45
  [Assistant A: final reply]       ← primary response at t=45
```

The message repair pipeline fixes this before the next LLM call:

```
After repair_message_order() + repair_tool_pairs():
  [User A]                         ← stays (user messages never displaced)
  [User B overflow]                ← stays
  [Assistant B]                    ← stays
  [Assistant A: tool_calls=[tc1]]  ← stays (no scattered results to gather)
  [Tool: tc1 result]               ← already contiguous
  [Assistant A: final reply]       ← stays
```

The key insight: concurrent messages create **valid interleaving** (user A, then user B + response B, then response A). This is correct — the LLM sees both conversations and their responses. Only scattered tool results within a single tool_call group need repair.

---

## Files

| File | What |
|------|------|
| `crates/octos-llm/src/adaptive.rs` | `AdaptiveMode` enum, `hedged_chat()`, `select_provider()` |
| `crates/octos-llm/src/responsiveness.rs` | `ResponsivenessObserver` for auto-escalation + patience baseline |
| `crates/octos-cli/src/config.rs` | `AdaptiveRoutingMode`, `AdaptiveRoutingConfig`, `FallbackModel` |
| `crates/octos-cli/src/session_actor.rs` | `process_inbound_speculative` (spawn + select!), `serve_overflow` (overflow with history snapshot), `/adaptive` and `/queue` commands, auto-escalation |
| `crates/octos-cli/src/compaction.rs` | Session compaction with tool-pair-aware split boundary |
| `crates/octos-cli/src/commands/gateway/mod.rs` | Config → router wiring (`with_adaptive_config`), `adaptive_router_ref` |
| `crates/octos-agent/src/agent.rs` | `Agent` (Arc-compatible), `repair_message_order()`, `repair_tool_pairs()`, `is_retriable_response()` |

---

## Design Decisions

### Why not a separate `RacingProvider` wrapper?

The original design proposed a standalone `RacingProvider` that wraps two `Arc<dyn LlmProvider>`. This was rejected because:

1. **Metrics access**: Racing needs to pick the best alternate by score and record winner metrics — requires access to `ProviderMetrics` which lives inside `AdaptiveRouter`
2. **Dynamic switching**: Mode can change at runtime via `/adaptive hedge|off|lane` — a wrapper would need to be swapped out
3. **Single concern**: The router already manages providers; racing is just another routing strategy

### Why `AtomicU8` instead of `RwLock<AdaptiveMode>`?

- Lock-free: mode check happens on every LLM request (hot path)
- `AtomicU8` with `Ordering::Relaxed` is sufficient (mode changes propagate within microseconds)
- Mode enum values are 0/1/2, fitting in a single byte

### Why mutual exclusion (Off/Hedge/Lane)?

Hedge races 2 providers per request. Lane scores and picks 1 provider. Running both simultaneously would be incoherent: should we race the top-2 by score? That's just hedge with score-based alternate selection — which is exactly what hedge already does. The enum makes the choice explicit and avoids confusing combinations.

QoS ranking is orthogonal because it modifies the scoring function, not the routing strategy. Any mode can benefit from better scores.

---

## Related Documents

- [OCTOS_UX_VISION.md](./OCTOS_UX_VISION.md) — Full UX vision with all capabilities
- [OPENCLAW_UX_DESIGN.md](./OPENCLAW_UX_DESIGN.md) — OpenClaw's UX patterns
- [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) — Full cross-pollination guide
- [ARCHITECTURE.md](./ARCHITECTURE.md) — octos architecture overview
- [TESTING.md](./TESTING.md) — CI script, test inventory, and testing guide
