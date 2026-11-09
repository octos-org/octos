# Detect text errors against the intended artwork objects

Use when labels are misplaced or broad visual review missed overlap. Start with [PPTX-first audit](pptx-first-audit.md). The Rust `inspect` operation reports actual native text, layout and isolated ink measurements. It is evidence for the model, not a visual judge.

## Establish an independent reference

Read the authoritative copy, labeled design and actual blank artwork **before** deriving constraints from the candidate. Identify the owning card/cell/node for each meaningful occurrence. A paragraph and a node caption containing the same words are different objects. Record legitimate repeated occurrences separately. Mark intended one-line captions; a multiline body paragraph has a different line policy.

Use normalized artwork coordinates `[x0,y0,x1,y1]`, all between 0 and 1, for the safe text area within each owner. Read those boundaries from the artwork, including sensible insets. Do not copy the candidate's text-frame bounds, expand the region around misplaced glyphs, or use the whole slide as an owner. Inspect the full parent and neighboring connectors when identifying the region. If blank-art generation moved the owner, use the blank's geometry and separately review its fidelity to the labeled design. An uncertain owner stays unresolved.

Card containment and clearance are separate checks. An icon or connector inside the parent still occupies space: define the available caption area and record interfering artwork as forbidden regions. A text-only render cannot demonstrate clearance from artwork that it omits.

OCR can include an adjacent line in the text box even when its transcription is
correct. Strip neither the dash nor the geometry blindly: locate actual glyphs
and the line endpoint in context. Use [label comparison](label-comparison.md) for
native contextual panels and measured placement/clearance findings.

Treat `local_label_ownership_unverified` as pending visual work. Use the grouped
`visual_review_queue` in `inspection.json`; its `viewport` is normalized
`[left,top,right,bottom]` including container and nearby text. For image-first
reconstruction, compare the same crop from labeled design, current blank render
and current editable render. Background-subtracted `compare` highlights displaced
signals; it does not establish correctness. Record an observation for the actual
label/container relationship before closing this uncertainty. A broad parent
region or a renamed region cannot certify local label placement.

Build this region inventory once per artwork revision. Reuse it for text-only fixes; moving a text box must not silently move its expected region. Content comes from the manuscript, while source imagery establishes object roles. Decorative OCR marks are not required labels. A blank table required by the manuscript is a source-design omission, not evidence that there was no text to reconstruct.

## Run the check

Declare each local owner in page `regions`, using canvas-pixel rectangles or
measured label quadrilaterals. Link each text occurrence with `region`; use
[local label geometry](label-geometry.md) for slanted text. Use the actual source
artwork, not the candidate frame, to determine those independent boundaries.
Run `cycle` or `inspect` on a bound cycle. Review the object inventory, missing
owners, baseline mismatch, overflow and isolated ink bounds. Unsupported grouped
or chart text remains a manual review requirement. A selected-object check cannot
certify the whole page. Use `crop` with the owner and neighbors, not just letters.
The exact native schema is in [native builder](native-builder.md).

## Resolve findings with pixels

The report identifies roundtrip copy mismatches, native layout overflow, frame/ink outside the owner and baseline mismatches. Extra/missing raster text, semantic ownership, occlusion and group/chart text still require actual pixel inspection. Font metrics are not visible ink; a near-border warning needs review.

For each flagged or uncertain object, submit the saved source and rendered crops through the verified **same-model** vision route. Include the intended text and owner identity, but omit earlier pass verdicts. Ask for observations before a verdict:

> Identify the parent card/node and transcribe its visible caption and final line. Compare the source region and actual render. For each occurrence, report: source owner; rendered owner; visible line count and last line; clearance from each border; any touched icon/arrow/other text; missing or extra occurrence. Distinguish unreadable evidence from absence. If a label is visible elsewhere, say where—it does not satisfy this owner. Return the specific observation and affected text; do not answer only NO DEFECT.

Use a crop containing the parent and its neighbors, not a tight crop of letters that hides the collision. If the whole-page response and the crop disagree, resolve the same-object evidence; do not choose whichever says pass. If the source's graph, ratio or resource constraint is wrong, classify it as an image-design defect and repair that layer. Reconstruction accuracy and detection of upstream image errors are separate results.

For raster cross-checks, do not restrict observed ink to the candidate text frame: that can discard the overflow being tested. Include the independently identified parent and surrounding space, verify the complete caption/final line is visible, and retain both artwork and text evidence before dismissing a warning.

Repair through your own generator, then rerun the same independent regions against the fresh hash-bound render. Close each finding with its corrected object/crop or specific false-positive evidence. Keep uncertain cases open. When scoped checks and affected-region inspection are satisfied, finish independently; do not wait for a supervisor verdict or repeatedly poll a clean page.
