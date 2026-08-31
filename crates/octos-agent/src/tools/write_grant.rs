//! #1976 — per-path WRITE-grant enforcement for the native file tools.
//!
//! A [`WritePathGrant`] is the octos-agent realisation of a fleet task's
//! `fs: { write: [...], create_only }` grant (`octos_fleet::WorkerGrant::
//! write_paths` — this crate deliberately has no `octos-fleet` dependency, so
//! the host-side mapping lives in `octos-fleet-worker::closed_registry`,
//! mirroring how `FsGrant` maps to `EffectivePermissions`). Bound to
//! `write_file` / `edit_file` via their `with_write_grant` builders, it makes
//! the fence kernel-side: a write outside the allowlist is a typed refusal,
//! not a polite request the model may ignore.
//!
//! # Matching contract (must stay aligned with the sandbox translation)
//!
//! Patterns are workspace-relative with `*` / `?` wildcards confined to ONE
//! path segment (`literal_separator`); `**`, `[...]`, `{...}` are rejected —
//! the v1 syntax is deliberately the intersection that this globset matcher
//! and the macOS SBPL regex translation
//! (`crate::sandbox::macos` `write_allow_globs`) express IDENTICALLY, so the
//! tool layer and the shell sandbox can never disagree about what is granted.
//! The same rules are validated fleet-side at plan time
//! (`octos_fleet::validate_write_path_pattern`); re-validating here is
//! defense-in-depth for programmatic constructions.
//!
//! # Symlink safety (security round — ancestor-swap TOCTOU)
//!
//! The allowlist decision and the actual open MUST target the same resolved
//! object. An earlier design canonicalized the path for the CHECK but the
//! tools then opened the LEXICAL path — so an attacker who swapped a checked
//! real directory (`cards/`) for a symlink between check and open escaped
//! (leaf `O_NOFOLLOW` guards only the leaf, never ancestors). The fix:
//! [`check_write`](WritePathGrant::check_write) does a purely LEXICAL
//! allowlist match and returns the workspace-relative path, and the write goes
//! through [`open_confined`] — a component-wise `O_NOFOLLOW` `openat` walk from
//! the workspace-root dirfd. A symlinked (or swapped-to-symlink) ancestor
//! fails `ELOOP`/`ENOTDIR` at its OWN component, so the opened object is
//! provably the one the allowlist matched. `create_only` is `O_CREAT|O_EXCL`
//! on that same walked leaf; a fenced edit reads and rewrites ONE handle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// The marker every write-grant refusal (and ledger finding derived from one)
/// carries — the `[denied]` class from issue #1976. Violations must never be
/// silent: the model sees this in the tool error, the host can sink it into
/// the goal ledger.
pub const DENIED_MARKER: &str = "[denied]";

/// One recorded write-grant violation, as handed to a
/// [`WriteGrantViolationSink`].
#[derive(Debug, Clone)]
pub struct WriteGrantViolation {
    /// The workspace root the fenced tool was bound to (for fleet workers
    /// this is the attempt cwd `<workspace_root>/<fleet>/<task>`, which is
    /// how the host maps a violation back to its task).
    pub workspace: PathBuf,
    /// The refusing tool (`write_file` / `edit_file`).
    pub tool: String,
    /// The full `[denied] ...` refusal message returned to the model.
    pub detail: String,
}

/// Host-supplied sink invoked ONCE per violation, at refusal time (not
/// post-hoc), so a violation is durable even if the attempt later times out.
/// Must be cheap/non-blocking — the fleet host wraps its ledger write in a
/// detached task.
pub type WriteGrantViolationSink = Arc<dyn Fn(WriteGrantViolation) + Send + Sync>;

/// A compiled per-path write fence. See the module docs for the contract.
#[derive(Clone)]
pub struct WritePathGrant {
    /// The original patterns, for refusal messages (what IS writable).
    patterns: Vec<String>,
    /// Compiled matcher (`literal_separator`: `*`/`?` never cross `/`).
    matcher: GlobSet,
    /// Allowlisted paths may be CREATED but never overwritten/edited.
    create_only: bool,
    /// Optional host sink for the `[denied]` audit trail.
    sink: Option<WriteGrantViolationSink>,
}

impl std::fmt::Debug for WritePathGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WritePathGrant")
            .field("patterns", &self.patterns)
            .field("create_only", &self.create_only)
            .field("sink", &self.sink.as_ref().map(|_| "..."))
            .finish()
    }
}

