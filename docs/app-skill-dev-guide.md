# Octos App Skill Development Guide

[English](app-skill-dev-guide.md) | [中文](app-skill-dev-guide-zh.md)

This guide covers everything you need to build, register, and deploy a new app skill for octos.

---

## Architecture Overview

An app skill is a **standalone executable binary** that communicates with the octos gateway via a simple **stdin/stdout JSON protocol**. The gateway spawns the skill binary as a child process for each tool call, passes JSON arguments on stdin, and reads JSON results from stdout.

```
User message → LLM → tool_use("get_weather", {"city": "Paris"})
                         ↓
              Gateway spawns: ~/.octos/skills/weather/main get_weather
                         ↓
              Stdin:  {"city": "Paris"}
              Stdout: {"output": "Paris, France\nClear sky\n...", "success": true}
                         ↓
              LLM sees result → generates natural language response
```

---

## Skill Directory Structure

Each skill lives in its own crate under `crates/app-skills/`:

```
crates/app-skills/my-skill/
├── Cargo.toml          # Crate config, binary name
├── manifest.json       # Tool definitions (JSON Schema)
├── SKILL.md            # Documentation + frontmatter metadata
└── src/
    └── main.rs         # Binary entry point
```

After bootstrapping, the skill is installed at:

```
~/.octos/skills/my-skill/
├── main                # Executable binary (copied from target/)
├── manifest.json       # Tool definitions
└── SKILL.md            # Documentation
```

---

## Step-by-Step: Create a New Skill

### 1. Create the Crate

```bash
mkdir -p crates/app-skills/my-skill/src
```

### 2. Cargo.toml

```toml
[package]
name = "my-skill"
version = "1.0.0"
edition = "2021"
description = "Short description of what this skill does"
authors = ["your-name"]

[[bin]]
name = "my_skill"          # Binary name (used in bundled_app_skills.rs)
path = "src/main.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# Add other deps as needed:
# reqwest = { version = "0.12", features = ["blocking", "rustls-tls", "json"], default-features = false }
# chrono = "0.4"
```

**Important:** The `[[bin]] name` must match the `binary_name` in `bundled_app_skills.rs`.

### 3. manifest.json

Defines the tools the LLM can call. Uses JSON Schema for input validation.

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "author": "your-name",
  "description": "What this skill does",
  "timeout_secs": 15,
  "requires_network": false,
  "tools": [
    {
      "name": "my_tool",
      "description": "Clear description for the LLM. What does this tool do? When should it be used?",
      "input_schema": {
        "type": "object",
        "properties": {
          "param1": {
            "type": "string",
            "description": "What this parameter means"
          },
          "param2": {
            "type": "integer",
            "description": "Optional numeric parameter (default: 10)"
          }
        },
        "required": ["param1"]
      }
    }
  ]
}
```

**Manifest fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | — | Skill identifier |
| `version` | Yes | — | Semantic version |
| `author` | No | — | Author name |
| `description` | No | — | Human-readable description |
| `timeout_secs` | No | 30 | Max execution time per tool call (1-600) |
| `requires_network` | No | false | Informational flag |
| `sha256` | No | — | Binary integrity check (hex hash) |
| `tools` | Yes | — | Array of tool definitions |

**Tool definition fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Tool name (snake_case, globally unique) |
| `description` | Yes | Description shown to LLM — be specific about when to use |
| `input_schema` | Yes | JSON Schema for input parameters |

### 4. SKILL.md

Documentation with YAML frontmatter. The LLM reads this to understand when and how to use the skill.

```markdown
---
name: my-skill
description: Short description. Triggers: keyword1, keyword2, 关键词, trigger phrase.
version: 1.0.0
author: your-name
always: false
---

# My Skill

Detailed description of what this skill does and when to use it.

## Tools

### my_tool

Explain what this tool does with examples.

