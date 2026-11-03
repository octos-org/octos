# Reinspect text layout in an existing editable deck

Start with [PPTX-first audit](pptx-first-audit.md): the candidate package and its target-app appearance are primary. Derive full coverage from the current deck; no fixed page list or page-specific detection logic. PDF geometry is supplementary.

Use this pass when the user asks for a typography/layout recheck or an otherwise usable deck still has small text defects. Continue from the current candidate and existing findings; preserve its content, illustrations, topology and visual style. Each producer inspects and repairs its own deck. Another agent's score, deck or a supervisor's approval is not an input to this procedure.

Use [review evidence](review-evidence.md) to keep manuscript meaning, source/blank
changes and final appearance separate. A whole-page review includes unflagged
regions at readable scale; detected-object crops alone do not cover the page.

When unexplained letters, digits or symbols appear over artwork, also follow [ocr-artifact-sweep.md](ocr-artifact-sweep.md) for a full-deck source-pixel and native-layer comparison. The design's OCR output cannot validate its own graphic-to-text errors.

## Inspect the actual candidate

Freeze a distinctly named candidate and render it through the supplied service. Bind the PPTX, PDF and page images by hashes. Inspect every page requested, not only previously reported trouble spots. A contact sheet checks consistency; it cannot reveal small glyph collisions. At presentation scale check hierarchy/readability; for dense diagrams, inspect surrounding-context crops near native resolution. Use the verified same-model vision route and read its answer; tool success and receipt existence alone are not inspection.

First ask about visible problems without disclosing an earlier pass verdict. Then compare the current render with the accepted labeled design and authoritative text to locate omissions and wrong associations. A useful neutral inspection request is:

> Inspect this rendered slide for text-layout defects. Identify each affected text string and region, and the specific collision, clipping, wrapping, alignment or association error visible in the pixels. Check all cards, nodes, headers and footnotes. Distinguish observed defects from unreadable or uncertain regions; do not infer a pass from text presence. Small or uncertain regions need a contextual crop. In a separate comparison, identify meaningful labels present in the labeled source but absent or misplaced in the editable render.

If a whole-page review misses known short-label or association defects, switch to [text-region-audit.md](text-region-audit.md): derive owner regions independently from artwork, run the read-only detector, and inspect its contextual crops with observation-first questions. Do not keep asking the same full-page yes/no question or equate text-frame containment with artwork containment.

## Check these independently on each page

- **Containment and clearance:** actual glyph ink must fit its intended card/cell/node, including left, right, top and bottom. Check against borders, icons, arrowheads, connector lines, neighboring labels and the slide edge. Clearing one side must not create overflow on another. Text-to-text collision tests miss most artwork collisions.
- **Wrapping and line height:** remove accidental orphan characters, clipped last lines, compressed/overlapping baselines and broken short labels. Keep meaningful Latin names, numbers with units, operators and version labels legible. Do not turn a whole workflow sentence into one node caption. Match the source's semantic grouping and intentional line structure.
- **Alignment and spacing:** inspect header/body separation, consistent insets among peer cards, baseline alignment in rows, centered badge numbers and label-to-icon association. Check subtitles against separator rules and footnotes against their panels. Optical glyph alignment matters, not just textbox coordinates.
- **Typography:** retain the requested uniform black Heiti title size and role hierarchy. Check actual rendered font/fallback, weight, color contrast and body readability. A font-name property alone does not prove the rendered face. Do not globally shrink text or replace a title style to make a local box fit.
- **Completeness and placement:** account for meaning-bearing tier names, node/stage labels, table headers/fields, legend entries, numbers, units and qualifiers. Keep repeated occurrences attached to their own objects. Outline prose elsewhere does not replace a diagram label; an empty colored band requires source comparison. Remove OCR glyphs only after distinguishing them from intentional marks.
- **Meaning:** verify symbols such as ≠, ≤, %, primes and subscripts, and keep qualifiers with their claims. A rearrangement that preserves all words but changes their node/row association is still a defect.

For a partially populated diagram, reconcile the labeled source with the delivered objects, not only the manuscript's paragraphs. Account for each meaningful header, branch identity, step caption and metric name as present at its intended object, deliberately changed under the content authority, or still missing. A source label's presence elsewhere on the page does not fill an empty header or node. Preserve legitimate repeated labels and do not fill missing content with guessed wording.

## Enforce an explicitly requested common title style

