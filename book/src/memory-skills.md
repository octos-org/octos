# Memory & Skills

Octos has a layered memory system and an extensible skill framework. Memory gives the agent persistent context across sessions. Skills give the agent new tools and capabilities.

## Bootstrap Files

These files are loaded into the system prompt at startup. Create them with `octos init`.

| File | Purpose |
|------|---------|
| `.octos/AGENTS.md` | Agent instructions and guidelines |
| `.octos/SOUL.md` | Personality and values |
| `.octos/USER.md` | User information and preferences |
| `.octos/TOOLS.md` | Tool-specific guidance |
| `.octos/IDENTITY.md` | Custom identity definition |

Bootstrap files are hot-reloaded -- edit them and the agent picks up changes without a restart.

## Memory System

Octos uses a 3-layer memory architecture that combines automatic recording with agent-driven knowledge management:

```
┌──────────────────────────────────────────────────────────────────┐
│                     System Prompt (every turn)                    │
│                                                                   │
│  1. Episodic Memory  ─── top 6 relevant past task experiences    │
│  2. Memory Context   ─── MEMORY.md + recent 7 days daily notes   │
│  3. Entity Bank      ─── one-line abstracts of all known entities │
│                                                                   │
│  Tools: save_memory / recall_memory  (entity bank CRUD)           │
└──────────────────────────────────────────────────────────────────┘
```

### Layer 1: Episodic Memory (automatic)

Every completed task is automatically recorded as an **episode** in `episodes.redb`, a persistent embedded database. Each episode stores:

- **Summary** — LLM-generated, truncated to 500 chars
- **Outcome** — Success, Failure, Blocked, or Cancelled
- **Files modified** — list of file paths touched during the task
- **Key decisions** — notable choices made during execution
- **Working directory** — scope for directory-scoped retrieval

At the start of each new task, the agent queries the episode store for up to **6 relevant past experiences** using:

- **Hybrid search** (default when embedding is configured): combines BM25 keyword matching (30% weight) with HNSW vector similarity (70% weight)
- **Keyword search** (fallback when no embedder): matches query terms against episode summaries, scoped to the same working directory

**Embedding configuration** (in `config.json`):

```json
{
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null
  }
}
```

When configured, the agent embeds each episode summary in a fire-and-forget background task and stores the vector alongside the episode. At query time, the task instruction is embedded and used for vector search. When omitted, the system falls back to BM25-only keyword matching.

### Layer 2: Long-Term Memory & Daily Notes (file-based)

**Long-term memory** (`.octos/memory/MEMORY.md`) holds persistent facts and notes that survive across all sessions. Edit this file manually or via the `write_file` tool — it is injected verbatim into the system prompt on every turn.

**Daily notes** (`.octos/memory/YYYY-MM-DD.md`) provide a rolling window of recent activity. The last **7 days** of daily notes are automatically included in the agent's context. These files can be created manually or via the `write_file` tool.

> **Note:** Daily notes are read by the system prompt builder but are not auto-populated. You can populate them manually or instruct the agent to write to them using `write_file`.

### Layer 3: Entity Bank (tool-driven)

The entity bank is a structured knowledge store at `.octos/memory/bank/entities/`. Each entity is a markdown file containing everything the agent knows about a specific topic.

**How it works:**

1. **Abstracts in prompt** — The first non-heading line of each entity becomes a one-line abstract. All abstracts are injected into the system prompt, giving the agent a compact index of everything it knows.
2. **Full pages on demand** — The agent uses the `recall_memory` tool to load the full content of a specific entity when it needs more detail.
3. **Agent-managed** — The agent decides when to create and update entities using the `save_memory` tool.

**Memory tools:**

- **`save_memory`** — Create or update an entity page. The agent is instructed to first `recall_memory` for existing content, then merge new information before saving (no data loss).
- **`recall_memory`** — Load the full content of a named entity. If the entity doesn't exist, returns a list of all available entities.

> **Auto-deferral:** When the total tool count exceeds 15, memory tools are moved to the `group:memory` deferred group. The agent must use `activate_tools` to enable them before saving or recalling.

## File Layout

```
.octos/
├── config.json              # Configuration (versioned, auto-migrated)
├── cron.json                # Cron job store
├── AGENTS.md                # Agent instructions
├── SOUL.md                  # Personality
├── USER.md                  # User info
├── HEARTBEAT.md             # Background tasks
├── sessions/                # Chat history (JSONL)
├── memory/                  # Memory files
│   ├── MEMORY.md            # Long-term memory (manual or write_file)
│   ├── 2025-02-10.md        # Daily note (manual or write_file)
│   └── bank/
│       └── entities/        # Entity bank (managed by save/recall tools)
│           ├── yuechen.md   # Entity: "who is the user"
│           └── octos.md     # Entity: "what is this project"
├── skills/                  # Custom skills
├── episodes.redb            # Episodic memory DB (auto-populated)
└── history/
    └── chat_history         # Readline history
```

