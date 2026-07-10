# Octos UI Protocol Change Request: §6 Envelope-Model catalog resync + drift guard

## Header

- Request id: `UPCR-2026-027`
- Title: Resync the §6 "Envelope Model" method catalog and guard it against drift
- Author: UI Protocol spec-consistency audit
- Date: 2026-07-10
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Sibling UPCRs: none (documentation-reconciliation record; the methods below
  each shipped under their own feature work)

## Summary

Strictly a documentation + test-tooling change. It adds **no** new method,
notification, payload, enum variant, capability bit, or protocol identifier —
the wire surface is unchanged. Two things happen:

1. The §6 "Envelope Model" catalog is resynced to list six already-shipped,
   already-advertised methods that had drifted out of the hand-maintained
   catalog.
2. A guard test is added so the catalog can no longer silently fall behind the
   advertised method constants.

This UPCR exists because the completeness gate (`scripts/check-ui-protocol-upcr.sh`)
treats any edit to a Rust protocol file (`crates/octos-cli/src/api/ui_protocol.rs`,
which is where the guard and the `APPUI_EXTRA_METHODS` list live) as a
protocol-visible change requiring a paired UPCR. The change is documented here
so that governance stays honest even for a docs/test reconciliation.

## Motivation

§6 is a **hand-maintained mirror** of
`UI_PROTOCOL_COMMAND_METHODS ∪ UI_PROTOCOL_NOTIFICATION_METHODS` (octos-core) ∪
`APPUI_EXTRA_METHODS` (octos-cli). Nothing enforced its completeness — the
existing `check-ui-protocol-upcr.sh` only checks that a protocol edit ships with
*a* UPCR doc, not that §6 lists every advertised method — so it drifted. Six
methods were advertised and implemented but absent from the catalog:

| Method | Kind | Shipped in |
|---|---|---|
| `session/rollback` | command | #1516 |
| `session/fork` | command | #1613 |
| `session/btw` | command | #1609 |
| `message/reasoning_delta` | notification | #1502 |
| `voice/audio_chunk` | notification (gate `event.voice_audio.v1`) | #1504 |
| `plan/updated` | notification (gate `plan.todos.v1`) | #1622 |

§6 referenced no phantom methods, so this was pure under-documentation — not a
wire-breaking contradiction.

## Changes

- §6 gains the six methods in their natural groups. `voice/audio_chunk` is
  documented as gated by `event.voice_audio.v1`; the whole-file `file/attached`
  fallback is itself gated by `event.file_attached.v1`, and since the reply
  audio has no other carrier, a client with neither capability receives none
  (matching the server's two independent delivery filters). `plan/updated` is
  documented as gated by `plan.todos.v1`.
- New test `spec_section6_catalog_lists_every_advertised_method`
  (`octos-cli`, `api` module) reads the spec at runtime, bounds §6 at the next
  `## ` heading (fail-closed rather than a hardcoded `## 7.`), and asserts §6 is
  a superset of the three constant lists. Matching folds each entry's wrapped
  continuation lines into one logical bullet, restricts to the method-token head
  (before the first `(`/`—`) so prose can't mask an absence, and treats `.` as
  part of a dotted token so `session/status.get.v2` can't satisfy
  `session/status.get`.
- The guard is wired into CI explicitly (`ci.yml`, `ci-self-hosted.yml`,
  `scripts/milestone-ci.sh`) via
  `cargo test -p octos-cli --features api spec_section6_catalog_lists_every_advertised_method`,
  because the `api` module is feature-gated and the unfeatured lib/integration
  jobs never compile it.

## Compatibility

- Strictly additive/documentation-only: no existing method, notification,
  payload, enum variant, capability bit, or protocol identifier changes.
- No client or server behavior changes.

## Implementation anchors

- Spec: `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` §6 "Envelope Model"
- Guard: `octos-cli/src/api/ui_protocol.rs`
  `spec_section6_catalog_lists_every_advertised_method`
- CI: `.github/workflows/ci.yml`, `.github/workflows/ci-self-hosted.yml`,
  `scripts/milestone-ci.sh`
