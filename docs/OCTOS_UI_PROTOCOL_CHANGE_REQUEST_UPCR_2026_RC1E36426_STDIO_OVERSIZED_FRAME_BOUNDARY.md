# UPCR-2026-RC1E36426 — stdio oversized-frame drain preserves the following frame

Status: proposed
PR: TBD

## Decision record

**Problem (R-03, robust audit):** when a stdio NDJSON frame exceeds
`MAX_TEXT_FRAME_BYTES`, the transport drains the line with
`drain_until_line_boundary`, which used to discard the whole 8192-byte
read chunk containing the newline. Any bytes after the newline — the
start (or entirety) of the NEXT, legal RPC — were swallowed, so the
client's request and id vanished and the caller could wait forever; a
partially arrived next frame was truncated.

**Decision:** the drain consumes only up to the first `\n`; bytes past it
are pushed back into the reader buffer so the following frame parses
normally. Bytes retained this way can themselves exceed the cap (the next
line is also oversized), so the reader now treats `buffer > MAX` as its
own oversized case (drop prefix, drain to that line's newline, answer
`TooLarge`) instead of relying on the `== MAX` check that only capped
reads could reach.

**Wire compatibility:** none — the change is confined to the stdio
reader's byte accounting. The only observable difference is that a legal
frame following an oversized one is now delivered instead of lost.

Cite this record elsewhere as `UPCR-2026-RC1E36426`.