\```json
{"param1": "example value", "param2": 5}
\```

**Parameters:**
- `param1` (required): What it means
- `param2` (optional): What it controls. Default: 10
```

**Frontmatter fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | — | Skill identifier |
| `description` | Yes | — | One-line description. Include trigger keywords after "Triggers:" |
| `version` | Yes | — | Semantic version |
| `author` | No | — | Author name |
| `always` | No | `false` | If `true`, skill docs are always included in system prompt |
| `requires_bins` | No | — | Comma-separated binaries that must exist (checked via `which`) |
| `requires_env` | No | — | Comma-separated env vars that must be set |

**Trigger keywords** help the agent decide when to activate the skill. Include terms in multiple languages if your users are multilingual.

### 5. src/main.rs

The binary implements the stdin/stdout protocol.

**Minimal template:**

```rust
use std::io::Read;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct MyToolInput {
    param1: String,
    #[serde(default = "default_param2")]
    param2: i32,
}

fn default_param2() -> i32 { 10 }

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "my_tool" => handle_my_tool(&buf),
        _ => fail(&format!("Unknown tool '{tool_name}'. Expected: my_tool")),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn handle_my_tool(input_json: &str) {
    let input: MyToolInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    // ... your logic here ...

    let result = format!("Processed {} with param2={}", input.param1, input.param2);
    println!("{}", json!({"output": result, "success": true}));
}
```

**Protocol rules:**

1. **argv[1]** = tool name (e.g., `get_weather`, `get_forecast`)
2. **stdin** = JSON object matching the tool's `input_schema`
3. **stdout** = JSON object with:
   - `output` (string): Human-readable result text
   - `success` (bool): `true` for success, `false` for failure
4. **exit code**: 0 for success, non-zero for failure
5. **stderr**: Ignored by the gateway (use for debug logging)

---

## Register the Skill

### 6. Add to Workspace

In the root `Cargo.toml`, add to `members`:

```toml
[workspace]
members = [
    # ... existing members ...
    "crates/app-skills/my-skill",
]
```

### 7. Register in bundled_app_skills.rs

In `crates/octos-agent/src/bundled_app_skills.rs`, add to `BUNDLED_APP_SKILLS`:

```rust
pub const BUNDLED_APP_SKILLS: &[(&str, &str, &str, &str)] = &[
    // ... existing skills ...
    (
        "my-skill",                                          // dir_name (skill directory name)
        "my_skill",                                          // binary_name (must match [[bin]] name)
        include_str!("../../app-skills/my-skill/SKILL.md"),  // embedded docs
        include_str!("../../app-skills/my-skill/manifest.json"), // embedded manifest
    ),
];
```

**Tuple format:** `(dir_name, binary_name, skill_md, manifest_json)`

- `dir_name`: Name of the directory under `~/.octos/skills/`
- `binary_name`: Name of the binary in `target/release/` (must match `[[bin]] name` in Cargo.toml)
- `skill_md`: Embedded SKILL.md content
- `manifest_json`: Embedded manifest.json content

---

## Build & Test

### 8. Build

```bash
# Build just your skill
cargo build -p my-skill

# Build everything
cargo build --workspace
```

### 9. Test Standalone

```bash
# Test your tool directly
echo '{"param1": "hello", "param2": 5}' | ./target/debug/my_skill my_tool

# Expected output:
# {"output":"Processed hello with param2=5","success":true}

# Test error handling
echo '{}' | ./target/debug/my_skill my_tool
echo '{"param1": "test"}' | ./target/debug/my_skill unknown_tool
```

### 10. Test with Gateway

```bash
# Build release + install
cargo build --release --workspace

# Start gateway (skills are bootstrapped automatically)
octos gateway

# Check skill was loaded
ls ~/.octos/skills/my-skill/
# main  manifest.json  SKILL.md

# Ask the agent to use your skill
```

---

## Examples

### Example 1: Local-Only Skill (Clock)

