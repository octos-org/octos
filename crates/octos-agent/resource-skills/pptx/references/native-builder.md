# Bundled native-text builder

Use the supplied `scripts/native_deck.py` for the normal blank-artwork + editable
text workflow. It is reusable infrastructure, not a previous deck's solution.
Do not write another textbox, font-conversion, blank-deck or handoff framework.
Create or repair page data. Text items accept optional `rotation` (clockwise
degrees about the frame center); regions accept evidenced label quadrilaterals
as well as legacy rectangles. For tilted signs, use [label geometry](label-geometry.md)
to measure the local plane, propose a rotated frame, and inspect actual ink.
Use [font proposals and revisions](fit-and-revise.md)
for font calculations and automatic attempt bookkeeping. Extend the engine only for a demonstrated unsupported
feature; keep the old engine/version with already accepted proofs. Preserve the
selected design/reconstruction path and its review evidence when adding that primitive.
Permission to adapt an unsupported primitive is not permission to replace the whole
builder or introduce a parallel layout framework. For an existing user-supplied
project or an explicitly selected different engine, retain that engine and collect
equivalent evidence; this exception does not apply merely because new code is easier.

The engine does not decide authoritative copy, text ownership, font hierarchy or
placement. An OCR draft remains unreviewed. It does not remove raster text from
artwork, infer correct diagram labels, certify actual glyph fit, or make artwork
editable. Agent pixel inspection and the existing copy/visual criteria still apply.

This is the reconstruction stage, not a substitute for page design. If a new
image-first deck has no background yet, create and inspect the full-page design
and derive its blank artwork using [paired design](paired-workflow.md). A required
background field means that input is needed; it does not mean the requested
workflow cannot be used. Rasterizing an unaccepted layout merely preserves that
layout. Ordinary editable text over artwork does not require replacing the
illustrations with fully native vector objects.

## Input data

Use a JSON document with `schema_version: 1`, `canvas`, `styles`, and `pages`.
All geometry uses the document's pixel coordinate space, origin top-left; font
sizes use points. Pixel and physical aspect ratios must agree. Assets resolve
relative to the file containing their page. Each page can be inline or referenced
as `{"path":"pages/overview.json"}`, which gives workers independent data files.

This is a synthetic example, not a style or layout to impose on a real deck:

```json
{
  "schema_version": 1,
  "canvas": {"width_px": 1600, "height_px": 900, "width_in": 12, "height_in": 6.75},
  "styles": {
    "heading": {"font": "Arial", "size_pt": 30, "bold": true, "color": "000000", "locked": true},
    "label": {"font": "Arial", "size_pt": 16, "color": "222222"}
  },
  "pages": [{
    "id": "overview",
    "background": "art/overview-blank.png",
    "design": "art/overview-design.png",
    "copy": {"heading": "Example workflow", "node-left": "Input", "node-right": "Output"},
    "regions": {"left-card": [100, 220, 500, 300], "right-card": [1000, 220, 500, 300]},
    "texts": [
      {"id":"heading", "text":"Example workflow", "style":"heading", "box":[100,60,1400,100]},
      {"id":"node-left", "text":"Input", "style":"label", "box":[140,260,400,70], "region":"left-card"},
      {"id":"node-right", "text":"Output", "style":"label", "box":[1040,260,400,70], "region":"right-card"}
    ]
  }]
}
```

`copy` records every intended text occurrence separately, even when strings repeat.
Each textbox has an `id` and matches `copy[id]`, or an explicit `copy_id`. All
occurrences must appear once. Only whitespace differences are normalized; numbers,
case, punctuation and operators remain significant. Independently reconcile this
inventory with the manuscript and design: an inventory copied from bad OCR cannot
prove semantic completeness. To split a label around existing artwork, record
the intended parts as separate occurrences after checking the source.

