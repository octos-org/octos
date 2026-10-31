# Detect text errors against the intended artwork objects

Use when editable conversion misplaces labels/paragraphs or a visual check reports no defect despite visible overlap. `scripts/audit_text_regions.py` is a read-only second check. It does not repair slides or decide artistic quality. First complete [PPTX-first audit](pptx-first-audit.md). This optional helper combines native PPTX identity/copy with supplementary glyph geometry from a hash-bound PDF and independently identified artwork regions. Its crops show the PDF exporter's output, not necessarily PowerPoint's display. No external reviewer is needed.

## Establish an independent reference

Read the authoritative copy, labeled design and actual blank artwork **before** deriving constraints from the candidate. Identify the owning card/cell/node for each meaningful occurrence. A paragraph and a node caption containing the same words are different objects. Record legitimate repeated occurrences separately. Mark intended one-line captions; a multiline body paragraph has a different line policy.

Use normalized artwork coordinates `[x0,y0,x1,y1]`, all between 0 and 1, for the safe text area within each owner. Read those boundaries from the artwork, including sensible insets. Do not copy the candidate's text-frame bounds, expand the region around misplaced glyphs, or use the whole slide as an owner. Inspect the full parent and neighboring connectors when identifying the region. If blank-art generation moved the owner, use the blank's geometry and separately review its fidelity to the labeled design. An uncertain owner stays unresolved.

Card containment and clearance are separate checks. An icon or connector inside the parent still occupies space: define the available caption area and record interfering artwork as forbidden regions. A text-only render cannot demonstrate clearance from artwork that it omits.

Build this region inventory once per artwork revision. Reuse it for text-only fixes; moving a text box must not silently move its expected region. Content comes from the manuscript, while source imagery establishes object roles. Decorative OCR marks are not required labels. A blank table required by the manuscript is a source-design omission, not evidence that there was no text to reconstruct.

## Run the check

Inventory all slides with `scripts/inspect_slide_layers.py` without `--page`; names identify each candidate occurrence, not its intended location. Derive regions and scope from that current inventory and source artwork; do not select a fixed list of previously defective pages. Author a JSON region file, for example:

```json
{
  "version": 1,
  "pages": [{
    "page": 1,
    "design_image": "../generated/design/diagram.png",
    "artwork_image": "../generated/blank/diagram.png",
    "region_basis": "Bounds read from blank artwork; roles cross-checked with labeled design and manuscript.",
    "objects": [{
      "id": "decision-node-caption",
      "native_name": "TEXT__decision-caption",
      "text": "Ready",
      "owner_id": "decision-node",
      "owner_rect": [0.40, 0.30, 0.55, 0.40],
      "source_evidence": "Short caption inside the decision node; not the explanation card below.",
      "max_lines": 1,
      "check_orphan": true
    }],
    "forbidden": [{"id": "outgoing-arrow", "rect": [0.56, 0.34, 0.62, 0.36]}],
    "counts": [{"id": "ready-caption-count", "text": "Ready", "expected": 1}]
  }]
}
```

The page index and coordinates above are schema examples, not detection rules or reusable layout data. Derive every inventory from the current manuscript and artwork; reordering slides must not change which objects qualify for checks. Paths resolve relative to the region file. `review_rect` may additionally include the full parent/neighbor region. `tolerance_pt` defaults to 1 point for approximate PDF font bounds, not a license for visible overflow. `counts` checks exact complete native text shapes, excluding substrings inside prose; use it for accidental extra badges/captions. It cannot find baked-in duplicates.

```sh
python skills/pptx/scripts/audit_text_regions.py exports/candidate.pptx verification/current/candidate.pdf \
  --regions verification/text-regions.json \
  --render-report verification/current/render-report.json \
  --out verification/text-region-check-01
```

Use the project's interpreter. The render report must bind `input_sha256` and `pdf_sha256` to the actual files; retain the original render-service receipt. Use a fresh output directory. Exit 2 means findings, exit 0 means no findings **within the declared scope**, and other errors mean the audit did not complete. `whole_deck_pass` is deliberately unset. Read uncovered object names and unsupported groups/tables; a selected-object check does not certify a full slide.

Report checked objects against the full native inventory, with unchecked objects explicit. If claiming full review, record how the remaining objects were reviewed; neither existence checks nor cleared warnings establish their layout correctness.

## Resolve findings with pixels

The report identifies copy mismatches, text outside its owner, text overlapping declared artwork regions, unexpected wrapping, short final lines, duplicate-count mismatches and unresolved PDF mappings. PDF text can reorder subscripts; do not delete valid formulas to satisfy extraction. Font bounding boxes also include more than actual ink, so a near-border warning needs pixel inspection.

For each flagged or uncertain object, submit the saved source and rendered crops through the verified **same-model** vision route. Include the intended text and owner identity, but omit earlier pass verdicts. Ask for observations before a verdict:

> Identify the parent card/node and transcribe its visible caption and final line. Compare the source region and actual render. For each occurrence, report: source owner; rendered owner; visible line count and last line; clearance from each border; any touched icon/arrow/other text; missing or extra occurrence. Distinguish unreadable evidence from absence. If a label is visible elsewhere, say where—it does not satisfy this owner. Return the specific observation and affected text; do not answer only NO DEFECT.

Use a crop containing the parent and its neighbors, not a tight crop of letters that hides the collision. If the whole-page response and the crop disagree, resolve the same-object evidence; do not choose whichever says pass. If the source's graph, ratio or resource constraint is wrong, classify it as an image-design defect and repair that layer. Reconstruction accuracy and detection of upstream image errors are separate results.

For raster cross-checks, do not restrict observed ink to the candidate text frame: that can discard the overflow being tested. Include the independently identified parent and surrounding space, verify the complete caption/final line is visible, and retain both artwork and text evidence before dismissing a warning.

Repair through your own generator, then rerun the same independent regions against the fresh hash-bound render. Close each finding with its corrected object/crop or specific false-positive evidence. Keep uncertain cases open. When scoped checks and affected-region inspection are satisfied, finish independently; do not wait for a supervisor verdict or repeatedly poll a clean page.
