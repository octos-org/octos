# Background Tasks & Adaptive UX — Dev Plan

Concrete implementation plan grounded in the actual crew-rs codebase. Each phase is independently shippable.

---

## Phase 1: Background Task Result Injection (The Foundation)

**Goal:** Background subagent results get cleanly injected into the main session conversation history, not just announced as a new inbound message.

### Current State

`SpawnTool` (background mode) fires a `tokio::spawn`, then sends results back as an `InboundMessage` through `inbound_tx`:

```rust
// spawn.rs:403-414 — current result delivery
let announce = InboundMessage {
    channel: "system".into(),
    sender_id: "subagent".into(),
    chat_id: format!("{origin_channel}:{origin_chat_id}"),
    content, // "[Subagent N completed]\nTask: ...\nResult: ..."
    metadata: serde_json::json!({
        "deliver_to_channel": origin_channel,
        "deliver_to_chat_id": origin_chat_id,
    }),
};
```

This works, but the result goes through the full agent loop — the main agent receives it as a "user message" and must process it (costing another LLM call just to relay the result).

### Changes

**File: `crates/crew-cli/src/session_actor.rs`**

Add a dedicated result injection path to `SessionActor`:

```rust
// New enum variant in ActorMessage
pub enum ActorMessage {
    Inbound { message: InboundMessage, image_media: Vec<String> },
    Cancel,
    // NEW: inject background task result directly into history + notify user
    BackgroundResult {
        task_id: String,
        label: String,
        summary: String,        // Short (1-3 sentences) — injected as system message
        full_result: String,    // Full output — stored in episodic memory
        success: bool,
        task_description: String,
    },
}
```

Add handler in `SessionActor::run()`:

```rust
// In the tokio::select! loop
Some(ActorMessage::BackgroundResult { task_id, label, summary, full_result, success, task_description }) => {
    // 1. Inject summary into session history as system message
    let status = if success { "completed" } else { "failed" };
    let injection = format!(
        "[Background task \"{label}\" {status}]\n{summary}"
    );
    if let Ok(mut mgr) = self.session_mgr.lock() {
        mgr.add_message(&self.session_key, Message {
            role: MessageRole::System,
            content: injection.clone(),
            timestamp: chrono::Utc::now(),
            ..Default::default()
        });
    }

    // 2. Save full result to episodic memory for retrieval
    // (agent can reference it in future turns without re-injecting)
    if success {
        let episode = Episode {
            task: task_description,
            result: full_result,
            success: true,
            ..Default::default()
        };
        let _ = self.agent.memory().save(episode).await;
    }

    // 3. Notify user via channel
    let notification = if success {
        format!("✅ {label} complete: {summary}")
    } else {
        format!("❌ {label} failed: {summary}")
    };
    let _ = self.out_tx.send(OutboundMessage {
        channel: self.channel.clone(),
        chat_id: self.chat_id.clone(),
        content: notification,
        ..Default::default()
    }).await;
}
```

**File: `crates/crew-agent/src/tools/spawn.rs`**

Change background mode to send `BackgroundResult` instead of `InboundMessage`:

```rust
// Replace the InboundMessage announce with:
let result_msg = match &result {
    Ok(r) => {
        // Generate a short summary (first 500 chars or first paragraph)
        let summary = r.output.lines().take(5).collect::<Vec<_>>().join("\n");
        ActorMessage::BackgroundResult {
            task_id: wid.to_string(),
            label: label.clone(),
            summary,
            full_result: r.output.clone(),
            success: r.success,
            task_description: task_desc.clone(),
        }
    }
    Err(e) => ActorMessage::BackgroundResult {
        task_id: wid.to_string(),
        label: label.clone(),
        summary: format!("Error: {e}"),
        full_result: format!("{e:#}"),
        success: false,
        task_description: task_desc.clone(),
    },
};
```

This requires `SpawnTool` to hold an `mpsc::Sender<ActorMessage>` instead of (or in addition to) `mpsc::Sender<InboundMessage>`. The simplest approach: add an `actor_tx: Option<mpsc::Sender<ActorMessage>>` field. When set (gateway mode), use it. When unset (CLI/test), fall back to the existing `inbound_tx` path.

**Benefit:** No extra LLM call to relay results. Result lands in history, user gets notified, agent has the context for follow-up questions.

**LOC estimate:** ~80 lines changed across 2 files.

---

## Phase 2: Queue Modes

**Goal:** When the agent is busy processing a turn, incoming messages are handled according to a configurable policy instead of just backpressure.

### Current State

`ActorRegistry::dispatch()` in `session_actor.rs:140-158`:

```rust
// Current: try_send, if full → send backpressure message → blocking send
if let Err(TrySendError::Full(_)) = handle.tx.try_send(msg) {
    // Send "⏳ Still processing..." notification
    let _ = self.out_tx.send(OutboundMessage { content: backpressure_msg, ... }).await;
    let _ = handle.tx.send(msg).await;
}
```

### Changes

**File: `crates/crew-core/src/types.rs`** — Add queue mode enum:

```rust
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    /// Latest message replaces queued messages (default). Agent processes
    /// only the most recent message after the current turn completes.
    #[default]
    Steer,
    /// All queued messages are batched into a single combined prompt.
    Collect,
    /// New message cancels the current turn immediately.
    Interrupt,
    /// Queued messages processed sequentially after current turn.
    Followup,
}
```

