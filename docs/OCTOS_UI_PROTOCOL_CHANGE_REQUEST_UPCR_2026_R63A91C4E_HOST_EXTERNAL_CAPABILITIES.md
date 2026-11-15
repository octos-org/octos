# UPCR-2026-R63A91C4E — Host-managed external capability filtering

- Task: T-0627 / T-0621 F03.
- Stacked on UPCR-2026-RC81B9296 and UPCR-2026-R2E4C10A7 for ordered conflict resolution.
- No wire schema change.

## Contract

1. A host-managed external credential receives only `supported_methods` from the server's external execution allowlist. This filter applies to `client_hello`, `config/capabilities/list`, and direct `session/open` capability results/notifications.
2. `permission.profile.v1` is omitted when both permission SET and LIST are filtered out. The host/admin connection retains the existing methods and feature.
3. Filtering is not authorization. A direct external SET/LIST call still fails with the existing structured `external_method_denied`; no session ownership or durable-permission check is weakened.
4. Identity comes only from the authenticated host-managed credential, never User-Agent, client labels, or requested runtime mode.

## Acceptance

The existing real-binary host-managed stdin-token fixture exercises external and host WebSockets. It compares hello/config/open advertisements and confirms direct SET/LIST denial remains intact.
