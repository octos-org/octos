# Local Codex helper / Rust runtime comparison

Scope: the local `pptx/scripts` helpers used by Codex, not an assertion about all
Codex installations or an official built-in implementation. These helpers are
shared code plus deck-specific data and model-authored repair decisions. Reading
the scripts does not establish visual equivalence between renderers.

| Behavior | Local Codex code | Rust status / evidence |
|---|---|---|
| Font range and line budget | `native_fit.propose`: explicit min/max, binary search, max_lines even if the old frame fits | Added min/max, bounded search, max_lines including owner limit, exact fixed-size requests. `fitting_policy` exports/reopens the chosen layout. Previously max and line options were ignored. |
| Word/paragraph integrity | `native_fit.tokens/layout`: keeps Latin identifiers and combining marks; retains explicit paragraphs | Rust uses DeckCraft Unicode shaping/breaking and now rejects forced word breaks in fitting. It retains explicit newlines. Unicode break behavior is not bit-identical to Python's tokenizer. |
| Width/reflow | `native_fit.propose` measures at target width and may freeze explicit lines with wrap=false | Rust reflow is explicit (`reflow:true`); otherwise width is retained. Rust keeps engine wrapping, rather than copying Python-predicted line breaks. This is a deliberate API/renderer difference, not full interchangeability. |
| Visible ink positioning | `ink_layout/position_ink`: font glyph estimates, anchor-aware frame room | Rust measures actual isolated glyph pixels with its export engine, supports rigid rotated labels, and removes unused frame height. Actual export tests cover positive/negative rotation, anchors, CJK and multi-line text. Exact PowerPoint layout equivalence remains unproven. |
| Font verification | `native_inspect.font_runs`: exported run family/size, comparison to intended style | Added resolved exported `font_runs`, rendered sizes and `DECLARED_FONT_MISMATCH`. Rust also reports actual faces, fallback and missing glyphs. Source formatting alone is not visual proof. |
| Same-role typography | Local script exposes runs but does not infer list roles or enforce equality across every list | New explicit `typography_groups` QA checks actual exported formatting. This is an additional capability, not a missing Python function. Membership and chosen design size require producer judgment. |
| Arrow/card ownership | Model supplies local regions, source geometry and exclusion areas | Rust has quad/baseline/forbidden areas and registered placement constraints. Neither system can prove semantic ownership from a self-declared large box. Candidate-derived bounds are circular evidence. |
| Visible versus isolated glyphs | `native_inspect.attribute_ink` attributes full-minus-blank connected components using native/PDF mapping; ambiguity stays unknown | Added `composite_ink`: actual full/derived-blank render subtraction, connected components, reopened character cells, missing/ambiguous/unassigned signal. Automatically used in cycle inspection/QA. Real export tests cover hidden low-contrast text, overlapping objects, rotated multiline CJK and ligatures. Character cells come from DeckCraft rather than an independent PDF/Office renderer. |
| Raster/native duplicate candidates | `raster_overlap.proposals` joins OCR boxes on the raster-only layer to editable objects, then sends matching crops for judgment | Added `raster_overlap` action: native OCR on the bound derived blank, frame-overlap candidate queue, same-viewport design/blank/render crops. Real raster-text fixture verifies the complete path. Host peer supplies judgment; OCR still misses/hallucinates. Native OCR currently requires macOS. |
| Independent model review | `pptx_host_check` / `defect_followup` can invoke a configured visual model | Native runtime reuses the real host peer and its image-capable tool feedback. A second hidden model caller is not an algorithm to port; correct peer review and coverage must be verified operationally. |
| Font fallback policy | Fitter requires an explicit font map/face, checks coverage; contextual scripts require shaping support | Rust now rejects fallback/missing-glyph fitting and reports exact face hashes/indexes/variation coordinates. It resolves installed/bundled fonts rather than requiring a Python font-map file. Synthetic styles and cross-application font matching still need review. |
| Perspective / nonrigid labels | `label_geometry` marks perspective/shear and requires visual inspection | Rust marks nonrigid geometry and refuses to infer a rigid fit as perspective reconstruction. Neither solves arbitrary editable text perspective. |
| Contextual feedback | `native_inspect.panels/sweep_views/revision_evidence`: original/blank/render, unflagged quadrants, before/after | Rust `review_pack`, `compare_cycle` and finding-change reports provide corresponding views. Producing them does not prove the model opened and understood them. |
| Final visual acceptance | Explicit model review of original copy, artwork drift, actual render | Still required. QA checks cannot turn semantic unknowns into a pass, and visual quality parity cannot be claimed from test counts. |

The Rust tests use generic synthetic content; no task page IDs, answers, wording
or coordinates are embedded. Production page data and artwork remain producer-owned.
Tests executed for this slice are recorded in the task's audit log. Still open:
independent Office-renderer agreement, OCR support on non-macOS hosts, complete
arbitrary transformed/grouped text coverage, and measured visual repair recall
on held-out complex decks. Do not label the entire implementation parity-complete
from synthetic tests or a mechanism port alone.