When the user asks for all titles to be left aligned and the same size, define one shared title style in the producer's source: font/fallback, point size, weight, color, text origin, top position/baseline, paragraph alignment and insets. Paragraph `left` inside differently positioned boxes does not align titles to the page. Do not inherit a centered title position or per-page size from image/OCR geometry after the user requests uniformity. Preserve current wording and artwork; keep subtitles and other text roles distinct, with consistent styles among peers.

Inventory every delivered title, including all runs, inherited styles, text-frame geometry and autofit settings. Verify the rendered PDF's font and size as well as the native settings; report accurately when the numeric size already matches but position, font fallback or weight differs. Use the available header width or the established wrapping policy for longer titles instead of silently shrinking just that page.

Render all changed pages and create a title-band comparison at a common slide scale without individually resizing or centering each crop. Check the shared left edge/top position, subtitle gap, long-title fit, actual typeface and absence of raster/native duplicates. A common subtitle baseline must clear the highest separator or content boundary across the deck: uniform coordinates are not sufficient if a raster rule crosses the actual glyphs. Preserve the chosen title style while resolving that clearance. Bind the style audit and visual evidence to the final PPTX/PDF hashes; do not infer cross-page consistency from a single title or XML alignment alone.

## Repair, then verify the whole affected region

Record each observed issue under a stable page/object identity in the existing ledger before switching work. Repair through your own layout manifest/generator, using the source geometry and the actual font. Prefer bounded changes to box geometry, intended line breaks, paragraph spacing, alignment or mapping. Do not hide text under patches, silently delete required copy, create arbitrary footer dumps, or use tiny fonts as the default escape. Regenerate artwork only if a demonstrated defect is in that artwork and cannot be addressed through the authorized editable layer.

Before declaring a label unrepairable without regenerating art, inspect its actual layer with `scripts/inspect_slide_layers.py` and the slide XML. A garbled string in an `a:t` run is native editable text even when it originated from OCR of a generated image. Recover its intended wording and object association from the authoritative manuscript and labeled source, then repair that mapping in your generator. Inspect the underlying raster separately: native and raster defects can coexist. An OCR detector's failure does not make individual known strings uneditable, and a receipt or automated collision count cannot overrule visible clipping.

Several unsuccessful placements do not by themselves create a need for external design approval. Preserve the known-working candidate, inspect the available region and neighboring artwork, and resolve the finite native-layout defect within the existing design constraints. If you revert an unsuccessful change, its finding remains open. Do not call a partial delivery complete while claiming target-app opening is the only remaining check: list outstanding layout, copy, inspection-route and compatibility items separately.

After each coherent repair batch, render a new candidate and inspect the whole changed region plus neighboring objects. A rightward shift also needs a right-edge check; a height change also needs a lower-card check. Shared layout changes require reinspection of every changed page, identified by rendered-page differences. Do not build a general detector indefinitely while a specific observed defect can be repaired.

For an identifier or short-label repair, inventory all occurrences in the page's prose, diagrams, legends and repeated badges. Correcting `R0` in a quadrant does not correct `RO` in its definition list. Before adding a missing letter, inspect all native runs and raster glyphs at that node to avoid a duplicate letter on another line. After moving a qualifier, verify its destination panel and its neighbors in the actual render; clearing the old overlap while landing in a legend is still a defect. Preserve these object-level checks in the finding's closure evidence.

Keep page-level inspection scope in the existing coverage record: candidate/page hashes, regions actually checked, receipt paths, findings and disposition. A complete-page input hash does not enlarge a question about one label into full-page coverage. A crop from a different slide or object cannot dismiss the current finding. Resolve an issue only with evidence about that same object in the current candidate; unexplained low confidence is not a pass. Reuse earlier evidence only for genuinely unchanged pixels and the same inspection scope.

## Finish without an external design approval gate

Run the existing copy, paired-artwork, font, render-binding and scoped PowerPoint compatibility checks after the final build. Previously successful PowerPoint opening applies to that exact hash; do not claim a new target-app test when none was performed. Keep this distinction separate from visual inspection.

Deliver a distinctly versioned editable PPTX with matching PDF/previews and a concise list of repaired page/object issues, actual inspection coverage derived from the delivered slide count, and any remaining uncertainty. Keep the prior delivered candidate available. Completion means the requested inspection and evidence-based repairs are performed, not merely that instructions were read or a checklist file was created. Proceed independently; report a real tool blockage precisely instead of waiting for a quality verdict.

Write the final chat summary and completion report from the same current artifact manifest. Check that their filename, hash, render revision, target-app evidence and remaining findings agree. A current hash in one report does not correct an old hash in the chat summary; a successful compatibility check does not close visual findings or establish that overlaps are inherent to the chosen workflow.
