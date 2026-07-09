# CLI Reference

## `octos chat`

Interactive multi-turn conversation with readline history.

```
octos chat [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    LLM provider
      --model <NAME>       Model name
      --base-url <URL>     Custom API endpoint
  -m, --message <MSG>      Single message (non-interactive)
      --max-iterations <N> Max tool iterations per message (default: 50)
  -v, --verbose            Show tool outputs
      --no-retry           Disable retry
```

**Features:**

- Arrow keys and line editing (rustyline)
- Persistent history at `.octos/history/chat_history`
- Exit: `/exit`, `/quit`, `exit`, `quit`, `:q`, Ctrl+C, Ctrl+D
- Full tool access (shell, files, search, web)

**Examples:**

```bash
octos chat                              # Interactive (default)
octos chat --provider deepseek          # Use DeepSeek
octos chat --model glm-4-plus           # Auto-detects Zhipu
octos chat --message "Fix auth bug"     # Single message, exit
```

---

## `octos gateway`

Run as a persistent multi-channel daemon.

```
octos gateway [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    Override provider
      --model <NAME>       Override model
      --base-url <URL>     Override API endpoint
  -v, --verbose            Verbose logging
      --no-retry           Disable retry
```

Requires a `gateway` section in config with a `channels` array. Runs continuously until Ctrl+C.

---

## `octos init`

Initialize workspace with config and bootstrap files.

```
octos init [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
      --defaults       Skip prompts, use defaults
```

**Creates:**

- `.octos/config.json` -- Provider/model config
- `.octos/.gitignore` -- Ignores state files
- `.octos/AGENTS.md` -- Agent instructions template
- `.octos/SOUL.md` -- Personality template
- `.octos/USER.md` -- User info template
- `.octos/memory/` -- Memory storage directory
- `.octos/sessions/` -- Session history directory
- `.octos/skills/` -- Custom skills directory

---

## `octos status`

Show system status.

```
octos status [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
```

**Example output:**

```
octos Status
══════════════════════════════════════════════════

Config:    .octos/config.json (found)
Workspace: .octos/            (found)
Provider:  anthropic
Model:     claude-sonnet-4-20250514

API Keys
──────────────────────────────────────────────────
  Anthropic    ANTHROPIC_API_KEY         set
  OpenAI       OPENAI_API_KEY           not set
  ...

Bootstrap Files
──────────────────────────────────────────────────
  AGENTS.md        found
  SOUL.md          found
  USER.md          found
  TOOLS.md         missing
  IDENTITY.md      missing
```

---

## `octos serve`

Launch the web UI and REST API server. Requires the `api` feature flag.

```bash
cargo install --path crates/octos-cli --features api
octos serve                               # Binds to 127.0.0.1:50080
octos serve --host 0.0.0.0 --port 50080   # Accept external connections
octos serve --solo                        # Enable local no-password "solo" login
octos serve --stdio                       # AppUI JSON-RPC over stdin/stdout (no HTTP bind)
```

Key options:

| Flag | Description |
|------|-------------|
| `--port <N>` | Port to listen on (default `50080`, in IANA's dynamic range) |
| `--host <ADDR>` | Bind address (default `127.0.0.1`; use `0.0.0.0` for external) |
| `--stdio` | Run the AppUI JSON-RPC protocol over stdin/stdout instead of HTTP |
| `--solo` | Enable the loopback-only no-password solo login (`POST /api/auth/solo*`); also `OCTOS_SOLO_LOGIN=1`. Never enable behind a reverse proxy |
| `--data-dir <P>` | Data directory for episodes/memory/sessions (default `$OCTOS_HOME` or `~/.octos`) |
| `--auth-token <T>` | Admin bearer token for API access |
| `--config <P>` | Config file path |
| `--swarm-backend <stdio\|http>` | Enable the `/api/swarm/*` contract-authoring endpoints (pairs with `--swarm-backend-cmd` / `--swarm-backend-url`) |

Serves the embedded SPAs at `/app/` (chat/studio) and `/admin/` (operator dashboard) plus the WS UI Protocol at `/api/ui-protocol/ws`. A `/metrics` endpoint provides Prometheus-format metrics (`octos_tool_calls_total`, `octos_tool_call_duration_seconds`, `octos_llm_tokens_total`). Multiple instances can run in parallel with distinct `--data-dir` + `--port`.

---

## `octos clean`

Clean database and state files.

```bash
octos clean [--all] [--dry-run]
```

| Flag | Description |
|------|-------------|
| `--all` | Remove all state files |
| `--dry-run` | Show what would be removed without deleting |

---

## `octos completions`

Generate shell completions.

```bash
octos completions <shell>
```

Supported shells: `bash`, `zsh`, `fish`, `powershell`.

---

## `octos cron`

Manage scheduled jobs.

```bash
octos cron list [--all]                  # List active jobs (--all includes disabled)
octos cron add [OPTIONS]                 # Add a cron job
octos cron remove <job-id>               # Remove a cron job
octos cron enable <job-id>               # Enable a cron job
octos cron enable <job-id> --disable     # Disable a cron job
```

**Adding jobs:**

```bash
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
```

Cron expressions use standard syntax. Jobs support an optional `timezone` field with IANA timezone names (e.g., `"America/New_York"`, `"Asia/Shanghai"`). When omitted, UTC is used.

When Matrix is fronted by a BotFather-style management bot, the same cron runtime is also available through natural-language chat commands:

```text
/schedule 20秒之后提醒我看天气
/schedule 每天早上 9 点提醒我看天气
/schedules
/unschedule <job-id>
```

These commands create, list, and remove jobs scoped to the current Matrix room or DM instead of exposing raw `cron` syntax to end users.

---

## `octos channels`

Manage messaging channels.

```bash
octos channels status    # Show channel compile/config status
octos channels login     # WhatsApp QR code login
```

The status command shows a table with channel name, compile status (feature flags), and config summary (env vars set/missing).

---

## `octos office`

Office file manipulation (DOCX/PPTX/XLSX). Native Rust implementation with no external dependencies for the core operations; a few subcommands optionally shell out to LibreOffice (`soffice`) when installed.

```bash
# Core (pure Rust)
octos office extract <file>                     # Extract text as Markdown
octos office unpack <file> <output-dir>         # Unpack into pretty-printed XML
octos office pack <input-dir> <output>          # Pack directory into Office file
octos office clean <dir>                        # Remove orphaned files from unpacked PPTX
octos office validate <file>                    # Validate an Office file's structure
octos office make-slide <image> -o <pptx>       # Compose a slide (bg image + --texts JSON overlays) into a .pptx
octos office add-slide <unpacked-dir> <source>  # Add a slide to an unpacked PPTX (dup slideN.xml or apply slideLayoutN.xml)
octos office overlay-text <image> <text>        # Burn text onto a PNG/JPEG (--x/--y position)
octos office comment <unpacked-dir> <id> <text> # Add a comment to an unpacked DOCX

# LibreOffice-backed (require `soffice` on PATH)
octos office accept-changes <input> <output>    # Accept tracked changes (DOCX) → clean copy
octos office recalc <file>                      # Recalculate XLSX formulas
octos office thumbnail <file> [OPTIONS]         # Render slide/page thumbnails (also needs Poppler's pdftoppm)
octos office soffice <args...>                  # Passthrough to a sandboxed soffice
```

`make-slide` composes a rendered background image plus JSON text overlays into a `.pptx` slide (used by the slides pipeline). `comment` inserts its text into the DOCX XML verbatim, so pass **pre-escaped** XML (`&amp;`, `&lt;`, …). Office is **CLI-only** — it is not exposed as an agent tool. Run `octos office <subcommand> --help` for the exact arguments.

---

## `octos account`

Manage sub-accounts under profiles. Sub-accounts inherit LLM provider config but have their own data directory (memory, sessions, skills) and channels.

```bash
octos account list --profile <id>                         # List sub-accounts
octos account create --profile <id> <name> [OPTIONS]      # Create sub-account
octos account update <id> [OPTIONS]                       # Update sub-account
```

---

## `octos auth`

OAuth login and API key management.

```bash
octos auth login --provider openai           # PKCE browser OAuth
octos auth login --provider openai --device-code  # Device code flow
octos auth login --provider anthropic        # Paste-token (stdin)
octos auth logout --provider openai          # Remove stored credential
octos auth status                            # Show authenticated providers
```

Credentials are stored in `~/.octos/auth.json` (file mode 0600). The auth store is checked before environment variables when resolving API keys.

---

## `octos skills`

Manage skills.

```bash
octos skills list                            # List installed skills
octos skills install user/repo/skill-name    # Install from GitHub
octos skills remove skill-name               # Remove a skill
```

Fetches `SKILL.md` from the GitHub repo's main branch and installs to `.octos/skills/`.

---

## `octos doctor`

Run local environment diagnostics for the octos server and print a health report.

```bash
octos doctor [OPTIONS]

Options:
      --json          Emit a machine-readable JSON support bundle
      --verbose       Add resolved paths / versions to each line
      --strict        Promote warnings to failures (affects exit code)
      --data-dir <P>  Data dir override (defaults to ~/.octos)
```

Checks the installed binary's location (and PATH shadowing), the terminal (terminfo), config/data-dir writability, the UI-protocol version skew, and `api.github.com` reachability for update checks. (It does **not** validate provider API keys — use `octos status` for those.) Exit code is non-zero when a check fails (or, with `--strict`, when any check warns). Use `--json` to attach the bundle to a bug report.

---

## `octos docs`

Generate reference documentation for the built-in tools and providers.

```bash
octos docs [--output <DIR>]
```

With no `--output` the Markdown is written to stdout; otherwise it creates `<DIR>` and writes `<DIR>/TOOLS.md`. The output documents the built-in tools plus a provider list that is currently **hard-coded** in the command, so it can lag the actual provider registry.

---

## `octos memory`

Inspect and drive the memory-refresh pipeline (see [Memory & Skills](./memory-skills.md)).

```bash
octos memory refresh [--data-dir <P>]          # Run one extraction pass now
octos memory status  [--data-dir <P>]          # Sweep state: lock holder, backlog, budgets
octos memory remember "<text>" [--data-dir <P>] # Host-authored remember (no model in the loop)
octos memory forget  "<text>" [--sensitive]     # Free-text forget (starts a confirm flow)
octos memory forget  --id ^m4k2abq              # Hard-delete an exact MEMORY.md entry
```

`refresh` works even when the background sweep is disabled in config, but refuses when a running service holds the profile lock. `remember`/`forget` only write a **local staging note** (no LLM at write time); the note is applied on the next consolidation pass — the background sweep or `octos memory refresh` — which *does* send it to the consolidation model. `--sensitive` interim-archives candidates immediately and scrubs them everywhere on confirmation.

---

## `octos update`

Check for a newer octos release.

```bash
octos update --check         # Print the update plan; exit 10 if an update is available, 0 if up to date
octos update --check --json  # Same, machine-readable
```

This is the Stage-2 **check-only** command: it detects the installer lineage (Homebrew, cargo, cargo-dist receipt, …) and prints the exact per-installer upgrade command. Applying updates in-place is Stage 3 and is **not wired yet** — run the printed command to upgrade.

---

## `octos mcp-serve`

Expose octos itself as an MCP server so an outer orchestrator can invoke it as a sub-agent.

```bash
octos mcp-serve [OPTIONS]

Options:
      --transport <stdio|http>  Transport to bind (default: stdio)
      --bind <ADDR>             Bind address for the HTTP transport (default: 127.0.0.1:4033)
  -c, --cwd <PATH>              Working directory
```

`stdio` uses parent-trust auth (JSON-RPC over stdin/stdout). `http` is a minimal HTTP/1.1 JSON-RPC endpoint and **requires** a bearer token via the `OCTOS_MCP_SERVER_TOKEN` environment variable.

---

## `octos admin`

Tenant and tunnel management for the hosted/fleet deployment (frps reverse-tunnel onboarding). Most single-user installs never need this.

```bash
octos admin create-tenant --name <id> [OPTIONS]   # Assign subdomain, auth token, SSH/serve ports
octos admin list-tenants                          # List registered tunnel tenants
octos admin delete-tenant <id>                    # Remove a tenant
octos admin show-tenant-config <id>               # Print the frpc config for a tenant
octos admin reset-token                           # Reset the admin token (restores bootstrap auth)
octos admin set-smtp-password                     # Write smtp_secret.json (0600) for OTP email
octos admin operator-summary [--base-url <URL>] [--auth-token <TOK>]  # Condensed runtime observability view
```

`create-tenant` defaults the base domain to `octos-cloud.org` and the local serve port to `50080` (matching `octos serve`). `reset-token` and `set-smtp-password` operate on the local `--data-dir`; `operator-summary` queries a running API.