**File: `crates/crew-cli/src/session_actor.rs`** — Replace backpressure with queue:

```rust
// New field on SessionActor
struct SessionActor {
    // ... existing fields ...
    queue_mode: QueueMode,
    message_queue: Vec<InboundMessage>,  // buffered while busy
    is_processing: bool,
}
```

Change the main `run()` loop:

```rust
loop {
    tokio::select! {
        Some(msg) = self.inbox.recv() => {
            match msg {
                ActorMessage::Inbound { message, image_media } => {
                    if self.is_processing {
                        match self.queue_mode {
                            QueueMode::Interrupt => {
                                // Cancel current run, process new message
                                self.cancelled.store(true, Ordering::SeqCst);
                                self.message_queue.clear();
                                self.message_queue.push(message);
                            }
                            QueueMode::Steer => {
                                // Keep only newest
                                self.message_queue.clear();
                                self.message_queue.push(message);
                            }
                            QueueMode::Collect | QueueMode::Followup => {
                                self.message_queue.push(message);
                            }
                        }
                    } else {
                        self.process_inbound(message, image_media).await;
                    }
                }
                ActorMessage::Cancel => { /* ... */ }
                ActorMessage::BackgroundResult { .. } => { /* Phase 1 handler */ }
            }
        }
        _ = tokio::time::sleep(self.idle_timeout) => break, // idle shutdown
    }
}
```

After `process_inbound()` completes, drain the queue:

```rust
async fn drain_queue(&mut self) {
    while let Some(msg) = self.dequeue_next() {
        self.process_inbound(msg, vec![]).await;
    }
}

fn dequeue_next(&mut self) -> Option<InboundMessage> {
    if self.message_queue.is_empty() {
        return None;
    }
    match self.queue_mode {
        QueueMode::Steer => {
            // Take last, discard rest
            let msg = self.message_queue.pop();
            self.message_queue.clear();
            msg
        }
        QueueMode::Collect => {
            // Batch all into one combined message
            let messages = std::mem::take(&mut self.message_queue);
            if messages.is_empty() { return None; }
            let combined = messages.iter().enumerate()
                .map(|(i, m)| format!("---\nQueued #{}: {}", i + 1, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            let mut base = messages.into_iter().last().unwrap();
            base.content = format!("[Queued messages while agent was busy]\n{combined}");
            Some(base)
        }
        QueueMode::Followup => {
            // Process one at a time (FIFO)
            if self.message_queue.is_empty() { None }
            else { Some(self.message_queue.remove(0)) }
        }
        QueueMode::Interrupt => {
            // Should have been handled inline; drain normally
            if self.message_queue.is_empty() { None }
            else { Some(self.message_queue.remove(0)) }
        }
    }
}
```

**LOC estimate:** ~120 lines across 2 files.

---

## Phase 3: Cancel with Multilingual Triggers

**Goal:** User can cancel any running operation by sending a trigger word.

### Changes

**File: `crates/crew-core/src/abort.rs`** (new, ~40 LOC):

```rust
/// Check if a message is an abort trigger.
pub fn is_abort_trigger(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    ABORT_TRIGGERS.iter().any(|t| normalized == *t)
}

static ABORT_TRIGGERS: &[&str] = &[
    // English
    "stop", "abort", "cancel", "exit", "halt", "wait", "interrupt", "quit", "enough",
    // Chinese
    "停", "停止", "取消", "停下", "别说了",
    // Japanese
    "やめて", "止めて", "ストップ",
    // Russian
    "стоп", "отмена", "хватит",
    // French
    "arrête", "stop", "annuler",
    // Spanish
    "para", "detente", "cancelar",
    // Hindi
    "रुको", "बंद करो",
    // Arabic
    "توقف", "قف",
    // Korean
    "멈춰", "중지",
];
```

**File: `crates/crew-cli/src/session_actor.rs`** — Check before queuing:

```rust
ActorMessage::Inbound { message, .. } => {
    if crew_core::abort::is_abort_trigger(&message.content) {
        // Cancel current run
        self.cancelled.store(true, Ordering::SeqCst);
        self.message_queue.clear();
        let _ = self.out_tx.send(OutboundMessage {
            content: "⏹ Cancelled.".into(),
            ..routing_fields()
        }).await;
        continue;
    }
    // ... normal queue/process logic
}
```

**LOC estimate:** ~60 lines across 2 files.

---

## Phase 4: Prompt Changes

**Goal:** Teach the agent about background tasks, queue behavior, and cancellation so it uses them naturally.

### File: `crates/crew-cli/src/prompts/gateway_default.txt`

Current prompt is 45 lines. Add a new section:

```diff
 Only use the `message` tool to send an early heads-up when you need to run slow tools (deep_research, deep_search, deep_crawl, spawn, run_pipeline, take_photo) — NOT for simple questions. Save important user preferences with `save_memory`.
+
+## Background Tasks
+
+For tasks that will take more than ~30 seconds (deep research, large file analysis,
+complex multi-step work), use `spawn` in background mode. This lets the user keep
+chatting while the task runs. The result will be automatically injected into the
+conversation when complete.
+
+Guidelines:
+- ALWAYS use background mode for: deep_research, deep_crawl, multi-file analysis,
+  report generation, any task you estimate will take >30 seconds
+- Send a brief acknowledgement via `message` tool: "Starting [task] in background.
+  You can keep chatting."
+- When a background result arrives (marked [Background task "..." completed]),
+  summarize it naturally for the user. Do not just echo the raw output.
+- If the user asks about a pending background task, tell them it's still running.
+
+## Queue Behavior
+
+If the user sends messages while you're processing, they are queued and delivered
+after your current turn completes. In "collect" mode, multiple queued messages are
+batched together — address all of them in your response.
+
+## Cancellation
+
+The user can cancel your current operation by saying "stop", "cancel", "abort",
+or equivalent words in other languages. When cancelled, your current turn ends
+immediately. Do not apologize excessively — just acknowledge and move on.
```

### File: `crates/crew-agent/src/prompts/worker.txt`

Add awareness that background workers should be concise and structured:

```diff
 Guidelines:
 - Make minimal, focused changes
 - Verify your work before completing
 - Report any blockers or uncertainties
 - Keep code simple and readable
 - Respond in plain text only — no markdown formatting (no **, ##, -, ```, tables, etc.)
 - When the user shares preferences, personal info, or important project facts, proactively save them to the memory bank using `save_memory`
+- Structure your output for injection: start with a 1-3 sentence summary,
+  then provide details. Your output will be injected into the main conversation.
+- Be thorough but concise — your result should stand alone without requiring
+  the parent agent to ask follow-up questions.
```

### File: `crates/crew-cli/src/prompts/admin_default.txt`

No changes needed — admin prompt is for profile management, not user-facing chat.

### Runtime prompt injection (prompt_layer.rs)

Add queue mode context as a runtime layer in `SessionActor::process_inbound()`:

```rust
// Before calling agent.process_message(), inject runtime context
let queue_context = match self.queue_mode {
    QueueMode::Collect => "\n\n## Runtime Context\nQueue mode: collect. If you see [Queued messages while agent was busy], address ALL queued messages in a single response.",
    QueueMode::Steer => "",  // no special instruction needed
    QueueMode::Interrupt => "",
    QueueMode::Followup => "",
};
// Append to system prompt if non-empty
```

This is lightweight — a one-line append to the system prompt only when collect mode is active and there are batched messages.

**LOC estimate:** ~40 lines of prompt text, ~10 lines of runtime injection code.

---

## Phase 5: Responsiveness Observer & Auto Circuit Breaker

**Goal:** Detect when the current LLM provider is degraded and automatically activate failover.

### File: `crates/crew-llm/src/responsiveness.rs` (new, ~100 LOC)

```rust
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct ResponsivenessObserver {
    window: VecDeque<Duration>,
    window_size: usize,
    baseline: Option<Duration>,
    baseline_samples: usize,
    degradation_threshold: f64,
    consecutive_slow: u32,
    slow_trigger: u32,
    active: bool,
}

impl ResponsivenessObserver {
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(20),
            window_size: 20,
            baseline: None,
            baseline_samples: 5,
            degradation_threshold: 5.0,
            consecutive_slow: 0,
            slow_trigger: 3,
            active: false,
        }
    }

    pub fn record(&mut self, latency: Duration) {
        self.window.push_back(latency);
        if self.window.len() > self.window_size {
            self.window.pop_front();
        }

        if self.baseline.is_none() && self.window.len() >= self.baseline_samples {
            let sum: Duration = self.window.iter().sum();
            self.baseline = Some(sum / self.window.len() as u32);
        }

        if let Some(baseline) = self.baseline {
            if latency > baseline.mul_f64(self.degradation_threshold) {
                self.consecutive_slow += 1;
            } else {
                self.consecutive_slow = 0;
            }
        }
    }

    pub fn should_activate(&self) -> bool {
        !self.active && self.consecutive_slow >= self.slow_trigger
    }

    pub fn should_deactivate(&self) -> bool {
        self.active && self.consecutive_slow == 0
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }
}
```

### Integration point: `crates/crew-llm/src/adaptive.rs`

The `AdaptiveRouter` already tracks latency EMA and has circuit breaker logic. Add:

```rust
impl AdaptiveRouter {
    // Existing: select_provider(), record_result(), etc.

    // NEW: called after each LLM response
    pub fn observe_responsiveness(&mut self, provider_idx: usize, latency: Duration) {
        // Feed into ResponsivenessObserver
        if self.observer.should_activate() {
            // Enable circuit breaker on degraded provider
            self.providers[provider_idx].circuit_open = true;
            self.observer.set_active(true);
            tracing::warn!(
                provider = self.providers[provider_idx].name,
                "auto-enabled circuit breaker due to sustained high latency"
            );
        }
        if self.observer.should_deactivate() {
            // Reset — provider recovered
            self.observer.set_active(false);
            tracing::info!("responsiveness recovered, deactivating auto-protection");
        }
    }
}
```

**LOC estimate:** ~100 lines new file + ~30 lines integration.

---

## Phase 6: Adaptive Lane Changing

**Goal:** When circuit breaker trips (manually or auto), transparently switch to the next best provider.

### Current State

`AdaptiveRouter` already scores providers and selects the best one. Circuit breaker skips unhealthy providers. But it doesn't notify the user or session about the switch.

### Changes

**File: `crates/crew-llm/src/adaptive.rs`**

Add a callback/event when provider switches:

```rust
pub struct ProviderSwitch {
    pub from: String,      // provider name
    pub to: String,        // provider name
    pub reason: SwitchReason,
}

