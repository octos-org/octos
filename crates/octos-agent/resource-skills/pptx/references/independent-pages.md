# Independent pages

Parallel page work requires authorization and independent write ownership. First
agree the full-deck design, content, shared typography and acceptance criteria.
Generate/approve the required page artwork before delegating reconstruction.
Prove the common method on a representative page and share the same skill/tool
version and a small valid example, not its answer or deck-specific coordinates.

Inventory every page and assign each independent page group to exactly one
maker; record any unassigned or coupled pages. A maker may own several pages when
peer slots are fewer than pages. Give each maker its exclusive page-data files
and output paths, design/blank assets, authoritative copy, shared canvas/styles,
and expected page IDs. Both maker and independent reviewer need direct images.
Workers may use `cycle` with `page` for focused previews; they do not edit the
shared builder or shared styles. See [page worker](page-worker.md) and
[Rust runtime](rust-runtime.md).

Check the actual host's dispatch limits and available tool profile before
assigning work. In this version the CLI admits up to four `peer_handoff` calls
per master turn, including a new reviewer; other versions can differ. Assign
multiple disjoint pages per maker where necessary. Record maker session/turn IDs,
page ownership, start/end or equivalent host events and terminal outcomes. The
dispatch calls can be serial while peer turns overlap; report concurrency only
when those intervals show it. A multi-page maker can process its own pages
sequentially, so coverage does not prove all pages ran simultaneously.

Use page references in the root spec: `{"path":"pages/overview.json"}`. Resolve
assets relative to that page file. The coordinator keeps an ordered inventory of
accepted page sources and hashes. Verify current source/assets against the worker's
cycle before assembly; do not accept a pretty preview from different bytes.
Assemble by running the same `cycle` over the ordered root spec, never by overlaying
full-page screenshots or letting workers change shared slides. Review the final
PPTX rendering for complete text, page order, shared typography and regressions.

Collect a complete round of review findings before routing independent page
repairs back to their owners. Batch related defects on the same page, rather
than rebuilding the whole deck for each one. Build the final combined candidate
once after the owned page data is ready; do not infer that the built-in `patch`
returns a single page unless the loaded runtime explicitly advertises it.
Maintain the original assets and source evidence. If the user asks to remove
intermediate PPTX copies, clean only this task's superseded copies after final
acceptance; retain the delivered deck, previews, source inputs and review
receipts needed to support the conclusion.

Before using `patch.output_page`, call
`{"action":"help","topic":"patch"}` and require
`supports_output_page:true`. Each peer's request must edit only its assigned
page and use a unique output path. Require `output_kind:"page"` and the
expected `page_id`, and check the returned source bindings against the frozen
baseline before putting page files into the ordered root spec. This applies
to a deck with source data, not arbitrary extraction from an existing PPTX.
Page output binds and structurally validates every source JSON, but verifies
only the selected page's images. The final full cycle must verify all page
assets before review; one successful page patch is not an all-deck check.
`sources` binds the root and referenced page JSON, not the image bytes. Check
the emitted page's `background`, `design`, and each `artwork_layers[].image`
hash against the frozen image inventory (or the approved hash for a newly
introduced repair asset). A matching JSON source binding alone does not prove
that the selected artwork matches the approved input.

Measure elapsed time from preparation through final independent QA, separating
dispatch, peer work, waiting, JIT compilation, integration and review where
the host exposes them. A speed comparison needs the same input, scope and
acceptance criteria; do not infer an overall speedup from worker count.

Use existing peer/subagent completion and cancellation mechanisms. Do not add a
polling daemon or repeatedly restart stalled workers. Preserve unresolved findings
and apply [peer exit criteria](peer-review-exit.md); final acceptance belongs to
the independent review session, not worker tool success.
