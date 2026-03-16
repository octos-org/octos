# NLSpec Features Guide

Technical documentation for the NLSpec feature set implemented across octos. These features were added to close gaps identified in the [Attractor NLSpec audit](./ATTRACTOR_GAP_ANALYSIS.md).

## Architecture Overview

NLSpec features span three crates in the octos workspace:

```
octos-llm          LLM abstraction layer
  - error.rs        Typed error hierarchy
  - high_level.rs   Ergonomic LlmClient API
  - middleware.rs    Request/response interceptors
  - catalog.rs      Model registry with capabilities & costs

octos-agent         Agent runtime
  - exec_env.rs     Execution environment abstraction
  - provider_tools.rs  Per-provider tool adjustments
  - turn.rs         Typed conversation turns

octos-pipeline      Pipeline orchestration
  - human_gate.rs   Human-in-the-loop gates
  - fidelity.rs     Context carryover control
  - manager.rs      Child pipeline supervision
  - thread.rs       LLM session reuse
  - server.rs       Pipeline HTTP server interface
```

---

## octos-llm Features

### Typed Error Hierarchy (`octos_llm::error`)

Replaces string-matching on `eyre::Report` with structured, actionable errors.

```rust
use octos_llm::error::{LlmError, LlmErrorKind};

match client.generate("hello").await {
    Ok(text) => println!("{text}"),
    Err(e) => {
        if let Some(llm_err) = e.downcast_ref::<LlmError>() {
            match llm_err.kind() {
                LlmErrorKind::RateLimited => {
                    // Back off and retry
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                LlmErrorKind::ContextOverflow => {
                    // Compact messages and retry
                }
                LlmErrorKind::Authentication => {
                    // Check API key configuration
                }
                _ => {}
            }

            if llm_err.is_retryable() {
                // RateLimited, ServerError, Network, Timeout are retryable
            }
        }
    }
}
```

**Error kinds:**

| Kind | Retryable | Description |
|------|-----------|-------------|
| `Authentication` | No | Invalid or missing API key |
| `RateLimited` | Yes | 429 Too Many Requests |
| `ContextOverflow` | No | Input exceeds model context window |
| `ModelNotFound` | No | Requested model doesn't exist |
| `ServerError` | Yes | 5xx from provider |
| `Network` | Yes | Connection/DNS failure |
| `Timeout` | Yes | Request timed out |
| `InvalidRequest` | No | Malformed request (400) |
| `ContentFiltered` | No | Content policy violation |
| `StreamError` | Yes | SSE stream interrupted |
| `Provider` | No | Catch-all for provider-specific errors |

**Constructing from HTTP status codes:**

```rust
let err = LlmError::from_status(429, "rate limit exceeded");
assert_eq!(err.kind(), &LlmErrorKind::RateLimited);
assert!(err.is_retryable());
```

### High-Level LLM Client (`octos_llm::high_level`)

Ergonomic wrapper around `LlmProvider` for common patterns.

```rust
use octos_llm::high_level::LlmClient;
use octos_llm::config::ChatConfig;

let client = LlmClient::new(provider);

// Simple text generation
let answer = client.generate("What is 2+2?").await?;

// Structured JSON output
let schema = serde_json::json!({
    "type": "object",
    "properties": {
        "name": {"type": "string"},
        "age": {"type": "integer"}
    }
});
let value = client.generate_object("Create a person", "Person", schema.clone()).await?;
println!("{}", value["name"]); // "Alice"

// Typed deserialization
#[derive(serde::Deserialize)]
struct Person { name: String, age: u32 }
let person: Person = client.generate_typed("Create a person", "Person", schema).await?;

// Streaming
let stream = client.stream("Tell me a story").await?;

// Full control with message history and tools
let response = client.generate_with(&messages, &tools, &config).await?;
```

**Custom default config:**

```rust
let client = LlmClient::new(provider)
    .with_config(ChatConfig {
        temperature: Some(0.7),
        max_tokens: Some(4096),
        ..Default::default()
    });
```

### Middleware Pipeline (`octos_llm::middleware`)

Composable request/response interceptors for cross-cutting concerns.

