# Skill Development

This guide covers the full lifecycle of an Octos skill — from development to publication to end-user installation — similar to building an app, submitting it to an app store, and distributing it to users.

---

## The Skill Ecosystem

```
 Developer                    Octos Hub                     User
 ─────────                    ─────────                     ────
 1. Develop skill        ──▶  3. Publish to registry   ──▶  5. Search & discover
 2. Test locally              4. Pre-built binaries         6. Install
                                                            7. Update
```

| Concept | App Store Analogy | Octos Equivalent |
|---------|-------------------|------------------|
| **App** | iOS/Android app | Skill (binary + manifest + docs) |
| **SDK** | Xcode / Android Studio | Rust + `manifest.json` + `SKILL.md` |
| **App Store** | Apple App Store | [octos-hub](https://github.com/octos-org/octos-hub) registry |
| **Distribution** | App Store binary delivery | Pre-built binaries in GitHub Releases |
| **Install** | Tap "Get" | `octos skills install user/repo` |
| **Sideload** | Ad-hoc / TestFlight | Copy to `~/.octos/skills/` directly |

---

## Before You Start: Skill vs. Workspace Contract

Before writing a skill, decide whether the logic belongs in a **plugin tool** (external binary, what this guide covers) or in the **in-process workspace contract** framework (`workspace_policy.toml` + `ValidatorSpec` variants).

These two surfaces look similar — both gate tool behavior — but they serve different concerns and have very different reliability characteristics. Putting logic in the wrong place is the most common mistake we see.

### Decision matrix

| Axis | Workspace contract (host, in-process Rust) | Plugin tool / lifecycle hook (external binary) |
|---|---|---|
| **Correctness criticality** | Load-bearing — must always run | Best-effort enrichment; tool still works if hook fails to load |
| **Stability** | Stable contract; same shape across deployments | Frequently customized per-profile / per-deployment |
| **Performance** | Hot path; runs on every tool call, in-process | Cold / occasional; spawns a subprocess + JSON IPC |
| **Failure semantics** | Fails the tool with a typed `SpawnTaskContractResult::Failed` | Failure is logged; execution proceeds |
| **Cross-cutting reach** | Generic — applies to many tools (file format, integrity, content) | Tool-specific behavior or capability implementation |
| **Customization scope** | System invariant, not user-tunable | User-configurable; ships and versions independently of host |

If **all five** axes point to the same column, build there. Mixed signals → use the two tiebreakers below.

**Tiebreaker A — "Can the user disable it safely?"**
- Yes → plugin (user-controllable, low blast radius)
- No (system invariant) → workspace contract

**Tiebreaker B — "Is the language / runtime constrained?"**
- Pure Rust data ops, file I/O, HTTP — host (workspace contract)
- Needs Python ecosystem, native CLI, GPU/Metal, cloud SDK with hairy auth — plugin

### Where existing surface lives

These belong in **workspace contract** (`workspace_policy.toml`, see `crates/octos-agent/src/workspace_policy.rs::ValidatorSpec`):

- **`AudioNonSilent`** / **`PerFileNonSilent`** — TTS output content invariant. The system MUST refuse to claim "audio generated" if the `.wav` is silent. Applies to every TTS-emitting skill regardless of vendor.
- **`MagicBytes`** — file format integrity. Applies to every tool that emits files.
- **`Sha256Match`** — supply-chain / artifact integrity. Must always verify.
- **`HttpProbeUntil`** — deploy / health gate after a publish step.
- **`FileExists`** — basic deliverable contract.

These belong as **plugin tools** (this guide):

- **`fm_tts`**, **`mofa_slides`**, **`mofa_publish`**, **`search`** (formerly `deep_search`) — the work itself. Each is the capability implementation, often wraps an external runtime (Python, Chromium, native CLI), ships versioned independently of the host.
- **`qwen-tts`** voice clone — wraps an external HTTP API with auth; per-tenant credentials; ergonomic to update without recompiling octos.

These belong as **plugin hooks** ONLY when they are **optional enrichment** (the tool would still work without them):

- **Metrics / audit** hooks (after_tool_call): log cost / latency to an external system. Failure is fine — the tool still ran.
- **Channel-side notifications**: ping Slack on completion. Optional.

### The pipeline-guard case study (and what we'd build differently)

**`pipeline-guard`** today is shipped as a plugin (`crates/app-skills/pipeline-guard/`) with a `before_tool_call` hook that mutates the DOT graph the LLM authored for `run_pipeline` — injecting `model="cheap"` on `dynamic_parallel` workers and `model="strong"` on `synthesize` nodes from the live QoS catalog.

Score it against the matrix:

| Axis | pipeline-guard score |
|---|---|
| Correctness criticality | **Load-bearing** — without it, every pipeline node hits the same default model regardless of cost/quality |
| Stability | Stable — same shape on every deploy; logic is "fill in `node.model` from `model_catalog.json`" |
| Performance | Hot path — runs on every `run_pipeline` call |
| Failure semantics | **Currently silent on failure** — manifest-parse failures or missing binary degrade to "no model assignment" with no user-visible error |
| Cross-cutting reach | run_pipeline-specific |
| Customization scope | System invariant — users don't pick which DOT nodes get which model |
| Language constraint | Pure Rust data ops on a parsed `PipelineGraph` |

**All five primary axes (and both tiebreakers) point to host / in-process — not plugin.** The lifecycle-hook plumbing is a category mismatch. In practice it's been the failure surface: manifest-parse errors fire on every daemon start, and when they do, the hook silently doesn't run.

**The correct home is inline in `octos-pipeline` itself.** Recommended shape (not yet landed at time of writing):

```rust
// In RunPipelineTool::execute, after parse_dot:
octos_pipeline::model_assignment::assign_from_catalog(
    &mut graph,
    &model_catalog, // passed via ProfileRuntime → RunPipelineTool
)?;
```

A small (~50-line) deterministic in-process function with unit tests beats a separate binary + JSON IPC + plugin loader + manifest parser, for logic that is correctness-critical, non-customizable, and never needs to ship independently of the host.

**Generalization** — if future tools need similar pre-call mutation (e.g. `mofa_slides` enforcing a style default), the workspace contract framework should grow a new variant rather than each tool adding its own hook plugin:

```rust
pub enum ValidatorSpec {
    // existing post-execution validators...
    AudioNonSilent { ... },
    MagicBytes { ... },
    HttpProbeUntil { ... },

    // proposed: pre-call argument mutator
    PreCall {
        mutate_args: PreCallMutator,
    },
}
```

This keeps integrity-critical logic in one in-process channel with a typed failure path, instead of scattered across plugin binaries with best-effort semantics.

### Quick rubric

```
   correctness-critical?
        ┌──────┴──────┐
       yes            no
        │             │
   stable shape?     plugin hook (best-effort)
        ┌──┴──┐
       yes   no
        │    │
   workspace plugin tool
   contract  (customized capability)
```

If you find yourself reaching for a plugin hook to enforce an invariant the user must never override — stop, file an issue against `octos-agent`, and consider whether a workspace contract variant is the right home instead.

---

## Part 1: Develop

### Architecture

A skill is a **standalone executable** that communicates via **stdin/stdout JSON**. The gateway spawns it as a child process for each tool call. Skills can be written in **any language** — Rust, Python, Node.js, shell, etc.

```
User message → LLM → tool_use("get_weather", {"city": "Paris"})
                        ↓
             Gateway spawns: ~/.octos/skills/weather/main get_weather
                        ↓
             Stdin:  {"city": "Paris"}
             Stdout: {"output": "25°C, sunny", "success": true}
                        ↓
             LLM sees result → generates response
```

### Skill Anatomy

Every skill is a directory with three files:

```
my-skill/
├── manifest.json       # Tool definitions (JSON Schema) — the "API contract"
├── SKILL.md            # Documentation + metadata — the "app description"
├── main                # Executable binary — the "app binary"
└── (optional extras)
    ├── styles/         # Bundled assets
    ├── prompts/*.md    # System prompt fragments
    └── hooks/          # Lifecycle hook scripts
```

### Step 1: Create manifest.json

The manifest declares what tools the skill provides. The LLM reads this to decide when and how to call your skill.

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
| `tools` | No | `[]` | Array of tool definitions |
| `mcp_servers` | No | `[]` | MCP server declarations |
| `hooks` | No | `[]` | Lifecycle hook definitions |
| `prompts` | No | — | Prompt fragment config |
| `binaries` | No | `{}` | Pre-built binaries by `{os}-{arch}` |

### Step 2: Create SKILL.md

Documentation with YAML frontmatter. The LLM reads this to understand context and trigger conditions.

```markdown
---
name: my-skill
description: Short description. Triggers: keyword1, keyword2, trigger phrase.
version: 1.0.0
author: your-name
always: false
---

# My Skill

Detailed description of what this skill does and when to use it.

## Tools

### my_tool

Explain what this tool does with examples.

**Parameters:**
- `param1` (required): What it means
- `param2` (optional): What it controls. Default: 10
```

**Frontmatter fields:**

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | Yes | — | Skill identifier |
| `description` | Yes | — | One-line description with trigger keywords |
| `version` | Yes | — | Semantic version |
| `author` | No | — | Author name |
| `always` | No | `false` | If `true`, always included in system prompt |
| `requires_bins` | No | — | Comma-separated binaries that must exist |
| `requires_env` | No | — | Comma-separated env vars that must be set |

### Step 3: Implement the Binary

The binary implements the stdin/stdout JSON protocol.

**Protocol:**

1. **argv[1]** = tool name (e.g., `get_weather`)
2. **stdin** = JSON object matching the tool's `input_schema`
3. **stdout** = JSON with `output` (string) and `success` (bool)
4. **exit code** = 0 for success, non-zero for failure
5. **stderr** = ignored (use for debug logging)

**Rust template:**

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
        _ => fail(&format!("Unknown tool '{tool_name}'")),
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

    let result = format!("Processed {} with param2={}", input.param1, input.param2);
    println!("{}", json!({"output": result, "success": true}));
}
```

**Python template:**

```python
#!/usr/bin/env python3
import sys, json