`box` and named owner `regions` are `[x,y,width,height]`. Determine regions from
artwork, not from the textboxes being checked. Native frame containment is a useful
check but cannot establish rendered glyph containment or correct ownership.

Styles require `font` and `size_pt`; optional fields are `color` (six-digit RGB),
`bold`, `italic`, `align` (left/center/right), `anchor` (top/middle/bottom), `wrap`,
`margins_px` ([left,top,right,bottom]), `line_spacing` (multiplier),
`space_before_pt`, and `space_after_pt`. Defaults are black, left/top, wrapping,
zero margins/paragraph spacing and single line spacing. A text's `format` object
overrides an unlocked style. `locked: true` prevents local overrides of shared
typography. Use `\n` for explicit line breaks. The engine never auto-shrinks text.
Assets may be `{ "path": "...", "sha256": "..." }` to require exact bytes.
The canvas is a coordinate system, not an instruction to downsample artwork.
Keep original-resolution backgrounds embedded; create separate small previews if
needed. For example, 1280×720 layout coordinates can use a 3840×2160 background.
Do not resize the source to the canvas dimensions just to simplify placement.

## Build, review, patch

Use the workspace's Python with python-pptx, Pillow and lxml installed:

```text
python <skill>/scripts/native_deck.py doctor
python <skill>/scripts/native_deck.py build --spec deck.json --out attempts/candidate-a
python <skill>/scripts/native_cycle.py run --spec deck.json --out attempts/cycle-a --workspace <workspace>
```

`doctor` reports versions; it does not perform font calibration or approve a page.
Reuse the deployed environment check. Inspect a real render when fonts change or
visible evidence suggests a mismatch; do not rebuild a generic calibration suite
before each assignment.

`build` creates editable and derived blank PPTX, resolved input, diagnostics and
hash-bound records. CLI output is concise; complete diagnostics stay in the linked
`build.json`/`cycle.json`. `--page ID` builds only that page using the same implementation
as the whole deck. A one-page build includes `page.py` and exact engine snapshots
for the existing `build_slide(presentation, job)` handoff interface; no worker
adapter needs to be authored. Do not edit this generated adapter to fix placement.

`native_cycle.py run` combines build and both checked renders. It requires the
existing `tools/service.json` `/jobs` renderer, normally waits at most 35 seconds,
and returns exact image paths. Exit 2 means pending: resume the SAME command/output
to collect the SAME jobs, or use its runtime wake-up receipts. Exit 1 means error.
Unchanged inputs reuse the build; changed inputs require a fresh output directory.
It does not approve pixels or declare the page complete.

When the host provides `render_cache_identity` in `tools/service.json`, the cycle
reuses exact-source renders across candidate attempts. The host changes this
identity on backend restart or any renderer/font/configuration change. Without
it, rendering safely remains uncached. The default cache is inside the candidate's
parent directory, preserving page-worker ownership. ZIP serialization is normalized
without changing any package part bytes; a text-only change can therefore reuse
the identical derived blank. Each candidate retains its own verified render copies
and truthful reuse provenance. Do not hand-edit cache records or render reports.

Use the shared measurement helper after a ready cycle:

```text
python <skill>/scripts/native_inspect.py --cycle attempts/cycle-a/cycle.json --out attempts/cycle-a/inspection
```

Or add `--inspect` to `native_cycle.py run` to build, render and produce this report
in one command. Inspection binds the unchanged cycle record. The returned panels
and findings still need actual visual judgment.

It verifies the exact PPTX/PDF/image bindings, measures native copy/font runs and
visible text ink from the full/blank difference, and produces source/full/blank
comparison panels. Its compact
output links the full report; read actual images and relevant object rows, not
every character box. Exit 0 means measurement completed, not visual acceptance.
PDF font bounds remain informational: ascenders are not actual visible overflow.
Spatial flags use safely attributed ink. Overlapping or otherwise uncertain
attribution is reported as unknown and needs visual review, not automatic repair.
Flags are candidates for visual inspection. This
avoids writing a separate measurement or crop program for each page.

