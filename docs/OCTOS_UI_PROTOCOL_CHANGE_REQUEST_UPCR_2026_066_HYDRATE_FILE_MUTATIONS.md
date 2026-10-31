# UPCR-2026-066 — Recover session change preview references

Status: proposed (implemented for review)
Task: T-0062
Capability: `state.session_file_mutations.v1`

## Problem

File-mutation progress and diff previews are already durable, but hydrate
returns neither the mutations nor their preview IDs. A client loses its
Changes list on selection or reload even though `diff/preview/get` still
serves the preview. Tool-card write paths cannot recover those references.

## Contract

Add optional `file_mutations: UiFileMutationNotice[]` to `session/hydrate`
when `include` requests messages (including the default). This is retained
actual mutation history in ascending ledger order through the captured
hydrate cursor, independent of the request's `after`. Read beyond a trimmed
hot ring from existing retained logs; exclude later events and other
session/workspace streams. No pending diff proposal is counted as a mutation.
No file content, inferred paths, or regenerated diff is added. Clients fold
this snapshot and then continue with live progress after the cursor.

The field is additive and ungated, with a discoverable feature like tool
write paths. Old clients ignore it; old servers omit it, so clients preserve
live observations when absent. Existing hydrate authorization, retention,
and diff-preview lifetime apply. Conversation-only rollback omits the field:
rewinding a transcript does not undo filesystem mutations.

## Validation

Cover cold recovery outside the hot ring, captured head and workspace
isolation, hydrate include/after behavior, and profile denial. Exercise real
file editing, session switching and page reload in the browser.