def main():
    tool_name = sys.argv[1] if len(sys.argv) > 1 else "unknown"
    input_data = json.loads(sys.stdin.read())

    if tool_name == "my_tool":
        result = f"Processed {input_data['param1']}"
        print(json.dumps({"output": result, "success": True}))
    else:
        print(json.dumps({"output": f"Unknown tool: {tool_name}", "success": False}))
        sys.exit(1)

if __name__ == "__main__":
    main()
```

**Shell template:**

```bash
#!/bin/sh
TOOL="$1"
INPUT=$(cat)

if [ "$TOOL" = "my_tool" ]; then
    PARAM1=$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin)['param1'])")
    printf '{"output": "Processed %s", "success": true}\n' "$PARAM1"
else
    printf '{"output": "Unknown tool: %s", "success": false}\n' "$TOOL"
    exit 1
fi
```

### Step 4: For Bundled Skills (Rust Crate)

If contributing a skill to the core Octos distribution:

```bash
mkdir -p crates/app-skills/my-skill/src
```

**Cargo.toml:**

```toml
[package]
name = "my-skill"
version = "1.0.0"
edition = "2021"

[[bin]]
name = "my_skill"
path = "src/main.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Add to workspace `Cargo.toml`:

```toml
members = [
    # ...
    "crates/app-skills/my-skill",
]
```

