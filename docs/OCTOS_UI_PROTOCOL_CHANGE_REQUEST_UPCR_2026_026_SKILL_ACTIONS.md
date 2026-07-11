# UPCR-2026-026: Manifest-Declared Skill Actions

Status: accepted
Date: 2026-07-09
PR: TBD

## Summary

Add a generic AppUI surface for session-scoped, manifest-declared skill actions:
`skill/action/list` and `skill/action/invoke`, advertised through
`skill.actions.v1`.

This is not a NotebookLM-specific API and not a generic client-side tool-call
escape hatch. The backend only exposes actions that installed skill manifests
declare, and invocation always uses the manifest-owned backend binding.

## Decision

Do add `actions[]` to skill manifests. Each action declares:

- `id`, `label`, optional `description`
- semantic `tags[]` and optional `surfaces[]` filters
- action-level `input_schema`
- opaque `ui_schema` hints for clients
- a backend `binding`

Do support a `tool` binding that points at an existing registered skill tool.
The client may choose an action and provide action arguments, but it cannot
override the tool name or binding mode.

Do support two input modes:

- `single`: forward the action argument object once
- `file_each`: require `arguments.paths[]`, prepare each path, and invoke the
  bound tool once per file, inserting the prepared path into `file_argument`
  (default `path`)

Do make file preparation explicit and skill-owned through
`file_materialization`:

- `raw` (default): forward path strings unchanged
- `workspace_relative`: copy owned upload references into
  `<workspace>/uploads/` and pass workspace-relative paths, including images
- `turn_media`: use existing chat-turn media semantics, where non-images become
  workspace-relative paths and images use the vision-readable upload path

Do gate capability advertisement behind an available profile store. Skill
actions require a profile-backed session runtime because tool availability and
plugin directories are profile-scoped.

Do NOT add `/api/notebook/*` routes or a NotebookLM-only import protocol. The
Notebook-like source import flow is implemented by the `mofa-notebook-source`
skill declaring a `source.import` action bound to its existing `source_import`
tool.

Do NOT let AppUI clients invoke arbitrary model-visible tools directly. Actions
are the skill author's exported UI affordances, not a client-selected tool
execution API.

## Capabilities

Feature token:

- `skill.actions.v1`

Methods:

- `skill/action/list`
- `skill/action/invoke`

Both methods are server-handled `APPUI_EXTRA_METHODS`. They are omitted from
profile-skill capability sets when the server has no profile store.

## AppUI Surface

### `skill/action/list`

Request:

- `session_id` — required
- `profile_id` — optional profile override
- `surface` — optional UI surface filter
- `tags[]` — optional required tag filter

Response:

- `profile_id`, `session_id`, `count`
- `actions[]`, each containing `id`, `skill_id`, `label`, `execution`,
  optional `description`, `tags[]`, `surfaces[]`, `input_schema`,
  `ui_schema`, and `available`

`skill_dir` is server-only runtime state and is never included in the AppUI
response.

Actions bound to tools unavailable in the session runtime are filtered out.

### `skill/action/invoke`

Request:

- `session_id` — required
- `profile_id` — optional profile override
- `action_id` — action id or `skill_id/action_id`
- `arguments` — optional JSON object

Response:

- `action_id`
- `ok`
- `results[]`, each containing `success`, `output`, `file_modified`,
  `artifacts[]`, and `structured_metadata`; file fields use opaque
  session-workspace handles rather than raw host paths
- `materialized_paths[]` for `file_each` actions

## Compatibility

Backward-compatible. The change adds methods, a capability feature, and optional
manifest fields. Existing skills without `actions[]` continue to load and expose
their model-visible tools exactly as before.

`file_materialization` defaults to `raw` so existing or future file actions do
not inherit chat-media behavior implicitly. Skills that require
workspace-relative paths must opt in with `workspace_relative`.

## Tests

- `crates/octos-agent/src/plugins/manifest.rs` parses manifest `actions[]`,
  tool bindings, `file_each`, and `file_materialization`.
- `crates/octos-cli/src/api/ui_protocol.rs` covers discovery, capability
  advertisement, action invocation, `file_each` materialization, and the
  default `raw` behavior.
- `crates/octos-bus/src/file_handle.rs` covers copying upload images into
  workspace-relative `uploads/` paths for explicit `workspace_relative`
  materialization.

## References

- v1 spec § 4.1 (change control), § 6 (catalog), and runtime/profile method
  semantics.
- `crates/octos-cli/src/api/ui_protocol.rs::APPUI_EXTRA_METHODS`.
- `crates/octos-agent/src/plugins/manifest.rs::SkillActionDef`.
