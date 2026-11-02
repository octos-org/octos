# UPCR-2026-081 — Launch Resolve Authentication Scope

Status: proposed (implemented for review)
Task: T-0178

## Decision record

This record documents an authorization correction for the existing
`launch/resolve` method. It adds no OUP method, parameter, result field,
notification, capability, or error variant.

For an authenticated user connection, folder scanning, sticky/default
candidates, and `existing_profiles` are confined to that identity's profile.
The identity also participates as the requested profile in the existing launch
decision. An explicit `params.profile_id` outside that profile is rejected with
the existing `auth_scope_violation` error data before resolution. As a result,
a successful resolution names only a profile that the same connection may pass
to `session/open`, and foreign profile ids/history are not exposed in the
response. If the authenticated profile is absent or disabled in the enabled
known-profile set, launch resolution returns `no_profile` with no
resolved/existing id instead of a launch-activation target. This launch-scope
correction does not change the existing manual `session/open` behavior for a
disabled profile.

Admin connections retain explicit cross-profile resolution. Their implicit
sticky behavior follows UPCR-2026-076 (admin-owned marker or admin/default),
not ordinary-user ownership. Existing folder writes may replace an admin-owned
marker with a user-owned marker; this change does not block that write. The next
implicit admin resolve ignores the user-owned marker and falls back to the
admin/default profile. For example, with non-default `zbeta` selected as admin
default and a user-owned `coding` marker/store, admin resolution names `zbeta`:
it resumes `zbeta` when that store exists, otherwise returns `cross_profile`
with `coding` as the existing choice. Stdio has no authenticated identity: it
neither inherits a narrowing scope from a prior session/open binding nor uses the
admin-owned sticky rule, and retains the generic sticky decision path. Existing
tenant-Host authorization is not widened or replaced. The launch DTO and
`session.workspace_cwd.v1` capability remain unchanged.
