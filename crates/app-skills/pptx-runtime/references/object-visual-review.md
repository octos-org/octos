# Readable object evidence

Use the task's selected model for visual judgments. This workflow does not call
another vision model and does not require a model-specific prompt or page map.

Keep measurement and observation distinct. Do not generate `pass` observations
from the same OCR centers or width ratios used to place the text: that only checks
agreement with the placement assumption. A claimed card boundary needs artwork
evidence independent of the text rectangle. Review same-role neighbors together;
matching one label's reference width does not establish typography consistency.
The independent reviewer uses the original/blank/current images, rather than
inheriting the maker's numeric classifications as visual conclusions.

1. Run `review_pack` for a current `cycle` and fresh output directory. Read the
   compact `view-index.json` first (`view_batches` in the full report retains measurements): the full page establishes meaning; each object group
   supplies separate original / blank / actual PPTX render crops. Absent original
   references remain absent. The declared owner is a hypothesis, not proof.
2. Read each distinct group in the index once, examining all listed objects;
   shared crops do not require repeated requests. Reopen only for unresolved
   context. Open one matched group at a time using the host's image-capable read/view tool.
   Crops preserve source pixels and include the owner and neighbors. Do not make
   a contact sheet that shrinks small type, attach the entire deck to one request,
   or substitute OCR text for image input. Check image omission warnings; reread
   omitted groups. If a crop omits relevant context, inspect the corresponding
   wider viewport/full page before judging. Request-level image budgets may still
   apply; a file path or crop creation does not mean pixels reached the model.
3. Author a fresh observations JSON using the template's `review_id`; preserve
   its object identities and `relation_reviews`. Do not copy opaque hash strings
   through model output or write a script to manufacture review metadata. Merge its `object_reviews` into the
   existing review without deleting comparisons/findings. Each object needs five
   brief observations with `status: pass | defect | unknown`:
   - `ownership`: identify the intended label/card/arrow and supporting visual cue;
     an unboxed title is allowed when the design supports it.
   - `containment`: actual visible glyphs versus the owner's real edges, including
     irregular corners; a large rectangular text box is not the owner.
   - `alignment`: baseline/center/edge relation and spacing to neighboring objects.
   - `direction`: baseline direction versus the label surface; rotation is not
     perspective. Describe deliberate horizontal text as such.
   - `typography`: readability, wrapping and same-role size/style consistency.
   Use concrete observations, not repeated “looks fine”. If the image cannot
   resolve the question, record unknown and obtain better evidence.
   Each group's `relation_signals` contains native measurements: a local surface's
   angle/centre and, where native OCR is available, same-text reference anchors.
   Inspect them in the matched images. These are candidates, not instructions to
   centre every label or restore the original font size. For each signal ID supply
   `status: defect | intentional | unknown` and a concrete `basis`. An intentional
   exception also requires a `support_id` selected from that signal's
   `intentional_options`. These options are calculated by Rust from the bound
   reference/current geometry; do not invent an ID or reuse another signal's proof.
   Preserving a left edge cannot explain loss of an established centre relation;
   preserving a horizontal baseline cannot justify a position error. When the
   reference text was centred in the same registered surface, that relation stays
   visible even after the font changes. An option supports a geometric explanation,
   not aesthetic approval: judge whether it actually applies to the intended owner.
   With no applicable option, keep the relation unknown or defect. Do not relabel
   uncertain measurements as defects just to finish, or rewrite the report/source
   to create an exception. `object_measurements` gives actual ink coordinates even
   when no signal fired; frame bounds are not glyph bounds. Missing OCR, changed
   copy, ambiguous matches and unsupported surfaces remain unknown. No signals
   does not mean the page passed; duplicated icons still require direct inspection.
   `RASTER_STROKE_CONTACT` supplies measured native-glyph contact points with
   contrast edges in the actual blank render, independent of OCR and authored
   forbidden boxes. `RASTER_OCR_OVERLAP` separately reports blank OCR intersecting
   the actual ink envelope. Use their local original/blank/current crops; OCR
   failing to read an arrow does not clear a measured pixel contact. A contact is
   not automatically an unintended collision: textured photographs and deliberate
   overlays can need further evidence. Do not erase artwork simply to silence a
   candidate. Unresolved intent remains unknown under the existing exit policy.
   Final `evidence_record` remeasures `ARTWORK_CONTACT_UNVERIFIED` even without an
   attached relation report; a prose pass or editing a saved inspection cannot
   clear it. A repaired placement needs a new actual render and fresh measurement.
4. Use existing `geometry`, `fit`, `layout_search` and native QA for measurements
   and repair. Translate crop coordinates through the supplied pixel offset and
   source-to-canvas scale. Difference images locate candidates; they do not decide
   semantics or establish quality. Do not shrink all fonts to make warnings vanish.
5. Submit observations with `{"action":"review_submit","pack":"evidence/report.json",
   "observations":"review/observations.json","output":"review/bound.json"}`.
   Rust verifies the reviewed artifacts, binds metadata and rejects omitted relation
   responses, unsupported intentional exceptions, and a `defect`/`unknown` signal
   paired with a contradictory object pass. It returns all issues in one response.
   It does not decide whether an intentional exception is aesthetically justified.
   Missing checks remain unknown; submission is not quality approval.
6. After an edit, render a fresh cycle and review the final bytes. `evidence_record`
   recomputes the required inventory, accepts only matching `evidence_hash`, and
   keeps absent/stale/unknown observations incomplete. Unchanged page evidence can
   be reused; a changed page or neighbor invalidates its object observations.
   `object_coverage` names precisely which objects/checks remain unreviewed.

The tool checks coverage, provenance and existing geometry constraints. It cannot
prove that an image was understood or that a written observation is correct.
Whole-page visual review and independent review, when selected, remain necessary.
Use the existing stopping policy: passing means no unresolved required defects;
stagnation exits with an explicit incomplete result, never an invented pass.

See [review lifecycle](review-lifecycle.md) for immediate finding registration, a separately frozen independent visual route (native failures stay blocking), and verified local observation reuse.
