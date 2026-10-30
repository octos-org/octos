# Inspect the PPTX; use rendered evidence to verify its appearance

The delivered PPTX is the inspection target. A generated design image, producer manifest, PDF or successful export cannot replace it. Check three distinct things and report their evidence separately.

## 1. Inventory the actual editable package

Start with the exact candidate file, without a PDF dependency:

```sh
python skills/pptx/scripts/inspect_slide_layers.py exports/candidate.pptx \
  --out verification/candidate-native-inventory.json
python skills/pptx/scripts/check_pptx_compat.py exports/candidate.pptx \
  --out verification/candidate-compatibility.json
```

Omitting `--page` inventories every slide in presentation order. Use the resulting native strings, stable slide/shape identities, raw paragraph/run formatting, body properties, transforms and embedded-image hashes. Reconcile every required occurrence against the authoritative manuscript and actual design; identify unintended extra text. Check the requested title style across all titles, including mixed runs, wrapping/autofit, insets and alignment. Raw formatting can inherit from layouts/masters/themes: an absent explicit property is not a missing font or a resolved value. Follow inheritance or use the actual application's rendering where needed.

The inventory is read-only evidence, not an automatic semantic verdict. Group coordinates are local; tables, charts, z-order, inherited formatting and visibility may need additional inspection. The compatibility preflight also has a limited scope; see [PowerPoint compatibility](powerpoint-compatibility.md).

## 2. Inspect the presentation as rendered by its target application

When Microsoft PowerPoint is available through the authorized tool route, open the exact candidate in PowerPoint and inspect slide images exported by PowerPoint or captured from it. Record the application/version, PPTX hash, slide count, repair prompt status and image provenance. Do not alter the production deck just to inspect it. Target-app opening and target-app visual review are separate results.

Inspect every slide for a new complete deck. After a repair, inspect the changed objects with their complete parent regions, neighboring objects and any pages affected by shared logic. Derive those pages from actual changes, not a permanently selected list. Check actual lines, glyphs, panel clearance, ownership, duplicates, missing content and diagram meaning. Compare with the source artwork and manuscript; a screenshot alone does not establish editability.

If the available render service uses LibreOffice, label the images **LibreOffice-rendered PPTX** and keep PowerPoint visual/compatibility status unverified until tested. Rendering differences can include font fallback, wrapping and clipping. Do not equate the two applications or change the producer's source to satisfy an extraction artifact.

## 3. Use PDF geometry only as supporting evidence

A PDF exported from this same PPTX can expose rendered glyph positions, fonts and line grouping, which raw text-frame coordinates do not provide. This is why [the text-region helper](text-region-audit.md) accepts both PPTX and PDF. It checks native copy/identity from the PPTX and supplementary glyph geometry from the PDF. It does not establish PowerPoint editability, opening, or native-app appearance. Its PDF crops are evidence of the named exporter only.

Keep the render receipt and hashes linking PPTX → exporter → PDF/images. A PDF is optional to the overall audit; use native-package checks and target-app slide images without it when that is the available path. If PDF extraction and target-app pixels disagree, resolve against the actual target-app display and content authority. Keep extraction failures explicitly unresolved rather than deleting or rewriting correct text to obtain a pass.

## General rules, artifact-specific evidence

- No production detector or skill rule may branch on a fixed page number, known label, project name, or memorized coordinate. No hardcoded deck length or whitelist of previously defective slides.
- Derive coverage from the current PPTX and content/design inventory. The same object rules apply when slides are reordered, inserted, translated, or resized.
- Coordinates, page references and expected strings are legitimate **per-artifact evidence data**, derived anew from that artifact's manuscript and artwork. Keep them outside reusable code/skills. They are not reusable layout prescriptions. Do not copy a known-answer region map from a previous project or a reviewer.
- Identify semantic ownership independently of the candidate's possibly wrong text box. Freeze artwork-derived regions for that artwork revision; do not enlarge them to make misplaced text pass. An uncertain owner remains unresolved.
- Use shared role/style constraints and actual font/renderer measurements. Do not choose a threshold solely because it catches one known bad page while sparing other known pages.
- Tests for known mistakes are regression tests. To claim generalization, use unseen layouts/copy with positive and negative cases, changed slide order/count, and no defect hints. Report misses, false alarms, repairs and regressions separately; a passing fixture is not proof that the producing model can infer ownership independently.
