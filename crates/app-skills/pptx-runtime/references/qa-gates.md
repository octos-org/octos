# Native QA gates

The Rust runtime enforces measured constraints in addition to model visual review.
No page numbers, domain phrases or known defect coordinates belong in this implementation.

## Operations and result authority

- Every `cycle` includes a `native_qa` summary and a hash-bound report for **all**
  native top-level text objects, including objects with no warning.
- `{"action":"qa_check","cycle":"attempt/cycle.json","output":"qa.json"}`
  reopens and remeasures the actual PPTX. It does not trust a cached inspection,
  an earlier QA report, or a model's pass/dismissal. Output is optional.
- The standalone command exits 0 for a mechanical pass, 2 for fail/unknown, and
  1 for an operation error. A successful tool invocation is not a QA pass:
  consume `status`, `failed`, `unknown`, and the report.
- `evidence_record` **always** reruns this check and combines its results with
  the existing package/copy/artwork/readability checks. Omitting `qa_check`,
  supplying `review.qa.status = "pass"`, dismissing a machine ID, or modifying
  an inspection JSON cannot clear measured findings.
- `evidence_status.native_qa` exposes the recorded machine result. Tool-generated
  QA records are bound into the existing continuation evidence artifact list.
  No separate task scheduler or kernel-specific PPTX policy is introduced.

The report contains stable finding IDs, object IDs, measurements, context
viewports and repair guidance. Open the matching actual rendered crop; native
ink measurements deliberately exclude artwork unless an exclusion region was declared.

`BASELINE_MISMATCH.observed` gives `actual_deg`, `expected_deg`, `delta_deg`,
`tolerance_deg`, `unit: "degrees"` and the target source. These are angles,
not vertical coordinates. A stored PPTX rotation of 340 degrees means -20
degrees; moving a text frame up or down does not change its angle. The expected
angle comes from the explicit owner `baseline_deg` or its quad orientation.

## Deterministic checks and limits

Hard findings include missing/duplicate/mismatched native text, missing glyphs,
visible ink outside the slide, actual glyph overflow, and violation of a declared
local owner, baseline, line-count, centering or exclusion constraint.
A font line-height estimate alone is not a visible overflow finding.

Native-qa-v3.4 also compares reopened PPTX run family/point-size/bold/italic with
the resolved source (`DECLARED_FONT_MISMATCH`). `inspect.font_runs` exposes the
actual values per object, alongside rendered sizes and fallback provenance.
Optional page `typography_groups` declare same-role text IDs and the fields
that must match. Differences become `TYPOGRAPHY_GROUP_MISMATCH` in the regular
cycle/evidence checks; the producer cannot clear them with a prose pass. Group
membership/fields cannot disappear during repair. This does not infer semantic
roles or require headings, body and footnotes to share one font size.

Missing/legacy owner geometry, unknown measurement, fallback fonts and
unsupported grouped/table/chart/opaque containers remain explicit unknowns.
A generic rectangle without `scope: "label"` and a nonempty independent
`basis` is not proof of an artwork boundary. Native QA now also samples each
proposed quad side against the actual blank pixels. A prose assertion alone
cannot clear `OWNER_BOUNDARY_UNVERIFIED`. The report exposes support for every
edge and the source image binding. This conservative check supports outlined
or contrast-bounded quadrilaterals; borderless, curved, textured or ambiguous
labels can remain unknown. It does not establish semantic ownership or force
every legitimate layout to have a box. Do not add borders merely to satisfy it.
No clean mechanical result sets `quality_approved` to true.

## Diagnose before another repair cycle

`cycle`, `qa_check`, `review_pack`, `evidence_record` and `evidence_status`
return `diagnosis` alongside the native result (under `finding_summary` for a
review pack). The categories are computed by Rust, not supplied by a model:

- `measured_constraint_violations`: a measured declared requirement failed.
  Repair against the recorded target and verify the actual result. The target's
  semantic ownership is still a separate review question.
- `visual_questions`: measured raster contact or ambiguous composite visibility
  needs local original/blank/actual comparison. Contact alone does not prove a
  defect; matching OCR rectangles alone do not prove an intentional overlay.
- `measurement_gaps`: required input, coverage or supported geometry is missing.
  Borderless text may be correct despite an unverified quadrilateral boundary.
  Supply missing evidence, diagnose the tool limitation or hand back unresolved;
  do not move text, add a border or enlarge a region just to reduce this count.

