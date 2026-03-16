# Attractor Spec Gap Analysis

Audit of octos against [strongdm/attractor](https://github.com/strongdm/attractor) NLSpecs.
Date: 2026-03-06

## Spec-to-Crate Mapping

| Attractor Spec | octos Crate | Coverage |
|---|---|---|
| unified-llm-spec | `octos-llm` | ~60% |
| coding-agent-loop-spec | `octos-agent` + `octos-bus` | ~65% |
| attractor-spec (pipeline) | `octos-pipeline` | ~55% |

---

## 1. Unified LLM Client (`octos-llm`)

### Implemented

- `LlmProvider` trait: `chat()`, `chat_stream()`, `model_id()`, `context_window()`
- Native Anthropic API (`/v1/messages`)
- Native Gemini API (`/v1beta/models/*/generateContent`)
- SSE parsing with 1MB buffer limit
- Retry + failover: exponential backoff, circuit breaker, `ProviderChain`
- Streaming: `TextDelta`, `ReasoningDelta`, `ToolCallDelta`, `Usage`, `Done`, `Error`
- Tool calling: `ToolSpec` with JSON Schema, `ToolChoice` (Auto/Required/None/Specific)
- 14 providers with registry auto-detection
- Adaptive routing: latency EMA, p95, error rates, circuit breaker, probe requests
- Vision: base64 image encoding for multimodal
- Context window + pricing lookup tables

### Gaps

| ID | Gap | Impact | Effort | Phase |
|---|---|---|---|---|
| L1 | OpenAI uses Chat Completions, not Responses API (`/v1/responses`) | Missing reasoning token breakdowns, streaming structured output | M | 2 |
| L2 | No Anthropic prompt caching (`cache_control` annotations) | ~50% cost savings lost on long contexts | L | 1 |
| L3 | No structured output (`response_format` in ChatConfig) | Can't request JSON schema-validated responses | M | 2 |
| L4 | Incomplete `TokenUsage` — missing `reasoning_tokens`, `cache_read/write_tokens` | Inaccurate cost tracking | L | 1 |
| L5 | No high-level APIs (`generate()`, `generate_object()`, `stream()`) | Agent must implement own tool loop | M | 3 |
| L6 | No middleware/interceptor pattern | Can't inject logging, cost tracking, rate limiting | M | 3 |
| L7 | No Gemini caching or thinking (`cachedContent`, `thinkingConfig`) | Missing cost optimization + reasoning support | L-M | 2 |
| L8 | No Anthropic extended thinking (`thinking` param, thinking blocks) | Can't control Claude thinking budget | L | 1 |
| L9 | Old Anthropic API version (`2023-06-01` vs `2024-06-15+`) | May miss newer features | Trivial | 1 |
| L10 | No typed error hierarchy — uses `eyre::Report` with string matching | Poor error handling ergonomics | M | 3 |
| L11 | No `ChatResponse.id`, `.model`, `.raw`, `.warnings` | Can't trace responses or debug | L | 1 |
| L12 | No `StreamAccumulator` | Each provider rebuilds response independently | L | 2 |
| L13 | No model catalog (`ModelInfo` with capabilities, costs, aliases) | No programmatic model discovery | M | 3 |

---

## 2. Coding Agent Loop (`octos-agent`)

### Implemented

- Core agent loop: budget checks -> build messages -> call LLM -> execute tools -> loop
- Parallel tool execution: `tokio::spawn` per tool with configurable timeout
- Subagent spawning: `SpawnTool` with independent history, custom model/prompt
- Context compaction: token-aware summarization (`compaction.rs`)
- Hooks: before/after tool/LLM with deny capability, circuit breaker
- Sandbox: Linux bwrap, macOS sandbox-exec, Docker
- Tool policy: allow/deny lists with provider-specific overrides
- Session persistence: JSONL with LRU cache, atomic write, forking
- Episodic memory: hybrid BM25+vector search
- Streaming: real-time token tracking via `ProgressEvent`
- Graceful shutdown: `AtomicBool` flag, tool timeout enforcement
- Env var sanitization: `BLOCKED_ENV_VARS` (18 vars)

### Gaps

| ID | Gap | Impact | Effort | Phase |
|---|---|---|---|---|
| A1 | No loop detection — no tool call signature tracking | Agent can spin infinitely | M | 1 |
| A2 | No steering/follow-up messages — can't inject mid-session | No mid-task redirection | M | 2 |
| A3 | No provider-aligned toolsets (apply_patch for OpenAI, read_many_files for Gemini) | Suboptimal tool usage per provider | H | 3 |
| A4 | No session state machine (IDLE/PROCESSING/AWAITING_INPUT/CLOSED) | No formal lifecycle | L | 2 |
| A5 | No session config object (max_turns, max_tool_rounds, per-tool limits) | Limited loop configurability | M | 2 |
| A6 | No per-tool output truncation limits (spec: read_file 50K, shell 30K) | Only global truncation, no head/tail split | M | 1 |
| A7 | No system prompt layering — string concatenation, not modular | No AGENTS.md/CLAUDE.md auto-discovery | M | 2 |
| A8 | No reasoning effort control (low/medium/high) | Can't tune thinking depth | L | 1 |
| A9 | No execution environment abstraction | Can't swap local/Docker/K8s without modifying tools | H | 3 |
| A10 | No turn type semantics — only Message with role | No distinction between steering/tool/user turns | M | 3 |
| A11 | Basic event system — callback, not publish-subscribe | No session events, no filtering | M | 2 |
| A12 | No SIGTERM->wait->SIGKILL protocol — relies on tokio timeout | Processes may not clean up | L | 2 |

---

## 3. Pipeline Orchestration (`octos-pipeline`)

### Implemented

- DOT parsing: digraph, attributes, chained edges, comments
- 5-phase lifecycle: Parse -> Validate -> Initialize -> Execute -> Finalize
- 5-step edge selection: condition -> preferred_label -> suggested_next -> weight -> lexical
- Goal gate enforcement: `goal_gate=true` nodes must pass
- 6 handlers: Codergen, Shell, Gate, Noop, Parallel, DynamicParallel
- Condition language: `outcome.status == "pass"`, `&&`, `||`, `!`, parens
- 14 lint rules: start node, reachability, edge targets, conditions, parallel converge
- Retry with exponential backoff
- Variable expansion: `{variable}` substitution
- Pipeline-as-tool: `RunPipelineTool` integration

### Gaps

| ID | Gap | Impact | Effort | Phase |
|---|---|---|---|---|
| P1 | No checkpoint save/resume — no crash recovery | Must restart on failure | H | 2 |
| P2 | No human-in-the-loop (Interviewer pattern) | Can't pause for human decisions | H | 3 |
| P3 | No model stylesheet (CSS-like LLM config) | Must set model per-node | M | 2 |
| P4 | No fidelity modes (full/truncate/compact/summary:*) | No context carryover control | H | 3 |
| P5 | No manager loop handler (supervisor pattern) | Can't orchestrate child pipelines | H | 3 |
| P6 | No artifact store (memory/disk by size) | No large output storage | M | 2 |
| P7 | No run directory (`{logs_root}/{node_id}/status.json`) | No audit trail | M | 2 |
| P8 | No subgraph support — no class derivation | Can't scope defaults to node groups | M | 2 |
| P9 | No shape-to-handler mapping (Mdiamond, Msquare, etc.) | Explicit `handler=` required | L | 1 |
| P10 | No thread resolution for LLM session reuse | Every node starts fresh | M | 3 |
| P11 | No `context.*` variables in conditions | Can only check outcome status | L | 1 |
| P12 | No observability events — only tracing logs | No structured lifecycle events | M | 2 |
| P13 | No HTTP server mode for pipeline management | Can't submit/cancel via API | H | 3 |
| P14 | No typed values in DOT — all strings | No Duration, Boolean, Integer parsing | L | 1 |

---

## Implementation Plan

### Phase 1: Quick Wins (Low effort, high value)

**Goal**: Cost savings, safety, and correctness improvements.

| Task | Gap IDs | Crate | Est. Lines |
|---|---|---|---|
| Add `cache_control` annotations for Anthropic | L2 | octos-llm | ~80 |
| Complete `TokenUsage` (reasoning, cache read/write) | L4 | octos-llm | ~60 |
| Add Anthropic extended thinking support | L8 | octos-llm | ~120 |
| Update Anthropic API version header | L9 | octos-llm | ~5 |
| Add response metadata to `ChatResponse` (id, model, raw) | L11 | octos-llm | ~50 |
| Implement loop detection (tool call signature tracking) | A1 | octos-agent | ~200 |
| Per-tool output truncation with head/tail split | A6 | octos-agent | ~150 |
| Reasoning effort control (low/medium/high config) | A8 | octos-llm + agent | ~80 |
| Shape-to-handler mapping in DOT parser | P9 | octos-pipeline | ~40 |
| `context.*` variables in condition evaluator | P11 | octos-pipeline | ~60 |
| Typed value parsing in DOT (Duration, Bool, Int) | P14 | octos-pipeline | ~80 |

**Estimated total**: ~925 lines, 1-2 weeks

### Phase 2: Core Architecture (Medium effort)

**Goal**: Resilience, configurability, and developer experience.

| Task | Gap IDs | Crate | Est. Lines |
|---|---|---|---|
| Migrate OpenAI to Responses API | L1 | octos-llm | ~400 |
| Add `response_format` / structured output | L3 | octos-llm | ~300 |
| Gemini caching + thinkingConfig | L7 | octos-llm | ~150 |
| StreamAccumulator utility | L12 | octos-llm | ~100 |
| Steering/follow-up message queues | A2 | octos-agent | ~250 |
| Session state machine (IDLE/PROCESSING/etc.) | A4 | octos-agent | ~150 |
| Session config object (max_turns, tool_rounds) | A5 | octos-agent | ~200 |
| System prompt layering + AGENTS.md discovery | A7 | octos-agent | ~250 |
| Event system upgrade (publish-subscribe) | A11 | octos-agent | ~300 |
| SIGTERM->wait->SIGKILL for shell processes | A12 | octos-agent | ~100 |
| Pipeline checkpoint save/resume | P1 | octos-pipeline | ~500 |
| Model stylesheet (CSS-like config) | P3 | octos-pipeline | ~350 |
| Artifact store (memory + disk backing) | P6 | octos-pipeline | ~250 |
| Run directory with status.json per node | P7 | octos-pipeline | ~200 |
| Subgraph parsing and class derivation | P8 | octos-pipeline | ~200 |
| Pipeline observability events | P12 | octos-pipeline | ~250 |

**Estimated total**: ~3,950 lines, 4-6 weeks

### Phase 3: Advanced Features (High effort, architecture changes)

**Goal**: Full spec compliance and extensibility.

| Task | Gap IDs | Crate | Est. Lines |
|---|---|---|---|
| High-level APIs (generate, generate_object, stream) | L5 | octos-llm | ~600 |
| Middleware/interceptor pipeline | L6 | octos-llm | ~400 |
| Typed error hierarchy | L10 | octos-llm | ~300 |
| Model catalog (ModelInfo with metadata) | L13 | octos-llm | ~250 |
| Provider-aligned toolsets (apply_patch, read_many_files) | A3 | octos-agent | ~800 |
| Execution environment abstraction | A9 | octos-agent | ~600 |
| Turn type semantics | A10 | octos-agent/core | ~200 |
| Human-in-the-loop (Interviewer pattern) | P2 | octos-pipeline | ~500 |
| Fidelity modes for context carryover | P4 | octos-pipeline | ~400 |
| Manager loop handler (child pipeline supervision) | P5 | octos-pipeline | ~500 |
| Thread resolution for session reuse | P10 | octos-pipeline | ~250 |
| HTTP server for pipeline management | P13 | octos-pipeline | ~600 |

**Estimated total**: ~5,400 lines, 6-8 weeks

---

## Summary

| Phase | Items | Lines | Focus |
|---|---|---|---|
| Phase 1 | 11 tasks | ~925 | Cost savings, safety, quick correctness |
| Phase 2 | 16 tasks | ~3,950 | Resilience, configurability, DX |
| Phase 3 | 12 tasks | ~5,400 | Full spec compliance, extensibility |
| **Total** | **39 tasks** | **~10,275** | |

### Already Strong (No Action Needed)

- Multi-provider support (14 providers with auto-detection)
- Adaptive routing with metrics-driven selection
- Sandbox isolation (3 backends)
- Hook system with circuit breaker
- Session persistence with forking
- Hybrid memory search (BM25 + vector)
- Tool policy system with deny-wins semantics
- DOT pipeline parsing and execution engine
- Parallel + DynamicParallel handlers
