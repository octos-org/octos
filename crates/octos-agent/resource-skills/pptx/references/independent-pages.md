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
per master turn, including a new reviewer; this limits staging new peers, not
assignments to existing peers. Existing `peer_send_input` calls are `Safe`, so
independent assignments to different peers can be sent together. Other versions
can differ. A host
that advertises fleet/preparation can stage the full per-page roster; otherwise
admit distinct page makers in supported waves. Do not silently bundle several
pages into one maker to work around a limit. If the user requires simultaneous
page makers and the host cannot provide them, report the actual concurrency and
unmet requirement. Skills cannot increase host capacity. Preserve authorization,
workspace binding and existing peer connections.

Record IDs, page ownership, real start/end events and terminal outcomes. Coverage,
prepared idle sessions and serial dispatch calls do not prove simultaneous work.
Do not launch duplicate workers or another scheduler to create that appearance.
One maker per page covers ownership; it does not prove review and repair coverage
for unfinished pages. Accepted current pages stay complete even when peers are idle.

## Activate the page-first gate

For the source-data builder route, before production create `.octos/pptx-page-plan.json` in the actual shared task
workspace with the complete final page order, coordinator and unique maker IDs.
Get the schema from native `help topic:page_gate`; confirm
`doctor.page_first_gate.supported` and `active`, then inspect `page_gate` output.
The default page threshold is 95/100; an explicit user target takes precedence.
Every page needs zero criticals and no open required findings, not a passing
deck-wide average. Unless another rubric is agreed, use content 20, native
editability 20, typography 20, readability/clipping 15, composition/fidelity 15,
and sources 10. Freeze the agreed component maxima as the numeric plan-level
`rubric` map, and the reviewer per row as `reviewer_session`. Define the editable
scope once in the brief. A raster illustration allowed by the task may still have
a concrete fidelity defect; score that observed effect in the agreed category.
For a previously approved or image-model-produced background, bind that page’s
approved source in the plan row as `approved_background: {path, sha256}` and
retain its authorized generation receipt. Select it from the approved input;
a maker copying the hash of its own output does not establish provenance.

Only the independent reviewer assigns scores/acceptance. The tool checks declared
ownership and bound evidence; the host verifies real peer identities/execution.
Successful admission is not a visual grade or proof that agents ran. Older
runtimes and source-less edits without native build receipts have workflow-only
enforcement; never fabricate receipts or claim this gate covers those routes.

## Dispatch ready work before audits or waits

The native Octos outer coordinator owns this loop. Before a long deck-wide audit
or bounded wait, inspect every unfinished page and current peer execution state.
Use `page_gate`'s per-page `work_stage` (`make`, `review`, `repair`, `complete`)
and `candidate_current` to identify the current work, then check actual inputs
and dependencies. Send all ready independent page work to available eligible
peers first. Keep repairs with the original maker and preserve that page's defect,
review and stagnation history. A ready review needs the exact current candidate,
actual pixels and an independent reviewer; a real wait records the missing input
or unfinished dependency and its owner. A shared dependency may delay its affected
pages, not unrelated ready pages.

Do not leave multiple ready page reviews queued behind one busy reviewer while
authorized, attached independent reviewers are idle. Dispatch those pages to
different eligible reviewers through independent `peer_send_input` calls, keeping
each page's complete defect batch together. Keep at most one outstanding packet
per reviewer. If a page is already queued, stand down or supersede that old queued
work through the existing native mechanisms and confirm its disposition before
reassignment; do not create two reviewers acting on the same candidate. Carry over
all evidence and unresolved findings. Reassignment distributes capacity; it must
not shop for a reviewer who will give a higher score. Do not move page source
ownership or clear history to improve the appearance of parallelism.

Read `{"action":"help","topic":"page_dispatch"}` before using the native
`page_dispatch` checker; follow that deployed schema if it differs. Supply the
coordinator's current `assignments` (page, phase `make`/`review`/`repair`, peer,
state `running`/`queued`/`waiting`, and applicable candidate hash or dependency)
and `peers` (peer, current state, authorization and attachment). Current peer
execution facets describe running/queued work; old result text may describe a
previous turn and is not an idle/completion signal. Unknown state stays unknown.
These are coordinator declarations, not host attestations of identity,
authorization, attachment, independence or actual execution.