Counts are check occurrences, not independent defects. The same object can have
several findings. The compact `by_code` groups show at most three example object
references; the complete findings stay in the bound native report. `next_step`
routes diagnosis; it grants no permission to override failures or unknowns and
does not replace the host's continuation/exit policy. A reduced unknown count
after reclassification is not evidence that a persistent visual defect improved.
The current tool still cannot automatically approve unsupported geometry.

For inexpensive feedback before a rebuild, use the existing `geometry` action
with optional `image` and `quad`/`region`. The quad is then in source-image
pixels. The result adds per-edge `owner_boundary` support to the existing
rotation and padded text-frame proposal; it does not modify the source page.
This lets the producer check a candidate local boundary without cycling the deck.

`owner_audit` groups every used region with its linked text IDs, independently
measured edge support and source-image binding. Inspect the whole group: several
labels sharing a panel does not prove that each label fits its own card. Pixel
support establishes geometry only, not the correct text-to-card relationship.
Each entry has `page` (1-based numeric display order), `page_id` (source ID),
`region_id` (local region ID), `quad`, `object_ids`, and `boundary.status/sides`.
Filter source IDs with `.owner_audit[] | select(.page_id == "overview")`;
do not apply string matching to numeric `page` or confuse `region_id` with a
page ID. These are checks of the supplied boundaries, not newly discovered
correct boundaries. View the actual artwork and measure local frames yourself.

If an earlier envelope was incorrectly declared `scope: "label"`, prose must
not make that mistake permanent. Starting with native-qa-v3.2, an unsupported
envelope can be refined to a contained child whose edges are supported by the
unchanged artwork. Declare `parent`, `scope` and `basis`, retain the original
envelope, and inherit its constraints. Existing supported local labels and
explicit centering/baseline/reference-position targets remain frozen. Expansion,
an unsupported replacement, removing constraints, or changing the background is
not this refinement. Borderless/curved labels can remain unknown; do not invent
borders or claim the tool has verified them. Refinement updates the measurement
target, so fewer findings alone do not count as visual repair or reset history.

Native-qa-v3.3 keeps the originally recorded free-form `basis` description but
does not treat rewording that description as a geometry change. It never uses
that wording as pixel proof. Quad, scope, content, artwork and explicit numeric
constraints remain protected; an omitted required basis still leaves ownership
unverified. This avoids blocking repair on narrative spelling changes.

## Reference correspondence and local closure evidence

Before a design/blank subtraction can count as reviewed, the tool checks
whether the blank's artwork edges still correspond spatially to the design.
`reference_alignment` reports `supported`, `incompatible`, or `unknown`.
This uses normalized artwork samples, not text recognition or an aesthetic
score. Full-resolution difference views remain unchanged. Incompatible or
insufficient evidence keeps comparison coverage unknown even if the model
accepts every difference region. Restore a correct design/blank pair; do not
classify a different composition as a font change. Old v2 comparisons need
regeneration. `evidence_record` recomputes this measurement from bound inputs.

Record each actual visual defect with a stable ID, source page ID and local
viewport in **render pixels**, before repairing it:

```json
{"id":"caption-collision","page":"overview","box":[30,30,380,170],
 "check":"readability","state":"open","note":"Caption touches the card edge",
 "evidence":"attempt-a/rendered/slide-001.png"}
```

The tool retains the initial local pixel digest. Submitting `resolved` needs
the same viewport and changed actual local pixels. No change, a moved viewport,
a stale baseline, or missing before evidence leaves it open/unknown with a
specific `closure_check` reason. An unrelated page edit does not suffice.
Unchanged accepted repair evidence can be reused. Later changes in that local
area reopen a retained closure for review. Legacy prose-only closures are not
silently carried forward as verified repairs.

This requirement covers **copy, artwork and readability**, including legacy
closures. Calling an alignment problem a copy problem does not bypass it.
If `page_number` is supplied, it is the 1-based display position in the checked
cycle, and must agree with `page` (the source ID). The evidence PNG must belong
to that same page. IDs and filenames are not ordinal page numbers.

Changed pixels are **not** proof that a defect disappeared. The independent
reviewer must still inspect the before/after crop and surrounding context.
`dismissed` remains a model judgment for false positives/intentional design,
not a claim of repair, and cannot clear deterministic QA failures. This guard
does not make a one-pixel change or a false subjective dismissal trustworthy.
Final full-page and whole-deck review remains required; a list of inspected
page IDs does not prove the model actually judged every relevant relation.

The native inventory does not certify raster captions, subjective aesthetics or
undeclared artwork collisions. Use the existing design/blank/render comparison
and independent peer review for those. Do not describe a geometry pass as a visual
pass, nor convert unknown into accepted with a prose explanation.