impl WritePathGrant {
    /// Compile a fence from workspace-relative patterns. Fails (so the caller
    /// can fail CLOSED — no registry, no worker) on any pattern outside the
    /// v1 syntax: absolute, `.`/`..` segments, empty segments, `**`, glob
    /// classes/alternations, profile metacharacters, control bytes, `:`.
    pub fn new(patterns: &[String], create_only: bool) -> eyre::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        let mut kept = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let pattern = pattern.trim();
            validate_pattern(pattern)?;
            let glob = GlobBuilder::new(pattern)
                // `*` / `?` stay within one path segment — the property the
                // SBPL regex translation mirrors ([^/]* / [^/]).
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    eyre::eyre!("write grant pattern `{pattern}` failed to compile: {e}")
                })?;
            builder.add(glob);
            kept.push(pattern.to_owned());
        }
        let matcher = builder
            .build()
            .map_err(|e| eyre::eyre!("write grant failed to compile: {e}"))?;
        Ok(Self {
            patterns: kept,
            matcher,
            create_only,
            sink: None,
        })
    }

    /// Attach the host's violation sink (`[denied]` audit trail).
    pub fn with_violation_sink(mut self, sink: WriteGrantViolationSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Whether allowlisted paths may only be CREATED (never overwritten /
    /// edited / deleted).
    pub fn create_only(&self) -> bool {
        self.create_only
    }

    /// The original allowlist patterns (for messages / profile generation).
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// Decide `write_file` against the allowlist and return the
    /// workspace-RELATIVE path to write (the input for the confined openat
    /// walk). `Ok(rel)` = allowed → the caller MUST open it via
    /// [`confined_write`] (which re-derives the SAME object symlink-safely,
    /// so the allowlist decision and the actual open never diverge — the
    /// security-round TOCTOU fix). `Err(msg)` = typed `[denied]` refusal,
    /// already recorded to the sink.
    ///
    /// The match is purely LEXICAL here (no canonicalization): ancestor
    /// symlink safety is enforced at OPEN time by the component-wise
    /// `O_NOFOLLOW` walk, not by a check-time canonicalize that a later open
    /// could contradict.
    pub fn check_write(
        &self,
        workspace_root: &Path,
        resolved: &Path,
        display_path: &str,
        tool: &str,
    ) -> Result<PathBuf, String> {
        let Some(rel) = workspace_relative(workspace_root, resolved) else {
            return Err(self.record(
                workspace_root,
                tool,
                format!("`{display_path}` is not inside the workspace."),
            ));
        };
        if self.matcher.is_match(&rel) {
            Ok(rel)
        } else {
            Err(self.record(
                workspace_root,
                tool,
                format!(
                    "`{display_path}` is outside this task's write grant \
                     (writable: {}). Everything else is read-only.",
                    self.patterns_for_message(),
                ),
            ))
        }
    }

    /// Decide `edit_file`. Under `create_only` EVERY edit is refused —
    /// allowlisted or not (issue #1976: created, never modified); otherwise
    /// the allowlist applies exactly as for writes, returning the
    /// workspace-relative path the caller opens via [`confined_open_rdwr`].
    pub fn check_edit(
        &self,
        workspace_root: &Path,
        resolved: &Path,
        display_path: &str,
        tool: &str,
    ) -> Result<PathBuf, String> {
        if self.create_only {
            return Err(self.record(
                workspace_root,
                tool,
                format!(
                    "this task's write grant is create-only — `{display_path}` (and every \
                     other file) may not be edited; allowlisted paths may only be CREATED \
                     (writable: {}).",
                    self.patterns_for_message(),
                ),
            ));
        }
        self.check_write(workspace_root, resolved, display_path, tool)
    }

    /// The typed refusal for a create-only overwrite (`O_CREAT|O_EXCL` came
    /// back `AlreadyExists`), recorded to the sink like every violation.
    pub fn deny_overwrite(&self, workspace_root: &Path, display_path: &str, tool: &str) -> String {
        self.record(
            workspace_root,
            tool,
            format!(
                "create-only write grant — `{display_path}` already exists and may not be \
                 overwritten."
            ),
        )
    }

    /// Map a confined-open/rewrite `io::Error` into a typed `[denied]` (or a
    /// plain I/O failure), recording the security-relevant cases to the sink.
    /// `ELOOP` / `ENOTDIR` from the `O_NOFOLLOW` walk means a symlinked
    /// ancestor (the ancestor-swap TOCTOU) — refuse loudly. `AlreadyExists`
    /// under `create_only` is the overwrite refusal.
    pub fn map_confined_error(
        &self,
        e: &std::io::Error,
        workspace_root: &Path,
        display_path: &str,
        tool: &str,
    ) -> String {
        use std::io::ErrorKind;
        if self.create_only && e.kind() == ErrorKind::AlreadyExists {
            return self.deny_overwrite(workspace_root, display_path, tool);
        }
        // ELOOP (symlink with O_NOFOLLOW) and ENOTDIR (a non-dir where a dir
        // component was required) are the ancestor-swap signatures. (`raw` is
        // scoped to the unix branch so the non-unix build has no unused var.)
        #[cfg(unix)]
        let is_symlink_ancestor = {
            let raw = e.raw_os_error();
            raw == Some(libc::ELOOP) || raw == Some(libc::ENOTDIR) || raw == Some(libc::EMLINK)
        };
        #[cfg(not(unix))]
        let is_symlink_ancestor = e.kind() == ErrorKind::PermissionDenied;
        if is_symlink_ancestor {
            return self.record(
                workspace_root,
                tool,
                format!(
                    "`{display_path}` traverses a symlinked directory — symlinked ancestors \
                     are not followed for granted writes (no escape via symlink/rename)."
                ),
            );
        }
        // A non-security failure (e.g. ENOENT parent for an edit of a missing
        // file): surface plainly, still sink-recorded so the audit is complete.
        self.record(
            workspace_root,
            tool,
            format!("`{display_path}` could not be written: {e}"),
        )
    }

    fn patterns_for_message(&self) -> String {
        if self.patterns.is_empty() {
            "nothing — this grant is read-only".to_string()
        } else {
            self.patterns.join(", ")
        }
    }

    /// Format the `[denied]` message, push it to the sink, return it.
    fn record(&self, workspace_root: &Path, tool: &str, detail: String) -> String {
        let message = format!("{DENIED_MARKER} {tool}: {detail}");
        if let Some(sink) = &self.sink {
            sink(WriteGrantViolation {
                workspace: workspace_root.to_path_buf(),
                tool: tool.to_owned(),
                detail: message.clone(),
            });
        }
        tracing::warn!(
            tool,
            workspace = %workspace_root.display(),
            %message,
            "write grant violation refused",
        );
        message
    }
}

/// The workspace-RELATIVE form of `resolved` (an absolute path the tool's
/// scope machinery already confined), or `None` if it is not under the
/// workspace. Purely lexical `strip_prefix` (the symlink safety is the
/// confined open's job); falls back to a canonicalized root prefix so a
/// symlinked workspace root (e.g. cwd under `/tmp` → `/private/tmp`) still
/// relativizes. A `..`/`.` component (there should be none post-normalize)
/// makes the confined walk reject it — deny-wins.
fn workspace_relative(workspace_root: &Path, resolved: &Path) -> Option<PathBuf> {
    if let Ok(rel) = resolved.strip_prefix(workspace_root) {
        return Some(rel.to_path_buf());
    }
    if let Ok(root_c) = std::fs::canonicalize(workspace_root) {
        if let Ok(rel) = resolved.strip_prefix(&root_c) {
            return Some(rel.to_path_buf());
        }
    }
    None
}

/// The leaf open discipline for the confined walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfinedLeaf {
    /// `O_CREAT|O_EXCL` — create, refuse if it already exists (`create_only`).
    CreateNew,
    /// `O_CREAT|O_TRUNC` — create or overwrite (fenced write, not create_only).
    CreateOrTruncate,
    /// `O_RDWR`, no create — the leaf must already exist (fenced edit).
    OpenExistingRw,
}

