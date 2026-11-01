# UPCR-2026-067 — Session Execution Environment

Status: proposed (implemented for review)
Task: T-0085
Capability: `session.execution_environment.v1`

## Problem

A permission profile and a sandbox backend are different facts. Clients cannot
currently tell whether commands actually run under bwrap, the Landlock helper,
Docker, another native backend, or no isolation, nor whether common development
tools exist inside that execution environment.

## Read-only method

`session/execution_environment/read {session_id, profile_id?}` is gated by the
new feature and the existing session/profile scope rules. It returns:

- `backend`: `bwrap`, `landlock`, `docker`, `macos`, `appcontainer`, `none`,
  `unavailable`, or `unknown`;
- `docker_image`: the selected image for Docker, otherwise `null`;
- `tools.node`, `tools.npm`, `tools.git`, and `tools.python`: `available`,
  `missing`, or `unknown`;
- `probe_status`: `complete`, `failed`, or `unsupported`, `probed_at` in RFC
  3339 UTC, and an optional bounded `probe_error` category. `unsupported`
  reports a backend without a compatible probe shell, keeps every tool
  `unknown`, and omits `probe_error`; its timestamp records that determination.

`runtime_policy_stamp.sandbox_mode` remains a permission preset and is not used
as the backend.

## Probe and cache

The first read for a materialized session runtime with a POSIX shell executes
one bounded, fixed-command `command -v` probe through that runtime's actual
`Sandbox` implementation and workspace. This includes Docker on Windows. A
backend without a POSIX shell, including Windows `NoSandbox` and AppContainer,
returns `unsupported` without executing a command. The probe runs no project
code, package manager, installer, privilege escalation, or network operation.
Missing executables are ordinary results. Spawn failure, refusal, timeout, or
malformed probe output returns `failed` and marks every tool `unknown` rather
than guessing availability.

The complete result, including failed/unsupported outcomes and its timestamp, is cached in a
`tokio::sync::OnceCell` on that session runtime. Subsequent reads do not probe.
Existing runtime invalidation on permission/profile/environment changes drops
the cache and prevents results crossing sessions or effective sandboxes.

No mutation, new mount, network permission, Docker socket access, credential,
or arbitrary command parameter is introduced. Host-managed external clients do
not receive this method unless a separate ACL change explicitly allows it.
