# Rust PPTX runtime

The bundled `pptx_tool` is the native mechanics interface. In a terminal the same
request is JSON on stdin to `octos-pptx pptx_tool`; set `OCTOS_SESSION_WORKSPACE`
to the task directory. Paths stay inside that directory. Existing outputs are
never overwritten. No interpreter or office conversion process is launched.
`doctor` reports the binary and pinned engine revision; it is not a visual pass.

For current visual evidence, `review_pack` includes full-page context plus matched
native-pixel crops for every text object, even with no detector finding. Follow
[object visual review](object-visual-review.md); merge the generated template's
`object_reviews` into the cycle review. Use `review_submit` to bind authored observations to the reviewed pack without copying hashes; inspect and disposition `relation_signals` before submission. `evidence_record` reports missing/stale
object observations as `object_coverage` unknowns. Existing comparison/QA checks
remain required; the template is intentionally not an approval.

Comparison observations also use that receipt: fill `comparisons` in the same
template, identifying each page/region/label finding. Rust binds `comparison_key`
and `evidence_sha256` rather than asking the model to transcribe hidden digests.
Changed panels, stale receipts and unknown IDs are rejected. Missing observations
stay unknown; a valid receipt does not establish visual quality. For a separate
`compare_cycle` report, pass its manifest as `comparisons` to `review_pack` and
to the later `evidence_record`; preserve that exact report during review.

## Build and render

Use the page-data schema in [native builder](native-builder.md). Sources may
reference independent page files. Preserve supplied fonts and images. Assets
can have expected SHA-256; pixels and inches must have the same aspect ratio.

```json
{"action":"cycle","spec":"source/deck.json","output":"attempts/a","width":3840}
```

This builds `editable.pptx` and a derivative `blank.pptx` from the same objects,
saves and reopens them, renders both directly to PNG, and writes `cycle.json`
and `inspection.json`. Every page with a declared `design` also gets a whole-page
comparison in `comparisons/manifest.json`. Declare references whenever supplied;
do not omit them to skip QA. `width` means output width, not native artwork resolution.
Read the returned full-page PNGs with the host's image-capable `read_file`.
Tool-image history keeps a bounded recent working set (up to 8 images / 6 MiB raw),
with explicit omission notes when older images no longer fit. For comparison,
read the needed design, blank and render together; use smaller full-page previews
plus original-resolution contextual crops when necessary. A path or an expired
image note does not mean its pixels are still visible.
The tool response contains file paths and measurements, not a model visual opinion.

`build` uses the same data without rendering. Add `page` to build one page with
the same engine. For an existing deck use `render` with `input`, fresh `output`
directory and `width`. It reads the actual PPTX; do not substitute the design PNG.
`inspect` takes `cycle` (verifies its hashes, page count and order) or `input`
(an existing PPTX); optional `output` writes the full report.

Current automated measurements cover top-level native text, its layout overflow,
frame containment, isolated native glyph ink and local owner baseline. Missing local owner evidence remains
unknown. Grouped text, chart labels, raster text, semantic glyph ownership and
occlusion require the model's visual review. Font substitution is reported.
Never convert these limitations into a successful check.

## Batch corrections and native drawing

```json
{"action":"patch","spec":"source/deck.json","output":"source/revised.json","edits":[{"page":"overview","id":"caption","set":{"box":[120,240,420,80],"rotation":-12}}]}
```

`set` accepts box, rotation, style, format, region and text. A text change needs
an explicit matching `copy` on that edit after reconciling the authoritative
source; this prevents silently blessing OCR errors. Locked shared styles cannot
be locally overridden. Optional `expected_sha256` rejects a stale source.
Build another cycle in a new directory. Do not edit a receipt or old candidate.

`set.format` merges properties by default. Editing color preserves existing size
and alignment. `changes` returns effective formats before/after plus changed field
names. An edit may explicitly set `format_mode:"replace"`; then `format:{}` (or
null) clears overrides. Do not rely on omission to clear properties. A failed
edit validates the whole result before saving; it does not write a partial output.

To restructure a wide text object without rewriting canonical copy:

```json
{"action":"split_text","spec":"source/deck.json","output":"source/split.json","page":"overview","id":"caption","parts":[{"id":"first","text":"Observe","box":[120,240,160,60]},{"id":"operator","text":"→","box":[300,240,40,60]},{"id":"second","text":"Explain","box":[360,240,160,60]}]}
```