/// #1976 security round — open `workspace_root`/`rel` for writing via a
/// component-wise `O_NOFOLLOW` `openat` walk, so the opened object is THE SAME
/// one the allowlist matched (no check-vs-open divergence). Each intermediate
/// component is opened from the parent dir's fd with `O_NOFOLLOW|O_DIRECTORY`,
/// so a symlinked — or swapped-to-symlink — ancestor fails `ELOOP`/`ENOTDIR`
/// at its own component, closing the ancestor-swap TOCTOU. The leaf is opened
/// `O_NOFOLLOW` with the [`ConfinedLeaf`] discipline. `rustix` wraps the raw
/// `openat`/`mkdirat` syscalls in a safe API (no `unsafe` under the
/// workspace-wide `deny(unsafe_code)`).
#[cfg(unix)]
pub fn open_confined(
    workspace_root: &Path,
    rel: &Path,
    leaf_mode: ConfinedLeaf,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, RawMode, mkdirat, openat};
    use std::ffi::OsStr;
    use std::io;
    use std::os::fd::OwnedFd;

    // Only Normal components survive (the grant validator + `workspace_relative`
    // already reject `..`; re-check fail-closed so a `.`/`..`/root can never
    // re-point the walk).
    let mut names: Vec<&OsStr> = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(name) => names.push(name),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write grant: non-normal path component",
                ));
            }
        }
    }
    let Some((leaf, ancestors)) = names.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write grant: empty relative path",
        ));
    };

    // Anchor at the CANONICAL workspace root: resolving the trusted root's OWN
    // symlinks is intended; the fence guards components UNDER it. The canonical
    // root has no symlink components, so a plain `O_DIRECTORY` open is safe.
    let root_real = std::fs::canonicalize(workspace_root)?;
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir: OwnedFd = openat(
        rustix::fs::CWD,
        root_real.as_path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    let create_parents = matches!(
        leaf_mode,
        ConfinedLeaf::CreateNew | ConfinedLeaf::CreateOrTruncate
    );
    let dir_mode = Mode::from_raw_mode(0o755 as RawMode);
    for name in ancestors {
        dir = match openat(&dir, *name, dir_flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) if create_parents => {
                // Create the missing intermediate FROM this dirfd (never
                // through a symlink), then reopen it `O_NOFOLLOW`.
                mkdirat(&dir, *name, dir_mode)?;
                openat(&dir, *name, dir_flags, Mode::empty())?
            }
            Err(e) => return Err(io::Error::from(e)),
        };
    }

    let leaf_flags = OFlags::NOFOLLOW
        | OFlags::CLOEXEC
        | match leaf_mode {
            ConfinedLeaf::CreateNew => OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            ConfinedLeaf::CreateOrTruncate => OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            ConfinedLeaf::OpenExistingRw => OFlags::RDWR,
        };
    let leaf_fd = openat(
        &dir,
        *leaf,
        leaf_flags,
        Mode::from_raw_mode(0o644 as RawMode),
    )?;
    Ok(std::fs::File::from(leaf_fd))
}

