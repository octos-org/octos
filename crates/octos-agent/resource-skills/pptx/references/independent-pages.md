# Independent pages

For full-deck production/reconstruction with separable pages, use one persistent
maker peer per page, an outer coordinator, and independent reviewer(s). Explicit
user choices override this default. A targeted edit reuses unchanged valid page
acceptances; it does not require restarting the full deck. Shared-master/style
changes have one writer and a frozen common contract before dependent page work.

Settle content, canvas, shared typography, editability and acceptance criteria.
Reuse approved artwork and the proven common tool method. Verify a common
capability only if missing or changed; a probe remains single-page until the
page-first gate opens. Continuing an authorized task may reuse its valid history
and accepted geometry. For a clean comparison, isolate earlier answers, QA and
finished geometry from the permitted baseline inputs for each condition.

Inventory every final page. Assign each page to exactly one distinct maker,
using real host session/task receipts and exclusive page source/output paths.
The maker keeps that page through all repairs. Supply authoritative copy,
design/blank pixels, shared styles, current skill/runtime paths and acceptance
criteria. Both maker and reviewer need actual images. Workers read
[page worker](page-worker.md); reuse a single compiled builder and renderer.

Check the actual host's dispatch limits and available tool profile before
assigning work. In this version the CLI admits up to four `peer_handoff` calls
per master turn, including a new reviewer; other versions can differ. A host
that advertises fleet/preparation can stage the full per-page roster; otherwise
admit distinct page makers in supported waves. Do not silently bundle several
pages into one maker to work around a limit. If the user requires simultaneous
page makers and the host cannot provide them, report the actual concurrency and
unmet requirement. Skills cannot increase host capacity. Preserve authorization,
workspace binding and existing peer connections.

Record IDs, page ownership, real start/end events and terminal outcomes. Coverage,
prepared idle sessions and serial dispatch calls do not prove simultaneous work.
Do not launch duplicate workers or another scheduler to create that appearance.

## Activate the page-first gate

For the source-data builder route, before production create `.octos/pptx-page-plan.json` in the actual shared task
workspace with the complete final page order, coordinator and unique maker IDs.
Get the schema from native `help topic:page_gate`; confirm
`doctor.page_first_gate.supported` and `active`, then inspect `page_gate` output.
The default page threshold is 95/100; an explicit user target takes precedence.
Every page needs zero criticals and no open required findings, not a passing
deck-wide average. Unless another rubric is agreed, use content 20, native
editability 20, typography 20, readability/clipping 15, composition/fidelity 15,
and sources 10. Freeze it for this task.

Only the independent reviewer assigns scores/acceptance. The tool checks declared
ownership and bound evidence; the host verifies real peer identities/execution.
Successful admission is not a visual grade or proof that agents ran. Older
runtimes and source-less edits without native build receipts have workflow-only
enforcement; never fabricate receipts or claim this gate covers those routes.

## Stream page review and repair

Each maker builds/renders only its own page and publishes the first readable
candidate after build/file/page-count checks. Do not duplicate the reviewer's full
audit. Use immutable per-pass page sources and correct relative asset paths.
Publish one current candidate pointer with exact source, PPTX, actual render,
dependency and receipt hashes. Bind native post-build commands/receipts when used;
the reviewed artifact and render must include those repairs. A base cycle's
`editable.pptx` is not the separate `postbuild.pptx`.

The reviewer reads the latest pointer and actual original/blank/candidate pixels,
verifies the build chain, and returns a complete defect batch **for that page**.
Route it immediately to that page's existing maker while other pages continue.
Do not wait for a deck-wide findings round or dispatch one defect at a time.
Shared causes go to the shared-part writer; invalidate dependent page evidence
when that dependency changes.

Keep at most one outstanding packet per reviewer and a current file-based queue.
Busy `peer_send_input` queues another turn; corrections do not replace old work.
Reviewers reconcile the latest candidates at each turn and skip superseded or
resolved coordination questions. A changed candidate invalidates its old review;
unchanged accepted pages need no cosmetic edits or full re-review. Compare a
frozen cycle with its actual source snapshot, not a mutable later draft.

## Assemble accepted pages

Use page references in the root spec: `{"path":"pages/overview.json"}`. Resolve
assets relative to that page file. The coordinator keeps an ordered inventory of
accepted page sources and hashes. Verify current source/assets against the worker's
cycle before assembly; do not accept a pretty preview from different bytes.
Only after all current pages pass, assemble with the ordered root spec. Active
native admission rejects multi-page `build`/`cycle`/`batch`, including probes and
nested `operations`, until then. Use `page_gate` to see specific stale/missing
evidence; do not remove the plan, lower its target or switch tools to evade it.

A source-only rebuild does not preserve native post-build repairs. Replay the
approved commands or use a supported page-preserving integration route; preserve
the recipe and receipts. Never assemble full-page screenshots. The independent
reviewer checks the actual combined PPTX against approved page renders, native
editability, dependencies, page order, shared typography and continuity. Matching
PNGs alone do not prove editability or slide-part identity. Final review must meet
the agreed threshold with zero criticals. Changed pages return to their owners
and get fresh acceptance before reassembly; reuse valid evidence for unchanged
pages. Do not infer `patch` returns one page unless the runtime advertises it.
Maintain the original assets and source evidence. If the user asks to remove
intermediate PPTX copies, clean only this task's superseded copies after final
acceptance; retain the delivered deck, previews, source inputs and review
receipts needed to support the conclusion.

Before using `patch.output_page`, call
`{"action":"help","topic":"patch"}` and require
`supports_output_page:true`. Each peer's request must edit only its assigned
page and use a unique output path. Require `output_kind:"page"` and the
expected `page_id`, and check the returned source bindings against the frozen
baseline before putting page files into the ordered root spec. This applies
to a deck with source data, not arbitrary extraction from an existing PPTX.
Page output binds and structurally validates every source JSON, but verifies
only the selected page's images. The final full cycle must verify all page
assets before review; one successful page patch is not an all-deck check.
`sources` binds the root and referenced page JSON, not the image bytes. Check
the emitted page's `background`, `design`, and each `artwork_layers[].image`
hash against the frozen image inventory (or the approved hash for a newly
introduced repair asset). A matching JSON source binding alone does not prove
that the selected artwork matches the approved input.

Measure elapsed time from preparation through final independent QA, separating
dispatch, peer work, waiting, JIT compilation, integration and review where
the host exposes them. A speed comparison needs the same input, scope and
acceptance criteria; do not infer an overall speedup from worker count.

Use existing peer/subagent completion and cancellation mechanisms. Do not add a
polling daemon or repeatedly restart stalled workers. Preserve unresolved findings
and apply [peer exit criteria](peer-review-exit.md); final acceptance belongs to
the independent review session, not worker tool success.