No network, no env vars. Uses `chrono` + `chrono-tz`.

```
crates/app-skills/time/
├── Cargo.toml          # deps: chrono, chrono-tz, serde, serde_json
├── manifest.json       # 1 tool: get_time, timeout_secs: 5
├── SKILL.md            # Triggers: time, clock, 几点
└── src/main.rs         # Reads system clock, formats with timezone
```

**Key pattern:** Default to local time when no timezone given.

### Example 2: Network Skill (Weather)

Calls external API, needs network. Uses `reqwest` (blocking).

```
crates/app-skills/weather/
├── Cargo.toml          # deps: reqwest (blocking, rustls-tls), serde, serde_json
├── manifest.json       # 2 tools: get_weather, get_forecast, timeout_secs: 15
├── SKILL.md            # Triggers: weather, forecast, 天气
└── src/main.rs         # Geocode city → fetch weather from Open-Meteo
```

**Key patterns:**
- Build HTTP client with timeouts
- Handle API errors gracefully (return `success: false`)
- URL-encode user input
- Multiple tools in one binary (match on `argv[1]`)

### Example 3: Env-Var Skill (Send Email)

Requires credentials from environment variables.

```
crates/app-skills/send-email/
├── Cargo.toml          # deps: lettre, serde, serde_json, reqwest
├── manifest.json       # 1 tool: send_email
├── SKILL.md            # requires_env: SMTP_HOST,SMTP_USERNAME,SMTP_PASSWORD
└── src/main.rs         # Reads SMTP_* env vars, sends via SMTP
```

**Key pattern:** Check env vars early, fail with clear error message.

```rust
fn get_smtp_config() -> SmtpConfig {
    let host = std::env::var("SMTP_HOST")
        .unwrap_or_else(|_| fail("SMTP_HOST env var not set"));
    // ...
}
```

---

## Manifest Extensions: MCP Servers, Hooks, and Prompt Fragments

Skills can declare more than just tools in `manifest.json`. Three additional extension points let a skill provide MCP servers, lifecycle hooks, and system prompt content. These are collectively called **extras**.

### MCP Servers

A skill can declare MCP (Model Context Protocol) servers that the gateway auto-starts when the skill loads. This lets a skill expose tools via the MCP protocol instead of (or in addition to) the stdin/stdout binary protocol.

