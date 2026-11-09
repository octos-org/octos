---
name: pptx-runtime
description: Native PPTX build, edit, render and evidence tools. Use with the bundled pptx production skill.
---

Call `pptx_tool` with an `action`. Read the `pptx` skill and its `references/rust-runtime.md`
before making a deck. This binary provides mechanics, not content, visual decisions,
an image generator or another task scheduler. Use the existing image-generation tool
and peer sessions selected for the task. Read the returned PNG files with the host's
image-capable `read_file` tool; paths alone are not visual review.

Choose local text targets and same-role sizes from the artwork before fitting.
`patch` merges partial `set.format` values by default and reports effective
before/after formatting; use explicit `format_mode:"replace"` to reset overrides.
`split_text` partitions one native text into independently placed fragments,
retains the canonical copy, and validates complete non-overlapping `copy_range`
coverage. It cannot replace a written symbol with a claimed raster equivalent.
See the production skill's `references/rust-runtime.md` for these schemas.

`cycle` also emits native QA for every text object. `evidence_record` independently
remeasures the actual PPTX and cannot clear machine failures through model
pass/dismissal claims. Use `qa_check` for a standalone check (CLI exit 2 means
fail/unknown). Call `action: qa_help` for the embedded constraint schemas,
repair feedback, coverage limits and the distinction between mechanical and visual
acceptance. No extra model-written harness or checking script is needed.

Read `native_qa.diagnosis` before deciding to repair again. It separates measured
constraint violations, visual questions and measurement gaps. An unsupported
owner boundary is not automatically a misplaced label; a raster-contact question
is not automatically an intentional overlay. Diagnosis changes neither acceptance
nor the existing exit policy. See `references/qa-gates.md`.

For readable whole-page coverage, call `review_pack` with `cycle` and a fresh
`output` directory. Supply `previous_cycle` after a revision. It writes separate,
unresized current/blank/design quadrant images independent of detector findings,
matched before/current contextual crops for changed objects, and the persistent,
new and no-longer-detected mechanical findings. The report never labels a blank
as a design reference. Read the images using the host's image-capable tool and
give the pack to the existing independent peer; generating it is not inspection.
`pages` optionally limits an intermediate check; final review must cover all pages.
Changed acceptance targets or measurement criteria cannot count as repaired defects.

`review_pack` also produces a detector-independent inventory of **every** text
object, matched native-pixel crops, small `view_batches`, and an unreviewed
`object-review-template.json`. Follow [object visual review](references/object-visual-review.md).
`evidence_record` requires current object-level observations for ownership,
containment, alignment, direction and typography; missing/stale observations stay
unknown. Whole-page claims cannot substitute for this evidence. Use the selected
model's own vision; no additional vision service is introduced.

New tasks separately require reference fidelity for objects with design images.
Readable text cannot waive new interference or changed layout. Rust binds the
original/blank/current evidence and registers confirmed defects before repair.
See [review lifecycle](references/review-lifecycle.md) for structured observations,
safe local background repairs, third-session arbitration, and bounded exit.

The index also includes `relation_signals`: local surface directions/offsets and
canonical-text OCR reference candidates. Treat them as measured questions, not
automatic defects. `intentional` requires a measured `support_id` from that
signal's `intentional_options`; a prose explanation alone cannot close it.
No supported explanation means unknown or defect, never an invented pass.
Raster OCR/ink overlap is collected automatically. Native glyph contact with
blank-image contrast edges is measured without OCR and also checked afresh by
the final native QA gate. Read the returned contact points and local crops;
contact is a candidate interference, not proof that the artwork is an arrow or
that an overlap is unintended. Keep unresolved overlays unknown, not silently
passed or automatically erased.
Submit authored object and relation observations through
`review_submit` with `pack`, `observations` and a fresh `output`; Rust binds hashes
and checks disposition consistency. OCR-assisted anchors currently require macOS
Vision; surface measurements work without OCR. Missing evidence is not approval.

When text fits but its visible glyphs are misplaced, use `fit` with
`ink_align: "top" | "middle" | "bottom"` and optional
`targets: [{"page":"page-id","id":"text-id","box":[x,y,w,h]}]` in canvas pixels.
Horizontal placement uses the existing style alignment. The tool measures actual
isolated glyph ink with DeckCraft and returns placement in `set.box` plus
before/after measurements. Source, font size and owner geometry are preserved;
font changes require explicit `min_size_pt` / `max_size_pt`. Inspect the composite after
applying the proposal. For local slanted labels, establish the frame with
`geometry` first, then include the same rigid angle as `targets[].rotation`.
Alignment follows the label axes; the entire rotated frame must remain inside
the owner and canvas. Infeasible targets, angle mismatches, perspective and
justified text get explicit feedback instead of guessed edits.

If unused frame height alone prevents a valid placement, the tool reduces that
height while retaining the largest feasible line-box space. It preserves font,
wrap width, complete line layout, and the original owner/canvas constraints.
The result records `frame_height_adjustment`; apply the whole proposed box,
including its height. A genuine glyph/owner conflict remains infeasible.

Read the returned `mode` and `placement_proposal_count`: `size_measurement`
without `ink_align` computes no placement. Do not report size-only output as
an aligned label. Only a `proposed` ink-alignment row with `set.box` supplies
an applicable placement; preserve and inspect any infeasible reason.

