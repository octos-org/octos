# 架构文档：octos

## 概述

octos 是一个包含 15 个成员的 Rust 工作区（Edition 2024，rust-version 1.85.0），提供编码 Agent CLI 和多频道消息网关。通过 rustls 实现纯 Rust TLS（无 OpenSSL 依赖）。错误处理使用 `eyre`/`color-eyre`。

**工作区成员**：
- **6 个核心 crate**：octos-core、octos-memory、octos-llm、octos-agent、octos-bus、octos-cli
- **1 个流水线 crate**：octos-pipeline
- **7 个应用技能 crate**：news、deep-search、deep-crawl、send-email、account-manager、time、weather
- **1 个平台技能 crate**：asr

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

## octos-core — 基础类型

无内部依赖的共享类型。仅依赖 serde、chrono、uuid、eyre。

`MessageRole` 实现了 `as_str() -> &'static str` 和 `Display`，用于跨提供商的一致字符串转换（system/user/assistant/tool）。

### 任务模型

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

**TaskId**：基于 `Uuid` 的新类型。通过 `Uuid::now_v7()` 生成 UUID v7。实现 Display、FromStr、Default。

**TaskStatus**（标记枚举，`"state"` 判别器）：
- `Pending` — 等待分配
- `InProgress { agent_id: AgentId }` — 执行中
- `Blocked { reason: String }` — 等待依赖
- `Completed` — 成功
- `Failed { error: String }` — 失败（附带消息）

**TaskKind**（标记枚举，`"type"` 判别器）：
- `Plan { goal: String }`
- `Code { instruction: String, files: Vec<PathBuf> }`
- `Review { diff: String }`
- `Test { command: String }`
- `Custom { name: String, params: serde_json::Value }`

**TaskContext**：
- `working_dir: PathBuf`、`git_state: Option<GitState>`、`working_memory: Vec<Message>`、`episodic_refs: Vec<EpisodeRef>`、`files_in_scope: Vec<PathBuf>`

**TaskResult**：
- `success: bool`、`output: String`、`files_modified: Vec<PathBuf>`、`subtasks: Vec<TaskId>`、`token_usage: TokenUsage`

**TokenUsage**：`input_tokens: u32`、`output_tokens: u32`（默认 0/0）

### 消息类型

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

### 网关协议

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

`InboundMessage::session_key()` 派生 `SessionKey::new(channel, chat_id)` — 格式为 `"{channel}:{chat_id}"`。

### Agent 间协调

```rust
pub enum AgentMessage {           // tagged: "type", snake_case
    TaskAssign { task: Box<Task> },
    TaskUpdate { task_id: TaskId, status: TaskStatus },
    TaskComplete { task_id: TaskId, result: TaskResult },
    ContextRequest { task_id: TaskId, query: String },
    ContextResponse { task_id: TaskId, context: Vec<Message> },
}
```

### 错误系统

```rust
pub struct Error {
    pub kind: ErrorKind,
    pub context: Option<String>,      // Chained context
    pub suggestion: Option<String>,   // Actionable fix hint
}
```

**ErrorKind 变体**：TaskNotFound、AgentNotFound、InvalidStateTransition、LlmError、ApiError（状态码感知：401→检查密钥，429→限流）、ToolError、ConfigError、ApiKeyNotSet、UnknownProvider、Timeout、ChannelError、SessionError、IoError、SerializationError、Other(eyre::Report)。

### 工具函数

`truncate_utf8(s: &mut String, max_len: usize, suffix: &str)` — 在 UTF-8 字符边界处原地截断。截断后追加后缀。用于所有工具输出。

---

## octos-llm — LLM 提供商抽象

### 提供商 Trait

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

### 配置

```rust
pub struct ChatConfig {
    pub max_tokens: Option<u32>,        // default: Some(4096)
    pub temperature: Option<f32>,       // default: Some(0.0)
    pub tool_choice: ToolChoice,        // Auto | Required | None | Specific { name }
    pub stop_sequences: Vec<String>,
}
```

### 响应类型

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

### 提供商注册表（`registry/`）

所有提供商定义在 `octos-llm/src/registry/` 中 — 每个提供商一个文件。每个文件导出一个 `ProviderEntry`，包含元数据（名称、别名、默认模型、API 密钥环境变量、基础 URL）和 `create()` 工厂函数。添加新提供商 = 一个文件 + `mod.rs` 中一行代码。

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

**查找**：`registry::lookup(name)` — 不区分大小写，匹配规范名称或别名。
**自动检测**：`registry::detect_provider(model)` — 从模型名称模式推断提供商。

### 原生提供商（4 种协议实现）

| 提供商 | 基础 URL | 认证头 | 图片格式 | 默认模型 |
|----------|----------|-------------|--------------|---------------|
| Anthropic | api.anthropic.com | x-api-key | Base64 块 | claude-sonnet-4-20250514 |
| OpenAI | api.openai.com/v1 | Authorization: Bearer | Data URI | gpt-4o |
| Gemini | generativelanguage.googleapis.com/v1beta | x-goog-api-key | Base64 内联 | gemini-2.5-flash |
| OpenRouter | openrouter.ai/api/v1 | Authorization: Bearer | Data URI | anthropic/claude-sonnet-4-20250514 |

### OpenAI 兼容提供商（通过 `OpenAIProvider::with_base_url()`）

| 提供商 | 别名 | 基础 URL | 默认模型 | API 密钥环境变量 |
|----------|---------|----------|---------------|-------------|
| DeepSeek | — | api.deepseek.com/v1 | deepseek-chat | DEEPSEEK_API_KEY |
| Groq | — | api.groq.com/openai/v1 | llama-3.3-70b-versatile | GROQ_API_KEY |
| Moonshot | kimi | api.moonshot.ai/v1 | kimi-k2.5 | MOONSHOT_API_KEY |
| DashScope | qwen | dashscope.aliyuncs.com/compatible-mode/v1 | qwen-max | DASHSCOPE_API_KEY |
| MiniMax | — | api.minimax.io/v1 | MiniMax-Text-01 | MINIMAX_API_KEY |
| Zhipu | glm | open.bigmodel.cn/api/paas/v4 | glm-4-plus | ZHIPU_API_KEY |
| Nvidia | nim | integrate.api.nvidia.com/v1 | meta/llama-3.3-70b-instruct | NVIDIA_API_KEY |
| Ollama | — | localhost:11434/v1 | llama3.2 | （无） |
| vLLM | — | （用户提供） | （用户提供） | VLLM_API_KEY |

### Anthropic 兼容提供商

| 提供商 | 别名 | 基础 URL | 默认模型 | API 密钥环境变量 |
|----------|---------|----------|---------------|-------------|
| Z.AI | zai, z.ai | api.z.ai/api/anthropic | glm-5 | ZAI_API_KEY |

### ModelHints（OpenAI 提供商）

从模型名称在构造时自动检测，可通过配置中的 `model_hints` 覆盖：

```rust
pub struct ModelHints {
    pub uses_completion_tokens: bool,  // o-series, gpt-5, gpt-4.1
    pub fixed_temperature: bool,       // o-series, kimi-k2.5
    pub lacks_vision: bool,            // deepseek, minimax, mistral, yi-
    pub merge_system_messages: bool,   // default: true
}
```

### SSE 流式处理

`parse_sse_response(response) -> impl Stream<Item = SseEvent>` — 基于 unfold 的有状态解析器。最大缓冲区：1 MB。处理 `\n\n` 和 `\r\n\r\n` 分隔符。每个提供商将 SSE 事件映射为 `StreamEvent`：

