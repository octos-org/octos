# Session Actor Architecture

Reference architecture for crew-rs gateway processing model, user isolation, and data protection. Reflects the current implementation.

**Status**: Fully implemented (Phases 1-5)
**Last updated**: 2026-03-10

---

## 1. Processing Model

### 1.1 Runtime Hierarchy

```
crew serve (control plane, OS process)
│
├── Profile "work-bot" ──→ Gateway (OS process 1)
│   │
│   │  Tokio Runtime (multi-threaded, 1 OS thread per CPU core)
│   │  ├── Worker Thread 0 ─── runs ready tasks
│   │  ├── Worker Thread 1 ─── runs ready tasks
│   │  ├── Worker Thread 2 ─── runs ready tasks
│   │  └── Worker Thread 3 ─── runs ready tasks
│   │
│   │  Tasks (green threads, ~2KB each, work-stolen across workers):
│   │  ├── SessionActor: telegram:alice         ← tokio::spawn
│   │  ├── SessionActor: telegram:bob           ← tokio::spawn
│   │  ├── SessionActor: whatsapp:charlie       ← tokio::spawn
│   │  ├── overflow task (alice, msg2)           ← tokio::spawn
│   │  ├── stream forwarder (alice)             ← tokio::spawn
│   │  ├── outbound forwarder (bob)             ← tokio::spawn
│   │  └── ... hundreds more
│   │
│   └── Shared (read-only / thread-safe):
│       ├── Arc<Agent> (LLM provider, system prompt)
│       ├── Arc<EpisodeStore> (redb, per-profile memory)
│       └── Arc<ToolRegistryFactory> (cloned per actor)
│
├── Profile "personal-bot" ──→ Gateway (OS process 2)
│   └── ... independent runtime, memory, files
│
└── Profile "sub-account" ──→ Gateway (OS process 3)
    └── ... inherits parent config, but fully isolated at runtime
```

### 1.2 Isolation Levels

| Level | Mechanism | Boundary | What's isolated |
|-------|-----------|----------|-----------------|
| Profile ↔ Profile | OS process | Kernel | Memory, files, API keys, crashes, env vars |
| User ↔ User | SessionActor + SessionHandle | Per-user directory | Session history, active topic, tool context |
| Session ↔ Session | SessionHandle (per-JSONL file) | File | Message history, compaction state |
| Overflow ↔ Overflow | tokio::spawn + Arc<Mutex<SessionHandle>> | Per-actor mutex | Concurrent agent calls within one session |

### 1.3 Why Tokio Tasks, Not OS Processes Per User

| | OS Process per user | Tokio task per user |
|---|---|---|
| Context switch | ~1-10μs | ~50ns |
| Memory per unit | ~10MB | ~2KB (+ ~100KB session data) |
| Max practical count | ~1K | ~100K+ |
| CPU utilization | 1 core per process | All cores via work-stealing |
| Isolation | Full kernel isolation | Shared memory (Arc), data isolation via SessionHandle |
| Failure blast radius | Process crash = 1 user | Panic in task = 1 user (caught by tokio::spawn) |

Tokio's multi-threaded runtime spawns one OS thread per CPU core. Tasks are automatically work-stolen across all cores. When a SessionActor awaits I/O (LLM HTTP response, disk write), the worker thread immediately picks up another task. CPU-bound work (JSON serialization, token counting) runs on the same pool.

**Blocking I/O caveat**: `std::fs::read` blocks the OS thread. SessionHandle uses `tokio::task::spawn_blocking()` for all disk I/O, which runs on a separate expandable thread pool so main workers stay free.

---

## 2. SessionActor

### 2.1 Structure

```rust
struct SessionActor {
    session_key: SessionKey,        // e.g., "telegram:12345#research"
    channel: String,                // e.g., "telegram"
    chat_id: String,                // e.g., "12345"
    inbox: mpsc::Receiver<ActorMessage>,

    agent: Arc<Agent>,              // shared LLM provider, system prompt
    session_handle: Arc<Mutex<SessionHandle>>,  // per-actor, NOT shared
    llm_for_compaction: Arc<dyn LlmProvider>,

    out_tx: mpsc::Sender<OutboundMessage>,
    semaphore: Arc<Semaphore>,      // global concurrency limit
    idle_timeout: Duration,         // 30 min default
    session_timeout: Duration,      // per-message processing timeout
    queue_mode: QueueMode,          // Followup | Collect | Steer | Speculative
    active_overflow_tasks: Arc<AtomicU32>,  // per-session overflow counter
    // ...
}
```

### 2.2 Lifecycle