This example requires the original copy to be `Observe → Explain`. Each part
needs its own target box; optional style/rotation/region and merged format are
allowed. The action retains the canonical entry, computes validated `copy_range`
spans, and returns the mapping. Every symbol remains native text. Missing,
changed, reordered or repeated content is rejected. Existing group memberships
expand to the fragments; review whether the fragments still share that role
before freezing a new task's contract. Existing evidence is not rewritten, and
restructuring an already frozen layout may require reporting a contract mismatch.

`fit` accepts spec, optional `objects` (`page/object` IDs), optional output.
It uses DeckCraft's actual shaping/layout engine. It only proposes shrinking when
`min_size_pt` explicitly permits it; locked styles still apply. The source is
unchanged. Layout metrics are not visible ink bounds or ownership judgments.

For a declared same-size typography group, use
`group:{"page":"overview","id":"labels","size_pt":18}` with `reflow:true`,
`ink_align` and targets for **every** member. Do not combine this with min/max.
The group selects all members, fixes their size, and withholds all proposed edits
if one is infeasible. `group.blocking_objects` identifies the actual conflicts.
This does not infer semantic groups or authorize a different target area.
The source page declares `typography_groups:[{id,members,fields,basis}]`;
`members` and `fields` are arrays of strings, and `fields:["size_pt"]` contains
the property name, not its value. See the complete source/request example in
[fit and revise](fit-and-revise.md), also available from `help.fit` and fit errors.

For all-native shapes, tables and charts, use `commands` with an optional filter
to discover DeckCraft command IDs/parameters, then `batch` with `commands` rows
`{"command":"…","params":{…}}`. Supply `input` for an existing deck, or start
with `file.new`. Optional `output` saves a fresh PPTX. Commands run in order and
stop on error; batch cannot overwrite sources through file commands. Units here
are points. This route does not replace an image-first route chosen by the user.

## Visual aids

`image_info` returns actual file dimensions and a hash. It does not certify
native generation resolution or the absence of upscaling.

`compare` takes `before`, `after`, `output` (PNG), optional threshold 0–255.
For a labeled-design reconstruction, add `background` (the corresponding blank
render) to get a six-view comparison: design / blank / actual render above,
design-minus-blank / render-minus-blank / signal differences below. Separate PNGs
are returned for readable close-up review, along with a JSON report. `box` is an
optional `[x,y,width,height]` crop in **after-image pixels**; all views use that
same viewport. Matching aspect ratios are required; design/blank are resized to
the after-image canvas, with sizes recorded. No per-object alignment or warping.
This mode defaults to `threshold:24`, `tolerance:2` pixels (allowed 0–8). Blue
means a reference signal without nearby render signal, red the reverse, gray
nearby signals in both. Fonts, antialiasing, artwork drift and intended edits
can all cause color; low difference is not a pass and high difference is not
automatically an error. Read all three original views and the manuscript.

The separate views and analysis retain the actual render resolution (including
4K); only the six-view overview panel is reduced for readability and memory.
The report's `regions` group adjacent marked pixels into contextual viewports,
not recognized labels or automatically detected defects. Every marked pixel is
covered. Use additional crops where a region contains dense or tiny text.

## Whole-deck difference coverage

`cycle` generates the comparisons automatically; no extra calls per page are
needed. The manifest names each page, its input hashes, `comparison_key`, and
bound report. A failed comparison is recorded as unavailable and cannot count as
passed. The mandatory whole-page comparisons use fixed sensitivity defaults;
custom `compare` crops/settings are additional diagnostic views, not replacements.

For a subsequent cycle, optionally pass
`"reuse_comparisons":"attempts/a/comparisons/manifest.json"`. Only unchanged
design/blank/render bytes and label inputs reuse the old report. The key excludes paths and page
order; a renamed identical image does not require new visual work. Algorithm
changes invalidate the key too. Without this option reports are regenerated, but
unchanged review judgments still remain reusable through evidence history.

Older cycles can be checked without rebuilding or modifying them:

```json
{"action":"compare_cycle","cycle":"attempts/old/cycle.json","output":"checks/old-comparisons"}
```

Supply `"comparisons":"checks/old-comparisons/manifest.json"` on their
`evidence_record` call. Keep the same task and history.

For older cycles, first run `inspect` with that `cycle` and a fresh `output` if
isolated native glyph bounds are needed, then pass `"inspection":"checks/inspection.json"`
to `compare_cycle`. The tool verifies the report's PPTX and resolved-spec hashes
and canvas-pixel coordinates. New cycles already include that inspection and
reuse it; no second native measurement pass is needed.

