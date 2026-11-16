# Independent page worker

Read the common PPTX skill, [shared method](page-method.md), and
[Rust runtime](rust-runtime.md). Receive authoritative content, accepted design
and blank artwork, shared canvas/styles and exclusive page-data/output paths.
In the default full-deck page-first workflow, own exactly one page and retain it
through repairs. A different assignment requires an explicit user choice. Do not
take another maker's page to fill idle time; report your current state instead.
Do not invent new copy from OCR, regenerate accepted artwork, change shared styles
or write another worker's files. Report genuinely missing input.

Use the same `cycle` operation with `page: PAGE_ID`; do not write a new builder,
page adapter, renderer or QA harness. Inspect the actual PNG in your own session.
Repair from evidence and return page data, the bound cycle, reviewed PNGs and
unresolved findings. Do not claim an independent peer review if you self-reviewed.
Publish the first readable candidate promptly rather than performing a complete
independent review yourself. After a review, fix the complete defect batch for
your page, check affected neighbors locally, and publish once. Only the reviewer
awards a score or acceptance.

Use immutable per-pass page sources with correct asset-relative paths; the cycle
must bind the source actually used. Publish one current `candidate.json` pointer
with exact source/PPTX/render/dependency/receipt hashes (see native
`help topic:page_gate`). When native post-build repairs exist, name and render
the post-built PPTX, not the cycle's earlier editable file. Keep the candidate
and its dependencies frozen until reviewer release; a changed candidate requires
fresh acceptance. Do not edit the coordinator's plan or the reviewer's verdict.

The coordinator's all-page admission and final-deck review remain necessary.