Keep coordinate spaces explicit. When measuring a full-page image, call
`geometry` with both `image` and `spec`: input region/padding are image pixels,
and the returned box/region are converted to spec canvas pixels. The result
records both dimensions and scale. Without `spec`, output stays in image pixels
and must not be copied blindly into page data. Do not pass a crop as a full page;
crop coordinates require their original offset first. Aspect mismatches are
rejected, but matching aspect alone does not prove an image is a full page.
`spec` is the deck JSON path string; `quad` remains a separate top-level field.
Read `owner_boundary` rather than filtering it away: the proposal is geometry
of the supplied corners, not proof that those corners match the artwork.

When changing a text column's width, add `reflow:true` to `fit`; otherwise a
target only requests translation, not new wrapping. For repeated artwork/line
collisions, use `layout_search` rather than repeatedly guessing coordinates.
Choose an independent local `area`, include solid illustration interiors in
`forbidden`, and inspect the returned obstacle overlay and candidate PNGs.
`register:true` freezes those constraints for automatic checks of the actual
PPTX in later `cycle`, `qa_check` and `evidence_record`; a prose pass cannot
clear a remaining collision. See [placement](references/placement.md) for syntax
and conservative detector limits. Apply a chosen proposal through the existing
page source, rebuild, and check the composite with its neighbors. An empty
candidate list means this bounded search found none, not that the page is impossible.

`fit` supports explicit size intervals and `max_lines`, including labels that
already fit their large frames but must occupy one line in a smaller local slot.
Omitted bounds retain the current size; equal min/max requests one exact size.
The search keeps explicit newlines and rejects emergency breaks inside words.
It reports exact font-face provenance and returns `font_unresolved` with no edits
when fitting would use fallback or missing glyphs. Select an installed face that
covers the text; do not silently accept another font's measurements.
Use `reflow:true` with a measured local target when its width changes. Unsupported
options now fail rather than being ignored. `state:no_fit_within_bounds` or
`placement_infeasible` has no applicable edits; never apply a partial shrink as
if it met the target. To keep a declared same-role list uniform, use
`group:{page,id,size_pt}` with explicit targets for every member, `reflow:true`
and `ink_align`. The chosen size is exact; do not combine it with min/max.
If one member fails, all group edits are withheld and `group.blocking_objects`
identifies the conflict. Membership/targets remain model decisions. Independent
maximum-size fitting can yield different sizes and is not a uniform-list solver.

`inspect` reports actual `font_runs` (family/point-size/bold/italic), rendered sizes
and source-format mismatches per object. For repeated labels, declare each
same-role set in its page's `typography_groups`, e.g.
`[{"id":"steps","members":["step-a","step-b"],"fields":["font","size_pt"],"basis":"same-level step descriptions; heading excluded"}]`.
The producer decides membership from the design, not page numbers. `cycle`,
`qa_check` and `evidence_record` check the reopened PPTX formatting and produce
`TYPOGRAPHY_GROUP_MISMATCH` for differences. Do not include deliberate hierarchy
differences. Existing group constraints are retained during repair; deleting
members does not count as fixing typography. Groups are an explicit contract,
not automatic semantic recognition. See [parity audit](references/codex-parity.md)
for remaining differences from the local Codex scripts.

`cycle` generates whole-page comparisons for every declared design reference.
Read the comparison manifest and actual images; classify all regions in the
existing review's `comparisons` entries. `evidence_record` keeps missing/stale
coverage incomplete and reuses reviews only for unchanged image hashes. For an
older cycle use `compare_cycle`, without rebuilding its PPTX. Differences are
review prompts, not automatic defects or a quality score. Comparison reports also
include `label_evidence`: contextual design/render/difference panels and placement,
line-clearance or uncertainty findings per native text object. Follow the bundled
`pptx/references/label-comparison.md`; record findings individually in
`review.comparisons[].label_findings`, with their current evidence hashes. Whole-page
font-difference explanations cannot close line-clearance or alignment findings.

The same executable accepts JSON on stdin: `octos-pptx pptx_tool < request.json`.

Cycle inspection now includes `composite_ink`: actual full-minus-derived-blank
components attributed to reopened character cells. Read missing/ambiguous-cell
feedback rather than equating isolated glyph presence with final visibility.
These unknowns can mean occlusion, low contrast or mapping limits; examine their
context crops. Attribution itself does not prove correct arrow/card relationships.

For residual bitmap text, use
`{"action":"raster_overlap","cycle":"attempt/cycle.json","output":"review/raster"}`
(optional `page` is a source ID). It OCRs the actual derived blank, joins text
detections to overlapping native objects, and returns a report with matched
design/blank/render crops. Open those images and ask the existing reviewer peer
to judge duplication versus legitimate nearby symbols. No new model caller or
task loop is introduced. OCR is currently macOS-only; no candidates is not a pass.
`octos-pptx mcp` exposes DeckCraft's existing headless MCP for compatible clients.
Both modes confine document access to `OCTOS_SESSION_WORKSPACE` (default: current directory).
Use fresh output paths: the tool refuses to overwrite deck sources or previous attempts.

Text localization: `action: ocr` (macOS Vision) returns hash-bound text candidates,
confidence and pixel quadrilaterals. `doctor` reports availability. It is not an
exact transcript or visual acceptance check. Under Octos, `evidence_init` binds
to the host's actual execution identity; omit hand-written owner nicknames.

Before a repair cycle, read [review lifecycle](references/review-lifecycle.md) for immediate defect registration, a predeclared independent verification route, and safe local evidence reuse.

Use `operations` with ordered `steps` (ordinary action requests) and optional workspace
`images` to return crops/OCR/patch/cycle results and actual pixels in one tool
response. It stops at the first failure and reports completed steps; no rollback.
Use `review_view` with `cycle`, `index` (view-index.json) and zero-based `groups`
to return bound reference/current images with measurement candidates. These
operations never approve visual quality.
