# Independent page workers

For normal page workers use the compact [worker guide](page-worker.md). This
reference contains coordinator, custom-adapter and handoff-contract details.
Use [automatic revisions](fit-and-revise.md) to allocate and publish replacement
attempts; accepted source/proof/QA remain immutable without manual path bookkeeping.

Use this mode when the task authorizes subagents or peers. A page worker owns
one assigned page's data, exceptions and local QA, reusing the common production
pipeline. Independence means exclusive page ownership, not a new implementation
of conversion, rendering and validation. It does not make the whole
deck, inspect other workers' layouts, or wait for a supervisor's quality verdict.
Its page assignment overrides full-deck delivery instructions, not fidelity or
verification requirements. The coordinator owns assembly and cross-page review.

Use the [shared reconstruction method](page-method.md) for both solo and delegated
production. The coordinator passes its verified method and applicable constraints;
workers apply it to page-specific data. Do not add a second open-ended "perfect
this page" procedure in an addendum. Tool preflight and OCR drafts have different
validation status; record which parts are established rather than implying that
a runnable draft is an approved conversion.

## Share production work before multiplying workers

For blank artwork plus native text, use the supplied [native builder](native-builder.md)
and its page cycle. The implementation, generated adapter and handoff/assembly
commands already exist. The producing agent establishes shared input data and
font settings, not a new conversion engine. Extend code only when the actual task
requires an unsupported capability. Reuse applicable deployment evidence. Batch
mechanical preparation such as OCR, inspection views and input hashing; preserve
provenance and let workers verify candidates against pixels and canonical copy.
When the common converter can generate initial page candidates, batch that work
and pass the candidates to workers for inspection and repair rather than asking
each worker to recreate them. Otherwise keep placement work page-local; do not
add a serial manual-layout stage as a prerequisite to fanout.
Reuse completed image pairs. Do not wait for unrelated preparation once a page
and its shared dependencies are ready. A judge must not supply solved page layouts.

Prefer a thin page adapter plus page-local data over a standalone engine per page.
Workers batch independent object fixes, run the common build/check/render cycle,
then inspect the resulting page and relevant regions together. Inspect context
when necessary; do not turn every crop or label into a separate conversation turn.
Execution can batch operations; visual judgment and completeness remain required.

Report a suspected common-helper defect with a reproducible case. A designated
owner fixes it once and publishes a versioned update; other workers continue
unaffected work. Only outputs depending on the change need fresh attempts and
proofs. Do not invent parallel helper forks, silently change frozen dependencies,
or propagate a page-specific coordinate fix to unrelated pages.

For timing comparisons, use the same page pipeline and starting artifacts in
serial and parallel modes. Include common setup, retries, assembly and acceptance
time; record per-page model calls and revisions as well as elapsed time. A batched
solo build versus independently reimplemented page workflows does not isolate
the effect of parallelism. Quality must meet the same acceptance criteria.

## Repair an existing deck without repeating conversion

When the user requests repairs and provides an editable candidate, use that
candidate and its existing source as the starting point. This mode overrides
fresh-conversion calibration and independent recreation of page builders. Keep
the original candidate immutable. Reuse its native objects, layouts and artwork;
rebuild a region only when inspection identifies a concrete reconstruction defect.

The coordinator resolves the current interpreter, installed font, existing build
entrypoints and shared render/validation helpers once. Supply these findings,
current page source/candidate, authoritative copy and artwork to each job. A
relocated baseline may contain stale absolute paths: resolve them in a working
copy once where shared, without rewriting otherwise working slide layouts.
Workers should not each rediscover dependencies or invent their own validator.

Each worker inspects its assigned page, records actual object-level findings,
batches independent native-text fixes and verifies the changed regions with their
neighbors. Use [text-layout reinspection](text-layout-reinspection.md). A page
with no demonstrated defect can return unchanged with fresh inspection evidence;
activity alone is not required. Reuse unchanged artifacts only after verifying
identity and coverage. No per-page iteration or render quota is introduced.

