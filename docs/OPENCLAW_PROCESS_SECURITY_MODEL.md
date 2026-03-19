# OpenClaw Process & Security Model

Reference analysis of [openclaw/openclaw](https://github.com/openclaw/openclaw) process isolation and security architecture. Compared against octos for cross-pollination opportunities.

Last updated: 2026-03-10.

---

## 1. Trust Model

OpenClaw is designed as a **personal assistant** framework — one trusted operator per Gateway instance. It explicitly does NOT target hostile multi-tenant isolation.

| Boundary | Mechanism | Strength |
|----------|-----------|----------|
| Operator ↔ Operator | Separate Gateway process (separate host/VPS recommended) | OS-level |
| Agent ↔ Agent | Per-agent directory (`~/.openclaw/agents/<agentId>/`) | Directory-based |
| Session ↔ Session | Session key routing + per-session JSONL files | Application-level |
| Tool execution | Optional Docker sandbox (per-session/agent/shared) | Container-level |

**octos comparison**: octos targets multi-tenant (profile = tenant, OS process boundary). OpenClaw is single-tenant per Gateway, multi-agent within the Gateway.

---

## 2. Process Architecture

```
Gateway (single long-lived Node.js process)
├── WebSocket Server (port 18789)
│   ├── CLI clients
│   ├── macOS app / web UI
│   └── Mobile nodes (iOS/Android)
├── Channel Integrations
│   ├── WhatsApp, Telegram, Slack, Discord, Signal, iMessage...
│   └── Each channel as plugin or built-in
├── Agent Runtime (embedded TypeScript — "Pi engine")
│   ├── Session lane concurrency (default 4 per agent)
│   ├── Per-agent tool execution (host or sandboxed)
│   └── Per-agent memory + session store
├── ACP (Agent Control Protocol) — optional child processes
│   └── Sub-agent spawning (persistent or oneshot mode)
├── Admin HTTP API
└── Maintenance timers (cleanup, cron, health)
```

**Key difference from octos**: OpenClaw runs everything in ONE Node.js process. No child-process-per-profile. Isolation between agents is in-process (directory + session key scoping). octos spawns a separate OS process per profile via `octos serve`.

### Session Lane Concurrency

OpenClaw limits concurrent agent runs per agent via **session lanes** (default 4). This is analogous to octos's queue mode — prevents resource exhaustion from many simultaneous chats.

---

## 3. DM Scope (`dmScope`)

`dmScope` controls how direct messages from different users map to agent sessions. This is OpenClaw's most notable session isolation feature that octos lacks.

### The Problem

When multiple users DM the same bot, how should their conversations be grouped?

- **Shared session**: All DMs go to one session → users see each other's context (privacy leak)
- **Per-user session**: Each user gets their own session → isolated but more sessions to manage

### OpenClaw's 4 Modes

| Mode | Session Key Pattern | Behavior |
|------|-------------------|----------|
| `"main"` | `agent:main:main` | All DMs → shared main session. No user isolation. |
| `"per-peer"` | `agent:main:direct:<peerId>` | One session per user, cross-channel. User on Telegram and WhatsApp shares one session. |
| `"per-channel-peer"` | `agent:main:<channel>:direct:<peerId>` | One session per user per channel. Telegram user ≠ WhatsApp user even if same person. |
| `"per-account-channel-peer"` | `agent:main:<channel>:<accountId>:direct:<peerId>` | Finest granularity. Separate sessions per bot account too (for multi-account channels). |

### Session Key Construction (`session-key.ts`)

```typescript
// DM scope determines session key structure
function buildAgentPeerSessionKey(params) {
  if (peerKind === "direct") {
    switch (dmScope) {
      case "main":
        return `agent:${agentId}:main`;           // shared
      case "per-peer":
        return `agent:${agentId}:direct:${peerId}`;
      case "per-channel-peer":
        return `agent:${agentId}:${channel}:direct:${peerId}`;
      case "per-account-channel-peer":
        return `agent:${agentId}:${channel}:${accountId}:direct:${peerId}`;
    }
  }
  // Groups always include channel + groupId
  return `agent:${agentId}:${channel}:${peerKind}:${peerId}`;
}
```

### Identity Links (Cross-Channel User Merging)

OpenClaw supports `identityLinks` — a map of canonical user names to platform-specific IDs. When `dmScope` is `per-peer` or `per-channel-peer`, it resolves linked IDs to a canonical name before building the session key. This lets the same person on Telegram and WhatsApp share a session if desired.

```json
{
  "identityLinks": {
    "alice": ["telegram:12345", "whatsapp:8613800138000"]
  }
}
```

### octos Comparison

octos always uses `channel:chat_id` as the session key (equivalent to `"per-channel-peer"`). This is hardcoded — no configurable scoping. octos does NOT support:
- Shared main session (`"main"` mode) — useful for single-user personal assistant
- Cross-channel identity linking (`identityLinks`)
- Per-account scoping (multi-account per channel)

**Recommendation**: Add configurable `dm_scope` to octos profile config. Default to `"per-channel-peer"` (current behavior). The `"main"` mode is valuable for personal single-user setups where the operator IS the only user.

---

## 4. Data Isolation

### Per-Agent Directory Structure

```
~/.openclaw/
├── openclaw.json                        # Global config
├── workspace-main/                      # Agent workspace (tool cwd)
├── workspace-work/                      # Second agent's workspace
├── agents/
│   ├── main/
│   │   └── agent/
│   │       ├── sessions/
│   │       │   ├── sessions.json        # Session index
│   │       │   └── *.jsonl              # Per-session transcripts
│   │       ├── credentials/
│   │       │   ├── auth-profiles.json   # LLM provider auth
│   │       │   └── *.json              # Channel-specific creds
│   │       └── memory/                  # Agent memory store
│   └── work/
│       └── agent/
│           ├── sessions/
│           ├── credentials/
│           └── memory/
└── sandboxes/                           # Docker sandbox workspaces
```

**octos comparison**:

| Aspect | OpenClaw | octos |
|--------|----------|---------|
| Top-level scoping | Per-agent | Per-profile (OS process) then per-user |
| Session files | `agents/<agentId>/agent/sessions/` | `users/<base_key>/sessions/` |
| Credentials | Per-agent directory | Per-profile env vars / config |
| Memory | Per-agent | Per-profile (`episodes.redb`) |
| Workspace | Per-agent configurable path | Per-profile `--cwd` |

**Key insight**: OpenClaw scopes data per-agent (not per-user within agent). Sessions within an agent are separated by session key but live in the same directory. octos now has per-user directories — finer isolation within a profile.

### Session Persistence

- Format: JSONL (same as octos)
- Crash safety: atomic write-then-rename (same as octos)
- File size limit: 10MB (same as octos)
- Session maintenance: configurable cleanup, pruning, LRU cache
- Duplicate agent directory detection at startup (`findDuplicateAgentDirs()`)

---

## 5. Sandbox Architecture

### Docker-Based (Only Backend)

OpenClaw uses Docker containers exclusively for sandboxing (no bubblewrap, no macOS sandbox-exec).

**Three sandbox modes** (`agents.defaults.sandbox.mode`):

| Mode | Behavior |
|------|----------|
| `"off"` | No sandboxing — tools run on host (default) |
| `"non-main"` | Sandbox only non-main sessions (production recommendation) |
| `"all"` | Every session runs in sandbox |

**Three sandbox scopes** (`agents.defaults.sandbox.scope`):

| Scope | Container Lifecycle |
|-------|-------------------|
| `"session"` | One container per session (default, strongest isolation) |
| `"agent"` | One container per agent (shared across sessions) |
| `"shared"` | One container for all sandboxed sessions |

**Workspace access** (`agents.defaults.sandbox.workspaceAccess`):

| Mode | Mount |
|------|-------|
| `"none"` | Isolated sandbox workspace only (default) |
| `"ro"` | Agent workspace mounted read-only at `/agent` |
| `"rw"` | Agent workspace mounted read-write at `/workspace` |

### Container Security

- `--security-opt no-new-privileges`, `--cap-drop ALL`
- Network: **disabled by default** (`--network none`)
- Dangerous modes blocked: `host` network, `container:*` namespace joins
- Break-glass: `dangerouslyAllowContainerNamespaceJoin` flag
- Auto-pruning: 24h idle or 7 days max age
- Runs as unprivileged user (uid:gid from workspace ownership)

### Dangerous Bind Mount Protection

OpenClaw **blocks specific dangerous bind sources**:
- `docker.sock` — prevents container escape
- `/etc` — prevents host config access
- `/proc`, `/sys`, `/dev` — prevents kernel interface access

**octos does NOT have this** — a valuable addition.

### Browser Sandbox

Dedicated Docker container with:
- Isolated network (`openclaw-sandbox-browser`)
- noVNC with password-protected access
- CDP access restricted via `cdpSourceRange` CIDR allowlist
- `allowHostControl` flag gates host browser targeting

### Env Sanitization

Same 18 blocked env vars as octos (`LD_PRELOAD`, `DYLD_*`, `NODE_OPTIONS`, etc.).

---

## 6. Credential Isolation

| Secret Type | Scope | Location |
|-------------|-------|----------|
| LLM API keys | Per-agent | `agents/<agentId>/agent/credentials/auth-profiles.json` |
| Channel creds | Per-agent | `agents/<agentId>/agent/credentials/*.json` |
| Gateway auth | Global | Config file or `OPENCLAW_GATEWAY_TOKEN` env |
| Secret refs | Config-level | `{ $ref: "secret:..." }` in config |

**Auth profile rotation**: Per-agent OAuth + API key rotation with failover (401/403 → next provider, 429/5xx → retry + failover). Same pattern as octos.

**octos comparison**: octos stores credentials as env vars or in profile config. OpenClaw's per-agent credential files with `auth-profiles.json` provide better isolation when running multiple agents. octos's profile-level isolation (OS process) compensates — credentials can't leak between profiles.

---

## 7. Multi-Gateway Isolation

For stronger isolation, OpenClaw recommends running separate Gateway instances:

```bash
# Instance 1
OPENCLAW_STATE_DIR=~/.openclaw-personal \
OPENCLAW_CONFIG_PATH=~/.openclaw/personal.json \
openclaw gateway --port 18789

# Instance 2
OPENCLAW_STATE_DIR=~/.openclaw-work \
OPENCLAW_CONFIG_PATH=~/.openclaw/work.json \
openclaw gateway --port 18790
```

Each instance gets:
- Own state directory (sessions, credentials, cache)
- Own config file
- Own port
- Profile shorthand: `openclaw --profile rescue` auto-scopes everything

**octos equivalent**: `octos serve` with multiple profiles — each profile gets its own child process and `data_dir`. octos's approach is more automated (dashboard manages profiles, auto-spawns processes).

---

## 8. What octos Can Learn

### High Priority

1. **Configurable DM scope** — Add `dm_scope` to profile config with 4 modes. Default to `"per-channel-peer"` (current behavior). The `"main"` mode simplifies single-user personal setups.

2. **Identity links** — Cross-channel user merging via config. One person on Telegram and WhatsApp gets a unified session when desired.

3. **Dangerous bind mount blocking** — Block `docker.sock`, `/etc`, `/proc`, `/sys`, `/dev` in Docker sandbox mounts. Simple check, prevents container escape.

4. **Sandbox scope options** — Add per-session vs per-agent vs shared container scope. Currently octos creates a new sandbox per shell invocation (most expensive option).

### Medium Priority

5. **Sandbox mode `"non-main"`** — Only sandbox non-main sessions. Main session (operator) runs unsandboxed for convenience, while external users get sandboxed. Good default for production.

6. **Workspace access modes** — `none`/`ro`/`rw` mount options for Docker sandbox. Currently octos has `ro`/`rw` but no `none` (fully isolated workspace).

7. **Browser sandbox isolation** — Dedicated container with network isolation, noVNC, and CDP CIDR allowlist. Currently octos's browser tool runs on host.

### Lower Priority

8. **Multi-account per channel** — Support multiple bot accounts per channel type (e.g., personal + business WhatsApp). Requires `per-account-channel-peer` DM scope.

9. **Secret references in config** — `{ $ref: "secret:keychain:MY_KEY" }` for config values, avoiding plaintext secrets in config files.

---

## 9. What OpenClaw Can Learn from octos

| Area | octos Advantage |
|------|-------------------|
| **Per-user directories** | `users/{base_key}/sessions/` — finer isolation than OpenClaw's flat per-agent sessions |
| **Per-actor state** | `SessionHandle` per actor — zero cross-user mutex contention |
| **3 sandbox backends** | bubblewrap (Linux) + sandbox-exec (macOS) + Docker — OpenClaw is Docker-only |
| **Adaptive model routing** | Weighted scoring (latency EMA + error rate) vs static fallback chains |
| **Hybrid memory search** | BM25 + HNSW vector vs SQLite-only |
| **Token-aware compaction** | Model-specific token estimation vs character heuristics |
| **Tool argument limits** | 1MB non-allocating size check — OpenClaw has no equivalent |
| **Native performance** | Rust binary, no V8 overhead, ~2KB per task vs Node.js coroutine overhead |

---

## 10. Architecture Comparison Summary

```
octos multi-tenant model:                OpenClaw single-tenant model:

octos serve (control plane)                  Gateway (single Node.js process)
├── Profile A (child process)               ├── Agent "main"
│   ├── User X (SessionActor + Handle)      │   ├── Session lane 1
│   ├── User Y (SessionActor + Handle)      │   ├── Session lane 2..4
│   └── data_dir/users/{X,Y}/sessions/     │   └── agents/main/agent/sessions/
├── Profile B (child process)               ├── Agent "work"
│   ├── User Z (SessionActor + Handle)      │   ├── Session lanes...
│   └── data_dir/users/{Z}/sessions/       │   └── agents/work/agent/sessions/
└── Dashboard (React SPA)                   └── WebSocket clients (CLI, app, web)

Isolation: OS process per profile           Isolation: directory per agent
           + per-user directory                        + session key routing
           + per-actor mutex                           + session lane limits
           + optional sandbox                          + optional Docker sandbox
```

---

## Related Documents

- [SESSION_ACTOR_ARCHITECTURE.md](./SESSION_ACTOR_ARCHITECTURE.md) — octos per-actor model
- [SECURITY_ARCHITECTURE.md](./SECURITY_ARCHITECTURE.md) — octos security layers
- [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) — Feature cross-pollination guide
- [OPENCLAW_CHANNEL_ARCHITECTURE.md](./OPENCLAW_CHANNEL_ARCHITECTURE.md) — Channel adapter patterns