```
ActorRegistry::dispatch(inbound, session_key)
    │
    ├── Actor exists? ──→ send to inbox
    │
    └── Create new actor:
        1. ActorFactory::spawn(session_key, channel, chat_id)
        2. Create SessionHandle::open(data_dir, &session_key)
           └── Loads from disk or creates empty session
        3. Clone ToolRegistry (per-actor tools)
        4. Create per-session MessageTool, SendFileTool, SpawnTool
        5. tokio::spawn(actor.run())

Actor run loop:
    loop {
        select! {
            msg = inbox.recv() => {
                match msg {
                    Inbound { message, media } => process(message, media),
                    BackgroundResult { label, content } => inject_to_history(),
                    Cancel => set cancelled flag,
                    None => break,  // all senders dropped
                }
            }
            _ = sleep(idle_timeout) => break,  // 30 min idle → shutdown
        }
    }
```

### 2.3 Message Processing Modes

#### Followup (default)
Sequential processing. Each message waits for the previous to complete.

#### Collect
Batch queued messages into one combined prompt when the current LLM call finishes.

#### Steer
Keep only the newest queued message, discard older ones (user changed their mind).

#### Speculative
Concurrent processing. When the primary LLM call exceeds the patience threshold (2× baseline latency, min 10s), overflow messages spawn independent agent tasks:

```
SessionActor: telegram:alice
│
│  Processing msg1 (slow LLM call)...
│  │
│  ├── msg2 arrives, patience exceeded
│  │   └── tokio::spawn(serve_overflow)    ← new task, MAX 5 per session
│  │       ├── Own status indicator ("✦ Thinking...")
│  │       ├── Own stream reporter (separate chat bubble)
│  │       ├── Shares Arc<Mutex<SessionHandle>>
│  │       └── Saves ONLY final reply (avoids tool_call ID collisions)
│  │
│  └── msg1 finishes
│       ├── Saves all messages (user, tool_calls, tool_results, reply)
│       ├── sort_by_timestamp() to restore chronological order
│       └── rewrite() to persist sorted state
```

**Why only save final reply from overflow**: LLM providers generate sequential tool_call IDs (e.g., `deep_search_0`). Concurrent overflow tasks produce duplicate IDs that corrupt session history. Saving only the final assistant reply avoids this.

---

## 3. Data Architecture

### 3.1 Per-User Directory Layout

```
{data_dir}/                              ← per-profile (one OS process)
├── users/
│   ├── telegram%3A12345/                ← alice (per-user directory)
│   │   ├── sessions/
│   │   │   ├── default.jsonl            ← default session
│   │   │   ├── research.jsonl           ← /new research
│   │   │   └── code.jsonl              ← /new code
│   │   └── (future: preferences, quotas)
│   │
│   ├── telegram%3A67890/                ← bob
│   │   └── sessions/
│   │       └── default.jsonl
│   │
│   └── whatsapp%3A99999/                ← charlie
│       └── sessions/
│           └── default.jsonl
│
├── episodes.redb                        ← per-profile (shared agent memory)
├── memory/                              ← per-profile (knowledge bank)
├── active_sessions.json                 ← per-profile (topic tracking)
└── skills/                              ← per-profile (installed plugins)
```

**Why per-user directories**: Enables future filesystem-level isolation (quotas, chroot, sandboxing per user). Each user's data is in one directory that can be independently:
- Quota-limited (filesystem quotas per directory)
- Sandboxed (chroot, bind mount)
- Backed up / migrated / deleted

**Backward compatibility**: `SessionHandle::open()` tries the new per-user path first, falls back to legacy flat layout (`{data_dir}/sessions/{encoded_key}.jsonl`), and migrates automatically (loads from old path, deletes old file, writes to new path on next save).

### 3.2 Session File Format (JSONL)

```
Line 1: {"schema_version":1,"session_key":"telegram:12345","topic":null,"summary":"Hello...","created_at":"...","updated_at":"..."}
Line 2: {"role":"user","content":"hello","media":[],"timestamp":"..."}
Line 3: {"role":"assistant","content":"Hi!","tool_calls":null,"timestamp":"..."}
Line 4: {"role":"user","content":"search for X","timestamp":"..."}
Line 5: {"role":"assistant","content":"","tool_calls":[{"id":"search_0","name":"web_search","arguments":{"query":"X"}}],"timestamp":"..."}
Line 6: {"role":"tool","content":"Results...","tool_call_id":"search_0","timestamp":"..."}
Line 7: {"role":"assistant","content":"I found...","timestamp":"..."}
```

Properties:
- **Append-only** for normal writes (O(1) per message)
- **Atomic rewrite** via write-then-rename for compaction/sort (crash-safe)
- **10 MB size limit** per file (prevents OOM)
- **Schema versioning** for forward compatibility (rejects unknown versions)