- **Anthropic**：`message_start` → 输入 token，`content_block_start/delta` → 文本/工具块，`message_delta` → 停止原因。自定义 SSE 状态机。
- **OpenAI/OpenRouter**：标准 OpenAI SSE，`[DONE]` 哨兵。`delta.content` 用于文本，`delta.tool_calls[]` 用于工具。共享解析器：`parse_openai_sse_events()`。
- **Gemini**：`alt=sse` 端点。`candidates[0].content.parts[]`，包含函数调用数据。

### RetryProvider

用指数退避包装任意 `Arc<dyn LlmProvider>`。被 `ProviderChain` 包装以实现多提供商故障转移。

```rust
pub struct RetryConfig {
    pub max_retries: u32,           // default: 3
    pub initial_delay: Duration,    // default: 1s
    pub max_delay: Duration,        // default: 60s
    pub backoff_multiplier: f64,    // default: 2.0
}
```

**延迟公式**：`initial_delay * backoff_multiplier^attempt`，上限为 max_delay。

**可重试错误**（三层检测）：
1. HTTP 状态码：429、500、502、503、504、529
2. reqwest：`is_connect()` 或 `is_timeout()`
3. 字符串兜底："connection refused"、"timed out"、"overloaded"

### 提供商故障转移链

`ProviderChain` 包装多个 `Arc<dyn LlmProvider>`，在可重试错误时透明地故障转移。通过配置中的 `fallback_models` 配置。

```rust
pub struct ProviderChain {
    slots: Vec<ProviderSlot>,       // provider + AtomicU32 failure count
    failure_threshold: u32,         // default: 3
}
```

**行为**：按顺序尝试提供商，跳过已劣化的（失败次数 >= 阈值）。可转移错误时移至下一个。成功时重置失败计数。如果全部劣化，选择失败次数最少的。

**可转移范围**（比可重试更广）：包括 401/403（不应重试同一提供商但应转移到其他提供商）和超时（不应浪费 120s × 重试次数在无响应的提供商上）。

### AdaptiveRouter（`adaptive.rs`）

指标驱动的提供商选择，支持三种互斥模式（Off/Hedge/Lane）。跟踪每个提供商的 EMA 延迟（可配置 `ema_alpha`，默认 0.3）、P95 延迟（64 样本循环缓冲区）、错误率、吞吐量（输出 tokens/sec EMA）和成本。四因子评分：稳定性、质量、优先级、成本（所有权重可配置）。包含熔断器、探测请求、模型目录播种（`model_catalog.json`）和 QoS 排名。评分使用 EMA 混合：冷启动时使用目录基线数据，实时指标逐渐替代（权重在 10 次调用中从 0 渐变到 1）。

```rust
pub struct AdaptiveSlot {
    provider: Arc<dyn LlmProvider>,
    metrics: ProviderMetrics,
    priority: usize,
    cost_per_m: f64,
    model_type: Mutex<ModelType>,        // Strong | Fast
    cost_in: AtomicU64,
    ds_output: AtomicU64,                // 深度搜索输出质量
    baseline_stability: AtomicU64,
    baseline_tool_avg_ms: AtomicU64,
    baseline_p95_ms: AtomicU64,
    context_window: AtomicU64,
    max_output: AtomicU64,
}
```

**Hedge 模式**：通过 `tokio::select!` 竞速主服务商 + 最便宜的备选，取消输家。只有完成的请求记录指标（被取消的输家指标不记录）。如果主服务商失败，备选会顺序重试。

**Lane 模式**：对所有服务商评分，选择最佳的一个。向过期服务商发送探测请求（概率可配置，默认 0.1；间隔默认 60s）。

### FallbackProvider（`fallback.rs`）

包装主服务商 + 按 QoS 排名的备选。失败时通过 `ProviderRouter` 记录冷却。按顺序尝试每个备选。

### SwappableProvider（`swappable.rs`）

通过 `RwLock` 实现运行时模型切换。每次切换泄漏约 50 字节（对于罕见的用户操作可接受）。`cached_model_id` 和 `cached_provider_name` 是泄漏的 `&'static str`，以满足 `&str` 返回类型的要求。

### ProviderRouter（`router.rs`）

子 Agent 多模型路由，支持前缀键解析。支持冷却（默认 60s）、按模型目录评分的 `compatible_fallbacks()`、从 `pricing.rs` 自动推导的费用信息和 LLM 可见的工具模式元数据。

```rust
pub struct ProviderRouter {
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
    active_key: RwLock<Option<String>>,
    metadata: RwLock<HashMap<String, SubProviderMeta>>,
    cooldowns: RwLock<HashMap<String, Instant>>,
    qos_scores: RwLock<HashMap<String, f64>>,
}
```

### OminixClient（`ominix.rs`）

通过 Ominix 运行时访问本地 ASR/TTS 的客户端。

### Token 估算

```rust
pub fn estimate_tokens(text: &str) -> u32  // ~4 chars/token ASCII, ~1.5 chars/token CJK
pub fn estimate_message_tokens(msg: &Message) -> u32  // content + tool_calls + 4 overhead
```

### 上下文窗口

| 模型系列 | Token 数 |
|---|---|
| Claude 3/4 | 200,000 |
| GPT-4o/4-turbo | 128,000 |
| o1/o3/o4 | 200,000 |
| Gemini 2.0/1.5 | 1,000,000 |
| 默认（未知） | 128,000 |

### 定价

`model_pricing(model_id) -> Option<ModelPricing>` — 不区分大小写的子串匹配。费用 = `(input/1M) * input_rate + (output/1M) * output_rate`。

| 模型 | 输入 $/1M | 输出 $/1M |
|---|---|---|
| claude-opus-4 | 15.00 | 75.00 |
| claude-sonnet-4 | 3.00 | 15.00 |
| claude-haiku | 0.80 | 4.00 |
| gpt-4o | 2.50 | 10.00 |
| gpt-4o-mini | 0.15 | 0.60 |
| o3/o4 | 10.00 | 40.00 |

### 嵌入

```rust
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}
```

**OpenAIEmbedder**：默认模型 `text-embedding-3-small`（1536 维）。`text-embedding-3-large` = 3072 维。

### 语音转文字

**GroqTranscriber**：通过 `https://api.groq.com/openai/v1/audio/transcriptions` 使用 Whisper `whisper-large-v3`。Multipart 表单。60 秒超时。MIME 类型检测：ogg/opus→audio/ogg、mp3→audio/mpeg、m4a→audio/mp4、wav→audio/wav。

### 视觉

`encode_image(path) -> (mime_type, base64_data)` — JPEG/PNG/GIF/WebP。`is_image(path) -> bool`。

### 类型化错误层次（`error.rs`）

`LlmError` 包含 `LlmErrorKind` 枚举：Authentication、RateLimited、ContextOverflow、ModelNotFound、ServerError、Network、Timeout、InvalidRequest、ContentFiltered、StreamError、Provider。`is_retryable()` 对 RateLimited、ServerError、Network、Timeout、StreamError 返回 true。`from_status(code, body)` 将 HTTP 状态码映射为错误类型。提供商响应体仅在 debug 级别记录（不暴露在错误消息中）。

### 高级客户端（`high_level.rs`）

`LlmClient` 用友好 API 包装 `Arc<dyn LlmProvider>`：`generate(prompt)`、`generate_with(messages, tools, config)`、`generate_object(prompt, schema_name, schema)`、`generate_typed<T>(prompt, schema_name, schema)`、`stream(prompt)`、`stream_with(messages, tools, config)`。可通过 `with_config(ChatConfig)` 配置。

### 中间件流水线（`middleware.rs`）

