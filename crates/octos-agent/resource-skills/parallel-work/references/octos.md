# Mapping to Octos

Use the deployed runtime's advertised schemas; Octos versions differ. This file
describes capability checks, not a promise that every named tool or option exists.

For background native workers, supply the full initial assignment through the
available `spawn` tool. Where supported, pass the applicable `skill_paths` and
typed `owned_paths` relative to the actual spawn workspace. Supply executable
and input paths that resolve in that workspace. Preserve the user's model choice;
inherit it unless a different model is authorized. Confirm the worker really has
the needed read/write/edit/bash, image, or source-access capabilities.

Collect through the supported completion events or `check_background_tasks` and
`read_task_output`, using the actual returned ID rather than a display label.
For supported peer execution, use `peer_handoff`, then `peer_list`/`peer_gather`;
use `peer_send_input` only where supported. A peer is appropriate when an ongoing,
independently managed collaborator is useful, not as a speed upgrade by definition.
Worktree selection follows write ownership and repository constraints.

If iteration controls exist, honor the configured user/host budget. Versions that
define `max_iterations: 0` as unlimited can use it for the user's explicit no-limit
choice. This does not remove cancellation or time/cost limits. Worker concurrency
and external render/API concurrency are separate settings; do not copy a prior
deck's worker count or service slot count into unrelated work.

Typed claims, when enforced by the host, govern admission among cooperating
workers; they are not a security sandbox. A canceled label does not establish that
a writer stopped. Confirm execution termination before reusing a writable output.
If live steering is unavailable, do independent work and collect the original
result, or use the runtime's real cancellation protocol before a replacement.

Use exact job receipts for external work. If completion notifications are broken,
use bounded status checks, preserve evidence of the runtime failure, and avoid
duplicate submissions. A skill can describe the correct behavior but cannot fix
a missing runtime event or an adapter that drops image content.

Load this portable folder through the active deployment/profile's supported skill
path or explicit worker skill references. Do not assume a legacy HOME-rooted skill
directory is discovered, or claim a running session has loaded updated instructions
without checking. Do not change the skill files in a frozen experiment mid-run.
