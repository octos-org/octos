# Independent pages

Parallel page work requires authorization and independent write ownership. First
agree the full-deck design, content, shared typography and acceptance criteria.
Generate/approve the required page artwork before delegating reconstruction.
Prove the common method on a representative page and share the same skill/tool
version and a small valid example, not its answer or deck-specific coordinates.

Each worker receives its own page data file, design/blank assets, authoritative
copy, shared canvas/styles, expected page ID and an exclusive attempt directory.
Both worker and reviewer need direct images. Workers may use `cycle` with `page`
for focused previews; the shared builder is already compiled, so no worker writes
or recompiles it. See [page worker](page-worker.md) and [Rust runtime](rust-runtime.md).

Use page references in the root spec: `{"path":"pages/overview.json"}`. Resolve
assets relative to that page file. The coordinator keeps an ordered inventory of
accepted page sources and hashes. Verify current source/assets against the worker's
cycle before assembly; do not accept a pretty preview from different bytes.
Assemble by running the same `cycle` over the ordered root spec, never by overlaying
full-page screenshots or letting workers change shared slides. Review the final
PPTX rendering for complete text, page order, shared typography and regressions.

Use existing peer/subagent completion and cancellation mechanisms. Do not add a
polling daemon or repeatedly restart stalled workers. Preserve unresolved findings
and apply [peer exit criteria](peer-review-exit.md); final acceptance belongs to
the independent review session, not worker tool success.
