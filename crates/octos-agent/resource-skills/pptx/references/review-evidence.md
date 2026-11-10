# Review evidence without a quality controller

Use this short pass for final editable-deck review and page-worker handoff. The
model selects repairs and decides delivery. Existing tools supply evidence; no
external approval, new orchestration framework or fixed revision count is needed.
Record observations in the existing review file, not another parallel checklist.

## Three independent comparisons

| Comparison | Read and observe | What cannot establish a pass |
| --- | --- | --- |
| Current manuscript → actual PPTX text and notes | Reconcile required occurrences with native objects and intended card/node. Read complete statements: subject, action, qualifiers, negation, scope, comparisons, numbers/units and formulas. Check claims about notes, citations or appendices against those actual parts. | Matching keywords, a character-set test, or the producer's rewritten page data validating itself. |
| Approved labeled design → supplied/embedded blank artwork | Compare non-text objects and relationships: connector direction/endpoints, branches, boxes, icon count, bullets, quantitative marks and available label space. Separate intended text removal from changed artwork. | A removal prompt saying “preserve everything”, zero OCR output, or native full/blank layer parity. Those do not prove that text removal preserved the original design. |
| Actual final PPTX render → manuscript and visual intent | Look across the whole page, including unflagged areas. Check label-to-owner association, borders/icons/connectors, contrast, wrapping and peer alignment at readable scale. Readability and reference fidelity are separate observations. | A contact sheet, a clean overlap report, or accepting an inherited defect merely because the supplied blank contains it. |

Apply only relevant comparisons: a native deck without a labeled reference has no
reference-fidelity check. Mark the reference unavailable; do not invent one. For
literal reconstruction retain wording; when editorial condensation is authorized,
check preserved meaning rather than demanding identical strings. Check content
against the independently supplied manuscript, including Chinese prose, not only
Latin identifiers or numbers. Generated lettering can itself be wrong.

`pptx_tool` 的 `inspect` 读取实际原生文字、备注和形状清单，不经过 PDF。完整内容与原文逐项核对；原生对象中出现一个关键词不证明整句或来源完整。

For an artwork discrepancy, identify the layer responsible before editing. Prefer
repairing the source artwork when permitted, or adapt the native placement without
changing the intended relationship. If a shared input is read-only, report the
specific discrepancy to its owner. Do not silently delete a required label, shrink
everything, add a covering patch, or expand a declared owner to hide a conflict.

## Make small defects visible

First inspect the final rendered page without a prior “pass” verdict. Describe
observations before deciding whether they matter. A useful question is:

> For each card/row/branch in this view, which label belongs to which object?
> Identify any visible clearance, alignment, contrast, duplication or association
> problem. State what is unreadable or uncertain. Then compare with the reference
> and manuscript; distinguish intended edits, source defects and conversion defects.

A full-page view establishes composition. If small content is not legible, use
overlapping region views covering the page, including regions without automatic
findings. Include the whole card and neighbors for an ambiguous association; a
tight crop of the text alone removes the evidence needed to judge placement.

Use `pptx_tool` `crop` on current full-page renders and the relevant source/blank
images. Keep matching normalized viewports when their pixel sizes differ. Use
separate images so dense text is not squeezed into a composite thumbnail. Sweep
unflagged areas as well as reported defects; include the whole owner and neighbors.
Use `compare` for additional contextual difference panels, never as a quality score. When
continuing a user-selected different builder, use its bound renders and equivalent
crops; this does not authorize changing the selected reconstruction workflow.

For reconstruction from an approved image, declare each page's `design` in the
source. `cycle` automatically subtracts the same blank artwork from both the
labeled design and actual render for **every** reference page. Read its
`comparisons/manifest.json`, whole-page images and marked-region viewports;
record each page and every region disposition in the existing review file.
`evidence_record` cannot pass artwork/readability from `inspected` page lists
alone. Missing comparisons, unread regions and unresolved differences remain
incomplete; a model-classified defect fails. A zero-difference page still needs
whole-page inspection. No reference is required for a genuinely reference-free
native deck. Do not drop an existing reference to avoid checking it.

The native tool returns matched views and a blue/red location-difference view;
see [Rust runtime](rust-runtime.md) for review schema, automatic reuse and the
`compare_cycle` action for older decks. Use the current cycle's blank-rendered page
so it shares the candidate geometry. Full-page scaling is recorded; local label
warping is not allowed to hide displacement. Read original design, blank and
render too: font shapes, intentional edits and artwork drift can all produce
differences, and dark text on a dark icon can disappear in subtraction.

For reconstruction labels, also read [label comparison](label-comparison.md).
The report supplies individual contextual panels, ink/line-clearance measurements
and peer-alignment candidates. Whole-page region dispositions do not cover these
findings: record each in `label_findings`, bound to its current panel. Inspect
evidence before the producer's pass verdict. A clean containment check cannot
establish clearance from an artwork line inside the same container.

`local_label_ownership_unverified` means **not checked**, not harmless. The
inspection report groups such objects into `visual_review_queue` with normalized
context viewports. Inspect each relevant group (nearby labels can share one view),
including its complete container, icons and connecting lines; expand a viewport
when necessary. In the existing review, identify the checked objects and record
observed clearance/ownership, an intentional difference with its reason, or an
unresolved issue. Do not mark a whole page clean merely because overflow is zero
or known defects were fixed. Sweep repeated/symmetric components individually.
Reuse the observation for unchanged source and rendered bytes. Changing only an
owner region does not fix the image or provide new visual evidence.

## Close an observation with relevant evidence

In the existing review record keep concise page/region coverage, material findings
and dispositions. For a finding retain the object/region, observed problem, evidence
path and candidate identity. After a repair, inspect that same region and neighbors
in the new render. For a repeated component, inspect its peers where the defect
could recur. Do not copy one component's verdict to every occurrence.

Use distinct dispositions: repaired and rechecked; accepted intentional difference
with a reason tied to the brief; unresolved; or not inspected. Missing coverage is
not a clean page. A known defect cannot disappear from `open_findings` merely because
a later broad question returned “no issues”. Preserve and resolve contradictory
observations; do not repeatedly request an identical review until it says “pass”.

A short existing-record observation can be sufficient:

```json
{"region":"comparison row","evidence":"current-context.png",
 "observation":"Second caption touches its icon; the peer row has clear space.",
 "disposition":"unresolved"}
```

Generating images does not prove they were viewed. A worker's inspection booleans
record its claim; they do not certify quality. The coordinator checks integration
and remaining findings, reusing unchanged page evidence. Keep provenance for shared
inputs so an upstream artwork problem is fixed once rather than rediscovered by
every worker. Exact rendering evidence is reusable; author-written claims about
meaning still require reconciliation with the original brief.