---

## Built-in System Skills

Octos bundles 3 system skills at compile time:

| Skill | Description |
|-------|-------------|
| `cron` | Cron tool usage examples (always-on) |
| `skill-store` | Skill installation and management |
| `skill-creator` | Guide for creating custom skills |

Workspace skills in `.octos/skills/` override built-in skills with the same name.

## Bundled App Skills

Eight app skills ship as compiled binaries alongside Octos. They are automatically bootstrapped into `.octos/skills/` on gateway startup -- no installation required.

### News Fetch

**Tool:** `news_fetch` | **Always active:** Yes

Fetches headlines and full article content from Google News RSS, Hacker News API, Yahoo News, Substack, and Medium. The agent synthesizes raw data into a formatted digest.

**Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `categories` | array | all | News categories to fetch |
| `language` | `"zh"` / `"en"` | `"zh"` | Output language |

Categories: `politics`, `world`, `business`, `technology`, `science`, `entertainment`, `health`, `sports`

**Configuration:**

```
/config set news_digest.language en
/config set news_digest.hn_top_stories 50
/config set news_digest.max_deep_fetch_total 30
```

### Deep Search

**Tool:** `deep_search` | **Timeout:** 600 seconds

Multi-round web research tool. Performs iterative searches, parallel page crawling, reference chasing, and generates structured reports saved to `./research/<query-slug>/`.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `query` | string | *(required)* | Research topic or question |
| `depth` | 1--3 | 2 | Research depth level |
| `max_results` | 1--10 | 8 | Results per search round |
| `search_engine` | string | auto | `perplexity`, `duckduckgo`, `brave`, `you` |

**Depth levels:**

- **1 (Quick):** single search round, ~1 minute, up to 10 pages
- **2 (Standard):** 3 search rounds + reference chasing, ~3 minutes, up to 30 pages
- **3 (Thorough):** 5 search rounds + aggressive link chasing, ~5 minutes, up to 50 pages

### Deep Crawl

**Tool:** `deep_crawl` | **Requires:** Chrome/Chromium in PATH

Recursively crawls a website using headless Chrome via CDP. Renders JavaScript, follows same-origin links via BFS, extracts clean text.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `url` | string | *(required)* | Starting URL |
| `max_depth` | 1--10 | 3 | Maximum link-following depth |
| `max_pages` | 1--200 | 50 | Maximum pages to crawl |
| `path_prefix` | string | none | Only follow links under this path |

Output is saved to `crawl-<hostname>/` with numbered markdown files.

**Configuration:**

```
/config set deep_crawl.page_settle_ms 5000
/config set deep_crawl.max_output_chars 100000
```

### Send Email

**Tool:** `send_email`

Sends emails via SMTP or Feishu/Lark Mail API (auto-detected from available environment variables).

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `to` | string | *(required)* | Recipient email address |
| `subject` | string | *(required)* | Email subject |
| `body` | string | *(required)* | Email body (plain text or HTML) |
| `html` | boolean | false | Treat body as HTML |
| `attachments` | array | none | File attachments (SMTP only) |

**SMTP environment variables:**

```bash
export SMTP_HOST="smtp.gmail.com"
export SMTP_PORT="465"
export SMTP_USERNAME="your-email@gmail.com"
export SMTP_PASSWORD="your-app-password"
export SMTP_FROM="your-email@gmail.com"
```

### Weather

**Tools:** `get_weather`, `get_forecast` | **API:** Open-Meteo (free, no key required)

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `city` | string | *(required)* | City name in English |
| `days` | 1--16 | 7 | Forecast days (forecast only) |

### Clock

**Tool:** `get_time`

Returns current date, time, day of week, and UTC offset for any IANA timezone.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `timezone` | string | server local | IANA timezone name (e.g., `Asia/Shanghai`, `US/Eastern`) |

### Account Manager

**Tool:** `manage_account`

Manages sub-accounts under the current profile. Actions: `list`, `create`, `update`, `delete`, `info`, `start`, `stop`, `restart`.

---

## Platform Skills (ASR/TTS)

Platform skills provide on-device voice transcription and synthesis. They require the OminiX backend running on Apple Silicon (M1/M2/M3/M4).

