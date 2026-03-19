# Architecture Document: octos

## Overview

octos is a 15-member Rust workspace (Edition 2024, rust-version 1.85.0) providing both a coding agent CLI and a multi-channel messaging gateway. Pure Rust TLS via rustls (no OpenSSL). Error handling via `eyre`/`color-eyre`.

**Workspace members**:
- **6 core crates**: octos-core, octos-memory, octos-llm, octos-agent, octos-bus, octos-cli
- **1 pipeline crate**: octos-pipeline
- **7 app-skill crates**: news, deep-search, deep-crawl, send-email, account-manager, time, weather
- **1 platform-skill crate**: asr

```
┌─────────────────────────────────────────────────────────────┐
│                        octos-cli                             │
│           (CLI: chat, gateway, init, status)                │
├──────────────────────────┬──────────────────────────────────┤
│       octos-agent         │           octos-bus               │
│  (Agent, Tools, Skills)  │  (Channels, Sessions, Cron)     │
├──────────┬───────────────┼──────────────────────────────────┤
│octos-memory│  octos-llm    │       octos-pipeline              │
│(Episodes) │ (Providers)  │  (DOT-based orchestration)      │
├──────────┴───────────────┴──────────────────────────────────┤
│                       octos-core                             │
│            (Types, Messages, Gateway Protocol)              │
└─────────────────────────────────────────────────────────────┘
```

---

## octos-core — Foundation Types

Shared types with no internal dependencies. Only depends on serde, chrono, uuid, eyre.

`MessageRole` implements `as_str() -> &'static str` and `Display` for consistent string conversion across providers (system/user/assistant/tool).

### Task Model

```rust
pub struct Task {
    pub id: TaskId,                   // UUID v7 (temporal ordering)
    pub parent_id: Option<TaskId>,    // For subtasks
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub context: TaskContext,
    pub result: Option<TaskResult>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

**TaskId**: Newtype over `Uuid`. Generates UUID v7 via `Uuid::now_v7()`. Implements Display, FromStr, Default.

**TaskStatus** (tagged enum, `"state"` discriminant):
- `Pending` — awaiting assignment
- `InProgress { agent_id: AgentId }` — executing
- `Blocked { reason: String }` — waiting for dependency
- `Completed` — success
- `Failed { error: String }` — failure with message

**TaskKind** (tagged enum, `"type"` discriminant):
- `Plan { goal: String }`
- `Code { instruction: String, files: Vec<PathBuf> }`
- `Review { diff: String }`
- `Test { command: String }`
- `Custom { name: String, params: serde_json::Value }`

**TaskContext**:
- `working_dir: PathBuf`, `git_state: Option<GitState>`, `working_memory: Vec<Message>`, `episodic_refs: Vec<EpisodeRef>`, `files_in_scope: Vec<PathBuf>`

**TaskResult**:
- `success: bool`, `output: String`, `files_modified: Vec<PathBuf>`, `subtasks: Vec<TaskId>`, `token_usage: TokenUsage`

**TokenUsage**: `input_tokens: u32`, `output_tokens: u32` (defaults to 0/0)

### Message Types

```rust
pub struct Message {
    pub role: MessageRole,           // System | User | Assistant | Tool
    pub content: String,
    pub media: Vec<String>,          // File paths (images, audio)
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub timestamp: DateTime<Utc>,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}
```

### Gateway Protocol

```rust
pub struct InboundMessage {       // channel → agent
    pub channel: String,          // "telegram", "cli", "discord", etc.
    pub sender_id: String,
    pub chat_id: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub media: Vec<String>,
    pub metadata: serde_json::Value,
}

pub struct OutboundMessage {      // agent → channel
    pub channel: String,
    pub chat_id: String,
    pub content: String,
    pub reply_to: Option<String>,
    pub media: Vec<String>,
    pub metadata: serde_json::Value,
}
```

`InboundMessage::session_key()` derives `SessionKey::new(channel, chat_id)` — format `"{channel}:{chat_id}"`.

### Inter-Agent Coordination

```rust
pub enum AgentMessage {           // tagged: "type", snake_case
    TaskAssign { task: Box<Task> },
    TaskUpdate { task_id: TaskId, status: TaskStatus },
    TaskComplete { task_id: TaskId, result: TaskResult },
    ContextRequest { task_id: TaskId, query: String },
    ContextResponse { task_id: TaskId, context: Vec<Message> },
}
```

### Error System

```rust
pub struct Error {
    pub kind: ErrorKind,
    pub context: Option<String>,      // Chained context
    pub suggestion: Option<String>,   // Actionable fix hint
}
```

**ErrorKind variants**: TaskNotFound, AgentNotFound, InvalidStateTransition, LlmError, ApiError (status-aware: 401→check key, 429→rate limit), ToolError, ConfigError, ApiKeyNotSet, UnknownProvider, Timeout, ChannelError, SessionError, IoError, SerializationError, Other(eyre::Report).

### Utilities

`truncate_utf8(s: &mut String, max_len: usize, suffix: &str)` — in-place truncation at UTF-8 char boundaries. Appends suffix after truncation. Used across all tool outputs.

---

## octos-llm — LLM Provider Abstraction

### Provider Trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Result<ChatResponse>;
    async fn chat_stream(&self, messages: &[Message], tools: &[ToolSpec], config: &ChatConfig) -> Result<ChatStream>;  // default: falls back to chat()
    fn context_window(&self) -> u32;  // default: context_window_tokens(self.model_id())
    fn model_id(&self) -> &str;
    fn provider_name(&self) -> &str;
}
```

### Configuration

```rust
pub struct ChatConfig {
    pub max_tokens: Option<u32>,        // default: Some(4096)
    pub temperature: Option<f32>,       // default: Some(0.0)
    pub tool_choice: ToolChoice,        // Auto | Required | None | Specific { name }
    pub stop_sequences: Vec<String>,
}
```

### Response Types

```rust
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,       // EndTurn | ToolUse | MaxTokens | StopSequence
    pub usage: TokenUsage,
}

pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta { index, id, name, arguments_delta },
    Usage(TokenUsage),
    Done(StopReason),
    Error(String),
}

pub type ChatStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;
```

### Provider Registry (`registry/`)

All providers are defined in `octos-llm/src/registry/` — one file per provider. Each file exports a `ProviderEntry` with metadata (name, aliases, default model, API key env var, base URL) and a `create()` factory function. Adding a new provider = one file + one line in `mod.rs`.

```rust
pub struct ProviderEntry {
    pub name: &'static str,              // canonical name
    pub aliases: &'static [&'static str], // e.g. ["google"] for gemini
    pub default_model: Option<&'static str>,
    pub api_key_env: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub requires_api_key: bool,
    pub requires_base_url: bool,          // true for vllm
    pub requires_model: bool,             // true for vllm
    pub detect_patterns: &'static [&'static str], // model→provider auto-detect
    pub create: fn(CreateParams) -> Result<Arc<dyn LlmProvider>>,
}

pub struct CreateParams {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub model_hints: Option<ModelHints>,  // config-level override
}
```

**Lookup**: `registry::lookup(name)` — case-insensitive, matches canonical name or aliases.
**Auto-detect**: `registry::detect_provider(model)` — infers provider from model name patterns.

### Native Providers (4 protocol implementations)

| Provider | Base URL | Auth Header | Image Format | Default Model |
|----------|----------|-------------|--------------|---------------|
| Anthropic | api.anthropic.com | x-api-key | Base64 blocks | claude-sonnet-4-20250514 |
| OpenAI | api.openai.com/v1 | Authorization: Bearer | Data URI | gpt-4o |
| Gemini | generativelanguage.googleapis.com/v1beta | x-goog-api-key | Base64 inline | gemini-2.5-flash |
| OpenRouter | openrouter.ai/api/v1 | Authorization: Bearer | Data URI | anthropic/claude-sonnet-4-20250514 |

### OpenAI-Compatible Providers (via `OpenAIProvider::with_base_url()`)