pub enum SwitchReason {
    CircuitBreaker,
    HighLatency,
    ErrorBurst,
    UserRequested,
}

impl AdaptiveRouter {
    // Existing select_provider() already does the right thing —
    // it picks the lowest-score provider, skipping circuit-broken ones.
    // We just need to track what changed.

    pub fn select_provider_with_tracking(&self) -> (usize, Option<ProviderSwitch>) {
        let selected = self.select_provider();
        let switch = if selected != self.last_selected {
            Some(ProviderSwitch {
                from: self.providers[self.last_selected].name.clone(),
                to: self.providers[selected].name.clone(),
                reason: if self.providers[self.last_selected].circuit_open {
                    SwitchReason::CircuitBreaker
                } else {
                    SwitchReason::HighLatency
                },
            })
        } else {
            None
        };
        (selected, switch)
    }
}
```

**File: `crates/crew-agent/src/agent.rs`**

In `call_llm_with_hooks()`, after a successful call, if the provider wrapper reports a switch, inject a brief system message:

```rust
if let Some(switch) = provider_switch {
    // Inject a system-level note so the agent knows context
    messages.push(Message {
        role: MessageRole::System,
        content: format!(
            "[Provider switched from {} to {} due to {}]",
            switch.from, switch.to, switch.reason
        ),
        ..Default::default()
    });
}
```

The agent sees this as context and can optionally mention it to the user. No extra LLM call needed — it's just a message in the history.

**LOC estimate:** ~60 lines across 2 files.

---

## Phase 7: Long-Running Skill Status Emission

**Goal:** Skills like `deep_research` emit progress updates that reach the user during execution.

### Current State

`deep_research` spawns via `SpawnTool` in background mode. No progress updates during execution — user sees nothing until completion.

### Changes

**File: `crates/crew-agent/src/tools/mod.rs`** — Extend Tool trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn tags(&self) -> &[&str] { &[] }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult>;

    // NEW: optional progress channel for long-running tools
    async fn execute_with_progress(
        &self,
        args: &serde_json::Value,
        progress: tokio::sync::mpsc::Sender<ToolProgress>,
    ) -> Result<ToolResult> {
        // Default: ignore progress channel, call regular execute
        let _ = progress;
        self.execute(args).await
    }
}

pub enum ToolProgress {
    Status(String),     // "Phase 2/4: Analyzing sources..."
    Percent(u8),        // 0-100
    Intermediate(String), // Partial result available
}
```

**File: `crates/crew-agent/src/agent.rs`** — In tool execution, wire progress to reporter:

```rust
// In handle_tool_use(), for each tool call:
let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(16);

// Spawn progress forwarder
let reporter = self.reporter.clone();
let tool_name = tool_name.clone();
tokio::spawn(async move {
    while let Some(progress) = progress_rx.recv().await {
        match progress {
            ToolProgress::Status(msg) => {
                reporter.tool_progress(&tool_name, &msg).await;
            }
            ToolProgress::Percent(pct) => {
                reporter.tool_progress(&tool_name, &format!("{pct}%")).await;
            }
            ToolProgress::Intermediate(partial) => {
                reporter.tool_progress(&tool_name, &format!("Partial: {partial}")).await;
            }
        }
    }
});

let result = tool.execute_with_progress(args, progress_tx).await;
```

**File: `crates/crew-cli/src/session_actor.rs`** — `ChannelStreamReporter` handles `tool_progress`:

The reporter already supports streaming text deltas to the channel via message editing. Add a `tool_progress()` method that updates the streaming message with the current tool status:

```rust
impl ProgressReporter for ChannelStreamReporter {
    // ... existing methods ...

    async fn tool_progress(&self, tool_name: &str, status: &str) {
        // Update the current streaming message (or send a new ephemeral update)
        let update = format!("⏳ {tool_name}: {status}");
        // Use edit_message if we have a message ID, otherwise send typing indicator
        if let Some(msg_id) = &self.current_message_id {
            let _ = self.channel.edit_message(&self.chat_id, msg_id, &update).await;
        }
    }
}
```

Skills that support progress (e.g., `deep_research`) implement `execute_with_progress`:

```rust
async fn execute_with_progress(
    &self,
    args: &serde_json::Value,
    progress: Sender<ToolProgress>,
) -> Result<ToolResult> {
    let _ = progress.send(ToolProgress::Status("Generating search angles...".into())).await;
    let angles = self.generate_angles(query).await?;

    let _ = progress.send(ToolProgress::Status(
        format!("Searching {} angles in parallel...", angles.len())
    )).await;
    let results = self.search_all(angles).await?;

    let _ = progress.send(ToolProgress::Percent(60)).await;
    let _ = progress.send(ToolProgress::Status("Synthesizing findings...".into())).await;
    let report = self.synthesize(results).await?;

    let _ = progress.send(ToolProgress::Percent(100)).await;
    Ok(ToolResult { output: report, success: true, ..Default::default() })
}
```

