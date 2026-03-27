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
octos serve                              # Binds to 127.0.0.1:8080
octos serve --host 0.0.0.0 --port 3000  # Accept external connections
```

Features: session sidebar, chat interface, SSE streaming, dark theme. A `/metrics` endpoint provides Prometheus-format metrics (`octos_tool_calls_total`, `octos_tool_call_duration_seconds`, `octos_llm_tokens_total`).

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

Office file manipulation (DOCX/PPTX/XLSX). Native Rust implementation with no external dependencies for basic operations.

```bash
octos office extract <file>               # Extract text as Markdown
octos office unpack <file> <output-dir>   # Unpack into pretty-printed XML
octos office pack <input-dir> <output>    # Pack directory into Office file
octos office clean <dir>                  # Remove orphaned files from unpacked PPTX
```

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