As workers finish, the coordinator validates handoff structure and stages their
outputs. After the final page arrives, perform the full-deck check of assembly,
common typography, ordering and any composition changes. Do not recreate every
worker's local audit or reimplement their pages. A defect discovered by the
coordinator belongs in a focused, exclusively owned repair attempt, which can run
in parallel with other independent repairs; it does not require a fresh full-page
conversion or sequential repair of every page by the coordinator. Never dismiss
visible defects merely because local proofs passed. Preserve whole-deck visual
coverage and bind final evidence to the final delivered file.

## Freeze the shared contract, then dispatch ready pages

Before dispatch, the coordinator records a versioned **JSON** shared contract.
`schema_version: 1` and the ordered `page_order` array are required by the handoff
checker. Record canvas, typography, builder interface and runtime as structured
fields or concise descriptions in that JSON; a Markdown-only substitute does not
satisfy the checker. The contract contains:

- Ordered, stable page IDs from the actual outline; slide dimensions and units.
- The user-selected style, title face/size/origin/baseline/alignment, body hierarchy,
  installed font paths, and any approved exceptions. Freeze these across workers.
- The actual runtime/interpreter, dependencies, page-builder interface and render
  route. Resolve paths for each worker; do not assume parent shell variables or
  conversation history are inherited.
- Content authority, image-generation provenance and permitted tool capabilities.
  Supply service configuration paths, not copied credentials in worker messages.
- A small working, generic build/render specimen if needed to verify the interface.
  It contains no target-page labels, OCR maps, layout coordinates or repaired slides.

Reuse current, applicable shared-helper execution evidence. If unavailable or
invalidated, execute the affected CLIs on a disposable specimen before fanout. Check their
actual outputs and a deliberately changed specimen; import/AST success alone can
pass a truncated no-op script. Record commands, versions/hashes and outcomes in
the shared contract or its referenced preflight record. Workers verify that this
record applies to their environment and helpers, then check their own page outputs;
they need not repeat the same environment discovery and specimen validation unless
dependencies changed or actual failures invalidate it. Use the supplied package,
native-object and region checkers before inventing another QC implementation. If a shared helper breaks,
record the shared blocker and repair it once, versioning the contract and affected
jobs; do not have every worker rebuild a different checker.

When the workspace supplies the compatible asynchronous `/jobs` render service,
use the provided [checked job client](checked-jobs.md) in shared builders instead
of rewriting polling or render-output allocation. Other backends must provide
the equivalent fresh-attempt, terminal-success and source-bound evidence contract.

Keep the full applicable skill available to every worker. The coordinator's
summary or TASK.md is an assignment, not a replacement for SKILL.md, this reference,
or the tool instructions. Use `scripts/prepare_page_jobs.py` to create consistent
packets instead of rewriting the protocol. Supply a plan JSON:

```json
{
  "shared_contract": "contract.json",
  "pages": [{
    "page_id": "intro",
    "inputs": [
      {"role": "canonical-copy", "path": "copy/intro.md"},
      {"role": "design", "path": "art/intro-full.png"},
      {"role": "blank-art", "path": "art/intro-blank.png"}
    ]
  }]
}
```

Plan paths resolve relative to the plan file. Run with `--plan PLAN --out JOBS
--skill PATH/TO/SKILL.md --tools PATH/TO/TOOLS.md`. It writes hashed job inputs,
worker instruction references, exclusive output directories and native spawn
request objects to `JOBS/prepared-jobs.json`. The helper itself generates no layouts;
the receipt lists the actual input roles for each job. Additional inputs may
include machine-derived layout or OCR candidates. Preserve their hashes and
independently verify candidates against the authoritative copy, design and blank
pixels; role names do not establish correctness or provenance. Use the canonical
`result.json` protocol below instead of inventing a second manifest.