### 3.3 Data Ownership

| Data | Scope | Owner | Shared? |
|------|-------|-------|---------|
| Session JSONL files | Per-session | SessionHandle (per-actor mutex) | Only with own overflow tasks |
| User directory | Per-user | SessionActor | No cross-user access |
| EpisodeStore (redb) | Per-profile | All actors (read), agent loop (write) | Yes — thread-safe via redb |
| MemoryStore | Per-profile | All actors | Yes — agent knowledge shared |
| System prompt | Per-profile | Shared via RwLock | Yes — read by all actors |
| ToolRegistry | Per-actor | Cloned at actor creation | No — each actor owns its copy |
| MessageTool / SendFileTool | Per-actor | Wired to specific channel:chat_id | No |
| API keys / env vars | Per-profile | OS process environment | No cross-profile access |

### 3.4 Why EpisodeStore Is Per-Profile (Not Per-User)

EpisodeStore is the agent's **long-term memory** — summaries of completed tasks. It's cross-user by design:
- Agent learns from all interactions within a profile (bot gets smarter)
- User A: "research quantum computing" → stored as episode
- User B: "what do you know about quantum computing?" → agent recalls User A's research

Per-user episodes would silo knowledge and defeat the purpose. Per-profile is the correct boundary — each bot profile is a separate "agent identity" with its own accumulated experience.

---

## 4. Concurrency Control

### 4.1 No Shared Session Mutex

**Before (old design)**:
```
All SessionActors → Arc<Mutex<SessionManager>> → one LRU cache, one lock
    13 lock sites, all competing across users
    5-second timeout, degraded fallbacks on contention
    One user's compaction blocks all other users' reads/writes
```

**After (current design)**:
```
Each SessionActor → own Arc<Mutex<SessionHandle>> → own data, own file
    Per-actor mutex only contested by own overflow tasks
    No timeout needed (instant lock acquisition)
    Zero cross-user contention
```

The shared `SessionManager` still exists for admin operations (`/sessions`, `/new`, `/delete`, `/s preview`) in the gateway main loop. These are infrequent, single-threaded, and don't affect actor performance.

### 4.2 Concurrency Limits

| Constant | Default | Scope | Purpose |
|----------|---------|-------|---------|
| `max_concurrent_sessions` | 10 | Per-profile (Semaphore) | Bounds total active LLM calls |
| `MAX_OVERFLOW_TASKS` | 5 | Per-session (AtomicU32) | Limits concurrent overflow agent tasks |
| `ACTOR_INBOX_SIZE` | 32 | Per-actor (mpsc channel) | Backpressure — full inbox triggers "queued..." |
| `MAX_PENDING_PER_SESSION` | 50 | Per-session (buffer) | Limits buffered messages for inactive sessions |

### 4.3 Backpressure Flow

```
User sends message
    │
    ├── Actor inbox has space → delivered immediately
    │
    ├── Actor inbox full (32 messages) →
    │   ├── Send "⏳ Still processing, your message is queued..." immediately
    │   └── Block until space available (actor finishes current message)
    │
    ├── Overflow limit reached (5 concurrent) →
    │   └── Send "I'm currently handling several tasks. Please wait."
    │
    └── Actor died (panic/OOM) →
        ├── Remove from registry
        ├── Create new actor (loads history from disk)
        └── Deliver message to new actor
```

---

## 5. Profile & Sub-Account Isolation

### 5.1 Profile Hierarchy

```
crew serve
├── Profile "main" (parent)                  ← OS process 1
│   ├── data_dir: ~/.crew/profiles/main/
│   ├── provider: kimi-2.5 (KIMI_API_KEY)
│   ├── channels: [telegram:bot-A, feishu:app-X]
│   └── users: alice, bob, charlie
│
├── Profile "sub-1" (child of main)          ← OS process 2
│   ├── data_dir: ~/.crew/profiles/sub-1/
│   ├── provider: inherits from parent (or overrides)
│   ├── channels: [telegram:bot-B]
│   └── users: dave, eve
│
└── Profile "sub-2" (child of main)          ← OS process 3
    ├── data_dir: ~/.crew/profiles/sub-2/
    └── channels: [whatsapp:bot-C]
```

### 5.2 What `parent_id` Controls

The `parent_id` field on a sub-account profile controls **config inheritance only**:
- Provider, model, base_url fall back to parent if not set
- API key env var falls back to parent
- Fallback models inherited