/// Non-Unix fallback for [`open_confined`].
///
/// **KNOWN LIMITATION (non-Unix only; fleet workers run on Unix).** Without
/// `openat`, this cannot walk the path component-by-component race-free, so it
/// approximates: an ancestor `symlink_metadata` scan up to the root, then a
/// leaf open at the lexically-joined path. Precisely:
/// - [`ConfinedLeaf::CreateNew`] is race-free at the LEAF — `create_new(true)`
///   is `O_CREAT|O_EXCL`, which atomically refuses a pre-existing file OR
///   symlink at the leaf; parents are created first (parity with Unix).
/// - [`ConfinedLeaf::CreateOrTruncate`] / [`ConfinedLeaf::OpenExistingRw`] use
///   a check-then-open leaf symlink test that is inherently TOCTOU-racy, and
///   the ANCESTOR `lstat` scan is likewise racy against a concurrent swap.
///
/// This path is therefore a best-effort DEGRADED fallback, NOT the race-free
/// security boundary the Unix `openat` walk provides. The tool-layer allowlist
/// still applies on every platform; only the OS-level ancestor-swap defense is
/// weaker here.
#[cfg(not(unix))]
pub fn open_confined(
    workspace_root: &Path,
    rel: &Path,
    leaf_mode: ConfinedLeaf,
) -> std::io::Result<std::fs::File> {
    use std::io;
    let full = workspace_root.join(rel);
    // Best-effort: reject if any ancestor up to the root is a symlink (racy —
    // see the KNOWN LIMITATION above).
    let mut cur = full.parent();
    while let Some(dir) = cur {
        if dir == workspace_root {
            break;
        }
        if dir.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "symlinked ancestor",
            ));
        }
        cur = dir.parent();
    }
    // Create missing parents for BOTH create modes (parity with the Unix walk's
    // `mkdirat`), so a fenced create of `dir/leaf` behaves the same per-OS.
    if matches!(
        leaf_mode,
        ConfinedLeaf::CreateNew | ConfinedLeaf::CreateOrTruncate
    ) {
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    match leaf_mode {
        // `create_new` == O_CREAT|O_EXCL: atomically refuses an existing file
        // or symlink at the leaf, so no separate (racy) leaf check is needed.
        ConfinedLeaf::CreateNew => {
            opts.write(true).create_new(true);
            return opts.open(&full);
        }
        ConfinedLeaf::CreateOrTruncate => {
            opts.write(true).create(true).truncate(true);
        }
        ConfinedLeaf::OpenExistingRw => {
            opts.read(true).write(true);
        }
    }
    // Racy leaf symlink test for the non-exclusive modes (documented residual).
    if full.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "symlink leaf",
        ));
    }
    opts.open(&full)
}