Register in `crates/octos-agent/src/bundled_app_skills.rs`:

```rust
pub const BUNDLED_APP_SKILLS: &[(&str, &str, &str, &str)] = &[
    // ...
    (
        "my-skill",                                          // dir_name
        "my_skill",                                          // binary_name
        include_str!("../../app-skills/my-skill/SKILL.md"),
        include_str!("../../app-skills/my-skill/manifest.json"),
    ),
];
```

---

## Part 2: Test

### Standalone Testing

Test your skill binary directly without the gateway:

```bash
# Build (Rust)
cargo build -p my-skill

# Test a tool call
echo '{"param1": "hello", "param2": 5}' | ./target/debug/my_skill my_tool
# Expected: {"output":"Processed hello with param2=5","success":true}

# Test error handling
echo '{}' | ./target/debug/my_skill my_tool
echo '{"param1": "test"}' | ./target/debug/my_skill unknown_tool
```

For non-Rust skills, make the binary executable and test the same way:

```bash
chmod +x my-skill/main
echo '{"param1": "hello"}' | ./my-skill/main my_tool
```

### Gateway Integration Testing

```bash
# Build everything
cargo build --release --workspace

# Start the gateway
octos gateway

# Verify skill loaded
ls ~/.octos/skills/my-skill/
# main  manifest.json  SKILL.md

# Ask the agent to use your skill in conversation
```

### Recommended Timeout Values

| Skill Type | Timeout |
|------------|---------|
| Local computation | 5s |
| Single API call | 15s |
| Multi-step API calls | 30-60s |
| Long-running research | 300-600s |

---

## Part 3: Publish

Publishing makes your skill discoverable to all Octos users — like submitting an app to the App Store.

### Push to GitHub