Add an `mcp_servers` array to `manifest.json`:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "tools": [],
  "mcp_servers": [
    {
      "command": "node",
      "args": ["mcp-server/index.js"],
      "env": ["API_KEY", "API_SECRET"]
    }
  ]
}
```

**MCP server fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `command` | No* | Command to spawn the MCP server process |
| `args` | No | Arguments passed to the command |
| `env` | No | List of environment variable **names** (not values) to forward |
| `url` | No* | HTTP transport: URL of a remote MCP server endpoint |
| `headers` | No | HTTP transport: additional headers (key-value object) |

\* Exactly one of `command` or `url` should be set. Use `command` for local (stdio) MCP servers and `url` for remote (HTTP) MCP servers.

**Path resolution:** If `command` starts with `./` or `../`, it is resolved relative to the skill directory. Bare commands (e.g. `"node"`, `"python3"`) are looked up on `PATH` as usual.

**Environment forwarding:** The `env` array contains environment variable *names*, not values. At load time, each name is looked up in the process environment. Only variables that are actually set are forwarded to the MCP server process. Variables that are missing are silently omitted.

**Example: local stdio MCP server**

```json
{
  "mcp_servers": [
    {
      "command": "./bin/mcp-server",
      "args": ["--port", "0"],
      "env": ["DATABASE_URL"]
    }
  ]
}
```

**Example: remote HTTP MCP server**

```json
{
  "mcp_servers": [
    {
      "url": "https://mcp.example.com/v1",
      "headers": {
        "Authorization": "Bearer ${API_KEY}"
      }
    }
  ]
}
```

---

### Lifecycle Hooks

A skill can declare lifecycle hooks that run shell commands when specific agent events occur. This is useful for auditing, policy enforcement, or side effects.

Add a `hooks` array to `manifest.json`:

```json
{
  "name": "my-audit-skill",
  "version": "1.0.0",
  "tools": [],
  "hooks": [
    {
      "event": "after_tool_call",
      "command": ["./hooks/audit.sh"],
      "timeout_ms": 5000,
      "tool_filter": ["shell"]
    }
  ]
}
```

**Hook fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `event` | Yes | -- | Lifecycle event name (see table below) |
| `command` | Yes | -- | Command as an argv array (no shell interpretation) |
| `timeout_ms` | No | 5000 | Maximum execution time in milliseconds |
| `tool_filter` | No | `[]` (all tools) | Only trigger for these tool names (tool events only) |

**Supported events:**

| Event | Can Deny? | When it fires |
|-------|-----------|---------------|
| `before_tool_call` | Yes | Before a tool is executed. Exit code 1 = deny. |
| `after_tool_call` | No | After a tool finishes (success or failure). |
| `before_llm_call` | Yes | Before sending a request to the LLM. Exit code 1 = deny. |
| `after_llm_call` | No | After the LLM response is received. |

**Path resolution:** The first element of the `command` array (`command[0]`) follows the same rules as MCP servers -- paths starting with `./` or `../` are resolved against the skill directory. Other elements are passed as-is.

**Hook payload:** The gateway sends a JSON payload on stdin to the hook process. For tool events, the payload includes `tool_name`, `arguments`, and session context. For LLM events, it includes `model`, `message_count`, etc.

**Deny behavior:** `before_*` hooks can deny the operation by exiting with code 1. The hook's stdout is included as the denial reason.

**Example: audit all shell tool calls**

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["./hooks/policy-check.sh"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "bash"]
    },
    {
      "event": "after_tool_call",
      "command": ["./hooks/audit-log.sh"],
      "timeout_ms": 5000,
      "tool_filter": ["shell", "bash"]
    }
  ]
}
```

---

### Prompt Fragments

A skill can inject content into the system prompt by declaring prompt fragment files. This is useful for teaching the agent domain-specific knowledge, rules, or behavior without writing any code.

Add a `prompts` object to `manifest.json`:

```json
{
  "name": "my-style-guide",
  "version": "1.0.0",
  "tools": [],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

**Prompt fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `include` | Yes | Array of glob patterns for files to include |

**Path resolution:** Glob patterns are resolved relative to the skill directory. For example, `"prompts/*.md"` matches all `.md` files in the `prompts/` subdirectory of the skill.

**Behavior:** Matched files are read at load time and their content is appended to the system prompt. Files are processed in glob expansion order.

**Example: skill directory layout**

```
~/.octos/skills/my-style-guide/
├── manifest.json
├── SKILL.md
└── prompts/
    ├── coding-rules.md
    └── review-checklist.md
