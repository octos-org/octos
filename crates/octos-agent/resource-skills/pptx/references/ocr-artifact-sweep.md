# Find text invented from icons and connectors

Use this focused full-deck pass when an image-first editable conversion contains unexplained letters, numbers or symbols over artwork. Continue from the latest candidate and any completed, evidence-backed repairs. This pass complements text-layout inspection; it does not redesign the deck.

## Evidence and candidate selection

Freeze the current PPTX and identify its matching render, authoritative manuscript, labeled design images, underlying art and native text objects. Use the existing layer-inspection tools and producer manifests. Keep each candidate's page, shape/label ID, actual text, rectangle and semantic container so a finding survives a rebuild.

Inventory native text on **every page**, then prioritize isolated characters, short digit/letter clusters, circled symbols, OCR fragments, duplicate badge letters, and runs sitting on icons, connectors, arrowheads or legend strokes. Include suspicious longer strings and punctuation; a single-character filter alone misses errors. Inspect the page pixels too, including text remaining in raster art. A regex or short-string inventory proposes candidates; it cannot decide which text to delete.

Compare three independent kinds of evidence:

1. **Content authority:** does the manuscript, a source-grounded diagram label or an intentional numbering/axis convention justify this text and its association?
2. **Source pixels:** at that location, does the labeled design actually contain readable text, or only a graphic such as a shard, thumb, clipboard, chip, robot eye or arrow?
3. **Delivered layers:** is a native run duplicating or misreading that graphic, is there genuine text in the underlying raster, or are both present?

An entry in the **design's OCR output is not independent source evidence**. The same recognizer may invent the same characters from both source and output. “It appears in design OCR” cannot establish that an icon is a label. The generated design can also contain unintended glyphs; consistency with that image alone does not override the manuscript and the object's intended meaning. Conversely, absence from a prose outline alone does not justify deleting legitimate axis values, badge numbers, units, operators or explanatory diagram labels.

## Adjudicate finite objects

Use the verified vision service with paired, sufficiently detailed crops of the same source/render region, including surrounding labels. Verify crop registration and page identity. Ask neutrally what strokes/text are visible in each image and whether the added native text has source support. Do not supply a PASS verdict, exempt icon overlaps, or ask the model to approve a guessed deletion. Read its answer; uncertain or truncated output requires a contextual crop or another supported inspection, not a pass.

Match crops by semantic object, not by blindly reusing pixel coordinates. Source and rendered images can differ in dimensions, font metrics or line wrapping. Locate the complete target independently in each image; for a title or short caption, prefer its whole text band and nearby artwork. Verify that the target is actually included before submitting. A blank crop or a cut-off prefix cannot prove absence or duplication. If the answer describes a different fragment, correct the region once from measured bounds rather than repeatedly guessing tiny crops.

Classify each inspected candidate as legitimate text, confirmed graphic-to-text error, duplicated text, source-image defect, or unresolved. Record the evidence and intended action in the existing finding ledger or a linked manifest. Preserve already validated decisions when the relevant pixels are unchanged. If a detector repeatedly fails, use bounded per-object decisions; do not build a perfect general classifier before fixing confirmed cases.

For example, four outlined shard squares can yield `20`/`00`; a thumbs-down can yield a circled numeral; a clipboard/checkmark can yield Latin letters. These are examples of a failure mode, **not a global deletion list**. The same characters can be valid elsewhere.

Inspect mixed runs too: an icon misread can be merged into an otherwise legitimate caption, such as invented digits prefixed to `(shared use)`. Adjudicate the suspicious substring, preserve the supported caption and reassess its placement from the real text's source region. Deleting the entire run loses content; dropping only the characters while retaining an icon-inclusive OCR rectangle can leave the caption misplaced. Do not restrict the sweep to wholly nonsensical or one-character runs.

For a legend or repeated icon-label group, compare peer text origins, font sizes and icon-to-label gaps. OCR can merge an arrow shaft into the first caption as a dash or `一`, moving its box left and distorting its inferred size while the remaining captions are correct. Verify the actual source text column and the whole delivered group; removing the extra character alone is not enough. Preserve intentional graphic variations and legitimate repeated labels on distinct objects.

Check numbering by semantic group, not by character value or page-wide counts. A section badge and a workflow step can legitimately share the same number. Before deleting a suspected duplicate, identify both containers and compare the complete source and delivered sequences. Empty numbered circles, a lone surviving step number, or missing members of an otherwise supported sequence are content-loss findings. After cleanup, verify both that invented marks disappeared and that every supported label remains at its own object; restoring section badges alone does not verify workflow-step labels.

## Detect valid text duplicated across layers

A correct short label can still be wrong twice: `SSD`, `GPU` or a unit may remain rasterized inside an icon while a native textbox repeats it. For the same semantic region compare the labeled source, the actual delivered background with native text hidden, and the final composite. Check repeated equipment icons and dense micro-labels at readable scale, including valid-looking words; a nonsense-string sweep and full-slide OCR can both miss this. Distinguish legitimate repetition on separate objects from two copies on one object.

Extract the raster from the final PPTX and identify its source by hash. A newer clean background elsewhere in the workspace does not prove the build used it. Check the generator's selected asset and any rollback/alias; bind background-only evidence to those exact embedded bytes. OCR is a locator, not a substitute for viewing the region.

For editable labels, remove raster text through the authorized artwork-editing workflow while retaining one native label per intended object. Do not solve a doubled label by dropping editability, hiding it with a cover patch, or swapping to an unverified alternative background. Verify the underlying region is text-free, each native label occurs once at its intended node, and the corrected artwork preserves topology/borders/icons. Update full and blank derivatives together; invalidate prior visual evidence for any changed pixels. Retaining unavoidable raster text is a disclosed editability exception, not a clean text-free-artwork pass.

## Repair and close

Repair confirmed errors in the producer's own mapping/generator. Remove spurious native runs while retaining the intended icon and nearby labels. If the defect is actually rasterized, record that separately and use the established authorized artwork workflow. Do not hide the symptom with a cover patch or erase legitimate content with a blanket character filter.

Render a distinctly versioned candidate and inspect each changed region with its neighbors. Check that the icon is intact, the unwanted glyph is gone, required labels remain and no duplicate replacement was added. Reinspect every page affected by shared logic. Run the existing copy, layer, render-binding and compatibility checks within their documented scopes.

Provide one coverage row for **every requested page**, bound to the final candidate/page pixels: objects/regions examined, retained and removed candidates with reasons, evidence references, and unresolved items. Link valid prior evidence explicitly for unchanged regions. Deliver the new PPTX and matching PDF/previews with the actual repair list. An empty candidate list, an OCR receipt, a successful render or an earlier broad PASS is not proof that this focused inspection occurred. Continue independently through repairs and verification; a supervisor score is not required.