For native shared workers, also pass `--workspace` naming the parent's actual
spawn workspace (default: plan directory). The packet's `spawn_request.owned_paths`
claims only its output subtree, relative to that workspace; `spawn_workspace`
records the binding. Keep job files and shared inputs immutable. Preserve the
typed claims when dispatching or repairing, not only the prose ownership brief.
On Octos versions advertising `owned_paths`, hosts can enable
`OCTOS_REQUIRE_SHARED_WRITE_CLAIMS=1` before launch so a repair that omits its claim
is refused. This is cooperative admission within one runtime, not a security
sandbox or a lock on unrelated workers. A conflict identifies the existing owner:
use a fresh output attempt or wait until that worker's execution actually stops.
A different label or a cancelled status does not transfer ownership. Older
runtimes without this capability provide no mechanical claim enforcement; retain
fresh isolated attempt directories and do not describe prose as a runtime lock.

Leave iteration budgets unlimited unless the user/host explicitly requires a
finite budget. Do not invent per-page iteration or render quotas to chase a speed
target. Complete work ends when its artifacts and checks are finished; a real
blocker must remain visible. Concurrency is a separate capacity setting.

Use a fresh isolated output directory per page attempt. Give the worker a job file
with the fields below. Paths in the job are relative to that job file's directory.
Hash inputs after generation finishes; never bind a worker to a file still being
written. For image-first work, that page's full design and blank art must exist
before reconstruction. Already supplied frozen pairs satisfy this stage. Pages
whose inputs are ready need not wait for unrelated pages.

```json
{
  "schema_version": 1,
  "page_id": "<assigned page ID>",
  "attempt_id": "<unique attempt ID>",
  "shared_contract": {"path": "<shared contract>", "sha256": "<hash>"},
  "inputs": [
    {"role": "canonical-copy", "path": "<page manuscript>", "sha256": "<hash>"},
    {"role": "design", "path": "<complete generated page>", "sha256": "<hash>"},
    {"role": "blank-art", "path": "<text-free counterpart>", "sha256": "<hash>"}
  ],
  "output_dir": "<exclusive page-attempt directory>"
}
```

The worker also needs the skill path, tool instructions and a concise acceptance
brief: preserve the design, reconcile every intended label/occurrence with the
manuscript, produce editable text, and verify its actual rendered page. Include
relevant neighboring terminology if necessary; do not copy the entire parent chat
or all other pages into every worker. The shared contract may name dependencies,
but page production must not require another worker's unfinished layout.

## A common builder interface prevents merge repair

Choose the available authoring backend once. Prefer re-running page builders into
one clean deck over transplanting XML between independently authored packages.

For python-pptx, each worker provides a thin `page.py` adapter with
`build_slide(presentation, job) -> Slide`: add exactly one slide to the caller's
presentation, using the shared implementation and canvas/theme; resolve assets
from job paths. The same function builds its one-page proof and the final deck. Importing it must not
write files, render, change cwd, mutate global fonts, or start agents. A worker may
use sibling source/data files inside its own attempt directory, all declared in
its handoff. For PptxGenJS use the equivalent `buildSlide(presentation, job)` export;
do not mix backends within one assembly contract.

The coordinator establishes reusable code and the font contract; workers verify
or determine their page's text placement on the assigned blank art. In repair
mode retain existing correct placement. Creating a new page does not require a
new textbox, measurement or validation implementation. Do not flatten a
worker PPTX into an image for merging; that discards editability. If a template
requires package import, use the existing OOXML validation reference and verify
all transitive relationships, notes, masters and layouts.

## Worker execution and evidence

