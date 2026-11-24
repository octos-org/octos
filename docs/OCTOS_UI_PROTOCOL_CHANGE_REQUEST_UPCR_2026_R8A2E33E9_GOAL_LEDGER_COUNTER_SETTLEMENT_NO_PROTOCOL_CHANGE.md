# UPCR-2026-R8A2E33E9 — Goal-ledger counter settlement (no protocol change)

Status: proposed
PR: #3270

## Decision record

#2083/#2084 makes live turn and verifier charges settle exactly once into the per-profile goal ledger. The UI-protocol transport and session plumbing passes the owning profile's data directory to the existing accountant; it does not add or change any wire method, event, payload field, error code, capability, dispatcher rule, or client compatibility contract.

The additive charge writer exclusively owns token/time increments. Goal-ledger snapshots carry the stable counter base excluding those increments and MAX-fold that base, while a charge settles `MAX(row, base) + delta` in one immediate transaction. Snapshot-first and charge-first therefore converge without double-counting or losing spend. On an existing row, only the metadata snapshot writer advances `updated_at_ms`, so a later charge cannot suppress a queued budget or status update. Post-clear settlement and tombstone ownership remain unchanged.

The new regressions cover both writer orders, first-row creation, lagging bases, and budget metadata ordering. `docs/upcr/README.md` intentionally has zero diff under the decentralized-id rule.
