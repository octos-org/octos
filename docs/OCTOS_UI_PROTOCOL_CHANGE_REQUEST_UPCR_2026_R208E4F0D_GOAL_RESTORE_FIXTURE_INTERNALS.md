# UPCR-2026-R208E4F0D — Goal restore fixture internals

Status: proposed
PR: #3314

## Decision record

This change is internal to the `transport-test-fixtures` / test-only M9 task-output fixture and the goal-ledger restore path. It threads the already-admitted connection/routed profile through the fixture runner, resolves the canonical profile data dir, and installs the shared goal task-row observers before supervisor replay. A bare session key therefore uses the same tenant goal ledger as production instead of silently falling back to `_main`.

If the effective profile or its data dir cannot be resolved, the fixture now fails explicitly with its existing `task_fixture_failed` outcome rather than silently skipping goal-row coverage or selecting a different tenant's ledger. This is confined to the deterministic fixture; it does not change the production `run_standalone_turn` path.

**No UI Protocol contract changes.** No RPC methods, payload schemas, notification variants, capabilities, feature tokens, serialization, replay envelopes, or client-visible production behavior are added, removed, or altered. The OUP specification and client codecs need no corresponding change. The SQLite candidate-index migration and the at-least-once restore documentation are likewise internal and have no wire effect.

The regression uses a bare session under a non-main tenant and proves both restore reconciliation and fixture registration/settlement through the real fixture runner. Cite this record as `UPCR-2026-R208E4F0D`.