```rust
use octos_llm::middleware::{MiddlewareStack, LlmMiddleware, LoggingMiddleware, CostTracker};

// Stack middleware layers
let tracker = Arc::new(CostTracker::new());
let stack = MiddlewareStack::new(provider)
    .with(Arc::new(LoggingMiddleware))
    .with(tracker.clone());

// Use stack as a normal LlmProvider
let response = stack.chat(&messages, &tools, &config).await?;

// Query accumulated costs
println!("Total input tokens: {}", tracker.total_input_tokens());
println!("Total output tokens: {}", tracker.total_output_tokens());
println!("Request count: {}", tracker.request_count());
```

**Custom middleware (e.g., caching):**

```rust
use octos_llm::middleware::LlmMiddleware;

struct CacheMiddleware { /* ... */ }

#[async_trait]
impl LlmMiddleware for CacheMiddleware {
    async fn before(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<Option<ChatResponse>> {
        // Return Some(response) to short-circuit the LLM call
        if let Some(cached) = self.lookup(messages) {
            return Ok(Some(cached));
        }
        Ok(None) // Continue to next layer / actual provider
    }

    async fn after(
        &self,
        messages: &[Message],
        response: ChatResponse,
    ) -> Result<ChatResponse> {
        // Store in cache, transform response, etc.
        self.store(messages, &response);
        Ok(response)
    }

    fn on_error(&self, error: &eyre::Report) {
        // Log, increment error counters, etc.
    }
}
```

**Note:** Streaming calls (`chat_stream`) bypass middleware layers (logged as a debug warning). This is by design since streaming responses can't be buffered for `after()` hooks without breaking the streaming contract.

### Model Catalog (`octos_llm::catalog`)

Programmatic model discovery with capabilities, costs, and aliases.

```rust
use octos_llm::catalog::ModelCatalog;

let catalog = ModelCatalog::with_defaults();

// Lookup by ID or alias
let model = catalog.get("sonnet").unwrap();
assert_eq!(model.id, "claude-sonnet-4-20250514");
assert_eq!(model.context_window, 200_000);
assert!(model.capabilities.reasoning);

// Find models by provider
let anthropic_models = catalog.by_provider("anthropic"); // [Sonnet 4, Haiku 4.5]

// Find models with specific capabilities
let reasoning_models = catalog.with_capability(|c| c.reasoning);
let vision_models = catalog.with_capability(|c| c.vision);

// Cost estimation
let cost = &model.cost;
let estimated = (input_tokens as f64 / 1_000_000.0) * cost.input_per_mtok
              + (output_tokens as f64 / 1_000_000.0) * cost.output_per_mtok;
```

**Pre-registered models (as of 2025-06-01):**

| Alias | Model ID | Provider | Context | Reasoning |
|-------|----------|----------|---------|-----------|
| `sonnet` | claude-sonnet-4-20250514 | anthropic | 200K | Yes |
| `haiku`, `cheap` | claude-haiku-4-5-20251001 | anthropic | 200K | No |
| `4o` | gpt-4o | openai | 128K | No |
| `flash` | gemini-2.5-flash | google | 1M | Yes |

**Register custom models:**

```rust
let mut catalog = ModelCatalog::with_defaults();
catalog.register(ModelInfo {
    id: "my-model-v1".into(),
    name: "My Fine-tuned Model".into(),
    provider: "custom".into(),
    context_window: 32_768,
    max_output_tokens: Some(4096),
    capabilities: ModelCapabilities { tool_use: true, ..Default::default() },
    cost: ModelCost { input_per_mtok: 1.0, output_per_mtok: 3.0, ..Default::default() },
    aliases: vec!["my-model".into()],
});
```

---

## octos-agent Features

### Execution Environment (`octos_agent::exec_env`)

Abstracts command execution across local and Docker environments.

```rust
use octos_agent::exec_env::{LocalEnvironment, DockerEnvironment, ExecEnvironment};

// Local execution
let local = LocalEnvironment::new("/path/to/workdir");
let output = local.exec("cargo", &["test"], &[("RUST_LOG", "debug")]).await?;
println!("stdout: {}", output.stdout);
println!("exit code: {}", output.exit_code);

// Docker execution
let docker = DockerEnvironment::new("rust:latest", "/workspace");
let output = docker.exec("cargo", &["build"], &[]).await?;

// File operations (same interface for both)
let contents = local.read_file("src/main.rs").await?;
local.write_file("output.txt", "results here").await?;
let exists = local.file_exists("Cargo.toml").await?;
let entries = local.list_dir("src").await?;
```

