# Plugin SDK — Design Document

crew-rs extensibility architecture for skills, tools, channels, and hooks.

## Status

**Current state**: crew-rs has three extension points, each with its own mechanism:
- **App Skills** — Rust binaries, stdin/stdout JSON protocol (see `app-skill-dev-guide.md`)
- **Skills** — SKILL.md + .dot pipeline files (declarative, no code)
- **Hooks** — Shell commands on lifecycle events (see `HOOKS.md`)

**Goal**: Unify these into a coherent Plugin SDK that supports multiple plugin types while keeping the subprocess isolation model.

---

## Plugin Types

A plugin is a directory with a `manifest.json` that declares what it provides:

```
~/.crew/plugins/my-plugin/
├── manifest.json       # Plugin declaration
├── SKILL.md            # Optional: LLM instructions
├── main                # Optional: executable binary
└── ...                 # Optional: additional resources
```

### Type 1: Tool Plugin (current "app-skill")

Provides one or more tools the LLM can call. Executed as a subprocess per tool call.

```json
{
  "id": "weather",
  "version": "1.0.0",
  "type": "tool",
  "description": "Weather forecasts via Open-Meteo API",
  "tools": [
    {
      "name": "get_weather",
      "description": "Get current weather for a city",
      "input_schema": {
        "type": "object",
        "properties": {
          "city": { "type": "string" }
        },
        "required": ["city"]
      }
    }
  ],
  "binary": "main",
  "timeout_secs": 15,
  "requires": {
    "env": ["WEATHER_API_KEY"],
    "bins": []
  }
}
```

**Protocol**: Same as today — `argv[1]` = tool name, stdin = JSON args, stdout = `{"output": "...", "success": true}`.

### Type 2: Skill Plugin (current SKILL.md)

Provides LLM instructions — teaches the agent how to use existing tools. No executable.

```json
{
  "id": "git-workflow",
  "version": "1.0.0",
  "type": "skill",
  "description": "Git branching and PR workflow best practices",
  "requires": {
    "bins": ["git", "gh"]
  }
}
```

The `SKILL.md` in the same directory is injected into the system prompt when the skill is active.

### Type 3: Channel Plugin

Provides a messaging channel integration. Long-running process that connects to an external service and forwards messages to the gateway.

```json
{
  "id": "matrix",
  "version": "1.0.0",
  "type": "channel",
  "description": "Matrix/Element chat integration",
  "binary": "main",
  "config_schema": {
    "type": "object",
    "properties": {
      "homeserver": { "type": "string" },
      "access_token": { "type": "string", "sensitive": true }
    },
    "required": ["homeserver", "access_token"]
  }
}
```

**Protocol**: JSON-RPC over stdin/stdout (see Channel Protocol below).

### Type 4: Hook Plugin

Provides lifecycle hooks that fire on agent events. Replaces shell-command hooks with a structured plugin interface.

```json
{
  "id": "audit-logger",
  "version": "1.0.0",
  "type": "hook",
  "description": "Log all tool calls to audit trail",
  "binary": "main",
  "hooks": ["before_tool_call", "after_tool_call", "after_llm_call"]
}
```

**Protocol**: Long-running process. Gateway sends JSON events on stdin, reads decisions from stdout.

---

## Plugin Discovery

Plugins are discovered from multiple locations with precedence (highest first):

| Priority | Location | Description |
|----------|----------|-------------|
| 1 | Profile `plugins` config | Per-profile plugin overrides |
| 2 | `<data_dir>/plugins/` | Per-profile installed plugins |
| 3 | `~/.crew/plugins/` | User-installed plugins |
| 4 | `~/.crew/skills/` | Legacy app-skills (auto-migrated) |
| 5 | Bundled | Compiled into the binary |

Higher-priority plugins override lower-priority ones with the same `id`.

### Plugin Config (in profile JSON)

```json
{
  "config": {
    "plugins": {
      "weather": {
        "enabled": true,
        "env": {
          "WEATHER_API_KEY": "sk-..."
        }
      },
      "matrix": {
        "enabled": false
      },
      "audit-logger": {
        "enabled": true,
        "config": {
          "log_path": "/var/log/crew-audit.jsonl"
        }
      }
    }
  }
}
```

---

## Protocols

### Tool Protocol (unchanged from current app-skills)

```
Gateway                          Plugin Binary
  │                                   │
  │  spawn: ./main get_weather        │
  │  stdin:  {"city": "Paris"}        │
  │ ─────────────────────────────────>│
  │                                   │
  │  stdout: {"output":"...",         │
  │           "success": true}        │
  │ <─────────────────────────────────│
  │                                   │
  │  process exits                    │
```