| Provider | Aliases | Base URL | Default Model | API Key Env |
|----------|---------|----------|---------------|-------------|
| DeepSeek | — | api.deepseek.com/v1 | deepseek-chat | DEEPSEEK_API_KEY |
| Groq | — | api.groq.com/openai/v1 | llama-3.3-70b-versatile | GROQ_API_KEY |
| Moonshot | kimi | api.moonshot.ai/v1 | kimi-k2.5 | MOONSHOT_API_KEY |
| DashScope | qwen | dashscope.aliyuncs.com/compatible-mode/v1 | qwen-max | DASHSCOPE_API_KEY |
| MiniMax | — | api.minimax.io/v1 | MiniMax-Text-01 | MINIMAX_API_KEY |
| Zhipu | glm | open.bigmodel.cn/api/paas/v4 | glm-4-plus | ZHIPU_API_KEY |
| Nvidia | nim | integrate.api.nvidia.com/v1 | meta/llama-3.3-70b-instruct | NVIDIA_API_KEY |
| Ollama | — | localhost:11434/v1 | llama3.2 | (none) |
| vLLM | — | (user-provided) | (user-provided) | VLLM_API_KEY |

### Anthropic-Compatible Provider

| Provider | Aliases | Base URL | Default Model | API Key Env |
|----------|---------|----------|---------------|-------------|
| Z.AI | zai, z.ai | api.z.ai/api/anthropic | glm-5 | ZAI_API_KEY |

### ModelHints (OpenAI provider)

Auto-detected from model name at construction, overridable via config `model_hints`:

```rust
pub struct ModelHints {
    pub uses_completion_tokens: bool,  // o-series, gpt-5, gpt-4.1
    pub fixed_temperature: bool,       // o-series, kimi-k2.5
    pub lacks_vision: bool,            // deepseek, minimax, mistral, yi-
    pub merge_system_messages: bool,   // default: true
}
```

### SSE Streaming

`parse_sse_response(response) -> impl Stream<Item = SseEvent>` — stateful unfold-based parser. Max buffer: 1 MB. Handles `\n\n` and `\r\n\r\n` separators. Each provider maps SSE events to `StreamEvent`:

- **Anthropic**: `message_start` → input tokens, `content_block_start/delta` → text/tool chunks, `message_delta` → stop reason. Custom SSE state machine.
- **OpenAI/OpenRouter**: Standard OpenAI SSE with `[DONE]` sentinel. `delta.content` for text, `delta.tool_calls[]` for tools. Shared parser: `parse_openai_sse_events()`.
- **Gemini**: `alt=sse` endpoint. `candidates[0].content.parts[]` with function call data.

### RetryProvider

Wraps any `Arc<dyn LlmProvider>` with exponential backoff. Wrapped by `ProviderChain` for multi-provider failover.

```rust
pub struct RetryConfig {
    pub max_retries: u32,           // default: 3
    pub initial_delay: Duration,    // default: 1s
    pub max_delay: Duration,        // default: 60s
    pub backoff_multiplier: f64,    // default: 2.0
}
```

**Delay formula**: `initial_delay * backoff_multiplier^attempt`, capped at max_delay.

**Retryable errors** (three-tier detection):
1. HTTP status: 429, 500, 502, 503, 504, 529
2. reqwest: `is_connect()` or `is_timeout()`
3. String fallback: "connection refused", "timed out", "overloaded"

### Provider Failover Chain

`ProviderChain` wraps multiple `Arc<dyn LlmProvider>` and transparently fails over on retriable errors. Configured via `fallback_models` in config.

```rust
pub struct ProviderChain {
    slots: Vec<ProviderSlot>,       // provider + AtomicU32 failure count
    failure_threshold: u32,         // default: 3
}
```

**Behavior**: Tries providers in order, skipping degraded ones (failures >= threshold). On retriable error, moves to the next. On success, resets failure count. If all degraded, picks the one with fewest failures.

**Retryable**: Same criteria as RetryProvider (429, 5xx, connect/timeout errors).

### AdaptiveRouter (`adaptive.rs`)

Metrics-driven provider selection. Tracks per-provider EMA latency, p95 latency, error rates. Includes circuit breaker with probe requests to recover failed providers.

### SwappableProvider (`swappable.rs`)

Runtime model switching via `RwLock`. Allows changing the underlying provider without restarting the agent.

### ProviderRouter (`router.rs`)

Sub-agent multi-model routing. Routes different sub-agent tasks to different providers/models.

### OminixClient (`ominix.rs`)

Client for local ASR/TTS via Ominix runtime.

### Token Estimation

```rust
pub fn estimate_tokens(text: &str) -> u32  // ~4 chars/token ASCII, ~1.5 chars/token CJK
pub fn estimate_message_tokens(msg: &Message) -> u32  // content + tool_calls + 4 overhead
```

### Context Windows

| Model Family | Tokens |
|---|---|
| Claude 3/4 | 200,000 |
| GPT-4o/4-turbo | 128,000 |
| o1/o3/o4 | 200,000 |
| Gemini 2.0/1.5 | 1,000,000 |
| Default (unknown) | 128,000 |

### Pricing

`model_pricing(model_id) -> Option<ModelPricing>` — case-insensitive substring match. Cost = `(input/1M) * input_rate + (output/1M) * output_rate`.

| Model | Input $/1M | Output $/1M |
|---|---|---|
| claude-opus-4 | 15.00 | 75.00 |
| claude-sonnet-4 | 3.00 | 15.00 |
| claude-haiku | 0.80 | 4.00 |
| gpt-4o | 2.50 | 10.00 |
| gpt-4o-mini | 0.15 | 0.60 |
| o3/o4 | 10.00 | 40.00 |

### Embedding

```rust
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}
```

**OpenAIEmbedder**: Default model `text-embedding-3-small` (1536 dims). `text-embedding-3-large` = 3072 dims.

### Transcription

**GroqTranscriber**: Whisper `whisper-large-v3` via `https://api.groq.com/openai/v1/audio/transcriptions`. Multipart form. 60s timeout. MIME detection: ogg/opus→audio/ogg, mp3→audio/mpeg, m4a→audio/mp4, wav→audio/wav.

### Vision

`encode_image(path) -> (mime_type, base64_data)` — JPEG/PNG/GIF/WebP. `is_image(path) -> bool`.

### Typed Error Hierarchy (`error.rs`)

`LlmError` with `LlmErrorKind` enum: Authentication, RateLimited, ContextOverflow, ModelNotFound, ServerError, Network, Timeout, InvalidRequest, ContentFiltered, StreamError, Provider. `is_retryable()` returns true for RateLimited, ServerError, Network, Timeout, StreamError. `from_status(code, body)` maps HTTP status codes to error kinds. Provider response bodies logged at debug level only (not exposed in error messages).

### High-Level Client (`high_level.rs`)

`LlmClient` wraps `Arc<dyn LlmProvider>` with ergonomic APIs: `generate(prompt)`, `generate_with(messages, tools, config)`, `generate_object(prompt, schema_name, schema)`, `generate_typed<T>(prompt, schema_name, schema)`, `stream(prompt)`, `stream_with(messages, tools, config)`. Configurable via `with_config(ChatConfig)`.

### Middleware Pipeline (`middleware.rs`)

`LlmMiddleware` trait with `before()`/`after()`/`on_error()` hooks. `MiddlewareStack` wraps `LlmProvider` and runs layers in insertion order. `before()` can short-circuit with cached responses. Built-in: `LoggingMiddleware` (tracing), `CostTracker` (AtomicU64 counters for input/output tokens and request count). Streaming bypasses middleware (logged as debug warning).

### Model Catalog (`catalog.rs`)

`ModelCatalog` with `ModelInfo` (id, name, provider, context_window, max_output_tokens, capabilities, cost, aliases). Lookup by ID or alias via HashMap index. `with_defaults()` pre-registers 4 models (Claude Sonnet 4, Claude Haiku 4.5, GPT-4o, Gemini 2.5 Flash). `by_provider()` and `with_capability()` for filtered queries.