`--objects ID ...` focuses detailed inspection and records uncovered objects.
`--previous prior/inspection/report.json` distinguishes persisting/new/no-longer-
detected findings for the objects actually rechecked. It cannot certify closure
by deleting an object or excluding it from scope. For independently identified
artwork ownership or an explicit user policy, pass `--regions rules.json`:

```json
{"schema_version":1,"pages":[{"id":"overview","basis":"card borders inspected in blank artwork",
  "regions":{"card":[100,220,500,300]},"objects":{"node-left":{"region":"card"}}}]}
```

Regions use canvas pixel `[x,y,width,height]`. Optional `forbidden` maps artwork
IDs to rectangles, object `max_lines` checks a justified line limit, and page
`policy.min_size_pt` checks an explicit minimum. No such policy is imposed by
default. State its source in the shared contract, and follow the
[shared reconstruction method](page-method.md) to resolve actual conflicts.

Patch independent defects together by object ID, then build a new candidate:

```json
[{"page":"overview", "id":"node-left", "set":{"box":[140,260,420,80]}}]
```

```text
python <skill>/scripts/native_deck.py patch --spec deck.json --edits changes.json --out revised.json
```

Each `set` replaces the specified field (including an entire `format` object).
All edits validate before the new spec is written; invalid starting geometry can
be repaired. External page-file references remain separate: only edited pages get
new files, preserving independent worker ownership. Correct canonical-copy errors
in page data with source evidence; a layout patch cannot silently delete a copy
occurrence or change a number. Inspect the changed regions and their neighbors
together. Avoid optimizing against exact font-pixel agreement with generated text.

## Optional mechanical OCR draft

`draft-ocr --plan PLAN --out DRAFT` accepts the same schema/canvas/styles, an
`ocr_style` name, and page entries with `id`, `background`, `design`, and `ocr`.
The OCR file must have `image.sha256` and pixel-coordinate `detections` containing
`text,x,y,w,h`; a completed service receipt with that result is accepted. The
image hash must match the design. All detections are retained as candidates,
including uncertain text; repeated labels are not deduplicated. Font hierarchy,
OCR artifacts, copy authority, placement and foreground/background contrast all
need agent reconciliation. This saves mechanical transcription, not judgment.

## Independent page handoff and assembly

Use the existing `prepare_page_jobs.py` packet/ownership protocol. Give each worker
its page inputs and a one-page document referencing the frozen styles. Workers
write revised data and cycle outputs under their exclusive output directory.

After actually inspecting source, editable render and blank render, the worker
writes a review JSON with `inspection` booleans for those three views,
`open_findings`, and concrete observations/evidence paths. No unresolved defect
may be erased to obtain completion. Publish the handoff mechanically:

```text
python <skill>/scripts/native_cycle.py handoff --job jobs/overview/job.json --cycle jobs/overview/out/candidate-a/cycle.json --review jobs/overview/out/review.json
python <skill>/scripts/native_deck.py assemble --jobs jobs/prepared-jobs.json --out assembly/final-a
```

Handoff binds the agent's declarations to exact sources/artifacts; it does not
verify their truth. Assembly requires the full validated page collection, rebuilds
each accepted source and compares its native proof. Render and inspect the final
assembled deck using the existing final-review workflow. These helpers never
convert worker status, hashes or an absence of machine findings into visual approval.

## Read the cycle's evidence summary

`native_cycle` reports `workflow_evidence`: whether each page has a labeled design,
actual background/design dimensions, layout coordinates and whether object
measurements were produced. Treat a missing reference as a gap when the chosen
workflow requires it; native-only tasks may legitimately have none. Available
files and produced measurements are not verdicts about manuscript completeness,
art preservation or readability. Keep those observations in the existing review
record. The summary supplies evidence and does not change the selected workflow.
