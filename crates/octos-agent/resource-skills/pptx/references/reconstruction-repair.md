# Repair image-to-editable reconstruction failures

Use this procedure when designed source images are readable but native conversion loses labels, moves copy into another object, or overflows retained artwork. Continue with the producer's own source and design; this reference supplies no page layout or supervisor approval gate.

## Reconcile occurrences, not just strings

Build a small per-page inventory joining **content authority → source object/occurrence → final native object**. Include prose blocks and diagram captions separately. Record the intended parent card/cell/node and a source-region reference. A matching word elsewhere on the slide does not satisfy this occurrence. Repeated node captions, legends, formula operands and row identifiers need separate identities.

A high OCR similarity score only proposes a match. Shared vocabulary can map a whole explanatory paragraph to a short diagram caption. Check role, spatial region, surrounding objects and occurrence count before using its geometry. If no credible match exists, retain an unresolved mapping instead of silently using nominal coordinates. Character-set overlap cannot establish sentence identity or location.

## Constrain layout to the owning object

The safe boundary is the intended panel/cell/node, not the slide. Page-edge clamping does not prevent a paragraph from covering a neighboring chart. Estimate font metrics to start, then inspect actual rendered glyphs inside that object's inset boundaries, including the last line. A multiline paragraph and a one-line node caption require different sizing. Check short labels for unnecessary wrapping as well as long paragraphs for overflow.

Repair through the producer's generator/manifest using actual available space and the established hierarchy. A PIL estimate is not a PowerPoint/LibreOffice rendering test. Do not repair a bad semantic match by shrinking its font or widening it across other objects. Preserve the requested shared title style.

When these checks have been missed or disputed, use [the independent text-region audit](text-region-audit.md). Start with the native-package and target-app checks in [PPTX-first audit](pptx-first-audit.md). The optional region helper combines native PPTX identity/copy with auxiliary PDF glyph positions, flags misplaced/wrapped/duplicate labels, and produces exporter-specific contextual crops. Regions must not be learned from the same candidate text boxes being checked. Keep uncovered objects and unresolved mappings visible.

## Distinguish source defects from conversion defects

Compare the generated labeled image, actual embedded blank art and actual editable render:

- Present and legible in source but absent/misplaced in render: repair the native mapping/layout; do not regenerate sound art.
- A required caption removed from the blank artwork belongs in the native text layer; its absence from a text-free image does not itself require regenerating the image. First reconcile its meaning with authoritative copy. Do not restore unexplained generated-image marks merely because they existed in the source.
- Missing from the generated source itself, despite required manuscript content/diagram intent: this is an image-design omission, not unreadable OCR. Use authoritative copy and known object roles to repair, or perform a bounded image edit if the intended structure needs correction. Do not invent claims or accept an empty comparison table as a completed diagram.
- Quantitative geometry conflicts with labels: verify arithmetic, proportions and shared-resource constraints against the manuscript. Faithful conversion can preserve a source error; repair that error explicitly rather than treating image fidelity as correctness.

Measure quantitative marks individually. For bars, identify each bar's actual endpoints on a contextual crop and exclude adjacent captions, brackets and unrelated diagrams from the measured span. For timelines, check intervals against the shared scale and declared resource capacity. A number from unsegmented ink bounds is not a validated measurement.

## Evidence and completion

Use the verified same-model pixel-inspection route specified in the deployment tools. A native viewer's metadata or `shown_to_model` flag alone is insufficient. If its actual image delivery is unaudited, use the already working vision service, not hundreds of repeated crops through the uncertain route. Retain the question and actual visual answer.

Keep three results separate: package/copy checks, per-object mapping coverage, and rendered containment/association review. A text-only render can test whether glyphs fit their own text frames; it cannot establish that those frames belong inside the correct artwork card or avoid arrows/icons. Do not report that result as artwork-panel containment. Reclassifying duplicate-text warnings as notes cannot close unrelated visible overlaps or omissions.

A full-page vision answer of `NO DEFECT` does not close a previously identified object defect. For disputed objects, inspect a crop containing the whole owning card/node plus its neighbors and compare it with the corresponding source region. Ask about that object's identity, association, wrapping and border/connector clearance, not only clipping. Retain the crop, question, actual answer and final render hash; reconcile contradictory visual and geometric evidence instead of taking the most favorable result. Quantitative diagrams also require the arithmetic/resource check above.

Before dismissing a missing native label as retained raster text, inspect the actual embedded background and final render at that occurrence. Source-image presence alone is insufficient because text removal may have erased it. Record a source omission or intentional removal separately; do not claim an absent mark remains visible. State unresolved findings even when automated checks report zero.

Retain generation/render receipts and old candidates. Every changed render uses a fresh job ID and output directory; do not delete old receipts or outputs to force an ID to rerun, including in recovery instructions. Bind the final artifact and review evidence to its hashes. Finish independently when the concrete repairs and full affected-page review are complete.