### Voice Transcription

**Tool:** `voice_transcribe`

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `audio_path` | string | *(required)* | Path to audio file (WAV, OGG, MP3, FLAC, M4A) |
| `language` | string | `"Chinese"` | `"Chinese"`, `"English"`, `"Japanese"`, `"Korean"`, `"Cantonese"` |

### Voice Synthesis

**Tool:** `voice_synthesize`

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `text` | string | *(required)* | Text to synthesize |
| `output_path` | string | auto | Output file path |
| `language` | string | `"chinese"` | `"chinese"`, `"english"`, `"japanese"`, `"korean"` |
| `speaker` | string | `"vivian"` | Voice preset |

**Available voices:** `vivian`, `serena`, `ryan`, `aiden`, `eric`, `dylan` (EN/ZH), `uncle_fu` (ZH only), `ono_anna` (JA), `sohee` (KO)

### Voice Cloning

**Tool:** `voice_clone_synthesize`

Synthesizes speech using a cloned voice from a 3--10 second reference audio sample.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `text` | string | *(required)* | Text to synthesize |
| `reference_audio` | string | *(required)* | Path to reference audio |
| `language` | string | `"chinese"` | Target language |

### Podcast Generation

**Tool:** `generate_podcast`

Creates multi-speaker podcast audio from a script of `{speaker, voice, text}` objects.

---

## Custom Skill Installation

### Installing from GitHub

```bash
# Install all skills from a repo
octos skills install user/repo

# Install a specific skill
octos skills install user/repo/skill-name

# Install from a specific branch
octos skills install user/repo --branch develop

# Force overwrite existing
octos skills install user/repo --force

# Install into a specific profile
octos skills install user/repo --profile my-bot
```

The installer tries to download a pre-built binary from the skill registry (SHA-256 verified), falls back to `cargo build --release` if a `Cargo.toml` is present, or runs `npm install` if a `package.json` is present.

### Managing Skills

```bash
octos skills list                    # List installed skills
octos skills info skill-name         # Show detailed info
octos skills update skill-name       # Update a specific skill
octos skills update all              # Update all skills
octos skills remove skill-name       # Remove a skill
octos skills search "web scraping"   # Search the online registry
```

### Skill Resolution Order

Skills are loaded from these directories (highest priority first):

1. `.octos/plugins/` (legacy)
2. `.octos/skills/` (user-installed custom skills)
3. `.octos/bundled-app-skills/` (bundled app skills)
4. `.octos/platform-skills/` (platform: ASR/TTS)
5. `~/.octos/plugins/` (global legacy)
6. `~/.octos/skills/` (global custom)

User-installed skills override bundled skills with the same name.

---

## Skill Authoring

A custom skill lives in `.octos/skills/<name>/` and contains:

```
.octos/skills/my-skill/
├── SKILL.md         # Required: instructions + frontmatter
├── manifest.json    # Required for tool skills: tool definitions
├── main             # Compiled binary (or script)
└── .source          # Auto-generated: tracks install source
```

### SKILL.md Format

```markdown
---
name: my-skill
version: 1.0.0
author: Your Name
description: A brief description of what this skill does
always: false
requires_bins: curl,jq
requires_env: MY_API_KEY
---

# My Skill Instructions

Instructions for the agent on how and when to use this skill.

## When to Use
- Use this skill when the user asks about...

## Tool Usage
The `my_tool` tool accepts:
- `query` (required): The search query
- `limit` (optional): Maximum results (default: 10)
```

**Frontmatter fields:**

| Field | Description |
|-------|-------------|
| `name` | Skill identifier (must match directory name) |
| `version` | Semantic version |
| `author` | Skill author |
| `description` | Short description |
| `always` | If `true`, included in every system prompt. If `false`, available on demand. |
| `requires_bins` | Comma-separated binaries checked via `which`. Skill is unavailable if any are missing. |
| `requires_env` | Comma-separated environment variables. Skill is unavailable if any are unset. |

### manifest.json Format

For skills that provide executable tools:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "description": "My custom skill",
  "tools": [
    {
      "name": "my_tool",
      "description": "Does something useful",
      "timeout_secs": 60,
      "input_schema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "The search query"
          },
          "limit": {
            "type": "integer",
            "description": "Maximum results",
            "default": 10
          }
        },
        "required": ["query"]
      }
    }
  ],
  "entrypoint": "main"
}
```

The tool binary receives JSON input on stdin and must output JSON on stdout:

```json
// Input (stdin)
{"query": "test", "limit": 5}

// Output (stdout)
{"output": "Results here...", "success": true}
```