## Local constraints

Use actual artwork geometry, independent of the candidate text frame:

```json
{
  "quad": [[40,40],[400,40],[400,200],[40,200]],
  "scope": "label",
  "basis": "Local card boundary measured from the current artwork",
  "max_lines": 1,
  "ink_align": "center",
  "ink_valign": "center",
  "forbidden": [[[170,60],[190,60],[190,100],[170,100]]]
}
```

These are synthetic syntax examples, not production coordinates.
All constraints except the existing quad/scope/basis are optional. Centring
applies only when explicitly declared; prose need not be centred.
Exclusions count actual native glyph pixels, rather than intersecting empty
text-frame rectangles. Centring uses a fixed physical-unit tolerance; the
request cannot supply a relaxed threshold or an ignore list.

At the first `evidence_record`, the tool retains the source copy, references,
declared owners and constraints. Moving text, changing its fit and re-rendering
is a repair. Deleting source text, changing an existing owner/constraint, removing
a reference, or changing artwork yields a constraint-change result, not a pass.
Previously unspecified ownership/reference data may be added; existing targets
cannot be relaxed. The retained contract survives subsequent attempts.
An unverified legacy panel may be refined into independent local child owners:
retain the original panel unchanged; add a new local quad with `parent` equal
to the old region ID, `scope: "label"` and an artwork-based `basis`; bind the
relevant text to that child. The child must be entirely inside the original
envelope and preserve its explicit semantic constraints. Separate slanted
labels need separate owners; the candidate text's own bounds are not an
independent measurement. A successfully recorded child becomes the frozen
target; it cannot be renamed/replaced to escape its constraints. This is a
constrained refinement, not a reset or authorization to alter the old panel.
Legitimate content/design revisions need a separately authorized task-contract
revision; this version deliberately provides no model-callable reset/accept-all
operation. Preserve history rather than deleting it to proceed.

## Enforcement boundary

This gate resists bypass through the normal Rust tool API. It does **not** provide
OS isolation from a process that can edit the executable, all source references,
policy and history files. Hashes bind bytes but are not signatures or authority.
A host requiring that stronger guarantee must protect its verifier and policy
store from agent writes and make delivery status consume the verifier result.
Merely installing this tool does not make every arbitrary shell export verified.

## Validation

`cargo test -p octos-pptx` includes synthetic negative and positive controls:
forged reports, model dismissals, old QA reuse, owner expansion, source deletion,
unknown containers, true glyph overflow vs harmless font metrics, exclusion
collisions, page reordering, content replacement, canvas scaling, and CLI exit
codes. These test enforceable behavior; they are not an aesthetic score.

## Feedback parity with the earlier Python workflow

For width changes use `fit` with `reflow:true`; targets otherwise remain
translation-only. `layout_search` can propose unchanged-copy/unchanged-font
placements that avoid artwork edges, measured exclusions and neighboring text.
Registered placement contracts are automatically rechecked here against the
actual PPTX, including when evidence is recorded. See [placement.md](placement.md).

Two useful feedback capabilities are available without Python:

```json
{"action":"fit","spec":"source.json","objects":["scene/caption"],
 "ink_align":"middle",
 "targets":[{"page":"scene","id":"caption","box":[180,180,240,60]}]}
```

`fit` measures isolated native glyphs with the same DeckCraft renderer as export,
including the existing font, margins and vertical anchor. Horizontal alignment
uses the current style. `ink_alignment` reports the target, before/after ink,
translation and feasibility. Apply `set` only for a proposed result, then rebuild
and inspect actual composite pixels. The proposal never changes the source,
owner or artwork and never shrinks without the existing explicit size option.
For a rigid slanted label, first establish its frame with `geometry`, then pass
that same angle as `targets[].rotation`. The target box is measured before
rotation about its center. The tool aligns actual glyphs along those local axes,
preserves the text rotation and rechecks the rotated frame against the owner and
canvas. Omitted rotation means zero; mismatched angles return `unsupported`.
It never infers semantic ownership, perspective, shear or justified placement.

Measure in the correct coordinate space. A render width can differ from the
spec canvas width. `geometry` with `image` and `spec` converts a measured
full-page image region and image-pixel padding into canvas coordinates and
returns `coordinate_transform` with both dimensions, scale and input quad.
Its `owner_boundary` measurements remain in source-image pixels, explicitly
labeled. Without `spec`, geometry returns image pixels and a conversion warning.
Never treat crop-local coordinates as full-page coordinates; the tool rejects
aspect mismatches but cannot identify a same-aspect crop from dimensions alone.

