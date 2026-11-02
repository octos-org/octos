# UPCR-2026-077 — Send File Transcript Order Without Wire Contract Changes

Status: proposed (implemented for review)
Task: T-0116
PR: #2937

## Decision record

This is a persistence-order correction with no OUP method, parameter, result
field, notification, capability, or error variant changes. Existing
`session/open`, `turn/start`, `session/hydrate`, `turn/spawn_complete`, and
canonical v2 projection shapes remain unchanged.

Explicit `send_file` deliveries are persisted through the turn's existing
`SessionManager`, the same writer used for the prompt and final response. This
keeps the assistant/media row in the active transcript and preserves the
existing merged sequence after reload. The generic background-completion path
continues using its canonical persistence route, so media identity and
backdated-row behavior are unchanged.

The change does not introduce another transcript layout, migration, or client
compatibility requirement. A regression fixture captured from a real replay
failure pins prompt → send_file delivery → final response ordering without
adding a new wire contract.
