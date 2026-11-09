# Font proposals and revision

Use `pptx_tool` actions `fit`, `patch`, `cycle`, `compare` and `crop` as described
in the [Rust runtime](rust-runtime.md). `fit` measures with the same font/layout
engine as rendering. It returns a proposal and never changes the source. Shrink
requires explicit `min_size_pt` and cannot override a locked shared style.

Do not use font shrinking to conceal a wrong label owner, copied OCR mistake or
missing content. Preserve the hierarchy; change the available layout when needed.
Frame/line metrics do not establish actual ink fit. Check the real PNG, including
neighbors, descenders, punctuation and rotated text. Native rotation is not a
perspective warp; follow [label geometry](label-geometry.md).

Choose placement before fitting: identify the local artwork surface, keep arrows
and icons out of the available text area, and decide which labels have the same
typographic role. OCR rectangles are text observations, not those surface bounds.
Do not derive each font size independently from OCR-width/native-ink ratios.

For a same-role set, declare `typography_groups` with `size_pt` in its fields.
This declaration belongs on the source **page**, not in the `fit` request:

```json
"typography_groups": [{
  "id": "labels",
  "members": ["label-a", "label-b"],
  "fields": ["size_pt"],
  "basis": "Two same-level labels identified from the artwork; heading excluded"
}]
```

`members` contains existing local text IDs; at least two are required. `fields`
is an array of property names, not a map such as `{"size_pt":18}`. The numeric
size is supplied only in the request's `group.size_pt`. For example:

```json
{
  "action": "fit", "spec": "source/deck.json",
  "group": {"page":"overview", "id":"labels", "size_pt":18},
  "reflow": true, "ink_align": "middle",
  "targets": [
    {"page":"overview", "id":"label-a", "box":[100,100,500,80]},
    {"page":"overview", "id":"label-b", "box":[100,250,500,80]}
  ]
}
```

These are synthetic schema examples; choose real membership, size and targets
from the current artwork. The tool's `help.fit` returns separate source and
request examples, also included with `fit` errors.

Call `fit` with `group:{page,id,size_pt}`, `reflow:true`, `ink_align` and one
explicit target per member. The chosen size is exact, not a per-label maximum.
If any member fails, the tool returns the blocking IDs and withholds every
group edit. Reconsider that target, wrapping or the whole group's chosen size;
do not apply the successful subset and silently shrink the difficult member.
Group membership and target geometry still require visual judgment.

`patch` merges `set.format` by default, retaining unmentioned properties. Read
`changes[].changed_format_fields` and the before/after effective format. Use
`format_mode:"replace"` on an edit only when replacing the entire override is
intentional; an empty replacement resets it to the shared style. Splitting a
wide sentence into independently placed labels uses `split_text`, documented in
[Rust runtime](rust-runtime.md), rather than deleting canonical copy entries.

Batch edits by cause, create a fresh candidate directory and compare actual
before/after images. Keep prior artifacts and unresolved findings. Return the
new PPTX, bound cycle and explicit review to the same peer coordinator; use
[continuation evidence](continuation-evidence.md) and the existing exit rules.

For several labels with known local owners, `layout_region` reuses this exact
fit engine to propose whole-group layouts and actual PPTX previews at explicitly
chosen common sizes. It also checks neighbor/declared-obstacle clearance. See
[semantic authoring](semantic-authoring.md); it does not infer semantic regions
or approve a candidate's appearance.