`LlmMiddleware` trait 包含 `before()`/`after()`/`on_error()` 钩子。`MiddlewareStack` 包装 `LlmProvider` 并按插入顺序运行各层。`before()` 可通过缓存响应短路。内置：`LoggingMiddleware`（tracing）、`CostTracker`（AtomicU64 计数器，用于输入/输出 token 和请求数）。流式推送绕过中间件（记录为 debug 警告）。

### 模型目录（`catalog.rs`）

`ModelCatalog` 包含 `ModelInfo`（id、name、provider、context_window、max_output_tokens、capabilities、cost、aliases）。通过 HashMap 索引按 ID 或别名查找。`with_defaults()` 预注册 4 个模型（Claude Sonnet 4、Claude Haiku 4.5、GPT-4o、Gemini 2.5 Flash）。`by_provider()` 和 `with_capability()` 用于过滤查询。

---

## octos-memory — 持久化与搜索

### EpisodeStore

redb 数据库位于 `.octos/episodes.redb`，包含三张表：

| 表 | 键 | 值 | 用途 |
|---|---|---|---|
| episodes | &str (episode_id) | &str (JSON) | 完整的片段记录 |
| cwd_index | &str (working_dir) | &str (JSON array of IDs) | 按目录范围的查找 |
| embeddings | &str (episode_id) | &[u8] (bincode Vec<f32>) | 向量嵌入 |

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

**操作**：
- `store(episode)` — 序列化为 JSON，更新 cwd_index，插入内存中的 HybridIndex
- `get(id)` — 按 episode_id 直接查找
- `find_relevant(cwd, query, limit)` — 限定在目录范围内的关键词匹配
- `recent_for_cwd(cwd, n)` — 按 created_at 降序取最近 N 条
- `store_embedding(id, Vec<f32>)` — bincode 序列化，存入 embeddings 表，更新 HybridIndex
- `find_relevant_hybrid(query, query_embedding, limit)` — 跨所有片段的全局混合搜索

**初始化**：`open()` 时通过遍历所有片段并从数据库加载嵌入来重建内存中的 HybridIndex。

### MemoryStore

基于文件的持久化记忆，位于 `{data_dir}/memory/`：

- `MEMORY.md` — 长期记忆（全量覆写）
- `YYYY-MM-DD.md` — 每日笔记（带日期头的追加）

**`get_memory_context()`** 构建系统提示注入：
1. `## Long-term Memory` — 完整的 MEMORY.md
2. `## Recent Activity` — 7 天滚动窗口的每日笔记
3. `## Today's Notes` — 当天内容

### HybridIndex — BM25 + 向量搜索

```rust
pub struct HybridIndex {
    inverted: HashMap<String, Vec<(usize, u32)>>,  // term → [(doc_idx, raw_tf_count)]
    doc_lengths: Vec<usize>,
    total_len: usize,                         // 运行总量，用于 O(1) avg_dl
    avg_dl: f64,
    ids: Vec<String>,
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    has_embedding: Vec<bool>,
    dimension: usize,                               // default: 1536
}
```

**BM25 评分**（常量：K1=1.2, B=0.75）：
- 分词：小写化，按非字母数字字符拆分，过滤长度 < 2 的 token
- IDF：`ln((N - df + 0.5) / (df + 0.5) + 1.0)`
- 评分：`IDF * (tf * (K1 + 1)) / (tf + K1 * (1 - B + B * dl/avg_dl))` — 使用**原始词频计数**（非归一化）
- 去重检测：`ids.contains(episode_id)` 跳过已索引的文档（第 76-78 行）
- 归一化到 [0, 1] 范围（epsilon `1e-10` 防止接近零的最大分数导致 NaN）

**HNSW 向量索引**（通过 `hnsw_rs`）：
- 命名常量：`HNSW_MAX_NB_CONNECTION=16`、`HNSW_CAPACITY=10_000`、`HNSW_EF_CONSTRUCTION=200`、`HNSW_MAX_LAYER=16`、`DistCosine`
- 插入/搜索前进行 L2 归一化；拒绝零向量（返回 `None`）
- 余弦相似度 = `1 - distance`（DistCosine 返回 1-cos_sim）

**混合排名** — 从每种方法获取 `limit * 4` 个候选：
- 可配置权重，通过 `with_weights(vector_weight, bm25_weight)`（默认：0.7 / 0.3）
- 无向量时：仅使用 BM25（优雅降级）

---

## octos-agent — Agent 运行时

### Agent 核心

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

### 执行循环（`run_task` / `process_message`）

```
1. 构建消息：系统提示 + 历史 + 记忆上下文 + 输入
2. 循环（最多 max_iterations 次）：
   a. 检查 shutdown 标志和 token 预算
   b. trim_to_context_window() — 必要时压缩
   c. 通过 chat_stream() 调用 LLM
   d. 消费流 → 累积文本、tool_calls、token
   e. 匹配 stop_reason：
      - EndTurn/StopSequence → 保存片段，返回结果
      - ToolUse → execute_tools() → 追加结果 → 继续
      - MaxTokens → 返回结果
```

**ConversationResponse**：`content: String`、`token_usage: TokenUsage`、`files_modified: Vec<PathBuf>`、`streamed: bool`

**片段保存**：任务完成后，如果有 embedder 则异步触发嵌入生成。

**墙钟超时**：Agent 在 `max_timeout`（默认 600 秒）后终止，不论迭代次数。

### 工具输出清理

在将工具结果反馈给 LLM 之前，`sanitize_tool_output()`（在 `sanitize.rs` 中）剥离噪声：
- **Base64 数据 URI**：`data:...;base64,<payload>` → `[base64-data-redacted]`
- **长十六进制字符串**：64+ 个连续十六进制字符（SHA-256、原始密钥）→ `[hex-redacted]`

### 上下文压缩

当估算的 token 超过上下文窗口的 80% / 1.2 安全系数时触发。

**算法**：
1. 保留最近的 MIN_RECENT_MESSAGES（6）条非系统消息
2. 不在工具调用/结果对内部拆分
3. 摘要旧消息：首行（200 字符），剥离工具参数，丢弃媒体
4. 预算：摘要占总量的 40%（BASE_CHUNK_RATIO = 0.4）
5. 替换为：`[System, CompactionSummary, Recent1, Recent2, ...]`

**格式**：
- User：`> User: first line [media omitted]`
- Assistant：`> Assistant: content` 或 `- Called tool_name`
- Tool：`-> tool_name: ok|error - first 100 chars`

### 内置应用技能（`bundled_app_skills.rs`）

编译时嵌入的应用技能条目。每个应用技能 crate（news、deep-search、deep-crawl 等）注册为运行时可用的内置技能。

### 引导（`bootstrap.rs`）

在网关启动时引导内置技能。确保所有内置应用技能已注册并可用。

### 提示词防护（`prompt_guard.rs`）

提示注入检测。`ThreatKind` 枚举分类检测到的威胁。在传递给 Agent 之前扫描用户输入。

### 工具系统

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

**ToolRegistry**：`HashMap<String, Arc<dyn Tool>>`，带有 `provider_policy: Option<ToolPolicy>` 用于软过滤。

### 内置工具（14 个）

