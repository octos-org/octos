# Text reflow and obstacle-aware placement

For image reconstruction, keep the original Paddle OCR line IDs, transcription, canvas
pixel boxes and TL/TR/BR/BL polygons together throughout a repair. Match the reference
word to the measured ink of the editable text. A chip or card can contain several labels
with different centers.

`build` and `cycle` pick up an `ocr.json` beside the page spec when its
`source_sha256` matches the page's bound `design`. Use the page's `source_ocr`
asset field to specify another registered file. The observation supplies `canvas`
as a width/height pair and `lines` with `id`, `text`, `box` and an optional
`polygon`. The runtime measures native glyphs, uses the polygon baseline when available,
and translates misplaced single-line text to its source anchor. It keeps the authored
wording, font size and frame dimensions. The build receipt records each correction in
`ocr_placement`. Check the rendered result for weight, fitting and perspective.

When an OCR reading is wrong, retain the verified wording. For a geometrically bad
detection, mark that line `geometry_status: "rejected"` in a source-reviewed
observation or provide its verified polygon and envelope. Ambiguous repeated words
need a matching occurrence ID. Unmatched text stays with the maker.

These mechanics support the existing production peer and reviewer. They do not
generate a new design, alter copy, shrink type, or replace visual acceptance.

`fit` retains translation-only semantics by default. To actually recompute
wrapping at a target rectangle, pass `reflow:true` alongside `ink_align` and
`targets`. The result includes `layout_box`, line count, required height and
measured visible glyphs. Use the current font/style and apply only feasible `set`.

For repeated collisions, search the usable local area once:

```json
{"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
 "area":[30,40,500,260],"forbidden":[[30,40,160,210]],
 "register":true,"output":"review/placement-a"}
```

The numbers above are synthetic syntax examples. Determine the real allowed
area from the intended card/label and actual artwork, independently of the
current misplaced text. `forbidden` accepts rectangles or convex TL/TR/BR/BL
quads. Include solid illustration interiors: edge detection alone cannot tell
a flat-colored part of an illustration from blank space. Existing owner
exclusions are preserved. All geometry uses the spec's canvas pixels, even if
the background bitmap has a different resolution.

The tool reserves neighboring native glyph envelopes, finds contrast edges in
the bound background, adds the requested clearance (default 3 px), then lays
out the unchanged text/font in empty rectangles with DeckCraft. It measures
real glyph envelopes and returns up to eight proposals and previews. Inspect
`obstacles.png` before accepting one: textured backgrounds may reject useful
space; faint strokes may be missed. Previews contain the proposed text on the
artwork, not all other native text; the full rebuilt PPTX remains authoritative.
`artwork_components.regions` also reports measured contrast-component boxes in
canvas pixels, independently of your exclusions. Use these and the actual
image when correcting an oversized forbidden region; do not invent dimensions
from memory. Components are pixel groups, not recognized illustrations or labels.
The search uses a conservative 3 px grid and evaluates up to 512 rectangles.
`no_candidate_found` is not an exhaustive impossibility proof. For rotated
text use `geometry` and the existing fit workflow; this search does not guess
perspective transforms.

With `register:true`, the tool records the object's allowed area, exclusions,
reference image, canvas, copy and typography under `.octos/pptx-placement-contracts/`.
It does not edit page source or the deck's existing continuation policy/history.
Subsequent searches can omit `area` and reuse the same contract with a fresh
output directory. Moving or widening that area, removing exclusions, disabling
artwork checks, deleting copy or silently changing typography cannot clear it.
Every subsequent native QA and evidence record independently checks the actual
PPTX's glyph envelopes against that contract and current neighboring text.
`PLACEMENT_CONSTRAINT_FAILED` cannot be dismissed by model-written review.
Grid edges allow only 0.001 canvas pixels of numerical snapping for PPTX's
integer-EMU coordinate round trip. This does not change artwork clearance;
measurable subpixel overlaps remain failures. Collision feedback includes the
actual glyph envelope, grid cell size and this numerical tolerance.

Registration is an application-level gate, not OS isolation from arbitrary shell
writes. Protect verifier/policy files at the host when adversarial isolation is
needed. Geometry and edge clearance still do not establish semantic ownership
or a whole-deck visual pass. Keep unresolved reference/boundary evidence unknown.
