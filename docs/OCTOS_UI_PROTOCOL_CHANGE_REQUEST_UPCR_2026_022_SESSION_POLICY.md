# UPCR-2026-022: Session Sandbox Narrowing

Status: proposed
Date: 2026-05-25

## Summary

Add an optional, capability-gated sandbox policy object to `session/open` so a
client can narrow the sandbox for one session without changing the profile's
default runtime policy.

This closes the protocol gap found by the M11 SOAK-5 validation: two sessions
under one profile need to prove distinct sandbox policies such as one
network-denied room and one default room. The client request is not trusted as a
source of authority. The server validates it against the profile-derived
sandbox and rejects any request that would widen access.

## Capability

Clients must negotiate:

- `session.sandbox.v1`

Servers that advertise this feature accept `session/open.params.sandbox`.
Servers that do not advertise it reject the field with a typed
`feature_required` error.

## Request Shape

`session/open.params.sandbox` is optional:

```json
{
  "session_id": "coding:local:gamma",
  "profile_id": "coding",
  "sandbox": {
    "enabled": true,
    "network_access": false,
    "read_allow_paths": ["/repo/docs"]
  }
}
```

Fields:

- `enabled`: optional boolean. `false` is rejected when the inherited profile
  sandbox is enabled.
- `network_access`: optional boolean. `true` is rejected when the inherited
  profile sandbox denies network.
- `read_allow_paths`: optional list of existing paths. When the inherited
  profile has a read allowlist, every requested path must be equal to or under
  one inherited allowlist root. When the inherited list is empty, a non-empty
  requested list narrows the backward-compatible "allow reads" default.

## Validation

The effective policy is derived in this order:

1. Profile `SandboxConfig`.
2. Effective permission profile for the session.
3. Validated `session/open.params.sandbox` narrowing.

Any request that cannot be proven to narrow the derived profile policy fails
closed with `permission_denied` and `kind:
session_sandbox_would_widen_profile`.

## Non-Production Proof

The local validation path is:

```bash
bash scripts/validate-session-policy-local.sh
```

It runs focused protocol and runtime tests, including a two-session same-profile
runtime case where `gamma` narrows network access while `delta` inherits the
profile default.