| 工具 | 参数 | 关键行为 |
|---|---|---|
| **read_file** | path, start_line?, end_line? | 行号（NNN\|），100KB 截断，拒绝符号链接 |
| **write_file** | path, content | 创建父目录，返回 file_modified |
| **edit_file** | path, old_string, new_string | 要求精确匹配，0 或 >1 次出现报错 |
| **diff_edit** | path, diff | 统一 diff 格式，模糊匹配（+-3 行），反向 hunk 应用 |
| **glob** | pattern, limit=100 | 拒绝绝对路径和 `..`，相对结果 |
| **grep** | pattern, file_pattern?, limit=50, context=0, ignore_case=false | 通过 `ignore::WalkBuilder` 感知 .gitignore，正则带 `(?i)` 标志 |
| **list_dir** | path | 排序，`[dir]`/`[file]` 前缀 |
| **shell** | command, timeout_secs=120 | SafePolicy 检查，50KB 输出截断，沙箱包装，超时钳制到 [1, 600] 秒 |
| **web_search** | query, count=5 | Brave Search API (BRAVE_API_KEY) |
| **web_fetch** | url, extract_mode="markdown", max_chars=50000 | SSRF 防护，htmd HTML→markdown，30 秒超时 |
| **message** | content, channel?, chat_id? | 通过 OutboundMessage 跨频道消息。**仅网关模式** |
| **spawn** | task, label?, mode="background", allowed_tools, context? | 继承提供商策略的子 Agent。sync=内联，background=异步。**仅网关模式** |
| **cron** | action, message, schedule params | 调度 add/list/remove/enable/disable。**仅网关模式** |
| **browser** | action, url?, selector?, text?, expression? | 通过 CDP 的无头 Chrome（始终编译）。操作：navigate（SSRF + scheme 检查）、get_text、get_html、click、type、screenshot、evaluate、close。5 分钟空闲超时，环境清理，10 秒 JS 超时，提前操作验证 |

**注册**：核心工具在 `ToolRegistry::with_builtins()` 中注册（所有模式）。Browser 始终编译。Message、spawn 和 cron 仅在网关模式注册（`gateway.rs`）。

### 工具策略

```rust
pub struct ToolPolicy {
    pub allow: Vec<String>,   // empty = allow all
    pub deny: Vec<String>,    // deny-wins
}
```

**分组**：`group:fs`（read_file、write_file、edit_file、diff_edit）、`group:runtime`（shell）、`group:web`（web_search、web_fetch、browser）、`group:search`（glob、grep、list_dir）、`group:sessions`（spawn）。

**通配符**：`exec*` 匹配前缀。按提供商的策略通过配置 `tools.byProvider`。

### 命令策略（ShellTool）

```rust
pub enum Decision { Allow, Deny, Ask }
```

**SafePolicy 拒绝模式**：`rm -rf /`、`rm -rf /*`、`dd if=`、`mkfs`、`:(){:|:&};:`、`chmod -R 777 /`。匹配前对命令进行空白归一化，防止通过额外空格/制表符绕过。

**SafePolicy 询问模式**：`sudo`、`rm -rf`、`git push --force`、`git reset --hard`

### 沙箱

```rust
pub enum SandboxMode { Auto, Bwrap, Macos, Docker, None }
```

**BLOCKED_ENV_VARS**（18 个变量，所有后端 + MCP 共享）：
`LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH, DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH, NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB, JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR`

| 后端 | 隔离 | 网络 | 路径验证 |
|---|---|---|---|
| **Bwrap**（Linux） | 只读绑定 /usr,/lib,/bin,/sbin,/etc；读写绑定工作目录；tmpfs /tmp；unshare-pid | 如果 !allow_network 则 `--unshare-net` | 无 |
| **Macos**（sandbox-exec） | SBPL 配置：process-exec/fork、file-read*、工作目录+/private/tmp 写入 | `(allow network*)` 或 `(deny network*)` | 拒绝控制字符、`(`、`)`、`\`、`"` |
| **Docker** | `--rm --security-opt no-new-privileges --cap-drop ALL` | `--network none` | 拒绝 `:`、`\0`、`\n`、`\r` |

**Docker 资源限制**：`--cpus`、`--memory`、`--pids-limit`。挂载模式：None（/tmp 工作目录）、ReadOnly、ReadWrite。

### 钩子系统

生命周期钩子在 Agent 事件时运行 shell 命令。通过配置中的 `hooks` 数组配置。

```rust
pub enum HookEvent { BeforeToolCall, AfterToolCall, BeforeLlmCall, AfterLlmCall }

pub struct HookConfig {
    pub event: HookEvent,
    pub command: Vec<String>,       // argv array (no shell interpretation)
    pub timeout_ms: u64,            // default: 5000
    pub tool_filter: Vec<String>,   // tool events only; empty = all
}
```

**Shell 协议**：通过 stdin 传递 JSON 载荷。退出码语义：0=允许，1=拒绝（仅 before 钩子），2+=错误。Before 钩子可以拒绝操作；after 钩子的退出码仅计为错误。

**熔断器**：`HookExecutor` 在连续 3 次失败后自动禁用钩子（可通过 `with_threshold()` 配置）。成功时重置。

**环境**：命令通过 `BLOCKED_ENV_VARS` 清理。波浪号展开支持 `~/` 和 `~username/`。

**集成**：接入 `chat.rs`、`gateway.rs`、`serve.rs`。钩子配置变更通过配置监视器触发重启。

### MCP 集成

Model Context Protocol 服务器的 JSON-RPC 传输。两种传输模式：

**传输方式**：
1. **Stdio**：将服务器作为子进程启动（command + args + env）。行限制：1MB。通过 `BLOCKED_ENV_VARS` 清理环境。
2. **HTTP/SSE**：通过 `url` 字段连接远程服务器。POST JSON，SSE 响应处理。

**生命周期**（stdio）：
1. 启动服务器（command + args + env，过滤 BLOCKED_ENV_VARS）
2. 初始化：`protocolVersion: "2024-11-05"`
3. 发现工具：`tools/list` RPC
4. 验证输入 schema（最大深度 10，最大大小 64KB）；拒绝无效 schema 的工具
5. 注册 McpTool 包装器（30 秒超时，1MB 最大响应）

**McpTool 执行**：`tools/call` 传入 name + arguments。从响应中提取 `content[].text`。

### 技能系统

技能是扩展 Agent 能力的 markdown 指令文件。两个来源：内置（编译进二进制）和工作区（用户安装）。

#### 技能文件格式（SKILL.md）

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

**Frontmatter 解析**：简单的 `key: value` 行匹配（非完整 YAML）。`split_frontmatter()` 在 `---` 分隔符之间查找内容。`strip_frontmatter()` 仅返回正文。

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

**可用性检查**：`available = requires_bins 全部在 PATH 中找到 且 requires_env 全部已设置`。缺少依赖的技能不可用但仍会列出。

#### SkillsLoader

```rust
pub struct SkillsLoader {
    skills_dir: PathBuf,        // {data_dir}/skills/
}
```

**方法**：
- `list_skills()` — 扫描工作区目录 + 内置。工作区技能覆盖同名内置（通过 HashSet 检查）。结果按字母排序。
- `load_skill(name)` — 返回正文（已剥离 frontmatter）。先检查工作区，回退到内置。
- `build_skills_summary()` — 生成 XML 用于系统提示注入：
  ```xml
  <skills>
    <skill available="true">
      <name>skill_name</name>
      <description>What it does</description>
      <location>/path/to/SKILL.md</location>
    </skill>
  </skills>
  ```
- `get_always_skills()` — 过滤 `always: true` 且 `available: true` 的技能。
- `load_skills_for_context(names)` — 加载多个技能，用 `\n---\n` 连接。

#### 内置技能（编译时 `include_str!()`）

```rust
pub struct BuiltinSkill {
    pub name: &'static str,
    pub content: &'static str,  // full SKILL.md including frontmatter
}
pub const BUILTIN_SKILLS: &[BuiltinSkill] = &[...];
```

