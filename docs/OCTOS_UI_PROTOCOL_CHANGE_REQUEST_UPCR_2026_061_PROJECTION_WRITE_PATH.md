# UPCR-2026-061 — Structured Write Path in v2 Tool Replay

Status: proposed (implemented for review)
Task: T-0046
Issue: #2894
Capability: `projection.write_path.v1`

## Problem

`PayloadV2::ToolStart` carried only a 700-byte `arguments_preview`. For
`write_file` and `edit_file`, long `content` can precede `path`, so hydrate
replay loses the write path and clients cannot restore artifact cards. Live
`tool/started` already has full arguments and is unaffected.

## Change

Add optional `write_path` to `PayloadV2::ToolStart`. It is the `path` argument
from the original tool call, UTF-8 bounded to 1024 bytes and passed through the
existing UI secret redaction. `tool_call_id`, tool name, and the existing
bounded arguments preview are unchanged. File content is never replayed in
this field.

## Compatibility

The field is additive and omitted when no string path exists, so old clients
can ignore it without changes. The capability lets clients discover support;
canonical v2 envelopes remain compatible JSON. No full structured argument
object is replayed, avoiding hydrate growth and accidental content leakage.
