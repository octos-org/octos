# Dashboard API Contract

This document defines the stable contract between the React dashboard (`dashboard/`) and the hosted backend (`crew-cli` API server).

## Scope

- Frontend: `dashboard/src/api.ts`
- Backend: `crates/crew-cli/src/api/router.rs` and handlers under `crates/crew-cli/src/api/`
- Authenticated web UI mounted at `/admin/`

## Base Paths

- Admin endpoints: `/api/admin/*`
- User self-service endpoints: `/api/my/*`
- Auth endpoints: `/api/auth/*`

The dashboard must call only these paths and must not depend on internal Rust module paths.

## Authentication Contract

- Session token is provided by `/api/auth/verify`.
- Dashboard sends `Authorization: Bearer <token>` for protected routes.
- Public auth endpoints:
  - `POST /api/auth/send-code`
  - `POST /api/auth/verify`
- Protected auth endpoints:
  - `GET /api/auth/me`
  - `POST /api/auth/logout`

When auth is disabled in backend config, protected routes may be accessible without token. The dashboard should still send the token if present.

## Endpoint Families Used by Dashboard

- Admin overview/profile lifecycle: `/api/admin/overview`, `/api/admin/profiles*`, start/stop/restart/status/logs
- Sub-accounts: `/api/admin/profiles/{id}/accounts*`, `/api/my/profile/accounts*`
- Skills management: `/api/admin/profiles/{id}/skills*`
- User management: `/api/admin/users*`
- Monitoring/system: `/api/admin/monitor/*`, `/api/admin/system/metrics`
- Provider/search test: `/api/admin/test-provider`, `/api/my/test-provider`, `/api/my/test-search`
- User self profile: `/api/my/profile*`

See `dashboard/src/api.ts` for the exact currently implemented calls.

## Compatibility Rules

1. Additive-first changes
   - Add new fields/endpoints without removing existing ones.
   - Keep existing response keys stable.
2. Breaking changes
   - Any field rename/removal or status-code semantic change is breaking.
   - Breaking changes require synchronized frontend+backend PR and docs updates.
3. Nullability
   - Backend may return `null` for optional objects (for example metrics when unavailable).
   - Frontend must treat nullable fields defensively.
4. Error payloads
   - Backend may return plain-text errors on non-2xx responses.
   - Frontend must handle non-JSON error bodies.

## Routing And Hosting Contract

- SPA is served under `/admin/` (trailing slash expected).
- React Router basename is `/admin`.
- Unknown non-admin paths are redirected to `/admin/` by backend static handler.

## Build Artifact Contract (Monorepo Embedded Mode)

- Source: `dashboard/`
- Embedded output: `crates/crew-cli/static/admin/`
- Build command: `scripts/build-dashboard.sh`

When dashboard source changes, embedded assets must be rebuilt and committed so `crew-cli` serves the updated UI.

## CI Enforcement

CI should validate:

- Dashboard dependencies install (`npm ci`)
- Dashboard build success (`npm run build`)
- Embedded asset sync (`scripts/build-dashboard.sh` followed by clean git diff)