Organize your repository. A repo can contain a single skill or multiple skills:

**Single-skill repo:**

```
my-skill/                    ← repo root
├── manifest.json
├── SKILL.md
├── Cargo.toml               (or package.json, requirements.txt, etc.)
└── src/main.rs
```

**Multi-skill repo:**

```
my-skills/                   ← repo root
├── skill-a/
│   ├── manifest.json
│   ├── SKILL.md
│   └── src/main.rs
├── skill-b/
│   ├── manifest.json
│   ├── SKILL.md
│   └── main.py
└── shared/                  ← shared dependencies (auto-detected)
    └── utils.py
```

### Submit to the Registry

The [octos-hub](https://github.com/octos-org/octos-hub) registry is the central catalog for discoverable skills. Submit a PR to add your entry to `registry.json`:

```json
{
  "name": "my-skills",
  "description": "What your skills do",
  "repo": "your-user/your-repo",
  "version": "1.0.0",
  "author": "your-name",
  "license": "MIT",
  "skills": ["skill-a", "skill-b"],
  "requires": ["git", "cargo"],
  "provides_tools": true,
  "tags": ["keyword1", "keyword2"]
}
```

**Registry entry fields:**

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Package name (can differ from repo name) |
| `description` | Yes | Searchable description |
| `repo` | Yes | GitHub `user/repo` or full URL |
| `version` | No | Latest version |
| `author` | No | Author name |
| `license` | No | License identifier (MIT, Apache-2.0, etc.) |
| `skills` | No | Individual skill names in the package |
| `requires` | No | External dependencies (e.g., `["git", "cargo"]`) |
| `provides_tools` | No | Whether skills have `manifest.json` with tools |
| `tags` | No | Searchable tags |
| `binaries` | No | Pre-built binaries (see Distribution below) |

Once the PR is merged, users can discover your skill:

```bash
octos skills search keyword1
```

---

## Part 4: Distribute

Pre-built binaries let users install instantly without compiling — like downloading an app binary from the store.

### Add Binaries to manifest.json

In your skill's `manifest.json`, add a `binaries` section keyed by `{os}-{arch}`:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "binaries": {
    "darwin-aarch64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/my-skill-darwin-aarch64.tar.gz",
      "sha256": "abc123..."
    },
    "darwin-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/my-skill-darwin-x86_64.tar.gz",
      "sha256": "def456..."
    },
    "linux-x86_64": {
      "url": "https://github.com/you/repo/releases/download/v1.0.0/my-skill-linux-x86_64.tar.gz",
      "sha256": "789ghi..."
    }
  },
  "tools": [ ... ]
}
```

### Automate with GitHub Actions

Set up CI to build and publish binaries on each release tag:

```yaml
name: Release Skill
on:
  push:
    tags: ["v*"]

jobs:
  build:
    strategy:
      matrix:
        include:
          - os: macos-latest
            target: aarch64-apple-darwin
            platform: darwin-aarch64
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            platform: linux-x86_64

    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v5
      - uses: actions-rust-lang/setup-rust-toolchain@v1

      - run: cargo build --release --target ${{ matrix.target }}

      - name: Package
        run: |
          mkdir dist
          cp target/${{ matrix.target }}/release/my_skill dist/main
          cd dist && tar czf my-skill-${{ matrix.platform }}.tar.gz main
          shasum -a 256 my-skill-${{ matrix.platform }}.tar.gz

      - uses: softprops/action-gh-release@v2
        with:
          files: dist/my-skill-*.tar.gz
```

### Install Resolution Order

When a user runs `octos skills install`, the installer tries these sources in order:

1. **manifest.json `binaries`** — skill author's own CI/CD builds
2. **Registry `binaries`** — registry-audited pre-built binaries
3. **`cargo build --release`** — fallback: compile from source (if `Cargo.toml` exists)
4. **`npm install`** — fallback: install Node.js dependencies (if `package.json` exists)

Pre-built binaries are verified with SHA-256 before installation.

---

## Part 5: Install

### For Users: Search and Install

```bash
# Search the registry
octos skills search weather
octos skills search "deep research"

# Install from GitHub (all skills in repo)
octos skills install user/repo

# Install a specific skill from a multi-skill repo
octos skills install user/repo/skill-name

# Install with a specific branch
octos skills install user/repo --branch dev

# Force reinstall
octos skills install user/repo --force
```

### Per-Profile Installation

Skills are isolated per profile (like per-user app installs):

```bash
# Install to a specific profile
octos skills --profile alice install user/repo/my-skill

