# Efficient production

Reuse accepted artwork, the shared compiled builder, styles and page-data schema.
Do not write helper programs per page. Group edits that address the same cause and
render after the batch. `cycle` combines build, reopen, render and inspection;
`patch` applies multiple object changes in one call. Read only relevant object
rows and actual images. See [Rust runtime](rust-runtime.md).

Use page-scoped attempts during local repairs; rebuild and visually sweep the
final assembled deck before delivery. Reuse earlier review only when the exact
artifact and relevant dependencies have not changed. The native runtime currently
rerenders requested pages; it does not claim the old service's cross-attempt cache.
Do not fabricate cache-hit receipts or skip verification to make a run look faster.

Use `crop` for dense contexts and `compare` for before/after evidence. Stable
unresolved finding IDs and meaningful progress determine repair continuation;
extra calls, filenames or moved boundaries alone do not demonstrate improvement.
