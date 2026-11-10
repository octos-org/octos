# Inspect the actual PPTX first

Call `pptx_tool` `inspect` with the candidate `input` and a report `output`.
This reads the package/native objects. Call `render` to obtain actual PPTX PNGs;
read those images, not the input design, for layout acceptance.

Keep three evidence sources separate: native text/structure, actual rendered
pixels, and any optional extraction/geometry aid. None alone proves all others.
Rendering is identified as DeckCraft-rendered; a successful native reopen does
not certify Microsoft PowerPoint. Test the target application when required.
Do not change correct content to appease an extraction artifact.

Use [Rust runtime](rust-runtime.md) and [review evidence](review-evidence.md).