**LOC estimate:** ~80 lines for trait extension + reporter, then per-skill integration varies.

---

## Phase 8: Runtime Config Toggles (Chat Commands + Dashboard + Config File)

**Goal:** Users can turn adaptive circuit breaker, QoS ranking, and lane changing on/off via chat commands, the dashboard API, and the config file — all without restarting the gateway.

### Current State

- `AdaptiveRoutingConfig` exists in `config.rs` but is **not** runtime-settable (requires gateway restart)
- Hot-reload only covers `system_prompt` and `max_history` (via `ConfigWatcher`)
- `GatewayConfig` already has `queue_mode` field
- Chat commands are dispatched in `gateway/mod.rs` lines 1327-1526 (pattern: match on `/command` prefix)
- Dashboard API lives at `/api/my/profile` and `/api/admin/profiles/{id}`
- `SwappableProvider` wraps the LLM with `RwLock` for runtime swaps

### 8.1 Config Schema Changes

**File: `crates/crew-cli/src/config.rs`**

Extend `AdaptiveRoutingConfig` with the new toggles:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdaptiveRoutingConfig {
    // Existing fields...
    pub enabled: bool,
    pub latency_threshold_ms: u64,
    pub error_rate_threshold: f64,
    pub probe_probability: f64,
    pub probe_interval_secs: u64,
    pub failure_threshold: u32,

    // NEW: per-feature toggles
    /// Auto-enable circuit breaker when provider latency degrades.
    /// When off, circuit breaker only trips on consecutive hard errors.
    #[serde(default = "default_true")]
    pub auto_circuit_breaker: bool,

    /// Adaptive lane changing: automatically switch to a faster provider
    /// when the current one degrades. Requires multiple providers configured.
    #[serde(default = "default_true")]
    pub lane_changing: bool,

    /// QoS quality ranking: factor response quality (not just latency) into
    /// provider selection. Requires quality signal collection.
    #[serde(default)]
    pub qos_ranking: bool,
}

fn default_true() -> bool { true }
```

Extend `GatewayConfig` to include queue mode (already exists) and new fields:

```rust
pub struct GatewayConfig {
    // Existing fields...
    pub queue_mode: QueueMode,

    // NEW: user-visible adaptive features (override adaptive_routing per-gateway)
    /// Allow users to toggle adaptive features via chat commands.
    #[serde(default = "default_true")]
    pub allow_adaptive_commands: bool,
}
```

**Config file example (`.crew/config.json`):**

```json
{
    "adaptive_routing": {
        "enabled": true,
        "auto_circuit_breaker": true,
        "lane_changing": true,
        "qos_ranking": false,
        "latency_threshold_ms": 30000,
        "failure_threshold": 3,
        "probe_probability": 0.1
    },
    "gateway": {
        "queue_mode": "collect",
        "allow_adaptive_commands": true
    }
}
```

### 8.2 Runtime State Store

**File: `crates/crew-llm/src/adaptive.rs`**

The `AdaptiveRouter` already holds provider state behind internal mutability. Add runtime toggle flags:

```rust
pub struct AdaptiveRouter {
    // Existing fields...
    providers: Vec<ProviderSlot>,
    config: AdaptiveConfig,
    rng: Mutex<SmallRng>,

    // NEW: runtime-toggleable flags (AtomicBool for lock-free reads)
    auto_circuit_breaker_enabled: AtomicBool,
    lane_changing_enabled: AtomicBool,
    qos_ranking_enabled: AtomicBool,
}

impl AdaptiveRouter {
    // NEW: runtime toggle methods
    pub fn set_auto_circuit_breaker(&self, enabled: bool) {
        self.auto_circuit_breaker_enabled.store(enabled, Ordering::Release);
        tracing::info!(enabled, "auto circuit breaker toggled");
    }

    pub fn set_lane_changing(&self, enabled: bool) {
        self.lane_changing_enabled.store(enabled, Ordering::Release);
        tracing::info!(enabled, "lane changing toggled");
    }

    pub fn set_qos_ranking(&self, enabled: bool) {
        self.qos_ranking_enabled.store(enabled, Ordering::Release);
        tracing::info!(enabled, "QoS ranking toggled");
    }