- `argv[1]` = tool name
- stdin = JSON matching `input_schema`
- stdout = `{"output": string, "success": bool}`
- stderr = debug logs (ignored by gateway)
- Exit 0 = success, non-zero = failure

### Channel Protocol (new)

Long-running bidirectional JSON-RPC over stdin/stdout:

```
Gateway                          Channel Plugin
  │                                   │
  │  spawn: ./main                    │
  │  stdin: {"jsonrpc":"2.0",         │
  │    "method":"init",               │
  │    "params":{"config":{...}}}     │
  │ ─────────────────────────────────>│
  │                                   │
  │  stdout: {"jsonrpc":"2.0",        │
  │    "result":{"ok":true}}          │
  │ <─────────────────────────────────│
  │                                   │
  │  (plugin connects to Matrix)      │
  │                                   │
  │  stdout: {"jsonrpc":"2.0",        │  Incoming message
  │    "method":"message",            │
  │    "params":{"from":"@user:mx",   │
  │      "text":"Hello",              │
  │      "channel":"matrix",          │
  │      "chat_id":"!room:mx"}}       │
  │ <─────────────────────────────────│
  │                                   │
  │  (gateway processes, gets reply)  │
  │                                   │
  │  stdin: {"jsonrpc":"2.0",         │  Outgoing reply
  │    "method":"send",               │
  │    "params":{"chat_id":"!room:mx",│
  │      "text":"Hi there!"}}         │
  │ ─────────────────────────────────>│
  │                                   │
  │  stdout: {"jsonrpc":"2.0",        │
  │    "result":{"ok":true,           │
  │      "message_id":"$evt123"}}     │
  │ <─────────────────────────────────│
```

**Gateway → Plugin methods:**

| Method | Description |
|--------|-------------|
| `init` | Initialize with config. Plugin should connect to service. |
| `send` | Send a message to a chat. Params: `chat_id`, `text`, `media_url?`, `reply_to?` |
| `shutdown` | Graceful shutdown. Plugin should disconnect and exit. |

**Plugin → Gateway methods:**

| Method | Description |
|--------|-------------|
| `message` | Incoming message. Params: `from`, `text`, `channel`, `chat_id`, `media_url?`, `reply_to?` |
| `status` | Status update. Params: `connected`, `error?` |

### Hook Protocol (new)

Long-running bidirectional JSON-RPC over stdin/stdout:

```
Gateway                          Hook Plugin
  │                                   │
  │  spawn: ./main                    │
  │  stdin: {"jsonrpc":"2.0",         │
  │    "method":"init",               │
  │    "params":{"hooks":[...]}}      │
  │ ─────────────────────────────────>│
  │                                   │
  │  stdin: {"jsonrpc":"2.0",         │  Tool call event
  │    "method":"before_tool_call",   │
  │    "id": 1,                       │
  │    "params":{"tool_name":"shell", │
  │      "arguments":{...}}}          │
  │ ─────────────────────────────────>│
  │                                   │
  │  stdout: {"jsonrpc":"2.0",        │  Allow
  │    "id": 1,                       │
  │    "result":{"allow": true}}      │
  │ <─────────────────────────────────│
```

**Hook events** (same as current HOOKS.md):

| Event | Can Deny | Payload |
|-------|----------|---------|
| `before_tool_call` | Yes | tool_name, arguments, session_id, profile_id |
| `after_tool_call` | No | tool_name, result, success, duration_ms |
| `before_llm_call` | Yes | model, message_count, iteration |
| `after_llm_call` | No | model, stop_reason, input_tokens, output_tokens |
| `message_received` | Yes | from, text, channel, chat_id |
| `message_sending` | Yes | text, channel, chat_id |

**Deny response**: `{"allow": false, "reason": "blocked by policy"}`

---

## Manifest Schema

Full `manifest.json` schema:

```json
{
  "id": "string (required, kebab-case)",
  "version": "string (required, semver)",
  "type": "tool | skill | channel | hook (required)",
  "description": "string",
  "author": "string",
  "homepage": "string (URL)",
  "license": "string",

  "binary": "string (filename, default: 'main')",
  "timeout_secs": "number (tool type only, default: 30)",

  "tools": [
    {
      "name": "string (snake_case, globally unique)",
      "description": "string (for LLM)",
      "input_schema": { "JSON Schema object" }
    }
  ],

  "hooks": ["before_tool_call", "after_tool_call", ...],

  "requires": {
    "bins": ["binary names that must exist on PATH"],
    "env": ["env var names that must be set"],
    "os": ["darwin", "linux", "win32"]
  },

  "config_schema": {
    "JSON Schema for plugin-specific config"
  },

  "install": [
    {
      "id": "string",
      "kind": "brew | apt | cargo | download",
      "formula": "string (brew)",
      "package": "string (apt)",
      "crate": "string (cargo)",
      "url": "string (download)",
      "bins": ["binary names provided"],
      "label": "string (human-readable)",
      "os": ["darwin", "linux"]
    }
  ]
}
```