In the existing review, add one entry per reference page:

```json
{
  "page":"overview",
  "comparison_key":"copy from the current manifest",
  "inspected":true,
  "note":"Observed the full actual render, original and blank; labels remain legible inside their owners.",
  "regions":[
    {"ids":["region-001"],"classification":"defect","note":"Caption crosses the right edge of its card."},
    {"ids":["region-002","region-003"],"classification":"acceptable_difference","note":"Approved typeface substitution changes glyph shapes; spacing remains clear."}
  ],
  "label_findings":[
    {"id":"label-003-001","evidence_sha256":"current finding's context panel hash",
     "classification":"defect","note":"Caption starts before its connector ends; the reference leaves space."}
  ]
}
```

Store these entries in `review.comparisons` alongside `inspected` and `findings`.
Valid classifications are `defect`, `acceptable_difference`, `unresolved`.
Nearby regions may share one reason if they were actually inspected. Missing
IDs stay unknown; unknown or duplicated IDs and stale keys are rejected. Use
`regions:[]` when the report has none, while still inspecting the actual page.
The harness validates coverage/provenance, not whether the model's judgment is
correct. Continue the separate text, artwork, readability and owner checks.

Reports also include `label_evidence`: one design/render/difference context panel
per text object, pixel-estimated glyph bounds, nearby axis-aligned line clearance,
reference-derived peer alignment and explicit measurement uncertainty. Read
[label comparison](label-comparison.md) for limitations and acceptance reasons.
Every finding needs its own `label_findings` disposition; whole-page `regions`
cannot dismiss it. An accepted finding requires `reason_code` (`intentional_change`,
`measurement_artifact`, `reference_relation`, or `font_metrics`). Font metrics may
only explain ink position/extent findings, not line clearance, peer alignment or
uncertain geometry. Missing findings stay unknown; wrong/stale context hashes fail.
These checks establish evidence coverage, not correctness of the model's judgment.

`evidence_record` automatically carries forward recorded judgments when all three
image hashes, label inputs and analysis versions are unchanged. Submit fresh entries only for changed pages or revised
judgments, plus the current `cycle_sha256` and ordinary review coverage. Omitted
unchanged defects stay active. Stale explicit entries fail; stale retained entries
become unreviewed. Do not discard references or reset the task to clear evidence.
The returned `comparison_coverage` identifies missing pages/regions and defects.

`inspect`/`cycle` also report `ownership_unverified_objects` and
`visual_review_groups`. The report's `visual_review_queue` groups flagged labels
by owner, with normalized `[left,top,right,bottom]` viewports. It is a pending
review list, not a defect verdict. Use these views plus an unflagged-page sweep
as described in [review evidence](review-evidence.md). Do not filter out unknown
ownership and then call the remaining empty list a visual pass.
Without `background`, `compare` requires equal dimensions and creates a before/after/difference panel.
Changed pixels and mean error measure change, not improvement.

`crop` takes input, output and pixel box `[x,y,w,h]`; it does not resample.
Use overlapping crops with surroundings for dense areas, including areas with
no automated flags. Inspect both the complete page and the crops.

`ocr` takes `input`, optional fresh JSON `output` and optional `languages`
(default `zh-Hans`, `en-US`). `doctor.ocr` reports host availability; the current
native backend is macOS Vision, with no Python or external executable. Other
hosts report unavailable, not an empty successful scan.

```json
{"action":"ocr","input":"assets/design.png","output":"checks/design-ocr.json","languages":["zh-Hans","en-US"]}
```

The response binds the input hash and actual dimensions to text, recognition
confidence, axis-aligned boxes and TL/TR/BR/BL quads in top-left pixel coordinates.
These are candidates, not an authoritative transcript. Match by text, occurrence
and surrounding meaning against canonical copy; detection order alone does not
identify labels. Inspect unmatched, merged, stylized and rotated text directly.
Use `crop` for a difficult region, then translate crop-local coordinates back by
its origin. Keep titles and small labels in the candidate set; size/color filters
can silently delete valid copy. Pixel projections or image differences do not
recognize text.

Read both design and blank images. If blank generation moved a surface, OCR on
the design is only a starting location; use the actual blank's owner surface and
baseline, then inspect the rendered native text. Do not blindly copy boxes or
shrink every mismatch to a micro style. The same method applies to any deck.

`geometry` takes `quad` in TL/TR/BR/BL order and optional padding. It proposes an
inscribed rotated frame and reports whether the plane is sheared/perspective.
Native rotation cannot reproduce arbitrary perspective text.

