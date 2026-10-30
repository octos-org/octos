# UPCR-2026-063 — Non-git file-mutation diff previews

- Status: proposed (implemented for review)
- Issue: #2885
- PR: #2914

## Problem

`diff/preview/get` stored an empty preview for a newly created file when the
session workspace was not inside a git repository. `materialize_file_mutation_diff`
returned early when no git root existed, so octoscode-web displayed `+0 −0` and
could not show the file contents. A fallback must not treat every post-write file
as newly created: ordinary overwrites and deletes need an authoritative baseline.

## Change

1. Built-in file writers capture bounded process-local metadata before mutation:
   - `file_created`: whether the writer observed a new path;
   - `previous_content`: bounded UTF-8 text before an overwrite, edit, move, or
     delete, when available under the existing preview limits.
2. The agent progress path carries that metadata to the OUP progress mapper.
   `UiFileMutationNotice.created` and `.previous_content` are internal-only and
   use `#[serde(skip)]`. The agent executor strips the corresponding keys from
   structured tool metadata after emitting the progress event. Neither the old
   content nor the create flag is added to the OUP wire, ledger, or durable tool
   metadata.
3. Outside a git workspace, the preview renderer uses only this authoritative
   metadata:
   - confirmed create: current text versus empty, producing all additions;
   - overwrite/edit: previous text versus current text;
   - delete: previous text versus empty, producing all removals.
   The old/new text is compared with `git diff --no-index` through temporary
   files; no repository or index is required.
4. Confirmed creates map to the existing `create` file-mutation operation.
   Existing git-worktree and untracked-file behavior is unchanged.

## Limits and security

- Old and new text are each capped at 256 KiB. Binary (NUL-containing),
  non-UTF-8, unreadable, or oversized content produces no non-git fallback hunk.
- A post-write filesystem check alone never establishes that a file was created.
  Missing previous content for an overwrite/delete yields no fallback rather
  than a misleading all-additions diff.
- No client-supplied command, shell interpolation, repository config, or index
  mutation is introduced. The no-index invocation uses a fixed argv and two
  server-owned temporary files.
- Managed `slides/` and `sites/` auto-commit flows remain out of scope; their
  pre-snapshot behavior is unchanged.

## Compatibility

There is no new RPC method, feature token, wire field, or persisted wire schema.
Existing clients receive the same `diff/preview/get` shape, now with hunks for
covered non-git mutations. Clients that do not negotiate any new capability see
no behavior change beyond the corrected preview content.

## Verification

- Non-git create, modify, delete, binary, oversized, and internal-metadata serde
  regressions.
- Existing git untracked-file and proposal-time snapshot regressions.
- Full affected `write_file`, `edit_file`, `diff_edit`, and `apply_patch` suites.
- Real isolated `octos serve` + octoscode-web: agent `write_file` in a non-git
  workspace, Review changes shows `+2 −0` and both created lines.
