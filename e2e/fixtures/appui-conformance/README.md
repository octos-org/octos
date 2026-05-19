# M18 AppUI Conformance Fixtures

These fixtures support octos#1030 and octos#1032.

- `m18-route-inventory.json` lists the AppUI route surface that the M18
  parity runner checks against negotiated capabilities.
- `m18-conformance-allowlist.json` records deliberate WebSocket-vs-stdio
  non-parity with a reason and an expected removal condition.

The allowlist is intentionally narrow. A method advertised by stdio and then
failing with transport-specific behavior is a failed parity check, not an
acceptable allowance. Current allowances are limited to normalized environment
differences such as per-run absolute paths.

The M18-J runner exercises capability negotiation, profile create/open/status,
router probes, auth/content probes, turn start/approval/interrupt, a
reconnect-style replay probe, and deterministic route-inventory probes for the
remaining advertised methods. Methods without a deterministic unauthenticated
semantic probe are marked capability-only rather than broadening the allowlist.
Auth-bound methods unavailable on unauthenticated stdio are classified as
`auth_bound_stdio_unsupported` in the route inventory.
Live turn notifications are checked through per-turn semantic summaries so
approval and interrupt races do not become order or extra-delta flakes;
non-live notifications and every RPC exchange remain transport-compared. Runtime
parity gaps are expected to appear in
`normalized-diff.json` as `unexpectedDifferences`. Unauthenticated stdio
auth-bound methods are omitted from `supported_methods`, listed as explicit
`unsupported` capabilities, and exercised only as direct non-parity auth probes
that must return typed `auth_unavailable`.