## Review and completion

Keep the peer workflow in [peer review and exit](peer-review-exit.md).
`evidence_init` takes task_id, explicit stagnant_attempts and
workspace-relative deliverable files (not directories). Under Octos, omit owner_agent: the host injects the execution identity (full session ID for peers; agent ID for native workers), including CLI helpers launched through its shell tools. A conflicting nickname is rejected. Prefer the registered `pptx_tool` for evidence operations; a helper launched by Octos is not a standalone CLI. Only an independently launched standalone CLI supplies the real owner explicitly. Never pass a nickname to make `owner_matches` appear true; require `runtime_consumed_current_evidence` before claiming that the outer loop consumed evidence. It preserves existing task policy/history.
It connects to Octos's existing continuation observer, not another scheduler.

Declare stable final delivery paths, not a temporary attempt's PNG name. Before
recording, copy the checked PPTX and preview bytes to those final paths. A fresh
`cycle` binds its source manifest and external page JSON as well as its outputs;
changed source files require a new cycle. Old cycles without source provenance
cannot certify an editable source JSON delivery.

If an existing policy names obsolete delivery paths, call `evidence_retarget`
with `deliverables` and a concrete `reason`. It changes only file locations and
publishes an audited revision; the task, owner, checks, thresholds, findings and
stagnation history remain unchanged. Never call `evidence_init` again, delete
state, or rename the task to repair a path mismatch. After retargeting, record the
current checked cycle and require `delivery_bindings_current: true` in status.
Retargeting does not approve a page or change a cover-only review into a full-deck
review. After all checks for the current batch pass, include
`extend_scope_reason` on `evidence_record` with a new cycle containing every old
page plus the next authorized pages. This archives the previous policy, snapshot
and findings, keeps attempt numbering and records the larger scope. It does not
approve the new pages: missing review coverage stays unknown. The observer uses
the larger batch as the new comparison baseline, retaining the old state in an
archive. Removing pages or expanding an unfinished batch is rejected. For an
unchanged scope, just record a fresh cycle; do not restore stale source bytes to
match a past snapshot. Do not end the task after each batch: finish review, then
continue the already authorized remaining pages.

Every `cycle` now includes `native_qa`. `evidence_record` independently reopens
and measures the actual PPTX, and combines the machine result with the review.
A model-authored `pass`, dismissal, altered inspection JSON, or old QA report
cannot clear machine findings. `qa_check` can also run separately; its standalone
CLI exits 2 for fail/unknown. Tool invocation success is not QA acceptance.
Use `qa_help` for the embedded schema, retained constraints and enforcement limits.
Repair the actual text against its independent artwork owner; do not move or
remove the acceptance target. Missing ownership/measurement stays unknown.
Native geometry passing does not approve aesthetics or replace image comparison.

After each checked cycle, write a model/human review. For reference pages also
include the `comparisons` entries described above; the minimal example below
only supplies the ordinary coverage/findings. Missing comparison entries reuse
unchanged recorded judgments, otherwise remain unknown.

```json
{
  "cycle_sha256":"hash of the current cycle.json",
  "inspected":{"copy":["overview"],"artwork":["overview"],"readability":["overview"]},
  "findings":[{"id":"caption-baseline","check":"readability","state":"open","note":"Caption baseline does not follow its local sign","evidence":"attempts/a/rendered/slide-001.png"}]
}
```

Call `evidence_record` with cycle and review paths. States: open, unknown,
resolved, dismissed. Each finding needs current rendered evidence and a reason.
Omitted unresolved findings remain unresolved. Missing review coverage is unknown.
Identical evidence is idempotent and does not count as another attempt.
`evidence_status` reports configured_owner, runtime_owner and owner_matches, then checks whether the kernel consumed
the exact policy/evidence hashes. It does not infer process liveness or approval.
It also returns `unbound_deliverables`; fix those before handing off. Recording
rejects unmatched delivery files immediately rather than deferring this error to
the end of the agent turn.

Rendering success never means the deck is done. Reconcile copy, artwork and actual
readability separately, retain unresolved defects, and follow the existing peer
exit rules. No automatic OCR/model reviewer is hidden in this binary: use the
host's configured OCR/image tools and real peer sessions when needed. Image
generation and text removal remain operations of the selected image service.

Read [review lifecycle](review-lifecycle.md) before a repair cycle: register defects before editing, freeze the verification route, and reuse only unchanged local evidence.
