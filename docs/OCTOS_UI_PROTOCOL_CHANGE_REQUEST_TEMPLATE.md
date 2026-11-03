# UPCR-YYYY-RXXXXXXXX — <Title>

Status: proposed
PR: TBD

## Decision record

Copy this file and fill it in for every UI Protocol change. Naming:

- **New records** use a decentralized id: `R` + 8 random uppercase hex
  digits, minted before the PR exists with `scripts/new-upcr.py <slug>`
  (T-0328). The id never comes from a shared "next free number" table, so
  in-flight PRs cannot collide, and no PR edits a shared registry index.
- The filename is
  `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_YYYY_RXXXXXXXX_<SHORT_SLUG>.md`
  (uppercase-underscore slug mandatory) and the body must cite the exact id
  on its first line, like the heading above.
- **Legacy records** (`UPCR-YYYY-NNN`, numbered in merge order) keep their
  ids and shape; do not renumber them.
- `scripts/lint-upcr-registry.py` fails the `check` job on a duplicate full
  id in either namespace, a non-conforming filename, or a new record whose
  body never cites its id. `scripts/lint-upcr-registry.py --report` prints
  a generated human-readable index on demand — the output is not committed.
- `docs/upcr/README.md` is the frozen historical registry for the numbered
  era; it receives no new rows and is not a merge gate.

<What changes, why, the wire/durability contract, and compatibility notes.>
