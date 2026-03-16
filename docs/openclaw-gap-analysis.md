# OpenClaw Gap Analysis for octos

Based on analysis of [openclaw/openclaw](https://github.com/openclaw/openclaw) (Feb 2026).

**Status: ALL 9 ITEMS COMPLETE** (implemented Feb 2026)

## Current Parity

octos already has: tool registry, path traversal protection, MCP support, session management, shell sandbox (bwrap/macOS/Docker), cron scheduling, SSRF protection, plugin system, tool policies, context compaction, config hot-reload, hybrid memory search, message coalescing, session forking.

## Implemented Items

### 1. Tool Policy System - DONE

`ToolPolicy` with allow/deny lists, deny-wins semantics, wildcard matching (`exec*`).
Implemented in `octos-agent/src/tools/policy.rs`.

### 2. Tool Groups - DONE

Named groups (`group:fs`, `group:runtime`, `group:search`, `group:web`, `group:sessions`) expanding to tool sets. Integrated with ToolPolicy.
Implemented in `octos-agent/src/tools/policy.rs`.

### 3. Context Compaction - DONE

Token-aware message compaction: estimates tokens, strips tool arguments, summarizes first lines, preserves recent tool call/result pairs.
Implemented in `octos-agent/src/compaction.rs`.

### 4. Config Hot-Reload - DONE

SHA-256 hash-based change detection with hot-reload for system prompt and restart-required for provider/model changes. Watches config, AGENTS.md, SOUL.md, USER.md.
Implemented in `octos-cli/src/config_watcher.rs`.

### 5. Hybrid Memory Search - DONE

BM25 + vector (cosine similarity) hybrid ranking with configurable alpha. HNSW index via `hnsw_rs`, L2-normalized embeddings. Falls back to BM25-only when no embedding provider configured.
Implemented in `octos-memory/src/hybrid_search.rs`.

### 6. Streaming Block Coalescing - DONE

Channel-aware message splitting: paragraph > newline > sentence > space > hard cut. Per-channel limits (Telegram 4000, Discord 1900, Slack 3900). MAX_CHUNKS (50) DoS limit. UTF-8 safe boundary detection.
Implemented in `octos-bus/src/coalesce.rs`.

### 7. Session Forking - DONE

`/new` command creates child session with `parent_key` tracking. Copies last N messages from parent. Namespaced by sender_id + timestamp. Persisted via JSONL with percent-encoded filenames.
Implemented in `octos-bus/src/session.rs` (fork method) and `octos-cli/src/commands/gateway.rs` (/new handler).

### 8. Docker Sandbox - DONE

`SandboxMode::Docker` with `DockerConfig`: image selection, mount modes (none/ro/rw), resource limits (CPU, memory, PIDs), network isolation, environment sanitization (18 blocked vars via shared `BLOCKED_ENV_VARS`). Path validation rejects `:`, `\0`, `\n`, `\r`.
Implemented in `octos-agent/src/sandbox.rs`.

### 9. Provider-Specific Tool Policies - DONE

`tools.byProvider` config maps model ID prefixes to ToolPolicy. Applied at both spec filtering and execution time. Propagated to subagents via spawn tool.
Implemented in `octos-cli/src/config.rs` and `octos-agent/src/tools/policy.rs`.

## Implementation Order (Completed)

```
Phase A: Tool groups + Tool policies + Provider policies
Phase B: Context compaction
Phase C: Config hot-reload
Phase D: Hybrid memory search
Phase E: Message coalescing + Session forking + Docker sandbox
```
