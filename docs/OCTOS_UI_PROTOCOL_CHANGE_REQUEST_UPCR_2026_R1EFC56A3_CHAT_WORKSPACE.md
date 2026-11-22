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
is kept inside it, and when it is deleted. Profiles present when a server
starts keep the original per-profile folder; a Profile registered while the
server runs uses a server-level chats root fixed before the first sandbox is
built, so no running command can meet an unreserved future chat root.

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
  - `workspace_root` is the folder's canonical path. Clients use this value
    and never derive it: the folder may be in the profile's legacy `chats/`
    root or in the server-level root for a runtime-created profile.
  - The method is not available over stdio, like the other methods that need
    an authenticated profile.
- **`session/open`** of a chat always uses the chat's folder.
  - With no `cwd`, the chat opens in its folder.
  - With a `cwd` that is not the folder, the open fails with `invalid_params`,
    `data.kind = "chat_workspace_mismatch"` and `data.workspace_root`.
  - The feature is not needed to open a chat. Without it the chat is still
    confined. A client simply cannot create or list chats.
- **`session/open`** of any other session with a `cwd` that reaches into a
  chat folder of any profile fails with `permission_denied` and
  `data.kind = "chat_workspace_reserved"`. For a legacy profile root that is a
  chat folder itself, a folder below one, or a folder inside the profile's data
  directory that holds them: the data directory or its `chats` folder. A folder
  above the legacy data directory is not refused by that rule, because the data
  directory may sit in a project (`--data-dir`, `OCTOS_HOME`) or in the user's
  home, and refusing those would refuse the project or the home itself. The
  server-level root follows the legacy ancestor rule: startup still refuses any
  overlap with a profile data directory, and it refuses a Code workspace at or
  below the root, but it allows a larger ancestor workspace such as a standard
  install's `$HOME`; the root itself is in the precomputed hidden set. Such a
  session still never reaches a chat folder of any profile. One workspace may hold several profiles' data directories (a project
  above them), so the reserved folders are the legacy `chats` folders of every
  profile the server stores or has started, plus the server-level root, not only
  the session's own. The server-level root exists before any sandbox is built,
  so a Profile registered while the server runs is already covered. Legacy
  profiles are reserved by the folder their data directory resolves to, however
  the profile's `data_dir` is spelled; a profile whose folder cannot be reserved
  cannot create a chat:
  - its file tools: its session scope hides those folders, so a path there,
    by name or through a symlink, is out of scope, and directory walks skip
    them. `diff_edit`, which resolves paths itself, refuses a path the scope
    hides;
  - its shell and every other command it runs (exec tools, checks,
    validators, subagents, goal monitors): the sandbox hides those folders.
    bwrap covers each one inside a folder it binds with an empty tmpfs, after
    binding each folder between a writable working folder and it over itself,
    so a command cannot rename one to move a reserved folder out from under
    the next command. macOS denies reading and writing them, and writing the
    folders between the working folder and them. Landlock, Docker and
    AppContainer cannot hide a folder inside one they grant, so there a
    command whose working folder (or a configured read path) holds one
    refuses to run. Without a sandbox (`sandbox.enabled = false`,
    `mode = "none"`, or `auto` finding no backend) a command can read any
    path the server can for as long as it runs, and a chat may be made
    while it runs, so every command refuses while any profile's `chats`
    folder is reserved, empty or not: while this server can make chats. A
    profile's `chats` folder is made when the profile is reserved, so a
    sandbox hides it before its first chat exists and for the whole run of
    a command started before that chat;
  - workspace snapshots: those folders are left out by git pathspec
    exclusions, which no ignore file in the workspace can override, and
    whatever an earlier snapshot staged from them leaves the snapshot index.
    A restore never writes them back or deletes anything in them, even from
    a snapshot taken before they were hidden;
  - `session/files/read`: a file there fails with `permission_denied` and
    `data.kind = "chat_workspace_reserved"`;
  - `session/files.list`: those folders are not listed, and a `prefix`
    inside one fails the same way;
  - `send_file`: nothing in those folders is delivered, by absolute path,
    profile handle (`pf/…`) or workspace-relative path, even though such a
    session may otherwise deliver from its profile's data directory.
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
- **`/api/files`** (`GET /api/files`, `GET /api/files/{filename}`,
  `POST /api/files/mutate`) acts on a file in any of those reserved `chats`
  folders only for a request whose `session` is the chat that owns it, as
  bound now. It
  checks whichever way the file was named: profile handle (`pf/…`), absolute
  path or a path in the session's workspace. A request that names no session,
  another session, or a chat whose folder is unusable is refused (`403`, or
  `404` for a mutation). `GET /api/files/list` never lists a reserved
  `chats` folder or anything in it.

