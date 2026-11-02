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
   use `#[serde(skip)]`. After building the preview, the transport clears both
   fields before appending or broadcasting the event, so live and recovered
   ledger notices are identical. The agent executor and skill-action result
   projection strip the corresponding keys from structured tool metadata.
   Neither the old content nor the create flag is added to the OUP wire,
   in-memory ledger, or durable tool metadata.
3. Outside a git workspace, the preview renderer uses only this authoritative
   metadata:
   - confirmed create: current text versus empty, producing all additions;
   - overwrite/edit: previous text versus current text;
   - delete: previous text versus empty, producing all removals.
   The old/new text is compared with `git diff --no-index` through temporary
   files; no repository or index is required. Its headers use Git C-style
   quoted paths, decoded before prefix removal and display sanitization, so
   spaces in filenames or workspace roots cannot split the old/new paths.
4. Confirmed creates map to the existing `create` file-mutation operation.
   Existing git-worktree and untracked-file diff generation is unchanged.

## Limits and security

- Old and new text are each capped at 256 KiB. Binary (NUL-containing),
  non-UTF-8, unreadable, or oversized content produces no non-git fallback hunk.
- `write_file` and `apply_patch` capture previous content as raw bytes from one
  descriptor, with a bounded read even if the file grows after its size check.
  `edit_file` and `diff_edit` reuse the edit's own read; transformed input is
  not an original-file baseline. PDF baselines are excluded even when their
  bytes are UTF-8, and extracted PDF text is never used as previous content.
  This exclusion concerns baselines, not newly written text-only PDF source.
- A `write_file` guarded by `WritePathGrant` omits overwrite baselines instead
  of reopening its lexical path outside the confined writer. Fenced edits may
  retain text from their existing confined read/write handle; fenced creates
  retain their normal all-additions behavior.
- Direct non-git reads require canonical containment within the session
  workspace (or process cwd when no session workspace is available). Deleted
  files require a missing leaf under a canonical in-workspace parent; dangling
  symlinks are not deletions. Existing unrenderable files never become removal
  previews just because their current text cannot be read.
- A post-write filesystem check alone never establishes that a file was created.
  Missing previous content for an overwrite/delete yields no fallback rather
  than a misleading all-additions diff.
- Non-git paths containing control characters retain the existing sanitized
  mutation-path display but omit fallback hunks. Raw path labels must not be
  interpolated into unified headers where they could forge files or hunks.
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
- Live versus cold ledger replay, skill-action metadata exclusion, absolute and
  parent traversal, symlink escapes, and symlinked workspace regressions.
- Existing git untracked-file and proposal-time snapshot regressions.
- Actual writer-to-preview PDF/binary delete and overwrite negatives, decoded
  and fenced PDF edits, `diff_edit` on ASCII and binary-comment PDFs, and a
  deterministic ancestor-swap baseline regression.
- Control-character paths preserve one path-only preview for non-git creates,
  modifications and deletions; real `diff/preview/get` returns no forged hunk.
- Spaces in file paths and workspace roots preserve the actual sanitized
  path and a single-file hunk; ordinary text `diff_edit` retains local changes
  without exposing distant baseline lines.
- Full affected `write_file`, `edit_file`, `diff_edit`, and `apply_patch` suites.
- Real isolated `octos serve` + octoscode-web: agent `write_file` in a non-git
  workspace, Review changes shows `+2 −0` and both created lines.