1. Read the job, its shared contract, page inputs and applicable skill references.
   Verify hashes and capability. Inspect actual design and blank pixels in this
   worker's model conversation using a functioning image-capable read/view tool.
   OCR or a parent saying "the image looks fine" does not replace this step.
   When supplied, use the inspection-view manifest to read a size-compatible
   derivative first; retain original art for placement and detailed crops. If
   no view exists, create one with the supplied workspace imaging library. On
   sandboxed macOS, `sips` may fail while writing system temporary files; after
   that capability failure use workspace-local Pillow rather than retrying it.
   Inspect the blank as well as the design: removing text can also remove a
   colored label background. Choose readable native text against the actual
   retained artwork, and report missing art separately.
2. Author only the assigned page data, adapter and necessary exceptions under
   `output_dir`, reusing the common implementation. Shared art, content, helpers,
   dependencies and sibling directories are read-only. Do not write root exports,
   root progress/delivery files or a shared layout file. Do not spawn nested workers.
   These are ownership instructions; a shared workspace does not enforce them as
   a filesystem sandbox. Configure confinement when the runtime supports it.
3. Build an editable one-page PPTX and derive its blank companion from that PPTX.
   Every changed candidate gets a unique attempt directory propagated through
   all proof, full/blank render, OCR and QA paths, plus unique renderer profiles
   and job IDs. Changing only an attempt label while reusing output paths is not
   isolation. Bind successful render receipts to the current source/PPTX hashes
   and exact output paths before accepting those images as evidence. Stop the
   dependent OCR/QA chain on any failed build or render, even if a previous image
   still exists at a familiar path; never fall back to stale output and report
   the failed attempt as inspected. Preserve the failure and make a fresh attempt.
   Save asynchronous job receipts, do independent work or bounded waits, and resume
   the existing job. Repeated "running" snapshots do not justify resubmission.
4. Inspect the actual PPTX objects and both rendered pages. Use the common cycle
   to collect render paths and machine findings in one result; create detailed
   crop comparisons for ambiguous or failing regions instead of automatically
   repeating measurement of every object. Reconcile canonical
   copy and repeated labels; check font size/alignment, label-to-object association,
   clipping, overlaps and design fidelity. Native text that exists but is invisible
   does not pass. Record inherited art defects separately. Repair and re-render
   against fresh hashes; keep unresolved findings rather than erasing them.
   Pixel-overlap scores are optional diagnostics, not acceptance gates: font
   changes can lower them on readable text, while a high score can coexist with
   overlaps or invisible labels. Do not waive visible defects to meet a score.
5. Atomically publish `result.json` last, after files and QA evidence exist. A
   worker's completion claim is not independent visual acceptance. If a capability
   or required input is missing, publish `status: "blocked"` with evidence and
   report the missing item; do not substitute an unverified page.

## Handoff and acceptance

The result records `schema_version: 1`, the same `page_id` and `attempt_id`, the
job file's `job_sha256`, `status` (`complete` or `blocked`), and an `artifacts` map.
Each artifact is `{ "path": "<relative to output_dir>", "sha256": "<hash>" }`.
Required complete-result keys are `source`, `editable_pptx`, `blank_pptx`, `render`,
`blank_render`, and `qa`. Declare additional source, placement data, assets and
logs in the same map. Include reproducible build commands in the QA record.

The QA JSON binds `pptx_sha256`, `blank_pptx_sha256`, `render_sha256` and
`blank_render_sha256` to those exact artifacts. It records
`inspection: {"source": true, "editable_render": true, "blank_render": true}`
only for actual inspections, plus evidence paths, checks, remaining limitations
and `open_findings`. Relevant unresolved defects prevent a `complete` handoff.
Hash and boolean checks establish consistency, not the truth of visual judgments.

Use the read-only checker with the available Python interpreter:

```text
python <skill>/scripts/validate_page_handoff.py --pair <job.json> <result.json>
```

The coordinator validates identity, current input/contract hashes and artifacts
before accepting a page. Reject duplicate, missing, stale, blocked or
out-of-directory handoffs. Never choose files by latest modification time or
accept an old attempt's QA for a new PPTX. Preserve each accepted attempt's source,
auxiliary files, proofs, renders, QA and result unchanged. Renaming the old source
as a backup does not make a modified source covered by the old proof or QA.

