//! Git-backed workspace snapshots for undoing agent file mutations
//! (issue #1768, core mechanism).
//!
//! [`SnapshotManager`] records the state of a workspace directory into a
//! **separate** git directory (`<data_dir>/snapshots/<workspace-hash>`)
//! driven via `git --git-dir <snapshot-git-dir> --work-tree <workspace>`.
//! The user's own repository — `.git`, index, HEAD, hooks, config — is
//! **never** read from or written to, even when the workspace itself is a
//! git checkout: every invocation pins `--git-dir` explicitly, scrubs all
//! `GIT_*` redirection variables from the environment, and neutralises
//! global/system git config so behaviour never depends on the user's
//! personal git setup (`commit.gpgsign`, hooks, global excludes, ...).
//!
//! Snapshots are parentless commits addressed by refs under
//! `refs/octos/snapshots/<sortable-name>`. Because there is no parent
//! chain, pruning old snapshots is a ref deletion (no history rewrite),
//! and unchanged content is deduplicated by git's content-addressed
//! object store. `.gitignore` files inside the workspace are respected,
//! so build artifacts are neither recorded nor touched on restore.
//!
//! # Wall-clock cost
//!
//! `take_snapshot` costs roughly one `git add -A` plus object writes for
//! changed files:
//!
//! * The **first** snapshot of a workspace hashes every non-ignored file.
//!   On a large tree (multi-GB, hundreds of thousands of files) this can
//!   take seconds to minutes — comparable to `git add -A` in a fresh
//!   clone.
//! * **Subsequent** snapshots reuse the snapshot repo's index stat-cache
//!   and only re-hash files whose stat info changed; on a typical project
//!   this is tens of milliseconds.
//!
//! Because the cost scales with workspace size, the feature is **opt-in
//! and default OFF** ([`SnapshotConfig::enabled`]); hosts call
//! [`SnapshotManager::take_snapshot_async`] so the work runs on a
//! blocking thread instead of stalling the agent loop.
//!
//! # Known limitations
//!
//! * Two processes snapshotting the same workspace + data dir contend on
//!   the snapshot repo's `index.lock`; the loser's snapshot fails (the
//!   agent logs and continues — a failed snapshot never blocks a tool).
//! * Only file content/paths are restored. File modes are preserved by
//!   git's usual executable-bit handling; other metadata (mtimes,
//!   xattrs) is not.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default number of snapshots retained per workspace (prune policy).
pub const DEFAULT_SNAPSHOT_KEEP_LAST: usize = 20;

/// Ref namespace holding one ref per retained snapshot.
const SNAPSHOT_REF_PREFIX: &str = "refs/octos/snapshots";

/// Ref namespace for the temporary pin a restore places on its target so
/// the pre-restore snapshot's prune can never reclaim the commit that is
/// about to be checked out. Outside [`SNAPSHOT_REF_PREFIX`], so pins are
/// never listed or counted against `keep_last`.
const RESTORE_PIN_REF_PREFIX: &str = "refs/octos/restore-pin";

/// Ref recording the commit the most recent restore checked out — the known
/// state the workspace was left in. The next restore's conflict guard reads
/// it to detect files edited after that point (T-0346 follow-up). Outside
/// the snapshot namespace, so it is never listed or pruned as an undo point.
const LAST_RESTORE_REF: &str = "refs/octos/last-restore";

/// T-0467: a restore refused because files were edited after the snapshot.
/// Carries the workspace-RELATIVE conflicting paths so the RPC layer can attach
/// a structured `reason`/`paths` (no absolute paths or credentials) while the
/// Display text stays identical to the pre-existing message (the file names
/// stay in the client-visible message, per T-0434).
#[derive(Debug)]
pub struct SnapshotEditConflict {
    pub paths: Vec<String>,
}

impl std::fmt::Display for SnapshotEditConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "these files changed after the last snapshot/restore and a restore would \
             overwrite your edits: {}. Re-snapshot or re-read them first.",
            self.paths.join(", ")
        )
    }
}

impl std::error::Error for SnapshotEditConflict {}

/// Default object-reclaim grace for `git prune`. NOT `now`: two processes
/// snapshotting the same workspace race commit-tree/update-ref against
/// each other's prune, and an immediate expiry could reclaim the other
/// process's just-written (still unreachable) commit — leaving a dangling
/// ref that poisons dedup. A grace period spares freshly written objects;
/// unreachable objects are still reclaimed by a later prune once older
/// than this.
const DEFAULT_PRUNE_EXPIRE: &str = "1.hour.ago";

