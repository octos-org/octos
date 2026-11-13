# UPCR-2026-R1EFC56A3 — One server-created workspace per chat

Status: proposed (implemented for review)
PR: TBD

## Decision record

`UPCR-2026-R1EFC56A3`.

Octoscode Web's Chat mode (two-modes proposal, task T-B) gives every chat its
own folder. The chat reads and writes there and nowhere else. Two chats never
see each other's files, and reopening a chat finds the same folder.
`UPCR-2026-R6982BEFC` made a session's Chat / Code mode durable. This record
adds the folder: who creates it, how a session is bound to it, how the session
is kept inside it, and when it is deleted.

The folder and the fence are server state. A client cannot pick a chat's
folder, move a chat to another folder, or widen what a chat may do. A client
that did not negotiate the feature still opens a chat confined.

### Feature

`session.chat_workspace.v1` (`UI_PROTOCOL_FEATURE_SESSION_CHAT_WORKSPACE_V1`).
It is in the known-feature registry and is strictly opt-in:

- It is not part of the no-header `first_server_slice` baseline.
- It is not a stdio default.

A connection that does not request it sees byte-identical replies.

### Wire

- **`session/chat/create`** `{ session_id, profile_id }` creates a chat. The
  result is `{ session_id, profile_id, workspace_root, work_mode: "chat" }`.
  - Without the feature the method is `method_not_supported`, like other
    strictly gated methods. It is in `FIRST_SERVER_METHODS`, so a connection
    that negotiated the feature sees it in `supported_methods`.
  - `session_id` must be an API session of `profile_id`, with no topic:
    `<profile_id>:api:<chat>`. Anything else is `invalid_params`,
    `data.kind = "chat_session_id_invalid"`.
  - Profile scope is checked as for `session/open`. An unknown profile is
    refused.
  - An existing session that is not a chat is `invalid_params`,
    `data.kind = "session_exists"`. A session keeps the workspace it was
    created with.
  - Creating the same chat again returns the same folder. A retried create is
    harmless.
  - The session is recorded with `work_mode: "chat"`, as `UPCR-2026-R6982BEFC`
    defines.
  - `workspace_root` is the folder's canonical path.
  - The method is not available over stdio, like the other methods that need
    an authenticated profile.
- **`session/open`** of a chat always uses the chat's folder.
  - With no `cwd`, the chat opens in its folder.
  - With a `cwd` that is not the folder, the open fails with `invalid_params`,
    `data.kind = "chat_workspace_mismatch"` and `data.workspace_root`.
  - The feature is not needed to open a chat. Without it the chat is still
    confined. A client simply cannot create or list chats.
- **`session/open`** of any other session with a `cwd` inside a chat folder,
  whether the folder itself or below it, of any profile, fails with
  `permission_denied` and `data.kind = "chat_workspace_reserved"`.
- **`session/list { work_mode: "chat", profile_id }`** lists a profile's chats.
  - It requires the feature. Without it the request fails with
    `invalid_params`, `data.kind = "feature_required"`.
  - `profile_id` is the only other parameter. `cwd`, `workspace_id` and
    `cursor` are refused. The list is not paginated.
  - Profile scope works as for the cwd listing. An authenticated connection
    gets its own profile and cannot name another one.
  - Each row is a `session/list` row (`SessionInfo`) with `work_mode: "chat"`
    and the chat's `workspace_root`. Rows are newest first.
  - The result attests the scope with `work_mode: "chat"` and `profile_id`.
  - `work_mode: "code"` is refused. Code sessions are listed by cwd as before.
- **`permission/profile/set`** on a chat cannot ask for `danger_full_access` or
  `network: "allow"`. Either is refused with `permission_denied`,
  `data.kind = "chat_permission_bound"`.
  - A chat that has no explicit permission reads as `read_only` with
    `network: "deny"`.
  - The client may raise it to `workspace_write`, which writes the chat's
    folder only.
- **`session/files/read`** and `/api/files` resolve a chat's files in its
  folder, whether or not the chat is open.

### Folder and lifecycle

