# Harness Context Efficiency: octos vs pi (comparative review)

*Review date: 2026-07-11. Subjects: [pi](https://github.com/earendil-works/pi) `0.80.6` (earendil-works, TypeScript) and octos at `main@59da36916`. Token figures use octos's own heuristic (≈4 ASCII bytes/token, `octos-llm/src/context.rs:127`); pi tool-spec sizes were measured from its schemas, octos's by serializing `ToolRegistry::specs()` from a probe binary.*

## Why this review

A published observation: the same model at the same reasoning effort, run on Claude Code / Codex vs the much simpler pi harness, costs **2×+ more per task at equal quality**. The dominant variable is **how much context the harness feeds the model per round** (~3× less in pi) and how tightly the working set is managed — fewer, cheaper rounds. This document records where octos stands on the same axes, mechanism by mechanism, and the ranked remediation plan.

## 1. Fixed overhead fed to the model every round

| Component | pi (default) | octos (`octos chat`, default coding profile) |
|---|---|---|
| System prompt | ~30-line template ≈ **604 tok** (`system-prompt.ts`) | `prompts/worker.txt` + `TOOL_USE_DISCIPLINE` ≈ **615 tok** (`agent/mod.rs:52`, `agent/execution.rs:297-309`) |
| Tool schemas | **4 tools ≈ 678 tok** — read 164, bash 129, edit 288, write 98; grep/find/ls exist but are opt-in | **48 tools ≈ 9,325 tok** — 39 native (27,279 B) + 9 bundled-skill tools (10,024 B) auto-installed each chat (`chat.rs:734`). Whales: `run_pipeline` 4,055 B (6,421 B with `OCTOS_PIPELINE_IR=1`), `spawn` 3,292 B |
| Memory / project context | AGENTS.md **uncapped** verbatim (their footgun); skills metadata-only | memory block **capped 2,500 tok** (`memory_store.rs:19`) + ~470 tok fixed guidance riders (`memory_segment.rs`); `.octos/AGENTS.md` etc. **uncapped**; skills metadata-only ✓ |
| Per-turn injected extras | none | episodic recall ≤6 episodes (embedder-gated) |
| **Fixed total** | **≈1.3 K tok** | **≈10.4–13.5 K tok** |

The dominant octos cost is tool-schema emission. **RFC-0 (#1578) removed LRU tool deferral**, so `specs()` (`tools/registry.rs:661-665`) emits every enabled schema every round; the old 15-active/34-deferred behavior described in CLAUDE.md no longer exists. A `tools>25` warning exists but only logs (`loop_runner.rs:1022`).

For scale: `octos gateway` swaps in an 18 KB system prompt (≈4.5 K tok) on top of the same tool wall.

## 2. Prompt caching — the multiplier

| | pi | octos |
|---|---|---|
| Anthropic | 3 `cache_control: ephemeral` breakpoints: system prompt, **last** tool definition, **last** user/tool_result block (`anthropic-messages.ts:956-1283`); optional 1h TTL; new `defer_loading` lands late tool defs mid-transcript without invalidating the cached prefix | **none** — `octos-llm` never sets `cache_control`; `types.rs:81` only counts cached tokens if a provider volunteers them |
| OpenAI | `prompt_cache_key = sessionId`, `store:false` | nothing (OpenAI's automatic caching still helps) |
| History discipline | **never rewrites history** — frugality by bounding at the source keeps the prefix byte-stable and cache-hot | rewrites per iteration: `truncate_old_tool_results` collapses pre-turn results to 800 chars (`message_repair.rs:513-536`) — good token hygiene, but cache-hostile if caching were enabled |

At Anthropic's 0.1× cache-read pricing, an uncached 10 K-token fixed prefix replayed for ~20 iterations/turn is by itself in 2×-cost territory. **This is the single biggest lever.**

## 3. Tool-result bounding and the working set

| | pi | octos |
|---|---|---|
| read | 2,000 lines / 50 KB, head-truncate, footer teaches the continuation (`offset=N`); **no line-number gutter** | whole-file default, 100 KB internal / 50 K-char dispatch cap; `start_line`/`end_line` exist but no truncation footer teaches them |
| bash/shell | 50 KB tail-truncate; full stream spooled to a temp file the model can grep later (`OutputAccumulator`) | 30–50 K chars, 70/30 head/tail (`truncate_head_tail`, `octos-core/src/utils.rs:88-104`) |
| edit | result = one sentence; diff rides a `details` lane **stripped at the provider boundary** | comparable (previews ride UI envelopes, not model context) |
| search-class | n/a (no default web tools) | `search` / `deep_search` / `news_fetch` dispatch cap **200,000 chars ≈ 50 K tok per call** |
| grep | 100 matches / 500 chars-per-line caps + footer | 50 matches, **uncapped line length** |
| history | verbatim until compaction (cache-stable) | 800-char collapse of pre-turn results ✓; CLI chat drops all tool traffic across turns ✓ (`chat.rs:1223-1244`) |

pi's principle: **caps live in the tool; recovery lives in the result**. Every truncation footer names the exact next call (`offset=N`, temp-file path, `limit=200`), so no rounds are wasted on blind re-queries and no prompt space is spent teaching recovery up front.

## 4. Compaction

| | pi | octos |
|---|---|---|
| Trigger | `context > window − 16,384` (very late; cheap because cached) | est. tokens > ~66.7% of window (`agent/compaction.rs:25`, 0.8/1.2 safety) |
| Keeps | newest 20 K tok, cuts only at turn boundaries, never splits tool call/result | ≥6 recent messages, never splits pairs |
| Summary | LLM-generated, structured (Goal/Progress/Decisions/Next) + **cumulative read/modified file lists**; iterative UPDATE on re-compaction; split-turn prefix summaries; inputs clipped to 2 K chars/result | extractive, **zero LLM calls** (first-line ≤200 chars, results ≤100); LLM-iterative summarizer exists but is opt-in via workspace policy; tiered pruning dormant |
| Sessions | **tree** with branch summarization — a subtask branch collapses to one summary on navigation | linear + fork (`parent_key`), no summary injection |

## 5. Round-count mechanics

- pi: parallel tool execution default; **multi-edit `edits[]` in one call** (guidelines push merging nearby changes); steering queue injects mid-run user messages *between tool batches* (no abort/re-prompt); length-truncated responses fail all tool calls with an explicit re-issue error.
- octos: parallel `Safe` batches ✓ (`agent/execution.rs`), `spawn_only` backgrounding ✓, LoopDetector ✓, CLI `--max-iterations` 20; no multi-edit; nothing nudges batching.

## 6. What pi omits (the price of its frugality)

No subagents/Task tool, no permission system or sandbox (bash runs raw; isolation delegated to containers), no MCP client, no web tools by default, no plan scaffolding, no repo map, no per-file freshness tracking. octos is a multi-tenant agentic OS; the comparison holds for interactive coding tasks, not for octos's gateway/channel/pipeline surface.

## Ranked remediation for octos

1. **Prompt caching in `octos-llm`** — Anthropic breakpoints (system / last tool / last user block) + cache-usage accounting; make history edits turn-boundary-only so the prefix stays stable; sort `specs()` deterministically (a HashMap-ordered tool list busts any prefix cache by itself). → `feat/prompt-caching`
2. **Lean default tool profile** — the `tool_policy` machinery already exists; default `coding` profile allow-lists the core loop (~≤15 tools ≈ ≤3.5 K tok), `coding-full` preserves today's set. → `feat/lean-tool-profile`
3. **Schema diet** — `run_pipeline`/`spawn`/skill descriptions to 1–3 sentences; move recovery teaching into truncation footers.
4. **Cap the whales** — search-class 200 K → ~30 K chars; add continuation footers to `read_file`/`grep` teaching `start_line`/`limit`; cap grep line length.
5. **Adopt**: multi-edit `edits[]`; bash temp-file spool + footer; pi's 27-pattern provider overflow detection with compact-and-retry; branch summarization on session fork.

## Config knobs available today (no code changes)

- `tool_policy` allow/deny (+ `tools.byProvider`) — cutting 48→~10 tools saves ~7–8 K tok/round now.
- `memory.max_inject_tokens` / `OCTOS_MEMORY_MAX_INJECT_TOKENS`; `memory.refresh.enabled=false` drops the capture-policy rider + `memory_note` schema.
- Keep `.octos/AGENTS.md`/`SOUL.md` small — they are uncapped.
- Leave `OCTOS_PIPELINE_IR` unset (−2,366 B on `run_pipeline`'s schema); no embedder ⇒ no episodic-recall injection.
- `--max-iterations` caps rounds per message.