Resolve the checker's `ready_unassigned`, `avoidable_serialization` and
`invalid_assignments` using current evidence; `eligible_idle_reviewers` helps
locate declared capacity. `declared_coverage_complete` reports only coverage of
the supplied declarations against current page evidence. The checker does not
send work, grade quality, grant permission, verify real peer execution or create
an EndTurn gate. It does not require a fixed number of busy peers or reopen
complete pages. Preserve real host limits and report any remaining capacity or
dependency gap; do not add an external queue or automatic review prompts.

## Stream page review and repair

Each maker builds/renders only its own page and publishes the first readable
candidate after build/file/page-count checks. Do not duplicate the reviewer's full
audit. Use immutable per-pass page sources and correct relative asset paths.
Publish one current candidate pointer with exact source, PPTX, actual render,
dependency and receipt hashes. Bind native post-build commands/receipts when used;
the reviewed artifact and render must include those repairs. A base cycle's
`editable.pptx` is not the separate `postbuild.pptx`.

Use native `page_publish` to derive the candidate from the existing one-page
cycle and its real source/render receipts; do not spend model turns calculating
hashes or rebuild merely to publish. A new one-page `cycle` can combine both
steps with `publish_candidate:true, freeze_inputs:true` when doctor advertises
`freeze_inputs`. This captures page and root JSON once by content; the editable
draft and subsequent notes can change while the reviewer reads the frozen
candidate. Keep artwork bytes fixed. Use the published source for assembly.
Under an active page plan, a source-backed cycle defaults to its design width;
bare PPTX render jobs should explicitly specify the intended width. Check returned
dimensions once. For post-build changes, supply the actual
`artifact`, `render_receipt` and ordered native `receipts`. Publication rejects
changed dependencies and a caller other than the declared maker. It awards no
score and sends no peer message itself.

The Octos coordinator is the outer supervisor; page makers and independent
reviewers are its native peers. After `page_publish`, the maker uses
`peer_publish` to send the exact candidate/cycle paths to its native blackboard,
using the tool’s optional `handoff_id` set to the returned `candidate_id` when
advertised. The independent reviewer completes the visual checks, calls
`page_review_submit(page, candidate_id, verdict)` and publishes its returned
`verdict_id` as a new handoff. Use the same ID on retries. The coordinator calls `peer_gather`,
checks the current peer/turn and candidate, then dispatches each ready page to an
available independent reviewer with `peer_send_input`, following the coverage
check above. It gathers each reviewer's complete page defect batch and routes it
to the original maker. These
workflow decisions belong to Octos, not an external script.

The client opens model-staged sessions and forwards native protocol messages;
it may maintain transport and reconnect. It must not infer a new review task
from file existence, author review prompts, run a second page queue, synthesize
"continue" turns, or interrupt a writer just because publication succeeded.
Native publication is not acceptance or a terminal event. Report idle incomplete
execution honestly. While peers remain active, the Octos supervisor uses bounded
native `peer_gather` waits and continues its own supervisory loop. End a turn to
wait only when the deployed native wake/continuation path is actually verified.

### Verify the next publication can wake the coordinator

For each outstanding phase, record the real peer/turn and the publication that
makes its next step ready: a maker's current candidate, a reviewer's actual
verdict, or a native terminal result reporting completion, failure or a blocker.
Consume already-returned results before waiting. A maker publication is ready
for independent review without waiting for unrelated makers or a fleet-wide
synthesis. An inspection packet or metadata recap is not a completed visual
review; route its remaining inspection to the same reviewer.

Check the deployed native wake contract. A `peer_results_ready` prompt note can
be passive until the next coordinator turn; a fleet-synthesis wake can require
every owned peer to be settled. Neither proves that one page's completion wakes
the outer loop. Inspect pending inputs, live attachment and obsolete helpers if
that gate holds. A previous result does not prove a queued input is satisfied.
The authorized owner may use native `peer_close` to retire a helper only after
confirming it has no remaining work; preserve its results/history and let the
native close operation settle queued inputs. Do not delete queue records, forge
completion, reset claims or retire active page owners to force a wake.

When using a supported Octos-native monitor, cover the actual expected maker
candidate and review-result paths, plus terminal/blocker results where the host
supports them. Watching only review/assembly directories misses a maker's new
candidate. Use the task's authorized workspace and actual host command schemas.
Keep probe output stable; exclude renders, build intermediates and the
coordinator's own status edits. Read existing outputs first: poll-mode monitors
can establish their first observation as a baseline without firing. Arm before
the next expected publication, keep one scoped replacement, and retire consumed
watches through native controls. Do not bypass permissions or introduce an
external page scheduler to compensate for a missing native trigger.

