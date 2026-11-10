# Bundled native-text builder

Use the bundled Rust `pptx_tool` for the normal blank-artwork + editable
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
    "typography_groups": [{"id":"labels", "members":["node-left","node-right"], "fields":["font","size_pt"], "basis":"Two same-level card labels; heading excluded"}],
    "texts": [
      {"id":"heading", "text":"Example workflow", "style":"heading", "box":[100,60,1400,100]},
      {"id":"node-left", "text":"Input", "style":"label", "box":[140,260,400,70], "region":"left-card"},
      {"id":"node-right", "text":"Output", "style":"label", "box":[1040,260,400,70], "region":"right-card"}
    ]
  }]
}
```

`copy` records every intended text occurrence separately, even when strings repeat.
Each textbox has an `id` and matches `copy[id]`, or an explicit `copy_id`. Whole
occurrences must appear once. A split occurrence uses multiple textboxes sharing
`copy_id`, each with a `copy_range:[start,end]` in Unicode scalar offsets of
normalized authoritative copy (end exclusive). Ranges must cover the full copy
exactly once, without gaps or overlaps; whole and partial occurrences cannot be
mixed. Prefer `split_text` to compute the mapping and retain the original copy
entry. Only whitespace differences are normalized; numbers,
case, punctuation and operators remain significant. Independently reconcile this
inventory with the manuscript and design: an inventory copied from bad OCR cannot
prove semantic completeness. A raster arrow replacing a written operator is a
separate content/design decision requiring evidence; `split_text` does not silently
delete the operator or certify this conversion. Establish that representation
before freezing the content contract, or report the unsupported migration.

`box` and named owner `regions` are `[x,y,width,height]`. Determine regions from
artwork, not from the textboxes being checked. Native frame containment is a useful
check but cannot establish rendered glyph containment or correct ownership.

Optional page-level `typography_groups` is an array. Each group needs `id`, at
least two local text IDs in `members`, property names in the `fields` array,
and a nonempty `basis`. Supported fields are `font`, `size_pt`, `bold`, `italic`.
Do not put numeric values in this declaration. A chosen exact size goes in the
`fit` request's `group.size_pt`; see [fit and revise](fit-and-revise.md) for the
complete declaration/request pair.

Styles require `font` and `size_pt`; optional fields are `color` (six-digit RGB),
`bold`, `italic`, `align` (left/center/right), `anchor` (top/middle/bottom), `wrap`,
`margins_px` ([left,top,right,bottom]), `line_spacing` (multiplier),
`space_before_pt`, and `space_after_pt`. Defaults are black, left/top, wrapping,
zero margins/paragraph spacing and single line spacing. A text's `format` object
overrides an unlocked style. `locked: true` prevents local overrides of shared
typography. Use `\n` for explicit line breaks. The engine never auto-shrinks text.
Assets may be `{ "path": "...", "sha256": "..." }` to require exact bytes.
Optional `artwork_layers` on a page restores local raster artwork without editing
the exported PPTX. Each entry is `{ "id":"accent", "image":"art/accent.png",
"box":[120,200,24,80], "rotation":0, "basis":"Original accent lost in the blank" }`.
These synthetic coordinates are schema examples. Use actual image evidence.
Array order is paint order: background, local layers, then all editable text.
The same layers remain in the derived blank; `cycle` checks the actual final stack.
Paths resolve like other page assets, hashes bind the layer bytes, and rotated
layers must stay on the canvas. They are independently movable pictures, not
editable text or vector artwork. Preserve the reference texture and inspect seams
and neighbors; a successful insertion does not approve a repair.
The canvas is a coordinate system, not an instruction to downsample artwork.
Keep original-resolution backgrounds embedded; create separate small previews if
needed. For example, 1280×720 layout coordinates can use a 3840×2160 background.
Do not resize the source to the canvas dimensions just to simplify placement.

## Build, review, patch

Use the [Rust runtime](rust-runtime.md) actions `cycle`, `fit`, `patch` and `inspect`.
One-page work uses the same schema and `page` selector; assemble by referencing
accepted page data in the final spec. No worker code or adapters are required.
