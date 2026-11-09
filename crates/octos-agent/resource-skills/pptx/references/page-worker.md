# Independent page worker

Read the common PPTX skill, [shared method](page-method.md), and
[Rust runtime](rust-runtime.md). Receive authoritative content, accepted design
and blank artwork, shared canvas/styles and an exclusive page-data/output path.
Do not invent new copy from OCR, regenerate accepted artwork, change shared styles
or write another worker's files. Report genuinely missing input.

Use the same `cycle` operation with `page: PAGE_ID`; do not write a new builder,
page adapter, renderer or QA harness. Inspect the actual PNG in your own session.
Repair from evidence and return page data, the bound cycle, reviewed PNGs and
unresolved findings. Do not claim an independent peer review if you self-reviewed.
The coordinator's accepted page order and final-deck review remain necessary.
