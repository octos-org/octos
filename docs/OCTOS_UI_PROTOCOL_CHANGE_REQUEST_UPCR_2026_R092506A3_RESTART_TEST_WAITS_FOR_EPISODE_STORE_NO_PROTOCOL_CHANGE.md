# UPCR-2026-R092506A3 — Restart test waits for the episode store lock (no protocol change)

Status: proposed
PR: TBD

## Decision record

T-0657 changes one test file, `api/ui_protocol_external_turns_tests.rs`.
The restart test of external turns (UPCR-2026-RBB8E2A9A) now waits for the
previous in-process serve to release `episodes.redb` before serving again.

No product code changes. No method, params, result, error, capability,
feature token, event shape or storage format is added, removed or
altered.

Cite this record elsewhere as `UPCR-2026-R092506A3`.
