# UPCR-2026-057: read a file from a session workspace

## Header

- Request id: `UPCR-2026-057`
- Date: 2026-10-29
- Status: proposed (implemented for review)
- Target protocol: `octos-ui/v1alpha1`
- Scope: additive typed command `session/files/read` and feature `session.files.read.v1`

## Problem and relationship to `session/files.list`

`session/files.list` names workspace files but does not authorize downloading their bytes. `/api/files` intentionally accepts only scoped upload/delivery handles, so an agent-created relative path such as `report.md` cannot be read by octoscode-web's artifact preview. This adds a small read-only RPC for a single explicitly named file. It does not add directory browsing, writes, a repository-wide filesystem API, or a second handle-issuance/revocation system.

## Contract

Params: `SessionFilesReadParams` — `{ session_id, path, offset?, length? }`.

- `session_id` selects an existing session workspace under the existing authenticated profile/session resolver.
- `path` is a workspace-relative file path. Absolute paths, backslashes, `..`, directories, non-regular files, and canonical/symlink escapes are rejected.
- `offset` defaults to 0. `length` defaults to and is clamped to 512 KiB (524,288 bytes); zero is invalid. `offset > size_bytes` is invalid. The client reassembles chunks; there is no streaming notification.

Result: `SessionFilesReadResult` — `{ session_id, path, size_bytes, offset, eof, mime_type, encoding, content_base64 }`.

- `size_bytes` is the complete file size; `offset` is the chunk's starting byte; `eof` says whether this chunk reaches the current end.
- `encoding` is the literal `base64`; `content_base64` contains only this chunk, so binary files do not need UTF-8.
- `mime_type` is a conservative extension mapping with `application/octet-stream` fallback.
- A file larger than 32 MiB (33,554,432 bytes) returns `workspace_file_too_large`; smaller files are readable in multiple calls. One response stays below the 1 MiB WebSocket text-frame limit.

The approved workspace root is canonicalized once (preserving legitimate symlinked ancestors such as macOS `/var`), then on Unix it and every user-controlled relative component are opened relative to a trusted directory descriptor with `O_NOFOLLOW`; non-Unix uses the checked-path fallback. File type and size are validated on the opened descriptor, not a prior pathname stat, preventing ancestor-swap and size TOCTOU races. Reads are bounded and occur on a blocking thread. Errors use typed `data.kind` values (`workspace_file_invalid_path`, `workspace_file_escape`, `workspace_file_not_found`, `workspace_file_not_regular`, `workspace_file_too_large`, `workspace_file_offset_out_of_range`, `workspace_file_invalid_length`, `session_workspace_not_found`, `workspace_file_access_denied`) and do not disclose absolute host paths.

## Capability and authorization

- The method is advertised only when `session.files.read.v1` is negotiated; otherwise it is `method_not_supported`.
- Solo and trusted host connections use the same profile and session-workspace ownership checks as the existing upload/delivery boundary (#1377). No profile-global `up/` handle or alternate host path is accepted.
- **Host-managed external access is a separate commit and requires reviewer confirmation.** When enabled, the external token may read only a session already opened successfully on that same connection. Other sessions return `external_session_not_opened`; other profiles and global handles remain inaccessible. This does not grant the external client any new host-filesystem capability beyond its established session workspace.
- Session-ingress scope validation continues to require the granted session id. Existing `session/files.list`, `/api/files`, upload ownership, and all write paths are unchanged.

## Validation

Required coverage includes text and binary bytes, chunk boundaries and `eof`, size/MIME metadata, the 512 KiB frame bound, 32 MiB total limit, absolute/parent/directory/symlink escapes, zero length and out-of-range offsets, missing workspaces, external sessions not opened by this connection, and capability advertisement.