**Security features:**
- Environment variables are sanitized: `BLOCKED_ENV_VARS` (LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.) are automatically filtered
- Docker paths are validated against injection characters (`\0`, `\n`, `\r`, `:`)
- Docker `write_file` checks command exit status

### Provider Toolsets (`octos_agent::provider_tools`)

Per-provider tool adjustments for optimal tool use across different LLM providers.

```rust
use octos_agent::provider_tools::{ProviderToolsets, ToolAdjustment};

let toolsets = ProviderToolsets::with_defaults();

// Get adjustments for a specific provider
if let Some(adj) = toolsets.get("openai") {
    println!("Preferred tools: {:?}", adj.prefer);   // ["shell", "read_file"]
    println!("Demoted tools: {:?}", adj.demote);      // ["diff_edit"]
}

// Register custom adjustments
let mut toolsets = ProviderToolsets::new();
toolsets.register("my-provider", ToolAdjustment {
    prefer: vec!["read_file".into(), "write_file".into()],
    demote: vec!["diff_edit".into()],
    aliases: vec![("bash".into(), "shell".into())],
    extras: Default::default(),
});
```

**Default provider adjustments:**

| Provider | Preferred | Demoted |
|----------|-----------|---------|
| openai | shell, read_file | diff_edit |
| anthropic | diff_edit, shell | - |
| google | shell, read_file, write_file | diff_edit |

### Typed Turns (`octos_agent::turn`)

Typed wrapper around `Message` that tracks conversation semantics.

```rust
use octos_agent::turn::{Turn, TurnKind, turns_to_messages};
use octos_core::Message;

// Create typed turns
let turns = vec![
    Turn::new(Message::user("Fix the bug in auth.rs"), TurnKind::UserInput, 0),
    Turn::new(Message::assistant("I'll look at the file."), TurnKind::AgentReply, 1),
    Turn::new(Message::assistant("Called read_file"), TurnKind::ToolCall, 1),
    Turn::new(Message::user("File contents..."), TurnKind::ToolResult, 1),
    Turn::new(Message::assistant("Fixed the bug."), TurnKind::AgentReply, 2),
];

// Convert back to raw messages for LLM calls
let messages: Vec<Message> = turns_to_messages(&turns);
```

**Turn kinds:**

| Kind | Description |
|------|-------------|
| `UserInput` | User message |
| `AgentReply` | Assistant response (text) |
| `ToolCall` | Assistant requesting tool execution |
| `ToolResult` | Tool execution result |
| `System` | System/instruction message |

---

## octos-pipeline Features

### Human-in-the-Loop Gates (`octos_pipeline::human_gate`)

Block pipeline execution pending human approval or input.

```rust
use octos_pipeline::human_gate::{
    ChannelInputProvider, HumanInputProvider, HumanRequest, HumanInputType,
};

// Channel-based provider (for interactive UIs)
let (provider, sender) = ChannelInputProvider::new();

// Pipeline sends a request to the human
tokio::spawn(async move {
    let request = HumanRequest {
        node_id: "deploy_prod".into(),
        prompt: "Deploy to production?".into(),
        input_type: HumanInputType::Approval,
    };
    let response = provider.request_input(request).await.unwrap();
    if response.approved {
        // Continue pipeline
    }
});

// UI/CLI sends back the human's decision
use octos_pipeline::human_gate::HumanResponse;
sender.send(HumanResponse {
    approved: true,
    input: None,
}).unwrap();
```

**Input types:**

```rust
// Simple yes/no gate
HumanInputType::Approval

// Free-form text input
HumanInputType::FreeText

// Multiple choice
HumanInputType::Choice {
    options: vec!["staging".into(), "production".into()],
}
```

**Auto-approve for testing/CI:**

```rust
use octos_pipeline::human_gate::AutoApproveProvider;
let provider = AutoApproveProvider; // Always returns approved=true
```

**Timeout:** Default 5 minutes (`DEFAULT_INPUT_TIMEOUT`). Configurable via `ChannelInputProvider::with_timeout(Duration)`.

### Context Fidelity Control (`octos_pipeline::fidelity`)

