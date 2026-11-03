# Shared reconstruction method

Use this method for converting approved artwork to native editable text. It applies
to a solo producer and to each page worker. The coordinator supplies page inputs,
runtime and user requirements; it does not ask each worker to invent a new method.
The generated assignment narrows ownership without adding new aesthetic targets.

## What is established, and what still needs judgment

The shared builder establishes units, font application, native objects and layer
parity. Its successful test does not approve an OCR draft or prove a page is readable.
The shared inspection command measures the actual current render. It does not know
whether a label belongs to a particular arrow or whether the manuscript is correct.
The worker owns that page-specific judgment and its data changes.

Before dispatch, the coordinator records the applicable environment check, common
styles and user constraints in the existing shared contract. Give each ready page
its raw inputs and machine-derived draft when available. Label unreviewed drafts
honestly. Do not perform a full manual reconstruction of the deck before dispatch.
Existing verified conversion rules should be reused across pages and later tasks.
An environment or helper problem is fixed once by its owner and versioned.

## One method, page-specific data

1. Inspect the assigned design and blank artwork, and read the canonical copy.
   Reconcile OCR into native text occurrences; preserve correct starting objects.
   Set the intended hierarchy and independently identified owner regions when useful.
   Keep labels belonging to distinct cards/cells/nodes as distinct objects; line
   breaks inside one large text box do not establish those separate placements.
   Use the optional [font proposal helper](fit-and-revise.md) for actual-font size
   and line calculations, then inspect the result. It does not infer ownership.
2. Build, render and measure with `native_cycle.py run --inspect`. Alternatively,
   run `native_inspect.py --cycle CYCLE --out CHECK` on a ready cycle. Use the
   reported files rather than recreating glyph extraction, ink segmentation,
   contact sheets or coordinate calibration in local scripts.
3. Use [three independent comparisons](review-evidence.md) for manuscript meaning,
   source/blank artwork changes and final appearance. Inspect the page and relevant
   panels, including unflagged regions; `--sweep-pages PAGE_ID` can supply readable
   overlapping views without rerendering. Collect the actual defects together,
   then repair their page data in one batch. Use `--objects ID ...` for detailed
   inspection of affected objects and neighbors when needed. A fresh candidate
   can be compared with `native_inspect.py --previous CHECK/report.json`.
   `native_cycle.py revise` allocates the new attempt and supplies actual before/after
   crops; resume its saved revision rather than manually replacing accepted paths.
4. Once the page is complete, use the existing review JSON and `native_cycle.py
   handoff`. The coordinator assembles accepted sources and checks the final deck.

Run the bundled CLI help when a command is unfamiliar. Read implementation only
to diagnose a concrete tool problem. A page worker may generate or patch page data;
it should not write another measurement/validation framework. If the shared helper
cannot measure a needed property, preserve the unknown result, inspect the pixels,
and report a reproducible gap. Unknown measurements are neither passes nor defects.

Keep detected defects and their closure evidence in the existing review record;
no separate paperwork is needed for every command. Each change should be connected
to a source-correction, an explicit user requirement, or an observed visual defect.
Batch independent corrections; unchanged correct regions need no new treatment.

## Completion criteria and constraint conflicts

A page is complete when canonical text and occurrences are reconciled, required
typography is consistent, labels are readable and correctly associated, there is
no observed clipping/occlusion/overlap that harms reading, and the actual final
full/blank pixels and native objects have been inspected. Preserve unresolved
defects and unsupported checks. Machine checks alone cannot establish completion.

Generated lettering is a guide to hierarchy and placement, not a requirement to
reproduce its exact glyph outline, baseline pixels, punctuation advance or number
of antialiased pixels. A small pixel difference is not by itself a new defect.
If the user explicitly requests exact positioning, that requirement takes priority.
Do not continue changing an otherwise correct block solely to improve a similarity
score or make every measured margin numerically equal.

Trace hard font-size limits and exact positions to the user's brief or an approved
style, and record that origin in the shared contract. The coordinator and workers
must not add an arbitrary minimum size plus an unrelated maximum deviation from
generated lettering. Do not silently weaken a real user constraint to fit a panel.
For an actual conflict, identify it once, choose a permitted reflow/placement change,
or return the unresolved constraint to the coordinator. Do not repeatedly solve an
infeasible combination or require every worker to rediscover the same font behavior.

There is no fixed revision or render quota. Further work is justified by remaining
defects, not by a desire to keep polishing. The coordinator's extra assignment
text should contain page scope and concrete user constraints, not a second set of
quality criteria. Root review checks integration and visible regressions without
repeating every worker's measurement procedure.

## Reuse exact evidence

The cycle can reuse an unchanged render only when source bytes, renderer identity,
output dimensions and cached evidence validate. The helper records reuse rather
than pretending a render was newly executed. Inspect the corresponding actual
pixels; reuse does not approve a new page. A background/font/geometry/backend
change invalidates the relevant render. Keep the helper's cache inside the worker's
owned directory unless the host explicitly provides a shared infrastructure cache.