# List skills for a profile
octos skills --profile alice list

# Remove from a profile
octos skills --profile alice remove my-skill
```

### In-Chat Installation

Users can manage skills from within a conversation:

```
/skills install user/repo/my-skill
/skills list
/skills remove my-skill
/skills search comic
```

### Admin API

Programmatic skill management via REST:

```bash
# Install
POST /api/admin/profiles/alice/skills     {"repo": "user/repo/my-skill"}

# List
GET  /api/admin/profiles/alice/skills

# Remove
DELETE /api/admin/profiles/alice/skills/my-skill
```

### Sideloading (Manual Install)

Copy a skill directory directly — like sideloading an app:

```bash
# Copy to global skills directory
cp -r my-skill/ ~/.octos/skills/my-skill/
chmod +x ~/.octos/skills/my-skill/main

# Or to a profile-specific directory
cp -r my-skill/ ~/.octos/profiles/alice/data/skills/my-skill/
```

### Installed Skill Layout

```
~/.octos/skills/my-skill/
├── main                # Executable binary
├── manifest.json       # Tool definitions
├── SKILL.md            # Documentation
├── .source             # Install tracking (repo, branch, date)
└── styles/             # Bundled assets (if any)
```

The `.source` file tracks where the skill was installed from:

```json
{
  "repo": "user/repo",
  "subdir": "my-skill",
  "branch": "main",
  "installed_at": "2026-03-28T..."
}
```

### Skill Loading Priority

When multiple directories contain a skill with the same name, first match wins:

| Priority | Location | Source |
|----------|----------|--------|
| 1 (highest) | `<profile-data>/skills/` | Per-profile install |
| 2 | `<project-dir>/skills/` | Project-local |
| 3 | `<project-dir>/bundled-skills/` | Bundled app-skills |
| 4 (lowest) | `~/.octos/skills/` | Global install |

---

## Part 6: Update

```bash
# Update a skill from its source repo
octos skills update my-skill

# Update from a specific branch
octos skills update my-skill --branch main

# View skill details (version, source, tools)
octos skills info my-skill
```

The updater reads the `.source` file to know where to pull from, then re-runs the install flow (clone → discover → build/download → copy).

### Hot-Reload

Skill binaries can be updated without restarting the gateway:

```bash
# Build just the skill
cargo build --release -p my-skill

# Replace the binary
cp target/release/my_skill ~/.octos/skills/my-skill/main

# Next tool call automatically uses the new binary
```

> **Note:** If you change `SKILL.md` or `manifest.json` for a *bundled* skill, you must rebuild the `octos` binary too (they're embedded via `include_str!`). External skills reload immediately.

---

## Advanced Topics

### Multiple Tools in One Skill

A single binary can serve multiple tools. Route on `argv[1]`:

```rust
match tool_name {
    "get_weather" => handle_get_weather(&buf),
    "get_forecast" => handle_get_forecast(&buf),
    _ => fail(&format!("Unknown tool '{tool_name}'")),
}
```

Declare all tools in `manifest.json`:

```json
{
  "tools": [
    { "name": "get_weather", "description": "...", "input_schema": { ... } },
    { "name": "get_forecast", "description": "...", "input_schema": { ... } }
  ]
}
```

### Environment Variables

Skills inherit the gateway's environment (minus blocked security-sensitive vars). Declare requirements in SKILL.md:

```yaml
---
requires_env: MY_API_KEY,MY_SECRET
---
```

The gateway auto-injects provider API keys (e.g., `DASHSCOPE_API_KEY`, `OPENAI_API_KEY`) plus `OCTOS_DATA_DIR` and `OCTOS_WORK_DIR`.

### Bundled Assets

Skills with asset files should resolve paths relative to the executable:

```rust
let exe = std::env::current_exe()?;
let skill_dir = exe.parent().unwrap();
let styles_dir = skill_dir.join("styles");
```

> Do **not** use the current working directory — it points to the profile's data dir, not the skill dir.

### MCP Servers

A skill can declare MCP servers the gateway auto-starts:

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

Or remote MCP servers:

```json
{
  "mcp_servers": [
    {
      "url": "https://mcp.example.com/v1",
      "headers": { "Authorization": "Bearer ${API_KEY}" }
    }
  ]
}
```

Path resolution: `./` and `../` are relative to the skill directory. `env` lists variable *names* (not values) to forward.

### Lifecycle Hooks

Skills can run commands on agent events:

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
      "timeout_ms": 5000
    }
  ]
}
```

