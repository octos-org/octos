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

`scripts/inspect_slide_layers.py CANDIDATE.pptx --out INVENTORY.json` extracts actual
native strings and `speaker_notes` per slide without a PDF. It distinguishes an
absent notes part from an empty body; neither proves that a promised citation exists.

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

The existing `native_inspect.py` can reuse a bound cycle and generate a sweep:

```sh
python scripts/native_inspect.py --cycle CYCLE.json --out NEW_REVIEW_DIR \
  --sweep-pages PAGE_ID [PAGE_ID ...]
```

It creates separate source, editable and blank crops with matching normalized
viewports. Open the editable crops and the relevant source/blank counterparts.
Separate images avoid squeezing three dense panels into one model thumbnail.
The sweep uses a general overlapping grid, not inferred card boundaries or defect
locations. If an object crosses a grid boundary or remains unclear, inspect its
full context using existing object crops or the image viewer. No pixel-similarity
score is an acceptance target. When continuing an existing user-supplied project
or a user-selected different builder, use that builder's bound renders and equivalent
crops. This review compatibility does not change the normal reconstruction choice
in native-builder.md, or authorize replacing an already selected workflow.

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
