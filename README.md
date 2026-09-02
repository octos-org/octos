<div align="center">

<pre>
 ██████╗  ██████╗████████╗ ██████╗ ███████╗
██╔═══██╗██╔════╝╚══██╔══╝██╔═══██╗██╔════╝
██║   ██║██║        ██║   ██║   ██║███████╗
██║   ██║██║        ██║   ██║   ██║╚════██║
╚██████╔╝╚██████╗   ██║   ╚██████╔╝███████║
 ╚═════╝  ╚═════╝   ╚═╝    ╚═════╝ ╚══════╝
</pre>

</div>

> Like an octopus — 9 brains (1 central + 8 in the arms, one per arm). Every arm thinks independently, but they share one brain.

Octos is your own AI assistant, running on your own computer. Install one small program, connect any major AI provider (Anthropic, OpenAI, Gemini, DeepSeek, …), and chat with an agent that can run code, browse the web, remember things, schedule jobs, and build documents — from your browser, your terminal, or apps like Telegram, WhatsApp, and Discord. Your sessions, memory, and data stay on your machine — prompts go only to the AI provider you choose.

## Start here

The fastest way to a working assistant, on the supported platforms (macOS Apple Silicon, Linux x86-64/arm64, Windows x64):

```bash
# 1. Install
brew tap octos-org/octos https://github.com/octos-org/octos
brew install octos-org/octos/octos      # or: npm install -g @octos-org/octos

# 2. Choose your AI provider and a model (interactive — pick a real
#    model name; some providers reject the "auto" default)
octos init

# 3. Sign in to that provider — or paste its API key; stored securely
octos auth login --provider deepseek    # use the provider you chose above

# 4. Start your agent with password-free local sign-in
octos serve --solo
```

Now open **http://localhost:50080** (it lands on `/app/`), click the local sign-in button, and say hello. That's the whole setup.