Verify the chain on a real task event or a supported isolated runtime fixture:
peer publishes the expected current result → native wake is queued → the owning
coordinator resumes in the correct workspace → it gathers the result and
dispatches the newly-ready review/repair. A monitor listed `active`, a successful
probe, a queued wake alone, or an operator's manual resume is not that proof.
Until verified, report the gap and retain bounded native supervision instead of
claiming automatic continuation. Record the real handle and event evidence in
the existing task status. Wake success never grants page acceptance or changes
the quality threshold. If nothing remains executable, report the precise blocker
rather than wait on finished jobs or repeat equivalent repairs.

The reviewer reads the latest pointer and actual original/blank/candidate pixels,
verifies the build chain, and returns a complete defect batch **for that page**.
Route it immediately to that page's existing maker while other pages continue.
Do not wait for a deck-wide findings round or dispatch one defect at a time.
Shared causes go to the shared-part writer; invalidate dependent page evidence
when that dependency changes.

The native verdict writer supplies hashes and identity. For a changed judgment,
the same reviewer supplies the current `replaces_verdict_id`; immutable versions
are preserved. A schema repair reuses the existing visual work and PPTX. If a
rejection needs a rubric clarification or an unavailable tool, submit a
`disposition` with `owner`, `reason` and `resume_when`. The coordinator owns
routine production and assembly choices. Dispatch scoring disagreements to an
independent arbiter and shared capability failures to a technical peer in
parallel. Group pages with the same underlying question and bind each candidate.
Use `page_dispatch.ready_unassigned` when `doctor.page_first_gate.peer_resolution`
is true. On older hosts translate old `task_owner` rows into the same peer jobs.
Return rulings to the original reviewers and verified technical changes to the
original makers. An arbiter interprets the existing rubric and cannot waive a
required finding. If all authorized options are exhausted, publish a blocked
outcome with the exact remaining defect, attempted remedies and evidence. End
the attempt without waiting for human approval or repeating unchanged audits.
On hosts exposing peer resolution the specialist publishes its scoped report
through the native peer channel. Confirm the sender, page and candidate against
the assignment. The original reviewer binds that report with
`page_review_submit` as documented in its tool help. A dispatch inventory with
`only_blocked_pages_remain:true` ends this attempt with a technical blocker.
Consume the specialist's native publication once per candidate. Send it
straight to the original reviewer for attachment using the top-level
`blocked_report` option of `page_review_submit`. A spelling or binding error
requires correcting the submission, not commissioning another investigation.
While an investigator is active, await its publication. Do not send another
peer to audit the wait. When dispatch returns a blocked handoff, record the task
as blocked, retire this task's consumed native monitors, and transition its
existing goal to blocked through the owner if present. Do not dispatch a final
review or ask another peer to verify a terminal dispatch inventory.
Check the native queue for the reviewer that recorded the last blocker. If its
verdict and report are bound, no page is ready, and only stale instructions for
that job remain, the owning coordinator retires that peer with `peer_close`.
`peer_send_input` queues another turn even when the message says “stand down”.
Write the blocked outcome to the existing machine-readable task status used by
the host, with the bound report paths.
Earlier runtimes and pre-existing verdict files retain their original contracts.

Keep the current dispatch record in the existing coordination files.
Busy `peer_send_input` queues another turn; corrections do not replace old work.
Do not treat a serial review queue as a dependency or keep it while eligible
reviewers are idle. Stand down old queued work before reassignment as above.
Reviewers reconcile the latest candidates at each turn and skip superseded or
resolved coordination questions. A changed candidate invalidates its old review;
unchanged accepted pages need no cosmetic edits or full re-review. Compare a
frozen cycle with its actual source snapshot, not a mutable later draft.

## Assemble accepted pages

Use page references in the root spec: `{"path":"pages/overview.json"}`. Resolve
assets relative to that page file. The coordinator keeps an ordered inventory of
accepted page sources and hashes. Verify current source/assets against the worker's
cycle before assembly; do not accept a pretty preview from different bytes.
After all current pages pass, prefer native `page_assemble(output)` when advertised.
It reads each accepted one-slide file and its review from the plan in page order,
preserving page-specific styles and native additions made after the base cycle.
It returns the bound inputs and requires independent final review. For equivalent
source-only pages the ordered root-spec route is available. Active
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