---

## Gating (Load-Time Filtering)

Plugins are filtered at load time based on `requires`:

| Gate | Behavior |
|------|----------|
| `requires.bins` | All binaries must exist on PATH |
| `requires.env` | All env vars must be set (or in profile config) |
| `requires.os` | Current platform must match |
| `config_schema` | If plugin has config_schema, required fields must be present |

Gating is checked before spawning the binary. Failed gates mark the plugin as `unavailable` (not loaded, not in system prompt).

---

## Comparison with OpenClaw

| Aspect | crew.rs Plugin SDK | OpenClaw Plugin SDK |
|--------|-------------------|---------------------|
| **Language** | Any (subprocess protocol) | TypeScript only (in-process) |
| **Isolation** | Process-level (fork + exec) | None (shared JS runtime) |
| **Communication** | stdin/stdout JSON-RPC | Direct function calls |
| **Skill format** | SKILL.md + manifest.json | SKILL.md (YAML frontmatter) |
| **Tool definition** | manifest.json (JSON Schema) | `registerTool()` in code |
| **Channel plugins** | Long-running subprocess | In-process adapter interfaces |
| **Hook system** | Long-running subprocess, JSON-RPC | In-process callbacks (24 events) |
| **Config** | Profile JSON + manifest config_schema | openclaw.json + Zod schemas |
| **Plugin discovery** | Directory-based, multi-location | Directory-based, multi-location |
| **Security** | Process isolation, env sanitization, timeout | Trusted code, SSRF guard only |
| **Performance** | Fork/exec overhead per tool call | Zero overhead |
| **Multi-language** | Yes (any language that does JSON) | No (TypeScript/JS only) |

### crew.rs Advantages
- **True isolation**: Plugins can't crash the gateway
- **Language-agnostic**: Write plugins in Rust, Python, Go, shell, anything
- **Security by default**: Process boundary prevents data leaks

### crew.rs Tradeoffs
- **Latency**: Fork+exec overhead (~5-50ms per tool call)
- **No in-process hooks**: Hook plugins are long-running subprocesses
- **No streaming**: Tool results are atomic (no partial results during execution)

---

## Migration from Current System

### App Skills → Tool Plugins

Current app-skills are already tool plugins. Migration:

1. Add `"type": "tool"` to existing `manifest.json`
2. Move `requires_env` / `requires_bins` from SKILL.md to `manifest.json` `requires`
3. No code changes needed — protocol is identical

### Shell Hooks → Hook Plugins

Current shell hooks continue to work unchanged. Hook plugins are an additional option for stateful hooks that benefit from a persistent process (e.g., accumulating metrics, maintaining connections).

### Bundled Skills → Skill Plugins

Current bundled SKILL.md + .dot files become skill plugins by adding a `manifest.json`:

```json
{
  "id": "mofa-slides",
  "version": "1.0.0",
  "type": "skill",
  "description": "Generate presentation slides from text"
}
```

---

## Implementation Plan

### Phase 1: Manifest + Discovery (foundation)

- [ ] Define `manifest.json` schema (JSON Schema file in repo)
- [ ] Plugin discovery: scan `~/.crew/plugins/`, `<data_dir>/plugins/`, bundled
- [ ] Precedence resolution (profile > user > bundled)
- [ ] Gating: check `requires.bins`, `requires.env`, `requires.os`
- [ ] Migrate existing `BUNDLED_APP_SKILLS` to use manifest-based loading
- [ ] Per-profile plugin enable/disable via `config.plugins`

### Phase 2: Channel Plugin Protocol

- [ ] Define JSON-RPC protocol for channel plugins
- [ ] Implement `ChannelPluginManager` in gateway (spawn, supervise, restart)
- [ ] Wire inbound messages from channel plugins into gateway message bus
- [ ] Wire outbound replies from gateway to channel plugin stdin
- [ ] Health check / heartbeat for long-running channel processes

### Phase 3: Hook Plugin Protocol

- [ ] Define JSON-RPC protocol for hook plugins
- [ ] Implement `HookPluginManager` (spawn once, keep alive)
- [ ] Wire hook events from `HookExecutor` to plugin stdin
- [ ] Read allow/deny decisions from plugin stdout
- [ ] Timeout handling for hook responses

### Phase 4: Admin + Dashboard Integration

