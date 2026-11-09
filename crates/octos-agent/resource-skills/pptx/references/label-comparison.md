# Label evidence: placement, not just text recognition

Use when rebuilding editable text over artwork, especially near connectors,
rules, cards or repeated captions. OCR rectangles are search hints, not safe text
areas: OCR may include a connector as a dash, merge adjacent captions or omit
glyphs. Compare actual letters and surrounding artwork before adopting a box.

## Read evidence at the label scale

With the native Rust runtime, `cycle` and `compare_cycle` include
`label_evidence` in each reference-page comparison report. Each native text ID
gets a contextual panel ordered **design / actual render / difference**, its
source text, estimated ink extents and applicable findings. The crop includes
neighbors and artwork; it is not just a rectangle around the letters. For a
different selected runtime, obtain the equivalent matched contextual crops.

Look at the panels and measurements before reading a producer's pass explanation.
Interpret the available measurements:

- `ink_position_changed` / `ink_extent_changed`: estimated location, width or
  height changed. Separate intended copy/typeface changes from misplaced text.
- `artwork_line_clearance`: a thin horizontal/vertical artwork line approaches
  or enters the estimated ink envelope. Negative clearance means intersection;
  compare reference and candidate. A font change does not by itself excuse a
  label crossing a connector. An intentional underline may be appropriate.
- `peer_left_alignment_changed`: captions aligned in the reference no longer
  share that alignment. Inspect the listed peers, not only the first crop.
- `neighbor_label_clearance`: isolated native text ink envelopes approach or
  intersect. Check the paired context, wrapping and intended grouping; unlike
  overlapping text frames, visible ink proximity is relevant to readability.
- `reference_geometry_uncertain`: nearby labels, insufficient subtraction signal
  or rotation prevent a trustworthy estimate. Inspect the original, render and
  full context; do not convert unknown into “no defect”. Use measured local
  quadrilaterals for slanted labels.

Line measurements use full-page pixels, not positions in the three-panel image.
For `horizontal`, the endpoints are `(axis_start_px, cross_px)` and
`(axis_end_px, cross_px)`; for `vertical`, they are `(cross_px, axis_start_px)`
and `(cross_px, axis_end_px)`. Locate those endpoints in the actual render before
calling a finding a background highlight or another decorative feature. A line
running between letters can still split a caption or imply an unintended
underline; “no glyph stroke was covered” alone does not establish correct layout.

These are estimates and review candidates, not defect verdicts. Components are
not recognized words. Art drift can contaminate reference ink; lines can be
legitimate decorations; the detector does not cover every diagonal, curve or
perspective surface. A clean result does not establish semantic ownership or
correct wording. Read the actual page and unflagged labels too.

New cycles reuse their hash-bound native inspection for candidate glyph bounds,
avoiding the oversized text-frame/OCR-box problem. Reference lettering still
comes from pixels and can be ambiguous. Older cycles without this inspection
fall back to explicitly identified pixel estimates.

## Give an object-specific disposition

Keep the current page's existing `review.comparisons` entry. In addition to
whole-page `regions`, use `label_findings` with **one entry per finding**:

```json
{"id":"label-003-001",
 "evidence_sha256":"hash of this finding's current context panel",
 "classification":"defect",
 "note":"The caption starts before the connector ends; the reference has clear space."}
```

Allowed classifications: `defect`, `unresolved`, `acceptable_difference`.
An accepted difference needs a specific `reason_code` and a contextual observation:

- `intentional_change`: the brief permits this particular change; explain why
  the resulting relationship remains readable.
- `reference_relation`: the apparent collision is part of the intended reference
  design, such as an underline; confirm it in both views.
- `measurement_artifact`: identify the non-letter pixels, merged labels or other
  feature that makes this measurement misleading, using the local view.
- `font_metrics`: only for ink location/extent findings, after checking clearance
  and alignment. This cannot close artwork-line, peer-alignment or uncertain-
  reference findings.

For an artwork-line finding, `reference_relation` additionally requires reliable
reference geometry with the same low clearance. It is rejected when the reference
measurement is uncertain or shows clear space. A contaminated reference envelope
cannot prove “the original does this too”. If pixels reveal a measurement artifact
(including an intentional underline), use that reason and describe the actual
context; do not rely on an unreliable negative gap.

Do not copy a generic “font difference / a few pixels” explanation across a page.
Use the measured displacement, observed gap and affected object. If the estimate
is wrong, explain the pixel evidence rather than adjusting a tolerance or owner
box until the warning disappears. Fix through the page generator; rerun the same
label on the new actual render and inspect its neighbors.

`evidence_record` leaves omitted/unresolved findings unknown and rejects stale
panel hashes. It checks coverage and provenance, **not the truth of the visual
judgment**. A model can still misjudge a picture; independent peer review remains
necessary. Unchanged pages reuse evidence; a change to images or comparison label
geometry invalidates that page's old review. Do not impose extra revision rounds
after the concrete required findings have been resolved.