| 技能 | 用途 |
|---|---|
| cron | 任务调度指令 |
| skill-store | 技能商店浏览和安装 |
| skill-creator | 创建新技能 |
| tmux | 终端复用器控制 |
| weather | 天气信息查询 |

#### CLI 管理（`octos skills`）

- `list` — 显示内置技能（附覆盖状态）+ 工作区技能
- `install <user/repo/skill-name>` — 从 `https://raw.githubusercontent.com/{repo}/main/SKILL.md` 获取（15 秒超时），保存到 `.octos/skills/{name}/SKILL.md`。如果技能已存在则失败。
- `remove <name>` — 删除 `.octos/skills/{name}/` 目录

#### 与网关集成

在网关命令中，技能在系统提示构建期间加载：
1. `get_always_skills()` — 收集自动加载的技能名称
2. `load_skills_for_context(names)` — 加载并连接技能正文
3. `build_skills_summary()` — 将 XML 技能索引追加到系统提示
4. 始终开启的技能内容前置到系统提示

### 插件系统

插件通过独立可执行文件扩展 Agent 的工具。每个插件是一个包含 `manifest.json` 和可执行文件的目录。

#### 目录布局

```
.octos/plugins/           # 本地（项目级）
~/.octos/plugins/         # 全局（用户级）
  └── my-plugin/
      ├── manifest.json  # 插件元数据 + 工具定义
      └── my-plugin      # 可执行文件（或 "main" 作为回退）
```

**发现顺序**：先本地 `.octos/plugins/`，再全局 `~/.octos/plugins/`。两者均由 `Config::plugin_dirs()` 扫描。

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

**manifest.json 示例**：
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

**`load_into(registry, dirs)`**：
1. 扫描每个目录的子目录
2. 对每个子目录查找 `manifest.json`
3. 解析清单，查找可执行文件（先尝试目录名，再尝试 `main`）
4. 验证可执行权限（Unix：`mode & 0o111 != 0`；非 Unix：存在性检查）
5. 将每个工具定义包装为实现 `Tool` trait 的 `PluginTool`
6. 注册到 `ToolRegistry`
7. 记录警告：`"loaded unverified plugin (no signature check)"`
8. 返回工具总数。失败的插件带警告跳过，不会导致致命错误。

#### PluginTool — 执行协议

```rust
pub struct PluginTool {
    plugin_name: String,
    tool_def: PluginToolDef,
    executable: PathBuf,
}
```

**调用**：`executable <tool_name>`（工具名称作为第一个参数传递）。

**stdin/stdout 协议**：
1. 以工具名称为参数启动可执行文件，管道连接 stdin/stdout/stderr
2. 将 JSON 序列化的参数写入 stdin，关闭（EOF 表示输入结束）
3. 等待退出，30 秒超时（`PLUGIN_TIMEOUT`）
4. 解析 stdout 为 JSON：
   - **结构化**：`{"output": "...", "success": true/false}` → 使用解析后的值
   - **回退**：原始 stdout + stderr 拼接，成功由退出码决定
5. 返回 `ToolResult`（插件不跟踪 `file_modified`）

**错误处理**：
- 启动失败 → 包含插件名称和可执行文件路径的 eyre 错误
- 超时 → 包含插件名称、工具名称和持续时间的 eyre 错误
- JSON 解析失败 → 优雅回退到原始输出

### 进度报告

Agent 在执行期间通过基于 trait 的观察者模式发出结构化事件。消费者（CLI、REST API）实现该 trait 以各自的格式渲染进度。

#### ProgressReporter Trait

```rust
pub trait ProgressReporter: Send + Sync {
    fn report(&self, event: ProgressEvent);
}
```

Agent 持有 `reporter: Arc<dyn ProgressReporter>`。事件在执行循环期间同步触发（非阻塞 — 实现不得阻塞）。

#### ProgressEvent 枚举

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

#### 实现（3 种）

**SilentReporter** — 空操作，未配置报告器时用作默认值。

**ConsoleReporter** — 带 ANSI 颜色和流式支持的 CLI 输出：

```rust
pub struct ConsoleReporter {
    use_colors: bool,
    verbose: bool,
    stdout: Mutex<BufWriter<Stdout>>,  // buffered for streaming chunks
}
```

| 事件 | 输出 |
|---|---|
| Thinking | `\r⟳ Thinking... (iteration N)`（覆写行，黄色） |
| Response | `◆ first 3 lines...`（青色，清除 Thinking 行） |
| ToolStarted | `\r⚙ Running tool_name...`（覆写行，黄色） |
| ToolCompleted | `✓ tool_name (duration)` 绿色 或 `✗ tool_name` 红色；verbose：5 行输出 + `...` |
| FileModified | `📝 Modified: path`（绿色） |
| TokenUsage | `Tokens: N in, N out`（仅 verbose，暗色） |
| TaskCompleted | `✓ Completed N iterations, Xs` 或 `✗ Failed after N iterations` |
| TaskInterrupted | `⚠ Interrupted after N iterations.`（黄色） |
| MaxIterationsReached | `⚠ Reached max iterations limit (N).`（黄色） |
| TokenBudgetExceeded | `⚠ Token budget exceeded (used, limit).`（黄色） |
| StreamChunk | 写入缓冲 stdout；仅在 `\n` 时 flush（减少系统调用） |
| StreamDone | Flush + 换行 |
| CostUpdate | `Tokens: N in / N out \| Cost: $X.XXXX` |
| TaskStarted | `▶ Task: id`（仅 verbose，暗色） |

**持续时间格式化**：>1s → `{:.1}s`，≤1s → `{N}ms`。

**SseBroadcaster**（REST API，feature：`api`）— 将事件转换为 JSON 并通过 `tokio::sync::broadcast` 频道广播：

```rust
pub struct SseBroadcaster {
    tx: broadcast::Sender<String>,  // JSON-serialized events
}
```

| ProgressEvent | JSON `type` 字段 | 附加字段 |
|---|---|---|
| ToolStarted | `"tool_start"` | `tool` |
| ToolCompleted | `"tool_end"` | `tool`、`success` |
| StreamChunk | `"token"` | `text` |
| StreamDone | `"stream_end"` | — |
| CostUpdate | `"cost_update"` | `input_tokens`、`output_tokens`、`session_cost` |
| Thinking | `"thinking"` | `iteration` |
| Response | `"response"` | `iteration` |
| （其他） | `"other"` | —（debug 级别记录） |

订阅者通过 `SseBroadcaster::subscribe() -> broadcast::Receiver<String>` 接收事件。发送错误（无订阅者）静默忽略。

### 执行环境（`exec_env.rs`）

`ExecEnvironment` trait 包含 `exec(cmd, args, env)`、`read_file(path)`、`write_file(path, content)`、`file_exists(path)`、`list_dir(path)`。两种实现：`LocalEnvironment`（tokio::process::Command）和 `DockerEnvironment`（docker exec）。环境变量通过共享的 `BLOCKED_ENV_VARS` 清理。Docker 路径验证防止注入字符（`\0`、`\n`、`\r`、`:`）。Docker 环境变量通过 `--env` 标志转发。

### 提供商工具集（`provider_tools.rs`）

`ToolAdjustment`（prefer、demote、aliases、extras）按 LLM 提供商配置。`ProviderToolsets` 注册表包含 `with_defaults()` 用于 openai/anthropic/google。用于按提供商优化工具展示（例如 OpenAI 偏好 shell/read_file，降低 diff_edit）。

### 类型化回合（`turn.rs`）

