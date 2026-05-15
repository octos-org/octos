# UPCR-2026-018: Local Solo Onboarding And Policy Inspection

Status: accepted

## Summary

Add AppUI methods for local no-OTP solo onboarding and server-owned permission
policy inspection. The methods are additive and transport-neutral; WebSocket
and stdio clients use the same JSON-RPC names, params, results, and errors.

## Methods

- `profile/local/create`
- `permission/profile/list`
- `permission/profile/set`
- extended `session/status/read.runtime_policy_stamp`

## Contract

- `profile/local/create` creates or returns the local solo owner profile from
  `name`, `username`, and `email`.
- The onboarding email is metadata only. The server must not send OTP mail,
  call `auth/send_code`, call `auth/verify`, or require SMTP configuration.
- The server derives `profile_id` from the normalized username, creates a
  local `User`, and creates a matching `UserProfile`.
- Repeating the call with the same normalized username/name/email is
  idempotent and returns `created: false`.
- Repeating the call with the same normalized username and different owner
  metadata is rejected with `invalid_params` and
  `data.kind = "profile_local_collision"`.
- The method is local solo only. Tenant/cloud runtimes reject it with
  `permission_denied` and `data.kind = "profile_local_unsupported"`.
- `permission/profile/list` returns the server-supported permission profiles
  for the addressed session.
- `permission/profile/set` applies server-owned policy state. Its partial
  `update` accepts `mode`, `network`, and `approval_policy`.
  `approval_policy` accepts `on-request`, `on_request`, `ask`, and `never`;
  clients send `on-request` to clear a previous `never` selection.
- `permission/profile/set` rejects `danger_full_access` and
  `approval_policy=never` outside local solo mode with `permission_denied` and
  `data.kind = "permission_profile_disallowed"`.
- `session/status/read.runtime_policy_stamp` exposes the effective
  `runtime_mode`, `profile_id`, `workspace_root`, `approval_policy`,
  `sandbox_mode`, `permission_profile`, `filesystem_scope`, `network`,
  `tool_policy_id`, `mcp_servers`, and `memory_scope`.

## Compatibility

Older servers reject `profile/local/create` with `method_not_supported`.
Clients must consult `config/capabilities/list.supported_methods` before
showing local onboarding controls.

Servers that support this contract advertise:

- `profile.local_create.v1` when `profile/local/create` is available
- `permission.profile.v1` when permission profile inspection is available
- `runtime.policy_stamp.v1` when the extended status stamp is available

## Runtime Integration Note

The AppUI server owns method shapes, validation, rejection, and inspection.
Runtime enforcement must consume the selected permission profile before
tool-registry, shell-policy, sandbox, filesystem-scope, and network decisions.
That enforcement hook is owned by the runtime/session policy workstream, not by
client code.

## Tests

- `profile/local/create` creates local `users/<profile_id>.json` and
  `profiles/<profile_id>.json` without OTP.
- `profile/local/create` is idempotent for the same owner metadata.
- `profile/local/create` rejects username collisions and invalid fields with
  typed AppUI errors.
- `permission/profile/list` omits or rejects `danger_full_access` outside
  local solo mode.
- `session/status/read` reflects the server-effective policy stamp.
