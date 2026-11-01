# UPCR-2026-075 — Launch Resolve Authentication Scope

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
response.

Admin-token and unauthenticated stdio behavior is unchanged. In particular,
stdio does not inherit a narrowing scope from a prior session/open binding.
Existing tenant-Host authorization is not widened or replaced. The launch DTO
and `session.workspace_cwd.v1` capability remain unchanged.