`Turn` 用 `TurnKind`（UserInput、AgentReply、ToolCall、ToolResult、System）和迭代次数包装 `Message`。`turns_to_messages()` 转换回 `Vec<Message>` 用于 LLM 调用。支持对对话历史的语义分析。

### 事件总线（`event_bus.rs`）

`EventBus` 包含类型化的 `EventSubscriber`，用于 Agent 内部的发布/订阅。解耦事件生产者（工具执行、LLM 调用）与消费者（日志、指标、UI 更新）。

### 循环检测（`loop_detect.rs`）

检测重复的 Agent 行为（如使用相同参数调用同一工具）。可配置阈值和窗口。检测到循环时提前返回诊断消息。

### 会话状态（`session.rs`）

`SessionState` 包含 `SessionLimits` 和 `SessionUsage` 跟踪。`SessionStateHandle` 用于线程安全访问。根据配置的限制跟踪 token 用量、迭代次数和墙钟时间。

### 引导（`steering.rs`）

`SteeringMessage` 包含 `SteeringSender`/`SteeringReceiver`（mpsc 通道）。允许在对话中途从外部控制 Agent 行为（如注入引导、改变策略）。

### 提示层（`prompt_layer.rs`）

`PromptLayerBuilder` 用于从多个来源组合系统提示（基础提示、人设、用户上下文、记忆、技能）。各层按顺序拼接，可配置分隔符。

---

## octos-bus — 网关基础设施

### 消息总线

`create_bus() -> (AgentHandle, BusPublisher)` 通过 mpsc 通道连接（容量 256）。AgentHandle 接收 InboundMessage；BusPublisher 分发 OutboundMessage。

**队列模式**（通过 `gateway.queue_mode` 配置）：
- `Followup`（默认）：FIFO — 逐条处理排队消息
- `Collect`：按会话合并排队消息，拼接内容后再处理

### 频道 Trait

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

### 频道实现