- [ ] `admin_manage_plugins` tool for adminbot
- [ ] Plugin status in dashboard (installed, active, gating failures)
- [ ] Plugin install/remove via admin API
- [ ] Plugin config editing via dashboard

### Phase 5: Plugin Distribution

- [ ] Plugin registry (GitHub-based or custom)
- [ ] `crew plugin install <url>` CLI command
- [ ] Integrity verification (sha256 in manifest)
- [ ] Auto-update mechanism

---

## File Layout (Implementation)

```
crates/
├── crew-plugin/                    # New crate: Plugin SDK types + protocol
│   ├── src/
│   │   ├── lib.rs                  # Public API
│   │   ├── manifest.rs             # Manifest parsing + validation
│   │   ├── discovery.rs            # Plugin discovery + precedence
│   │   ├── gating.rs               # Requirements checking
│   │   ├── protocol.rs             # JSON-RPC types (shared)
│   │   └── types.rs                # PluginType, PluginStatus, etc.
│   └── Cargo.toml
│
├── crew-cli/src/
│   ├── plugin_manager.rs           # Plugin lifecycle management
│   ├── channel_plugin.rs           # Channel plugin subprocess manager
│   └── hook_plugin.rs              # Hook plugin subprocess manager
│
├── crew-agent/src/
│   └── tools/admin/plugins.rs      # Admin plugin management tools
```

---

## Example: Writing a Channel Plugin (Rust)

```rust
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    method: String,
    id: Option<u64>,
    params: serde_json::Value,
}

#[derive(Serialize)]
struct RpcResponse {
    jsonrpc: String,
    id: Option<u64>,
    result: serde_json::Value,
}

#[derive(Serialize)]
struct RpcNotification {
    jsonrpc: String,
    method: String,
    params: serde_json::Value,
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line.expect("stdin closed");
        let req: RpcRequest = serde_json::from_str(&line).expect("invalid JSON-RPC");

        match req.method.as_str() {
            "init" => {
                // Connect to external service using params.config
                let resp = RpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: serde_json::json!({"ok": true}),
                };
                writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
                stdout.flush().unwrap();

                // Start receiving messages (in real code, spawn a thread)
            }
            "send" => {
                // Send message to external service
                let chat_id = req.params["chat_id"].as_str().unwrap_or("");
                let text = req.params["text"].as_str().unwrap_or("");

                // ... send to Matrix/Slack/etc ...

                let resp = RpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: serde_json::json!({"ok": true, "message_id": "msg_123"}),
                };
                writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
                stdout.flush().unwrap();
            }
            "shutdown" => {
                // Disconnect and exit
                let resp = RpcResponse {
                    jsonrpc: "2.0".into(),
                    id: req.id,
                    result: serde_json::json!({"ok": true}),
                };
                writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
                stdout.flush().unwrap();
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown method: {}", req.method);
            }
        }
    }
}
```

To emit an incoming message (from plugin to gateway):

```rust
// Called from message-receiving thread
fn emit_message(from: &str, text: &str, chat_id: &str) {
    let notification = RpcNotification {
        jsonrpc: "2.0".into(),
        method: "message".into(),
        params: serde_json::json!({
            "from": from,
            "text": text,
            "channel": "matrix",
            "chat_id": chat_id,
        }),
    };
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", serde_json::to_string(&notification).unwrap()).unwrap();
    stdout.flush().unwrap();
}
```

## Example: Writing a Channel Plugin (Python)

```python
#!/usr/bin/env python3
import json, sys

def respond(id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}), flush=True)

def notify(method, params):
    print(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}), flush=True)

for line in sys.stdin:
    req = json.loads(line)
    method = req["method"]

    if method == "init":
        # Connect to service
        respond(req.get("id"), {"ok": True})
    elif method == "send":
        # Send message
        respond(req.get("id"), {"ok": True, "message_id": "123"})
    elif method == "shutdown":
        respond(req.get("id"), {"ok": True})
        sys.exit(0)
```

---

## Design Principles

1. **Subprocess isolation** — Plugins run as separate processes. A crashing plugin never takes down the gateway.
2. **Language-agnostic** — Any language that reads stdin and writes stdout works. Rust, Python, Go, shell, Node.js.
3. **Manifest-first** — Plugin capabilities are declared in `manifest.json`, validated without executing code.
4. **Backward compatible** — Current app-skills and shell hooks continue working. Migration is additive.
5. **Secure by default** — Process isolation, env sanitization, timeout enforcement, binary integrity checks.
6. **Simple protocol** — JSON-RPC on stdio. No sockets, no HTTP servers, no IPC complexity.
