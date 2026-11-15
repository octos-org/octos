# Efficient production

Reuse accepted artwork, the shared compiled builder, styles and page-data schema.
Do not write helper programs per page. Group edits that address the same cause and
render after the batch. `cycle` combines build, reopen, render and inspection;
`patch` applies multiple object changes in one call. Read only relevant object
rows and actual images. See [Rust runtime](rust-runtime.md).

For a repair pass, collect the reference/blank/current evidence for related
objects together, then decide their shared constraints before editing. Use `fit`
for measured placement, or `layout_region` to compare explicitly chosen common
sizes with actual candidate previews in one call. Apply the selected group and
other independent changes together, then inspect the resulting page and its
neighbors. A new pass should answer a remaining observed defect or a new
measurement conflict, not just repeat a slightly different numeric guess.

Local raster restoration belongs in page `artwork_layers`, so `cycle` builds and
checks the complete stack. Avoid the lower-level insert/reorder/export sequence
for this supported case. Review patch seams and geometry as well as the original
defect. Keep output reports to the selected changes, evidence paths and remaining
issues; do not retell every unsuccessful numeric experiment.

Use one current candidate pointer per page and a latest-state review queue, not
one queued peer message per status/ruling update. `peer_send_input` to a busy
peer queues another turn; it does not replace older requests. Keep at most one
outstanding review packet per reviewer, update its referenced queue, and ask the
reviewer to reconcile current candidates before each turn. Retire superseded
requests and reuse unchanged valid evidence. Route a page's complete finding
batch immediately while other pages are reviewed; do not wait for the whole deck.

Keep peer connections alive through integration QA. A host using official
active-only steering should target the existing turn, without reopening and
closing that peer's routing session. A lost connection calls for reconnecting
the same peer and resending the undelivered task once, not a duplicate maker or
self-approval. Use only the deployed runtime's actual advertised interfaces.

Use page-scoped attempts during local repairs; rebuild and visually sweep the
final assembled deck before delivery. Reuse earlier review only when the exact
artifact and relevant dependencies have not changed. The native runtime currently
rerenders requested pages; it does not claim the old service's cross-attempt cache.
Do not fabricate cache-hit receipts or skip verification to make a run look faster.

Use `crop` for dense contexts and `compare` for before/after evidence. Stable
unresolved finding IDs and meaningful progress determine repair continuation;
extra calls, filenames or moved boundaries alone do not demonstrate improvement.