```

With manifest:

```json
{
  "name": "my-style-guide",
  "version": "1.0.0",
  "tools": [],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

Both `coding-rules.md` and `review-checklist.md` are injected into the system prompt whenever this skill is active.

---

### Extras-Only Skills

A skill does not need to provide any tool executables. If `manifest.json` has an empty `tools` array (or omits it entirely) but declares `mcp_servers`, `hooks`, or `prompts`, the gateway loads the extras without looking for a binary. This is useful for:

- **Pure prompt injection skills** -- a collection of `.md` files that teach the agent a domain
- **Configuration skills** -- hooks that enforce policies across all tool calls
- **Remote MCP skills** -- MCP servers that run elsewhere, declared via `url`

**Example: prompt-only skill**

```json
{
  "name": "company-policy",
  "version": "1.0.0",
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

No `tools`, no binary, no `mcp_servers`, no `hooks` -- just prompt content.

**Example: hooks-only skill**

```json
{
  "name": "audit-logger",
  "version": "1.0.0",
  "hooks": [
    {
      "event": "after_tool_call",
      "command": ["./hooks/log-to-siem.sh"],
      "timeout_ms": 5000
    }
  ]
}
```

No tools -- the skill only provides an audit hook.

**Example: combined extras**

A single skill can declare all three extras alongside regular tools:

```json
{
  "name": "advanced-skill",
  "version": "1.0.0",
  "tools": [
    { "name": "analyze", "description": "Run analysis", "input_schema": { "type": "object" } }
  ],
  "mcp_servers": [
    { "command": "node", "args": ["mcp/server.js"], "env": ["API_KEY"] }
  ],
  "hooks": [
    { "event": "after_tool_call", "command": ["./hooks/audit.sh"], "tool_filter": ["analyze"] }
  ],
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

---

### Updated Manifest Field Reference

The complete set of top-level `manifest.json` fields, including extensions:

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | -- | Skill identifier |
| `version` | Yes | -- | Semantic version |
| `author` | No | -- | Author name |
| `description` | No | -- | Human-readable description |
| `timeout_secs` | No | 30 | Max execution time per tool call (1-600) |
| `requires_network` | No | false | Informational flag |
| `sha256` | No | -- | Binary integrity check (hex hash) |
| `tools` | No | `[]` | Array of tool definitions |
| `mcp_servers` | No | `[]` | Array of MCP server declarations |
| `hooks` | No | `[]` | Array of lifecycle hook definitions |
| `prompts` | No | -- | Prompt fragment configuration object |
| `binaries` | No | `{}` | Pre-built binaries keyed by `{os}-{arch}` |

---

## Advanced Topics

### Multiple Tools in One Skill

A single skill binary can implement multiple tools. The tool name is passed as `argv[1]`:

```rust
match tool_name {
    "get_weather" => handle_get_weather(&buf),
    "get_forecast" => handle_get_forecast(&buf),
    _ => fail(&format!("Unknown tool '{tool_name}'")),
}
```

Each tool must be declared in `manifest.json`:

```json
{
  "tools": [
    { "name": "get_weather", "description": "...", "input_schema": { ... } },
    { "name": "get_forecast", "description": "...", "input_schema": { ... } }
  ]
}
```

### Environment Variables

Skills inherit the gateway's environment (minus blocked vars). To use API keys:

```rust
let api_key = std::env::var("MY_API_KEY")
    .unwrap_or_else(|_| fail("MY_API_KEY not set"));
```

Declare requirements in SKILL.md frontmatter so the skill is marked unavailable when env vars are missing:

```yaml
---
requires_env: MY_API_KEY
---
```

### Timeout Configuration

Set appropriate timeouts in `manifest.json`:

| Skill Type | Recommended Timeout |
|------------|-------------------|
| Local computation | 5s |
| Single API call | 15s |
| Multi-step API calls | 30-60s |
| Long-running research | 300-600s |

### Security

**Binary integrity:**

- **Symlinks rejected:** Plugin binaries must be regular files. Symlinks are rejected at load time as a defense against link-swap attacks. The loader uses `symlink_metadata()` (not `metadata()`) to detect this.
- **SHA-256 verification:** If `sha256` is present in `manifest.json`, the loader computes the hash of the binary and rejects it if the hash does not match. The verified bytes are written to a separate file that the gateway actually executes, closing the TOCTOU (time-of-check to time-of-use) gap.
- **Size limit:** Plugin executables must be under 100 MB. Larger binaries are rejected before being read.

**Environment sanitization:**

The gateway automatically strips these env vars before spawning skill processes:

- `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`
- `NODE_OPTIONS`, `PYTHONPATH`, `PERL5LIB`
- `RUSTFLAGS`, `RUST_LOG`
- And 10+ others (see `BLOCKED_ENV_VARS` in `sandbox.rs`)

**Best practices for skill authors:**

- Validate all input (never trust `city`, `path`, etc.)
- Use timeouts on HTTP requests
- Avoid shell injection (don't pass user input to shell commands)
- Set `sha256` in `manifest.json` for release builds to enable integrity verification

### Platform Skills vs App Skills

| | App Skills | Platform Skills |
|---|---|---|
| **Location** | `crates/app-skills/` | `crates/platform-skills/` |
| **Array** | `BUNDLED_APP_SKILLS` | `PLATFORM_SKILLS` |
| **Bootstrap** | Every gateway startup | Admin bot only |
| **Scope** | Per-gateway | Shared across all gateways |
| **Use when** | Always available, self-contained | Requires external service |

### Updating Skills Without Full Rebuild

Skills can be rebuilt and deployed independently:

```bash
# Build just the skill
cargo build --release -p weather

# Copy to remote server
scp target/release/weather remote:~/.octos/skills/weather/main

# No gateway restart needed — next tool call uses the new binary
```

Note: If you change `SKILL.md` or `manifest.json`, you must rebuild the `octos` binary too (they're embedded via `include_str!`).

---

## Installation & Distribution

### Skill Types

| Type | Location | Install Method | Binary | Use Case |
|------|----------|---------------|--------|----------|
| **Bundled** | `crates/app-skills/` | Compiled into `octos` binary | Embedded | Core skills shipped with every release |
| **External** | GitHub repo | `octos skills install user/repo` | Downloaded or built | Community/custom skills |
| **Profile-local** | `<profile-data>/skills/` | Per-profile install | Self-contained | Tenant-isolated skills |

### Per-Profile Skill Management

Skills are installed per-profile to ensure tenant isolation. Each profile has its own skills directory:

```
~/.octos/profiles/alice/data/
  skills/
    mofa-comic/
      main              ← binary (self-contained, NOT in ~/.cargo/bin)
      SKILL.md
      manifest.json
      styles/*.toml     ← bundled assets
    mofa-slides/
      main
      SKILL.md
      manifest.json
      styles/*.toml
```

**Important:** Skill binaries stay in their skill directory as `main`. They are NOT copied to `~/.cargo/bin/` or any global location. The plugin loader finds them at `<skill-dir>/main`.

### Install/Remove/List Commands

All surfaces support per-profile operation:

```bash
# CLI (--profile flag goes BEFORE subcommand)
octos skills --profile alice install mofa-org/mofa-skills/mofa-comic
octos skills --profile alice list
octos skills --profile alice remove mofa-comic

# In-chat (automatically uses current profile)
/skills install mofa-org/mofa-skills/mofa-comic
/skills list
/skills remove mofa-comic

# Admin API
POST /api/admin/profiles/alice/skills     {"repo": "mofa-org/mofa-skills/mofa-comic"}
GET  /api/admin/profiles/alice/skills
DELETE /api/admin/profiles/alice/skills/mofa-comic

# Agent tool (automatically uses current profile)
manage_skills(action="install", repo="mofa-org/mofa-skills/mofa-comic")
manage_skills(action="list")
manage_skills(action="remove", name="mofa-comic")
manage_skills(action="search", query="comic")
```

### Skill Loading Priority

The gateway loads skills from multiple directories. First match wins on name conflict:

1. `<profile-data>/skills/` — per-profile (highest priority)
2. `<project-dir>/skills/` — project-local
3. `<project-dir>/bundled-skills/` — bundled app-skills
4. `~/.octos/skills/` — global (lowest priority)

### Publishing to the Registry

External skills are discoverable via the [octos-hub](https://github.com/octos-org/octos-hub) registry.

1. Push your skill repo to GitHub
2. Add an entry to `registry.json` via PR:

```json
{
  "name": "my-skills",
  "description": "What your skills do",
  "repo": "your-user/your-repo",
  "skills": ["skill-a", "skill-b"],
  "requires": ["git", "cargo"],
  "tags": ["keyword1", "keyword2"]
}
```

3. Users can then find and install your skills:

```bash
octos skills search keyword1
octos skills --profile alice install your-user/your-repo/skill-a
```

### Pre-built Binary Distribution

For faster installs (skip compilation), add a `binaries` section to `manifest.json`:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "binaries": {
    "darwin-aarch64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-darwin-aarch64.tar.gz",
      "sha256": "abc123..."
    },
    "darwin-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-darwin-x86_64.tar.gz",
      "sha256": "def456..."
    },
    "linux-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/skill-linux-x86_64.tar.gz",
      "sha256": "789ghi..."
    }
  },
  "tools": [ ... ]
}
```

The installer downloads the matching binary, verifies SHA-256, and extracts to `<skill-dir>/main`. Falls back to `cargo build --release` if no binary is available.

### Environment Variables for Skills

The gateway automatically injects API keys into plugin processes:

- Primary provider's API key (e.g., `DASHSCOPE_API_KEY`)
- Fallback provider keys (e.g., `GEMINI_API_KEY`, `OPENAI_API_KEY`)
- Base URLs for non-standard endpoints
- `OCTOS_DATA_DIR` and `OCTOS_WORK_DIR`

Keys are resolved from the macOS Keychain at gateway startup. Skill binaries receive them as environment variables — no manual export needed.

### Bundled Assets (Styles, Config)

Skills that include asset files (styles, templates, config) should bundle them in the skill directory:

```
my-skill/
  main
  SKILL.md
  manifest.json
  styles/
    default.toml
    manga.toml
  templates/
    report.html
```

The binary should resolve assets relative to its own executable location:

```rust
let exe = std::env::current_exe()?;
let skill_dir = exe.parent().unwrap();
let styles_dir = skill_dir.join("styles");
```

**Do NOT** look for assets in the working directory (cwd) — it points to the profile's data dir, not the skill dir.

---

## Checklist

### For tool skills (binary + tools)

- [ ] Create `crates/app-skills/<name>/` with Cargo.toml, manifest.json, SKILL.md, src/main.rs
- [ ] `[[bin]] name` in Cargo.toml matches `binary_name` in bundled_app_skills.rs
- [ ] manifest.json has valid JSON Schema for all tool inputs
- [ ] SKILL.md has frontmatter with trigger keywords
- [ ] Binary reads `argv[1]` for tool name, stdin for JSON input
- [ ] Binary writes `{"output": "...", "success": true/false}` to stdout
- [ ] Error cases return `success: false` with clear message
- [ ] Add to workspace `Cargo.toml` members
- [ ] Add to `BUNDLED_APP_SKILLS` in `bundled_app_skills.rs`
- [ ] `cargo build --workspace` succeeds
- [ ] Standalone test: `echo '{"param": "value"}' | ./target/debug/my_skill my_tool`
- [ ] Gateway test: skill appears in `~/.octos/skills/` and agent can use it

### For extras (MCP servers, hooks, prompt fragments)

- [ ] `mcp_servers`: `command` or `url` is set; `env` lists only variable names, not values
- [ ] `mcp_servers`: relative command paths (`./bin/server`) exist in the skill directory
- [ ] `hooks`: `event` is one of `before_tool_call`, `after_tool_call`, `before_llm_call`, `after_llm_call`
- [ ] `hooks`: `command` is an argv array (not a shell string); `command[0]` relative paths resolve correctly
- [ ] `hooks`: `tool_filter` is set when the hook should only apply to specific tools
- [ ] `prompts`: glob patterns in `include` match the intended `.md` files in the skill directory
- [ ] Extras-only skills: `tools` array is empty or omitted; no binary needed
- [ ] Gateway test: extras appear in loader logs (`loaded skill extras`)