/// Maximum bytes of a snapshot label kept in the commit message.
const MAX_LABEL_BYTES: usize = 200;

/// Tool names whose execution can mutate workspace files. A snapshot is
/// taken before a tool batch containing any of these (when the feature is
/// enabled). Plugin/MCP tools are intentionally not classified here —
/// their side effects are unknown, and snapshotting before every unknown
/// tool would defeat the opt-in cost model.
pub const MUTATING_TOOLS: &[&str] = &[
    // group:fs write tools
    "write_file",
    "edit_file",
    "apply_patch",
    "diff_edit",
    // group:runtime — shell can write anywhere in the workspace
    "shell",
    "exec_command",
    "write_stdin",
    "bash",
    // Sub-agent dispatch tools: spawned workers run fs/shell tools in the
    // parent's workspace but do NOT inherit the snapshot manager (a
    // shared manager would make N concurrent children contend on the
    // snapshot index and churn the keep-last window with per-batch
    // snapshots). Classifying the dispatch tools as mutating instead
    // records ONE pre-spawn undo point that covers everything the
    // delegated subtree goes on to do.
    "spawn",
    "spawn_agent",
    "delegate",
    "delegate_task",
];

/// Whether `name` is a built-in tool that can mutate workspace files.
pub fn is_mutating_tool(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// Opt-in configuration for workspace snapshots (config key `snapshots`).
///
/// Default is **disabled**: snapshotting costs a `git add -A` per
/// mutating tool batch (see module docs), so users must opt in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotConfig {
    /// Master switch. `false` (default) = no snapshots are ever taken.
    pub enabled: bool,
    /// Retain at most this many snapshots per workspace (default 20).
    /// Values below 1 are clamped to 1.
    pub keep_last: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_last: DEFAULT_SNAPSHOT_KEEP_LAST,
        }
    }
}

/// Identifier of one snapshot — the full hex hash of its commit in the
/// snapshot repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Wrap a user-supplied id (e.g. from a future `octos snapshot
    /// restore <id>` command). Validated against the object store when
    /// used.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry from [`SnapshotManager::list_snapshots`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: SnapshotId,
    /// The label passed to [`SnapshotManager::take_snapshot`].
    pub label: String,
    /// Creation time, seconds since the Unix epoch.
    pub timestamp_unix: i64,
}