---

## octos-memory — Persistence & Search

### EpisodeStore

redb database at `.octos/episodes.redb` with three tables:

| Table | Key | Value | Purpose |
|---|---|---|---|
| episodes | &str (episode_id) | &str (JSON) | Full episode records |
| cwd_index | &str (working_dir) | &str (JSON array of IDs) | Directory-scoped lookup |
| embeddings | &str (episode_id) | &[u8] (bincode Vec<f32>) | Vector embeddings |

```rust
pub struct Episode {
    pub id: String,                   // UUID v7
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub working_dir: PathBuf,
    pub summary: String,              // LLM-generated, truncated to 500 chars
    pub outcome: EpisodeOutcome,      // Success | Failure | Blocked | Cancelled
    pub key_decisions: Vec<String>,
    pub files_modified: Vec<PathBuf>,
    pub created_at: DateTime<Utc>,
}
```

**Operations**:
- `store(episode)` — serialize to JSON, update cwd_index, insert into in-memory HybridIndex
- `get(id)` — direct lookup by episode_id
- `find_relevant(cwd, query, limit)` — keyword matching scoped to directory
- `recent_for_cwd(cwd, n)` — N most recent by created_at descending
- `store_embedding(id, Vec<f32>)` — bincode serialize, store in embeddings table, update HybridIndex
- `find_relevant_hybrid(query, query_embedding, limit)` — global hybrid search across all episodes

**Initialization**: On `open()`, rebuilds in-memory HybridIndex by iterating all episodes and loading embeddings from DB.

### MemoryStore

File-based persistent memory at `{data_dir}/memory/`:

- `MEMORY.md` — long-term memory (full overwrite)
- `YYYY-MM-DD.md` — daily notes (append with date header)

**`get_memory_context()`** builds system prompt injection:
1. `## Long-term Memory` — full MEMORY.md
2. `## Recent Activity` — 7-day rolling window of daily notes
3. `## Today's Notes` — current day

### HybridIndex — BM25 + Vector Search

```rust
pub struct HybridIndex {
    inverted: HashMap<String, Vec<(usize, f32)>>,  // term → [(doc_idx, tf)]
    doc_lengths: Vec<usize>,
    avg_dl: f64,
    ids: Vec<String>,
    texts: Vec<String>,
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    has_embedding: Vec<bool>,
    dimension: usize,                               // default: 1536
}
```

**BM25 scoring** (constants: K1=1.2, B=0.75):
- Tokenization: lowercase, split on non-alphanumeric, filter tokens < 2 chars
- IDF: `ln((N - df + 0.5) / (df + 0.5) + 1.0)`
- Score: `IDF * (tf * (K1 + 1)) / (tf + K1 * (1 - B + B * dl/avg_dl))`
- Normalized to [0, 1] range (epsilon `1e-10` prevents NaN from near-zero max scores)

**HNSW vector index** (via `hnsw_rs`):
- Named constants: `HNSW_MAX_NB_CONNECTION=16`, `HNSW_CAPACITY=10_000`, `HNSW_EF_CONSTRUCTION=200`, `HNSW_MAX_LAYER=16`, `DistCosine`
- L2 normalization before insertion/search; zero vectors rejected (returns `None`)
- Cosine similarity = `1 - distance` (DistCosine returns 1-cos_sim)

**Hybrid ranking** — fetches `limit * 4` candidates from each:
- Configurable weights via `with_weights(vector_weight, bm25_weight)` (defaults: 0.7 / 0.3)
- Without vectors: BM25 only (graceful fallback)

---

## octos-agent — Agent Runtime

### Agent Core

```rust
pub struct Agent {
    id: AgentId,
    llm: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    memory: Arc<EpisodeStore>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    system_prompt: RwLock<String>,
    config: AgentConfig,
    reporter: Arc<dyn ProgressReporter>,
    shutdown: Arc<AtomicBool>,       // Acquire/Release ordering
}

pub struct AgentConfig {
    pub max_iterations: u32,          // default: 50 (CLI overrides to 20)
    pub max_tokens: Option<u32>,      // None = unlimited
    pub max_timeout: Option<Duration>,// default: 600s wall-clock timeout
    pub save_episodes: bool,          // default: true
}
```

### Execution Loop (`run_task` / `process_message`)

```
1. Build messages: system prompt + history + memory context + input
2. Loop (up to max_iterations):
   a. Check shutdown flag and token budget
   b. trim_to_context_window() — compact if needed
   c. Call LLM via chat_stream()
   d. Consume stream → accumulate text, tool_calls, tokens
   e. Match stop_reason:
      - EndTurn/StopSequence → save episode, return result
      - ToolUse → execute_tools() → append results → continue
      - MaxTokens → return result
```

**ConversationResponse**: `content: String`, `token_usage: TokenUsage`, `files_modified: Vec<PathBuf>`, `streamed: bool`

**Episode saving**: After task completion, fires-and-forgets embedding generation if embedder present.

**Wall-clock timeout**: Agent aborts after `max_timeout` (default 600s) regardless of iteration count.

### Tool Output Sanitization

Before feeding tool results back to the LLM, `sanitize_tool_output()` (in `sanitize.rs`) strips noise:
- **Base64 data URIs**: `data:...;base64,<payload>` → `[base64-data-redacted]`
- **Long hex strings**: 64+ contiguous hex chars (SHA-256, raw keys) → `[hex-redacted]`

### Context Compaction

Triggered when estimated tokens exceed 80% of context window / 1.2 safety margin.

**Algorithm**:
1. Keep MIN_RECENT_MESSAGES (6) most recent non-system messages
2. Don't split inside tool call/result pairs
3. Summarize old messages: first line (200 chars), strip tool arguments, drop media
4. Budget: 40% of total for summary (BASE_CHUNK_RATIO = 0.4)
5. Replace: `[System, CompactionSummary, Recent1, Recent2, ...]`

**Format**:
- User: `> User: first line [media omitted]`
- Assistant: `> Assistant: content` or `- Called tool_name`
- Tool: `-> tool_name: ok|error - first 100 chars`

### Bundled App Skills (`bundled_app_skills.rs`)

Compile-time embedded app-skill entries. Each app-skill crate (news, deep-search, deep-crawl, etc.) is registered as a bundled skill available at runtime.

### Bootstrap (`bootstrap.rs`)

Bootstraps bundled skills at gateway startup. Ensures all bundled app-skills are registered and available.

### Prompt Guard (`prompt_guard.rs`)

Prompt injection detection. `ThreatKind` enum classifies detected threats. Scans user input before passing to the agent.

### Tool System

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn tags(&self) -> &[&str];
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult>;
}