### Folder and lifecycle

- A chat's folder is either `<profile data dir>/chats/<name>/` for a profile
  already present when the server starts, or
  `<shared server data root>/chats/<profile>/<name>/` for a Profile registered
  while the server runs. `<profile>` and `<name>` are SHA-256-derived path
  components; the server-level root is created before the first sandbox. Every
  chat folder is created with mode `0700`.
  `<name>` is the first 32 hex digits of SHA-256 of the session id without its
  topic, in ASCII lowercase where the storage folds case.
  - Lookup is therefore a path computation. Every session bootstrap and
    permission read can ask "is this a chat?" cheaply.
  - Every spelling of an id that reaches the chat's transcript finds the
    chat's record: the `default` topic (`<id>#default`, or `topic:
    "default"`), which the session store files as the bare id, and, on
    storage that folds case (`CHATS` names the `chats` folder, as on most
    macOS and Windows volumes), an ASCII case variant. Only the chat's own id
    is the chat; any other spelling is refused like a broken chat
    (`chat_workspace_unavailable` at `session/open`, no runtime for a turn),
    never run as an ordinary session. There, a chat is not created over a
    case variant's existing transcript (`session_exists`).
  - Where storage tells case apart, a case variant is a session of its own,
    including one that existed before the chat. Another topic
    (`<id>#notes`) is always a transcript of its own: an ordinary session,
    whose delete leaves the chat's folder and record alone.
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
  - Deleting the session still deletes it. A broken chat first gets a
    fresh, empty, private folder (a symlink in its place is removed, never
    followed), so it can be opened as a fenced chat for the delete. Then
    the folder, the transcript and the record are removed, in that order.
    If the folder cannot be removed, the delete fails and nothing else is
    removed. If a later step fails, the record is still there. Either way the
    session stays a fenced chat, and deleting it again retries.
- **A cached session runtime is re-checked on every reuse.** A runtime whose
  chat binding no longer matches is rebuilt:
  - one cached before its session became a chat (`session/chat/create`
    resolves the session store first) is rebuilt as a fenced chat;
  - one whose folder was removed or replaced since is rebuilt, and the
    rebuild refuses, so a turn on it ends with an error.
- **Tools stay pinned to the folder the session was admitted with.** The
  session runtime keeps the path the folder resolved to when the binding was
  checked. A chat's file tools and its `send_file` hold that path, including
  a `send_file` a turn builds later. If the folder resolves anywhere else,
  for example after it is replaced by a symlink before or during a turn,
  they refuse. They do not follow the replacement.
- The binding is a record beside the folder: `chats/<name>.json` in the
  legacy root, or `<profile>/<name>.json` under the server-level root, with
  `{session_id, profile_id, created_at}`.
  - The record is outside the folder, so nothing the chat writes can change
    whose folder it is.
  - The record is on disk, so the binding survives restarts and reconnects.
  - Lookup checks the legacy and server-level roots. If the same session
    resolves in both, the server fails closed instead of choosing one.
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
- Existing profiles keep their legacy chats root; no folders or records are
  migrated. A rollback to an older binary cannot see chats stored under the
  server-level root, but those folders and records remain on disk and become
  visible again after upgrading.
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

### Test-only follow-up (T-0655)

Chat workspace tests unregister hidden roots belonging only to their own
TempDir when the fixture ends, and test helpers prune fixture roots that an
earlier test already deleted. This changes no protocol, product, or production
runtime behavior.