/// Stable 16-hex-char hash of a workspace path, used as the snapshot
/// git-dir name so distinct workspaces sharing one data dir never mix.
pub fn workspace_hash(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Process-wide cached `git` binary discovery (`which`/`where` semantics
/// via the `which` crate, which honours `PATHEXT` on Windows).
static GIT_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
/// Ensures the "git missing" warning is logged exactly once per process.
static MISSING_GIT_LOGGED: OnceLock<()> = OnceLock::new();

fn discover_git() -> Option<PathBuf> {
    GIT_BINARY.get_or_init(|| which::which("git").ok()).clone()
}

/// Platform null device, used to neutralise global/system git config.
fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// `GIT_*` environment variables that could redirect our commands at the
/// user's repository (or elsewhere). All are scrubbed before every git
/// invocation; `--git-dir`/`--work-tree` are always passed explicitly.
const SCRUBBED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_TEMPLATE_DIR",
    "GIT_INDEX_VERSION",
];

/// Git-backed snapshot store for one workspace. See module docs.
pub struct SnapshotManager {
    /// Absolute path of the `git` binary.
    git: PathBuf,
    /// Parent directory holding all per-workspace snapshot repos
    /// (conventionally `<data_dir>/snapshots`).
    snapshots_root: PathBuf,
    /// This workspace's dedicated git dir:
    /// `<snapshots_root>/<workspace_hash>`.
    git_dir: PathBuf,
    /// Canonicalized workspace root; scope of every snapshot.
    workspace: PathBuf,
    /// Prune policy: retain at most this many snapshots (>= 1).
    keep_last: usize,
    /// `git prune --expire=<this>` grace period (see
    /// [`DEFAULT_PRUNE_EXPIRE`]; tests shorten it to `now` to assert
    /// reclaim deterministically).
    prune_expire: String,
    /// Per-process tiebreaker making ref names unique within one
    /// millisecond.
    seq: AtomicU64,
    /// One-shot latch for the nested-git-repository (gitlink) advisory —
    /// warn once per manager, not once per snapshot.
    gitlink_warned: AtomicBool,
}

impl SnapshotManager {
    /// Create a manager rooted at `snapshots_root` (conventionally
    /// `<data_dir>/snapshots`) for `workspace`.
    ///
    /// Returns `None` when no `git` binary is on `PATH` — the feature is
    /// then silently unavailable (a warning is logged once per process).
    pub fn new(
        snapshots_root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        keep_last: usize,
    ) -> Option<Self> {
        Self::new_with_discovery(discover_git(), snapshots_root, workspace, keep_last)
    }

    /// Injection point for git discovery (tested directly; `new` passes
    /// the process-wide cached discovery result).
    pub(crate) fn new_with_discovery(
        git: Option<PathBuf>,
        snapshots_root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        keep_last: usize,
    ) -> Option<Self> {
        match git {
            Some(git) => Some(Self::with_git_binary(
                git,
                snapshots_root,
                workspace,
                keep_last,
            )),
            None => {
                MISSING_GIT_LOGGED.get_or_init(|| {
                    tracing::warn!(
                        "git binary not found on PATH; workspace snapshots are unavailable"
                    );
                });
                None
            }
        }
    }

    /// Construct with an explicit git binary (bypasses discovery).
    pub fn with_git_binary(
        git: PathBuf,
        snapshots_root: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        keep_last: usize,
    ) -> Self {
        let snapshots_root = snapshots_root.into();
        let workspace: PathBuf = workspace.into();
        // Canonicalize so the workspace hash (and therefore the git dir)
        // is stable regardless of how the caller spelled the path. A
        // not-yet-existing workspace keeps the literal path; git commands
        // will fail with a clear error later.
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        let git_dir = snapshots_root.join(workspace_hash(&workspace));
        Self {
            git,
            snapshots_root,
            git_dir,
            workspace,
            keep_last: keep_last.max(1),
            prune_expire: DEFAULT_PRUNE_EXPIRE.to_string(),
            seq: AtomicU64::new(0),
            gitlink_warned: AtomicBool::new(false),
        }
    }

    /// The workspace this manager snapshots.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The dedicated git dir backing this manager (never the user's
    /// `.git`).
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Record the current workspace state and return its id.
    ///
    /// Respects `.gitignore` files inside the workspace. If nothing
    /// changed since the most recent snapshot, no new snapshot is created
    /// and the existing id is returned. Prunes to the `keep_last` newest
    /// snapshots afterwards.
    ///
    /// Blocking (spawns git subprocesses); from async code use
    /// [`Self::take_snapshot_async`].
    pub fn take_snapshot(&self, label: &str) -> Result<SnapshotId> {
        self.ensure_repo()?;
        let label = Self::sanitize_label(label);

        // Stage the whole workspace (honours .gitignore; the user's `.git`
        // dir is never tracked by git). The snapshot repo's index
        // stat-cache makes repeat calls incremental.
        self.run_git(&["add", "-A"])
            .wrap_err("failed to stage workspace state for snapshot")?;
        let tree = self.run_git(&["write-tree"])?.trim().to_string();

        // Nested git repositories are staged as gitlinks (mode 160000):
        // their FILES are absent from the snapshot and restore cannot
        // recover them. Covering them would mean writing into their own
        // `.git` — which this module promises never to do — so surface the
        // hole loudly (once per manager) instead of leaving it silent.
        if !self.gitlink_warned.swap(true, Ordering::Relaxed) {
            let nested: Vec<String> = self
                .run_git(&["ls-tree", "-r", &tree])
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("160000 "))
                .filter_map(|line| line.split('\t').nth(1).map(str::to_string))
                .collect();
            if !nested.is_empty() {
                tracing::warn!(
                    nested_repos = %nested.join(", "),
                    "nested git repositories are NOT covered by workspace snapshots; \
                     files under them cannot be restored"
                );
            }
        }

        // Dedup: identical tree to the newest snapshot → reuse it instead
        // of minting an empty-diff snapshot (which would erode the
        // keep-last window with no-ops). Dedup is an optimisation only: a
        // cross-process prune race can leave the newest ref dangling
        // (its commit object reclaimed), and failing here would wedge
        // every subsequent snapshot — and, via the pre-restore snapshot,
        // every restore — so an unreadable newest snapshot just skips
        // dedup.
        if let Some(newest) = self.newest_snapshot_commit()? {
            // Turn-scoped undo points are identified by their LABEL
            // (`pre-turn <turn_id>`), not by the tree: a chat-only turn and
            // the file-writing turn after it can share a tree, yet each must
            // keep its own row or the turn would have no undo point
            // (T-0434). Dedup only when the newest snapshot also carries the
            // SAME label, or the label is not turn-scoped at all (the no-op
            // case the keep-last window is protected from).
            let newest_label = self
                .run_git(&["log", "-1", "--format=%s", &newest])
                .map(|subject| subject.trim().to_string())
                .unwrap_or_default();
            let same_label = newest_label == label;
            match self.run_git(&["rev-parse", &format!("{newest}^{{tree}}")]) {
                Ok(newest_tree)
                    if newest_tree.trim() == tree
                        && (same_label || !label.starts_with("pre-turn ")) =>
                {
                    return Ok(SnapshotId(newest));
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(error = %err,
                        "newest snapshot ref is unreadable (dangling?); skipping dedup");
                }
            }
        }

        // Parentless commit: prune stays a ref deletion (no history
        // rewrite) and ids stay stable.
        let commit = self
            .run_git(&["commit-tree", "-m", &label, &tree])?
            .trim()
            .to_string();
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let short = &commit[..commit.len().min(8)];
        // Zero-padded millis + per-process seq make the ref name
        // lexicographically chronological, so `--sort=-refname` is
        // newest-first even for sub-second bursts.
        let ref_name = format!("{SNAPSHOT_REF_PREFIX}/{millis:016}-{seq:06}-{short}");
        self.run_git(&["update-ref", &ref_name, &commit])?;

        if let Err(err) = self.prune() {
            tracing::warn!(error = %err, "snapshot prune failed");
        }
        // A new turn invalidates the "workspace still matches the last
        // restore" assumption the conflict guard relies on: changes the agent
        // makes from here are its own work, not user edits to protect
        // (T-0434). Clearing the marker keeps undoing THIS turn possible
        // while a redo straight after an undo (no new turn) still refuses to
        // clobber genuine user edits. Only pre-turn labels disarm — the
        // pre-restore snapshot taken by a restore must stay armed.
        if label.starts_with("pre-turn ") {
            self.run_git(&["update-ref", "-d", LAST_RESTORE_REF])
                .wrap_err("failed to reset last-restore marker for the new turn")?;
        }
        Ok(SnapshotId(commit))
    }

    /// [`Self::take_snapshot`] on a blocking worker thread, so the agent
    /// loop never stalls on git.
    pub async fn take_snapshot_async(
        self: &Arc<Self>,
        label: impl Into<String>,
    ) -> Result<SnapshotId> {
        let manager = Arc::clone(self);
        let label = label.into();
        tokio::task::spawn_blocking(move || manager.take_snapshot(&label))
            .await
            .map_err(|err| eyre!("snapshot task join error: {err}"))?
    }

    /// Restore the workspace to the state recorded in `id`.
    ///
    /// * Files modified or deleted since the snapshot get their snapshot
    ///   content back.
    /// * Non-ignored files created since the snapshot are removed.
    /// * Ignored files (build artifacts, ...) are never touched.
    ///
    /// A `pre-restore` snapshot of the current state is taken first, so a
    /// restore is itself undoable. The restore target is pinned (a ref
    /// outside the snapshot namespace) while the restore runs, so the
    /// pre-restore snapshot's prune can never delete the very snapshot
    /// being restored — even when the store is at `keep_last` capacity
    /// and the target is the oldest snapshot.
    pub fn restore(&self, id: &SnapshotId) -> Result<()> {
        if !self.git_dir.join("HEAD").exists() {
            bail!("no snapshots have been taken for this workspace");
        }
        // Snapshot ids are (possibly abbreviated) commit hashes. Reject
        // anything else BEFORE handing the string to git so a crafted id
        // can never be parsed as a flag or revspec (`--force`, `HEAD`,
        // ranges, ...).
        let raw = id.as_str();
        if raw.len() < 4 || raw.len() > 64 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid snapshot id {raw:?}: expected a hex commit hash");
        }
        let full = self
            .run_git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{raw}^{{commit}}"),
            ])
            .map_err(|_| eyre!("snapshot {raw} not found"))?
            .trim()
            .to_string();

        // Pin the target before the pre-restore snapshot: that snapshot's
        // prune would otherwise push the ref count over `keep_last` and
        // could delete the target's ref AND reclaim its commit object
        // mid-restore (guaranteed with `keep_last == 1`). The pin ref is a
        // reachability root for `git prune`, and `prune()` spares listed
        // snapshot refs whose commit is pinned, so the target stays listed
        // and restorable afterwards.
        let pin_ref = format!("{RESTORE_PIN_REF_PREFIX}/{full}");
        self.run_git(&["update-ref", &pin_ref, &full])
            .wrap_err("failed to pin restore target")?;
        let result = self.restore_pinned(&full);
        // Best effort: a pin leaked by a crash mid-restore only exempts
        // that one snapshot from pruning (never unbounded growth — pins
        // are per-commit and overwritten by the next restore of the same
        // id).
        if let Err(err) = self.run_git(&["update-ref", "-d", &pin_ref]) {
            tracing::debug!(error = %err, "failed to remove restore pin ref");
        }
        result
    }

    /// [`Self::restore`] after id validation, running with `full` pinned.
    fn restore_pinned(&self, full: &str) -> Result<()> {
        // Record the current state first so the restore is itself
        // undoable. This also stages the current tree (`add -A`), which
        // the created-file diff below relies on.
        let pre_restore = self
            .take_snapshot("pre-restore")
            .wrap_err("failed to record pre-restore snapshot")?;

        // T-0346 follow-up: refuse to clobber a file the user edited AFTER
        // the pre-restore point. A path the restore would overwrite whose
        // current bytes differ from BOTH the restore target and the
        // pre-restore snapshot was changed externally in that window —
        // overwriting it would silently destroy the user's edit. Detect and
        // fail honestly BEFORE touching the work tree, naming the paths.
        // (Redo — restoring the pre-restore point itself — has no such
        // conflict, because current == pre-restore there.)
        self.refuse_overwrite_of_later_edits(full, &pre_restore)
            .wrap_err("restore refused: files were edited after the snapshot was taken")?;

        // Files in the index (current state) but absent from the snapshot
        // were created after it and must be removed to reach the snapshot
        // state. Ignored files are not in the index, so they are never
        // listed here.
        let created_raw = self.run_git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=A",
            "-z",
            full,
        ])?;

        // Write snapshot content over the work tree (restores modified
        // AND deleted files). Skipped for an empty snapshot tree, where
        // the pathspec would match nothing and git would error.
        let has_content = !self
            .run_git(&["ls-tree", "--name-only", full])?
            .trim()
            .is_empty();
        if has_content {
            self.run_git(&["checkout", "-f", full, "--", "."])
                .wrap_err("failed to check out snapshot content")?;
        }

        let mut created_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
        let mut failed_removals: Vec<String> = Vec::new();
        for rel in created_raw.split('\0').filter(|p| !p.is_empty()) {
            let path = Path::new(rel);
            // git emits workspace-relative paths; refuse anything absolute
            // or traversing (defence in depth against a tampered snapshot
            // repo).
            let suspicious = path.is_absolute()
                || path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)));
            if suspicious {
                tracing::warn!(
                    path = rel,
                    "skipping suspicious path during snapshot restore"
                );
                continue;
            }
            match std::fs::remove_file(self.workspace.join(path)) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(path = rel, error = %err,
                        "failed to remove file created after the snapshot");
                    // T-0464: a file that could not be removed means the
                    // workspace is NOT back at the snapshot state — the restore
                    // must fail honestly (naming the file) instead of
                    // reporting success and leaving it behind.
                    failed_removals.push(rel.to_string());
                }
            }
            // Remember the parent chain: removing the created files can
            // leave freshly created directory shells behind.
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir.as_os_str().is_empty() {
                    break;
                }
                created_dirs.insert(dir.to_path_buf());
                parent = dir.parent();
            }
        }
        // Sweep now-empty directories deepest-first so the restored
        // workspace matches the snapshot's shape (git cannot represent an
        // empty directory, so anything the removals emptied did not exist
        // in the snapshot tree). `remove_dir` is non-recursive: a directory
        // that still holds (e.g. ignored) content simply refuses, which is
        // exactly the conservative behavior wanted here.
        for dir in created_dirs.iter().rev() {
            let _ = std::fs::remove_dir(self.workspace.join(dir));
        }
        // T-0464: only record the restore as complete when every created
        // file was actually removed. A leftover means the workspace is NOT at
        // the snapshot state — fail honestly (naming the files, matching the
        // T-0434 conflict-error shape) and do NOT advance the last-restore
        // marker, so a retry is possible after the workspace is writable.
        if !failed_removals.is_empty() {
            bail!(
                "restore incomplete: could not remove files created after the snapshot: {}.                  The workspace is only partially restored; re-check permissions and retry.",
                failed_removals.join(", ")
            );
        }
        // Record the state this restore left the workspace in, so the NEXT
        // restore can detect external edits made after it (T-0346).
        self.run_git(&["update-ref", LAST_RESTORE_REF, full])
            .wrap_err("failed to record last-restore marker")?;
        Ok(())
    }

    /// T-0346 follow-up: refuse to overwrite files the user edited after the
    /// previous restore left them in a known state. `LAST_RESTORE_REF` points
    /// at the commit the last restore checked out; for every path this
    /// restore would change (target `full` vs the just-taken pre-restore
    /// `current`), the current worktree bytes must still match that known
    /// state — otherwise the content is foreign (typed after the undo) and
    /// overwriting it would silently destroy the user's edit. No prior
    /// restore means no such window, so the first undo is unguarded.
    fn refuse_overwrite_of_later_edits(&self, full: &str, current: &SnapshotId) -> Result<()> {
        let prev = self
            .run_git(&["rev-parse", "--verify", "--quiet", LAST_RESTORE_REF])
            .unwrap_or_default()
            .trim()
            .to_string();
        if prev.is_empty() {
            return Ok(());
        }
        // Paths this restore would change.
        let changed = self.run_git(&["diff", "--name-only", "-z", full, &current.0])?;
        let mut conflicts: Vec<String> = Vec::new();
        for rel in changed.split('\0').filter(|p| !p.is_empty()) {
            let path = Path::new(rel);
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                continue;
            }
            let abs = self.workspace.join(path);
            let current_blob = if abs.is_file() {
                self.run_git(&["hash-object", "--", rel])?
                    .trim()
                    .to_string()
            } else {
                String::new()
            };
            let known_blob = self
                .run_git(&["rev-parse", &format!("{prev}:{rel}")])
                .unwrap_or_default()
                .trim()
                .to_string();
            if current_blob != known_blob {
                conflicts.push(rel.to_string());
            }
        }
        if !conflicts.is_empty() {
            return Err(SnapshotEditConflict { paths: conflicts }.into());
        }
        Ok(())
    }

    /// List retained snapshots, newest first.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        if !self.git_dir.join("HEAD").exists() {
            return Ok(Vec::new());
        }
        let out = self.run_git(&[
            "for-each-ref",
            "--sort=-refname",
            "--format=%(objectname)%00%(creatordate:unix)%00%(subject)",
            SNAPSHOT_REF_PREFIX,
        ])?;
        let mut snapshots = Vec::new();
        for line in out.lines() {
            let mut parts = line.splitn(3, '\0');
            let (Some(id), Some(ts), Some(label)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            snapshots.push(SnapshotInfo {
                id: SnapshotId(id.to_string()),
                label: label.to_string(),
                timestamp_unix: ts.trim().parse().unwrap_or(0),
            });
        }
        Ok(snapshots)
    }

    // ------------------------------------------------------------------
    // internals
    // ------------------------------------------------------------------

    /// Base git invocation: explicit `--git-dir`/`--work-tree`, cwd at the
    /// workspace root, `GIT_*` redirection env scrubbed, global/system
    /// config neutralised, deterministic identity, hooks disabled.
    fn git_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.git);
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.workspace)
            .args(["-c", "user.name=octos-snapshot"])
            .args(["-c", "user.email=snapshot@octos.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "core.autocrlf=false"])
            .args(["-c", "gc.auto=0"])
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                self.git_dir.join("_no_hooks").display()
            ))
            .arg("-c")
            .arg(format!("core.excludesFile={}", null_device()))
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        for var in SCRUBBED_GIT_ENV {
            cmd.env_remove(var);
        }
        cmd
    }

    /// Run git, returning stdout on success and a stderr-carrying error on
    /// failure.
    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = self
            .git_command(args)
            .output()
            .wrap_err_with(|| format!("failed to spawn git {:?}", args.first().unwrap_or(&"")))?;
        if !output.status.success() {
            bail!(
                "git {} failed ({}): {}",
                args.first().unwrap_or(&""),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Initialise the snapshot repo on first use and keep the snapshot
    /// storage itself out of snapshots when it lives inside the
    /// workspace.
    fn ensure_repo(&self) -> Result<()> {
        if !self.git_dir.join("HEAD").exists() {
            std::fs::create_dir_all(&self.git_dir).wrap_err_with(|| {
                format!("failed to create snapshot dir {}", self.git_dir.display())
            })?;
            self.run_git(&["init", "--quiet"])
                .wrap_err("failed to initialise snapshot repository")?;
        }
        self.write_self_exclude();
        Ok(())
    }

    /// If the snapshots root is inside the workspace (e.g. workspace ==
    /// home and data dir under it), exclude it via the snapshot repo's
    /// own `info/exclude` so snapshots never recursively ingest snapshot
    /// storage. The user's repo is untouched — this file lives in OUR git
    /// dir.
    fn write_self_exclude(&self) {
        let root = match self.snapshots_root.canonicalize() {
            Ok(root) => root,
            Err(_) => self.snapshots_root.clone(),
        };
        let Ok(rel) = root.strip_prefix(&self.workspace) else {
            return;
        };
        if rel.as_os_str().is_empty() {
            tracing::warn!(
                workspace = %self.workspace.display(),
                "snapshots root equals the workspace; snapshots of it are skipped"
            );
            return;
        }
        let pattern: String = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let exclude_dir = self.git_dir.join("info");
        if let Err(err) = std::fs::create_dir_all(&exclude_dir)
            .and_then(|_| std::fs::write(exclude_dir.join("exclude"), format!("/{pattern}/\n")))
        {
            tracing::warn!(error = %err, "failed to write snapshot self-exclude");
        }
    }

    /// Newest snapshot ref's commit hash, if any snapshot exists.
    fn newest_snapshot_commit(&self) -> Result<Option<String>> {
        let out = self.run_git(&[
            "for-each-ref",
            "--count=1",
            "--sort=-refname",
            "--format=%(objectname)",
            SNAPSHOT_REF_PREFIX,
        ])?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Delete refs beyond the newest `keep_last` and reclaim their
    /// objects (best effort). Snapshots pinned by an in-flight
    /// [`Self::restore`] are spared: deleting the restore target's ref
    /// (and reclaiming its commit) mid-restore would destroy the very
    /// state being restored.
    fn prune(&self) -> Result<usize> {
        let out = self.run_git(&[
            "for-each-ref",
            "--sort=-refname",
            "--format=%(refname)%00%(objectname)",
            SNAPSHOT_REF_PREFIX,
        ])?;
        let refs: Vec<(&str, &str)> = out
            .lines()
            .filter_map(|line| line.split_once('\0'))
            .collect();
        if refs.len() <= self.keep_last {
            return Ok(0);
        }
        let pinned = self.pinned_commits()?;
        let mut deleted = 0usize;
        for (stale_ref, commit) in &refs[self.keep_last..] {
            if pinned.contains(*commit) {
                continue;
            }
            self.run_git(&["update-ref", "-d", stale_ref])?;
            deleted += 1;
        }
        // Reclaim the now-unreachable objects so storage is actually
        // bounded, not just hidden behind deleted refs. Best effort — a
        // failed reclaim leaves stale objects, never corruption. Objects
        // referenced by the snapshot index (staged current state) or a
        // restore pin are reachability roots for `git prune` and survive.
        // The expiry grace (see [`DEFAULT_PRUNE_EXPIRE`]) spares objects a
        // concurrent process has written but not yet ref'd.
        if let Err(err) = self.run_git(&["prune", &format!("--expire={}", self.prune_expire)]) {
            tracing::debug!(error = %err, "git prune failed; objects reclaimed on a later prune");
        }
        Ok(deleted)
    }

    /// Commits currently pinned by an in-flight restore (usually empty).
    fn pinned_commits(&self) -> Result<std::collections::HashSet<String>> {
        let out = self.run_git(&[
            "for-each-ref",
            "--format=%(objectname)",
            RESTORE_PIN_REF_PREFIX,
        ])?;
        Ok(out
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Single printable line, capped at [`MAX_LABEL_BYTES`], never empty.
    fn sanitize_label(label: &str) -> String {
        let cleaned: String = label
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            return "snapshot".to_string();
        }
        octos_core::truncated_utf8(cleaned, MAX_LABEL_BYTES, "…")
    }
}
