# Octos UI Protocol Change Request: Session Pane Snapshots

## Header

- Request id: `UPCR-2026-002`
- Title: Add `session/open` pane snapshots
- Author: M9 protocol workstream
- Date: 2026-04-24
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issue: M9 protocol contract

## Summary

This change request adds optional workspace, artifact, and git pane snapshots
to the `session/open` result. The snapshots let AppUI clients bootstrap the
right rail from server truth during open/resume without scraping local files or
guessing the workspace state.

The change is additive and capability-gated. A client that does not negotiate
`pane.snapshots.v1` receives the legacy `session/open` result.

## Wire Contract

Feature flag:

- `pane.snapshots.v1`

Method:

- `session/open`

Optional result fields:

- `workspace_root`: canonical server-approved workspace root when known
- `panes`: `UiPaneSnapshot`

`panes` contains:

- `workspace`: optional workspace tree summary
- `artifacts`: optional generated-artifact summary
- `git`: optional repository status/history summary
- `limitations`: open list of `{ code, message }` entries describing missing
  filesystem, artifact, or git information
- `generated_at`: server timestamp

Example:

```json
{
  "session_id": "local:demo",
  "workspace_root": "/repo",
  "panes": {
    "workspace": {
      "root": "/repo",
      "contract": ["api octos-app-ui/v1alpha1", "feature pane.snapshots.v1"],
      "entries": []
    },
    "artifacts": { "items": [] },
    "git": {
      "clean": true,
      "limitations": []
    },
    "limitations": [],
    "generated_at": "2026-04-24T00:00:00Z"
  }
}
```

## Compatibility

- `panes` and `workspace_root` are additive. Old clients can ignore them.
- Servers must emit `panes` only when `pane.snapshots.v1` is negotiated.
- Snapshot data is best-effort. Missing workspace roots, artifact directories,
  or git repositories are represented as `limitations`, not protocol errors.
- This UPCR does not define live pane-update notifications. Clients that need
  fresh state after open/resume must call the relevant AppUI methods or open a
  later protocol change request.

## Tests

Coverage lives in:

- `crates/octos-core/src/ui_protocol.rs`
  - `session/open` result round-trips with `workspace_root` and `panes`
  - feature constants advertise `pane.snapshots.v1`
- `crates/octos-cli/src/api/ui_protocol.rs`
  - `session/open` includes panes only when negotiated
  - pane snapshots prefer the approved session workspace root
  - missing git/workspace state appears in `limitations`
- M18 AppUI parity fixtures and runner
  - `e2e/fixtures/appui-conformance/m18-route-inventory.json`
  - `e2e/scripts/m18-appui-transport-parity-soak.mjs`

## Decision

Accepted by the M9 protocol workstream. The pane snapshot payload was already
referenced by the spec and implemented in source; this document restores the
missing change-control record.
