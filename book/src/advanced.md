# Advanced Features

This chapter covers power-user features: tool management, queue modes, lifecycle hooks, sandboxing, session management, and the web dashboard.

---

## Tools

Octos sends the **full set of enabled tools** to the LLM as callable tool specifications on every turn. There is no recency-based deferral: which tools are available is controlled by [Tool Policies](#tool-policies) (allow/deny lists, named groups) and per-provider policy — not by how recently a tool was used.

Two categories are intentionally kept out of the per-turn tool list:

- **`spawn_only` tools** — background/fire-and-forget skills. They are auto-intercepted and run in a detached task rather than offered as normal callable specs in the main session (they surface to sub-agents).
- **Internal-hidden tools** — dispatcher targets (e.g. `mofa_make`'s per-skill entry points) that a forwarding tool calls internally but that are hidden from the model to keep the surface clean.

> **History:** earlier versions used an LRU-by-recency scheme (≈15 active, the rest deferred and re-promoted via an `activate_tools` meta-tool). This was removed in RFC-0 (#1289): modern LLMs handle the full ~40–50-tool set without quality degradation, and always sending the full list keeps the prompt cache stable and every tool discoverable.

### Tool Configuration

Tools can be configured at runtime using the `/config` slash command. Settings persist in `{data_dir}/tool_config.json`.

| Tool | Setting | Type | Default | Description |
|------|---------|------|---------|-------------|
| `news_digest` | `language` | `"zh"` / `"en"` | `"zh"` | Output language for news digests |
| `news_digest` | `hn_top_stories` | 5-100 | 30 | Hacker News stories to fetch |
| `news_digest` | `max_rss_items` | 5-100 | 30 | Items per RSS feed |
| `news_digest` | `max_deep_fetch_total` | 1-50 | 20 | Total articles to deep-fetch |
| `news_digest` | `max_source_chars` | 1000-50000 | 12000 | Per-source HTML char limit |
| `news_digest` | `max_article_chars` | 1000-50000 | 8000 | Per-article content limit |
| `deep_crawl` | `page_settle_ms` | 500-10000 | 3000 | JS render wait time (ms) |
| `deep_crawl` | `max_output_chars` | 10000-200000 | 50000 | Output truncation limit |
| `web_search` | `count` | 1-10 | 5 | Default number of search results |
| `web_fetch` | `extract_mode` | `"markdown"` / `"text"` | `"markdown"` | Content extraction format |
| `web_fetch` | `max_chars` | 1000-200000 | 50000 | Content size limit |
| `browser` | `action_timeout_secs` | 30-600 | 300 | Per-action timeout |
| `browser` | `idle_timeout_secs` | 60-600 | 300 | Idle session timeout |

**In-chat config commands:**

```
/config                              # Show all tool settings
/config web_search                   # Show web_search settings
/config set web_search.count 10      # Set default result count to 10
/config set news_digest.language en  # Switch news digests to English
/config reset web_search.count       # Reset to default
```

**Priority order** (highest first):
1. Explicit per-call arguments (tool invocation parameters)
2. `/config` overrides (stored in `tool_config.json`)
3. Hardcoded defaults

---

## Tool Policies

Tool policies control which tools the agent can use. They can be set globally, per-provider, or per-context.

### Global Policy

```json
{
  "tool_policy": {
    "allow": ["group:fs", "group:search", "web_search"],
    "deny": ["shell", "spawn"]
  }
}
```

- **`allow`** -- If non-empty, only these tools are permitted. If empty, all tools are allowed.
- **`deny`** -- These tools are always blocked. **Deny wins over allow.**

### Named Groups

| Group | Expands To |
|-------|-----------|
| `group:fs` | `read_file`, `write_file`, `edit_file`, `diff_edit` |
| `group:runtime` | `shell` |
| `group:web` | `web_search`, `web_fetch`, `browser` |
| `group:search` | `glob`, `grep`, `list_dir` |
| `group:sessions` | `spawn` |

Additional tools not in named groups: `send_file`, `switch_model`, `run_pipeline`, `configure_tool`, `cron`, `message`.

### Wildcard Matching

Suffix `*` matches prefixes:

```json
{
  "tool_policy": {
    "deny": ["web_*"]
  }
}
```

This denies `web_search`, `web_fetch`, etc.

### Per-Provider Policies

Different tool sets for different LLM models:

```json
{
  "tool_policy_by_provider": {
    "openai/gpt-4o-mini": {
      "deny": ["shell", "write_file"]
    },
    "gemini": {
      "deny": ["diff_edit"]
    }
  }
}
```

---

## Queue Modes

Queue modes control how incoming user messages are handled while the agent is busy processing a previous request. Set via `/queue <mode>` in chat, or `queue_mode` in profile config.

### Followup (default)

Sequential processing. Each message waits its turn.

- Agent processes A, finishes, processes B, finishes, processes C.
- Simple and predictable.
- The user is blocked until the current request completes.

### Collect

Batch queued messages into a single combined prompt.

- Agent processes A. User sends B, then C.
- When A finishes, B and C are merged into one prompt: `B\n---\nQueued #1: C`
- One LLM call for the batch.
- Good for users who send thoughts in multiple short messages (common in chat apps).

### Steer

Keep only the newest queued message, discard older ones.

- Agent processes A. User sends B, then C.
- When A finishes, B is discarded; only C is processed.
- Good when the user corrects or refines their question mid-flight.
- Example: "search for X" then "actually search for Y" -- only Y is processed.

### Interrupt

Keep only the newest queued message and cancel the running agent.

- Agent processes A. User sends B, then C.
- A is **cancelled**, B is discarded, C is processed immediately.
- Fastest response to course-correction.
- Use when responsiveness matters more than completing the current task.

> **Note:** Currently, Interrupt and Steer share the same drain-and-discard behavior. There is no in-flight agent cancellation — the running agent completes before the newest message is processed. True mid-flight cancellation is planned.

### Speculative

Spawn concurrent overflow agents for each new message while the primary runs.

- Agent processes A. User sends B, then C.
- B and C each get their own concurrent agent task (overflow).
- All three run in parallel -- no blocking.
- Best for slow LLM providers where users do not want to wait.
- Overflow agents use a snapshot of conversation history from before the primary started.

#### How overflow works

1. Primary agent is spawned for the first message.
2. While the primary runs, new messages arrive in the inbox.
3. Each new message triggers `serve_overflow()`, spawning a full agent task with its own streaming bubble.
4. Overflow agents use the history snapshot from before the primary to avoid re-answering the primary question.
5. All agents run concurrently and save results to session history.

#### Known limitations

- **Interactive prompts break in overflow**: If the LLM asks a follow-up question and returns EndTurn, the overflow agent exits. The user's reply spawns a new overflow with no context of the question.
- **Short replies misrouted**: A "yes" or "2" intended as a continuation may be treated as an independent new query.

### Auto-Escalation

The session actor can auto-escalate from Followup to Speculative when sustained latency degradation is detected:

- `ResponsivenessObserver` learns a **median** baseline from the first 5 requests (robust to outliers), then tracks LLM response times in a 20-sample rolling window. The baseline **adapts** every 20 samples via 80/20 EMA blend with the current window median, so gradual drift is tracked.
- If 3 consecutive responses exceed **3× baseline** latency, Speculative queue mode and Hedge racing are auto-activated simultaneously.
- A user notification is sent: "Detected slow responses. Enabling hedge racing + speculative queue."
- When the provider recovers (one normal-latency response), both revert to Followup and static routing.
- Auto-escalation also triggers on API channel (web client), which always uses the speculative processing path.

### Queue Commands

```
/queue                  -- show current mode
/queue followup         -- sequential processing
/queue collect          -- batch queued messages
/queue steer            -- keep newest only
/queue interrupt        -- cancel current + keep newest
/queue speculative      -- concurrent overflow agents
```

---

## Hooks

Hooks are the primary extension point for enforcing LLM policies, recording metrics, and auditing agent behavior -- per profile, without modifying core code.

Hooks are shell commands that run at agent lifecycle events. Each hook receives a JSON payload on stdin and communicates its decision via exit code.

### Exit Codes

| Exit Code | Meaning | Before-events | After-events |
|-----------|---------|---------------|--------------|
| 0 | Allow | Operation proceeds | Success logged |
| 1 | Deny | Operation blocked (reason on stdout) | Treated as error |
| 2+ | Error | Logged, operation proceeds | Logged |

### Events

Ten lifecycle events. Only the three **`before_*`** events can deny (exit 1); every other event is observe-only (a non-zero exit is logged but does not block).

| Event | When it fires | Can deny |
|-------|---------------|----------|
| `before_tool_call` | Before each tool execution | ✅ |
| `after_tool_call` | After each tool execution | — |
| `before_llm_call` | Before each LLM API call | ✅ |
| `after_llm_call` | After each successful LLM response | — |
| `before_spawn_verify` | Before a spawned sub-agent's verify step | ✅ |
| `on_spawn_verify` | At a spawned sub-agent's verify step | — |
| `on_spawn_complete` | When a spawned (background) task completes | — |
| `on_spawn_failure` | When a spawned task fails | — |
| `on_turn_end` | When an agent turn finishes | — |
| `on_resume` | When a session/turn resumes (e.g. after a client reconnect) | — |

The four core events carry the richest payloads (shown below); the spawn/turn/resume events carry the relevant task/session identifiers plus `event`, `session_id`, and `profile_id`.

#### `before_tool_call`

Fires before each tool execution. **Can deny** (exit 1).

```json
{
  "event": "before_tool_call",
  "tool_name": "shell",
  "arguments": {"command": "ls -la"},
  "tool_id": "call_abc123",
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

#### `after_tool_call`

Fires after each tool execution. Observe-only.

```json
{
  "event": "after_tool_call",
  "tool_name": "shell",
  "tool_id": "call_abc123",
  "result": "file1.txt\nfile2.txt\n...",
  "success": true,
  "duration_ms": 142,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

Note: `result` is truncated to 500 characters.

#### `before_llm_call`

Fires before each LLM API call. **Can deny** (exit 1).

```json
{
  "event": "before_llm_call",
  "model": "deepseek-chat",
  "message_count": 12,
  "iteration": 3,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

#### `after_llm_call`

Fires after each successful LLM response. Observe-only.

```json
{
  "event": "after_llm_call",
  "model": "deepseek-chat",
  "iteration": 3,
  "stop_reason": "EndTurn",
  "has_tool_calls": false,
  "input_tokens": 1200,
  "output_tokens": 350,
  "provider_name": "deepseek",
  "latency_ms": 2340,
  "cumulative_input_tokens": 5600,
  "cumulative_output_tokens": 1800,
  "session_cost": 0.0042,
  "response_cost": 0.0012,
  "session_id": "telegram:12345",
  "profile_id": "my-bot"
}
```

### Hook Configuration

In `config.json` or per-profile JSON:

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["python3", "~/.octos/hooks/guard.py"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "write_file"]
    },
    {
      "event": "after_llm_call",
      "command": ["python3", "~/.octos/hooks/cost-tracker.py"],
      "timeout_ms": 5000
    }
  ]
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `event` | yes | -- | One of the 10 event types (see Events above) |
| `command` | yes | -- | Argv array (no shell interpretation) |
| `timeout_ms` | no | 5000 | Kill hook process after this timeout |
| `tool_filter` | no | all | Only trigger for these tool names (tool events only) |

Multiple hooks can be registered for the same event. They run sequentially; the first deny wins.

### Circuit Breaker

Hooks are auto-disabled after 3 consecutive failures (timeout, crash, or exit code 2+). A successful execution (exit 0 or deny exit 1) resets the counter.

### Security

- Commands use argv arrays -- no shell interpretation.
- 18 dangerous environment variables are removed (`LD_PRELOAD`, `DYLD_*`, `NODE_OPTIONS`, etc.).
- Tilde expansion is supported (`~/` and `~username/`).

### Per-Profile Hooks

Each profile can define its own hooks via the `hooks` field in profile config. This allows different policy enforcement per channel or bot. Hook changes require a gateway restart.

### Backward Compatibility

- New fields may be added to payloads.
- Existing fields will never be removed or renamed.
- Hook scripts should ignore unknown fields (standard JSON practice).

### Example: Cost Budget Enforcer

```python
#!/usr/bin/env python3
"""Deny LLM calls when session cost exceeds $1.00."""
import json, sys

payload = json.load(sys.stdin)
if payload.get("event") == "before_llm_call":
    try:
        with open("/tmp/octos-cost.json") as f:
            state = json.load(f)
    except FileNotFoundError:
        state = {}
    sid = payload.get("session_id", "default")
    if state.get(sid, 0) > 1.0:
        print(f"Session cost exceeded $1.00 (${state[sid]:.4f})")
        sys.exit(1)

elif payload.get("event") == "after_llm_call":
    cost = payload.get("session_cost")
    if cost is not None:
        sid = payload.get("session_id", "default")
        try:
            with open("/tmp/octos-cost.json") as f:
                state = json.load(f)
        except FileNotFoundError:
            state = {}
        state[sid] = cost
        with open("/tmp/octos-cost.json", "w") as f:
            json.dump(state, f)

sys.exit(0)
```

### Example: Audit Logger

```python
#!/usr/bin/env python3
"""Log all tool and LLM calls to a JSONL file."""
import json, sys, datetime

payload = json.load(sys.stdin)
payload["timestamp"] = datetime.datetime.utcnow().isoformat()

with open("/var/log/octos-audit.jsonl", "a") as f:
    f.write(json.dumps(payload) + "\n")

sys.exit(0)
```

---

## Sandbox

Shell commands run inside a sandbox for isolation. Three backends are supported:

| Backend | Platform | Isolation | Network Control |
|---------|----------|-----------|-----------------|
| bwrap | Linux | RO bind `/usr,/lib,/bin,/sbin,/etc`; RW bind workdir; tmpfs `/tmp`; unshare-pid | `--unshare-net` if network denied |
| macOS | macOS | sandbox-exec with SBPL profile: `process-exec/fork`, `file-read*`, writes to workdir + `/private/tmp` | `(allow network*)` or `(deny network*)` |
| Docker | Any | `--rm --security-opt no-new-privileges --cap-drop ALL` | `--network none` if network denied |

Configure in `config.json`:

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false,
    "docker": {
      "image": "alpine:3.21",
      "mount_mode": "rw",
      "cpu_limit": "1.0",
      "memory_limit": "512m",
      "pids_limit": 100
    }
  }
}
```

- **Modes**: `auto` (detect best available), `bwrap`, `macos`, `docker`, `none`.
- **Mount modes**: `rw` (read-write), `ro` (read-only), `none` (no workspace mount).
- **Docker resource limits**: `--cpus`, `--memory`, `--pids-limit`.
- **Docker bind mount safety**: `docker.sock`, `/proc`, `/sys`, `/dev`, and `/etc` are blocked as bind mount sources.
- **Path validation**: Docker rejects `:`, `\0`, `\n`, `\r`; macOS rejects control chars, `(`, `)`, `\`, `"`.
- **Environment sanitization**: 18 dangerous environment variables are automatically cleared in all sandbox backends, MCP server spawning, hooks, and the browser tool: `LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH, DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH, NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB, JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR`.
- **Process cleanup**: Shell tool sends SIGTERM, waits grace period, then SIGKILL to child processes on timeout.

---

## Session Management

### New & Named Sessions

Send bare `/new` (equivalent to `/clear`) to wipe the current session's history and start fresh:

```
/new
```

Use `/new <name>` to switch to — or create — a **named** session (e.g. `/new research`), and `/new slides <name>` / `/new site <preset>` to scaffold a project session. Sessions are keyed by sender/channel; the session store additionally carries a `parent_key` field used for internally-forked child sessions (e.g. background spawns).

### Session Persistence

Each channel:chat_id pair maintains its own session (conversation history).

- **Storage**: JSONL files in `.octos/sessions/`
- **Max history**: Configurable via `gateway.max_history` (default: 50 messages)
- **Sessions**: bare `/new` clears the current session; named sessions are keyed by sender/channel, with a `parent_key` field for internally-forked child sessions

### Config Hot-Reload

The gateway automatically detects config file changes:

- **Hot-reloaded** (no restart): system prompt, AGENTS.md, SOUL.md, USER.md
- **Restart required**: provider, model, API keys, gateway channels

Changes are detected via SHA-256 hashing with debounce.

### Message Coalescing

Long responses are automatically split into channel-safe chunks before sending:

| Channel | Max chars per message |
|---------|-----------------------|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

Split preference: paragraph boundary > newline > sentence end > space > hard cut. Messages exceeding 50 chunks are truncated with a marker.

---

## Autonomy & Session Control

Beyond one-shot chat, the graphical clients (octos-web, octos-tui) drive longer-running behaviors over the [UI Protocol](./architecture.md). Each of these is gated by a negotiated capability flag (see [Capability Negotiation](#capability-negotiation)); when the client doesn't advertise the flag, the method is simply not offered.

### Goals

A **goal** is a persistent objective attached to a session. Once set, the agent keeps working toward it — re-firing turns as long as the goal's policy allows — instead of stopping after a single answer. Goals survive across turns and are cleared explicitly.

- Protocol: `session/goal/set`, `session/goal/get`, `session/goal/clear` (notifications `session/goal/updated`, `session/goal/cleared`). Feature flags: **both** `coding.autonomy.v1` **and** `coding.goal_runtime.v1` must be negotiated (advertising only the group flag yields `method_not_supported`).
- Use it for "keep going until X is done" work; clear the goal to stop.

### Loops

A **loop** is a recurring agent run. Loops are either **fixed-interval** (fire every N seconds) or **self-paced** — the model decides its own next cadence by emitting a `<<loop-next-in: …>>` hint (default 15 minutes when it doesn't). A loop keeps firing until paused, deleted, or the fire cap (10,000) is reached.

- Protocol: `loop/create`, `loop/list`, `loop/pause`, `loop/resume`, `loop/delete`, `loop/fire_now` (fire once immediately) — notifications `loop/fired`, `loop/completed`, `loop/updated`. Feature flags: **both** `coding.autonomy.v1` **and** `coding.loop_runtime.v1`.
- Use it for polling, monitoring, and self-paced background agents.

### Rewind

`session/rollback` rewinds a session to an earlier point (dropping the last N user turns), and `session/snapshot` captures the current session state (files, tasks, status). This backs the "rewind" / checkpoint UI in the clients.

### Task & turn control

- **Background tasks** (spawned work, deep-search, pipelines) can be listed, cancelled, and restarted: `task/list`, `task/cancel`, `task/restart_from_node`, with output/artifacts via `task/output/read` and `task/artifact/list`. Also reachable over REST at `POST /api/tasks/{id}/cancel` and `/restart-from-node`.
- **A running turn** can be interrupted mid-flight with `turn/interrupt` (the in-flight LLM call and tools are cancelled and the partial turn is persisted). `turn/start` and `turn/interrupt` are **not** capability-gated — they're always available. The separate **sub-agent** controls (`agent/list`, `agent/status/read`, `agent/interrupt`, `agent/close`) and task artifacts (`task/artifact/list|read`) require `coding.autonomy.v1` + `coding.agent_control.v1`.

### Capability negotiation

A client advertises which protocol features it supports when it connects (the `ui_feature` / `ui_features` query params or the `X-Octos-Ui-Features` header). The server only exposes methods and emits notifications for the features the client negotiated — so older clients keep working and new capabilities roll out without breaking them. Representative flags:

| Flag | Unlocks |
|------|---------|
| `coding.autonomy.v1` | Autonomy root — required *together with* the goal/loop/agent-control flags below |
| `coding.goal_runtime.v1` | Goals (`session/goal/*`) — needs `coding.autonomy.v1` too |
| `coding.loop_runtime.v1` | Loops (`loop/*`) — needs `coding.autonomy.v1` too |
| `coding.agent_control.v1` | Sub-agent controls (`agent/*`) and task artifacts (`task/artifact/*`) — needs `coding.autonomy.v1` too |
| `harness.task_control.v1` | Task list/cancel/restart |
| `harness.task_artifacts.v1` | Task artifacts |
| `state.session_hydrate.v1` | `session/hydrate` resume |
| `state.thread_graph.v1` | Thread/turn graph |
| `context.lifecycle.v1` | Context-compaction events |
| `approval.typed.v1` | Typed human-approval cards |
| `user_question.v1` | Clarifying-question cards |
| `auxiliary.rest_to_ws.v1` | The 13 auxiliary REST→WS methods (`session/list`, `content/list`, `session/snapshot`, …) |

---

## Context Compaction

When the conversation exceeds the LLM's context window, older messages are automatically compacted:

- Tool arguments are stripped (replaced with `"[stripped]"`)
- Messages are summarized to first lines
- Recent tool call/result pairs are preserved intact
- The agent continues seamlessly without losing critical context

---

## In-Chat Commands

### Slash Commands

| Command | Description |
|---------|-------------|
| `/new [name]` | Bare `/new` clears the current session; `/new <name>` switches to (or creates) a named session; `/new slides <name>` / `/new site <preset>` scaffold a project |
| `/clear` | Wipe the current session's history |
| `/s`, `/switch <name>` | Switch to a named session |
| `/sessions` | List sessions for this chat |
| `/back`, `/b` | Switch to the previously active session |
| `/delete`, `/d` | Delete the current session |
| `/config` | View and modify tool configuration |
| `/queue` | View or change queue mode |
| `/thinking` | View or set the reasoning-effort level (transport-dependent) |
| `/router`, `/adaptive` | View or change the routing/adaptive mode |
| `/soul` | View or edit the profile's SOUL (personality) |
| `/skills` | List / install / remove skills inline |
| `/status` | Show session/runtime status |
| `/help` | List the commands available on the current transport |
| `/exit`, `/quit`, `:q` | Exit chat (CLI mode only) |

The exact set varies by transport (CLI, gateway channel, web, TUI). Matrix management rooms add scheduling (`/schedule`, `/schedules`, `/unschedule`) and multi-bot (`/createbot`, `/deletebot`, `/listbots`, `/allbots`, `/bothelp`) commands — see [Gateway & Channels](./channels.md).

### In-Chat Provider Switching

The `switch_model` tool allows users to list available LLM providers and switch models at runtime through natural conversation. This tool is only available in gateway mode.

**List available providers:**

```
User: What models are available?

Bot: Current model: deepseek/deepseek-chat

     Available providers:
       - anthropic (default: claude-sonnet-4-20250514) [ready]
       - openai (default: gpt-4o) [ready]
       - deepseek (default: deepseek-chat) [ready]
       - gemini (default: gemini-2.5-flash) [ready]
       ...
```

**Switch models:**

```
User: Switch to GPT-4o

Bot: Switched to openai/gpt-4o.
     Previous model (deepseek/deepseek-chat) is kept as fallback.
```

When you switch models, the previous model automatically becomes a fallback:
- If the new model fails (rate limit, server error), requests automatically fall back to the original model.
- The fallback uses the circuit breaker (3 consecutive failures triggers failover).
- The chain is always flat: `[new_model, original_model]` -- repeated switches do not nest.

Model switches are persisted to the profile JSON file. On gateway restart, the bot starts with the last-selected model.

### Memory System

The agent maintains long-term memory across sessions:

- **`MEMORY.md`** -- Persistent notes, always loaded into context
- **Daily notes** -- `.octos/memory/YYYY-MM-DD.md`, auto-created
- **Recent memory** -- Last 7 days of daily notes included in context
- **Episodes** -- Task completion summaries stored in `episodes.redb`

### Hybrid Memory Search

Memory search combines BM25 (keyword) and vector (semantic) scoring:

- **Ranking**: `vector_weight * vector_score + bm25_weight * bm25_score` (defaults: 0.7 / 0.3)
- **Index**: HNSW with L2-normalized embeddings
- **Fallback**: BM25-only when no embedding provider is configured

Configure an embedding provider to enable vector search:

```json
{
  "embedding": {
    "provider": "openai"
  }
}
```

The embedding config supports three fields: `provider` (default: `"openai"`), `api_key_env` (optional override), and `base_url` (optional custom endpoint).

### Cron Jobs (Scheduled Tasks)

The agent can schedule recurring tasks using the `cron` tool:

```
User: Schedule a daily news digest at 8am Beijing time

Bot: Created cron job "daily-news" running at 8:00 AM Asia/Shanghai every day.
     Expression: 0 0 8 * * * *
```

Cron jobs can also be managed via CLI:

```bash
octos cron list                              # List active jobs
octos cron list --all                        # Include disabled
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron remove <job-id>
octos cron enable <job-id>
octos cron enable <job-id> --disable
```

When the agent is exposed through a Matrix management bot, the same scheduling capability can be presented as BotFather-style chat commands:

```text
/schedule 20秒之后提醒我看天气
/schedule 每天早上 9 点提醒我看天气
/schedules
/unschedule <job-id>
```

These commands stay bound to the current room/DM context. They do not expose raw cron syntax to end users, but still reuse the same cron store and delivery pipeline internally.

---

## Web Dashboard

The REST API server includes an embedded web UI:

```bash
octos serve                               # Binds to 127.0.0.1:50080
octos serve --host 0.0.0.0 --port 50080  # Accept external connections
# Open http://localhost:50080
```

Features:
- Session sidebar
- Chat interface
- UI Protocol WebSocket streaming
- Dark theme

A `/metrics` endpoint provides Prometheus-format metrics:
- `octos_tool_calls_total`
- `octos_tool_call_duration_seconds`
- `octos_llm_tokens_total`
