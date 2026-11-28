# Upgrading to Octos v2.1.0

This release includes the changes from v2.0.3 through the v2.1.0 release
baseline `cfab32f0944ecac1897af1346b9fcd19aad343d8`. Read the sections that
apply to existing serve installations before upgrading.

## Swap the binary (and how to roll back)

An upgrade replaces the running bundle with a newer one; it does not move
your data. The RC (`v2.1.0-rc.1`) is a **preview** that predates the v2.1.0
release baseline `cfab32f09` and its Chat-workspace layout, so for the final
upgrade use the published v2.1.0 tag (or the tag of the release you are
upgrading to). The bundle carries `octos`, `octos-sandbox`, tool binaries
and `model_catalog.json` — swap the whole set, never just `octos`, or you
end up with a mixed-version installation. Recommended order:

```bash
set -e                               # stop on any failed download or verification
TAG=v2.1.0                           # the release you are upgrading to (not the RC preview)
TARGET=x86_64-unknown-linux-gnu      # your platform (see the release page)
PREFIX=~/.octos/bin                  # install prefix from the original install
STAGE=~/.octos/bundles/${TAG}        # staged new bundle, its own directory

# The set -e above stops the recipe on any failed step; unset variables
# would be caught by adding -u — both guards are deliberate.

# 1. Download the new bundle and verify it against its SHA-256 sidecar.
#    set -e above stops the whole recipe if either curl or sha256sum fails.
mkdir -p "${STAGE}/payload"
cd "${STAGE}"
curl -fSLO "https://github.com/octos-org/octos/releases/download/${TAG}/octos-bundle-${TARGET}.tar.gz"
curl -fSLO "https://github.com/octos-org/octos/releases/download/${TAG}/octos-bundle-${TARGET}.tar.gz.sha256"
sha256sum -c "octos-bundle-${TARGET}.tar.gz.sha256"
tar xzf "octos-bundle-${TARGET}.tar.gz" -C "${STAGE}/payload"

# 2. Stop the service (skip if you run the binary directly).
sudo systemctl stop octos-serve      # macOS: sudo launchctl unload /Library/LaunchDaemons/io.octos.serve.plist

# 3. Back up exactly the files this bundle replaces (a fresh dated
#    backup per upgrade), then install the payload over the prefix.
#    Other programs in the prefix (install.sh, octos-doctor.sh,
#    unrelated binaries) stay in place.
BACKUP="${PREFIX}.backup.$(date +%Y%m%d%H%M%S)"
mkdir -p "${BACKUP}"
for f in "${STAGE}/payload/"*; do
  [ -f "$f" ] || continue
  name=$(basename "$f")
  [ -f "${PREFIX}/${name}" ] && cp -a "${PREFIX}/${name}" "${BACKUP}/"
done
cp -a "${STAGE}/payload/." "${PREFIX}/"

# 4. Restart and check health.
sudo systemctl start octos-serve     # macOS: sudo launchctl load /Library/LaunchDaemons/io.octos.serve.plist
~/.octos/bin/octos serve --version   # or: bash octos-doctor.sh from the release
```

Rollback restores the files that upgrade replaced, from the exact dated
backup it created, and restarts — data directories are not rewritten by the
swap, so no data rollback is needed:

```bash
# A fresh shell does not have these set — the commands below must abort
# rather than touch the filesystem root if either is empty.
: "${BACKUP:?set BACKUP to the dated backup dir printed by the upgrade}"
: "${PREFIX:?set PREFIX to your install prefix}"
# The backup must exist and hold files BEFORE the service stops, and the
# service restarts only when the copy fully succeeded — a mistyped path or
# partial copy never leaves a mixed-version install running.
[ -d "${BACKUP}" ] && [ -n "$(ls -A "${BACKUP}")" ]   || { echo "backup ${BACKUP} is missing or empty" >&2; exit 1; }
sudo systemctl stop octos-serve && cp -a "${BACKUP}/." "${PREFIX}/" && sudo systemctl start octos-serve
```

**Chat-workspace constraint before you roll back:** Chats created by the new
binary live under `<state home>/chat-workspaces/…`, which older binaries do
not discover (see the next section). Rolling back to a pre-2.1.0 binary is
safe for configuration and sessions, but those new Chats stay invisible to
the old server — they reappear after upgrading again.

