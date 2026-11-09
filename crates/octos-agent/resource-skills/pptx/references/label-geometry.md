# Reconstruct text on tilted labels

Read this when the artwork puts text on a slanted sign, ribbon, card, or surface.
Measure the **local label**, not the entire panel/island containing it. Ordinary
page titles need not be given artificial label regions.

1. Inspect the labeled design and current blank artwork at readable scale. For
   each affected label, identify its four corners in the layout canvas's pixel
   coordinates: local top-left, top-right, bottom-right, bottom-left. OCR may
   supply a quadrilateral candidate; its text and the actual label boundary still
   need reconciliation. A text bounding box is not an independently measured
   owner. If the blank edit moved the sign, measure that sign again.
2. Calculate a rotation and an inscribed frame proposal:

   ```json
   {"action":"geometry","quad":[[210,180],[510,235],[495,317],[195,262]],"padding":6}
   ```

   These are unrelated syntax-example coordinates. Read the result; it never
   applies a patch or chooses font size. `box` is the unrotated PowerPoint frame;
   `rotation` is clockwise degrees **about its center**. An axis-aligned OCR box
   is not this local frame. Patch both fields together, preserving canonical copy.
   Fit the font in local dimensions; inspect actual glyph margins after rotation.
   `fit` measures local text dimensions using the rendering engine. Inspect rotated ink in the actual render.
3. Declare the independently measured owner in page `regions`, and its page's `regions` definitions, to check actual ink against slanted edges and baseline angles:

   ```json
   {
     "sign": {
       "quad": [[210,180],[510,235],[495,317],[195,262]],
       "scope": "label",
       "basis": "Four corners of the sign face measured on the current blank artwork",
       "angle_tolerance_deg": 2
     }
   }
   ```

   Optional `baseline_deg` specifies the intended text direction if different
   from the sign edges. Optional `source_quad` records its separate location in
   the labeled reference, in the same canvas coordinates. Quads must be convex
   and ordered clockwise in screen coordinates. Existing `[x,y,w,h]` regions
   still work but cannot certify local label ownership. `scope: "panel"` remains
   useful for broad placement; it is explicitly not a local-label check.
4. Run the normal `cycle`. It measures native frame containment, baseline angle
   and isolated text ink against the supplied region. Isolation cannot prove
   clearance from surrounding artwork or later objects. Inspect actual source,
   full and blank crops with neighbors. `local_label_ownership_unverified` means
   no independently measured local label was supplied; never relabel a large
   panel merely to suppress it. Measurement success is not semantic acceptance.

## Perspective is a separate capability

A rigid rotation preserves right angles and equal opposite edges. A trapezoid,
shear, or curved baseline cannot generally be reproduced by one rotated native
textbox. The helper reports rigid-fit residual and flags a relative error above
3% of the smaller fitted dimension as `perspective_or_shear`. This is a measurement
heuristic, not an aesthetic threshold or proof of correspondence. The proposed
inscribed rectangle is a readable approximation, not an exact perspective transform.

When exact perspective matters, use a separately tested editable transform or
redesign that particular label consistently with the approved art direction.
Do not silently flatten all typography, shrink until it fits, or rasterize required
editable text. Report unsupported projective reconstruction honestly. Remeasure
geometry when the artwork changes. No deck-specific pages, angles, text, or success
answers belong in the shared skill.
