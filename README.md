# Octos 🐙

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

Now open **http://localhost:50080/app/**, click the local sign-in button, and say hello. That's the whole setup.

Prefer a hands-off install that runs Octos as a background service (auto-start, bundled skills, dashboard on port 8080)? Use the installer script instead — see [Option 2](#option-2-self-hosted-local-only) below:

```bash
# macOS / Linux
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```

### If something looks wrong

| Symptom | Fix |
|---|---|
| The page doesn't load | Is `octos serve --solo` still running? Solo serve uses port **50080**; the service installer uses port **8080** — check the one you set up. |
| The agent doesn't reply | No provider credential yet — run `octos auth login --provider <name>` (or export the provider's API key env var, or add the key in the dashboard settings). An `invalid model` error means the provider rejected the configured model name — re-run `octos init` and pick a real one (e.g. `deepseek-v4-flash`). |
| The dashboard (`/admin/`) asks for a login | Use the **"Login with admin token"** tab with the `Auth token:` the installer printed (also stored in the service file — see *First login to the dashboard* under Option 2). |
| Not sure what's wrong | `octos status` shows what's running; `octos doctor` checks your environment. |

### The pieces

- **octos** (this repo) — the **kernel**: the agent runtime, LLM providers, tools, sandbox, memory, channels, and the API everything else speaks. Install this first — then live in a client:
- **[octos-web](https://github.com/octos-org/octos-web)** — the full app experience in the browser (chat, voice, projects, slides, admin, and the hosted multi-tenant signup). A build ships inside the server — open `/app/`.
- **[octos-tui](https://github.com/octos-org/octos-tui)** — the terminal experience, in the spirit of Claude Code.

**Stuck?** [Documentation](https://octos-org.github.io/octos/) · [Issues](https://github.com/octos-org/octos/issues)

---

**Open Cognitive Tasks Orchestration System** — a Rust-native, API-first Agentic OS.

31MB static binary. 80+ REST endpoints + UI Protocol v1 over WebSocket/stdio. 15 LLM providers. 14 messaging channels. Multi-tenant. Zero external runtime services.

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
- **Swarm dispatcher**: Fan contracts to N sub-agents, aggregate artifacts, gate through validator, roll up cost — wired into `/api/swarm/dispatch`.
- **3-layer provider failover**: RetryProvider → ProviderChain → AdaptiveRouter. Hedge racing, lane scoring, circuit breakers.
- **LRU tool deferral**: ~15 active tools for fast LLM reasoning, ~50 on demand. Idle tools auto-evict. `spawn_only` tools auto-redirect to background execution.
- **5 queue modes per session**: Followup, Collect, Steer, Interrupt, Speculative — users control agent concurrency via `/queue`.
- **Session control in any channel**: `/new`, `/s <name>`, `/sessions`, `/back` — works in Telegram, Discord, Slack, WhatsApp, DingTalk, Matrix, Feishu.
- **Sticky thread_id + committed_seq**: Every SSE event is bound to a thread; replay is deterministic by committed sequence number (M8.10).
- **3-layer memory**: Long-term (entity bank, auto-injected), episodic (task outcomes in redb), session (JSONL + LLM compaction, three-tier).
- **Autonomy loops & goals**: `/loop` runs fixed-interval or self-paced maintenance loops; goals continue across turns with checkpointed continuations — the agent keeps working between your messages.
- **Session time-travel**: `session/rollback` RPC with resume/rewind checkpoint pickers in both clients; every session can be rolled back to any prior user turn.
- **Live reasoning**: streams the model's thinking as it happens, with per-session `/thinking` effort control.
- **Voice**: per-profile cloud TTS voices, rich HTML/image voice output, and an OMiniX runtime provider for local ASR/TTS.
- **Native office suite**: PPTX/DOCX/XLSX via pure Rust (zip + quick-xml).
- **Sandbox isolation**: bwrap + Landlock/seccomp + sandbox-exec + Docker + Windows AppContainer. `deny(unsafe_code)` workspace-wide. 67 prompt injection tests.

## Choose a setup path

If you just want an assistant on your own machine, you already have it — the [Start here](#start-here) steps above are Option 2 in its simplest form. The paths below matter when you want a managed signup, a background service, or public internet access.

| Option | Machines involved | Public internet access | Who manages the infrastructure | Best fit |
| --- | --- | --- | --- | --- |
| **1. Octos Cloud signup** | Your device + Octos Cloud | Yes | Octos Cloud + you | Hosted accounts — [guide in octos-web](https://github.com/octos-org/octos-web#octos-cloud) |
| **2. Self-hosted local-only** | One machine | No | You | Local/private use |
| **3. Self-hosted cloud + tenant pair** | Your VPS + your device | Yes | You | Full self-hosting with remote access |

Visual overview:

<img src="images/octos-options.jpg" alt="Three ways to run Octos: Octos Cloud signup, self-hosted local-only, and self-hosted cloud plus tenant pair" width="100%" />

### Option 1: Sign up on Octos Cloud

Octos Cloud is the hosted, multi-tenant way in: register with your email at
[octos.cloud](https://octos.cloud) (or a self-hosted operator's portal), pick a
node name, and run one generated setup command on your device. The signup and account experience is part of the **web client** —
the walkthrough lives in the
[octos-web README (Octos Cloud)](https://github.com/octos-org/octos-web#octos-cloud).

This repo's side of that story is the **server infrastructure** an operator
runs to offer it: see [Option 3](#option-3-self-hosted-cloud--tenant-pair)
for deploying the cloud host (portal, relay, wildcard TLS) yourself.

### Option 2: Self-hosted local-only

Choose this if you want Octos on your own machine with no public exposure. Your dashboard is available only on the machine itself or your local network.

```bash
# macOS / Linux
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex
```

This installs the binary, sets up `octos serve` as a service, and starts the local dashboard at `http://localhost:8080/admin/`. The end-user web app is served same-origin at `http://localhost:8080/app/` (embedded in the binary — no separate web server needed).

**First login to the dashboard.** The install summary prints your credential once:

```text
Auth token: 3f2a…64-hex…c9d1
```

Open `http://localhost:8080/admin/`, switch the login screen to the
**"Login with admin token"** tab, and paste that token — you're in as the
admin user. (The email-code tab needs the server's SMTP configured, so the
token tab is the way in on a fresh local install.)

Lost the token? It's kept in the service definition the installer wrote:

```bash
# macOS
grep -A1 OCTOS_AUTH_TOKEN /Library/LaunchDaemons/io.octos.serve.plist
# Linux
grep OCTOS_AUTH_TOKEN /etc/systemd/system/octos-serve.service
```

Alternatively, install just the binaries (the `octos` server plus its bundled skills) via a package manager:

```bash
# Homebrew (macOS Apple Silicon, Linux x86_64/ARM64) — this repo is its own tap
brew tap octos-org/octos https://github.com/octos-org/octos
brew install octos-org/octos/octos

# npm (macOS Apple Silicon, Linux x86_64/ARM64, Windows x64)
npm install -g @octos-org/octos
```

Both install the full release bundle — the `octos` server (with the web app and dashboard embedded) and its bundled skills (`news_fetch`, `deep-search`, `deep_crawl`, `send_email`, `account_manager`, `clock`, `weather`, plus the `voice` platform-skill) kept side-by-side so `octos serve` discovers them at startup. Unlike `install.sh`, they do not set up a background service; run `octos serve` yourself.

Supported platforms: **macOS ARM64**, **Linux x86_64**, **Linux ARM64**, and **Windows x64**.

Choose this path if you want:

- the simplest self-hosted setup
- one machine only
- local-network access only
- the option to upgrade later to tenant mode

### Option 3: Self-hosted cloud + tenant pair

Choose this if you want full self-hosting but still want your own device accessible from anywhere on the public internet.

This mode uses two machines:

- a **cloud VPS** that runs the public relay and HTTPS entrypoint
- your **tenant device** that runs your own Octos instance

The tenant device connects outbound to the VPS using `frpc`. The VPS runs the public components, including TLS and routing. This gives you ngrok-style public access, but through your own infrastructure.

For production use, the VPS also needs wildcard HTTPS. The current setup uses Caddy plus Cloudflare DNS challenge, or another supported DNS provider, to issue and manage certificates for the main domain and tenant subdomains.

Requirements for this option:

1. **Your own hosted domain name**
   Example: `octos.example.com`
2. **A DNS provider / authoritative DNS API**
   Its role here is specifically the ACME `DNS-01` solver used by Caddy's internal ACME client to mint the wildcard certificate for `*.octos.example.com`, which is what tenant subdomains use. If you stay HTTP-only with `--http-only`, or if you only need the apex domain, this wildcard-DNS flow is not required.
3. **An SMTP service**
   This is needed so the cloud host can send OTP emails to tenants during portal signup and login.

#### 1. Bootstrap the VPS

On a Linux VPS with DNS already pointed at it, you can either:

- run the script with full flags for a mostly non-interactive flow, or
- run `bash scripts/cloud-host-deploy.sh` with no flags and let it prompt you interactively

Before running it, export the environment variables needed by your chosen providers. For example:

```bash
export CF_API_TOKEN=xxx
export SMTP_PASSWORD=xxx
```

Notes:

- For Cloudflare, the script expects `CF_API_TOKEN` for the DNS provider token.
- For SMTP, you can pre-export `SMTP_PASSWORD` so the bootstrap does not need that secret entered later.
- If you enable SMTP, the script will also prompt for or use the rest of the SMTP settings such as host, port, username, and from-address.

Example using explicit flags:

```bash
git clone https://github.com/octos-org/octos.git
cd octos
bash scripts/cloud-host-deploy.sh \
    --domain octos.example.com \
    --https --dns-provider cloudflare
```

Interactive mode:

```bash
git clone https://github.com/octos-org/octos.git
cd octos
bash scripts/cloud-host-deploy.sh
```

This wraps three host-side steps:

- `scripts/install.sh` — installs `octos serve` and sets `mode = "cloud"`
- `scripts/frp/setup-frps.sh` — installs and configures `frps`
- `scripts/frp/setup-caddy.sh` — configures public routing and wildcard HTTPS

Windows Server targets use the PowerShell deploy script from an operator machine
with OpenSSH access to the server:

```powershell
.\scripts\deploy.ps1 `
    -HostName win.example.com `
    -User Administrator `
    -Version latest `
    -RemoteRoot 'C:\octos' `
    -ServiceName OctosServe
```

Run the same command with `-DryRun` first to print the remote commands without
connecting. The script deploys the `octos-bundle-x86_64-pc-windows-msvc.zip`
release bundle, installs `octos.exe` under `C:\octos\bin`, stores runtime data in
`C:\octos\data`, writes logs under `C:\octos\logs`, and registers `OctosServe` as
an auto-start Windows service through NSSM. Use `-LocalBundle <zip>` to deploy a
locally built bundle over `scp`, and `-Uninstall [-Purge]` to remove the service
and optionally delete the remote install root.

Recommended DNS split:

- `octos.example.com` and `*.octos.example.com` for the portal and tenant dashboards
- `frps.octos.example.com` as `DNS only` so tenant machines can reach the FRP control port

#### 2. Register or create a tenant

Once the VPS is up, the cloud host can issue a personalized tenant setup command. That command includes the tenant name, per-tenant tunnel token, SSH port, dashboard auth token, domain, and relay address. The user receives this command directly in the portal and also by email.

#### 3. Run the tenant setup command on your own device

Use the exact command provided in step 2. The example below is reference only, to show what kind of command the portal issues:

```bash
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash -s -- \
    --tunnel \
    --tenant-name alice \
    --frps-token <per-tenant-uuid> \
    --ssh-port 6001 \
    --domain octos.example.com \
    --frps-server frps.octos.example.com \
    --auth-token <dashboard-token>
```

The installer writes the tenant tunnel configuration, installs `frpc`, and starts the public tunnel alongside `octos serve`. The `--auth-token` in your personalized command doubles as your dashboard login: open `https://<your-name>.<domain>/admin/` and paste it into the **"Login with admin token"** tab (the same command also arrives by email, so the token is recoverable there).

### Can I start local and upgrade later?

Yes.

A local-only self-hosted machine can be upgraded later to tenant mode once you have a cloud host available. The saved installers support this directly:

```bash
# macOS / Linux
~/.octos/bin/install.sh --tunnel
~/.octos/bin/install.sh --doctor
```

```powershell
# Windows
& "$HOME\.octos\bin\install.ps1" -Tunnel
& "$HOME\.octos\bin\install.ps1" -Doctor
```

That upgrade path is intentional: start with one machine, then add a VPS only when you need internet-facing access.

### Optional self-hosted features

```bash
# Auto-install runtime dependencies (git, node, python, ffmpeg, chromium)
curl ... | bash -s -- --install-deps

# Set up Caddy reverse proxy with HTTPS for self-hosted local deployments
curl ... | bash -s -- --caddy-domain crew.example.com
```

### Uninstall

Use the matching uninstall flag on the machine you want to remove:

```bash
# Tenant or local machine (macOS / Linux)
~/.octos/bin/install.sh --uninstall

# Tenant or local machine (Windows PowerShell)
& "$HOME\.octos\bin\install.ps1" -Uninstall

# Cloud VPS — removes octos serve, frps, and Caddy
bash scripts/cloud-host-deploy.sh --uninstall

# Cloud VPS + wipe data directory (~/.octos) as well
bash scripts/cloud-host-deploy.sh --uninstall --purge
```

### Where config lives

User config + credentials live **outside** the install dir so reinstalls/upgrades never touch them:

- **macOS + Linux:** `~/.config/octos/` (`config.json`, `auth.json`) — honours `$XDG_CONFIG_HOME`
- **Windows:** `%APPDATA%\octos\`
- **Override:** set `OCTOS_CONFIG_DIR` to put config/auth anywhere
- `~/.octos/` holds only the **install + runtime state** (binaries, bundled skills, sessions, logs). The installer writes only there.

An existing `~/.octos/config.json` from older versions is auto-migrated to `~/.config/octos/` on first run (copied, not moved — the original stays as a backup).

### Runtime deployment modes

Octos uses `"mode"` in `config.json` (see *Where config lives* above) to describe how a running node behaves:

- **`local`** — standalone machine
- **`tenant`** — end-user machine with an optional public tunnel
- **`cloud`** — VPS relay with tenant management and public signup

`scripts/install.sh` and `scripts/install.ps1` create local or tenant configs. `scripts/cloud-host-deploy.sh` creates or updates cloud-host configs with `mode = "cloud"` plus `tunnel_domain` and `frps_server`.

## Build from source

For development against an unreleased checkout:

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
octos serve --stdio    # UI Protocol over stdio (how octos-tui embeds a backend)
```

The full CLI surface (see `octos help`):

| Command | Purpose |
|---|---|
| `chat` / `gateway` / `serve` | the three runtime modes |
| `init` / `status` / `doctor` | workspace init, node status, environment diagnostics |
| `auth` / `account` / `admin` | provider login (OAuth/PKCE), sub-accounts, tenant & tunnel admin |
| `channels` / `cron` / `skills` | messaging channels, scheduled jobs, skill install/remove |
| `mcp-serve` | run octos as an MCP server, so outer orchestrators can drive it as a sub-agent |
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
2. Runs `scripts/build-dashboard.sh` (admin SPA → `/admin/`) and `scripts/build-web-app.sh` (the octos-web submodule → `/app/`) so `rust_embed` bakes both SPAs into the binary. Skip the dashboard build and `/admin/` will 307-loop; skip the web build and `/app/` returns `web_bundle_missing`.
3. Delegates `cargo build --release` to `scripts/milestone-ci.sh release-bundle` (single source of truth for `FEATURES` / `SKILL_CRATES`).
4. Tars binaries into `scripts/octos-bundle-<TRIPLE>.tar.gz`, which `install.sh` auto-detects via `file://`, skipping the GitHub download.
5. With `--install`, chains into `install.sh` — copies binaries to `$PREFIX`, rewrites the service plist/unit, reloads the daemon.

Use this when:

- You changed Rust **or** dashboard code and need to see it running under the installed service.
- You want to exercise the full installer flow against a local build.

Skip it when you just need the CLI — `cargo install --path crates/octos-cli --features "api,telegram,discord,dingtalk,whatsapp,feishu,twilio,wecom,wecom-bot"` is faster. Trim the feature list to only the channels you need (or just `api` for `octos chat` + `octos serve`); leaving `api` off is what causes `octos serve` to fail with `unrecognized subcommand 'serve'`.

## Clients and the UI Protocol

Interactive clients talk to `octos serve` over **UI Protocol v1** — a JSON-RPC contract carried on WebSocket (`/api/ui-protocol/ws`) or stdio (`octos serve --stdio`). It covers session open with cursor replay, streamed turns, durable persistence events, tool activity, approvals, background tasks, and rollback. The protocol spec is the contract: server and clients release independently against it.

- **[octos-web](https://github.com/octos-org/octos-web)** — the browser client: chat, voice/video, studio, slides, and sites. A build is embedded in the server binary at `/app/`, so `octos serve` works with zero extra deploys. (The admin dashboard is a separate SPA, embedded at `/admin/`.)
- **[octos-tui](https://github.com/octos-org/octos-tui)** — the terminal client. Connects to a running server over WebSocket, or spawns `octos serve --stdio` as its own private backend.
- **`octos mcp-serve`** — the inverse direction: octos as an MCP server, callable as a sub-agent from outer orchestrators.

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
