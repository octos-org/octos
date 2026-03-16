# OpenClaw Cross-Pollination Guide

Based on comprehensive code review of [openclaw/openclaw](https://github.com/openclaw/openclaw) (Mar 2026). This documents what octos should adopt from OpenClaw and what octos already does better.

---

## What octos Should Adopt from OpenClaw

### 1. Channel Adapter Pattern

**Current octos**: Single `Channel` trait with 14 methods.
**OpenClaw**: 14+ decomposed adapter traits per channel.

See [CHANNEL_ADAPTER_PATTERN.md](./CHANNEL_ADAPTER_PATTERN.md) for the full proposal with Rust trait definitions and migration plan.

### 2. Slack Reference Architecture

**Current octos**: Basic Slack with Socket Mode, text-only.
**OpenClaw**: 40+ file Slack implementation with streaming, Block Kit, threading, slash commands.

See [SLACK_REFERENCE_ARCHITECTURE.md](./SLACK_REFERENCE_ARCHITECTURE.md) for the full feature reference and implementation priority.

### 3. DM Pairing Mode

**Current octos**: Binary `allowed_senders` list — edit config to add users.
**OpenClaw**: 4-mode DM policy (pairing, allowlist, open, disabled).

The **pairing mode** is the key innovation:
1. Unknown sender messages the bot
2. Bot issues a pairing code (random, time-limited)
3. Owner sees pairing request in dashboard/CLI
4. Owner approves -> sender added to allowlist automatically
5. No config file editing required

```rust
pub enum DmPolicy {
    Open,                          // Accept all
    Pairing { ttl_secs: u64 },    // Issue code, await approval
    Allowlist,                     // Pre-configured list only
    Disabled,                      // Ignore all DMs
}

pub struct PairingChallenge {
    pub code: String,              // 8-char random code
    pub sender_id: String,
    pub channel: String,
    pub issued_at: Instant,
    pub ttl: Duration,
}
```

**Applies to**: WhatsApp, Telegram, Discord, Signal, Slack — any channel with DMs.

### 4. Secret Redaction in Logs

**Current octos**: Basic `tracing` output, no redaction.
**OpenClaw**: 13+ regex patterns covering API keys, PEM blocks, token prefixes.

Token prefixes to redact:
```
sk-        (OpenAI, Anthropic)
ghp_       (GitHub PAT)
gho_       (GitHub OAuth)
xox[bpas]- (Slack tokens)
npm_       (npm tokens)
AIza       (Google API keys)
SG.        (SendGrid)
glpat-     (GitLab PAT)
AKIA       (AWS access key)
```

Additional patterns:
- `Authorization: Bearer ...` headers
- `-----BEGIN ... PRIVATE KEY-----` PEM blocks
- SSH keys (`ssh-rsa`, `ssh-ed25519`)
- Telegram bot tokens (`[0-9]+:AA...`)

Implementation: Add a tracing subscriber layer that intercepts log output and applies bounded regex replacement. Use ReDoS-safe patterns (no unbounded repetition).

### 5. Webhook Body Flood Protection

**Current octos**: No body size limits on webhook handlers.
**OpenClaw**: Per-request streaming byte tracking with hard limits.

```rust
pub struct BodyLimitGuard {
    max_bytes: usize,          // 1MB default
    timeout: Duration,         // 30s default
    bytes_read: usize,
}
```

Enforce on every HTTP webhook handler (Telegram, Slack HTTP mode, Feishu, etc.). Destroy request on violation — don't just log.

### 6. Unauthorized Flood Guard (WebSocket)

**Current octos**: No auth flood detection on WebSocket connections.
**OpenClaw**: Closes connections after N unauthorized attempts with logarithmic logging.

```rust
pub struct UnauthorizedFloodGuard {
    attempts: u32,
    close_after: u32,          // default: 10
    last_log_at: u32,          // logarithmic logging to prevent log DoS
}
```

Apply to the dashboard WebSocket and any gateway WebSocket endpoints.

### 7. Reconnection Intelligence

**Current octos**: Fixed 5-second backoff for WhatsApp.
**OpenClaw**: Error-code-specific handling per channel.

Pattern to adopt:
```rust
pub struct ReconnectPolicy {
    initial_ms: u64,
    max_ms: u64,
    factor: f64,
    jitter: f64,               // 0.0-1.0
    max_attempts: u32,
}

pub enum DisconnectAction {
    Retry(ReconnectPolicy),    // Transient error — backoff and retry
    Stop(String),              // Permanent error — stop and report reason
    RestartOnce,               // Server restart — retry immediately once
    Reauth,                    // Logged out — prompt re-authentication
}
```

Map specific error codes to actions:
- WhatsApp 440 (session conflict) -> `Stop`
- WhatsApp 515 (server restart) -> `RestartOnce`
- Slack `invalid_auth` / `token_revoked` -> `Stop`
- Telegram 401 -> `Stop`
- Generic disconnect -> `Retry`

### 8. Multi-Account Per Channel

**Current octos**: One connection per channel type.
**OpenClaw**: Multiple accounts per channel with independent config.

```rust
pub struct ChannelAccountConfig {
    pub account_id: String,
    pub enabled: bool,
    pub dm_policy: DmPolicy,
    pub allow_from: Vec<String>,
    pub settings: serde_json::Value,  // Channel-specific
}
```

Use cases:
- WhatsApp: personal number + business number
- Telegram: different bots for different agents
- Slack: multiple workspaces
- Discord: multiple guilds with different bot tokens

### 9. Per-Channel/Per-Group Tool Policies

**Current octos**: Global tool policy only.
**OpenClaw**: Tool policies scoped to channel, group, or even individual Slack channels.

```rust
pub struct GroupToolPolicy {
    pub channel: String,
    pub group_id: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub denied_tools: Option<Vec<String>>,
}
```

Critical for safety: restrict `exec`, `write_file`, `shell` in group chats where multiple untrusted users are present.

### 10. Health Monitoring & Status

**Current octos**: No channel health reporting.
**OpenClaw**: Per-channel health state with connection tracking, issue collection, and probe support.

```rust
pub struct ChannelHealthSnapshot {
    pub channel: String,
    pub account_id: String,
    pub state: ConnectionState,        // Connected, Disconnected, Degraded
    pub last_event_at: Option<Instant>,
    pub last_disconnect: Option<Instant>,
    pub issues: Vec<StatusIssue>,
    pub auth_age: Option<Duration>,
}

pub enum ConnectionState {
    Connected,
    Disconnected { reason: String },
    Degraded { reason: String },
    NotConfigured,
}
```

Expose via dashboard API for real-time monitoring.

---

## What octos Already Does Better

These are areas where octos's existing implementation is ahead of OpenClaw. Keep and maintain these advantages.

### 1. Adaptive Model Routing with Circuit Breakers

octos's `AdaptiveRouter` scores providers using weighted metrics (latency EMA + error rate + priority). OpenClaw uses static fallback chains with per-profile cooldown but no metrics-driven routing.

**Keep**: `crates/octos-llm/src/adaptive.rs`

### 2. Hybrid Memory Search (BM25 + HNSW)

octos has a dedicated memory crate with BM25 keyword scoring + HNSW vector similarity. OpenClaw uses SQLite without hybrid retrieval.

**Keep**: `crates/octos-memory/src/hybrid_search.rs`

### 3. Token-Aware Compaction

octos's compaction is model-aware, estimating actual token counts. More precise than character-count heuristics.

**Keep**: `crates/octos-agent/src/compaction.rs`

### 4. Platform-Specific Sandbox Backends

Three backends with auto-detection: bubblewrap (Linux), sandbox-exec (macOS), Docker. OpenClaw's sandbox is Docker-focused.

**Keep**: `crates/octos-agent/src/sandbox.rs`

### 5. Environment Variable Sanitization

18-var blocklist covering injection vectors (`LD_PRELOAD`, `DYLD_*`, `NODE_OPTIONS`, `PYTHONSTARTUP`, `BASH_ENV`, etc.) applied across all sandboxes and MCP servers.

**Keep**: Shared `BLOCKED_ENV_VARS` constant

### 6. Tool Argument Size Limits

1MB max per tool call with non-allocating `estimate_json_size`. OpenClaw has no equivalent guard.

**Keep**: `crates/octos-agent/src/tools/mod.rs`

### 7. Structured Error Context (eyre)

`eyre`/`color-eyre` with actionable suggestion hints. Better developer experience than flat error messages.

**Keep**: Error handling patterns throughout

### 8. Atomic Session Writes

Write-then-rename for crash-safe JSONL persistence. Prevents half-written files.

**Keep**: `crates/octos-bus/src/session.rs`

### 9. Native Performance

Pure Rust with rustls, no V8 overhead. Streaming via async channels without buffering entire responses.

**Keep**: Architectural advantage

---

## Subagent Enhancements

octos already has background subagent spawning via `SpawnTool` (`crates/octos-agent/src/tools/spawn.rs`). Four enhancements to make it production-grade:

### 11. Cascade Abort

**Current**: Background subagents run independently via `tokio::spawn()`. When the parent session cancels, children keep running to completion, wasting compute and tokens.

**Target**: Track spawned tasks in a registry. When parent cancels, propagate cancellation to all children.

```rust
use tokio_util::sync::CancellationToken;

pub struct SubagentRegistry {
    entries: HashMap<String, SubagentEntry>,
}

struct SubagentEntry {
    id: String,
    parent_session: String,
    cancel: CancellationToken,
    spawned_at: Instant,
    join_handle: JoinHandle<()>,
}

impl SubagentRegistry {
    /// Register a spawned subagent with its cancellation token.
    pub fn register(&mut self, id: String, parent: String, cancel: CancellationToken, handle: JoinHandle<()>) {
        self.entries.insert(id.clone(), SubagentEntry {
            id, parent_session: parent, cancel, spawned_at: Instant::now(), join_handle: handle,
        });
    }

    /// Cancel all subagents belonging to a parent session.
    pub fn cancel_for_parent(&mut self, parent_session: &str) {
        let children: Vec<String> = self.entries.iter()
            .filter(|(_, e)| e.parent_session == parent_session)
            .map(|(k, _)| k.clone())
            .collect();
        for id in children {
            if let Some(entry) = self.entries.remove(&id) {
                info!(subagent = %id, parent = %parent_session, "cascade abort: cancelling subagent");
                entry.cancel.cancel();
            }
        }
    }

    /// Clean up completed entries.
    pub fn reap_finished(&mut self) {
        self.entries.retain(|_, e| !e.join_handle.is_finished());
    }
}
```

**Integration** (`crates/octos-agent/src/tools/spawn.rs`):
```rust
// In background spawn (line ~358):
let cancel = CancellationToken::new();
let child_cancel = cancel.child_token();

let handle = tokio::spawn(async move {
    tokio::select! {
        result = worker.run_task(&subtask) => {
            // Normal completion — announce result
            announce_result(result, &inbound_tx).await;
        }
        _ = child_cancel.cancelled() => {
            info!(subagent = %id, "subagent cancelled by parent");
            // Cleanup: no result announcement
        }
    }
});

registry.register(subagent_id, parent_session, cancel, handle);
```

**In SessionActor cancel handler** (`crates/octos-cli/src/session_actor.rs`):
```rust
ActorMessage::Cancel => {
    self.cancelled.store(true, Ordering::Relaxed);
    self.subagent_registry.cancel_for_parent(&self.session_key);
}
```

**Files to modify**:
- `crates/octos-agent/src/tools/spawn.rs` — Add `CancellationToken` to spawned tasks
- `crates/octos-cli/src/session_actor.rs` — Call `cancel_for_parent()` on cancel
- New: `crates/octos-agent/src/subagent_registry.rs` — Registry struct (~80 LOC)

### 12. Result Streaming

**Current**: Background subagents send a single `InboundMessage` when fully complete. For long tasks (research, deep analysis), the user sees nothing until the entire task finishes.

**Target**: Stream partial results back via a channel so the parent sees progress.

```rust
use tokio::sync::mpsc;

pub struct SubagentProgress {
    pub subagent_id: String,
    pub kind: ProgressKind,
}

pub enum ProgressKind {
    /// LLM is generating text (stream partial output)
    Streaming { delta: String },
    /// Tool execution started
    ToolStarted { tool_name: String, tool_input_summary: String },
    /// Tool execution completed
    ToolCompleted { tool_name: String, success: bool },
    /// Iteration completed (N of max)
    IterationDone { current: u32, max: u32 },
    /// Final result
    Completed { output: String, success: bool },
    /// Cancelled
    Cancelled,
}
```

**Integration in SpawnTool**:
```rust
// Create progress channel
let (progress_tx, mut progress_rx) = mpsc::channel::<SubagentProgress>(64);

// Pass progress_tx to subagent's agent loop
let handle = tokio::spawn(async move {
    worker.run_task_with_progress(&subtask, progress_tx).await
});

// Forward progress to parent session (separate task)
tokio::spawn(async move {
    while let Some(progress) = progress_rx.recv().await {
        match progress.kind {
            ProgressKind::Streaming { delta } => {
                // Update a "subagent status" message in the parent's chat
                update_subagent_status(&inbound_tx, &origin, &progress.subagent_id, &delta).await;
            }
            ProgressKind::Completed { output, success } => {
                announce_result(&inbound_tx, &origin, output, success).await;
                break;
            }
            _ => { /* log or display as status indicator */ }
        }
    }
});
```

**In Agent loop** (`crates/octos-agent/src/agent.rs`):
```rust
// After each LLM streaming chunk:
if let Some(ref tx) = self.progress_tx {
    let _ = tx.send(SubagentProgress {
        subagent_id: self.id.clone(),
        kind: ProgressKind::Streaming { delta: chunk.clone() },
    }).await;
}

// After each tool completion:
if let Some(ref tx) = self.progress_tx {
    let _ = tx.send(SubagentProgress {
        subagent_id: self.id.clone(),
        kind: ProgressKind::ToolCompleted { tool_name, success },
    }).await;
}
```

**Files to modify**:
- `crates/octos-agent/src/agent.rs` — Add optional `progress_tx` channel, emit progress events
- `crates/octos-agent/src/tools/spawn.rs` — Create channel, forward to parent
- New: `crates/octos-agent/src/subagent_progress.rs` — Types (~40 LOC)

### 13. Multi-Level Spawning

**Current**: Subagents have `spawn` tool hard-denied (`deny: vec!["spawn".into()]`). Only 1 level of spawning is possible.

**Target**: Replace with a depth counter. Child at depth N can spawn at depth N+1, up to a configurable limit.

```rust
pub struct SpawnContext {
    pub depth: u32,
    pub max_depth: u32,          // default: 3
    pub parent_session: String,
    pub root_session: String,    // original parent (for cascade abort)
}

impl SpawnContext {
    pub fn can_spawn(&self) -> bool {
        self.depth < self.max_depth
    }

    pub fn child(&self, child_session: String) -> Self {
        Self {
            depth: self.depth + 1,
            max_depth: self.max_depth,
            parent_session: child_session,
            root_session: self.root_session.clone(),
        }
    }
}
```

**Integration in SpawnTool** (`crates/octos-agent/src/tools/spawn.rs`):
```rust
// Replace the hard deny (line ~303):
// OLD:
//   let mut denied = vec!["spawn".into()];
// NEW:
let mut denied = Vec::new();
if !self.spawn_context.can_spawn() {
    denied.push("spawn".into());  // Only deny at max depth
}

// When creating the child agent:
let child_context = self.spawn_context.child(child_session_id);
let child_spawn_tool = SpawnTool::new(/* ... */)
    .with_spawn_context(child_context);
```

**Depth tracking in registry** (for cascade abort across levels):
```rust
// SubagentRegistry tracks the full tree:
//   root → child-0 → grandchild-0
//                   → grandchild-1
//        → child-1
// cancel_for_parent("root") cancels child-0, child-1
// cancel_for_parent("child-0") cancels grandchild-0, grandchild-1
```

**Safety guardrails**:
- Default `max_depth: 3` (root → child → grandchild → great-grandchild)
- Configurable via `config.spawn.max_depth`
- Total concurrent subagents capped by a semaphore (e.g., 10 total across all depths)
- Each level inherits parent's tool policy restrictions (deny list accumulates)

**Files to modify**:
- `crates/octos-agent/src/tools/spawn.rs` — Replace hard deny with depth check
- New: `SpawnContext` struct in spawn.rs or separate file (~30 LOC)

### 14. Context Forking

**Current**: Subagents start with empty conversation history. They only access shared episodic memory. No awareness of the parent's current conversation.

**Target**: Optionally copy the last N messages from parent's history, so the subagent has conversational context.

```rust
pub struct SpawnInput {
    pub task: String,
    pub mode: SpawnMode,
    // ... existing fields ...

    /// Copy the last N messages from parent's conversation into the subagent's context.
    /// Default: 0 (no context). Useful values: 5-20.
    pub fork_context: Option<u32>,
}
```

**Integration**:
```rust
// In SpawnTool::execute(), after creating the subagent:
let mut initial_messages = Vec::new();

if let Some(n) = input.fork_context {
    // Read parent's conversation history
    let parent_history = self.session_manager
        .get_history(&parent_session_key, n as usize)
        .await?;

    // Prepend as context (marked as forked)
    initial_messages.push(Message {
        role: MessageRole::System,
        content: format!(
            "[Context from parent conversation — last {} messages]\n\
             Use this for awareness but focus on your assigned task.",
            parent_history.len()
        ),
    });
    initial_messages.extend(parent_history);
}

// Pass to subagent
worker.run_task_with_context(&subtask, initial_messages).await
```

**In Agent** (`crates/octos-agent/src/agent.rs`):
```rust
// Modify run_task to accept optional pre-seeded messages:
pub async fn run_task_with_context(
    &self,
    task: &Task,
    forked_context: Vec<Message>,
) -> Result<TaskResult> {
    let mut messages = Vec::new();
    messages.push(self.build_system_message());
    messages.extend(forked_context);  // Inject forked context
    messages.push(self.build_user_message(task));
    // ... continue with normal agent loop
}
```

**Token budget guard**:
```rust
// Don't fork more context than fits in the subagent's context window
let max_fork_tokens = (config.context_window as f64 * 0.3) as u32; // 30% budget
let forked = truncate_to_token_budget(parent_history, max_fork_tokens);
```

**Files to modify**:
- `crates/octos-agent/src/tools/spawn.rs` — Add `fork_context` param, read parent history
- `crates/octos-agent/src/agent.rs` — Add `run_task_with_context()` variant

---

## Provider Racing

octos's `LlmProvider` trait (`&self + Send + Sync`) already allows concurrent calls. Add a `RacingProvider` wrapper that races two providers and returns whichever responds first.

See [PROVIDER_RACING.md](./PROVIDER_RACING.md) for the full design with code, streaming racing, adaptive integration, and cost considerations.

---

## Neither Has Well (Shared Gaps)

| Gap | Description | Priority |
|-----|-------------|----------|
| Per-sender rate limiting | No sliding-window rate limit at channel ingress | High |
| Cost-aware routing | Neither dynamically selects cheaper models | Medium |
| Request correlation IDs | No trace ID propagation across async boundaries | Medium |
| Config migration system | octos has unused `version` field | Medium |
| Plugin runtime sandboxing | Plugins can access full Node.js/Rust APIs | Low |

---

## Summary

| Area | OpenClaw Advantage | octos Advantage |
|------|-------------------|-------------------|
| Channel abstraction | Adapter pattern (14 traits) | -- |
| Slack depth | 40+ files, Block Kit, streaming | -- |
| DM policy | 4-mode pairing system | -- |
| Log redaction | 13+ token patterns | -- |
| Flood protection | Body limits + auth flood guard | -- |
| Reconnection | Error-code-specific handling | -- |
| Multi-account | Per-channel account isolation | -- |
| Health monitoring | Per-channel health snapshots | -- |
| Model routing | -- | Adaptive scoring + circuit breaker |
| Memory search | -- | BM25 + HNSW hybrid |
| Compaction | -- | Token-aware (model-specific) |
| Sandboxing | -- | 3 backends + env sanitization |
| Tool safety | -- | 1MB arg size limit |
| Error UX | -- | eyre suggestions |
| Performance | -- | Native Rust |

---

## Related Documents

- [CHANNEL_ADAPTER_PATTERN.md](./CHANNEL_ADAPTER_PATTERN.md) — Detailed adapter trait proposal
- [SLACK_REFERENCE_ARCHITECTURE.md](./SLACK_REFERENCE_ARCHITECTURE.md) — Full Slack feature reference
- [PROVIDER_RACING.md](./PROVIDER_RACING.md) — Provider racing design and implementation
- [OPENCLAW_UX_DESIGN.md](./OPENCLAW_UX_DESIGN.md) — OpenClaw's UX design approach
- [openclaw-gap-analysis.md](./openclaw-gap-analysis.md) — Previous gap analysis (all 9 items complete)
- [ARCHITECTURE.md](./ARCHITECTURE.md) — octos architecture overview
