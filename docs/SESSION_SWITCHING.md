# Session Switching Architecture

This document describes how multi-session message routing, buffering, and delivery
work in the crew gateway. It covers the proxy/forwarder pattern, the pending
buffer, and the streaming-vs-proxy interaction that was fixed in the
session-inactive streaming bug.

## Overview

Each Telegram/WhatsApp/etc. chat can have multiple named sessions (e.g., "default",
"research", "0011"). Only one session is **active** at a time per chat. The active
session receives user messages; inactive sessions may still be running background
tasks (deep search, pipelines, etc.).

## Key Components

### ActiveSessionStore (`octos-bus/src/session.rs`)

Persisted to `active_sessions.json`. Maps `base_key → active_topic`.

```
base_key = "telegram:8516089817"
active_topic = "0011"  →  SessionKey = "telegram:8516089817#0011"
```

- `resolve_session_key(base_key)` → returns the full SessionKey with active topic
- `switch_to(base_key, topic)` → changes active topic, saves previous for `/back`
- `get_active_topic(base_key)` → returns current topic (empty = default)

### Message Routing

Every inbound message is routed by the gateway main loop:

```
User message → inbound.session_key() → base_key
            → active_sessions.resolve_session_key(base_key)
            → SessionKey with active topic
            → actor_registry.dispatch(session_key, ...)
```

This means all messages go to the **currently active** session. To talk to a
different session, the user must first switch with `/s <topic>`.

### Proxy/Forwarder Pattern

Each session actor does NOT send messages directly to the channel. Instead:

```
SessionActor.out_tx = proxy_tx  (NOT the real out_tx)
                ↓
         proxy channel (mpsc, capacity 64)
                ↓
      outbound_forwarder task
                ↓
    ┌─── is session active? ───┐
    │ YES                      │ NO
    │ deliver to real out_tx   │ buffer in pending_messages
    │ (user sees it now)       │ (HashMap<session_key, Vec<msg>>)
    └──────────────────────────┘
```

The `outbound_forwarder` is a per-session tokio task spawned in `ActorFactory::spawn()`.
It reads from `proxy_rx` and checks `ActiveSessionStore` on each message.

### Pending Buffer

When a message is buffered for an inactive session:

1. **First message**: Sends a notification to the active session:
   `"📌 {topic} finished. /s {topic} to view."`
2. **Subsequent messages**: Silently buffered (up to `MAX_PENDING_PER_SESSION = 50`)
3. **Overflow**: Messages beyond 50 are dropped with a warning log

### Flush on Session Switch

When the user switches sessions (via `/s`, `/back`, or inline keyboard), the
gateway calls `flush_pending(target_session_key)`:

```
/s research  →  switch_to(base_key, "research")
             →  flush_pending("telegram:8516089817#research")
             →  all buffered messages delivered in order
```

`flush_pending` is called in all switch paths:
- `/s <name>` (text command)
- `/s` (switch to default)
- `/back` (switch to previous)
- Inline keyboard callback (session picker buttons)
- `/n <name>` (switch via session number)

## Streaming vs Proxy Interaction (Bug Fix)

### The Problem

The stream reporter (`stream_reporter.rs`) sends LLM output directly to the
channel via `channel.edit_message()` / `channel.send_with_id()`, completely
bypassing the proxy/forwarder pipeline. This is by design for active sessions
— it enables real-time streaming to Telegram/Discord.

However, when a session is **inactive**:

1. Stream reporter sends output directly to the channel (user sees it in
   the wrong session context, or it gets lost in chat history)
2. The final reply check sees `already_streamed = true`
3. The proxy-path send is **skipped**
4. Nothing enters the pending buffer
5. On session switch, `flush_pending` has nothing to deliver

The user switches back and sees... nothing. They have to ask "what happened?"
to trigger the agent to report.

### The Fix

Three reply paths now check session activity before using the streaming result:

```rust
let session_active = self.is_active().await;  // checks ActiveSessionStore
let streamed = if session_active {
    // Use stream result if available (edit the streamed message)
    stream_result.message_id.is_some()
} else {
    false  // force proxy path → pending buffer
};

if !streamed {
    self.out_tx.send(reply).await;  // goes through proxy → forwarder
}
```

Fixed in:
- `process_inbound()` — main message processing path
- `process_inbound_speculative()` — speculative queue path
- `serve_overflow()` — overflow task path

The `SessionActor` now holds an `Arc<Mutex<ActiveSessionStore>>` reference
and exposes `is_active()` to check whether its session is currently active.

### Streaming Still Works

When the session IS active, streaming works exactly as before — chunks are
edited into a Telegram message in real-time. The fix only affects the
**final reply delivery** path, and only when the session is inactive.

Note: intermediate streaming edits still go directly to the channel even when
inactive. This is acceptable because:
- They edit the same message (not creating new ones)
- The final reply overwrites them via the proxy path
- The important thing is that the **final content** reaches the pending buffer

## Tool Output Buffering During Session Switch

### How Tool Calls Are Protected

All agent tools (`send_file`, `message_tool`, etc.) are created per-session with
fixed `channel` + `chat_id` via `with_context()` at actor spawn time. These values
are stored in `Mutex`-wrapped state and **never mutated** during the actor's lifetime.

Critically, tools write to `proxy_tx` (the per-session proxy channel), not the
gateway's real output channel. This means all tool output — including `send_file`
attachments — flows through the outbound forwarder and respects session activity.

### `send_file` During a Session Switch

When an agent calls `send_file` while the user has switched away:

```
t=0   Session "default" active, agent starts processing message
t=50  Agent calls send_file → file message goes to proxy_tx
t=51  User runs /s research (switches to "research")
t=52  outbound_forwarder reads from proxy_rx
t=53  Checks: is "default" active? NO → buffers the file message
t=54  User sees: "📌 (default) finished. /s to view."
t=55  User runs /s → flush_pending delivers the buffered file
```

The file is **never lost and never delivered to the wrong session**. It waits
in the pending buffer until the user switches back.

### Design Guarantees

| Property | Mechanism |
|----------|-----------|
| Fixed routing | `with_context()` bakes channel/chat_id at spawn time |
| No cross-session leaks | Tools write to per-session proxy, not shared output |
| Buffered delivery | Forwarder checks `ActiveSessionStore` on every message |
| FIFO ordering | Pending buffer is a `Vec<OutboundMessage>`, delivered in order |
| Bounded memory | `MAX_PENDING_PER_SESSION = 50`, overflow dropped with warning |
| No race conditions | No `set_context()` mutation; context is immutable per actor |

This pattern applies to **all tool output**, not just `send_file` — any tool
that sends outbound messages (code execution results, media, etc.) gets the
same buffering protection.

## Message Flow Summary

### Active Session
```
Agent → stream chunks → channel.edit_message() (real-time)
Agent → final reply → streamed=true → skip proxy → done
```

### Inactive Session
```
Agent → stream chunks → channel.edit_message() (still happens, harmless)
Agent → final reply → is_active()=false → streamed=false → proxy_tx
     → outbound_forwarder → inactive → pending_messages buffer
     → notification: "📌 topic finished. /s topic to view."

User: /s topic
     → flush_pending → deliver all buffered messages → user sees results
```

## Session Commands Reference

| Command | Action |
|---------|--------|
| `/s` | Switch to default session |
| `/s <name>` | Switch to named session (creates if needed) |
| `/back` | Switch to previous session |
| `/new` | Clear current session |
| `/new <name>` | Create & switch to named session |
| `/sessions` | Show session picker (inline keyboard) |
| `/n <number>` | Switch to session by number |
| `/delete <name>` | Delete a named session |
