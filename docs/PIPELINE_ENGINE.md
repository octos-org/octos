# Pipeline Engine Architecture

> How deep research and DOT-based pipelines work end-to-end.

## Overview

The pipeline engine executes multi-step research workflows defined as DOT graphs.
The **session-level LLM** is the architect — it designs the pipeline, picks models,
writes prompts, and assigns tools for each node. Sub-agents execute their assigned
tasks with no knowledge of the broader pipeline.

```
User: "深度研究 AI 芯片出口管制"
         │
         ▼
┌─────────────────────────────┐
│   Session Agent (gateway)   │  ← Has all tools (run_pipeline, deep_search, etc.)
│   Model: adaptive router    │  ← Hedge/lane across 8 providers
│   System: worker.txt +      │
│           deferred tools     │
│                             │
│   LLM decides: "I need a   │
│   research pipeline"        │
│         │                   │
│   Generates inline DOT:     │
│   digraph research {        │
│     plan_and_search [...]   │
│     analyze [...]           │
│     synthesize [...]        │
│   }                         │
│         │                   │
│   Calls: run_pipeline(DOT)  │
└─────────┬───────────────────┘
          │
          ▼
┌─────────────────────────────┐
│   Pipeline Executor         │
│   Parses DOT → PipelineGraph│
│   Walks nodes sequentially  │
│   Handles parallel fan-out  │
└─────────┬───────────────────┘
          │
          ├──────────────────────────────────────┐
          ▼                                      ▼
┌──────────────────────┐              ┌──────────────────────┐
│ plan_and_search node │              │                      │
│ handler=dynamic_parallel            │                      │
│                      │              │                      │
│ 1. Planner LLM call  │              │                      │
│    "Generate 6       │              │                      │
│     search angles"   │              │                      │
│         │            │              │                      │
│ 2. Spawns N workers: │              │                      │
│    ┌─────┬─────┐     │              │                      │
│    │ W0  │ W1  │ ... │              │                      │
│    └──┬──┴──┬──┘     │              │                      │
│       │     │        │              │                      │
│ 3. Merge results     │              │                      │
│    → converge node   │              │                      │
└──────────┬───────────┘              │                      │
           │                          │                      │
           ▼                          │                      │
┌──────────────────────┐              │                      │
│ analyze node         │              │                      │
│ handler=codergen     │              │                      │
│ prompt="Cross-ref    │              │                      │
│   findings..."       │              │                      │
│ model=qwen3.5-plus   │              │                      │
│ tools=read_file      │              │                      │
└──────────┬───────────┘              │                      │
           │                          │                      │
           ▼                          │                      │
┌──────────────────────┐              │                      │
│ synthesize node      │              │                      │
│ handler=codergen     │              │                      │
│ prompt="Write report │              │                      │
│   with citations..." │              │                      │
│ model=gemini-3-flash │              │                      │
│ tools=write_file     │              │                      │
│ goal_gate=true       │              │                      │
└──────────────────────┘              │                      │
                                      │                      │
```

## Two-Tier Agent Architecture

### Tier 1: Session Agent (the architect)

- **Created by**: gateway/session_actor per user session
- **LLM provider**: AdaptiveRouter (hedge/lane/failover across all configured providers)
- **System prompt**: worker.txt + deferred tools listing
- **Tools**: ~12-15 active (core tools), rest deferred with LRU auto-eviction
- **Role**: Understand user intent, decide whether to use a tool directly or design a pipeline

The session agent sees the full model catalog in the `run_pipeline` tool schema:
```
Available models (use model="key" in DOT nodes):
- 'deepseek-chat': deepseek-chat (deepseek), 8k output, 64k context, $0.14/1M in
- 'gemini-3-flash-preview': gemini-3-flash-preview (gemini), 65k output, 1000k context
- 'kimi-k2.5': kimi-k2.5 (moonshot), 65k output, 128k context
- 'glm-5-turbo': glm-5-turbo (zai), 131k output, 200k context
```

It uses this catalog to make model selection decisions per node.

### Tier 2: Pipeline Sub-Agents (the workers)

- **Created by**: CodergenHandler, one per pipeline node execution
- **LLM provider**: Resolved per-node via ProviderRouter + FallbackProvider
- **System prompt**: The node's `prompt` attribute from the DOT — written by the session LLM
- **Tools**: Only what the DOT specifies (e.g., `tools="deep_search,read_file"`)
- **Role**: Execute a single focused task (search, analyze, or synthesize)

Sub-agents have NO knowledge of:
- Other pipeline nodes
- The session agent's conversation history
- The full tool catalog
- The adaptive router or queue modes

They are purpose-built, disposable workers.

## Pipeline Node Types

| Handler | What it does | LLM calls? |
|---------|-------------|:----------:|
| `codergen` | Full agent loop (LLM + tools) | Yes |
| `dynamic_parallel` | LLM plans N tasks, executes in parallel, merges | Yes (planner + N workers) |
| `parallel` | Fan-out to all targets, merge at converge node | Via sub-handlers |
| `shell` | Run a shell command | No |
| `gate` | Evaluate a condition expression | No |
| `noop` | Pass-through | No |

## Provider Resolution

```
Node with model="X"                    Node without model=
        │                                      │
        ▼                                      ▼
  ProviderRouter::resolve("X")          default_provider
        │                               (= AdaptiveRouter)
        ▼                                      │
  FallbackProvider                             ▼
  ┌─────────────────────┐           Full hedge/lane/failover
  │ Primary: X          │           across all 8 providers
  │ Fallback 1: Y       │
  │ Fallback 2: Z       │
  │ (compatible output)  │
  └─────────────────────┘
```