Controls how much context carries over between pipeline nodes.

```rust
use octos_pipeline::fidelity::FidelityMode;

// Full context (default) — pass everything
let mode = FidelityMode::Full;

// Truncate to character limit
let mode = FidelityMode::Truncate { max_chars: 10_000 };

// Compact — keep first/last lines, strip middle
let mode = FidelityMode::Compact { keep_lines: 50 };

// Summary — keep only first N lines
let mode = FidelityMode::Summary { max_lines: 20 };

// Apply to text
let output = mode.apply("very long text from previous node...");
```

**Parsing from config strings:**

```rust
let mode = FidelityMode::parse("truncate:5000")?;   // Truncate { max_chars: 5000 }
let mode = FidelityMode::parse("compact:30")?;       // Compact { keep_lines: 30 }
let mode = FidelityMode::parse("summary:10")?;       // Summary { max_lines: 10 }
let mode = FidelityMode::parse("full")?;             // Full
```

**Safety limits:** `max_chars` capped at 10MB, `max_lines` capped at 100K.

### Pipeline Manager (`octos_pipeline::manager`)

Supervisor pattern for orchestrating child pipelines.

```rust
use octos_pipeline::manager::{PipelineManager, SupervisionStrategy, ChildSpec};

// Define child pipelines
let children = vec![
    ChildSpec {
        name: "lint".into(),
        pipeline: "digraph { lint -> report }".into(),
        input: "Check src/".into(),
        working_dir: None,
    },
    ChildSpec {
        name: "test".into(),
        pipeline: "digraph { test -> report }".into(),
        input: "Run tests".into(),
        working_dir: None,
    },
];

// All-or-nothing: fail on first child failure
let mgr = PipelineManager::new(SupervisionStrategy::AllOrNothing, executor);
let outcome = mgr.run(children.clone()).await?;

// Best effort: continue even if some fail
let mgr = PipelineManager::new(SupervisionStrategy::BestEffort, executor);
let outcome = mgr.run(children.clone()).await?;
assert!(outcome.success); // Always true for BestEffort

// Retry failed: retry with exponential backoff
let mgr = PipelineManager::new(
    SupervisionStrategy::RetryFailed { max_retries: 3 },
    executor,
);
let outcome = mgr.run(children).await?;
```

**Retry behavior:** Exponential backoff starting at 100ms, doubling each attempt, capped at 5s. `max_retries` capped at 10 (prevents DoS from unbounded configs).

**Converting to pipeline node outcomes:**

```rust
let node_outcome = outcome.to_node_outcome("ci_gate");
// node_outcome.status == OutcomeStatus::Pass or OutcomeStatus::Fail
// node_outcome.content contains "[pass] lint: ok\n[fail] test: error..."
```

### Thread Registry (`octos_pipeline::thread`)

Reuse LLM conversation sessions across pipeline nodes.

```rust
use octos_pipeline::thread::{Thread, ThreadRegistry};
use octos_core::Message;

let registry = ThreadRegistry::new();

// Create a thread for a pipeline conversation
let thread_id = "code-review-session";
registry.create(thread_id, "claude-sonnet-4-20250514")?;

// Append messages as the conversation progresses
registry.append(thread_id, Message::user("Review this PR"))?;
registry.append(thread_id, Message::assistant("I'll review the changes..."))?;

// Later nodes can read the thread's history
let thread = registry.get(thread_id).unwrap();
let messages = thread.messages(); // Full conversation history
let model = thread.model_id();   // "claude-sonnet-4-20250514"
```

**Limits:** Max 1,000 threads per registry, max 10,000 messages per thread.

### Pipeline Server (`octos_pipeline::server`)

HTTP interface for submitting and monitoring pipeline runs.

```rust
use octos_pipeline::server::{PipelineServer, SubmitRequest, RunStatus};

// Submit a pipeline run
let request = SubmitRequest {
    pipeline_id: "my-ci-pipeline".into(),
    dot_source: "digraph { build -> test -> deploy }".into(),
    input: "Build and deploy v2.1".into(),
    variables: Default::default(),
};

// Validate before processing
request.validate()?; // Checks size limits, path traversal, etc.

// Server trait for implementing your HTTP handler
#[async_trait]
impl PipelineServer for MyServer {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResponse> { /* ... */ }
    async fn status(&self, run_id: &str) -> Result<RunStatus> { /* ... */ }
    async fn cancel(&self, run_id: &str) -> Result<()> { /* ... */ }
}
```