At runtime, sub-accounts are **fully independent**:
- Separate OS process (crash in sub-1 doesn't affect sub-2)
- Separate data_dir (no shared files)
- Separate EpisodeStore (independent memory)
- Separate session history (independent users)
- Managed by launchd (auto-restart on crash)

### 5.3 User Identity

A "user" is identified by `channel:chat_id`:

```
Profile "work-bot"
├── users/
│   ├── telegram%3A12345/      ← Alice on Telegram
│   ├── whatsapp%3A8613800/    ← Alice on WhatsApp (same person, different user)
│   ├── telegram%3A67890/      ← Bob on Telegram
│   └── feishu%3Aoc_abc123/    ← A Feishu group chat
```

There is no cross-channel identity linking. `telegram:12345` and `whatsapp:8613800` are treated as independent users even if they're the same person. Future enhancement: `/link` command to bind accounts under a unified identity.

---

## 6. Data Protection

### 6.1 Session Data Safety

| Property | Mechanism |
|----------|-----------|
| Crash safety | Atomic write-then-rename for rewrite/compaction |
| Append safety | JSONL append (fsync on close) |
| Size limit | 10 MB per file, reject on load and append |
| Encoding safety | Percent-encoded filenames, FNV-1a hash suffix on truncation |
| Schema versioning | Rejects files with unknown future schema versions |
| User isolation | Per-user directory, no cross-user file access |

### 6.2 Memory Safety (Rust Guarantees)

| Threat | Protection |
|--------|-----------|
| Buffer overflow | Rust ownership model, bounds checking |
| Use-after-free | Borrow checker, Arc reference counting |
| Data race | No shared mutable state without Mutex/RwLock |
| Lock poisoning | `unwrap_or_else(\|e\| e.into_inner())` — recover inner value |

### 6.3 Secrets Protection

| Secret | Storage | Isolation |
|--------|---------|-----------|
| API keys | OS environment variables | Per-profile process (not inherited by children unless configured) |
| Auth tokens | `~/.crew/auth.json` | Per-host (shared) |
| Session content | JSONL files | Per-user directory |
| `BLOCKED_ENV_VARS` (18) | Stripped from: sandbox, MCP, hooks, plugins, browser | All child processes |

### 6.4 Future Filesystem Isolation

The per-user directory structure enables progressive hardening:

**Phase 1** (current): Directory-based isolation. SessionHandle scopes all I/O to the user's directory. No kernel enforcement.

**Phase 2** (planned): Filesystem quotas per user directory. Prevents a single user from filling disk.

**Phase 3** (future): Chroot/bind-mount per user. Shell tool sees only the user's directory + system read-only paths.

**Phase 4** (future): Per-profile UID. Each gateway process runs as a separate Unix user. Kernel-enforced tenant isolation.

---

## 7. QoS Configuration

### 7.1 Current Defaults

| Constant | Value | Location |
|----------|-------|----------|
| `DEFAULT_TOOL_TIMEOUT_SECS` | 600 (10 min) | `crew-agent/src/agent/mod.rs` |
| `MAX_TOOL_TIMEOUT_SECS` | 1800 (30 min) | `crew-agent/src/agent/mod.rs` |
| `DEFAULT_SESSION_TIMEOUT_SECS` | 1800 (30 min) | `crew-agent/src/agent/mod.rs` |
| `DEFAULT_LLM_TIMEOUT_SECS` | 120 (2 min) | `crew-llm/src/provider.rs` |
| `MAX_OVERFLOW_TASKS` | 5 | `crew-cli/src/session_actor.rs` |
| `ACTOR_INBOX_SIZE` | 32 | `crew-cli/src/session_actor.rs` |
| `DEFAULT_IDLE_TIMEOUT_SECS` | 1800 (30 min) | `crew-cli/src/session_actor.rs` |
| `MAX_PENDING_PER_SESSION` | 50 | `crew-cli/src/session_actor.rs` |
| Plugin timeout (default) | 600s | `crew-agent/src/plugins/tool.rs` |
| MCP timeout | 60s | `crew-agent/src/mcp.rs` |

### 7.2 Adaptive QoS

The `ResponsivenessObserver` tracks LLM response latencies per session and detects sustained degradation:

1. **Baseline established** from first 5 responses (exponential moving average)
2. **Degradation detected** when 3+ consecutive responses exceed 3× baseline
3. **Auto-escalation**: Switch to `QueueMode::Speculative` + `AdaptiveMode::Hedge` (race multiple providers)
4. **Recovery**: When latencies return to normal, revert to `QueueMode::Followup` + `AdaptiveMode::Off`

User commands for manual control:
- `/adaptive` — view status
- `/adaptive circuit on|off` — toggle circuit breaker
- `/adaptive lane on|off` — toggle lane changing
- `/adaptive qos on|off` — toggle QoS ranking
- `/queue followup|collect|steer|speculative` — set queue mode
