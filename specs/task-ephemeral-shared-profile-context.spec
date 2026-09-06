spec: task
name: "Ephemeral chat retains shared profile context"
tags: [chat, oup, memory, runtime, persistence]
---

## Contract

`chat --no-session-persistence` must not replace the profile's memory,
episode-recall source, tool configuration, skills, or scoped shared tools with
an empty temporary profile. Bootstrap those resources once at the real profile
root. Use the existing companion `open_or_degraded` episode policy when the
canonical store is locked; never copy a live database. Disable automatic
episode saving for this frontend.

Only the local OUP/session root is temporary: canonical conversation JSONL,
context/task sidecars and interactive input history follow that root. The
optional `ProfileRuntime.session_store_root` is set before creating sessions,
preserved across plugin rebuild, and has precedence over per-cwd storage both
in the session resolver and in the runtime-cache key resolver. Ordinary
Serve/Gateway/ACP runtime assembly retains its existing unset default.

This flag is not read-only mode. Explicit memory/file/cron/goal operations keep
their normal shared-state behavior. Workspace permissions remain authoritative.

## Tests

- `should_preserve_shared_profile_context_in_ephemeral_oup_turn`: a real
  in-process OUP turn with a recording provider receives actual shared memory,
  tool-config and profile-skill context. Its canonical transcript exists only
  under the temporary root, no episode is saved, and explicit per-cwd storage
  plus raw-hint cache resolution cannot move it into the workspace.
- `should_degrade_only_ephemeral_episode_store_on_shared_lock_contention`:
  a canonical redb owner blocks ordinary local bootstrap but ephemeral chat
  continues with degraded episode access and intact shared text memory, not
  a fresh temporary database.
- `should_keep_default_local_session_storage_unchanged`: ordinary local/ACP
  assembly has no override and keeps the historical profile/cwd precedence.

No second ContextManager, model loop, transcript copy, or live DB copying.