- A chat's folder is `<profile data dir>/chats/<name>/`, created with mode
  `0700`. `<name>` is the first 32 hex digits of SHA-256 of the session id.
  - Lookup is therefore a path computation. Every session bootstrap and
    permission read can ask "is this a chat?" cheaply.
  - A folder that is a symlink is refused.
- **A broken chat stays a chat.** The record decides whether a session is a
  chat. If the record exists but the folder is missing, a symlink or not a
  directory, or the record names another session, the chat is unusable, not
  ordinary:
  - `session/open` and `session/chat/create` fail with `permission_denied`,
    `data.kind = "chat_workspace_unavailable"`. The message carries no path;
    the server logs the cause.
  - Its session runtime refuses to start, so it never runs in another
    workspace or gets the full tool set back.
  - Its permission keeps the chat bound and the chat default.
  - `session/files/read` and `/api/files` serve none of its files.
  - It is left out of `session/list { work_mode: "chat" }`.
  - Deleting the session still deletes it: a symlink in the folder's place is
    removed without following it, then the record.
- The binding is a record beside the folder: `chats/<name>.json` with
  `{session_id, profile_id, created_at}`.
  - The record is outside the folder, so nothing the chat writes can change
    whose folder it is.
  - The record is on disk, so the binding survives restarts and reconnects.
- **Deletion policy.**
  - Deleting the session (`DELETE /api/sessions/{id}`, the web's delete)
    deletes the folder, everything in it, and then the record. There is no
    trash.
  - If the folder cannot be removed, the failure is logged and the record
    stays. The chat stays listed and the delete can be retried.
  - `/clear` and `/new` keep the folder and the chat's mode.
  - A folder is never deleted for any other reason. Nothing ages out.

### Fence

A chat's session runtime is built for the folder, whatever the caller asked
for:

- The workspace is the folder, even when the open passed another hint.
- No project overlay is read from it: no `.octos` settings, agents or plugin
  directories.
- The transcript stays in the profile's session store, never in
  `<folder>/.octos`. The same holds with `appui.sessions_in_cwd` on.
- The file scope is the folder with no shared zones. The profile's `research/`
  and `skills/` are not reachable. The profile's skill read zones (the
  `SKILL.md` references the prompt names) stay readable, as for every scoped
  session.
- A chat never runs unscoped. If the scope cannot be built, or the permission
  grants host file access, the bootstrap fails.
- Tools are cut to a fixed allow-list of compiled-in tools: file read, search,
  write and patch tools, `send_file`, `ask_user_question`, memory and recall
  tools, and the tool-dispatch gateway.
  - There is no shell, process, browser, web or network tool.
  - There is no spawn, delegation or peer tool.
  - No plugin or MCP tool is kept, even one that takes an allow-listed name.
  - Each turn applies the same cut to its finished registry, after the
    turn-scoped tools are registered.
- `send_file` delivers only files inside the folder. The managed uploads root
  and the profile directory are not deliverable from a chat.
- Turn uploads are materialized into `<folder>/uploads/`, as for every session
  workspace.

The file tools resolve every path through the scope:

- `..` is rejected.
- Paths are canonicalized and checked against the folder.
- Symlinks out of the folder, whether a file or an ancestor directory, are
  refused.

`session/files/read` walks relative components with `O_NOFOLLOW` and refuses
`..` and absolute paths.

### Compatibility

- Additive and opt-in. Existing sessions are never chats, and nothing about
  them changes.
- Old clients cannot create or list chats.
- An old server rejects `session/chat/create` as an unknown method.
- An old server ignores `work_mode` on `session/list` and returns its ordinary
  listing, which does not attest `work_mode: "chat"`. Clients gate on the
  feature and on that attestation.

### Not in this record

- MCP connectors and plugins inside a chat. They are dropped today.
- A size quota on a chat's folder.
- Pagination of the chat list.
- Moving a session between Chat and Code.
- The web client adding `session.work_mode.v1` and `session.chat_workspace.v1`
  to the features it requests. That is a web task.
