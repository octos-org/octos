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
| `sha256` | Strongly recommended for production fleets | — | Binary integrity check (hex hash). Required when the host has `plugins.require_signed = true` — plugins missing it are rejected at load time. TOCTOU-safe: verified copy written to `.{name}_verified` (`plugins/loader.rs:283-491`). Compute via `shasum -a 256 main`. |
| `protocol_version` | No | 1 | `1` = stdin/stdout JSON only (default). `2` = also emit structured events on stderr. See [Plugin Protocol v2](#plugin-protocol-v2). |
| `synthesis_config` | No | — | v2 only: declare a synthesis LLM call so the host injects provider/model + env keys |
| `x-octos-host-config-keys` | No | `[]` | v2 only: env keys the host should forward into the skill (e.g. provider API keys for synthesis) |
| `tools` | No | `[]` | Array of tool definitions. Each may set `spawn_only: true` to be auto-routed to background execution by `agent/execution.rs`. Spawn-only tools may emit a `named_outputs` map — see [Part 7: Workspace Contract](#part-7-workspace-contract). |
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

**Protocol v1 (default):**

1. **argv[1]** = tool name (e.g., `get_weather`)
2. **stdin** = JSON object matching the tool's `input_schema`
3. **stdout** = JSON with `output` (string) and `success` (bool)
4. **exit code** = 0 for success, non-zero for failure
5. **stderr** = ignored (free-form debug logging)

**Protocol v2 (opt-in)** — see [Plugin Protocol v2](#plugin-protocol-v2) below for the full reporting contract used by `deep-search`, `deep-crawl`, and other long-running skills. To opt in, set `"protocol_version": 2` in your `manifest.json`. Stdout still carries the final result; stderr becomes a structured event channel.

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

### Fleet Deploy

For multi-host fleets, use `scripts/fleet-install-skills.sh` (PR #939) — it replaces the legacy `mofa-skills/scripts/deploy-mini.sh` `scp` flow. The new script rsyncs each skill to a staging path on every host and then runs `octos skills install <staging-path>` server-side so the manifest's sha256 verification runs inside the same code path the runtime uses.

See [`docs/SKILL_DEPLOYMENT.md`](./SKILL_DEPLOYMENT.md) for the operator-side runbook (CLI flags, env overrides, migration from legacy `~/.octos/skills/`, verification commands).

### Sideloading (Manual Install)

Copy a skill directory directly — like sideloading an app:

```bash
# Canonical: per-profile install
cp -r my-skill/ ~/.octos/profiles/alice/data/skills/my-skill/
chmod +x ~/.octos/profiles/alice/data/skills/my-skill/main

# Legacy (deprecated): global skills directory — loader emits a warning
cp -r my-skill/ ~/.octos/skills/my-skill/
chmod +x ~/.octos/skills/my-skill/main
```

The global `~/.octos/skills/` directory is being retired (PR #944). New deployments should write directly to the per-profile path.

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

Loading dedupes by `manifest.id` — the **first** directory scanned that contains a given plugin id wins (`plugins/loader.rs:137-156`, PR #936). Later directories carrying the same id are silently ignored, even if the on-disk plugin is fresher.

| Priority | Location | Source |
|----------|----------|--------|
| 1 (highest) | `~/.octos/profiles/<profile>/data/skills/` | **Canonical** per-profile install (`octos skills install --profile <p>`; `fleet-install-skills.sh`) |
| 2 | `<project-dir>/skills/` | Project-local (development checkouts) |
| 3 | `<project-dir>/bundled-app-skills/` | Bundled app-skills (constant `BUNDLED_APP_SKILLS_DIR` in `octos-agent/src/bootstrap.rs`; scanned by `Config::plugin_dirs_from_project`) |
| 4 (lowest, **deprecated**) | `~/.octos/skills/` | Legacy global install — the loader emits a deprecation warning on every startup that sees this directory. See [Part 5: Install](#part-5-install) and [`docs/SKILL_DEPLOYMENT.md`](./SKILL_DEPLOYMENT.md) for the per-profile-only migration (PR #944). |

**Lesson from the fleet (2026):** before PR #936 the loader registered duplicate ids twice and `ToolRegistry::register` overwrote by tool name — so a stale per-profile install could silently shadow a freshly-deployed global skill, and vice versa. Two production regressions (yangmi, douwentao) traced back to this trap before the dedup landed. On fleet upgrades, update **both** `~/.octos/skills/<x>/` *and* `<profile>/data/skills/<x>/` in lock-step until the global directory is removed.

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

## Part 7: Workspace Contract

A skill that produces an artifact — a slide deck, a podcast MP3, a deployed URL, a `.diff` patch — has a **post-condition** the harness has to enforce. Historically each skill carried its own validators (silent-MP3 detection, magic-byte parsing, HTTP probes…) baked into its binary. This was the wrong layer: the contract is a property of the *task*, not of the skill's source code, and skills cannot be trusted to validate themselves.

As of 2026-05-13 the harness owns every post-condition through the **workspace_policy / workspace_contract** layer. Skills emit artifacts and structured outputs; the harness asserts the contract. Skill authors should **not** write their own validators.

> See [`docs/audits/HARNESS_CONTRACT_AUDIT_2026-05-13.md`](./audits/HARNESS_CONTRACT_AUDIT_2026-05-13.md) for the full audit of the previous skill-internal contracts and the canonical replacements. The five-layer model below is the architecture the audit prescribes.

### The five-layer model

1. **Contract in the harness** — `WorkspacePolicy::for_session()` / `for_coding()` / per-workspace `.octos-workspace.toml`. The validators that fire post-task are declared here, not in skill code.
2. **`named_outputs`** — spawn-only tools emit a structured `{key: value}` envelope on stdout the harness reads into `${output.X}` template references (PR #941).
3. **Per-profile install** — every customer skill lives at `<profile>/data/skills/`; no global shadow trap (PR #944).
4. **sha256-bound binary** — manifest's top-level `sha256` is hashed at install AND re-hashed at exec to close the load→exec TOCTOU window (`plugins/tool.rs:920-970`).
5. **Verify-at-merge + verify-at-deploy** — `fleet-install-skills.sh` routes through `octos skills install`, which re-verifies sha256 against the manifest before copying.

### The spawn-only output envelope

When a tool's manifest declares `"spawn_only": true`, the agent execution loop intercepts the call, runs it as a background task, and reads a JSON envelope from the binary's stdout:

```json
{
  "success": true,
  "output": "Deployed to https://example.com",
  "files_to_send": ["/abs/path/to/artifact.pptx"],
  "named_outputs": {
    "deploy_url": "https://example.com",
    "repo": "owner/name"
  }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `success` | Yes | Boolean — false demotes the spawn task to Failed before the contract gate runs. |
| `output` | Yes | Free-form string surfaced to the LLM in the next turn. |
| `files_to_send` | No | Absolute paths the harness should treat as deliverables. Bound to the workspace's `artifacts.*` declarations (`workspace_contract.rs:244-272`). |
| `named_outputs` | No | Optional map of structured outputs the harness can interpolate into validators via `${output.<key>}`. PR #941. |

**`named_outputs` shape (v1):**

- Keys must match `[a-z][a-z0-9_]*` (rejected at parse time — `plugins/tool.rs:697-707`).
- Values must be **strings**. Nested JSON / arrays / numbers are not supported in v1.
- The whole field is optional; absent / `null` / `{}` all deserialize to "no outputs".
- A malformed envelope (non-object payload, non-string value, key shape violation) is rejected with `success: false` and a descriptive reason — see `plugins/tool.rs:1331-1357`.

Emitting from a Rust spawn-only skill:

```rust
println!(
    "{}",
    serde_json::json!({
        "success": true,
        "output": format!("Deployed to {url}"),
        "files_to_send": [],
        "named_outputs": {
            "deploy_url": url,
            "repo": repo_slug,
        }
    })
);
```

> No helper exists in `octos-plugin` today — emit the envelope directly. A typed `SpawnOnlyResult` helper is on the open list.

### Anatomy of a validator

Every workspace-contract validator is a `Validator` struct (`crates/octos-agent/src/workspace_policy.rs:142-167`):

```rust
pub struct Validator {
    pub id: String,                 // Stable identifier, unique within the list.
    pub required: bool,             // default true — Hard tier blocks terminal success.
    pub soft_fail: bool,            // default false — Soft tier warns without demoting.
    pub timeout_ms: Option<u64>,    // Optional per-validator timeout.
    pub phase: ValidatorPhaseKind,  // TurnEnd | Completion (default).
    pub spec: ValidatorSpec,        // The typed body — see below.
}
```

**Gate tier** (`Validator::tier()` at `workspace_policy.rs:182-191`, PR #943):

| `required` | `soft_fail` | Tier | Behaviour on failure |
|------------|-------------|--------|------------------|
| `true` | `false` | `Hard` | Demote spawn task to `Failed`. Default. |
| `true` | `true` | `Soft` | Warn + persist to ledger; do **not** demote. |
| `false` | `false` | `None` | Informational only. |
| `false` | `true` | `Soft` | Same as `true`/`true`. |

Soft-fail is the canonical way to express partial-artifact contracts: "the primary report is hard-required, the sub-artifacts are nice-to-have." `Validator::tier()` collapses both fields into the operator-visible label that surfaces in metrics + the ledger.

### ValidatorSpec variants

Every `ValidatorSpec` variant currently merged. Source: `crates/octos-agent/src/workspace_policy.rs:263-357`.

| Variant | Serde shape | Interpolation | One-line use |
|---------|-------------|---------------|--------------|
| `Command { cmd, args }` | `{kind: "command", cmd, args}` | none | Run a subprocess (dispatched via the shell-safety layer + `BLOCKED_ENV_VARS`). |
| `ToolCall { tool, args }` | `{kind: "tool_call", tool, args}` | none | Invoke a registered agent tool. Status follows the tool's `ToolResult.success`. |
| `FileExists { path, min_bytes? }` | `{kind: "file_exists", path, min_bytes}` | `${args.X}` (percent-encoded for the single-filename segment use case) + `${output.X}` (verbatim) | Assert a file exists (and optionally meets a min byte count). |
| `HttpProbe { url_template, expected_status?, expected_contains? }` | `{kind: "http_probe", url_template, expected_status, expected_contains}` | `${args.X}` (percent-encoded) + `${output.X}` (verbatim) | Single-shot GET → status + optional body substring. PR #935. |
| `OminixVoiceExists { name_arg }` | `{kind: "ominix_voice_exists", name_arg}` | `name_arg` looked up in input args | Specialised probe of `${OMINIX_API_URL}/v1/voices` asserting the named voice is registered. PR #935. |
| `AudioNonSilent { glob, min_ratio? }` | `{kind: "audio_non_silent", glob, min_ratio}` | `${args.X}` + `${output.X}` (glob template) | Decode WAV (or MP3 with the `audio_mp3` feature) and assert at least `min_ratio` of samples are non-silent across the whole file. Passes if ANY matched file is non-silent. PR #935. |
| `PerFileNonSilent { glob, min_ratio?, require_at_least? }` | `{kind: "per_file_non_silent", glob, min_ratio, require_at_least}` | `${args.X}` only (path-traversal-rejected) | Per-segment companion to `AudioNonSilent`: EVERY matched file must independently meet `min_ratio`, and the match count must meet `require_at_least` (0 = no minimum). Failure message surfaces the offending file's basename. PR #955. |
| `MagicBytes { glob, format }` | `{kind: "magic_bytes", glob, format}` | none | Assert each file matching `glob` starts with the magic-byte prefix for `format`. Catches "wrote an HTML error page in place of an MP3" failures. PR #935. Supported formats: `mp3`, `wav`, `png`, `jpeg`, `pdf`, `mp4`, `web_m`, `pptx` (three ZIP signatures). |
| `HttpProbeUntil { url_template, expected_status?, expected_contains?, poll_interval_ms?, deadline_ms? }` | `{kind: "http_probe_until", ...}` | same as `HttpProbe` | Polling HTTP probe; closes silent-failure paths for asynchronous external operations (training a voice, deploying a site). Default 2s interval / 30s deadline. PR #943. |
| `Sha256Match { glob, sha256 }` | `{kind: "sha256_match", glob, sha256}` | `${args.X}` for both fields | Assert a single file's SHA-256 digest matches. Accepts either an explicit hex digest or an interpolated arg (e.g. the manifest's `sha256` captured at install). PR #943. |

**Whole-file vs per-segment audio gates.** `AudioNonSilent` passes when ANY matched file is non-silent, which means a multi-segment artifact (e.g. an assembled podcast MP3) can mask a single silent intermediate segment because the silent gap gets averaged out by the surrounding speech. When per-segment guarantees matter, pair `AudioNonSilent` (whole file) with `PerFileNonSilent` (each segment) on the same spawn task. The canonical example is `podcast_generate`, which gates BOTH the final MP3 and each preserved `<output_dir>/segments/seg_*.wav` after mofa-skills #59 made segments visible after assembly.

### Interpolation: `${args.X}` vs `${output.X}`

Both reference forms are resolved by `interpolate_template` (`validators.rs:1642-1682`):

| Reference | Source | Encoding | Missing-key behaviour |
|-----------|--------|----------|-----------------------|
| `${args.X}` | The spawn task's input args (LLM-controlled) | **Percent-encoded** for URL/filename-segment safety in `HttpProbe`/`HttpProbeUntil`/`FileExists`/`OminixVoiceExists`/`Sha256Match` | Typed `Error` outcome surfaces in the ledger; no silent pass. |
| `${output.X}` | The spawn-only tool's `named_outputs` envelope | **Verbatim** — values are tool-controlled and already trusted | Typed `Error` outcome surfaces in the ledger; no silent pass. |

The trust-boundary distinction is load-bearing: `args` come from the LLM (untrusted, must be sanitised), `outputs` come from the spawn-only binary the harness already exec'd through a sha256 gate (trusted to escape itself).

For glob-pattern templates (`MagicBytes.glob`, `AudioNonSilent.glob`, `Sha256Match.glob`) interpolation uses `interpolate_args_path` (`validators.rs:1695-1733`) which keeps `/` separators but rejects `..` segments and absolute paths — so an LLM-controlled arg cannot escape the workspace root.

### Per-spawn-task validators

`WorkspaceSpawnTaskPolicy.on_completion` (PR #935, lifted in #938 + #941 + #943) lets you attach validators to a specific spawn task. Both a bare `ValidatorSpec` table and a full `Validator` struct are accepted (`SpawnTaskValidatorSpec` is a serde `untagged` enum, `workspace_policy.rs:565-592`). Bare specs are auto-tagged as `required + Completion + synthetic id`.

`WorkspaceSpawnTaskPolicy.on_verify` (existing) takes the legacy shorthand strings (`file_exists:$artifact`, `file_size_min:$artifact:1024`). New contracts should use `on_completion` with typed validators.

### Workspace-level validators

`ValidationPolicy.validators: Vec<Validator>` (`workspace_policy.rs:127`) runs once per `phase` (`TurnEnd` or `Completion`) regardless of which task fired. Use this for workspace-wide guarantees (e.g. "after every turn, run `cargo check`").

### Where to declare the contract

**Option A — Harness-owned (canonical).** Add to `WorkspacePolicy::for_session()` in `crates/octos-agent/src/workspace_policy.rs`. Worked example, an imaginary `mofa_widget` skill:

```rust
let mofa_widget_contract = WorkspaceSpawnTaskPolicy {
    artifact: None,
    artifacts: Vec::new(),
    on_verify: Vec::new(),
    on_complete: vec![],
    on_deliver: vec![],
    on_failure: vec!["notify_user:Widget render failed".into()],
    on_completion: vec![
        // Hard: the widget file must land where the skill claims it did.
        SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists {
            path: "${args.out}".into(),
            min_bytes: Some(1024),
        }),
        // Hard: bytes must actually be a PNG (catches HTML-error-page failures).
        SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
            glob: "**/*.png".into(),
            format: MagicByteKind::Png,
        }),
        // Soft: a thumbnail is nice to have but absence shouldn't fail the task.
        SpawnTaskValidatorSpec::Full(Validator {
            id: "mofa_widget.thumbnail_warn".into(),
            required: true,
            soft_fail: true,
            timeout_ms: None,
            phase: ValidatorPhaseKind::Completion,
            spec: ValidatorSpec::FileExists {
                path: "${args.out}.thumb.png".into(),
                min_bytes: None,
            },
        }),
    ],
};
// ...
spawn_tasks.insert("mofa_widget".into(), mofa_widget_contract);
```

**Option B — Per-workspace TOML override.** Each session can ship a `.octos-workspace.toml` next to the working directory that overrides the defaults. The serde schema is the same as the `WorkspacePolicy` struct. Example:

```toml
schema_version = 1

[workspace]
kind = "session"

[validation]
on_turn_end = []
on_completion = []

[[validation.validators]]
id = "fm_voice_registered"
required = true
phase = "completion"
kind = "ominix_voice_exists"
name_arg = "name"

[spawn_tasks.mofa_widget]
artifact = "primary"

[[spawn_tasks.mofa_widget.on_completion]]
kind = "file_exists"
path = "${args.out}"
min_bytes = 1024
```

> **The harness is authoritative.** Per audit section 7, skills MUST NOT declare a canonical `workspace_contract` block in their own `manifest.json`. The historical "Documentation-only" block at `mofa-fm/manifest.json:125-149` is exactly the anti-pattern that broke this layer. If a skill wants to *suggest* a validator the operator should opt in to, do it in `SKILL.md` prose — never as a hand-written contract block the loader could be tempted to consume.

### Auto-fire on EndTurn

PR #940 + the loop wiring in `agent/loop_runner.rs:1435-1480` make `check_workspace_contract` fire automatically when the agent signals `EndTurn`. If `inspect_workspace_contracts(workspace_root)` reports `ready == false`, the task is demoted from `Success` to a typed `ContractFailed`. The LLM no longer has to remember to call the inspector tool — the harness drives the gate.

### Lifecycle phases

- `TurnEnd` — runs after every turn. Cheap checks only.
- `Completion` — runs once when the task signals it's done. Expensive checks (file decoding, HTTP probes) live here.
- `Preflight` and `AfterTool` — **proposed in the audit (Gap-1, Gap-10), not yet merged**. The `Coding` workspace kind (PR #940) ships `cargo check` / `eslint` / `ruff` defaults via the **hook system** instead — see [Part 8: Hook System](#part-8-hook-system) below — so the audit's `AfterTool` gap is closed in practice even though `ValidatorPhaseKind` is still `TurnEnd | Completion`.

---

## Part 8: Hook System

Hooks intersect with skill authoring because a skill can declare them in its manifest, and the host merges per-skill hooks with operator hooks at runtime. The two skill-author-relevant features that landed in PR #940:

### `path_filter`

Each `HookConfig` can declare `path_filter: Vec<String>` of glob patterns matched against the tool's `args.path` (`hooks.rs:46-78`, glob compilation at `hooks.rs:920-948`). The hook is skipped when:

- `path_filter` is non-empty AND
- the tool's args either have no `path` field or the value matches no pattern.

This is how `cargo check` only fires on `.rs` edits without re-running on every `write_file` to a Markdown file.

### `requires_bin`

Optional `requires_bin: Option<String>` — when the named binary is not on `PATH` the hook is silently skipped. Lets operators ship a hook list that gracefully degrades on hosts without `eslint` or `ruff` installed.

### `WorkspacePolicyKind::Coding` defaults

When the cwd contains `Cargo.toml` / `package.json` / `pyproject.toml`, `detect_workspace_policy_kind()` (`workspace_policy.rs:1142-1151`) returns `Coding` and the host merges `coding_default_hooks()` (`workspace_policy.rs:1170-1212`) into its `HookExecutor`:

| Tool | Path filter | Command | `requires_bin` |
|------|-------------|---------|----------------|
| `edit_file` / `write_file` / `diff_edit` | `**/*.rs` | `cargo check --message-format=short` | `cargo` |
| same | `**/*.{js,ts,tsx,jsx}` | `eslint --max-warnings 0` | `eslint` |
| same | `**/*.py` | `ruff check` | `ruff` |

Operator-defined hooks always merge **after** these defaults so a stricter project-local hook is always invoked too (both fire).

---

## Part 9: Migration Notes — Existing Skills

For skill authors with existing skills, here's what to align with as of 2026-05-13:

### Action items

- [ ] **Add `sha256` to `manifest.json`** — compute via `shasum -a 256 main` and commit. Required if any fleet host enables `plugins.require_signed = true`.
- [ ] **Remove any skill-internal contract block.** The historical pattern of declaring a `workspace_contract` field inside `manifest.json` (used by mofa-fm pre-2026-05-13) is unsupported. The agent-side manifest type does not parse such a field — it was always documentation-only — and the canonical equivalent lives in `WorkspacePolicy::for_session()` or a `.octos-workspace.toml` override.
- [ ] **Strip skill-internal validators from skill source code.** If your skill currently runs its own post-condition (silent-MP3 detector, HTTP probe of a remote API, magic-byte parse), move it onto the canonical `ValidatorSpec` path. Concrete examples — these are the ad-hoc patterns being retired (audit section 3):
  - `assert_voice_registered` / `fetch_registered_voices` (mofa-fm) → replace with `ValidatorSpec::OminixVoiceExists`.
  - `has_meaningful_tts_audio` / `parse_wav_metadata` (mofa-podcast) → replace with `ValidatorSpec::AudioNonSilent` + `ValidatorSpec::MagicBytes`.
  - `poll_training_status` (mofa-fm) → replace with `ValidatorSpec::HttpProbeUntil`.
- [ ] **For spawn-only artifact-producers, emit `named_outputs` where the harness needs to validate the output.** e.g. a `mofa_publish`-style skill must emit `{ "deploy_url": "..." }` so the harness's `HttpProbe { url_template = "${output.deploy_url}" }` can probe the live URL.
- [ ] **Move to per-profile install.** Stop writing to `~/.octos/skills/`; use `octos skills install --profile <p>` or the fleet script.
- [ ] **Ship `manifest.json` `binaries.<platform>.sha256` if you publish pre-built binaries.** The installer verifies before copy.

### Backward compatibility

- `manifest.json` fields that still work: every documented field from Part 1 above. The agent-side parser ignores unknown top-level fields, so an old "documentation-only" `workspace_contract` block does not break loading — but the harness does not read it either, and you should remove it.
- `spawn_only_message` on a tool entry is still supported (it's the literal string surfaced to the LLM as the tool's immediate response). It is **not** the contract result — the workspace policy's outcome is.
- The legacy `on_verify` shorthand strings (`file_exists:$artifact`, `file_size_min:$artifact:1024`) still work; new contracts should use the typed `on_completion` validators.
- Double-running validation (skill-internal AND canonical) during a transition is wasteful but safe — there's no `--skip-internal-validation` skill flag.

---

## Part 10: Reference

- **Audit:** [`docs/audits/HARNESS_CONTRACT_AUDIT_2026-05-13.md`](./audits/HARNESS_CONTRACT_AUDIT_2026-05-13.md) — full per-tool table, ad-hoc patterns inventory, framework gaps.
- **Workspace policy source:** `crates/octos-agent/src/workspace_policy.rs` — `Validator`, `ValidatorSpec`, `MagicByteKind`, `Required`, `WorkspacePolicy::for_session()`.
- **Validator runner:** `crates/octos-agent/src/validators.rs` — `ValidatorRunner`, interpolation, HTTP probe wiring (with the shared SSRF gate from `tools/ssrf.rs`).
- **Workspace contract enforcement:** `crates/octos-agent/src/workspace_contract.rs` — `enforce_spawn_task_contract`, `run_declared_validators`, `bind_explicit_files_to_artifacts`.
- **Plugin loader / sha256 gates:** `crates/octos-agent/src/plugins/loader.rs:70-491`, `crates/octos-agent/src/plugins/tool.rs:104-970`.
- **EndTurn auto-fire:** `crates/octos-agent/src/agent/loop_runner.rs:42-80, 1435-1480`.
- **Hook config:** `crates/octos-agent/src/hooks.rs:46-78` (HookConfig), `crates/octos-agent/src/workspace_policy.rs:1142-1212` (Coding detection + defaults).
- **Plugin SDK protocol:** [`crates/octos-plugin/docs/protocol-v2.md`](../crates/octos-plugin/docs/protocol-v2.md).
- **Compatibility contract:** [`docs/OCTOS_HARNESS_SKILL_COMPAT.md`](./OCTOS_HARNESS_SKILL_COMPAT.md) — the productization boundary every third-party skill must uphold.
- **Fleet deploy runbook:** [`docs/SKILL_DEPLOYMENT.md`](./SKILL_DEPLOYMENT.md).
- **Worked examples that already conform:** the four `crates/app-skills/harness-starter-*/` templates (audio / coding / generic / report). Their manifests demonstrate `spawn_only` + `concurrency_class` and they intentionally ship without skill-internal validators.
- **Real spawn-only contracts in production:** see `WorkspacePolicy::for_session()` entries for `fm_tts`, `podcast_generate`, `voice_synthesize`, `fm_voice_save`, `mofa_slides`, `mofa_cards`, `mofa_comic`, `mofa_infographic`, `mofa_publish`, `manage_skills`, `synthesize_research`, `deep_search` (`workspace_policy.rs:692-1087`).

---

## Advanced Topics

### Plugin Protocol v2

Long-running skills (research, crawls, voice training, multi-step pipelines) need to surface progress, partial results, and per-step cost back to the host while they run. Protocol v2 adds a structured event channel on **stderr** while keeping the v1 stdout contract for the final result.

**Opting in:** add `"protocol_version": 2` to `manifest.json`.

**Stderr event format:** one JSON object per line; each line is a tagged event:

| Event | Fields | Purpose |
|---|---|---|
| `LogEvent` | `{level, message}` | Free-form text log line (debug/info/warn/error) |
| `PhaseEvent` | `{phase, summary}` | Phase transitions (`planning`, `searching`, `synthesizing`, `verifying`, …) |
| `ProgressEvent` | `{current, total, label}` | Numeric progress |
| `CostEvent` | `{step, provider, model, input_tokens, output_tokens, usd}` | Per-step LLM cost — rolls up via `cost_ledger.rs` |
| `ArtifactEvent` | `{name, path, mime}` | Pointer to a produced artifact (file path or URL) |

The host parses each line, surfaces phase/progress events as `tool_progress` SSE events to the dashboard/clients, and aggregates `CostEvent` lines into the parent's per-turn cost rollup. Lines that don't parse as events are treated as plain log lines (downgraded to `LogEvent` with `level=info`).

**Stdout** still carries the final v1-shaped JSON: `{"output": "<final result>", "success": true, "files_to_send": [...]}`.

**Synthesis-style skills (`deep-search`, `deep-crawl`)** declare `synthesis_config` plus `x-octos-host-config-keys` so the host injects the correct LLM provider/model and forwards env keys for the synthesis call:

```json
{
  "name": "deep-search",
  "protocol_version": 2,
  "synthesis_config": {
    "provider": "anthropic",
    "model": "claude-sonnet-4-20250514"
  },
  "x-octos-host-config-keys": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
}
```

The host resolves these keys from the active profile's auth store and passes them on the skill's environment for the synthesis sub-call only.

**Contract tests:** if you author a v2 skill, mirror the contract tests at `crates/octos-plugin/tests/lifecycle_sandbox.rs` so CI verifies that your binary emits well-formed events and respects the BLOCKED_ENV_VARS list.

**spawn_only mechanics in detail** — when a tool's manifest declares `spawn_only: true`, the agent execution loop (`crates/octos-agent/src/agent/execution.rs`) intercepts the call **before** the LLM round-trip:

1. The tool invocation is wrapped in `tokio::spawn` and immediately returns an acknowledgement to the LLM.
2. `task_supervisor.rs` registers the task, applies the per-profile fan-out cap (#610), and sets up the orphan reaper.
3. The skill binary runs to completion in the background, emitting protocol v2 events.
4. On completion (or failure), the supervisor commits a `session_result` event with `committed_seq` and re-engages the LLM if needed (M8.9 runtime failure recovery).

`spawn_only` tools cannot be evicted from the LRU tool registry, and their `SKILL.md` is auto-injected into the system prompt so the LLM knows how to use them.

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

### Example 4: Deep Search (Protocol v2 + Synthesis)

```
crates/app-skills/deep-search/
├── Cargo.toml          # reqwest, async runtime, serde
├── manifest.json       # protocol_version: 2, synthesis_config + x-octos-host-config-keys
├── SKILL.md            # spawn_only: true (long-running)
└── src/main.rs         # Multi-step research; emits PhaseEvent + ProgressEvent
                        # + CostEvent on stderr; final synthesis on stdout
```

Demonstrates the full v2 protocol: structured stderr events, host-injected synthesis LLM config, spawn_only background execution, and contract tests under `crates/octos-plugin/tests/lifecycle_sandbox.rs`.

### Example 5: Harness Starters (Templates to Copy)

The four `crates/app-skills/harness-starter-*` crates are working examples of harnessed skills you can copy and adapt:

- `harness-starter-generic` — minimal echo-style harness for any text task
- `harness-starter-coding` — code-task harness with worktree integration
- `harness-starter-report` — report-generation harness with artifact production
- `harness-starter-audio` — audio-task harness with attachment validation (header + silence + duration)

Their `SKILL.md` says "Replace with a real ... when adapting the starter." Use them as the structural starting point — they include the manifest fields, contract tests, and event-emission scaffolding for v2.

---

## Checklists

### Tool Skill (binary + tools)

- [ ] Directory has `manifest.json`, `SKILL.md`, and executable (`main` or binary)
- [ ] `manifest.json` has valid JSON Schema for all tool inputs
- [ ] `manifest.json` includes top-level `sha256` (required for production fleets with `plugins.require_signed = true`; compute via `shasum -a 256 main`)
- [ ] `manifest.json` does NOT contain a self-invented `workspace_contract` block (see [Part 9](#part-9-migration-notes--existing-skills))
- [ ] `SKILL.md` has frontmatter with trigger keywords
- [ ] Binary reads `argv[1]` for tool name, stdin for JSON
- [ ] Binary writes `{"output": "...", "success": true/false}` to stdout (plus `files_to_send` / `named_outputs` if applicable)
- [ ] Error cases return `success: false` with clear messages
- [ ] No silent-failure paths in skill code — surface as `success: false` or rely on a harness validator (don't write your own post-condition check)
- [ ] If `spawn_only: true` and the harness needs to validate a structured output (e.g. a deploy URL), the binary emits `named_outputs` with keys matching `[a-z][a-z0-9_]*` and string values
- [ ] Workspace contract entry exists in `WorkspacePolicy::for_session()` or `.octos-workspace.toml` (see [Part 7](#part-7-workspace-contract))
- [ ] If the skill produces audio, the contract declares `AudioNonSilent` (whole-file) — and `PerFileNonSilent` if per-segment guarantees are needed (PR #955)
- [ ] Standalone test passes: `echo '{"param": "val"}' | ./main my_tool`
- [ ] Gateway test passes: skill loads and agent can invoke it
- [ ] `cargo clippy -D warnings` clean (for Rust skills)
- [ ] If the skill publishes pre-built binaries, every `binaries.<platform>.sha256` matches the released archive
- [ ] If the skill behavior is gated on the `api` feature (e.g. exposes via `octos serve`), the test plan includes `--features api` (M11-G lesson)

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
