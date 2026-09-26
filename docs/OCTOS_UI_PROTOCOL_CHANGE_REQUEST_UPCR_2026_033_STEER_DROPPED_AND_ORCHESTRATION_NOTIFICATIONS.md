# Octos UI Protocol Change Request: Advertise `turn/steer_dropped` and `session/orchestration`

## Header

- Request id: `UPCR-2026-033`
- Date: 2026-09-26
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: two additive `supported_notifications` advertisement entries; no new
  or changed frames

## Problem

Both notifications ship on the wire but were absent from
`UI_PROTOCOL_NOTIFICATION_METHODS`, so every surface derived from it — the
`client_hello` / capabilities `supported_notifications` list and the wire
inventory's Notifications table — never mentioned them:

- `turn/steer_dropped` — emitted from the steer path at turn end when
  accepted steer inputs were still pending. Delivery is unfiltered: the
  negotiated `event.turn_steer_dropped.v1` feature (itself already
  advertised) licenses the client-side inference described below, not the
  send.
- `session/orchestration` — the whole-job orchestration indicator emitted to
  connections with open/subscribed sessions.

Neither frame shipped with a UPCR or a spec entry; this record documents
both and pins their advertisement.

A strict client validating frames against the advertised set would reject
legitimate frames of either kind. Found by the wire-inventory parity lint
(#2425), which could only warn about them (#2540).

## Contract

- Both names are appended to `UI_PROTOCOL_NOTIFICATION_METHODS`, in the
  positions their constants already occupy. `full_protocol`,
  `first_server_slice`, and `for_negotiated_features` all build
  `supported_notifications` from that constant, so every advertisement
  surface picks them up and nothing else changes shape.
- **Advertisement ≠ emission.** Delivery rules are untouched:
  `turn/steer_dropped` is still sent to any connection settling a turn with
  leftover steers — the negotiated `event.turn_steer_dropped.v1` feature
  only licenses a client to treat a terminal without a preceding
  `turn/steer_dropped` naming its steer as consumed (see
  `specs/task-return-unconsumed-steer-inputs.spec.md`, Decisions) — and
  `session/orchestration` is still scoped to the connection's own
  open/subscribed sessions and deduped to changes. The list already
  contains feature-gated notifications (`background/activity`), so keeping
  the list verbatim matches existing practice. A per-notification gate
  mirroring `method_capability_gate` was considered and rejected as a
  larger protocol change with no concrete client need today.
- Growing `supported_notifications` changes the capabilities payload for
  every client — the same cost every addition to the list has paid (most
  recently `background/activity`, #2025). No emission site, filter, or
  ledger path reads the list, so server delivery behavior is unchanged.

## Risk

- A client that rejects unknown names in the server's advertised
  `supported_notifications` would newly see two names. Such a client already
  has to track the list's historical growth, while today's failure mode is
  strictly worse: the frames arrive regardless, so a client validating
  against the advertised set rejects legitimate `turn/steer_dropped` /
  `session/orchestration` frames.

## Tests

- `wire_shipped_steer_dropped_and_orchestration_notifications_are_advertised`
  (red on `main` before the fix)
- `ui_protocol_v1_representative_wire_payloads_are_golden` (pins the exact
  serialized capabilities payload)
- `spec_section6_catalog_lists_every_advertised_method` (§6 catalog stays a
  superset of the advertised set)
- `scripts/lint-ui-protocol-inventory.py` (#2539): the Notifications table
  equals the constant and the blind-spot warnings drop 2 → 0