Rebuild a clean complete deck in `page_order`, calling each accepted builder with
the same job and frozen contract. Verify one slide per page, canonical text,
fonts, package compatibility and final full/blank parity. Render the assembled
PPTX; assembly may change inherited theme or fonts, so worker previews do not
approve the assembled artifact. Review every final page and cross-page typography.
Run one final collection check using the complete prepared-job receipt:

```text
python <skill>/scripts/validate_deck_assembly.py --jobs <JOBS/prepared-jobs.json> \
  --deck <assembled.pptx> --out <assembly-report.json>
```

This read-only checker validates every job and current handoff/input/artifact hash,
requires receipt order to equal the frozen contract's full `page_order`, requires
the exact final page count, and compares every accepted one-page proof with its
corresponding final page. It checks native content/resources and inherited
layout/master/theme/defaults. There is no page selection, skip or waiver option;
do not narrow or reorder the receipt to make a partial collection pass. Any
failure leaves assembly incomplete. The report explicitly keeps
`visual_quality_verified: false`: it does not establish pixel equality or judge
visual quality, nor can it authenticate a rewritten history of acceptance.

Do not silently apply global font, weight, spacing or geometry changes during
assembly. Such changes require a fresh page attempt, new proof/render inspection
and a new hash-bound handoff. Return repairs to the page owner after the old writer
has actually stopped; keep a single active writer per page. Preserve the old
attempt and receipt, issue an updated complete receipt pointing at accepted new
attempts, then rerun the full collection check. Missing/blocked handoffs remain
unfinished, even when an intermediate PPTX happens to exist.
Avoid duplicating a worker's completed visual inspection when the final page's
decoded pixels are exactly identical at the same dimensions and renderer settings:
record that equivalence and carry forward the actual worker inspection with its
original coverage, bound to the final PPTX/render hashes. Similarity scores or a
matching background alone are insufficient. Inspect any changed or previously
uninspected page; native editability, copy and cross-page checks still apply.
Return page-specific repair evidence to its owner; change a shared style contract
only with a version/hash update and revalidate all affected pages. The coordinator
handles integration repairs, not silently replacing the workers' page production.

## Native Octos dispatch

Use the actual runtime's tool schemas. For background `spawn`, use a complete
initial brief and the assigned output directory; omit model overrides when the
task requires inherited model parity. Collect with `check_background_tasks` and
`read_task_output`. A running job is not failure. If live steering is unsupported,
finish/cancel the old task through its supported protocol before dispatching a
new attempt; never run two writers for the same page directory.
Use the actual task UUID/ID returned by the runtime for wait/read calls, not its
display label. On older runtimes whose spawn text omits that ID, resolve it once
through `check_background_tasks`. An `unknown`/not-found target is not completed
work, even if an older tool also reports `terminal: true`.

When supported, pass `skill_paths` for SKILL.md, this reference and tool usage;
these references tell the worker what to read, and do not prove it followed them.
Use `max_iterations: 0` for an unlimited assignment. Hosts that must enforce the
user's no-limit choice can set `OCTOS_SPAWN_MAX_ITERATIONS=0` before launching the
runtime; a model-authored smaller cap then cannot override that choice. This does
not remove cancellation, permissions or an explicitly configured cost/token budget.

For peers, use distinct names and `peer_handoff`; set `worktree:false` only when
inputs are intentionally shared and page-output ownership is established. Gather
actual results with `peer_list`/`peer_gather` and use `peer_send_input` only when
supported. Completion requires the returned manifest and existing artifacts,
not just a terminal session status. Use runtime completion events or bounded
waits instead of repeatedly polling every worker.

Dispatch one ready page per job, with concurrency bounded by the environment's
slots and render/API capacity. A worker finishing one page can take a new page
under a new job/attempt directory. Do not serialize page work on a global layout
file or require every worker to render and verify the whole deck.