When a node specifies `model="deepseek-chat"`, the system:
1. Resolves via ProviderRouter (exact key match, then prefix-split)
2. Wraps with FallbackProvider — compatible models (≥ same max_output_tokens) as fallbacks
3. If primary times out/errors, fallback providers are tried automatically

When a node omits `model=`, it uses the session's AdaptiveRouter with full protection.

## Dynamic Parallel Execution

The `dynamic_parallel` handler is the core of deep research:

```
1. PLAN: Planner LLM generates N research angles as JSON array
   ┌──────────────────────────────────────────┐
   │ System: "You are a research planner.     │
   │          Output ONLY a JSON array."      │
   │ User: "{node prompt}\n\nUser query: ..." │
   │                                          │
   │ Response: [                              │
   │   {"task": "...", "label": "..."},       │
   │   {"task": "...", "label": "..."},       │
   │   ...                                    │
   │ ]                                        │
   └──────────────────────────────────────────┘
   If planner fails → 3 generic fallback tasks

2. EXECUTE: N workers run in parallel (tokio::spawn + join_all)
   Each worker = CodergenHandler with:
   - prompt: worker_prompt template with {task} replaced
   - model: node's model key
   - tools: node's tool allowlist

3. MERGE: All worker outputs concatenated with headers
   "--- Worker 0: {label} ---\n{output}\n..."

4. CONVERGE: Merged output fed as input to the converge node
```

## Tool Lifecycle (LRU Auto-Eviction)

The session agent manages tools with an LRU cache:

```
┌─────────────────────────────────────────────┐
│              Tool Registry                   │
│                                             │
│  Active (sent to LLM as tool specs):        │
│  ┌─────────────────────────────────────┐    │
│  │ BASE (never evicted):               │    │
│  │   run_pipeline, deep_search,        │    │
│  │   read_file, write_file, shell,     │    │
│  │   glob, grep, list_dir, ...         │    │
│  ├─────────────────────────────────────┤    │
│  │ DYNAMIC (evicted when idle):        │    │
│  │   save_memory (last used: iter 5)   │    │
│  │   recall_memory (last used: iter 5) │    │
│  │   web_search (last used: iter 2) ←── candidate for eviction
│  └─────────────────────────────────────┘    │
│                                             │
│  Deferred (name listed in system prompt):   │
│  ┌─────────────────────────────────────┐    │
│  │   browser, manage_skills, spawn,    │    │
│  │   configure_tool, switch_model      │    │
│  └─────────────────────────────────────┘    │
│                                             │
│  Eviction rule:                             │
│  IF active_count > 15                       │
│  AND tool idle for 5+ iterations            │
│  AND tool not in base_tools                 │
│  THEN move to deferred (stalest first)      │
│                                             │
│  Re-activation:                             │
│  LLM calls activate_tools({"tools": [...]}) │
│  → resolves tool name to group              │
│  → activates entire group                   │
└─────────────────────────────────────────────┘
```

Pipeline sub-agents do NOT use LRU — they get a fixed, minimal tool set
from the DOT and run for a single task.

## Resilience Layers

| Layer | Protects against | Where |
|-------|-----------------|-------|
| **AdaptiveRouter** (hedge/lane/failover) | Provider outages, slow responses | Session agent (default_provider) |
| **FallbackProvider** | Explicit model timeout/error in pipeline nodes | Per-node in CodergenHandler |
| **RetryProvider** | Transient 429/500/503 errors | Wraps each raw provider |
| **30s stream chunk timeout** | Stalled SSE connections | Agent stream consumption loop |
| **Per-node wall-clock timeout** | Runaway agent loops | AgentConfig.max_timeout |
| **Pipeline-level timeout** | Entire pipeline hangs | tokio::time::timeout in RunPipelineTool |
| **Circuit breaker** | Repeatedly failing providers | AdaptiveRouter metrics |
| **Timeout → failover (no retry)** | Dead API burning retries | RetryProvider.is_retryable_error |

## File Reference

| Component | File | Key function |
|-----------|------|-------------|
| RunPipelineTool | `octos-pipeline/src/tool.rs` | `execute()`, `build_model_catalog()` |
| Pipeline executor | `octos-pipeline/src/executor.rs` | `run()`, `execute_graph()`, `plan_dynamic_tasks()` |
| CodergenHandler | `octos-pipeline/src/handler.rs` | `resolve_provider()`, `execute()` |
| DOT parser | `octos-pipeline/src/parser.rs` | `parse_dot()` |
| FallbackProvider | `octos-llm/src/fallback.rs` | `chat()` with fallback chain |
| AdaptiveRouter | `octos-llm/src/adaptive.rs` | `chat()` with hedge/lane/failover |
| ProviderRouter | `octos-llm/src/router.rs` | `resolve()`, `compatible_fallbacks()` |
| ToolLifecycle (LRU) | `octos-agent/src/tools/mod.rs` | `tick()`, `auto_evict()`, `find_evictable()` |
| Session actor | `octos-cli/src/session_actor.rs` | `process_inbound_speculative()` |
| Stream timeout | `octos-agent/src/agent/streaming.rs` | `consume_stream()` with CHUNK_TIMEOUT |
