# Host tools and model review

The host supplies mechanics through [Rust runtime](rust-runtime.md): `inspect`
for package/native-layout diagnostics, `render` or `cycle` for real candidate PNGs,
`fit` for font proposals, `geometry` for label frames, `crop` and `compare` for
visual evidence. Report the tool's stated coverage and unknowns honestly.

The model reconciles intended copy with the authoritative source and judges
artwork, reading order, density, occlusion and ownership in actual images. OCR
is optional detection support, not an authoritative transcript. A low collision
count, successful export or hash match cannot override visible defects.

Use a real independent peer for the required production review. An optional
host-provided visual service must receive actual pixels, preserve its model and
artifact identities, and return a complete response. Do not create another hidden
LLM client, scheduler or completion authority in the page project. Missing tools
or unsupported checks remain explicit unknowns.