| Event | Can Deny? | When |
|-------|-----------|------|
| `before_tool_call` | Yes (exit 1) | Before tool execution |
| `after_tool_call` | No | After tool completes |
| `before_llm_call` | Yes (exit 1) | Before LLM request |
| `after_llm_call` | No | After LLM response |

### Prompt Fragments

Inject content into the system prompt without writing code:

```json
{
  "name": "company-policy",
  "version": "1.0.0",
  "prompts": {
    "include": ["prompts/*.md"]
  }
}
```

### Extras-Only Skills

Skills don't need to provide tools. Valid combinations:

- **Prompt-only:** Teach the agent domain knowledge (no binary needed)
- **Hooks-only:** Enforce policies across all tool calls
- **MCP-only:** Expose tools via remote MCP servers
- **Combined:** Tools + MCP + hooks + prompts in one skill

### Security

**Binary integrity:**
- Symlinks rejected (defense against link-swap attacks)
- SHA-256 verification when `sha256` is set in manifest
- Size limit: 100 MB max per binary

**Environment sanitization** — these vars are stripped before spawning skills:
- `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`
- `NODE_OPTIONS`, `PYTHONPATH`, `PERL5LIB`
- `RUSTFLAGS`, `RUST_LOG`, and 10+ others

**Best practices:**
- Validate all input (never trust user-provided paths, names, etc.)
- Use timeouts on HTTP requests
- Avoid shell injection
- Set `sha256` in manifest for release builds

### Platform Skills vs App Skills

| | App Skills | Platform Skills |
|---|---|---|
| Location | `crates/app-skills/` | `crates/platform-skills/` |
| Bootstrap | Every gateway startup | Admin bot only |
| Scope | Per-gateway | Shared across gateways |
| Use when | Self-contained, always available | Requires external service |

---

## Examples

### Example 1: Clock (Local, No Network)

```
crates/app-skills/time/
├── Cargo.toml          # chrono, chrono-tz, serde, serde_json
├── manifest.json       # 1 tool: get_time, timeout_secs: 5
├── SKILL.md            # Triggers: time, clock
└── src/main.rs         # System clock + timezone formatting
```

### Example 2: Weather (Network API)

```
crates/app-skills/weather/
├── Cargo.toml          # reqwest (blocking, rustls-tls), serde, serde_json
├── manifest.json       # 2 tools: get_weather, get_forecast, timeout_secs: 15
├── SKILL.md            # Triggers: weather, forecast
└── src/main.rs         # Geocode city → Open-Meteo API
```

### Example 3: Email (Environment Credentials)

```
crates/app-skills/send-email/
├── Cargo.toml          # lettre, serde, serde_json
├── manifest.json       # 1 tool: send_email
├── SKILL.md            # requires_env: SMTP_HOST,SMTP_USERNAME,SMTP_PASSWORD
└── src/main.rs         # SMTP with credential validation
```

---

## Checklists

### Tool Skill (binary + tools)

- [ ] Directory has `manifest.json`, `SKILL.md`, and executable (`main` or binary)
- [ ] `manifest.json` has valid JSON Schema for all tool inputs
- [ ] `SKILL.md` has frontmatter with trigger keywords
- [ ] Binary reads `argv[1]` for tool name, stdin for JSON
- [ ] Binary writes `{"output": "...", "success": true/false}` to stdout
- [ ] Error cases return `success: false` with clear messages
- [ ] Standalone test passes: `echo '{"param": "val"}' | ./main my_tool`
- [ ] Gateway test passes: skill loads and agent can invoke it

### Extras Skill (MCP / hooks / prompts)

- [ ] `mcp_servers`: `command` or `url` set; `env` lists names only
- [ ] `hooks`: valid event name; `command` is argv array; relative paths resolve
- [ ] `prompts`: glob patterns match intended `.md` files
- [ ] Extras-only: `tools` is empty or omitted, no binary needed

### Publishing

- [ ] Repo pushed to GitHub with `manifest.json` and `SKILL.md` at expected paths
- [ ] Registry PR submitted to [octos-hub](https://github.com/octos-org/octos-hub)
- [ ] (Optional) Pre-built binaries for `darwin-aarch64`, `linux-x86_64`
- [ ] (Optional) SHA-256 hashes in `manifest.json` `binaries` section
- [ ] (Optional) GitHub Actions workflow for automated binary builds on release tags