pub struct ToolResult {
    pub output: String,
    pub success: bool,
    pub file_modified: Option<PathBuf>,
    pub tokens_used: Option<TokenUsage>,
}
```

**ToolRegistry**: `HashMap<String, Arc<dyn Tool>>` with `provider_policy: Option<ToolPolicy>` for soft filtering.

### Built-in Tools (14)

| Tool | Parameters | Key Behavior |
|---|---|---|
| **read_file** | path, start_line?, end_line? | Line numbers (NNN\|), 100KB truncation, symlink rejection |
| **write_file** | path, content | Creates parent dirs, returns file_modified |
| **edit_file** | path, old_string, new_string | Exact match required, error on 0 or >1 occurrences |
| **diff_edit** | path, diff | Unified diff with fuzzy matching (+-3 lines), reverse hunk application |
| **glob** | pattern, limit=100 | Rejects absolute paths and `..`, relative results |
| **grep** | pattern, file_pattern?, limit=50, context=0, ignore_case=false | .gitignore-aware via `ignore::WalkBuilder`, regex with `(?i)` flag |
| **list_dir** | path | Sorted, `[dir]`/`[file]` prefix |
| **shell** | command, timeout_secs=120 | SafePolicy check, 50KB output truncation, sandbox-wrapped, timeout clamped to [1, 600]s |
| **web_search** | query, count=5 | Brave Search API (BRAVE_API_KEY) |
| **web_fetch** | url, extract_mode="markdown", max_chars=50000 | SSRF protection, htmd HTML→markdown, 30s timeout |
| **message** | content, channel?, chat_id? | Cross-channel messaging via OutboundMessage. **Gateway-only** |
| **spawn** | task, label?, mode="background", allowed_tools, context? | Subagent with inherited provider policy. sync=inline, background=async. **Gateway-only** |
| **cron** | action, message, schedule params | Schedule add/list/remove/enable/disable. **Gateway-only** |
| **browser** | action, url?, selector?, text?, expression? | Headless Chrome via CDP (always compiled). Actions: navigate (SSRF + scheme check), get_text, get_html, click, type, screenshot, evaluate, close. 5min idle timeout, env sanitization, 10s JS timeout, early action validation |

**Registration**: Core tools registered in `ToolRegistry::with_builtins()` (all modes). Browser is always compiled. Message, spawn, and cron are registered only in gateway mode (`gateway.rs`).

### Tool Policies

```rust
pub struct ToolPolicy {
    pub allow: Vec<String>,   // empty = allow all
    pub deny: Vec<String>,    // deny-wins
}
```

**Groups**: `group:fs` (read_file, write_file, edit_file, diff_edit), `group:runtime` (shell), `group:web` (web_search, web_fetch, browser), `group:search` (glob, grep, list_dir), `group:sessions` (spawn).

**Wildcards**: `exec*` matches prefix. Provider-specific policies via config `tools.byProvider`.

### Command Policy (ShellTool)

```rust
pub enum Decision { Allow, Deny, Ask }
```

**SafePolicy deny patterns**: `rm -rf /`, `rm -rf /*`, `dd if=`, `mkfs`, `:(){:|:&};:`, `chmod -R 777 /`. Commands are whitespace-normalized before matching to prevent evasion via extra spaces/tabs.

**SafePolicy ask patterns**: `sudo`, `rm -rf`, `git push --force`, `git reset --hard`

### Sandbox

```rust
pub enum SandboxMode { Auto, Bwrap, Macos, Docker, None }
```

**BLOCKED_ENV_VARS** (18 vars, shared across all backends + MCP):
`LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH, DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH, NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB, JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR`

| Backend | Isolation | Network | Path Validation |
|---|---|---|---|
| **Bwrap** (Linux) | RO bind /usr,/lib,/bin,/sbin,/etc; RW bind workdir; tmpfs /tmp; unshare-pid | `--unshare-net` if !allow_network | N/A |
| **Macos** (sandbox-exec) | SBPL profile: process-exec/fork, file-read*, writes to workdir+/private/tmp | `(allow network*)` or `(deny network*)` | Rejects control chars, `(`, `)`, `\`, `"` |
| **Docker** | `--rm --security-opt no-new-privileges --cap-drop ALL` | `--network none` | Rejects `:`, `\0`, `\n`, `\r` |

**Docker resource limits**: `--cpus`, `--memory`, `--pids-limit`. Mount modes: None (/tmp workdir), ReadOnly, ReadWrite.

### Hooks System

Lifecycle hooks run shell commands at agent events. Configured via `hooks` array in config.

```rust
pub enum HookEvent { BeforeToolCall, AfterToolCall, BeforeLlmCall, AfterLlmCall }

pub struct HookConfig {
    pub event: HookEvent,
    pub command: Vec<String>,       // argv array (no shell interpretation)
    pub timeout_ms: u64,            // default: 5000
    pub tool_filter: Vec<String>,   // tool events only; empty = all
}
```

**Shell protocol**: JSON payload on stdin. Exit code semantics: 0=allow, 1=deny (before-hooks only), 2+=error. Before-hooks can deny operations; after-hook exit codes only count as errors.

**Circuit breaker**: `HookExecutor` auto-disables a hook after 3 consecutive failures (configurable via `with_threshold()`). Resets on success.

**Environment**: Commands sanitized via `BLOCKED_ENV_VARS`. Tilde expansion supports `~/` and `~username/`.

**Integration**: Wired into `chat.rs`, `gateway.rs`, `serve.rs`. Hook config changes trigger restart via config watcher.

### MCP Integration

JSON-RPC transport for Model Context Protocol servers. Two transport modes:

**Transports**:
1. **Stdio**: Spawns server as child process (command + args + env). Line limit: 1MB. Env sanitized via `BLOCKED_ENV_VARS`.
2. **HTTP/SSE**: Connects to remote server via `url` field. POST JSON, SSE response handling.

**Lifecycle** (stdio):
1. Spawn server (command + args + env, filtering BLOCKED_ENV_VARS)
2. Initialize: `protocolVersion: "2024-11-05"`
3. Discover tools: `tools/list` RPC
4. Validate input schemas (max depth 10, max size 64KB); reject tools with invalid schemas
5. Register McpTool wrappers (30s timeout, 1MB max response)

**McpTool execution**: `tools/call` with name + arguments. Extracts `content[].text` from response.

### Skills System

Skills are markdown instruction files that extend agent capabilities. Two sources: built-in (compiled into binary) and workspace (user-installed).

#### Skill File Format (SKILL.md)

```yaml
---
name: skill_name
description: What it does
requires_bins: binary1, binary2    # comma-separated, checked via `which`
requires_env: ENV_VAR1, ENV_VAR2   # comma-separated, checked via std::env::var()
always: true|false                 # auto-load into system prompt when available
---
Skill instructions here (markdown). This body is injected into the agent's
system prompt when the skill is activated.
```

**Frontmatter parsing**: Simple `key: value` line matching (not full YAML). `split_frontmatter()` finds content between `---` delimiters. `strip_frontmatter()` returns body only.

#### SkillInfo

```rust
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,          // filesystem path or "(built-in)/name/SKILL.md"
    pub available: bool,        // bins_ok && env_ok
    pub always: bool,           // auto-load into system prompt
    pub builtin: bool,          // true if from BUILTIN_SKILLS, false if workspace
}
```

**Availability check**: `available = requires_bins all found on PATH AND requires_env all set`. Missing requirements make the skill unavailable but still listed.

#### SkillsLoader

```rust
pub struct SkillsLoader {
    skills_dir: PathBuf,        // {data_dir}/skills/
}
```

**Methods**:
- `list_skills()` — scans workspace dir + built-ins. Workspace skills override built-ins with same name (checked via HashSet). Results sorted alphabetically.
- `load_skill(name)` — returns body (frontmatter stripped). Checks workspace first, falls back to built-in.
- `build_skills_summary()` — generates XML for system prompt injection:
  ```xml
  <skills>
    <skill available="true">
      <name>skill_name</name>
      <description>What it does</description>
      <location>/path/to/SKILL.md</location>
    </skill>
  </skills>
  ```
- `get_always_skills()` — filters skills where `always: true` AND `available: true`.
- `load_skills_for_context(names)` — loads multiple skills, joins with `\n---\n`.

#### Built-in Skills (3, compile-time `include_str!()`)

```rust
pub struct BuiltinSkill {
    pub name: &'static str,
    pub content: &'static str,  // full SKILL.md including frontmatter
}
pub const BUILTIN_SKILLS: &[BuiltinSkill] = &[...];
```

| Skill | Purpose |
|---|---|
| cron | Task scheduling instructions |
| skill-store | Skill store browsing and installation |
| skill-creator | Create new skills |
| tmux | Terminal multiplexer control |
| weather | Weather information retrieval |

#### CLI Management (`octos skills`)

- `list` — shows built-in skills (with override status) + workspace skills
- `install <user/repo/skill-name>` — fetches `SKILL.md` from `https://raw.githubusercontent.com/{repo}/main/SKILL.md` (15s timeout), saves to `.octos/skills/{name}/SKILL.md`. Fails if skill already exists.
- `remove <name>` — deletes `.octos/skills/{name}/` directory

#### Integration with Gateway

In the gateway command, skills are loaded during system prompt construction:
1. `get_always_skills()` — collects auto-load skill names
2. `load_skills_for_context(names)` — loads and joins skill bodies
3. `build_skills_summary()` — appends XML skill index to system prompt
4. Always-on skill content is prepended to the system prompt

### Plugin System

Plugins extend the agent with external tools via standalone executables. Each plugin is a directory containing a `manifest.json` and an executable file.

#### Directory Layout

```
.octos/plugins/           # local (project-level)
~/.octos/plugins/         # global (user-level)
  └── my-plugin/
      ├── manifest.json  # plugin metadata + tool definitions
      └── my-plugin      # executable (or "main" as fallback)
```

**Discovery order**: local `.octos/plugins/` first, then global `~/.octos/plugins/`. Both are scanned by `Config::plugin_dirs()`.

#### PluginManifest

```rust
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub tools: Vec<PluginToolDef>,    // default: empty vec
}

pub struct PluginToolDef {
    pub name: String,                 // must be unique across all plugins
    pub description: String,
    pub input_schema: serde_json::Value,  // default: {"type": "object"}
}
```

**Example manifest.json**:
```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "tools": [
    {
      "name": "greet",
      "description": "Greet someone by name",
      "input_schema": {
        "type": "object",
        "properties": { "name": { "type": "string" } }
      }
    }
  ]
}
```

#### PluginLoader

```rust
pub struct PluginLoader;  // stateless, all methods are associated functions
```

**`load_into(registry, dirs)`**:
1. Scan each directory for subdirectories
2. For each subdirectory, look for `manifest.json`
3. Parse manifest, find executable (try directory name first, then `main`)
4. Validate executable permissions (Unix: `mode & 0o111 != 0`; non-Unix: existence check)
5. Wrap each tool definition as a `PluginTool` implementing the `Tool` trait
6. Register into `ToolRegistry`
7. Log warning: `"loaded unverified plugin (no signature check)"`
8. Return total tool count. Failed plugins are skipped with warning, not fatal.

#### PluginTool — Execution Protocol

```rust
pub struct PluginTool {
    plugin_name: String,
    tool_def: PluginToolDef,
    executable: PathBuf,
}
```

**Invocation**: `executable <tool_name>` (tool name passed as first argument).

**stdin/stdout protocol**:
1. Spawn executable with tool name as arg, piped stdin/stdout/stderr
2. Write JSON-serialized arguments to stdin, close (EOF signals end of input)
3. Wait for exit with 30s timeout (`PLUGIN_TIMEOUT`)
4. Parse stdout as JSON:
   - **Structured**: `{"output": "...", "success": true/false}` → use parsed values
   - **Fallback**: raw stdout + stderr concatenated, success from exit code
5. Return `ToolResult` (no `file_modified` tracking for plugins)

**Error handling**:
- Spawn failure → eyre error with plugin name and executable path
- Timeout → eyre error with plugin name, tool name, and duration
- JSON parse failure → graceful fallback to raw output

### Progress Reporting

The agent emits structured events during execution via a trait-based observer pattern. Consumers (CLI, REST API) implement the trait to render progress in their own format.

#### ProgressReporter Trait

```rust
pub trait ProgressReporter: Send + Sync {
    fn report(&self, event: ProgressEvent);
}
```

Agent holds `reporter: Arc<dyn ProgressReporter>`. Events are fired synchronously during the execution loop (non-blocking — implementations must not block).

#### ProgressEvent Enum

```rust
pub enum ProgressEvent {
    TaskStarted { task_id: String },
    Thinking { iteration: u32 },
    Response { content: String, iteration: u32 },
    ToolStarted { name: String, tool_id: String },
    ToolCompleted { name: String, tool_id: String, success: bool,
                    output_preview: String, duration: Duration },
    FileModified { path: String },
    TokenUsage { input_tokens: u32, output_tokens: u32 },
    TaskCompleted { success: bool, iterations: u32, duration: Duration },
    TaskInterrupted { iterations: u32 },
    MaxIterationsReached { limit: u32 },
    TokenBudgetExceeded { used: u32, limit: u32 },
    StreamChunk { text: String, iteration: u32 },
    StreamDone { iteration: u32 },
    CostUpdate { session_input_tokens: u32, session_output_tokens: u32,
                 response_cost: Option<f64>, session_cost: Option<f64> },
}
```

#### Implementations (3)

**SilentReporter** — no-op, used as default when no reporter is configured.

**ConsoleReporter** — CLI output with ANSI colors and streaming support:

```rust
pub struct ConsoleReporter {
    use_colors: bool,
    verbose: bool,
    stdout: Mutex<BufWriter<Stdout>>,  // buffered for streaming chunks
}
```

| Event | Output |
|---|---|
| Thinking | `\r⟳ Thinking... (iteration N)` (overwrites line, yellow) |
| Response | `◆ first 3 lines...` (cyan, clears Thinking line) |
| ToolStarted | `\r⚙ Running tool_name...` (overwrites line, yellow) |
| ToolCompleted | `✓ tool_name (duration)` green or `✗ tool_name` red; verbose: 5 lines of output + `...` |
| FileModified | `📝 Modified: path` (green) |
| TokenUsage | `Tokens: N in, N out` (verbose only, dim) |
| TaskCompleted | `✓ Completed N iterations, Xs` or `✗ Failed after N iterations` |
| TaskInterrupted | `⚠ Interrupted after N iterations.` (yellow) |
| MaxIterationsReached | `⚠ Reached max iterations limit (N).` (yellow) |
| TokenBudgetExceeded | `⚠ Token budget exceeded (used, limit).` (yellow) |
| StreamChunk | Write to buffered stdout; flush only on `\n` (reduces syscalls) |
| StreamDone | Flush + newline |
| CostUpdate | `Tokens: N in / N out \| Cost: $X.XXXX` |
| TaskStarted | `▶ Task: id` (verbose only, dim) |

**Duration formatting**: >1s → `{:.1}s`, ≤1s → `{N}ms`.

**SseBroadcaster** (REST API, feature: `api`) — converts events to JSON and broadcasts via `tokio::sync::broadcast` channel:

```rust
pub struct SseBroadcaster {
    tx: broadcast::Sender<String>,  // JSON-serialized events
}
```

| ProgressEvent | JSON `type` field | Additional fields |
|---|---|---|
| ToolStarted | `"tool_start"` | `tool` |
| ToolCompleted | `"tool_end"` | `tool`, `success` |
| StreamChunk | `"token"` | `text` |
| StreamDone | `"stream_end"` | — |
| CostUpdate | `"cost_update"` | `input_tokens`, `output_tokens`, `session_cost` |
| Thinking | `"thinking"` | `iteration` |
| Response | `"response"` | `iteration` |
| (other) | `"other"` | — (logged at debug level) |

Subscribers receive events via `SseBroadcaster::subscribe() -> broadcast::Receiver<String>`. Send errors (no subscribers) are silently ignored.

### Execution Environments (`exec_env.rs`)

`ExecEnvironment` trait with `exec(cmd, args, env)`, `read_file(path)`, `write_file(path, content)`, `file_exists(path)`, `list_dir(path)`. Two implementations: `LocalEnvironment` (tokio::process::Command) and `DockerEnvironment` (docker exec). Environment variables sanitized via shared `BLOCKED_ENV_VARS`. Docker paths validated against injection characters (`\0`, `\n`, `\r`, `:`). Docker env vars forwarded via `--env` flags.

### Provider Toolsets (`provider_tools.rs`)

`ToolAdjustment` (prefer, demote, aliases, extras) per LLM provider. `ProviderToolsets` registry with `with_defaults()` for openai/anthropic/google. Used to optimize tool presentation per provider (e.g., OpenAI prefers shell/read_file, demotes diff_edit).

### Typed Turns (`turn.rs`)

`Turn` wraps `Message` with `TurnKind` (UserInput, AgentReply, ToolCall, ToolResult, System) and iteration number. `turns_to_messages()` converts back to `Vec<Message>` for LLM calls. Enables semantic analysis of conversation history.

### Event Bus (`event_bus.rs`)

`EventBus` with typed `EventSubscriber` for pub/sub within the agent. Decouples event producers (tool execution, LLM calls) from consumers (logging, metrics, UI updates).

### Loop Detection (`loop_detect.rs`)

Detects repetitive agent behavior (e.g., calling the same tool with same args). Configurable threshold and window. Returns early with diagnostic message when loop detected.

### Session State (`session.rs`)

`SessionState` with `SessionLimits` and `SessionUsage` tracking. `SessionStateHandle` for thread-safe access. Tracks token usage, iteration count, and wall-clock time against configured limits.

### Steering (`steering.rs`)

`SteeringMessage` with `SteeringSender`/`SteeringReceiver` (mpsc channel). Allows external control of agent behavior mid-conversation (e.g., injecting guidance, changing strategy).

### Prompt Layers (`prompt_layer.rs`)

`PromptLayerBuilder` for composing system prompts from multiple sources (base prompt, persona, user context, memory, skills). Layers are concatenated in order with configurable separators.

---

## octos-bus — Gateway Infrastructure

### Message Bus

`create_bus() -> (AgentHandle, BusPublisher)` linked by mpsc channels (capacity 256). AgentHandle receives InboundMessages; BusPublisher dispatches OutboundMessages.

**Queue Modes** (configured via `gateway.queue_mode`):
- `Followup` (default): FIFO — process queued messages one at a time
- `Collect`: Merge queued messages by session, concatenating content before processing

### Channel Trait

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;
    fn is_allowed(&self, sender_id: &str) -> bool;
    async fn stop(&self) -> Result<()>;
}
```

### Channel Implementations

| Channel | Transport | Feature Flag | Auth | Dedup |
|---|---|---|---|---|
| **CLI** | stdin/stdout | (always) | N/A | N/A |
| **Telegram** | teloxide long-poll | `telegram` | Bot token (env) | teloxide built-in |
| **Discord** | serenity gateway | `discord` | Bot token (env) | serenity built-in |
| **Slack** | Socket Mode (tokio-tungstenite) | `slack` | Bot token + App token | message_ts |
| **WhatsApp** | WebSocket bridge (ws://localhost:3001) | `whatsapp` | Baileys bridge | HashSet (10K cap, clear on overflow) |
| **Feishu** | WebSocket (tokio-tungstenite) | `feishu` | App ID + Secret → tenant token (TTL 6000s) | HashSet (10K cap, clear on overflow) |
| **Email** | IMAP poll + SMTP send | `email` | Username/password, rustls TLS | IMAP UNSEEN flag |
| **WeCom** | WeCom/WeChat Work API | `wecom` | Corp ID + Agent Secret | message_id |
| **Twilio** | Twilio SMS/MMS | `twilio` | Account SID + Auth Token | message SID |

**Email specifics**: IMAP `async-imap` with rustls for inbound (poll unseen, mark \Seen). SMTP `lettre` for outbound (port 465=implicit TLS, other=STARTTLS). `mailparse` for RFC822 body extraction. Body truncated via `truncate_utf8(max_body_chars)`.

**Feishu specifics**: Tenant access token with TTL cache (6000s). WebSocket gateway URL from `/callback/ws/endpoint`. Message type detection via `header.event_type == "im.message.receive_v1"`. Supports `oc_*` (chat_id) vs `ou_*` (open_id) routing.

**Markdown to HTML**: `markdown_html.rs` converts Markdown to Telegram-compatible HTML for rich message formatting.

**Media**: `download_media()` helper downloads photos/voice/audio/documents to `.octos/media/`.

**Transcription**: Voice/audio auto-transcribed via GroqTranscriber before agent processing.

### Message Coalescing

Splits oversized messages into channel-safe chunks:

| Channel | Max Chars |
|---|---|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

**Break preference**: paragraph (`\n\n`) > newline (`\n`) > sentence (`. `) > space (` `) > hard cut.

MAX_CHUNKS = 50 (DoS limit). UTF-8 safe boundary detection via `char_indices()`.

### Session Manager

JSONL persistence at `.octos/sessions/{key}.jsonl`.

- **In-memory cache**: LRU with disk sync on write
- **Filenames**: Percent-encoded SessionKey, truncated to 183 chars with `_{hash:016X}` suffix on truncation to prevent collisions
- **File size limit**: 10MB max (`MAX_SESSION_FILE_SIZE`); oversized files skipped on load
- **Crash safety**: Atomic write-then-rename
- **Forking**: `fork()` creates child session with `parent_key` tracking, copies last N messages

### Cron Service

JSON persistence at `.octos/cron.json`.

**Schedule types**:
- `Every { seconds: u64 }` — recurring interval
- `Cron { expr: String }` — cron expression via `cron` crate
- `At { timestamp_ms: i64 }` — one-shot (auto-delete after run)

**CronJob fields**: id (8-char hex from UUIDv7), name, enabled, schedule, payload (message + deliver flag + channel + chat_id), state (next_run_at_ms, run_count), delete_after_run.

### Heartbeat Service

Periodic check of `HEARTBEAT.md` (default: 30 min interval). Sends content to agent if non-empty.

---

## octos-cli — CLI & Configuration

### Commands

| Command | Description |
|---|---|
| `chat` | Interactive multi-turn chat. Readline with history. Exit: exit/quit/:q |
| `gateway` | Persistent multi-channel daemon with session management |
| `init` | Initialize .octos/ with config, templates, directories |
| `status` | Show config, provider, API keys, bootstrap files |
| `auth login/logout/status` | OAuth PKCE (OpenAI), device code, paste-token |
| `cron list/add/remove/enable` | CLI cron job management |
| `channels status/login` | Channel compilation status, WhatsApp bridge setup |
| `skills list/install/remove` | Skill management, GitHub fetch |
| `office` | Office/workspace management |
| `account` | Account management |
| `clean` | Remove .redb files with dry-run support |
| `completions` | Shell completion generation (bash/zsh/fish) |
| `docs` | Generate tool + provider documentation |
| `serve` | REST API server (feature: api) — axum on 127.0.0.1:8080 (`--host` to override) |

### Configuration

Loaded from `.octos/config.json` (local) or `~/.config/octos/config.json` (global). Local takes precedence.

- **`${VAR}` expansion**: Environment variable substitution in string values
- **Versioned config**: Version field with automatic `migrate_config()` framework
- **Provider auto-detect** (`registry::detect_provider(model)`): claude→anthropic, gpt/o1/o3/o4→openai, gemini→gemini, deepseek→deepseek, kimi/moonshot→moonshot, qwen→dashscope, glm→zhipu, llama/mixtral→groq. Patterns defined per-provider in `registry/`.

**API key resolution order**: Auth store (`~/.octos/auth.json`) → environment variable.

### Auth Module

**OAuth PKCE** (OpenAI):
1. Generate 64-char verifier (two UUIDv4 hex)
2. SHA-256 challenge, base64-URL encode (no padding)
3. TCP listener on port 1455
4. Browser → `auth.openai.com` with PKCE + state
5. Callback validates state (CSRF), exchanges code+verifier for tokens

**Device Code Flow** (OpenAI): POST `deviceauth/usercode`, poll `deviceauth/token` every 5s+.

**Paste Token**: Prompt for API key from stdin, store as `auth_method: "paste_token"`.

**AuthStore**: `~/.octos/auth.json` (mode 0600). `{credentials: {provider: AuthCredential}}`.

### Config Watcher

Polls every 5 seconds. SHA-256 hash comparison of file contents.

**Hot-reloadable**: system_prompt, max_history (applied live).

**Restart-required**: provider, model, base_url, api_key_env, sandbox, mcp_servers, hooks, gateway.queue_mode, channels.

### REST API (feature: `api`)

| Route | Method | Description |
|---|---|---|
| `/api/chat` | POST | Send message → response |
| `/api/chat/stream` | GET | SSE stream of ProgressEvents |
| `/api/sessions` | GET | List all sessions |
| `/api/sessions/{id}/messages` | GET | Paginated history (?limit=100&offset=0, max 500) |
| `/api/status` | GET | Version, model, provider, uptime |
| `/metrics` | GET | Prometheus text exposition format (unauthenticated) |
| `/*` (fallback) | GET | Embedded web UI (static files via rust-embed) |

**Auth**: Optional bearer token with constant-time comparison (API routes only; `/metrics` and static files are public). **CORS**: localhost:3000/8080. **Max message**: 1MB.

**Web UI**: Embedded SPA via `rust-embed` served as the fallback handler. Session sidebar, chat interface, SSE streaming, dark theme. Vanilla HTML/CSS/JS (no build tools).

**Prometheus Metrics**: `octos_tool_calls_total` (counter, labels: tool, success), `octos_tool_call_duration_seconds` (histogram, label: tool), `octos_llm_tokens_total` (counter, label: direction). Powered by `metrics` + `metrics-exporter-prometheus` crates.

### Session Compaction (Gateway)

Triggered when message count > 40 (threshold). Keeps 10 recent messages. Summarizes older messages via LLM to <500 words. Rewrites JSONL session file.

---

## octos-pipeline — DOT-based Pipeline Orchestration

DOT-based pipeline orchestration engine for defining and executing multi-step workflows.

- `parser.rs` — DOT graph parser (parses Graphviz DOT format into pipeline definitions)
- `graph.rs` — PipelineGraph with node/edge types
- `executor.rs` — Async pipeline execution engine
- `handler.rs` — Handler types: CodergenHandler, GateHandler, ShellHandler, NoopHandler, DynamicParallel
- `condition.rs` — Conditional edge evaluation (branching logic)
- `tool.rs` — RunPipelineTool integration (exposes pipeline execution as an agent tool)
- `validate.rs` — Graph validation and lint diagnostics
- `human_gate.rs` — Human-in-the-loop gates with `HumanInputProvider` trait, `ChannelInputProvider` (mpsc + oneshot, 5min default timeout), `AutoApproveProvider`. Input types: Approval, FreeText, Choice
- `fidelity.rs` — `FidelityMode` enum (Full, Truncate, Compact, Summary) for context carryover control between nodes. Parse from config strings. Safety caps: 10MB max_chars, 100K max_lines
- `manager.rs` — `PipelineManager` supervisor with `SupervisionStrategy` (AllOrNothing, BestEffort, RetryFailed). Retry capped at 10 with exponential backoff (100ms-5s). `ManagerOutcome` converts to `NodeOutcome`
- `thread.rs` — `ThreadRegistry` for LLM session reuse across pipeline nodes. `Thread` stores model_id + message history. Limits: 1000 threads, 10000 messages per thread
- `server.rs` — `PipelineServer` trait with `SubmitRequest` (validated: 1MB DOT, 256KB input, 64 variables, safe pipeline IDs), `RunStatus` lifecycle (Queued → Running → Completed/Failed/Cancelled)
- `artifact.rs` — Pipeline artifact storage for intermediate outputs
- `checkpoint.rs` — Pipeline checkpoint/resume for crash recovery
- `events.rs` — Pipeline event system for progress tracking
- `run_dir.rs` — Per-run working directories with isolation
- `stylesheet.rs` — Visual styling for pipeline graph rendering

---

## Data Flows

### Chat Mode

```
User Input → readline → Agent.process_message(input, history)
                              │
                              ├─ Build messages (system + history + memory + input)
                              ├─ trim_to_context_window() if needed
                              ├─ Call LLM via chat_stream() with tool specs
                              ├─ Execute tools if ToolUse (loop)
                              └─ Return ConversationResponse
                                    │
                              Print response, append to history
```

### Gateway Mode

```
Channel → InboundMessage → MessageBus → [transcribe audio] → [load session]
                                              │
                                    Agent.process_message()
                                              │
                                        OutboundMessage
                                              │
                                   ChannelManager.dispatch()
                                              │
                                    coalesce() → Channel.send()
```

System messages (cron, heartbeat, spawn results) flow through the same bus with `channel: "system"` and metadata routing.

---

## Feature Flags

```toml
# octos-bus
telegram = ["teloxide"]
discord  = ["serenity"]
slack    = ["tokio-tungstenite"]
whatsapp = ["tokio-tungstenite"]
feishu   = ["tokio-tungstenite"]
email    = ["async-imap", "tokio-rustls", "rustls", "webpki-roots", "lettre", "mailparse"]

# octos-agent (browser is always compiled in, no longer feature-gated)
git      = ["gix"]                  # git operations via gitoxide
ast      = ["tree-sitter"]          # code_structure.rs AST analysis
admin-bot = [...]                   # admin/ directory tools

# octos-bus (additional)
wecom    = [...]                    # WeCom/WeChat Work channel
twilio   = [...]                    # Twilio SMS/MMS channel

# octos-cli
api      = ["axum", "tower-http", "futures"]
telegram = ["octos-bus/telegram"]
discord  = ["octos-bus/discord"]
slack    = ["octos-bus/slack"]
whatsapp = ["octos-bus/whatsapp"]
feishu   = ["octos-bus/feishu"]
email    = ["octos-bus/email"]
wecom    = ["octos-bus/wecom"]
twilio   = ["octos-bus/twilio"]
```

---

## File Layout

```
crates/
├── octos-core/src/
│   ├── lib.rs, task.rs, types.rs, error.rs, gateway.rs, message.rs, utils.rs
├── octos-llm/src/
│   ├── lib.rs, provider.rs, config.rs, types.rs, retry.rs, failover.rs, sse.rs
│   ├── embedding.rs, pricing.rs, context.rs, transcription.rs, vision.rs
│   ├── adaptive.rs, swappable.rs, router.rs, ominix.rs
│   ├── anthropic.rs, openai.rs, gemini.rs, openrouter.rs  (protocol impls)
│   └── registry/ (mod.rs + 14 provider entries: anthropic, openai, gemini,
│                   openrouter, deepseek, groq, moonshot, dashscope, minimax,
│                   zhipu, zai, nvidia, ollama, vllm)
├── octos-memory/src/
│   ├── lib.rs, episode.rs, store.rs, memory_store.rs, hybrid_search.rs
├── octos-agent/src/
│   ├── lib.rs, agent.rs, progress.rs, policy.rs, compaction.rs, sanitize.rs, hooks.rs
│   ├── sandbox.rs, mcp.rs, skills.rs, builtin_skills.rs
│   ├── bundled_app_skills.rs, bootstrap.rs, prompt_guard.rs
│   ├── plugins/ (mod.rs, loader.rs, manifest.rs, tool.rs)
│   ├── skills/ (cron, skill-store, skill-creator SKILL.md)
│   └── tools/ (mod, policy, shell, read_file, write_file, edit_file, diff_edit,
│               list_dir, glob_tool, grep_tool, web_search, web_fetch,
│               message, spawn, browser, ssrf, tool_config,
│               deep_search, site_crawl, recall_memory, save_memory,
│               send_file, take_photo, code_structure, git,
│               deep_research_pipeline, synthesize_research, research_utils,
│               admin/ (profiles, skills, sub_accounts, system,
│                       platform_skills, update))
├── octos-bus/src/
│   ├── lib.rs, bus.rs, channel.rs, session.rs, coalesce.rs, media.rs
│   ├── cli_channel.rs, telegram_channel.rs, discord_channel.rs
│   ├── slack_channel.rs, whatsapp_channel.rs, feishu_channel.rs, email_channel.rs
│   ├── wecom_channel.rs, twilio_channel.rs, markdown_html.rs
│   ├── cron_service.rs, cron_types.rs, heartbeat.rs
└── octos-cli/src/
    ├── main.rs, config.rs, config_watcher.rs, cron_tool.rs, compaction.rs
    ├── auth/ (mod.rs, store.rs, oauth.rs, token.rs)
    ├── api/ (mod.rs, router.rs, handlers.rs, sse.rs, metrics.rs, static_files.rs)
    └── commands/ (mod, chat, init, status, gateway, clean,
                   completions, cron, channels, auth, skills, docs, serve,
                   office, account)
├── octos-pipeline/src/
│   ├── lib.rs, parser.rs, graph.rs, executor.rs, handler.rs
│   ├── condition.rs, tool.rs, validate.rs
```

---

## Security

### Workspace-Level Safety
- `#![deny(unsafe_code)]` — workspace-wide lint via `[workspace.lints.rust]`
- `secrecy::SecretString` — all provider API keys are wrapped; prevents accidental logging/display

### Authentication & Credentials
- API keys: auth store (`~/.octos/auth.json`, mode 0600) checked before env vars
- OAuth PKCE with SHA-256 challenges, state parameter (CSRF protection)
- Constant-time byte comparison for API bearer tokens (timing attack prevention)

### Execution Sandbox
- Three backends: bwrap (Linux), sandbox-exec (macOS), Docker — `SandboxMode::Auto` detection
- 18 BLOCKED_ENV_VARS shared across all sandbox backends, MCP server spawning, and browser tool
- Path injection prevention per backend (Docker: `:`, `\0`, `\n`, `\r`; macOS: control chars, `(`, `)`, `\`, `"`)
- Docker: `--cap-drop ALL`, `--security-opt no-new-privileges`, `--network none`

### Tool Safety
- ShellTool SafePolicy: deny `rm -rf /`, `dd`, `mkfs`, fork bombs; ask for `sudo`, `git push --force`. Whitespace-normalized before matching. Timeout clamped to [1, 600]s.
- Tool policies: allow/deny with deny-wins semantics, group support, provider-specific filtering
- Tool argument size limit: 1MB per invocation (non-allocating `estimate_json_size` with escape char accounting)
- Path traversal prevention + symlink-safe file I/O via `O_NOFOLLOW` (Unix) eliminating TOCTOU races
- SSRF protection in shared `ssrf.rs` module: blocks private IPs (10/8, 172.16/12, 192.168/16, 169.254/16, IPv6 ULA/link-local, IPv4-mapped/compatible). Used by web_fetch and browser.
- Browser: URL scheme allowlist (http/https only), 10s JS execution timeout, zombie process reaping, secure tempfiles for screenshots
- MCP: input schema validation (max depth 10, max size 64KB) prevents malicious tool definitions

### Data Safety
- Tool output sanitization: strips base64 data URIs and long hex strings (`sanitize.rs`)
- UTF-8 safe truncation via `truncate_utf8()` across all tool outputs and email bodies
- Session file collision prevention via percent-encoded filenames with hash suffix on truncation
- Session file size limit: 10MB max prevents OOM on corrupted files
- Atomic write-then-rename for session persistence (crash safety)
- API server binds to 127.0.0.1 by default (not 0.0.0.0)
- Channel access control via `allowed_senders` lists
- MCP response limit: 1MB per JSON-RPC line (DoS prevention)
- Message coalescing: MAX_CHUNKS=50 DoS limit
- API message limit: 1MB per request

---

## Concurrency Model

### Why Rust

octos uses Rust with the tokio async runtime, which provides significant advantages over Python (OpenClaw, etc.) and Node.js (NanoCloud, etc.) agent frameworks for concurrent session handling:

**True parallelism** — Tokio tasks run across all CPU cores simultaneously. Python has the GIL, so even with asyncio, CPU-bound work (JSON parsing, context compaction, token counting) is single-core. Node.js is single-threaded entirely. In octos, 10 concurrent sessions doing context compaction actually execute in parallel across cores.

**Memory efficiency** — No garbage collector, no runtime overhead per object. Agent sessions are compact structs on the heap. A Python agent session carries interpreter overhead, GC metadata on every object, and dict-based attribute lookup. This matters with hundreds of sessions and large conversation histories in memory.

**No GC pauses** — Python and Node.js GC can cause latency spikes mid-response. Rust has deterministic deallocation — memory is freed exactly when the owning struct drops.

**Single binary deployment** — No Python/Node runtime to install, no dependency hell, predictable resource usage. The gateway is one static binary.

### Tokio Tasks vs OS Threads

All concurrent session processing uses tokio tasks (green threads), not OS threads. A tokio task is a state machine on the heap (~few KB). An OS thread is ~8MB stack. Thousands of tasks multiplex across a handful of OS threads (defaults to CPU core count). Since agent sessions spend most of their time awaiting I/O (LLM API responses), they yield the thread to other tasks efficiently.

### Gateway Concurrency

```
Inbound messages → main loop
                      │
                      ├─ tokio::spawn() per message
                      │     │
                      │     ├─ Semaphore (max_concurrent_sessions, default 10)
                      │     │     bounds total concurrent agent runs
                      │     │
                      │     └─ Per-session Mutex
                      │           serializes messages within same session
                      │
                      └─ Different sessions run concurrently
                         Same session queues sequentially
```

- **Cross-session**: concurrent, bounded by `max_concurrent_sessions` semaphore (default 10)
- **Within same session**: serialized via per-session mutex — prevents race conditions on conversation history
- **Per-session locks**: pruned after completion (Arc strong_count == 1) to prevent unbounded HashMap growth

### Tool Execution

Within a single agent iteration, all tool calls from one LLM response execute concurrently via `join_all()`:

```
LLM response: [web_search, read_file, send_email]
                   │            │           │
                   └────────────┼───────────┘
                          join_all()
                   ┌────────────┼───────────┐
                   │            │           │
                 done         done        done
                          ↓
              All results appended to messages
                          ↓
                    Next LLM call
```

### Sub-Agent Modes (spawn tool)

| Aspect | Sync | Background |
|--------|------|------------|
| Parent blocks? | Yes | No (`tokio::spawn()`) |
| Result delivery | Same conversation turn | New inbound message via gateway |
| Token accounting | Counted toward parent budget | Independent |
| Use case | Sequential pipelines | Fire-and-forget long tasks |

Sub-agents cannot spawn further sub-agents (spawn tool is always denied in sub-agent policy).

### Multi-Tenant Dashboard

The dashboard (`octos serve`) runs each user profile as a **separate gateway OS process**:

```
Dashboard (octos serve)
  ├─ Profile "alice" → octos gateway --config alice.json  (deepseek, own semaphore)
  ├─ Profile "bob"   → octos gateway --config bob.json    (kimi, own semaphore)
  └─ Profile "carol" → octos gateway --config carol.json  (openai, own semaphore)
```

Each profile has its own LLM provider, API keys, channels, data directory, and `max_concurrent_sessions` semaphore. Profiles are fully isolated — no shared state between gateway processes.

---

## Testing

1300+ tests across all crates. See [TESTING.md](./TESTING.md) for the full inventory and CI guide.

- **Unit**: type serde round-trips, tool arg parsing, config validation, provider detection, tool policies, compaction, coalescing, BM25 scoring, L2 normalization, SSE parsing
- **Adaptive routing**: Off/Hedge/Lane modes, circuit breaker, failover, scoring, metrics, provider racing (19 tests)
- **Responsiveness**: baseline learning, degradation detection, recovery, threshold boundaries (8 tests)
- **Queue modes**: Followup, Collect, Steer, Speculative overflow, auto-escalation/deescalation (9 tests)
- **Session persistence**: JSONL storage, LRU eviction, fork, rewrite, timestamp sort, concurrent access (28 tests)
- **Integration**: CLI commands, file tools, cron jobs, session forking, plugin loading
- **Security**: sandbox path injection, env sanitization, SSRF blocking, symlink rejection (O_NOFOLLOW), private IP detection, dedup overflow, tool argument size limits, session file size limits, circuit breaker threshold edge cases, MCP schema validation
- **Channel**: allowed_senders, message parsing, dedup logic, email address extraction

Local CI: `./scripts/ci.sh` (mirrors GitHub Actions + focused subsystem tests). See [TESTING.md](./TESTING.md).
