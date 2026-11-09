# UPCR-2026-RC88AB899 — session-lifecycle .gitignore + a test-line offset

Status: proposed
PR: #3146

## Decision record

The per-project session store's managed `.gitignore` now excludes the kernel's
own `.session-lifecycle/` lock/threads files so they no longer appear as user
files in `git status` (and are not treated as snapshot/undo candidates). A
pre-existing `.gitignore` is backfilled with only the missing managed pattern
(append, never rewriting operator content). No wire-visible protocol change;
the test-only edit to `ui_protocol_tests.rs` deletes a non-kept structural
shape test to offset the ratchet. Cite this record elsewhere as
`UPCR-2026-RC88AB899`.