If you also run a web client: match it to the runtime's companion release.
The octos-web SPA embedded at `/app/` is deprecated; the supported browser
client is [Octoscode Web](https://github.com/octos-org/octoscode-web), whose
getting-started guide lists the compatible runtime versions.

## Server Chat workspaces move out of legacy `chats`

Runtime-created Profiles now keep server Chat workspaces under
`<state home>/chat-workspaces/<profile-hash>/<chat-hash>/` (normally beneath
`~/.octos/chat-workspaces`), not the pre-release `<state home>/chats` location
(#3256, #3326, #3356).

- Existing legacy `<profile data_dir>/chats` content is **not migrated**.
  Lookup still checks both roots. A legacy root is reserved only when it has
  real pre-upgrade content; an empty directory left by an older startup does
  not make a fresh profile legacy, so new Chats stay in the server root across
  restarts. A symlink or non-directory entry remains fail-closed evidence
  (#3372).
- A development instance that already ran the pre-release #3326 layout may
  have a server root at `<state home>/chats`. Startup moves that root to
  `chat-workspaces` only when it is not also a startup profile's legacy root.
- A legacy `_main` profile whose `data_dir` is exactly the state home is
  supported. Startup no longer rejects that layout merely because the server
  root is a sibling of the profile's data.

Startup still refuses, rather than guessing or merging data, when the old or
new server root is a symlink, both roots contain data, the migration source
contains a configured profile data directory, the server root overlaps a
legacy `<data_dir>/chats` root, or a Code workspace is **at or below** the
server root. The error names the conflicting paths. Resolve the reported
layout before restarting.

**Code workspace choice in this release:** `session/open` refuses a folder
that is **at or below** the server Chat root, or that **contains** it — on a
standard install that makes `$HOME` (which contains the state tree) refused
with `-32120 chat_workspace_reserved`. For a legacy `<data_dir>/chats` root,
only a containing folder at or below `data_dir` is refused, so a project
that holds a custom profile data directory while the server root lives
elsewhere remains a valid workspace. Choose a project folder that does not
contain the server Chat root; on the current client the first-run wizard
still offers the home folder, so select a project subfolder instead — a
client that no longer offers the home folder directly and shows a
folder-swap recovery for the refusal ships with the next Octoscode Web
release. Widening the classification to allow ancestors above the state
home — with the reserved roots inside them staying hidden — is tracked as a
post-2.1.0 change.

The session/open reservation refuses, with `chat_workspace_reserved`, a
Code workspace that is **at or below** the server Chat root, **inside** the
state tree (`~/.octos`, `~/.octos/profiles/<id>`), or that **contains** the
state home (`$HOME` on a standard install): a containing folder holds
profile state (transcripts, credentials, memory) that the hidden-folder
containment does not mask. Choose a project folder outside the state tree;
the first-run wizard guides the choice. Admitting ancestors above the state
home is blocked on a per-workspace containment design (post-2.1.0).

Rollback leaves files in the new root intact, but older binaries cannot
discover Chats stored there. Do not roll back with the expectation that the
old server will see those new Chats. The exact compatibility contract is in
[UPCR-2026-R1EFC56A3](OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_R1EFC56A3_CHAT_WORKSPACE.md).

## Hidden Chat roots remain fail-closed

The server Chat root is precomputed and hidden before commands start (#3326,
#3356, #3360, #3377).

- Only a missing server-owned root **still known to be empty** may be
  recreated privately, and only when none of the current command's workspace
  or read roots can reach it. The recreation is logged as a warning. A command
  root that overlaps the server root keeps the command refused.
- Once the server root can contain a Chat, its directory identity is checked
  on every command wrap. A symlink, replacement directory, missing populated
  root, or missing/renamed **legacy** profile `chats` root keeps commands
  refused; a legacy absence is never taken as proof that its contents are gone.
- Backends that cannot hide a reserved folder beneath a granted root refuse
  overlapping commands. With sandboxing disabled or unavailable, every command
  refuses while any Chat root is reserved.

Docker can now hide Chat roots instead of rejecting the default new-install
Code workflow (#3377). A sibling data directory outside the container's mounts
is naturally invisible; a hidden root inside a mount is covered with a private
empty `tmpfs`. If a bind source is the hidden root or lies inside it, the
command still refuses because that bind would cover the mask. The default
bootstrap read grant no longer exposes the whole Octos home: each existing
shared skill directory is granted by its precise path. When neither directory
exists, the derived configuration grants only the existing default system
read paths and enables strict reads, so macOS SBPL covers the workspace, Git
metadata and those system paths—never a global `file-read*`—and the default
shell still works (#3382). Explicit operator `read_allow_paths` are not
rewritten. Landlock, AppContainer and no-sandbox refusal semantics are
unchanged. Refusal text now distinguishes a **read path** that contains a
hidden root from the command workspace itself and names the path to narrow.

On Windows, Docker mount sources produced by canonicalization are normalized
from verbatim drive/UNC forms (`\\?\C:\…`, `\\?\UNC\server\share\…`) to the
plain forms Docker Desktop accepts. The workspace cwd, `extra_binds` and cache
slot all validate and mount the normalized source while hidden-root checks
keep the original host path; bind destinations and read-only modes are
preserved. Device paths, drive-relative or malformed UNC paths, extra colons
and control characters still fail closed (#3385).

The refusal names the missing path and the available recovery action. Restore
the original directory, or have an operator verify deletion and unregister a
legacy root through the repair flow; do not bypass the hidden-folder check.

## Tasks and peer handoffs can declare inputs

Fleet tasks and peer interactions may declare the files needed by their first
model request (#3350, #3352, #3363, including the scoped-precheck follow-up
reviewed in #3362):

- Fleet: the `goal_plan` task field `inputs`.
- Peer: the `peer_handoff` and `peer_send_input` argument `inputs`.
- Each entry is a path, or `{path, kind, required, sha256}`. Omitting `inputs`
  preserves the previous behavior.

Inputs resolve with the **worker's** file authority, not the declarer's.
Symbolic links are not followed, hidden-folder and confined-read rules still
apply, and a declaration never widens access. Text and structured data are
sent whole; images are sent as image parts. Size checks reject oversized
inputs before a full read, and nothing is silently truncated.

Every input is reported as delivered or with a specific reason (for example
missing, denied, changed, over budget, unsupported, unreadable, invalid or
duplicate). A required miss ends a fleet attempt before a model call and
refuses a handoff or `peer_send_input` at staging/send time. If a previously
accepted peer input cannot be delivered when its later turn runs, that turn
still runs with an explicit not-delivered notice so it can report the problem;
nothing is silently skipped. The request that first introduces a required
image fails if the endpoint refuses images. After the model has answered that
request, a later image-incapable request may fall back to text and says the
image is absent. See
[UPCR-2026-RBC0874AA](OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_RBC0874AA_FIRST_TURN_TASK_INPUTS.md)
for the schema and delivery contract.

## Serve IM bridge: initial Telegram direct-chat core

`octos serve` accepts a read-only `serve.im` configuration for Telegram direct
chats (#3334, #3361, #3365, #3366). Each entry selects `token_env`,
`allowed_senders`, `ttl_secs` and explicit bindings from one Telegram chat and
sender to an existing `(profile, session)` Chat. Text messages run as turns of
that Chat and replies go back to the originating message. `/status` and
`/stop` are supported.

Each bot account has one consumer: serve takes the bot's owner lock before it
starts gateways or accepts requests, so the same bot cannot also be consumed
by a gateway. A Telegram gateway with a parseable bot token now takes the same
lock even when `serve.im` is absent; startup is refused if another process
owns that bot or the lock cannot be used. With the lock available, the
existing gateway behavior is unchanged. Invalid entries, an empty allowlist,
missing or non-Chat bindings, duplicate ownership and conflicting bindings
fail startup with the entry named. Configuration changes require a serve
restart.

This is an initial, deliberately narrow cut: attachments, group-chat control,
IM approval UI and a persistent redelivering outbox are not included. See the
[IM remote-control contract](IM_REMOTE_CONTROL.md) for exact binding and
delivery rules.

## Skill installation and consumer compatibility

Third-party install-time execution is now off by default (#3202). Remote Cargo
builds, npm dependency scripts and skill lifecycle commands require the
respective operator opt-ins `OCTOS_SKILLS_CARGO_BUILD=1`,
`OCTOS_SKILLS_NPM_INSTALL=1` and `OCTOS_SKILLS_LIFECYCLE=1`. Allowed lifecycle
commands require an OS sandbox rather than running unconfined. See the
user guide's “Installation process” section before installing source-based
skills.

The already-unused standalone `octos-pipeline` crate is removed (#3044).
Consumers must drop that dependency; this does not change the OUP wire
contract. [DOT runtime retirement](DOT_PIPELINE_RETIREMENT.md) remains a
separate change (#2691).

## Other compatibility notes

- `/app` (the embedded legacy octos-web client) is deprecated and displays a
  migration banner. Use octoscode-web; hashed asset responses are unchanged
  (#3323, RFC #3262).
- Agent, peer-staged and peer-closed events delivered to a profile-scoped OUP
  connection are filtered to that profile on both replay and live paths;
  unscoped stdio/unauthenticated connections retain their previous behavior
  (#3271, UPCR-2026-R8C3A1D2E).
- Windows `doctor` now searches `OCTOS_PREFIX`, both `HOME/.octos/bin` and
  `USERPROFILE/.octos/bin`, and `C:\octos\bin`, so a fresh PATH can still
  locate an installed binary and print the directory to add (#3322).