    pub fn adaptive_status(&self) -> AdaptiveStatus {
        AdaptiveStatus {
            auto_circuit_breaker: self.auto_circuit_breaker_enabled.load(Ordering::Acquire),
            lane_changing: self.lane_changing_enabled.load(Ordering::Acquire),
            qos_ranking: self.qos_ranking_enabled.load(Ordering::Acquire),
            active_provider: self.current_provider_name(),
            providers: self.provider_summaries(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AdaptiveStatus {
    pub auto_circuit_breaker: bool,
    pub lane_changing: bool,
    pub qos_ranking: bool,
    pub active_provider: String,
    pub providers: Vec<ProviderSummary>,
}

#[derive(Debug, Serialize)]
pub struct ProviderSummary {
    pub name: String,
    pub model_id: String,
    pub latency_ms: f64,
    pub error_rate: f64,
    pub circuit_open: bool,
    pub is_active: bool,
}
```

Guard the new behaviors behind these flags:

```rust
// In observe_responsiveness():
if !self.auto_circuit_breaker_enabled.load(Ordering::Acquire) {
    return; // Skip auto circuit breaker
}

// In select_provider():
if !self.lane_changing_enabled.load(Ordering::Acquire) {
    // Stick with current provider even if degraded (unless hard circuit break)
    return self.last_selected;
}

// In compute_score():
if self.qos_ranking_enabled.load(Ordering::Acquire) {
    score += weight_quality * quality_penalty; // Include quality signal
}
```

### 8.3 Chat Commands

**File: `crates/crew-cli/src/commands/gateway/mod.rs`**

Add new commands in the command dispatch section (alongside `/new`, `/s`, `/config`):

```rust
// In the main command dispatch (line ~1327)
} else if content == "/adaptive" || content == "/adaptive status" {
    // Show current adaptive routing status
    if let Some(router) = &adaptive_router {
        let status = router.adaptive_status();
        let mut lines = vec![
            format!("**Adaptive Routing**"),
            format!("Auto circuit breaker: {}", if status.auto_circuit_breaker { "on" } else { "off" }),
            format!("Lane changing: {}", if status.lane_changing { "on" } else { "off" }),
            format!("QoS ranking: {}", if status.qos_ranking { "on" } else { "off" }),
            format!(""),
            format!("**Providers:**"),
        ];
        for p in &status.providers {
            let marker = if p.is_active { "▸ " } else { "  " };
            let circuit = if p.circuit_open { " ⚠ OPEN" } else { "" };
            lines.push(format!(
                "{marker}{} ({}) — {:.0}ms, {:.1}% errors{circuit}",
                p.name, p.model_id, p.latency_ms, p.error_rate * 100.0
            ));
        }
        let reply = lines.join("\n");
        let _ = out_tx.send(OutboundMessage { content: reply, ..routing() }).await;
    } else {
        let _ = out_tx.send(OutboundMessage {
            content: "Adaptive routing not available (single provider configured).".into(),
            ..routing()
        }).await;
    }
    continue;

} else if let Some(feature) = content.strip_prefix("/adaptive ") {
    if !gateway_config.allow_adaptive_commands {
        let _ = out_tx.send(OutboundMessage {
            content: "Adaptive commands are disabled in config.".into(),
            ..routing()
        }).await;
        continue;
    }

    let parts: Vec<&str> = feature.trim().splitn(2, ' ').collect();
    if let Some(router) = &adaptive_router {
        match parts.as_slice() {
            ["circuit", "on"] | ["circuit-breaker", "on"] | ["cb", "on"] => {
                router.set_auto_circuit_breaker(true);
                let _ = out_tx.send(OutboundMessage {
                    content: "Auto circuit breaker: on".into(), ..routing()
                }).await;
            }
            ["circuit", "off"] | ["circuit-breaker", "off"] | ["cb", "off"] => {
                router.set_auto_circuit_breaker(false);
                let _ = out_tx.send(OutboundMessage {
                    content: "Auto circuit breaker: off".into(), ..routing()
                }).await;
            }
            ["lane", "on"] | ["lane-changing", "on"] | ["lc", "on"] => {
                router.set_lane_changing(true);
                let _ = out_tx.send(OutboundMessage {
                    content: "Lane changing: on".into(), ..routing()
                }).await;
            }
            ["lane", "off"] | ["lane-changing", "off"] | ["lc", "off"] => {
                router.set_lane_changing(false);
                let _ = out_tx.send(OutboundMessage {
                    content: "Lane changing: off".into(), ..routing()
                }).await;
            }
            ["qos", "on"] | ["ranking", "on"] => {
                router.set_qos_ranking(true);
                let _ = out_tx.send(OutboundMessage {
                    content: "QoS ranking: on".into(), ..routing()
                }).await;
            }
            ["qos", "off"] | ["ranking", "off"] => {
                router.set_qos_ranking(false);
                let _ = out_tx.send(OutboundMessage {
                    content: "QoS ranking: off".into(), ..routing()
                }).await;
            }
            _ => {
                let _ = out_tx.send(OutboundMessage {
                    content: "Usage: /adaptive [circuit|lane|qos] [on|off]\n/adaptive — show status".into(),
                    ..routing()
                }).await;
            }
        }
    }
    continue;

} else if let Some(mode) = content.strip_prefix("/queue ") {
    // Change queue mode at runtime
    match mode.trim() {
        "steer" => { queue_mode.store(QueueMode::Steer); reply("Queue mode: steer"); }
        "collect" => { queue_mode.store(QueueMode::Collect); reply("Queue mode: collect"); }
        "interrupt" => { queue_mode.store(QueueMode::Interrupt); reply("Queue mode: interrupt"); }
        "followup" => { queue_mode.store(QueueMode::Followup); reply("Queue mode: followup"); }
        _ => reply("Usage: /queue [steer|collect|interrupt|followup]");
    }
    continue;
```

**Chat command summary:**

| Command | Effect |
|---------|--------|
| `/adaptive` | Show status: all toggles + provider metrics |
| `/adaptive circuit on/off` | Toggle auto circuit breaker |
| `/adaptive lane on/off` | Toggle adaptive lane changing |
| `/adaptive qos on/off` | Toggle QoS quality ranking |
| `/queue steer/collect/interrupt/followup` | Change queue mode |

Aliases: `cb` for circuit-breaker, `lc` for lane-changing, `ranking` for qos.

### 8.4 Dashboard API Endpoints

**File: `crates/crew-cli/src/api/router.rs`**

Add routes under the existing `/api/my/profile` namespace:

```rust
// GET /api/my/profile/adaptive — read adaptive status
// PUT /api/my/profile/adaptive — update adaptive toggles
// GET /api/my/profile/queue — read queue mode
// PUT /api/my/profile/queue — update queue mode

.route("/api/my/profile/adaptive", get(get_adaptive_status).put(put_adaptive_settings))
.route("/api/my/profile/queue", get(get_queue_mode).put(put_queue_mode))

// Admin equivalents
.route("/api/admin/profiles/:id/adaptive", get(admin_get_adaptive).put(admin_put_adaptive))
.route("/api/admin/profiles/:id/queue", get(admin_get_queue).put(admin_put_queue))
```

**Handler implementations:**

```rust
/// GET /api/my/profile/adaptive
async fn get_adaptive_status(
    State(state): State<AppState>,
) -> impl IntoResponse {
    match &state.adaptive_router {
        Some(router) => Json(router.adaptive_status()).into_response(),
        None => (StatusCode::NOT_FOUND, "Adaptive routing not configured").into_response(),
    }
}

/// PUT /api/my/profile/adaptive
/// Body: { "auto_circuit_breaker": true, "lane_changing": false, "qos_ranking": false }
async fn put_adaptive_settings(
    State(state): State<AppState>,
    Json(body): Json<AdaptiveSettingsUpdate>,
) -> impl IntoResponse {
    match &state.adaptive_router {
        Some(router) => {
            if let Some(v) = body.auto_circuit_breaker {
                router.set_auto_circuit_breaker(v);
            }
            if let Some(v) = body.lane_changing {
                router.set_lane_changing(v);
            }
            if let Some(v) = body.qos_ranking {
                router.set_qos_ranking(v);
            }
            Json(router.adaptive_status()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Adaptive routing not configured").into_response(),
    }
}

#[derive(Deserialize)]
struct AdaptiveSettingsUpdate {
    auto_circuit_breaker: Option<bool>,
    lane_changing: Option<bool>,
    qos_ranking: Option<bool>,
}

/// GET /api/my/profile/queue
async fn get_queue_mode(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "queue_mode": state.queue_mode.load(),
    }))
}

/// PUT /api/my/profile/queue
/// Body: { "queue_mode": "collect" }
async fn put_queue_mode(
    State(state): State<AppState>,
    Json(body): Json<QueueModeUpdate>,
) -> impl IntoResponse {
    state.queue_mode.store(body.queue_mode);
    Json(serde_json::json!({ "queue_mode": body.queue_mode }))
}

#[derive(Deserialize)]
struct QueueModeUpdate {
    queue_mode: QueueMode,
}
```

### 8.5 Hot-Reload from Config File

**File: `crates/crew-cli/src/commands/gateway/mod.rs`** — Extend `ConfigChange`:

Currently, `ConfigWatcher` emits `ConfigChange::HotReload { system_prompt, max_history }`. Extend:

```rust
pub enum ConfigChange {
    HotReload {
        system_prompt: Option<String>,
        max_history: Option<usize>,
        // NEW
        queue_mode: Option<QueueMode>,
        adaptive: Option<AdaptiveHotReload>,
    },
    RestartRequired(String),
}

pub struct AdaptiveHotReload {
    pub auto_circuit_breaker: Option<bool>,
    pub lane_changing: Option<bool>,
    pub qos_ranking: Option<bool>,
}
```

In the hot-reload handler:

```rust
if let Some(change) = config_rx.borrow_and_update().clone() {
    if let ConfigChange::HotReload { system_prompt, max_history, queue_mode, adaptive } = change {
        // Existing: update system_prompt, max_history
        // ...

        // NEW: update queue mode
        if let Some(qm) = queue_mode {
            queue_mode_shared.store(qm);
            tracing::info!(?qm, "hot-reloaded queue mode");
        }

        // NEW: update adaptive toggles
        if let (Some(adaptive), Some(router)) = (adaptive, &adaptive_router) {
            if let Some(v) = adaptive.auto_circuit_breaker {
                router.set_auto_circuit_breaker(v);
            }
            if let Some(v) = adaptive.lane_changing {
                router.set_lane_changing(v);
            }
            if let Some(v) = adaptive.qos_ranking {
                router.set_qos_ranking(v);
            }
            tracing::info!("hot-reloaded adaptive routing settings");
        }
    }
}
```

### 8.6 Shared Runtime State

The `AdaptiveRouter` is already behind `Arc` (created once, shared across actors). The `AtomicBool` flags allow lock-free reads from any task. No new synchronization needed.

For `queue_mode`, add a shared atomic wrapper since `QueueMode` is a small enum:

```rust
// In session_actor.rs or a shared state module
pub struct SharedQueueMode(AtomicU8);

impl SharedQueueMode {
    pub fn new(mode: QueueMode) -> Self {
        Self(AtomicU8::new(mode as u8))
    }
    pub fn load(&self) -> QueueMode {
        match self.0.load(Ordering::Acquire) {
            0 => QueueMode::Steer,
            1 => QueueMode::Collect,
            2 => QueueMode::Interrupt,
            3 => QueueMode::Followup,
            _ => QueueMode::Steer,
        }
    }
    pub fn store(&self, mode: QueueMode) {
        self.0.store(mode as u8, Ordering::Release);
    }
}
```

This gets shared as `Arc<SharedQueueMode>` in `ActorFactory` and `AppState`, readable by all session actors without locking.

### 8.7 Prompt Changes for Adaptive Commands

**File: `crates/crew-cli/src/prompts/gateway_default.txt`** — Append to the prompt additions from Phase 4:

```diff
+## Adaptive Features
+
+The user can control provider routing with these commands:
+- `/adaptive` — show adaptive routing status (provider latency, error rates, which is active)
+- `/adaptive circuit on/off` — toggle auto circuit breaker
+- `/adaptive lane on/off` — toggle adaptive lane changing
+- `/adaptive qos on/off` — toggle QoS quality ranking
+- `/queue steer|collect|interrupt|followup` — change message queue mode
+
+These are handled by the system — do not try to execute them as tools.
+If the user asks about provider performance or wants to switch providers,
+suggest they use `/adaptive` to check status.
```

**LOC estimate:** ~200 lines across config.rs, adaptive.rs, gateway/mod.rs, router.rs, session_actor.rs.

---

## Summary

| Phase | What | Files Changed | New Files | LOC | Dependencies |
|-------|------|--------------|-----------|-----|-------------|
| 1 | Background result injection | `session_actor.rs`, `spawn.rs` | — | ~80 | None |
| 2 | Queue modes | `session_actor.rs`, `types.rs` | — | ~120 | None |
| 3 | Multilingual cancel | `session_actor.rs` | `abort.rs` | ~60 | Phase 2 |
| 4 | Prompt changes | `gateway_default.txt`, `worker.txt`, `session_actor.rs` | — | ~50 | Phase 1-3 |
| 5 | Responsiveness observer | `adaptive.rs` | `responsiveness.rs` | ~130 | None |
| 6 | Lane change tracking | `adaptive.rs`, `agent.rs` | — | ~60 | Phase 5 |
| 7 | Skill status emission | `tools/mod.rs`, `agent.rs`, `session_actor.rs` | — | ~80 | None |
| 8 | Runtime config toggles | `config.rs`, `adaptive.rs`, `gateway/mod.rs`, `router.rs`, `session_actor.rs` | — | ~200 | Phase 5-6 |
| **Total** | | **12 files** | **2 files** | **~780** | |

### Dependency Graph

```
Phase 1 (result injection) ──┐
Phase 2 (queue modes) ───────┤
Phase 3 (cancel) ────────────┼── Phase 4 (prompts)
                              │
Phase 5 (responsiveness) ─────── Phase 6 (lane changing) ──┐
                              │                            ├── Phase 8 (runtime config)
Phase 7 (status emission) ────┘                            │
Phase 2 (queue modes) ────────────────────────────────────┘
```

Phases 1, 2, 5, and 7 are independent — can be developed in parallel.
Phase 3 depends on Phase 2 (needs the queue to clear on cancel).
Phase 4 depends on 1-3 (prompts reference all new behaviors).
Phase 6 depends on Phase 5 (lane changing triggered by responsiveness observer).
Phase 8 depends on Phase 5-6 (toggles control adaptive features) + Phase 2 (queue mode toggle).

### Prompt Change Summary

| File | Change | Purpose |
|------|--------|---------|
| `gateway_default.txt` | +25 lines: "Background Tasks", "Queue Behavior", "Cancellation" sections | Teach agent to use `spawn` for >30s tasks, handle batched messages, acknowledge cancel |
| `gateway_default.txt` | +10 lines: "Adaptive Features" section | Document `/adaptive` and `/queue` commands so agent doesn't try to handle them |
| `worker.txt` | +3 lines: output structure guidance | Workers produce summary-first output for clean injection |
| Runtime injection | +1-2 lines when collect mode active | Tell agent to address all queued messages |
| System message injection | Auto-injected on provider switch | Agent knows context changed (optional mention to user) |

### What This Does NOT Include

- **Edit-in-place streaming** — Requires Channel Adapter Pattern refactor (separate effort, see `CHANNEL_ADAPTER_PATTERN.md`)
- **LLM quality ranking** — Requires quality signal collection infrastructure (P2, high effort)
- **Three-way context merge** — Not needed (see earlier analysis)
- **Context forking** — Not needed (background tasks use isolated `run_task()`, results injected as system messages)

---

## Related Documents

- [CREW_UX_VISION.md](./CREW_UX_VISION.md) — Overall UX vision
- [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) — Cross-pollination analysis
- [CHANNEL_ADAPTER_PATTERN.md](./CHANNEL_ADAPTER_PATTERN.md) — Channel adapter refactor (enables edit-in-place streaming)
- [PROVIDER_RACING.md](./PROVIDER_RACING.md) — Provider racing design (complementary to lane changing)