/// Confined create/overwrite: open `rel` via [`open_confined`] and write
/// `content` to it, off the reactor (`spawn_blocking`). Returns the raw
/// `io::Error` for the caller to map through
/// [`WritePathGrant::map_confined_error`].
pub async fn confined_write(
    workspace_root: PathBuf,
    rel: PathBuf,
    content: Vec<u8>,
    create_only: bool,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let leaf_mode = if create_only {
            ConfinedLeaf::CreateNew
        } else {
            ConfinedLeaf::CreateOrTruncate
        };
        let mut file = open_confined(&workspace_root, &rel, leaf_mode)?;
        file.write_all(&content)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Confined overwrite of an EXISTING leaf, bound to `expected` (#2193 R4).
///
/// The fenced write path otherwise re-opens the leaf with `O_TRUNC` and
/// destroys whatever it resolves to — so an armed, *authorized* over-window
/// overwrite must instead open the confined leaf WITHOUT truncating, `fstat`
/// the descriptor, and truncate + rewrite only when its epoch still equals the
/// one the read ledger authorized. Same binding as
/// [`crate::tools::write_no_follow_checked`], but via the ancestor-safe
/// `openat` walk so the write fence still holds. Only for existing files
/// (`OpenExistingRw`), so it is never used under a `create_only` grant.
pub(crate) async fn confined_write_checked(
    workspace_root: PathBuf,
    rel: PathBuf,
    content: Vec<u8>,
    expected: crate::tools::read_window::ViewEpoch,
) -> std::io::Result<crate::tools::CheckedWrite> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = open_confined(&workspace_root, &rel, ConfinedLeaf::OpenExistingRw)?;
        // fstat the walked descriptor and bind to the authorizing epoch before
        // destroying any content.
        let meta = file.metadata()?;
        let found = crate::tools::read_window::ViewEpoch::from_metadata(&meta)
            .ok_or_else(|| std::io::Error::other("descriptor metadata unavailable"))?;
        if found != expected {
            return Ok(crate::tools::CheckedWrite::EpochChanged { found });
        }
        file.set_len(0)?;
        file.write_all(&content)?;
        Ok(crate::tools::CheckedWrite::Written)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Confined edit — phase 1: open the existing leaf `O_RDWR` via
/// [`open_confined`] and read its contents, returning BOTH the open handle and
/// the bytes. The SAME handle is handed to [`confined_rewrite`] for the
/// write-back, so read and write bind to one walked object (no re-open, no
/// ancestor-swap window between them).
pub async fn confined_open_rdwr(
    workspace_root: PathBuf,
    rel: PathBuf,
) -> std::io::Result<(std::fs::File, String)> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = open_confined(&workspace_root, &rel, ConfinedLeaf::OpenExistingRw)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok((file, content))
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// Confined edit — phase 2: rewrite the file handle from [`confined_open_rdwr`]
/// with `new_content` (truncate + write from offset 0), off the reactor. The
/// handle is the same object phase 1 opened, so the edit never re-resolves the
/// path (closing the read-vs-write TOCTOU an ancestor swap would exploit).
pub async fn confined_rewrite(
    mut file: std::fs::File,
    new_content: Vec<u8>,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(&new_content)?;
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

/// v1 pattern validation — the octos-agent twin of
/// `octos_fleet::validate_write_path_pattern` (kept textually close on
/// purpose; this crate has no octos-fleet dependency). Plan-time validation
/// already rejected these for fleet grants; re-checking here fails closed for
/// any programmatic caller.
fn validate_pattern(pattern: &str) -> eyre::Result<()> {
    let bail = |reason: &str| eyre::bail!("write grant pattern `{pattern}` is invalid: {reason}");
    if pattern.is_empty() {
        return bail("pattern is empty");
    }
    if Path::new(pattern).is_absolute() || pattern.starts_with('/') {
        return bail("pattern must be workspace-relative");
    }
    if pattern.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return bail("pattern contains control characters");
    }
    if pattern.contains("**") {
        return bail("recursive `**` globs are not supported in v1");
    }
    for ch in ['(', ')', '\\', '"', ':', '[', ']', '{', '}'] {
        if pattern.contains(ch) {
            return bail("pattern contains unsupported metacharacters");
        }
    }
    for segment in pattern.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return bail("`.`/`..`/empty path segments are not allowed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn confined_write_checked_refuses_a_changed_leaf() {
        // #2193 R4 (codex H2c): the fenced write path used to O_TRUNC the leaf
        // blind. It must now bind to the authorizing epoch, catching a same-size,
        // same-mtime content swap exactly like the non-fenced checked writer.
        let ws = tempfile::tempdir().unwrap();
        let path = ws.path().join("card.txt");
        std::fs::write(&path, b"AAAAAAAAAA").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mtime = meta.modified().unwrap();
        let authorized = crate::tools::read_window::ViewEpoch::from_metadata(&meta).unwrap();

        std::fs::write(&path, b"BBBBBBBBBB").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let result = confined_write_checked(
            ws.path().to_path_buf(),
            std::path::PathBuf::from("card.txt"),
            b"CCCCCCCCCC".to_vec(),
            authorized,
        )
        .await
        .unwrap();
        assert!(
            matches!(result, crate::tools::CheckedWrite::EpochChanged { .. }),
            "fenced checked write must refuse a changed leaf",
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"BBBBBBBBBB");
    }

    #[tokio::test]
    async fn confined_write_checked_writes_an_unchanged_leaf() {
        let ws = tempfile::tempdir().unwrap();
        let path = ws.path().join("card.txt");
        std::fs::write(&path, b"hello").unwrap();
        let authorized =
            crate::tools::read_window::ViewEpoch::from_metadata(&std::fs::metadata(&path).unwrap())
                .unwrap();
        let result = confined_write_checked(
            ws.path().to_path_buf(),
            std::path::PathBuf::from("card.txt"),
            b"world!!".to_vec(),
            authorized,
        )
        .await
        .unwrap();
        assert!(matches!(result, crate::tools::CheckedWrite::Written));
        assert_eq!(std::fs::read(&path).unwrap(), b"world!!");
    }

    fn grant(patterns: &[&str], create_only: bool) -> WritePathGrant {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        WritePathGrant::new(&owned, create_only).expect("test grant compiles")
    }

    #[test]
    fn write_grant_matches_workspace_relative_globs() {
        let dir = tempfile::tempdir().unwrap();
        let g = grant(&["exemplar.card", "cards/*.card"], false);

        let ok = |rel: &str| {
            g.check_write(dir.path(), &dir.path().join(rel), rel, "write_file")
                .is_ok()
        };
        assert!(ok("exemplar.card"));
        assert!(ok("cards/a.card"));
        // Exact-name glob does not match extensions of it.
        assert!(!ok("exemplar.card.bak"));
        // `*` must not cross `/` (literal_separator).
        assert!(!ok("cards/nested/b.card"));
        assert!(!ok("app.md"));
    }

    #[test]
    fn write_grant_rejects_invalid_patterns_at_construction() {
        for bad in ["../up", "/abs", "a/**", "a[b]", "a{b}", "a:b", "", "a//b"] {
            assert!(
                WritePathGrant::new(&[bad.to_string()], false).is_err(),
                "pattern {bad:?} must fail to compile",
            );
        }
    }

    #[test]
    fn write_grant_denial_records_to_sink_with_denied_marker() {
        let dir = tempfile::tempdir().unwrap();
        let seen: Arc<Mutex<Vec<WriteGrantViolation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let g = grant(&["exemplar.card"], true).with_violation_sink(Arc::new(move |v| {
            sink_seen.lock().unwrap().push(v);
        }));

        let denied = g
            .check_write(
                dir.path(),
                &dir.path().join("app.md"),
                "app.md",
                "write_file",
            )
            .expect_err("app.md is outside the grant");
        assert!(denied.contains(DENIED_MARKER), "typed refusal: {denied}");
        assert!(denied.contains("app.md"));
        assert!(denied.contains("exemplar.card"), "message lists the grant");

        let overwrite = g.deny_overwrite(dir.path(), "exemplar.card", "write_file");
        assert!(overwrite.contains(DENIED_MARKER));
        assert!(overwrite.contains("already exists"));

        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 2, "both violations recorded");
        assert_eq!(events[0].tool, "write_file");
        assert_eq!(events[0].workspace, dir.path());
        assert!(events[0].detail.contains(DENIED_MARKER));
    }

    #[test]
    fn write_grant_create_only_refuses_every_edit() {
        let dir = tempfile::tempdir().unwrap();
        let g = grant(&["exemplar.card"], true);
        // Even the ALLOWLISTED path may not be edited under create_only.
        let denied = g
            .check_edit(
                dir.path(),
                &dir.path().join("exemplar.card"),
                "exemplar.card",
                "edit_file",
            )
            .expect_err("create_only refuses edits");
        assert!(denied.contains("create-only"), "{denied}");

        // Without create_only, edits follow the allowlist.
        let g = grant(&["exemplar.card"], false);
        assert!(
            g.check_edit(
                dir.path(),
                &dir.path().join("exemplar.card"),
                "exemplar.card",
                "edit_file",
            )
            .is_ok()
        );
        assert!(
            g.check_edit(
                dir.path(),
                &dir.path().join("app.md"),
                "app.md",
                "edit_file"
            )
            .is_err()
        );
    }

    #[test]
    fn write_grant_empty_allowlist_denies_all_writes() {
        let dir = tempfile::tempdir().unwrap();
        let g = grant(&[], false);
        let denied = g
            .check_write(dir.path(), &dir.path().join("x.txt"), "x.txt", "write_file")
            .expect_err("empty allowlist = read-only");
        assert!(denied.contains("read-only"), "{denied}");
    }

    #[test]
    fn check_write_is_lexical_symlink_safety_is_the_open() {
        // Security round: `check_write` is now a PURELY LEXICAL allowlist
        // decision returning the relative path — it does NOT touch the
        // filesystem (the confined open enforces symlink safety). A matching
        // path returns Ok(rel) regardless of on-disk symlinks; a symlinked
        // ancestor is refused later, at `open_confined`.
        let dir = tempfile::tempdir().unwrap();
        let g = grant(&["cards/*.card"], false);
        let rel = g
            .check_write(
                dir.path(),
                &dir.path().join("cards/a.card"),
                "cards/a.card",
                "write_file",
            )
            .expect("lexical match returns the relative path");
        assert_eq!(rel, std::path::Path::new("cards/a.card"));
    }

    // ------------------------------------------------------------------
    // Security round (codex): the component-wise O_NOFOLLOW confined walk
    // is what makes the checked path and the opened object THE SAME, closing
    // the ancestor-swap TOCTOU. These exercise `open_confined` directly.
    // ------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn write_no_follow_follows_ancestor_symlink_the_gap_confined_open_closes() {
        // RED/motivation: the OLD fenced write used `write_no_follow(lexical)`,
        // whose leaf-only O_NOFOLLOW FOLLOWS a symlinked ANCESTOR — the exact
        // escape. Prove the gap concretely so the confined-open fix is
        // grounded, not theoretical.
        use std::os::unix::fs::symlink;
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), ws.path().join("cards")).unwrap();

        // Leaf-only O_NOFOLLOW open of `<ws>/cards/x.card` follows `cards`.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(crate::tools::write_no_follow(
            &ws.path().join("cards/x.card"),
            b"escaped\n",
        ));
        assert!(
            res.is_ok(),
            "the OLD primitive follows the ancestor symlink"
        );
        assert!(
            outside.path().join("x.card").exists(),
            "demonstrates the escape the confined walk must close",
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_confined_refuses_symlinked_ancestor() {
        // The fix: a symlinked ancestor fails at its own component (ELOOP /
        // ENOTDIR), so nothing is created at the symlink target.
        use std::os::unix::fs::symlink;
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), ws.path().join("cards")).unwrap();

        let err = open_confined(
            ws.path(),
            std::path::Path::new("cards/x.card"),
            ConfinedLeaf::CreateOrTruncate,
        )
        .expect_err("symlinked ancestor must be refused");
        assert!(
            matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)),
            "expected ELOOP/ENOTDIR, got {err:?}",
        );
        assert!(
            !outside.path().join("x.card").exists(),
            "nothing may land at the symlink target",
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_confined_toctou_ancestor_swap_after_stage_is_refused() {
        // The coordinator's scenario, deterministic: a REAL `cards/` exists
        // (an allowlist check would pass), THEN `cards` is swapped for a
        // symlink to outside; the confined open refuses at the `cards`
        // component. This is the TOCTOU the fix eliminates — check and open are
        // one walked object.
        use std::os::unix::fs::symlink;
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join("cards")).unwrap();

        // ... time passes; attacker swaps the checked dir for a symlink ...
        std::fs::remove_dir(ws.path().join("cards")).unwrap();
        symlink(outside.path(), ws.path().join("cards")).unwrap();

        let err = open_confined(
            ws.path(),
            std::path::Path::new("cards/loot.card"),
            ConfinedLeaf::CreateOrTruncate,
        )
        .expect_err("swapped ancestor must be refused at open");
        assert!(matches!(
            err.raw_os_error(),
            Some(libc::ELOOP) | Some(libc::ENOTDIR)
        ));
        assert!(!outside.path().join("loot.card").exists());
    }

    #[cfg(unix)]
    #[test]
    fn open_confined_creates_inside_real_dirs_and_mkdir_parents() {
        let ws = tempfile::tempdir().unwrap();
        // Missing intermediate dir is created safely (mkdirat in the walk).
        let file = open_confined(
            ws.path(),
            std::path::Path::new("cards/a.card"),
            ConfinedLeaf::CreateOrTruncate,
        )
        .expect("create inside real dirs");
        drop(file);
        assert!(ws.path().join("cards/a.card").exists());
        // Overwrite (CreateOrTruncate) succeeds; content truncated.
        std::fs::write(ws.path().join("cards/a.card"), "old-and-long").unwrap();
        let mut f = open_confined(
            ws.path(),
            std::path::Path::new("cards/a.card"),
            ConfinedLeaf::CreateOrTruncate,
        )
        .expect("overwrite ok");
        use std::io::Write;
        f.write_all(b"new").unwrap();
        drop(f);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("cards/a.card")).unwrap(),
            "new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_confined_create_new_refuses_existing() {
        let ws = tempfile::tempdir().unwrap();
        open_confined(
            ws.path(),
            std::path::Path::new("x.card"),
            ConfinedLeaf::CreateNew,
        )
        .expect("first create ok");
        let err = open_confined(
            ws.path(),
            std::path::Path::new("x.card"),
            ConfinedLeaf::CreateNew,
        )
        .expect_err("O_EXCL refuses the second create");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[cfg(unix)]
    #[test]
    fn open_confined_rdwr_requires_existing() {
        let ws = tempfile::tempdir().unwrap();
        assert!(
            open_confined(
                ws.path(),
                std::path::Path::new("missing.card"),
                ConfinedLeaf::OpenExistingRw,
            )
            .is_err(),
            "rdwr does not create",
        );
        std::fs::write(ws.path().join("here.card"), "v1").unwrap();
        assert!(
            open_confined(
                ws.path(),
                std::path::Path::new("here.card"),
                ConfinedLeaf::OpenExistingRw,
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn confined_write_and_rewrite_round_trip() {
        let ws = tempfile::tempdir().unwrap();
        confined_write(
            ws.path().to_path_buf(),
            std::path::PathBuf::from("note.txt"),
            b"hello\n".to_vec(),
            false,
        )
        .await
        .expect("confined write");
        let (file, content) = confined_open_rdwr(
            ws.path().to_path_buf(),
            std::path::PathBuf::from("note.txt"),
        )
        .await
        .expect("confined open rdwr");
        assert_eq!(content, "hello\n");
        confined_rewrite(file, b"world\n".to_vec())
            .await
            .expect("confined rewrite");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("note.txt")).unwrap(),
            "world\n"
        );
    }
}
