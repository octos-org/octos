# UPCR-2026-R1C7A9ECC — Test-only dead-code removal in UI-protocol modules (no protocol change)

Status: proposed
PR: #3125

## Decision record

T-0534 removes production code that was provably read only by tests, inside
`api/ui_protocol_*` modules. Nothing on the wire changes:

- `UiProtocolLedger`'s scan keeps its per-file skip warnings; the existing
  `skipped_records` aggregate is now also read by an end-of-scan runtime
  warning. Ledger record format, cursors, replay and recovery semantics are
  untouched.
- `ui_protocol_tests.rs` / `ui_protocol_smart_home_*_tests.rs` lose only
  assertions that called the retired REST handlers (`/api/my/cron*`,
  `/api/my/smart-home*`) whose routes no longer exist; the WS method
  coverage (`cron/*`, `smart_home/*`) is unchanged.
- `SmartHomeCommandError` retains the bridge `detail` strings and error-body
  construction. With the retired REST handlers gone, the WS wrappers consume
  them for a server-side diagnostic before sending the unchanged RPC error;
  the client-visible `status`/`reason` mapping is byte-identical.

No capability, feature token, method, error shape, or storage format is
added, removed, or altered. Cite this record elsewhere as
`UPCR-2026-R1C7A9ECC`.