```json
{"action":"review_pack","cycle":"attempt-b/cycle.json",
 "previous_cycle":"attempt-a/cycle.json","output":"review-b"}
```

This creates readable, separate actual-render/blank/design crops at the same
normalized viewport, with four overlapping views per selected page. They cover
unflagged regions too. It also creates matched before/current context images for
changed native objects and compares the hash-bound mechanical reports. The
optional `pages` list selects an intermediate scope; omission means all pages.
Alternatively pass `page_numbers` with the user's 1-based display page numbers;
do not combine it with `pages`. The tool returns `page_map` binding every display
number to its source ID, editable render and blank render for this exact cycle.
Crop records also include `page_number`. Use those bindings to locate source
objects and review images, rather than guessing an ID from a displayed number.
No page IDs, copy or coordinates from a production deck are built into this tool.

`finding_changes` distinguishes persistent/new/no-longer-detected measurements.
Changed criteria, engine, constraints or measurement coverage make the reports
incomparable; they do not create repair credit. A local image change is not a
visual verdict. The tool does not call a model, mark pictures inspected or alter
the existing continuation state. Supply these images to the existing producer
and independent peer through the host's image-capable tools.

The earlier Python scripts had these additional mechanisms; native-qa-v3.5 adds
the first two with explicit evidence limits:

- `native_inspect.attribute_ink` attributed connected components of the actual
  full-minus-blank image to rendered character cells, leaving ambiguous pixels
  unknown. Cycle inspection now also subtracts the actual derived-blank render
  from the actual full render, extracts connected components and attributes them
  to reopened native character cells. `composite_ink` exposes missing, ambiguous
  and unassigned signal. Uncertain visibility becomes `COMPOSITE_INK_UNVERIFIED`
  or `UNATTRIBUTED_COMPOSITE_INK`; it cannot be passed using isolated-glyph counts.
  Cell geometry comes from DeckCraft, not an independent Office/PDF renderer.
  This does not establish semantic ownership or guarantee collision-free placement.
- `raster_overlap` joined OCR detections on the raster-only layer with native
  objects, then requested a separate visual judgment on each candidate. The new
  `raster_overlap` action performs that join and produces matched design/blank/render
  crops for the existing peer. Its candidate queue is not a verdict. OCR currently
  requires macOS; zero candidates does not establish residual-text clearance.
- `review_pack` automatically joins blank OCR with actual ink envelopes. It also
  exposes `RASTER_STROKE_CONTACT`, measured by intersecting isolated glyph alpha
  with contrast edges of the actual derived-blank render. This channel does not
  require OCR, an author-supplied obstacle, or an extra model-written script.
  `ARTWORK_CONTACT_UNVERIFIED` is recomputed by final native QA, including when a
  review omits relation evidence or its saved inspection has been edited. It is
  an unknown candidate, not an automatic defect: real photographic textures or
  deliberate overlays may need external intent evidence. No prose dismissal is
  currently supported for this measurement; keep unresolved cases incomplete
  under the existing exit policy rather than deleting legitimate artwork.
  Low-contrast lines and uniform interiors may be missed; zero edge contacts is
  not a whole-page clearance. Returned contact locations use canvas pixels,
  counts use unique actual-render pixels, and exported samples are bounded.
- `pptx_host_check` and `defect_followup` could invoke a configured independent
  visual model on whole pages, dense crops and each historical defect. That was
  model-assisted review, not a Python geometry algorithm. The native runtime
  must reuse the host's actual peer/reviewer lifecycle, not silently add another
  model caller or report that a changed crop proves closure.

Neither implementation fully detects arbitrary diagonal/curved artwork, wrong
semantic ownership, duplicate graphic symbols or all typography relationships.
Evaluate candidate detection recall, false positives and actual repair success
on independent synthetic and real layouts. Passing these unit tests establishes
mechanics, not parity of visual review quality or an aesthetic score.

# Frame-height fitting

Ink alignment may reduce unused native text-frame height, matching the line-box
room calculation in the shared Codex fitting workflow. It does not shrink the
font or widen the owner. The runtime first checks unchanged wrapping/line layout,
then measures glyphs again and requires the complete rotated frame to fit the
original owner and canvas. `frame_height_adjustment` records the change. This
does not establish semantic ownership or approve the composite slide.


See [review lifecycle](review-lifecycle.md) for immediate finding registration, a separately frozen independent visual route (native failures stay blocking), and verified local observation reuse.