| 频道 | 传输方式 | Feature Flag | 认证 | 去重 |
|---|---|---|---|---|
| **CLI** | stdin/stdout | （始终启用） | 无 | 无 |
| **Telegram** | teloxide 长轮询 | `telegram` | Bot token (env) | teloxide 内置 |
| **Discord** | serenity gateway | `discord` | Bot token (env) | serenity 内置 |
| **Slack** | Socket Mode (tokio-tungstenite) | `slack` | Bot token + App token | message_ts |
| **WhatsApp** | WebSocket 桥接 (ws://localhost:3001) | `whatsapp` | Baileys 桥接 | HashSet（10K 上限，溢出时清空） |
| **飞书** | WebSocket (tokio-tungstenite) | `feishu` | App ID + Secret → tenant token (TTL 6000s) | HashSet（10K 上限，溢出时清空） |
| **邮件** | IMAP 轮询 + SMTP 发送 | `email` | 用户名/密码，rustls TLS | IMAP UNSEEN 标志 |
| **企业微信** | 企业微信 API | `wecom` | Corp ID + Agent Secret | message_id |
| **Twilio** | Twilio SMS/MMS | `twilio` | Account SID + Auth Token | message SID |

**邮件细节**：IMAP 通过 `async-imap` + rustls 接收（轮询未读，标记 \Seen）。SMTP 通过 `lettre` 发送（端口 465=隐式 TLS，其他=STARTTLS）。`mailparse` 用于 RFC822 正文提取。正文通过 `truncate_utf8(max_body_chars)` 截断。

**飞书细节**：带 TTL 缓存的 Tenant Access Token（6000 秒）。从 `/callback/ws/endpoint` 获取 WebSocket 网关 URL。通过 `header.event_type == "im.message.receive_v1"` 检测消息类型。支持 `oc_*`（chat_id）vs `ou_*`（open_id）路由。

**Markdown 转 HTML**：`markdown_html.rs` 将 Markdown 转换为 Telegram 兼容的 HTML 用于富文本消息格式化。

**媒体**：`download_media()` 辅助函数将照片/语音/音频/文档下载到 `.octos/media/`。

**语音转文字**：语音/音频在 Agent 处理前自动通过 GroqTranscriber 转录。

### 消息合并

将超大消息拆分为适合频道的分块：

| 频道 | 最大字符数 |
|---|---|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

**断开优先级**：段落（`\n\n`）> 换行（`\n`）> 句号（`. `）> 空格（` `）> 硬截断。

MAX_CHUNKS = 50（DoS 限制）。通过 `char_indices()` 实现 UTF-8 安全的边界检测。

### 会话管理器

JSONL 持久化位于 `.octos/sessions/{key}.jsonl`。

- **内存缓存**：LRU + 写入时同步到磁盘
- **文件名**：百分号编码的 SessionKey，截断到 183 字符，截断时添加 `_{hash:016X}` 后缀防止冲突
- **文件大小限制**：最大 10MB（`MAX_SESSION_FILE_SIZE`）；加载时跳过超大文件
- **崩溃安全**：原子写入-重命名
- **分支**：`fork()` 创建带 `parent_key` 追踪的子会话，复制最后 N 条消息

### 定时服务

JSON 持久化位于 `.octos/cron.json`。

**调度类型**：
- `Every { seconds: u64 }` — 周期性间隔
- `Cron { expr: String }` — 通过 `cron` crate 的 cron 表达式
- `At { timestamp_ms: i64 }` — 一次性（运行后自动删除）

**CronJob 字段**：id（来自 UUIDv7 的 8 字符十六进制）、name、enabled、schedule、payload（message + deliver 标志 + channel + chat_id）、state（next_run_at_ms, run_count）、delete_after_run。

### 心跳服务

定期检查 `HEARTBEAT.md`（默认：30 分钟间隔）。如果非空则将内容发送给 Agent。

---

## octos-cli — CLI 与配置

### 命令

| 命令 | 说明 |
|---|---|
| `chat` | 交互式多轮对话。Readline + 历史。退出：exit/quit/:q |
| `gateway` | 带会话管理的持久多频道守护进程 |
| `init` | 初始化 .octos/ 目录，包含配置、模板和子目录 |
| `status` | 显示配置、提供商、API 密钥、引导文件 |
| `auth login/logout/status` | OAuth PKCE（OpenAI）、设备码、粘贴 token |
| `cron list/add/remove/enable` | CLI 定时任务管理 |
| `channels status/login` | 频道编译状态、WhatsApp 桥接设置 |
| `skills list/install/remove` | 技能管理、GitHub 获取 |
| `office` | Office/工作区管理 |
| `account` | 账户管理 |
| `clean` | 删除 .redb 文件，支持 dry-run |
| `completions` | Shell 补全生成（bash/zsh/fish） |
| `docs` | 生成工具 + 提供商文档 |
| `serve` | REST API 服务器（feature：api）— axum 监听 127.0.0.1:8080（`--host` 覆盖） |

### 配置

从 `.octos/config.json`（本地）或 `~/.config/octos/config.json`（全局）加载。本地优先。

- **`${VAR}` 展开**：字符串值中的环境变量替换
- **版本化配置**：版本字段 + 自动 `migrate_config()` 框架
- **提供商自动检测**（`registry::detect_provider(model)`）：claude→anthropic、gpt/o1/o3/o4→openai、gemini→gemini、deepseek→deepseek、kimi/moonshot→moonshot、qwen→dashscope、glm→zhipu、llama/mixtral→groq。模式在 `registry/` 中按提供商定义。

**API 密钥解析顺序**：认证存储（`~/.octos/auth.json`）→ 环境变量。

### 认证模块

**OAuth PKCE**（OpenAI）：
1. 生成 64 字符验证器（两个 UUIDv4 十六进制）
2. SHA-256 挑战，base64-URL 编码（无填充）
3. TCP 监听端口 1455
4. 浏览器 → `auth.openai.com` + PKCE + state
5. 回调验证 state（CSRF），用 code+verifier 换取 token

**设备码流程**（OpenAI）：POST `deviceauth/usercode`，每 5 秒以上轮询 `deviceauth/token`。

**粘贴 Token**：从 stdin 提示输入 API 密钥，以 `auth_method: "paste_token"` 存储。

**AuthStore**：`~/.octos/auth.json`（mode 0600）。`{credentials: {provider: AuthCredential}}`。

### 配置监视器

每 5 秒轮询。通过文件内容的 SHA-256 哈希比较。

**可热重载**：system_prompt、max_history（实时生效）。

**需要重启**：provider、model、base_url、api_key_env、sandbox、mcp_servers、hooks、gateway.queue_mode、channels。

### REST API（feature：`api`）

| 路由 | 方法 | 说明 |
|---|---|---|
| `/api/chat` | POST | 发送消息 → 获取响应 |
| `/api/chat/stream` | GET | ProgressEvent 的 SSE 流 |
| `/api/sessions` | GET | 列出所有会话 |
| `/api/sessions/{id}/messages` | GET | 分页历史（?limit=100&offset=0，最大 500） |
| `/api/status` | GET | 版本、模型、提供商、运行时间 |
| `/metrics` | GET | Prometheus 文本格式（无需认证） |
| `/*`（回退） | GET | 嵌入式 Web UI（通过 rust-embed 的静态文件） |

**认证**：可选的 bearer token，常量时间比较（仅 API 路由；`/metrics` 和静态文件为公开）。**CORS**：localhost:3000/8080。**最大消息**：1MB。

**Web UI**：通过 `rust-embed` 嵌入的 SPA，作为回退处理器提供服务。会话侧边栏、聊天界面、SSE 流式推送、暗色主题。原生 HTML/CSS/JS（无构建工具）。

**Prometheus 指标**：`octos_tool_calls_total`（计数器，标签：tool, success）、`octos_tool_call_duration_seconds`（直方图，标签：tool）、`octos_llm_tokens_total`（计数器，标签：direction）。由 `metrics` + `metrics-exporter-prometheus` crate 驱动。

### 会话压缩（网关）

当消息数 > 40（阈值）时触发。保留最近 10 条消息。通过 LLM 将较旧消息摘要为 <500 词。重写 JSONL 会话文件。

---

## octos-pipeline — 基于 DOT 的流水线编排

基于 DOT 的流水线编排引擎，用于定义和执行多步骤工作流。

- `parser.rs` — DOT 图解析器（将 Graphviz DOT 格式解析为流水线定义）
- `graph.rs` — PipelineGraph，包含节点/边类型
- `executor.rs` — 异步流水线执行引擎
- `handler.rs` — 处理器类型：CodergenHandler、GateHandler、ShellHandler、NoopHandler、DynamicParallel
- `condition.rs` — 条件边求值（分支逻辑）
- `tool.rs` — RunPipelineTool 集成（将流水线执行暴露为 Agent 工具）
- `validate.rs` — 图验证和 lint 诊断
- `human_gate.rs` — 人在环路门，包含 `HumanInputProvider` trait、`ChannelInputProvider`（mpsc + oneshot，默认 5 分钟超时）、`AutoApproveProvider`。输入类型：Approval、FreeText、Choice
- `fidelity.rs` — `FidelityMode` 枚举（Full、Truncate、Compact、Summary），用于节点间上下文传递控制。从配置字符串解析。安全上限：10MB max_chars、100K max_lines
- `manager.rs` — `PipelineManager` 管理器，包含 `SupervisionStrategy`（AllOrNothing、BestEffort、RetryFailed）。重试上限 10 次，指数退避（100ms-5s）。`ManagerOutcome` 转换为 `NodeOutcome`
- `thread.rs` — `ThreadRegistry` 用于跨流水线节点的 LLM 会话复用。`Thread` 存储 model_id + 消息历史。限制：1000 线程，每线程 10000 条消息
- `server.rs` — `PipelineServer` trait，包含 `SubmitRequest`（已验证：1MB DOT、256KB 输入、64 个变量、安全流水线 ID）、`RunStatus` 生命周期（Queued → Running → Completed/Failed/Cancelled）
- `artifact.rs` — 流水线中间产物存储
- `checkpoint.rs` — 流水线检查点/恢复，用于崩溃恢复
- `events.rs` — 流水线事件系统，用于进度跟踪
- `run_dir.rs` — 按运行隔离的工作目录
- `stylesheet.rs` — 流水线图渲染的视觉样式

---

## 数据流

### Chat 模式

```
用户输入 → readline → Agent.process_message(input, history)
                              │
                              ├─ 构建消息（系统提示 + 历史 + 记忆 + 输入）
                              ├─ trim_to_context_window()（必要时）
                              ├─ 通过 chat_stream() 调用 LLM，附带工具规格
                              ├─ 如果 ToolUse 则执行工具（循环）
                              └─ 返回 ConversationResponse
                                    │
                              打印响应，追加到历史
```

### 网关模式

```
频道 → InboundMessage → MessageBus → [转录音频] → [加载会话]
                                              │
                                    Agent.process_message()
                                              │
                                        OutboundMessage
                                              │
                                   ChannelManager.dispatch()
                                              │
                                    coalesce() → Channel.send()
```

系统消息（cron、心跳、spawn 结果）通过相同的总线流转，`channel: "system"` 和 metadata 路由。

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

## 文件布局

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

## 安全

### 工作区级安全

- `#![deny(unsafe_code)]` — 通过 `[workspace.lints.rust]` 设置的工作区级 lint
- `secrecy::SecretString` — 所有提供商 API 密钥都被包装；防止意外日志/显示

### 认证与凭据

- API 密钥：认证存储（`~/.octos/auth.json`，mode 0600）优先于环境变量
- 带 SHA-256 挑战的 OAuth PKCE，state 参数（CSRF 保护）
- API bearer token 使用常量时间字节比较（防时序攻击）

### 执行沙箱

- 三种后端：bwrap（Linux）、sandbox-exec（macOS）、Docker — `SandboxMode::Auto` 检测
- 18 个 BLOCKED_ENV_VARS 在所有沙箱后端、MCP 服务器启动、钩子和浏览器工具中共享：`LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH, DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH, NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB, JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR`
- 按后端的路径注入防护（Docker：`:`、`\0`、`\n`、`\r`；macOS：控制字符、`(`、`)`、`\`、`"`）
- Docker：`--cap-drop ALL`、`--security-opt no-new-privileges`、`--network none`，阻止绑定挂载源（`docker.sock`、`/proc`、`/sys`、`/dev`、`/etc`）

### 工具安全

- ShellTool SafePolicy：拒绝 `rm -rf /`、`dd`、`mkfs`、fork 炸弹、`chmod -R 777 /`；询问 `sudo`、`rm -rf`、`git push --force`、`git reset --hard`。匹配前空白归一化。超时钳制到 [1, 600] 秒。SIGTERM→宽限期→SIGKILL 子进程清理。
- 工具策略：allow/deny，deny 优先语义，8 个命名分组（`group:fs`、`group:runtime`、`group:web`、`group:search`、`group:sessions` 等），通配符匹配，按提供商过滤（`tools.byProvider`）
- 工具参数大小限制：每次调用 1MB（非分配的 `estimate_json_size`，含转义字符计算）
- 符号链接安全文件 I/O：Unix 上通过 `O_NOFOLLOW` 实现原子级内核检查，消除 TOCTOU 竞态；Windows 上使用基于元数据的符号链接检查回退
- SSRF 防护在共享的 `ssrf.rs` 模块中：DNS 解析失败时采用故障关闭策略（DNS 失败时阻止请求），私有 IP 阻止（10/8、172.16/12、192.168/16、169.254/16），IPv6 覆盖（ULA `fc00::/7`、链路本地 `fe80::/10`、站点本地 `fec0::/10`、IPv4 映射 `::ffff:0:0/96`、IPv4 兼容 `::/96`），回环地址阻止。被 web_fetch 和 browser 使用。
- 浏览器：URL scheme 白名单（仅 http/https）、10 秒 JS 执行超时、僵尸进程清理、截图使用安全临时文件
- MCP：输入 schema 验证（最大深度 10，最大大小 64KB）防止恶意工具定义
- 提示注入防护（`prompt_guard.rs`）：5 种威胁类别（SystemOverride、RoleConfusion、ToolCallInjection、SecretExtraction、InstructionInjection），10 种检测模式。检测到的威胁被包裹在 `[injection-blocked:...]` 中进行清理。

### 数据安全

- 工具输出清理（`sanitize.rs`）：剥离 base64 数据 URI、长十六进制字符串（64+ 字符），以及**凭据脱敏** — 7 个正则表达式覆盖 OpenAI（`sk-...`）、Anthropic（`sk-ant-...`）、AWS（`AKIA...`）、GitHub（`ghp_/gho_/ghs_/ghr_/github_pat_...`）、GitLab（`glpat-...`）、Bearer token 和通用 `password`/`api_key` 赋值
- 通过 `truncate_utf8()` 在所有工具输出和邮件正文中实现 UTF-8 安全截断
- 通过百分号编码文件名 + 截断时的哈希后缀防止会话文件冲突
- 会话文件大小限制：最大 10MB 防止损坏文件导致 OOM
- 原子写入-重命名用于会话持久化（崩溃安全）
- API 服务器默认绑定到 127.0.0.1（非 0.0.0.0）
- 通过 `allowed_senders` 列表进行频道访问控制
- MCP 响应限制：每条 JSON-RPC 行 1MB（DoS 防护）
- 消息合并：MAX_CHUNKS=50 DoS 限制
- API 消息限制：每个请求 1MB

---

## 并发模型

### 为什么选择 Rust

octos 使用 Rust + tokio 异步运行时，与 Python（OpenClaw 等）和 Node.js（NanoCloud 等）Agent 框架相比，在并发会话处理方面具有显著优势：

**真正的并行** — Tokio 任务跨所有 CPU 核心同时运行。Python 有 GIL，即使使用 asyncio，CPU 密集型工作（JSON 解析、上下文压缩、token 计数）也是单核的。Node.js 完全是单线程的。在 octos 中，10 个并发会话进行上下文压缩实际上会跨核心并行执行。

**内存效率** — 无垃圾回收器，无每对象运行时开销。Agent 会话是堆上的紧凑结构体。Python Agent 会话携带解释器开销、每个对象的 GC 元数据和基于 dict 的属性查找。在数百个会话和大量对话历史都在内存中时，这一点很重要。

**无 GC 暂停** — Python 和 Node.js 的 GC 可能导致响应中途的延迟尖峰。Rust 有确定性的内存释放 — 当拥有者结构体 drop 时内存立即释放。

**单二进制部署** — 无需安装 Python/Node 运行时，无依赖地狱，可预测的资源使用。网关是一个静态二进制文件。

### Tokio 任务 vs 操作系统线程

所有并发会话处理使用 tokio 任务（绿色线程），而非操作系统线程。tokio 任务是堆上的状态机（约几 KB）。操作系统线程约 8MB 栈。数千个任务复用在少量操作系统线程上（默认为 CPU 核心数）。由于 Agent 会话大部分时间都在等待 I/O（LLM API 响应），它们会高效地让出线程给其他任务。

### 网关并发

```
入站消息 → 主循环
                      │
                      ├─ tokio::spawn() 每条消息
                      │     │
                      │     ├─ Semaphore（max_concurrent_sessions，默认 10）
                      │     │     限制总并发 Agent 运行数
                      │     │
                      │     └─ 按会话的 Mutex
                      │           序列化同一会话内的消息
                      │
                      └─ 不同会话并发运行
                         同一会话顺序排队
```

- **跨会话**：并发，由 `max_concurrent_sessions` 信号量限制（默认 10）
- **同一会话内**：通过按会话 mutex 序列化 — 防止对话历史的竞态条件
- **按会话锁**：完成后修剪（Arc strong_count == 1）以防止 HashMap 无限增长

### 工具执行

在单次 Agent 迭代内，一个 LLM 响应中的所有工具调用通过 `join_all()` 并发执行：

```
LLM 响应：[web_search, read_file, send_email]
                   │            │           │
                   └────────────┼───────────┘
                          join_all()
                   ┌────────────┼───────────┐
                   │            │           │
                 完成          完成         完成
                          ↓
              所有结果追加到消息
                          ↓
                    下一次 LLM 调用
```

### 子 Agent 模式（spawn 工具）

| 方面 | 同步 | 后台 |
|--------|------|------------|
| 父 Agent 是否阻塞？ | 是 | 否（`tokio::spawn()`） |
| 结果传递 | 同一对话轮次 | 通过网关的新入站消息 |
| Token 计算 | 计入父预算 | 独立 |
| 使用场景 | 顺序流水线 | 触发后不管的长任务 |

子 Agent 不能再生成子 Agent（spawn 工具在子 Agent 策略中始终被拒绝）。

### 多租户仪表板

仪表板（`octos serve`）将每个用户配置文件作为**独立的网关操作系统进程**运行：

```
Dashboard (octos serve)
  ├─ Profile "alice" → octos gateway --config alice.json  (deepseek, own semaphore)
  ├─ Profile "bob"   → octos gateway --config bob.json    (kimi, own semaphore)
  └─ Profile "carol" → octos gateway --config carol.json  (openai, own semaphore)
```

每个配置文件拥有自己的 LLM 提供商、API 密钥、频道、数据目录和 `max_concurrent_sessions` 信号量。配置文件完全隔离 — 网关进程间无共享状态。

---

## 测试

全部 crate 共 1300+ 测试。完整清单和 CI 指南见 [TESTING.md](./TESTING.md)。

- **单元测试**：类型 serde 往返、工具参数解析、配置验证、提供商检测、工具策略、压缩、合并、BM25 评分、L2 归一化、SSE 解析
- **自适应路由**：Off/Hedge/Lane 模式、熔断器、故障转移、评分、指标、提供商竞速（19 个测试）
- **响应性**：基线学习、劣化检测、恢复、阈值边界（8 个测试）
- **队列模式**：Followup、Collect、Steer、Speculative 溢出、自动升级/降级（9 个测试）
- **会话持久化**：JSONL 存储、LRU 淘汰、分支、重写、时间戳排序、并发访问（28 个测试）
- **集成测试**：CLI 命令、文件工具、定时任务、会话分支、插件加载
- **安全测试**：沙箱路径注入、环境清理、SSRF 阻断、符号链接拒绝（O_NOFOLLOW）、私有 IP 检测、去重溢出、工具参数大小限制、会话文件大小限制、熔断器阈值边界、MCP schema 验证
- **频道测试**：allowed_senders、消息解析、去重逻辑、邮件地址提取

本地 CI：`./scripts/ci.sh`（与 GitHub Actions 一致 + 针对性子系统测试）。见 [TESTING.md](./TESTING.md)。