Prefer a hands-off install that runs Octos as a background service (auto-start, bundled skills, dashboard on port 8080)? Use the installer script instead — see [self-hosted install options](https://github.com/octos-org/octos-web#self-hosting--deployment):

```bash
# macOS / Linux
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```

### If something looks wrong

| Symptom | Fix |
|---|---|
| The page doesn't load | Is `octos serve --solo` still running? Solo serve uses port **50080**; the service installer uses port **8080** — check the one you set up. |
| The agent doesn't reply | No provider credential yet — run `octos auth login --provider <name>` (or export the provider's API key env var, or add the key in the dashboard settings). An `invalid model` error means the provider rejected the configured model name — re-run `octos init` and pick a real one (e.g. `deepseek-v4-flash`). |
| The dashboard (`/admin/`) asks for a login | Use the **"Login with admin token"** tab with the `Auth token:` the installer printed (also stored in the service file — see *First login to the dashboard* in the [octos-web self-hosting guide](https://github.com/octos-org/octos-web#self-hosting--deployment)). |
| Not sure what's wrong | `octos status` shows what's running; `octos doctor` checks your environment. |

### The pieces

- **octos** (this repo) — the **kernel**: the agent runtime, LLM providers, tools, sandbox, memory, channels, and the API everything else speaks. Install this first — then live in a client:
- **[octos-web](https://github.com/octos-org/octos-web)** — the full app experience in the browser (chat, voice, projects, slides, admin, and the hosted multi-tenant signup). A build ships inside the server — open `/app/`.
- **[octoscode](https://github.com/octos-org/octoscode)** — the terminal experience, in the spirit of Claude Code.

**Stuck?** [Documentation](https://octos-org.github.io/octos/) · [Issues](https://github.com/octos-org/octos/issues)

---

A Rust-native, API-first Agentic OS.

31MB static binary. 80+ REST endpoints + UI Protocol v1 over WebSocket/stdio. 16 LLM providers. 14 messaging channels. Multi-tenant. Zero external runtime services.

## What is Octos?

Octos is an open-source AI agent platform that lets you run your own AI system on a single machine or across a cloud-and-device pair. You deploy one Rust binary, connect your LLM provider and channels, and Octos handles routing, sessions, tools, memory, and multi-user isolation through a web dashboard and REST API.

You can think of it as the **backend operating system for AI agents**. Instead of building a new chatbot stack for every use case, you configure Octos profiles with their own prompts, models, tools, and channels, then manage them from one control plane.

Beyond the quick local setup above, Octos can be deployed three ways:

1. **Octos Cloud signup** — a hosted multi-tenant account at [octos.cloud](https://octos.cloud); the signup experience belongs to the web client (see the [octos-web README](https://github.com/octos-org/octos-web#octos-cloud)).
2. **Self-hosted local** — run Octos only on your own machine or local network.
3. **Self-hosted cloud + tenant pair** — run your own public VPS plus your own tenant device for internet-accessible remote use.

## Why Octos

Most agentic systems are single-tenant chat assistants — one user, one model, one conversation at a time. Octos is different:

- **API-first Agentic OS**: 80+ REST endpoints (chat, sessions, admin, profiles, skills, swarm, pipeline, metrics, webhooks) plus **UI Protocol v1** — a JSON-RPC contract over WebSocket and stdio for interactive clients. Any frontend — web, mobile, CLI, CI/CD — can be built on top.
- **Multi-tenant by design**: One 31MB binary serves 200+ profiles on a 16GB machine. Each profile is a separate OS process with isolated memory, sessions, and data. Family Plan sub-accounts.
- **Multi-LLM DOT pipelines**: Define workflows as DOT graphs. Per-node model selection. Dynamic parallel fan-out spawns N concurrent workers at runtime, with bounded concurrency for fleet stability.
- **Multi-agent topologies**: sub-agents (in-process children you own), peer agents (sovereign sibling sessions via `peer_handoff`/`peer_gather`), and a **swarm dispatcher** (fan contracts to N workers — native or external `claude -p`/`codex exec` — validator-gated, cost rolled up, at `/api/swarm/dispatch`). See [Agent topologies](#agent-topologies-sub-agents-peers--swarm).
- **3-layer provider failover**: RetryProvider → ProviderChain → AdaptiveRouter. Hedge racing, lane scoring, circuit breakers.
- **LRU tool deferral**: ~15 active tools for fast LLM reasoning, ~50 on demand. Idle tools auto-evict. `spawn_only` tools auto-redirect to background execution.
- **5 queue modes per session**: Followup, Collect, Steer, Interrupt, Speculative — users control agent concurrency via `/queue`.
- **Session control in any channel**: `/new`, `/s <name>`, `/sessions`, `/back` — works in Telegram, Discord, Slack, WhatsApp, DingTalk, Matrix, Feishu.
- **Sticky thread_id + committed_seq**: Every SSE event is bound to a thread; replay is deterministic by committed sequence number (M8.10).
- **3-layer memory**: Long-term (entity bank, auto-injected), episodic (task outcomes in redb), session (JSONL + LLM compaction, three-tier).
- **Autonomy — goals & loops**: `/goal <objective>` keeps the agent working across turns via checkpointed continuations (under a token budget); `/loop` runs a task on a fixed interval or self-paced. The agent keeps going between your messages — see [Autonomy: goals & loops](#autonomy-goals--loops).
- **Session time-travel**: `session/rollback` RPC with resume/rewind checkpoint pickers in both clients; every session can be rolled back to any prior user turn.
- **Live reasoning**: streams the model's thinking as it happens, with per-session `/thinking` effort control.
- **Voice**: per-profile cloud TTS voices, rich HTML/image voice output, dedicated batch-ASR routing via `ASR_API_URL`, and an OMiniX fallback for local ASR/TTS.
- **Native office suite**: PPTX/DOCX/XLSX via pure Rust (zip + quick-xml).
- **Sandbox isolation**: bwrap + Landlock/seccomp + sandbox-exec + Docker + Windows AppContainer. `deny(unsafe_code)` workspace-wide. 67 prompt injection tests.

## Self-hosting & deployment

The full setup and hosting guide — the three deployment paths (Octos Cloud
signup, self-hosted local, and self-hosted cloud + tenant pair), the install
scripts, package-manager installs, first dashboard login, uninstall, config
locations, and runtime modes — lives in the **[octos-web README →
Self-hosting & deployment](https://github.com/octos-org/octos-web#self-hosting--deployment)**.

The [Start here](#start-here) steps above are the quickest local install; that
guide covers the managed-signup, background-service, and public-VPS options.

## Build from source

For development against an unreleased checkout:

### Local development shortcuts

The repository's `Makefile` wraps the common local workflow while retaining
the existing build scripts. Run `make help` to see every target.

```bash
make init                         # creates ./.octos/config.json interactively
# set the API key variable for the provider selected during init, for example:
export OPENAI_API_KEY=your-key-here
make serve                        # API + local password-free sign-in
make dev                          # build /admin/ and /app/, then start the server
```

`make serve` builds only the `api` feature and serves on `127.0.0.1:50080` by
default. Override values when needed, for example
`make serve PORT=50081 FEATURES=api,telegram`. `make dev` requires Node.js;
it initializes the `octos-web` submodule and invokes the existing frontend
build scripts.

```bash
# Build and install. The features below are the canonical default
# (matches scripts/milestone-ci.sh) — `octos serve` requires `api`,
# and the gateway needs the relevant channel feature for each
# transport (telegram, discord, etc.). A bare `cargo install --path
# crates/octos-cli` will give you a binary missing `serve` and
# without channel adapters.
cargo install --path crates/octos-cli \
    --features "api,telegram,discord,dingtalk,whatsapp,feishu,twilio,wecom,wecom-bot"

# Initialize workspace
octos init

# Set API key (any supported provider — auto-detected during install)
export OPENAI_API_KEY=your-key-here    # or ANTHROPIC_API_KEY, GEMINI_API_KEY, etc.

# Interactive chat
octos chat

# Multi-channel gateway
octos gateway

# Web dashboard + REST API + UI Protocol
octos serve
octos serve --solo     # same, plus password-free local login for the web app
octos serve --stdio    # UI Protocol over stdio (how octoscode embeds a backend)
```

The full CLI surface (see `octos help`):

| Command | Purpose |
|---|---|
| `chat` / `gateway` / `serve` | the three runtime modes |
| `init` / `status` / `doctor` | workspace init, node status, environment diagnostics |
| `auth` / `account` / `admin` | provider login (OAuth/PKCE), sub-accounts, tenant & tunnel admin |
| `channels` / `cron` / `skills` | messaging channels, scheduled jobs, skill install/remove |
| `mcp-serve` | run octos as an MCP server, so outer orchestrators can drive it as a sub-agent |
| `mcp` | `mcp login` / `logout` for OAuth-gated MCP servers octos connects to as a **client** (external MCP tools are declared in `config.json` → `mcp_servers`) |
| `acp` | run octos as an [Agent Client Protocol](https://agentclientprotocol.com) agent over stdio, so editors like Zed drive it as their coding agent |
| `office` | PPTX/DOCX/XLSX manipulation from the shell |
| `update` / `clean` / `completions` / `docs` | release check, cache cleanup, shell completions, doc generation |

For a repo-local tenant deploy (builds from source, sets up the same service + tunnel as `install.sh`), use `scripts/local-tenant-deploy.sh --full`.

### Iterating on a system-installed octos

`cargo install --path crates/octos-cli --features "api,..."` only drops a binary into `~/.cargo/bin`. It does **not** rebuild the embedded admin dashboard or touch the service installed by `scripts/install.sh` (the LaunchDaemon on macOS / systemd unit on Linux runs `/usr/local/bin/octos`). If you have already run `install.sh` and want to redeploy local changes, use:

```bash
./scripts/build-local-bundle.sh --install           # build + bundle + reinstall
./scripts/build-local-bundle.sh --install --tunnel  # same, with tunnel flags passed through
./scripts/build-local-bundle.sh --skip-dashboard    # only Rust changed, skip npm/vite
```

What it does:

1. Detects your host triple (mirrors `install.sh`'s platform mapping).
2. Runs `scripts/build-dashboard.sh` (admin SPA → `/admin/`) and `scripts/build-web-app.sh` (the octos-web submodule → `/app/`) so `rust_embed` bakes both SPAs into the binary. Skip the dashboard build and `/admin/` returns a 503 `admin_bundle_missing` diagnostic; skip the web build and `/app/` returns `web_bundle_missing` (and the root `/` falls back to redirecting to `/admin/`).
3. Delegates `cargo build --release` to `scripts/milestone-ci.sh release-bundle` (single source of truth for `FEATURES` / `SKILL_CRATES`).
4. Tars binaries into `scripts/octos-bundle-<TRIPLE>.tar.gz`, which `install.sh` auto-detects via `file://`, skipping the GitHub download.
5. With `--install`, chains into `install.sh` — copies binaries to `$PREFIX`, rewrites the service plist/unit, reloads the daemon.

Use this when:

- You changed Rust **or** dashboard code and need to see it running under the installed service.
- You want to exercise the full installer flow against a local build.

Skip it when you just need the CLI — `cargo install --path crates/octos-cli --features "api,telegram,discord,dingtalk,whatsapp,feishu,twilio,wecom,wecom-bot"` is faster. Trim the feature list to only the channels you need (or just `api` for `octos chat` + `octos serve`); leaving `api` off is what causes `octos serve` to fail with `unrecognized subcommand 'serve'`.

## Use octos as a library

Everything above builds the **binary**. octos is also embeddable — as Rust crates, or through C/Python/Swift/Kotlin/JS bindings that wrap the same agent loop.

### Rust

The crates are **not published to crates.io** (`publish = false` in the workspace), so depend on them by git. Pin a tag; `main` moves fast:

```toml
[dependencies]
octos-core  = { git = "https://github.com/octos-org/octos", tag = "v2.0.2" }
octos-agent = { git = "https://github.com/octos-org/octos", tag = "v2.0.2" }
```

```rust
use octos_agent::{HookConfig, HookEvent, HookExecutor};

let executor = HookExecutor::new(vec![HookConfig {
    event: HookEvent::BeforeSpawnVerify,
    command: vec!["/usr/local/bin/verify-motion".into()],
    timeout_ms: 5000,
    tool_filter: vec![],
    path_filter: vec![],
    requires_bin: None,
}]);
```

Take the smallest layer that does the job — each row pulls in the ones above it:

| Crate | Use it for | Weight |
| --- | --- | --- |
| `octos-core` | protocol types, task model, IDs, codecs | leaf — pure deps, the only crate that compiles to `wasm32` |
| `octos-llm` | provider abstraction, failover/routing | + HTTP/TLS |
| `octos-memory` | episodic store, hybrid BM25 + vector recall | + `redb` (filesystem) |
| `octos-agent` | the full loop: tools, sandbox, approvals, hooks | + `tokio` multi-thread, browser/CDP |

Worked examples live in `crates/octos-agent/examples/` — `robot_domain_hook.rs` shows the domain-hook pattern integrators use to veto a dispatched sub-task from live telemetry, without adding domain-specific variants to the core.

Feature flags worth knowing: `octos-agent` defaults to `browser` (CDP `web_search` fallback) and offers `git` and `ast`; in-process embeddings come from `octos-embed-llama` (`embed-llama`, plus `metal` / `cuda`), which is cross-platform and defaults to a CPU backend.

### From other languages

`octos-ffi` is the native core; `octos-pyo3` and `octos-uniffi` are built over it, so those three share behaviour and feature flags. `octos-wasm` is separate — it binds `octos-core` only (see below).

| Binding | Target | Build |
| --- | --- | --- |
| **`octos-pyo3`** | Python — **the recommended Python path** | `maturin build --release` (from `crates/octos-pyo3/`) |
| `octos-ffi` | C ABI — C, Go, Node, anything with FFI | `cargo build -p octos-ffi --release` → `.a` / `.dylib` / `.so` + generated header |
| `octos-uniffi` | Python, Swift, Kotlin from one definition | `cargo build -p octos-uniffi`, then `cargo run -p octos-uniffi --bin uniffi-bindgen -- generate ...` |
| `octos-wasm` | Browser / JS | `wasm-pack build --target web` |

Add the in-process GGUF embedder to any of them with the same flag, e.g. `cargo build -p octos-ffi --release --features embed-llama` (or `embed-llama-metal` on Apple GPUs).

**The browser is protocol-only.** `octos-wasm` binds `octos-core` — wire (de)serialization, message/task/ID modelling — and nothing more. The agent loop cannot run in a browser: `redb` needs a filesystem, `tokio`'s multi-thread runtime needs OS threads, and native TLS and llama.cpp do not target `wasm32-unknown-unknown`. Run the agent behind `octos serve` and talk to it over the network.

Each binding has its own README with the full API and examples: [octos-ffi](crates/octos-ffi/README.md) · [octos-pyo3](crates/octos-pyo3/README.md) · [octos-uniffi](crates/octos-uniffi/README.md) · [octos-wasm](crates/octos-wasm/README.md).

## Clients and the UI Protocol

Interactive clients talk to `octos serve` over **UI Protocol v1** — a JSON-RPC contract carried on WebSocket (`/api/ui-protocol/ws`) or stdio (`octos serve --stdio`). It covers session open with cursor replay, streamed turns, durable persistence events, tool activity, approvals, background tasks, and rollback. The protocol spec is the contract: server and clients release independently against it.

- **[octos-web](https://github.com/octos-org/octos-web)** — the browser client: chat, voice/video, studio, slides, and sites. A build is embedded in the server binary at `/app/`, so `octos serve` works with zero extra deploys. (The admin dashboard is a separate SPA, embedded at `/admin/`.)
- **[octoscode](https://github.com/octos-org/octoscode)** — the terminal client. Connects to a running server over WebSocket, or spawns `octos serve --stdio` as its own private backend.
- **`octos mcp-serve`** — the inverse direction: octos as an MCP server, callable as a sub-agent from outer orchestrators.
- **MCP client** — octos also *consumes* external MCP servers. Declare them in `config.json` under `mcp_servers` and any octos agent (`chat`, `serve`, `gateway`, `acp`) gains their tools in its own registry — stdio (`command` + `args`) or HTTP (`url`); run `octos mcp login <url>` for OAuth-gated servers.

  ```json
  "mcp_servers": [
    { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/data"] },
    { "url": "https://mcp.example.com/mcp", "oauth": true }
  ]
  ```

  Stdio children receive a **sanitized environment** — only the names explicitly listed under the server's `env` map are forwarded (injection vectors like `LD_PRELOAD` are stripped even from that map), so a server expecting inherited credentials must get them explicitly or read its own secrets file. Set `"concurrency_class": "exclusive"` to serialize a server's tools (default `"safe"` allows concurrent calls) — the right knob for a single-resource server such as a one-device driver. Handshake timeout is 30s and each `tools/call` times out after 60s, so long-running work should be started detached by the server and polled through read-only tools.
- **`octos acp`** — the editor-facing direction: octos as an **[Agent Client Protocol](https://agentclientprotocol.com) (ACP)** agent over stdio, so ACP-speaking editors (Zed and others) run octos as their coding agent — with the **same capabilities as `octos chat`** (your tools + sandbox, long-term memory + `MEMORY.md`, skills/plugins, MCP, hooks, context compaction, provider failover). It appears in the editor's agent picker alongside Claude Code and Gemini CLI. See [Use octos in Zed](#use-octos-in-zed-acp).

### Use octos in Zed (ACP)

`octos acp` turns octos into an **ACP server** that [Zed](https://zed.dev) (and other ACP editors) drive as a first-class coding agent. You get the same agent stack as `octos chat` — your tools + sandbox, long-term memory + `MEMORY.md` injection, bundled skills/plugins, MCP servers, hooks, and context compaction — but inside the editor.

> **One gap today:** interactive tool-approval prompts and `ask_user_question` aren't surfaced to the editor yet — octos runs tools under its own (non-interactive) approval policy rather than ACP `session/request_permission`, so a tool that would pause for approval in `octos chat` won't prompt you in Zed. Everything else matches.

**1. Install octos and initialize it** (skip if you already have it — see [Start here](#start-here) for all install options):

```bash
npm install -g @octos-org/octos      # or Homebrew / build from source — see Start here
octos init                           # pick a provider + model, then paste that provider's API key
```

`octos init` walks you through choosing a provider + model (this guide uses **DeepSeek**) and then prompts you to **paste that provider's API key** — stored securely in `auth.json` and read regardless of environment (`octos acp` resolves its LLM exactly like `octos chat`). Pressed Enter to skip it? Add the key later with `octos auth login --provider deepseek`. A Dock-launched Zed does **not** inherit your shell's env vars, so if you'd rather pass the key by an env var, put it in the `env` block below instead.

**2. Register octos as an agent server** in Zed's settings (`~/.config/zed/settings.json`, or run *zed: open settings*). Use `"command": "octos"` if it's on your `PATH`, or the absolute path from `which octos` — a Dock-launched Zed has a minimal `PATH` and may not find a bare `octos`:

```jsonc
{
  "agent_servers": {
    "Octos": {
      "command": "octos",
      "args": ["acp", "--provider", "deepseek", "--model", "deepseek-chat"],
      "env": {}
    }
  }
}
```

> The `--provider`/`--model` in `args` must match the provider you set up in step 1 (this guide uses DeepSeek). `octos acp` inherits the rest — `base_url`, `api_type`, `api_key_env` — from your `octos init` config, so pointing `deepseek` args at a differently-configured provider sends the wrong key/endpoint and the session fails.

**3. Play with it in Zed.**
- **Open a folder** — external agents need a workspace (with none open, the Agent Panel just shows *"Open Project"*).
- Open the **Agent Panel** (right dock), click the **＋ New Thread** dropdown (or press `⌥⌘⇧N`), and choose **Octos**.
- Type a prompt. octos runs the agent loop and streams tools, thinking, and results back into Zed — and it remembers across turns via your `MEMORY.md`.

> **Can't find Octos?** It lives in the **＋ New Thread** menu (external agents) — **not** the `⋯` → *MCP / Context Servers* list (that's a different feature). After editing `agent_servers`, fully quit and reopen Zed (`Cmd-Q`) so it reloads the config.

Flags mirror `octos chat`: `--provider`, `--model`, `--base-url`, `--config`, `--data-dir`, `--cwd`, `--profile`, and `--max-iterations`. Zed sends a per-session working directory with `session/new`; that's where octos roots tools, skills, and the filesystem scope.

## Headless agent mode & code review (`octos chat`)

`octos chat` is both an interactive REPL and a **one-shot headless agent** — the
`claude -p "…"` / `codex exec` equivalent. It has file, search, and shell tools,
so it reads code, runs `git diff`, and runs tests on its own; you just give it a
task.

```bash
octos chat                             # interactive REPL
octos chat "explain crates/octos-agent/src/agent.rs"   # one-shot: run one turn, exit
octos chat -m "…" --json               # one-shot, machine-readable result on stdout
```

### Modes: interactive, one-shot, JSON

| Invocation | Behavior |
| --- | --- |
| `octos chat` | interactive REPL (multi-turn) |
| `octos chat "PROMPT"` **or** `octos chat -m "PROMPT"` | one-shot: run a single turn and exit (`claude -p` parity) |
| `octos chat -m "PROMPT" --json` | one-shot, one JSON result object on **stdout** (logs/UI → stderr) |

Rules: give the prompt **positionally OR** via `-m`/`--message`, never both (that's
an error — the positional prompt is otherwise folded into `--message`). `--json`
needs a **one-shot prompt** (positional *or* `-m`); only **interactive** `--json`
(no prompt at all) is rejected, since a REPL can't keep stdout clean. On any error
`--json` still prints `{"error":"…"}` on stdout and exits non-zero, so stdout stays
machine-parseable.

### Scripting: the JSON result & exit codes

In `--json` mode stdout carries exactly one object (everything else — logs, the
status line, approval prompts — goes to stderr). On success:

```json
{ "text": "…final answer…", "model": "glm-5.3", "input_tokens": 1234, "output_tokens": 567 }
```

`text` is the final assistant answer; `model` is the model that actually
produced it (honest about adaptive failover to a fallback lane); the token
counts cover the whole turn. On any failure the object is `{"error":"…"}`
instead, so stdout is always parseable:

```bash
out=$(octos chat --profile dev -m "summarize the branch diff" --json)
echo "$out" | jq -r '.text'           # the answer
echo "$out" | jq -r '.output_tokens'  # token usage, e.g. for cost tracking
```

| Exit | Meaning |
| --- | --- |
| `0` | success |
| `2` | usage error — unknown / conflicting flags, `--json` without `-m`, or the prompt given both positionally and via `-m` |
| `1` | runtime error (auth / provider / agent). In `--json` mode `{"error":…}` is still written to stdout first, so a caller can read the reason |

### Sandbox × approval — every combination

Two orthogonal axes set what the agent may do unattended: **`--sandbox`**
(filesystem/network reach) and **`--ask-for-approval`** (whether risky commands
pause). `--yolo` is a shortcut for the most permissive corner. What each
combination resolves to:

| `--sandbox` | `--ask-for-approval` | Resolves to | The agent can… |
| --- | --- | --- | --- |
| *(omitted)* | *(omitted)* | **workspace-write + ask** — the default | read + write inside `--cwd`; pause for approval on risky commands |
| `read-only` | *(omitted → `ask`)* | read-only + ask | read + read-only commands (`git diff`, `grep`); write/edit tools fail |
| `read-only` | `never` | read-only + never | **unattended review** — reads only, never pauses |
| `workspace-write` | *(omitted → `ask`)* | workspace-write + ask | edit inside `--cwd`, pause on risky |
| `workspace-write` | `never` | workspace-write + never | **unattended edits** inside `--cwd` |
| `danger-full-access` | *(forced `never`)* | full access, no prompts | host filesystem + network, no approvals |
| `--yolo` | — | = `danger-full-access` + `never` | **full autonomy** (the shortcut) |

**Contradictions are rejected — the command errors, it doesn't silently pick one:**
- `--yolo` (or `--sandbox danger-full-access`) **+** `--ask-for-approval ask` — danger-full-access never asks.
- `--yolo` **+** `--sandbox read-only`/`workspace-write` — `--yolo` *is* danger-full-access.

`--ask-for-approval never` fails a risky command **closed** at the tool boundary
(there's no interactive approver in a headless run) rather than prompting.
Guardrails that stay on even under `danger-full-access`/`--yolo`:
`before_tool_call` hooks, `ToolPolicy` deny-lists, SSRF protection,
`BLOCKED_ENV_VARS`.

### Reuse an existing profile (model + API key)

`--profile <id>` reads a stored serve/onboarding profile
(`~/.octos/profiles/<id>.json`, created by `octos serve` or octoscode) and
reuses its provider, model, route, and API key — so you don't re-enter them:

```bash
octos chat --profile dev --yolo "refactor this module"   # uses dev's model + key
```

Precedence: `--config` > `--profile <id>` > ambient `config.json`.
`--provider` / `--model` / `--base-url` / `--api-type` each override their own
field on top. Naming a **different** `--provider` than the profile's does a
**clean switch** — it detaches the profile's route (base-url, key-env, wire
protocol) so the new provider's own defaults apply, rather than reusing the old
provider's key against the new one; add `--model` too, since the profile's model
won't fit the new provider. Re-naming the *same* provider keeps the route.

### Code review

The agent reads the code and returns its findings as its final answer on
**stdout** — capture it with your shell. (Stdout is outside the sandbox, so a
`read-only` reviewer, which cannot touch the repo, can still "produce a file".)

```bash
octos chat --profile dev --cwd ~/repo \
  --sandbox read-only --ask-for-approval never --effort high \
  -m "Review the diff of this branch against main. For each issue give file:line,
      severity, and a concrete failure scenario. Rank most-severe first." \
  > review.md
```

If you want the **agent itself** to write files (not shell capture), use
`--sandbox workspace-write` and tell it to write them — `read-only` blocks the
write. `workspace-write` lets it write anywhere under `--cwd`, so for a contained
run point it at a fresh `git worktree` and inspect the diff afterward.

### Run many agents in parallel on one profile

Add `--no-session-persistence` and point N agents at one `--data-dir` (hence one
shared `--profile`); they run concurrently — the ephemeral flag drops the
exclusive episode-store lock that would otherwise serialize them.

```bash
# Review fan-out — one repo, many lenses, each writes its own report
for lens in correctness security performance; do
  octos chat --profile dev --cwd ~/repo \
    --sandbox read-only --ask-for-approval never --no-session-persistence \
    -m "Review only for $lens. Write findings to REVIEW-$lens.md." &
done; wait

# Edit fan-out — one agent per folder, each changes its own tree
for d in svc-a svc-b svc-c; do
  octos chat --profile dev --cwd ~/work/$d \
    --sandbox workspace-write --ask-for-approval never --no-session-persistence \
    -m "Implement the TODOs in this folder." &
done; wait
```

Without `--no-session-persistence`, a second `octos chat` on the same
`--data-dir` fails with `Database already open` — that flag is what makes the
fan-out non-blocking.

### Full flag reference

| Flag | Default | Purpose |
| --- | --- | --- |
| `PROMPT` *(positional)* / `-m`, `--message <s>` | — | one-shot prompt (two spellings of the same thing; supplying both errors) |
| `--json` | off | one JSON result object on stdout; needs a one-shot prompt (positional or `-m`) — only interactive `--json` is rejected |
| `--sandbox <mode>` | `workspace-write` | `read-only` \| `workspace-write` \| `danger-full-access` (see the matrix above) |
| `--ask-for-approval <mode>` | `ask` | `ask` \| `never` (danger-full-access is always `never`) |
| `--yolo` | off | shortcut for `--sandbox danger-full-access` (approvals never). Alias of `--dangerously-bypass-approvals-and-sandbox`. **Local single-user boxes only.** |
| `--profile <id>` | `coding` | runtime **tool surface** (`coding` = files/shell/search/memory/spawn; `coding-full` adds web/pipelines/skills; or a user id). If `<id>` names a stored serve/tui profile, also reuses its **model + API key**. |
| `--provider <name>` | from config/profile | LLM provider override |
| `--model <id>` | from config/profile | model override |
| `--base-url <url>` | from config/profile | custom API endpoint |
| `--api-type <t>` | from config/profile | wire protocol for `--base-url`: `anthropic` \| `openai` \| `responses` (alias `--api-style`) |
| `--config <path>` | — | explicit flat config file — **wins over** `--profile` |
| `--cwd <dir>` | current dir | workspace root the agent reads/writes |
| `--data-dir <dir>` | `$OCTOS_HOME` / `~/.octos` | episodes / memory / sessions store |
| `--effort <e>` | provider default | `none` \| `low` \| `medium` \| `high` \| `max` (`none` disables reasoning where supported) |
| `--no-session-persistence` | off | ephemeral run (no episode saved); also **enables parallel agents** on one `--data-dir` |
| `--max-iterations <n>` | `20` | per-turn tool-call cap |
| `--no-retry` | off | disable automatic retry on transient LLM errors |
| `-v`, `--verbose` | off | show tool outputs |

**Model/credential precedence:** `--config` > `--profile <id>` > ambient
`config.json`; `--provider` / `--model` / `--base-url` / `--api-type` override
whichever of those supplied them. **Prerequisite:** a configured provider
(`octos auth login`, or the provider's API-key env var) **or** a `--profile` that
already carries one.

## Autonomy: goals & loops

Beyond a single turn, octos can keep working on its own — between your messages,
or on a schedule. Both are driven by slash commands in any client (octos-web,
octoscode) or channel session, and run on `octos serve` / `octos gateway`.

### Goals — keep going until it's done

`/goal <objective>` gives the agent a standing objective. After each turn it
checkpoints a **continuation** and re-fires itself to keep making progress across
turns — without you prompting again — until the objective is met or its budget
runs out.

```text
/goal build a REST API for the todo app, with tests   # start a goal
/goal <objective> --budget 5000000                    # cap it (default 2,000,000 tokens)
/goal stop                                            # end the goal
/goal resume                                          # re-activate a stopped goal
```

Each goal carries a **token budget** (default **2M**). When it's exhausted the
goal moves to `budget_limited`, wraps up the current state, and tells you how to
resume — reply `/goal <objective> --budget <N>` with a higher `N`, or `/goal
stop`. Continuations are rate-limited (≥30s apart, ≤12/hour) so a goal can't spin.

### Loops — run on a cadence

`/loop <prompt>` runs a recurring task. Give it an interval (`s`/`m`/`h`/`d`) for
a **fixed-interval** loop, or omit one for a **self-paced** loop where the agent
decides when to wake itself next:

```text
/loop 30m check CI and triage new failures        # fixed interval (leading)
/loop summarize unread email every 1h             # fixed interval (trailing)
/loop watch the deploy and report when it's green  # self-paced (no interval)
/loop resume <id>                                 # resume a paused loop
```

Loops persist across restarts (parked as paused on a solo reboot; you resume
them explicitly), and can be paused, listed, fired now, or deleted.

## Agent topologies: sub-agents, peers & swarm

A single `octos chat` or session runs one agent. When work needs *several*
agents, octos offers three relationships — they differ by **who owns whom** and
**how results come back**:

| | **Sub-agents** | **Peer agents** | **Agent swarm** |
| --- | --- | --- | --- |
| Relationship | hierarchical — a parent **owns** its children | lateral — **sovereign** sibling sessions | a dispatcher fans **contracts** to N workers |
| Started by | the `spawn_agent` tool, mid-turn | `peer_handoff` stages one; the **client** opens it | `Swarm::dispatch` / `POST /api/swarm/dispatch` |
| Live where | in-process children of the caller | independent sessions (own history, own client tab) | wherever the backend runs |
| Results | returned to the parent (final answer only; internals stay private) | fan-in via `peer_gather` over a shared blackboard | aggregated, validator-gated, cost rolled up |
| Workers | native octos agents | native octos sessions | native **or external CLI/MCP** agents |

### Sub-agents — delegation you own

A running agent calls **`spawn_agent`** (or its MCP-backed `delegate` variant)
to hand a scoped task to a child that runs **in the same process**. The parent
supervises the whole tree: status and token cost surface upward, cancelling the
parent cascade-fails its live children, and each child runs in its own sandbox.
A child's internal messages never leak back — only its final result. Spawns nest
(bounded by a max depth). The tools sit in the default `coding` tool surface, so
any agent with that profile can delegate; background (`spawn_only`) children run
detached and report when done.

### Peer agents — sovereign siblings

Sometimes you want a *second, equal* session rather than a child — its own tab
with its own history that a human can watch and steer. Sessions are coupled to a
client connection, so the model can't open one itself: **`peer_handoff`** instead
*stages* a peer server-side (a durable brief ≤ 64 KB, optionally fenced in its
own git worktree) and the host asks your **client** to open it in the background.
The originating agent later pulls results back with **`peer_gather`** — a shared
blackboard where handoff fans out and gather fans in. Guardrails live at the
serve layer: peer sessions can't themselves hand off (depth-1) and a per-turn
handoff cap applies. This path is **opt-in** and exists only on the
`serve`/WebSocket turn path (a client that can open sessions) — not `chat`,
`gateway`, or ACP.

### Agent swarm — a dispatcher over N workers

For fan-out at scale, **`octos-swarm`** runs the PM/supervisor pattern as a
primitive: a supervisor writes a **contract**, `Swarm::dispatch` fans it into N
sub-contracts across a **topology** — `Parallel` (bounded concurrency),
`Sequential` (one-at-a-time, aborts on the first terminal failure, crash/resume
aware), `Pipeline` (output of *i* feeds *i+1*), or `Fanout` (expand a typed
pattern, then run it parallel) — aggregates the artifacts, gates the aggregate
through a **validator**, and rolls up cost in an idempotent redb **ledger**
(re-dispatching the same id returns the stored result verbatim). Reachable at
`POST /api/swarm/dispatch`. A swarm worker need **not** be a native octos agent:
with **`--swarm-backend`**, contracts dispatch to *external* agents — an MCP
server, or a one-shot CLI like `claude -p` / `codex exec` (`CliAgentBackend`).

### "External agent" points two ways

The term is directional — check who is calling whom:

- **Inbound** — an outside orchestrator drives octos: a Zed/ACP client, or any
  MCP client via `octos mcp-serve`. Octos is the *callee* (a sub-agent to someone
  else).
- **Outbound** — octos drives an outside agent as a **swarm worker** via
  `--swarm-backend`. Octos is the *caller*.

Same phrase, opposite arrows.

## Documentation

📖 **[Full Documentation](https://octos-org.github.io/octos/)** — installation, configuration, channels, providers, memory, skills, advanced features, and more.

**Quick links:**
- [Installation & Deployment](https://octos-org.github.io/octos/installation.html)
- [Configuration](https://octos-org.github.io/octos/configuration.html)
- [LLM Providers & Routing](https://octos-org.github.io/octos/providers.html)
- [Gateway & Channels](https://octos-org.github.io/octos/channels.html)
- [Memory & Skills](https://octos-org.github.io/octos/memory-skills.html)
- [Advanced Features](https://octos-org.github.io/octos/advanced.html) (queue modes, hooks, sandbox, tools)
- [CLI Reference](https://octos-org.github.io/octos/cli-reference.html)
- [Skill Development](https://octos-org.github.io/octos/skill-development.html)

**中文:** [中文 README](README-zh.md) | [用户指南](https://octos-org.github.io/octos/zh/) (doc site)

## Architecture

12 `octos-*` crates + 13 app-skill crates + 1 platform-skill crate (26 workspace members total). The runtime auto-installs only the 8 entries in `BUNDLED_APP_SKILLS` plus the `voice` platform-skill — see `crates/octos-agent/src/bundled_app_skills.rs`.

```
octos-cli   (CLI entrypoint, REST API server, dashboard, config watcher, wizard)
   │
octos-agent (agent loop, tool registry, MCP, hooks, three-tier compaction,
             profile system, sub-agent output router, task supervisor)
   │
   ├─ octos-bus       (14 channels, sessions w/ sticky thread_id, coalescing, cron)
   ├─ octos-llm       (15 providers, AdaptiveRouter → ProviderChain → RetryProvider)
   ├─ octos-memory    (long-term + episodic + HNSW vector + BM25 hybrid search)
   ├─ octos-pipeline  (DOT-graph workflows, per-node model, bounded fan-out)
   ├─ octos-plugin    (skill manifest, discovery, gating, lifecycle, protocol v2)
   ├─ octos-sandbox   (platform sandbox helper binary — bwrap/Landlock/seccomp)
   ├─ octos-swarm     (PM/swarm dispatcher, ledger, topology, validator gate)
   ├─ octos-diagnostics (shared doctor diagnostics + update planning)
   ├─ octos-dora-mcp  (compat re-export of the dora bridge in octos-agent)
   └─ octos-core      (Task, Message, Error types — no internal deps)

Runtime view:
  octos serve (control plane + dashboard, 80+ REST endpoints + UI Protocol WS)
    ├── Profile A → gateway process (Telegram, WhatsApp)
    ├── Profile B → gateway process (Feishu, Slack, Matrix)
    └── Profile C → gateway process (CLI)
         │
         ├── LLM Provider (Anthropic, OpenAI, Gemini, DeepSeek, Moonshot, …)
         │   └── AdaptiveRouter → ProviderChain → RetryProvider
         ├── Tool Registry (~50 built-in + plugins + 8 app-skills)
         │   └── LRU Deferral (~15 active, activate on demand)
         ├── Pipeline Engine (DOT graphs, per-node model, bounded fan-out)
         ├── Swarm Dispatcher (fan-out → aggregate → validator gate → cost rollup)
         ├── Sandbox (bwrap / Landlock+seccomp / sandbox-exec / Docker / AppContainer)
         ├── Session Store (JSONL, LRU cache, three-tier compaction, thread_id)
         ├── Memory (MEMORY.md + entity bank + episodes.redb + HNSW)
         └── Skills (bundled + installable from octos-hub)
```

## License

See [LICENSE](LICENSE).
