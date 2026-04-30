# Octos Harness Audit: M6-M9 vs requirements at `origin/main` 119bf782

Date: 2026-04-30
Baseline: `origin/main` at `119bf782` (post PR #687/#688/#691/#692/#695/#696)
Method: 4 parallel agent audits (one per milestone) + codex 2nd-opinion review per agent
Source reports: `/tmp/m{6,7,8,9}-audit.md` per agent

## Purpose

Formal score-based check of M6-M9 implementation against the requirements at
[`OCTOS_HARNESS_ENGINEERING_REQUIREMENTS_M6_M9.md`](./OCTOS_HARNESS_ENGINEERING_REQUIREMENTS_M6_M9.md).

The user's hypothesis going in: *"most web UX bugs trace to incomplete or
ad-hoc harness engineering across M6-M9."* The audit confirms the hypothesis
and refines it — the gaps cluster into 5 cross-cutting patterns, not random
defects.

## Scoring rubric (recap)

| Score | Meaning |
| ----- | ------- |
| 0 | Not implemented |
| 1 | Type or schema only |
| 2 | Fixture/demo path |
| 3 | Production path wired |
| 4 | Durable under reconnect/restart/backpressure |
| 5 | Fully observable, controllable, tested, client-rendered |

Server runtime requires ≥ 4 to ship. Coding UX requires 5.

## Cross-milestone scorecard

| Milestone | Overall | Headline gap |
| --- | --- | --- |
| **M6** Harness Contract Foundation | mixed (Req 1 = 5; Req 4 = 2) | warning-only validators + skill manifests parsed-but-not-enforced |
| **M7** Swarm + External Agent Execution | **3.25 / 5** | **policy bypass on MCP/CLI backends** + cost ledger noop |
| **M8** Runtime Lifecycle + Observability | **3.8 / 5** | **cancel-vs-late-completion race violates DoD** + MCP/plugin concurrency-class bypass |
| **M9** AppUI Protocol + Coding UX | server 4/7 ≥ 4; UX 2-4 / 5 | `task/cancel` not in `UiCommand`; `/ps` not in TUI; diff preview cwd not session-scoped |

## Per-milestone summary

### M6 — Harness Contract Foundation

| Req | Score | Note |
| --- | --- | --- |
| 1 — Versioned event schemas | 5/5 | All 9 event types (task, phase, artifact, validator, retry, failure, cost, routing, dispatch) production-wired with `check_supported()` validation. |
| 2 — Workspace contract rules | **3/5** | Validators block spawn-task delivery, but **preservation validator only warns** instead of blocking terminal session success. No holistic integration test. |
| 3 — ABI/schema versioning policy | (in full report) | |
| 4 — Skill compatibility gates | **2/5** | Manifest fields (risk, env, spawn_only) **parsed but never enforced at runtime** for non-spawn_only fields. No wire-time skill rejection gate. |
| 5 — Deterministic fixtures | (in full report) | |
| 6 — Compaction preservation | **3/5** | Tool-result placeholders + unresolved-call filtering work, but preservation validator is warning-only and `content_hash` TODO from M8.4 isn't done. |

### M7 — Swarm + External Agent Execution

| Req | Score | Note |
| --- | --- | --- |
| 4 — Bounded parallel/sequential/fanout/pipeline | 5/5 | Topology solid. |
| 5 — Persist dispatch state | 5/5 | redb-backed; restart resumes correctly. |
| 8 — Swarm events → harness observability | 5/5 | Wired. |
| **6 — Cost + provenance attribution** | **2/5** | **Default `NoopCostLedger`** — no durable records, no budget enforcement, no provenance audit trail (no approval-decision links, no cost-gate decision logs). |
| **7 — Policy parity (sandbox/env/SSRF/tool-policy across MCP+CLI+native)** | **2/5** | **MCP HTTP backend bypasses sandbox, tool policy, env allowlist, approval gates.** Native dispatch enforces all four; swarm has no uniform pre-dispatch gate. |

### M8 — Runtime Lifecycle + Observability

| Req | Score | Note |
| --- | --- | --- |
| 1, 2, 3, 5, 6, 7, 9 — IDs, lifecycle, restart-recovery, terminal durability, etc. | 5/5 | Solid (M8 fix-first work + PRs #695, #696). |
| 8 — Structured task output | 4/5 | Snapshot projection only; no live-tail. Documented limitation. |
| **4 — Cancellation prevents late-completion overwrite** | **2/5** | **`mark_completed()` at `task_supervisor.rs:1537` has NO guard preventing a late worker from overwriting `Cancelled` → `Completed`.** Directly violates the DoD gate the requirements doc literally calls out. |
| **10 — Tool concurrency classes prevent unsafe parallel writes** | **3/5** | Native tools enforce Exclusive/Safe correctly. **MCP and plugin tools default to `Safe` regardless of file-mutation intent.** |

### M9 — AppUI Protocol + Coding UX

| Req | Score | Note |
| --- | --- | --- |
| 3 — Durable ledger ordering (no stale-replay overwrite) | ✓ verified clean | Write-ordering invariant is sound. |
| 4 — Backpressure preserves terminal/approvals/decisions | 4/5 | Terminal task states fixed (PR #696). Other event types require equivalent verification. |
| 1, 2, 5 — Protocol surface + approval UX | 4-5 | Solid. |
| 7 — Task output cursorable read | partial | `task/output/read` doesn't declare `is_snapshot_projection: bool` field; clients can't tell if cursor semantics are real. |
| **6 — Diff preview cwd-resolved, durable** | server **partial** | PR #698 added durability. **Path resolution NOT against session cwd** — will break post-reconnect when session context shifts. |
| **8 — Render parity (markdown, task cards, status rows, …)** | **4/5** | TUI renders components; app parity unverified. Coding UX requires 5. |
| **9 — Slash/control surfaces map to real protocol commands** | **3/5** | `/stop` + approvals wired. **`task/cancel` not in `UiCommand` enum. `/ps` not implemented in TUI.** Forces clients to REST fallback or UI hack. |
| 10 — UX parity vs Codex/Claude long sessions | **2/5** | Runtime real, parity unconfirmed. |

## Cross-cutting patterns

The 4 audits found **the same defects expressed at different layers**.

### Pattern 1 — "MCP/CLI/plugin paths bypass enforcement that native tools apply"

- **M6 req 4:** skill manifest fields parsed but never enforced
- **M7 req 7:** MCP HTTP backend bypasses sandbox, tool policy, env allowlist, approval
- **M8 req 10:** MCP and plugin tools default to `Safe` concurrency class regardless of mutation intent

→ **Native execution paths are well-policed. Anything not native escapes the gates.**

### Pattern 2 — "Validators warn instead of block"

- **M6 req 2:** preservation validator warns rather than failing the spawn-task contract
- **M6 req 6:** compaction preservation enforcement is warning-only
- **M7 req 6:** cost ledger defaults to `NoopCostLedger` (silently doesn't track)

→ Type system says "I checked"; runtime says "I'd let it through anyway."

### Pattern 3 — "Race conditions and write-ordering"

- **M8 req 4:** `mark_completed()` has no guard against late-worker overwriting `Cancelled` → `Completed`. Directly violates the explicit DoD gate.
- **M9 req 3:** ✓ verified clean — durable ledger correctly prevents stale disk replay from clobbering live events.

→ Lifecycle racing is real (M8); ledger ordering is sound (M9). The good engineering pattern exists; it just hasn't propagated to all lifecycle paths.

### Pattern 4 — "Protocol incomplete on the control side"

- **M9 req 9:** `task/cancel` not in `UiCommand` enum. `/ps` not implemented. `/stop` works.
- **M6 req 4:** no wire-time skill compatibility gate
- **M9 req 7:** `task/output/read` doesn't declare `is_snapshot_projection: bool`

→ Read-side surfaces are richer than control-side surfaces.

### Pattern 5 — "Path resolution against the wrong root"

- **M9 req 6:** diff preview paths NOT resolved against session cwd
- **M6 req 2:** workspace contract doesn't enforce same root semantics across nodes

## Priority-ordered fix list

| # | Title | Severity | Pattern | Issue |
| --- | --- | --- | --- | --- |
| **P0** | M8 req 4 — guard `mark_completed()` against `Cancelled → Completed` overwrite | DoD gate violation; race exists today | 3 | TBD |
| **P0** | M7 req 7 — route MCP/CLI swarm dispatch through approval + tool policy + sandbox + env-allowlist gates | Security: MCP can bypass policy | 1 | TBD |
| **P1** | M6 req 4 — enforce skill manifest fields at runtime (or fail-closed at registration) | Roots Pattern 1 | 1 | TBD |
| **P1** | M8 req 10 — extend `Exclusive` concurrency class to MCP/plugin file-mutating tools | Same root as P0+P1 above | 1 | TBD |
| **P2** | M9 req 9 — add `task/cancel` to `UiCommand`; add `/ps` slash command | Required for UX score 5 | 4 | TBD |
| **P2** | M9 req 6 — thread session_cwd into diff preview path resolution | Real bug at reconnect time | 5 | TBD |
| **P2** | M6 req 2 + 6 — convert warning-only validators to fail-closed; add holistic integration test | Roots Pattern 2 | 2 | TBD |
| **P3** | M9 req 7 — add `is_snapshot_projection: bool` to `task/output/read` result + UPCR | Protocol clarity for clients | 4 | TBD |
| **P3** | M7 req 6 — wire durable cost ledger (replace `NoopCostLedger`) + provenance audit trail | Observability + provenance | 2 | TBD |

P0 + P1 in flight as parallel fix PRs (this session).

## Recommended next steps

1. **Land P0 + P1 fixes** (4 PRs in flight) and redeploy to fleet — closes the most dangerous correctness gaps (cancel race + MCP policy bypass + manifest enforcement + concurrency parity).
2. **Triage P2/P3 by impact** — file as standalone issues (one per row above), prioritize per quarter.
3. **Add a periodic re-audit gate** — re-run the per-milestone audits before each milestone cut to catch regression.
4. **Add Global Quality Bar checks to PR review template** — the 7 gates (Contract/Runtime/Durability/Observability/Control/Safety/Tests) should be explicitly answered for any new feature.

## Method notes

- Each per-milestone agent ran `Explore` against `origin/main` 119bf782, scored each requirement 0-5 with file:line citations, then ran codex 2nd-opinion to challenge score inflation.
- Codex 2nd-opinion specifically targeted score-inflation, race conditions, bypass paths, and missing edge cases.
- Reports under `/tmp/m{6,7,8,9}-audit.md`; codex logs at `/tmp/codex-m{6,7,8,9}-audit.log`.

## Related documents

- [Requirements](./OCTOS_HARNESS_ENGINEERING_REQUIREMENTS_M6_M9.md)
- [M9 Ledger Durability ADR](./M9-LEDGER-DURABILITY-ADR.md)
- [M9 Issue Stack](./OCTOS_M9_ISSUE_STACK_2026-04-24.md)
- [M8 Fix-First Checklist](./OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24.md)