**Validation limits:**
- DOT source: max 1MB
- Input text: max 256KB
- Variables: max 64 entries
- Pipeline ID: alphanumeric, hyphens, underscores, dots only (no path traversal)

**Run status lifecycle:** `Queued -> Running -> Completed | Failed | Cancelled`

---

## Integration Patterns

### Error-Aware Retry with Middleware

Combine `LlmError` classification with `MiddlewareStack` for intelligent retry:

```rust
let tracker = Arc::new(CostTracker::new());
let stack = MiddlewareStack::new(provider)
    .with(Arc::new(LoggingMiddleware))
    .with(tracker.clone());

let client = LlmClient::new(Arc::new(stack));

loop {
    match client.generate("summarize this").await {
        Ok(text) => break text,
        Err(e) => {
            if let Some(llm_err) = e.downcast_ref::<LlmError>() {
                if !llm_err.is_retryable() {
                    return Err(e);
                }
                // Retryable — back off
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                return Err(e);
            }
        }
    }
}
```

### Model Selection from Catalog

Use the catalog to pick the right model for the task:

```rust
let catalog = ModelCatalog::with_defaults();

// Pick cheapest model with tool use
let model = catalog
    .with_capability(|c| c.tool_use)
    .into_iter()
    .min_by(|a, b| a.cost.input_per_mtok.partial_cmp(&b.cost.input_per_mtok).unwrap())
    .unwrap();

// Instantiate provider with selected model
let provider = create_provider(&model.provider, &model.id)?;
let client = LlmClient::new(provider);
```

### Pipeline with Human Gates and Fidelity Control

```rust
// Node 1: Generate code (full output)
// Node 2: Human approval gate
// Node 3: Deploy (receives truncated context from node 1)

let fidelity = FidelityMode::Truncate { max_chars: 5000 };
let (gate, sender) = ChannelInputProvider::new();

// In the pipeline executor:
let node1_output = run_code_gen_node().await?;
let truncated = fidelity.apply(&node1_output);

let approval = gate.request_input(HumanRequest {
    node_id: "approve_deploy".into(),
    prompt: format!("Deploy this?\n\n{truncated}"),
    input_type: HumanInputType::Approval,
}).await?;

if approval.approved {
    run_deploy_node(&truncated).await?;
}
```

### Child Pipeline Orchestration with Thread Reuse

```rust
let registry = ThreadRegistry::new();
let manager = PipelineManager::new(SupervisionStrategy::BestEffort, executor);

// Create a shared thread for the CI pipeline
registry.create("ci-run-42", "claude-sonnet-4-20250514")?;

// Spawn parallel children that share the thread
let outcome = manager.run(vec![
    ChildSpec { name: "lint".into(), pipeline: lint_dot, input: "lint src/".into(), working_dir: None },
    ChildSpec { name: "test".into(), pipeline: test_dot, input: "run tests".into(), working_dir: None },
]).await?;

// All child results visible in the thread
let thread = registry.get("ci-run-42").unwrap();
println!("Thread has {} messages", thread.messages().len());
```

---

## Security Considerations

- **Environment sanitization**: `ExecEnvironment` automatically strips `BLOCKED_ENV_VARS` (LD_PRELOAD, DYLD_*, etc.) from all command executions
- **Docker path validation**: Rejects paths containing `\0`, `\n`, `\r`, `:` to prevent container escape
- **Pipeline input validation**: `SubmitRequest::validate()` enforces size limits and rejects path traversal in pipeline IDs
- **Error information leakage**: `LlmError` logs raw provider response bodies at debug level only, never in user-facing error messages
- **Retry caps**: `SupervisionStrategy::RetryFailed` capped at 10 retries; `ChannelInputProvider` has a configurable timeout (default 5min) to prevent indefinite hangs
- **Thread limits**: `ThreadRegistry` caps at 1,000 threads / 10,000 messages to prevent unbounded memory growth
- **Fidelity caps**: `FidelityMode::parse()` caps `max_chars` at 10MB and `max_lines` at 100K
