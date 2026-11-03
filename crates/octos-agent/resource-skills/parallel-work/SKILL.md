---
name: parallel-work
description: "Coordinate authorized subagents or peers on separable questions, implementation, analysis, or artifact production. Use for dividing work, sharing a proven method, collecting evidence, and integrating results; do not launch agents for a simple answer or merely because tools are available."
---

# Parallel work

Use the existing authorization and the runtime's available agent tools. This skill
organizes delegation; it does not grant permission to spawn agents, spend beyond a
budget, publish, or change external systems. A domain skill still supplies the
actual method and acceptance criteria. Do not import document-specific gates into
coding, research, or ordinary questions.

## Choose useful boundaries

Delegate a unit with a clear result that can proceed without another worker's
unfinished decisions. For questions, split distinct evidence needs or hypotheses;
for production, split independent modules, data partitions, or artifact units.
Avoid giving several workers the same broad task unless an independent comparison
is intentional. Small tasks or tightly coupled changes can be faster in one agent.

Make dependencies explicit and dispatch ready work. Shared interfaces, terminology,
units, and output formats should be settled before their dependent work. Do not
finish all target units in the parent before delegating. Batch deterministic work
with an existing program rather than assigning an LLM to each mechanical item.
Select unit size to amortize context and setup, not one agent per file by default.

Concurrency depends on available slots, model/API capacity, memory, and constrained
services. Worker slots and renderer/build/database slots are separate resources.
Do not introduce arbitrary iteration caps to hit a speed target. Honor explicit
budgets and host limits; unlimited iterations are not permission to run forever.

## Establish the common method once

Reuse the applicable tools, domain skills, accepted interfaces, and relevant
validation evidence. Verify missing or changed shared capabilities with a small
representative check. Every worker should receive usable paths and tool access,
not just the instruction to "follow what the parent did."

Share the method, runtime, and legitimate constraints. Let workers solve their
assigned data and defects. Do not ask each worker to rebuild the same parser,
renderer, test harness, research procedure, or dependency setup. If the task is
itself to compare methods, deliberately allow separate implementations and label
the experiment accordingly. A common tool failure goes to one owner; update its
version and notify affected workers rather than having each invent a workaround.

A tested helper does not make an unreviewed draft correct. Identify what is proven
(for example, an interface or transformation) and what each worker must still
determine. Assignment prompts may narrow scope and convey sourced requirements;
they should not add speculative quality goals or substitute for the domain skill.

## Give each worker a complete, compact assignment

Use [assignment and result examples](references/assignments.md) when needed. Include:

- The goal, specific scope, and what the parent will integrate.
- Authoritative inputs and relevant versions, including unresolved assumptions.
- The applicable method/skill paths, executable commands, and required capabilities.
- Dependencies, output location or answer format, and exclusive write ownership.
- User-derived acceptance criteria, useful evidence, and the reporting channel.

Keep relevant source material available without copying the full parent history
into every worker. A stable shared brief plus a short assignment reduces repeated
input, but does not guarantee provider prompt-cache hits. Inspect capability, not
tool count: an image task requires actual image content in the worker's context,
and research requires source access. File paths, OCR, and another agent's summary
are not interchangeable with original evidence.

For shared files, assign one active writer per mutable output; use isolated
directories or worktrees as appropriate. Read-only investigations may overlap.
The parent owns shared interfaces, integration, and final delivery. Communicate an
interface change before dependent workers build on it. A prose ownership claim
is not a filesystem lock. Do not cancel or overwrite an unrelated user's work.

## Execute, observe, and finish

Workers use the shared method, inspect their actual results, and batch independent
repairs. Progress means resolved requirements, useful evidence, or changed working
artifacts, not the number of edits or a confident status message. A passing result
can return unchanged; it needs no cosmetic edits to demonstrate effort.

When outputs or failures repeat, compare the relevant input/version, attempted
change, and measured outcome. The same failure after a meaningful change may be
useful evidence; repeating an identical action without a new hypothesis is not.
Inspect the cause, try a justified different approach, or report a concrete
blocker. Do not invent a fixed repetition limit, silence a verifier, remove a
requirement, or declare success to stop the loop. Record genuine uncertainty.

For asynchronous work, keep the returned task/job ID and use completion events
or bounded waits. "Still running" is not a reason to submit a duplicate job.
Separate worker execution state, external-job state, and artifact acceptance.
File existence alone proves none of these. A repair attempt can write only after
the prior writer actually stops or in a new, non-overlapping location. A timeout
or unknown task ID is not a successful completion. Resume using the actual saved
state; do not rediscover and repeat completed work blindly.

For Octos native workers or peers, see [runtime mapping](references/octos.md).
Other runtimes should use their own advertised schemas rather than copy tool names.

## Integrate and verify the result

Collect a concise result with artifacts or claims, input versions, evidence, and
open issues. For ordinary read-only questions, a sourced answer is sufficient;
do not demand a filesystem manifest. For generated artifacts, bind relevant
checks to the actual delivered version. Use an existing domain handoff format.
Hashes identify bytes; they do not prove truth, visual quality, or correctness.

The parent reconciles conflicting findings and verifies the final task's coverage
and cross-unit behavior. Examples include shared API integration, global deduping
and denominators, consistent source dates, and whole-document relationships.
Separate proven facts from inference and unresolved contradictions. Multiple
agents agreeing is not independent evidence if they used the same flawed source.

Reuse valid checks when their inputs, environment, and coverage remain applicable.
Run checks affected by integration; do not mechanically repeat every worker's
local work. Conversely, a worker's self-reported pass cannot waive a concrete
defect in the final result. Return focused repairs to the responsible owner with
the observed failure and constraints, without prescribing an unverified answer.
Independent review is useful for important or ambiguous results, not obligatory
for every small task. Final completion requires the requested scope and applicable
checks, not merely terminal worker statuses.

When evaluating speed, include preparation, workers, waits, retries, integration,
and QA in wall time. Report output quality alongside requests, input/output tokens,
cached/uncached input, and constrained-service calls when available. Compare equal
tasks, capabilities, and acceptance criteria. Parallel runtime follows the longest
dependency path plus overhead; worker count alone does not promise that speedup.
