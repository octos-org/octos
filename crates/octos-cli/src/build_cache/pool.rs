//! Slot allocation, release, and reclamation for the build-cache pool
//! (design §3–§6; outer-loop #3).
//!
//! `acquire` scans the namespace's candidate slots, takes an exclusive
//! non-blocking flock on the first free one, runs the space gate, writes
//! holder metadata, and returns the slot with the lock fd held in-process
//! (I1: the lock is the truth, the fd is how we hold it). `release`
//! returns the slot idempotently and never deletes the cache (I2).
//! `reclaim_stale` walks every pool under a root and clears only slots
//! that are (a) unlocked, (b) whose holder pid is verifiably dead, and
//! (c) whose `last_used` is past the stale window — never by mtime.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::repo_key::RepoKey;

// Locking uses `fs2::FileExt` (MSRV 1.85; std's flock methods are 1.89+)
// — see the goal-notes twin in autonomy/monitor_runtime.rs for the full
// serialization rationale. Calls are fully qualified
// (`fs2::FileExt::try_lock_exclusive`) so they never resolve to the newer
// same-named std::fs::File methods; no trait import is needed for the
// fully-qualified form.

/// Seconds in one GiB-of-space comparison helper context: the gate compares
/// bytes (`min_free_gb * 1024^3`), per design §5.
const GIB: u64 = 1024 * 1024 * 1024;
/// Leaf name of the flock lock file inside a slot dir. Its inode is the
/// mutex's truth and is NEVER deleted or re-created by any code path.
pub(crate) const LOCK_LEAF: &str = ".lock";
/// Holder metadata leaf, present only while a slot is held.
const HOLDER_LEAF: &str = "holder.json";
/// One line of unix seconds; written only at acquire and release (§3.3).
const LAST_USED_LEAF: &str = "last_used";
/// The directory handed to cargo as `CARGO_TARGET_DIR`.
const TARGET_LEAF: &str = "target";
/// Directory name prefixes for the two namespaces (§1.3): peers and the
/// outer loop never share slots.
const SLOT_PREFIX: &str = "slot-";
const VERIFY_PREFIX: &str = "verify-";

/// Default per-repository peer-slot cap (§2).
pub const DEFAULT_PEER_SLOTS: u32 = 2;
/// Default per-repository outer-loop (verify) slot cap (§2).
pub const DEFAULT_VERIFY_SLOTS: u32 = 1;
/// Default space-gate threshold in GiB (§2). `0` disables the gate
/// (diagnostics only).
pub const DEFAULT_MIN_FREE_GB: u64 = 50;
/// Default stale window in hours before an unheld slot may be GC'd (§2;
/// 7 days matches cargo-gc.sh v1's staleness signal and keeps weekday-hot
/// caches out of weekly maintenance).
pub const DEFAULT_STALE_HOURS: u64 = 168;
/// Maximum GC retention window: ten 365-day years.
pub const MAX_STALE_HOURS: u64 = 10 * 365 * 24;

/// Why a slot is being held. Chooses the namespace (§1.3): `Peer` takes
/// `slot-1..=slot-peer_slots`, `Verify` takes `verify-1..=verify-verify_slots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotPurpose {
    /// A peer turn's compile slot.
    Peer,
    /// An outer-loop verification slot (`octos cache acquire --purpose verify`).
    Verify,
}

/// Terminal outcome recorded at release time (§3.4). Diagnostics only —
/// release behavior does not depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotOutcome {
    Completed,
    Failed,
    Cancelled,
    Retired,
}

impl SlotOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Retired => "retired",
        }
    }
}

/// Holder metadata written at acquire (§3.1). The pid is what crash
/// recovery checks; slug/goal/task/note are display-only. Public so
/// `octos cache status` (#5) can render the holder of each slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderMeta {
    /// `"peer"` or `"verify"`.
    pub kind: SlotKind,
    /// Holding process id; liveness-checked at reclamation (§3.5).
    pub pid: u32,
    /// Peer slug when `kind == Peer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Outer-loop command line note when `kind == Verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose_note: Option<String>,
    /// Unix seconds at acquire; lets a human audit a suspected pid-reuse
    /// resurrection (§3.5 residual risk).
    pub acquired_at: u64,
    /// Fresh identity for this acquisition, including repeated claims by one pid.
    #[serde(default)]
    pub claim_token: String,
}

/// Namespace tag mirrored into holder metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    Peer,
    Verify,
}

impl SlotKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Peer => SLOT_PREFIX,
            Self::Verify => VERIFY_PREFIX,
        }
    }

    /// If `name` is this namespace's `<prefix><number>` form, return the
    /// number; anything else (including `slot-`, `slot-x`, `slot-1x`) is not
    /// a slot dir and never touched. Caps GC enumeration to slot-shaped
    /// dirs only (D3).
    fn strip_prefix_of(self, name: &str) -> Option<u32> {
        name.strip_prefix(self.prefix())?
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
    }

    /// Namespace of a slot DIR, read off its final path component. Falls
    /// back to `Peer` for anything not named `verify-N` — diagnostics-only
    /// (the value feeds the `release` log line), and the pool's truth is
    /// the `.lock`, not this label.
    fn from_slot_dir_name(slot_dir: &Path) -> Self {
        match slot_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.strip_prefix(VERIFY_PREFIX)
                    .and_then(|rest| rest.parse::<u32>().ok())
                    .is_some()
            })
            .unwrap_or(false)
        {
            true => Self::Verify,
            false => Self::Peer,
        }
    }
}

impl From<SlotPurpose> for SlotKind {
    fn from(purpose: SlotPurpose) -> Self {
        match purpose {
            SlotPurpose::Peer => Self::Peer,
            SlotPurpose::Verify => Self::Verify,
        }
    }
}

/// Identity of one held slot, returned by [`acquire`].
///
/// Dropping a `Slot` does NOT release it — the holder must call
/// [`release`]; a dropped slot simply keeps the flock until process exit
/// (crash recovery then applies, §3.5).
#[derive(Debug)]
pub struct Slot {
    /// The slot directory (`<pool-root>/<repo-key>/<slot-N>`).
    pub path: PathBuf,
    /// The directory to export as `CARGO_TARGET_DIR` (`<slot>/target`).
    pub target_dir: PathBuf,
    /// Which namespace this slot belongs to.
    pub kind: SlotKind,
    pub claim_token: String,
    /// The held lock file, wrapped so [`release`] can drop it explicitly
    /// while the `Slot` handle stays with the caller (the close-path
    /// safety net may call `release` again). Held for the lifetime of
    /// this field — the fd IS the mutex (I1). Never closed early by
    /// anything but `release`, never re-created; dropping the `Slot`
    /// also drops the fd, which is exactly the crash path.
    lock: Option<File>,
}

/// Typed errors for the pool. Free-space failures are distinct variants so
/// callers can map them to model-readable copy (#4) or exit codes (#5)
/// instead of string-matching.
#[derive(Debug)]
pub enum BuildCacheError {
    /// Invalid pool configuration or GC override.
    InvalidConfig(String),
    /// Space gate refused the allocation (§5): `available_gb < min_gb`.
    /// `available_bytes` is the raw measured count — `gate --json` (#5)
    /// reports it directly instead of re-deriving precision from the
    /// rounded `available_gb` (D5).
    FreeSpaceLow {
        available_bytes: u64,
        available_gb: f64,
        min_gb: u64,
    },
    /// `fs2::available_space` failed (statvfs unavailable). Fail-closed:
    /// the whole point of this line is preventing full-disk incidents, so
    /// a failed measurement never lets an allocation through. Operators
    /// who need the gate off set `min_free_gb = 0`.
    FreeSpaceUnknown { source: std::io::Error },
    /// Every slot in the requested namespace is held (§3.2 step 5).
    /// Fail-fast: never queue, never wait — let the caller re-plan.
    PoolExhausted { repo_key: String, kind: SlotKind },
    /// Wrapped filesystem/serialization failure with context.
    Io {
        context: String,
        source: std::io::Error,
    },
    /// `release --slot <path>`: the path is not under the pool root (or
    /// escapes it via a symlink). Boundary enforced before any open/remove.
    SlotOutsidePool { slot: PathBuf, pool_root: PathBuf },
    /// `release --slot <path>`: no `.lock` under the named dir — i.e. not
    /// a slot this pool ever created.
    SlotNotFound { slot: PathBuf },
    /// `release --slot <path>`: the `.lock` is currently flocked by a live
    /// holder (a peer turn or a still-running acquire CLI). Detached
    /// release must not race a live one.
    SlotHeld { slot: PathBuf },
}

impl fmt::Display for BuildCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FreeSpaceLow {
                available_bytes,
                available_gb,
                min_gb,
            } => write!(
                f,
                "free space {available_gb:.2} GB ({available_bytes} bytes) is below the {min_gb} GB gate — refusing to allocate a build-cache slot; run `octos cache gc --apply` or free disk space"
            ),
            Self::FreeSpaceUnknown { source } => write!(
                f,
                "could not measure free space for the build-cache pool (fail-closed): {source}"
            ),
            Self::PoolExhausted { repo_key, kind } => {
                let label = match kind {
                    SlotKind::Peer => "peer",
                    SlotKind::Verify => "verify",
                };
                write!(
                    f,
                    "build-cache {label} pool for repo {repo_key} is full — every slot is held; each peer's slot frees at the end of its current turn, retry shortly or inspect holders with `octos cache status`"
                )
            }
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::InvalidConfig(message) => {
                write!(f, "invalid build-cache configuration: {message}")
            }
            Self::SlotOutsidePool { slot, pool_root } => write!(
                f,
                "refusing to release {} — it is not under the build-cache pool root {}",
                slot.display(),
                pool_root.display()
            ),
            Self::SlotNotFound { slot } => write!(
                f,
                "no build-cache slot at {} (missing .lock — not a slot this pool created)",
                slot.display()
            ),
            Self::SlotHeld { slot } => write!(
                f,
                "build-cache slot {} is currently held by a live process — refusing a detached release; retry when the holder finishes, or inspect `octos cache status`",
                slot.display()
            ),
        }
    }
}

impl std::error::Error for BuildCacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FreeSpaceUnknown { source } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
        .map(|s| s as &(dyn std::error::Error + 'static))
    }
}

impl BuildCacheError {
    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Optional `[build_cache]` configuration section (§2), structured like the
/// existing optional sections (`snapshots`, `tool_policy`): absent means
/// defaults, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildCacheConfig {
    /// Per-repository peer-slot cap, `>= 1` (default 2).
    pub peer_slots: u32,
    /// Per-repository outer-loop-slot cap, `>= 1` (default 1).
    pub verify_slots: u32,
    /// Space-gate threshold in GiB, `>= 0`; `0` disables the gate
    /// (default 50).
    pub min_free_gb: u64,
    /// Hours after which an unheld slot may be GC'd, `>= 1` (default 168).
    pub stale_hours: u64,
}

impl Default for BuildCacheConfig {
    fn default() -> Self {
        Self {
            peer_slots: DEFAULT_PEER_SLOTS,
            verify_slots: DEFAULT_VERIFY_SLOTS,
            min_free_gb: DEFAULT_MIN_FREE_GB,
            stale_hours: DEFAULT_STALE_HOURS,
        }
    }
}

impl BuildCacheConfig {
    /// Lower-bound validation (§2): each knob has a floor; violations are
    /// surfaced as config warnings rather than silently clamped.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.peer_slots < 1 {
            warnings.push("build_cache.peer_slots must be >= 1".to_string());
        }
        if self.verify_slots < 1 {
            warnings.push("build_cache.verify_slots must be >= 1".to_string());
        }
        if self.min_free_gb > 100_000 {
            warnings.push(format!(
                "build_cache.min_free_gb {} is implausibly large",
                self.min_free_gb
            ));
        }
        if self.stale_hours < 1 {
            warnings.push("build_cache.stale_hours must be >= 1".to_string());
        }
        if let Err(error) = stale_window_secs(self.stale_hours) {
            warnings.push(error.to_string());
        }
        warnings
    }

    fn slot_count(&self, kind: SlotKind) -> u32 {
        match kind {
            SlotKind::Peer => self.peer_slots.max(1),
            SlotKind::Verify => self.verify_slots.max(1),
        }
    }
}

/// Inputs describing the holder, carried into `holder.json` (§3.1). The
/// pid is taken from the current process — the serve process owns peer
/// slots and the CLI owns verify slots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HolderInfo {
    pub slug: Option<String>,
    pub goal_id: Option<String>,
    pub task_id: Option<String>,
    pub purpose_note: Option<String>,
    /// Record THIS pid in holder.json instead of the current process
    /// (#6): `octos cache acquire` exits right after acquiring, so its own
    /// pid is useless for §3.5 liveness — the pid that must be checked is
    /// the one whose lifetime the slot is supposed to span (the outer
    /// loop's driver). `None` (all peer-path callers) records
    /// `std::process::id()` as before.
    pub pid_override: Option<u32>,
}

/// Reclamation policy (§6): stale window plus whether to actually delete
/// (`octos cache gc --apply`) or only report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPolicy {
    pub stale_hours: u64,
    pub apply: bool,
}

/// What happened to one slot during a `reclaim_stale` walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimOutcome {
    /// Live holder (flock held): untouched.
    Locked,
    /// `holder.json` names a dead pid; metadata removed, contents kept
    /// pending the stale window.
    HolderCleared,
    /// Unheld and past the stale window: `target/` removed (never
    /// `.lock`).
    Reclaimed,
    /// Unheld and past the stale window, but nothing was deleted — a
    /// report-only walk (`apply: false`) or a slot with no `target/` left
    /// to reclaim. Carries the size `--apply` would free in
    /// [`ReclaimReport::would_free_bytes`] so a report can say WHAT would
    /// go, not merely that something would (D1).
    Stale,
    /// Unheld but inside the stale window.
    Fresh,
    /// The slot dir exists but its `.lock` file does not — structurally
    /// broken (we never create or delete `.lock`). Skipped, never deleted,
    /// and reported distinctly from `fresh` so a human can see it (D6).
    NoLock,
    /// Symbolic link or unresolved filesystem boundary; left untouched.
    Skipped,
}

impl ReclaimOutcome {
    /// Lowercase label for CLI/log rendering (`octos cache gc`, #5).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::HolderCleared => "holder_cleared",
            Self::Reclaimed => "reclaimed",
            Self::Stale => "stale",
            Self::Fresh => "fresh",
            Self::NoLock => "no_lock",
            Self::Skipped => "skipped",
        }
    }
}

/// Per-slot row of a reclamation report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimReport {
    pub slot_path: PathBuf,
    pub outcome: ReclaimOutcome,
    /// Bytes freed for `Reclaimed` rows (directory size before removal),
    /// `0` otherwise.
    pub freed_bytes: u64,
    /// Bytes `--apply` WOULD free now: the size of `target/` for `Stale`
    /// rows (report mode), and of any tree that existed at decision time
    /// for `Reclaimed`. `0` for every other outcome (D1: a report-only gc
    /// must be able to say what it would reclaim, not just "fresh").
    #[serde(default)]
    pub would_free_bytes: u64,
}

/// Unix seconds now (0 on a pre-epoch clock — treated as maximally stale).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `kill(pid, 0)` liveness probe for §3.5, via
/// `rustix::process::test_kill_process` (the safe wrapper for the
/// permission/existence check; signal 0 is never delivered). The mapping is
/// the design's three-state contract:
///
/// * `Ok(())` — alive and ours: `Some(true)`.
/// * `Err(ESRCH)` — no such process: `None` (dead, metadata may clear).
/// * `Err(EPERM)` — exists but belongs to another uid: `Some(false)`.
///   Treated as alive (skip); fail-safe toward never deleting a slot
///   someone may hold.
/// * `Err(EINVAL)` — malformed pid (out of range for the kernel): `None`
///   (dead). A holder pid our kernel cannot even name was not written by a
///   live holder on this host.
/// * any other errno — `Some(false)`: conservative alive. An errno we do
///   not recognize must not license deletion (§6 red line).
///
/// Unlike the `kill` binary (whose exit status collapses ESRCH and EPERM
/// into the same "1", and whose availability depends on `$PATH`), the
/// syscall returns a distinguishable errno — and unlike `libc::kill` it is
/// safe under the workspace-wide `deny(unsafe_code)`.
#[cfg(unix)]
fn pid_alive(pid: u32) -> Option<bool> {
    use rustix::io::Errno;
    use rustix::process::{Pid, test_kill_process};
    let Ok(raw_pid) = i32::try_from(pid) else {
        // Invalid metadata must never become a negative process/group id.
        return Some(false);
    };
    let Some(pid) = Pid::from_raw(raw_pid) else {
        return None;
    };
    match test_kill_process(pid) {
        Ok(()) => Some(true),
        Err(Errno::SRCH) => None,
        Err(Errno::INVAL) => None,
        Err(_) => Some(false), // EPERM and anything unrecognized: alive
    }
}

#[cfg(not(unix))]
fn pid_alive(pid: u32) -> Option<bool> {
    // Non-Unix hosts have no kill(pid,0); fail toward "alive" so a stale
    // holder file never causes a wrong delete. The flock still serializes
    // concurrent holders on those hosts.
    let _ = pid;
    Some(false)
}

/// Test seam for [`pid_alive`]: lets tests assert the §3.5 dead-holder
/// path deterministically instead of racing a real process exit.
#[cfg(test)]
#[cfg(unix)]
fn spawn_dead_pid() -> u32 {
    // Fork-style trick without libc: spawn a `sleep`, kill it, and wait it
    // out, so the pid is verifiably gone by the time we return it.
    use std::process::{Command, Stdio};
    let mut child = Command::new("sleep")
        .arg("5")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    pid
}

/// Atomic temp-file leaf name: `.<leaf>.tmp-<pid>-<uniq>` (the
/// peer_io atomic-write pattern from peers/mod.rs: O_EXCL create beside
/// the leaf + fsync + rename, so a reader never sees a torn file).
fn tmp_name(leaf: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_UNIQ: AtomicU64 = AtomicU64::new(0);
    let uniq = TMP_UNIQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!(".{leaf}.tmp-{pid}-{uniq}")
}

/// Atomically replace `dir/leaf` (peer_io pattern: temp + fsync + rename).
fn write_file_atomic(dir: &Path, leaf: &str, content: &str) -> std::io::Result<()> {
    let tmp = dir.join(tmp_name(leaf));
    let path = dir.join(leaf);
    {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(err) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn write_last_used(slot_dir: &Path, now: u64) -> std::io::Result<()> {
    write_file_atomic(slot_dir, LAST_USED_LEAF, &format!("{now}\n"))
}

/// Space gate (§5): ensure the filesystem holding `pool_root` has at least
/// `min_free_gb` GiB available. `min_free_gb == 0` disables the check.
/// A failed measurement is a refusal (`FreeSpaceUnknown`, fail-closed) —
/// not a pass.
fn space_gate(pool_root: &Path, min_free_gb: u64) -> Result<(), BuildCacheError> {
    if min_free_gb == 0 {
        return Ok(());
    }
    let available = fs2::available_space(pool_root)
        .map_err(|source| BuildCacheError::FreeSpaceUnknown { source })?;
    let min_bytes = min_free_gb.saturating_mul(GIB);
    if available < min_bytes {
        return Err(BuildCacheError::FreeSpaceLow {
            available_bytes: available,
            available_gb: available as f64 / GIB as f64,
            min_gb: min_free_gb,
        });
    }
    Ok(())
}

/// Space gate as a standalone probe for `octos cache gate` (#5). Same
/// semantics as [`space_gate`], including fail-closed on measurement
/// failure.
pub fn check_free_space(pool_root: &Path, min_free_gb: u64) -> Result<(), BuildCacheError> {
    space_gate(pool_root, min_free_gb)
}

fn slot_dir(repo_dir: &Path, kind: SlotKind, n: u32) -> PathBuf {
    repo_dir.join(format!("{}{n}", kind.prefix()))
}

/// Acquire a slot (§3.2). Space-gate first, then scan the namespace's
/// candidates in order; first slot whose `.lock` can be flocked
/// exclusively and non-blockingly wins. The lock fd is held by the
/// returned [`Slot`] — dropping it without [`release`] keeps the slot held
/// until process exit (by design: a leaked slot is recoverable, a
/// wrongly-shared slot is not).
pub fn acquire(
    pool_root: &Path,
    repo_key: &RepoKey,
    purpose: SlotPurpose,
    config: &BuildCacheConfig,
    holder: &HolderInfo,
) -> Result<Slot, BuildCacheError> {
    let kind = SlotKind::from(purpose);
    // I3 + D5 (§3.2 step 1): create the POOL ROOT if missing, then measure,
    // then gate — before creating the repo-key dir or any slot artifact, so
    // a refusal leaves nothing behind. (statvfs needs an existing path; the
    // root itself is the one dir creation the gate depends on.)
    if let Err(e) = fs::create_dir_all(pool_root) {
        return Err(BuildCacheError::io(
            format!("failed to create pool root {}", pool_root.display()),
            e,
        ));
    }
    space_gate(pool_root, config.min_free_gb)?;
    let repo_dir = pool_root.join(repo_key.as_str());
    fs::create_dir_all(&repo_dir).map_err(|e| {
        BuildCacheError::io(
            format!("failed to create pool dir {}", repo_dir.display()),
            e,
        )
    })?;

    let count = config.slot_count(kind);
    let mut last_err: Option<std::io::Error> = None;
    for n in 1..=count {
        let dir = slot_dir(&repo_dir, kind, n);
        if let Err(e) = fs::create_dir_all(&dir) {
            last_err = Some(e);
            continue;
        }
        let lock_path = dir.join(LOCK_LEAF);
        // create(true) but never truncate and never unlink: the inode, once
        // created, is the mutex for the slot's lifetime. truncate(false)
        // is stated explicitly — clippy:suspicious_open_options — and
        // keep(true) preserves an existing lock file's identity.
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {
                // Detached claims drop their flock but retain ownership while
                // their recorded process is alive. Inspect under this lock.
                if let Some(meta) = read_holder(&dir)
                    && pid_alive(meta.pid).is_some()
                {
                    continue;
                }
                let target_dir = dir.join(TARGET_LEAF);
                if let Err(e) = fs::create_dir_all(&target_dir) {
                    last_err = Some(e);
                    continue;
                }
                let now = now_secs();
                let meta = HolderMeta {
                    kind,
                    pid: holder.pid_override.unwrap_or_else(std::process::id),
                    slug: holder.slug.clone(),
                    goal_id: holder.goal_id.clone(),
                    task_id: holder.task_id.clone(),
                    purpose_note: holder.purpose_note.clone(),
                    acquired_at: now,
                    claim_token: uuid::Uuid::new_v4().to_string(),
                };
                let json = serde_json::to_string(&meta).map_err(|e| {
                    BuildCacheError::io("failed to serialize holder.json", e.into())
                })?;
                write_file_atomic(&dir, HOLDER_LEAF, &json).map_err(|e| {
                    BuildCacheError::io(
                        format!("failed to write {}", dir.join(HOLDER_LEAF).display()),
                        e,
                    )
                })?;
                write_last_used(&dir, now).map_err(|e| {
                    BuildCacheError::io(
                        format!("failed to write {}", dir.join(LAST_USED_LEAF).display()),
                        e,
                    )
                })?;
                return Ok(Slot {
                    path: dir,
                    target_dir,
                    kind,
                    lock: Some(lock),
                    claim_token: meta.claim_token,
                });
            }
            Err(_) => continue, // held by someone else: next candidate
        }
    }
    if let Some(e) = last_err {
        // A create/open failure on every candidate is an environment error,
        // not exhaustion — surface it distinctly instead of a misleading
        // "pool full".
        Err(BuildCacheError::io(
            "failed to access any candidate slot",
            e,
        ))
    } else {
        Err(BuildCacheError::PoolExhausted {
            repo_key: repo_key.as_str().to_owned(),
            kind,
        })
    }
}

/// Result of an identity-checked release; no-op cases preserve all metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDisposition {
    Released,
    AlreadyReleased,
    ClaimMismatch,
}

/// Release a slot (§3.4): remove `holder.json`, stamp `last_used = now`,
/// drop the lock fd (the kernel releases the flock). Idempotent — a slot
/// already missing `holder.json` is a no-op, because the turn-terminal
/// main release and the close/evict safety-net release may both fire.
/// The `target/` contents are NEVER deleted (I2: the whole point of the
/// pool is that the next peer of this repository reuses them).
pub fn release(
    slot: &mut Slot,
    outcome: SlotOutcome,
) -> Result<ReleaseDisposition, BuildCacheError> {
    if slot.lock.is_none() {
        return Ok(ReleaseDisposition::AlreadyReleased);
    }
    // §3.4 order: remove holder.json and stamp last_used WHILE still holding
    // the flock, and drop the lock fd last. Doing it the other way round
    // opens a window where another process acquires this slot and writes its
    // own holder.json — which our remove_file would then delete from under
    // the new holder. Holding the lock across both writes closes that race:
    // die before them and §3.5 clears the dead-pid metadata; die after them
    // and the flock vanishes with the process leaving a clean ownerless slot.
    let disposition = release_claim(&slot.path, &slot.claim_token, outcome, slot.kind)?;
    slot.lock = None;
    Ok(disposition)
}

/// Acquire a VERIFY-namespace slot on behalf of a caller that outlives
/// this process (#6's `octos cache acquire --purpose verify`).
///
/// TRUTH MODEL — holder.json + pid liveness, NOT a persistent flock. The
/// design (docs/build-cache-pool.md §3.5 arm 2 + the 外环槽 paragraph in
/// §4) assigns cross-process lifetime to the METADATA: the acquire CLI is
/// expected to exit immediately, the outer loop's verification runs in a
/// DIFFERENT process, and the release comes from yet another
/// `octos cache release` invocation. Nothing can hold a flock across
/// those three lifetimes, so the slot's "held" state is `holder.json`
/// (with a live pid recorded) + §3.5's `kill(pid, 0)` liveness check.
///
/// The pid written is the CALLER's (`holder.pid_override`), not ours: a
/// `std::process::id()` here would be dead the moment this CLI exits,
/// which would let `gc`/`reclaim_stale` reap the slot out from under the
/// still-running outer-loop verification. Callers with no distinct pid
/// (tests) pass `None` and get the current process recorded instead.
///
/// The lock fd is intentionally DROPPED at the end of this function (not
/// leaked): the flock is meaningless across the three lifetimes involved
/// (acquire CLI → verification process → release CLI), so the handoff is
/// explicit and immediate — from that drop onward, holder.json + pid
/// liveness is the slot's truth, exactly the state §3.5 arm 2 describes.
///
/// Returns the slot identity (dir + target dir) WITHOUT a live lock
/// handle — there is nothing for the caller to drop.
pub fn acquire_detached(
    pool_root: &Path,
    repo_key: &RepoKey,
    config: &BuildCacheConfig,
    holder: &HolderInfo,
) -> Result<DetachedSlot, BuildCacheError> {
    let pid = holder.pid_override.unwrap_or_else(std::process::id);
    let holder = HolderInfo {
        pid_override: Some(pid),
        ..holder.clone()
    };
    let mut slot = acquire(pool_root, repo_key, SlotPurpose::Verify, config, &holder)?;
    let detached = DetachedSlot {
        path: slot.path.clone(),
        target_dir: slot.target_dir.clone(),
        claim_token: slot.claim_token.clone(),
    };
    // Drop the flock ON PURPOSE, right here — the defining property of the
    // detached (#6) model: from this line on, the slot's held-state is
    // `holder.json` + pid liveness, nothing else. We do NOT leak the fd
    // (`std::mem::forget`): a leaked fd would keep the flock alive for the
    // whole CLI process, which (a) is indistinguishable from a peer-style
    // live holder for the duration of a long-lived CLI, and (b) pushes the
    // transition point to "whenever the process happens to exit" instead
    // of this explicit, documented line. Closing the fd here makes
    // `release_detached` (and `gc`) see exactly the §3.5 arm-2 world:
    // no flock, metadata present, pid alive ⇒ held; pid dead ⇒ reapable.
    slot.lock = None;
    Ok(detached)
}

/// Identity of a slot acquired by [`acquire_detached`] — paths only, no
/// lock handle (the fd was leaked by design; see that function).
#[derive(Debug, Clone)]
pub struct DetachedSlot {
    /// The slot directory (`<pool-root>/<repo-key>/verify-N`).
    pub path: PathBuf,
    /// The `CARGO_TARGET_DIR` value (`<slot>/target`).
    pub target_dir: PathBuf,
    /// Pass this exact identity to `release_detached` or `--token`.
    pub claim_token: String,
}

/// Release a slot by its slot DIR path, from a process that is NOT the
/// one that acquired it (`octos cache release --slot <path> --token <token>`).
///
/// Truth model (§3.5 arm 2, the cross-process half — see the long comment
/// on [`leak_for_detached_holder`]): between the acquire CLI exiting and
/// the release CLI running, the only thing keeping this slot "held" is
/// `holder.json` + pid liveness. Re-acquiring the `.lock` here is
/// therefore SAFE only because a detached holder never keeps the flock;
/// if it ever did (peer-style holders do), `try_lock_exclusive` fails and
/// we report [`BuildCacheError::SlotHeld`] instead of stomping a live
/// holder's metadata.
///
/// Idempotent: a slot with no `holder.json` is a no-op (same semantics as
/// [`release`]), so a double `release` invocation — or release racing with
/// the stale-holder cleanup inside `reclaim_stale` — is harmless. The
/// `target/` contents are never touched (I2).
pub fn release_detached(
    pool_root: &Path,
    slot_dir: &Path,
    claim_token: &str,
    outcome: SlotOutcome,
) -> Result<ReleaseDisposition, BuildCacheError> {
    // Guard: refuse to touch anything that is not a slot dir under THIS
    // pool root. `remove_dir_all` and the holder bookkeeping below operate
    // on caller-supplied paths, so the boundary is enforced first, before
    // any open/remove, and independently of symlinks (canonicalize both
    // sides — a symlinked argument must not smuggle a path outside the
    // pool).
    let canonical_root = pool_root
        .canonicalize()
        .map_err(|e| BuildCacheError::io("failed to canonicalize pool root".to_owned(), e))?;
    let canonical_slot = slot_dir.canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            // A path that doesn't exist can't be a slot; report it with
            // the same script-facing meaning as a missing `.lock` (the
            // dir-exists case below) rather than a raw io error.
            BuildCacheError::SlotNotFound {
                slot: slot_dir.to_path_buf(),
            }
        } else {
            BuildCacheError::io(format!("cannot resolve slot dir {}", slot_dir.display()), e)
        }
    })?;
    if !canonical_slot.starts_with(&canonical_root) {
        return Err(BuildCacheError::SlotOutsidePool {
            slot: slot_dir.to_path_buf(),
            pool_root: pool_root.to_path_buf(),
        });
    }
    // The lock file is the slot's mutex inode (never deleted, never
    // recreated). Missing ⇒ this is not a live slot dir at all: error
    // rather than fabricate state on an arbitrary directory.
    let lock_path = canonical_slot.join(LOCK_LEAF);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BuildCacheError::SlotNotFound {
                    slot: slot_dir.to_path_buf(),
                }
            } else {
                BuildCacheError::io(format!("failed to open {}", lock_path.display()), e)
            }
        })?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        // Intermittent double-release (outer loop 06: #6 review): on a loaded
        // test runner the second release occasionally observes the lock busy
        // for a microsecond after the first release's fd closed (flock
        // teardown vs fd reuse race on macOS). If the holder metadata is
        // ALREADY gone the release already happened — treat it as the
        // idempotent no-op it is, never an error. Only a slot that still has
        // a live holder.json is genuinely held.
        if canonical_slot.join(HOLDER_LEAF).exists() {
            return Err(BuildCacheError::SlotHeld {
                slot: slot_dir.to_path_buf(),
            });
        }
        return Ok(ReleaseDisposition::AlreadyReleased);
    }
    // Lock held for the duration of the release (same ordering argument as
    // `release`: metadata is cleared while holding, so a racing acquirer
    // can never observe a half-released slot; the lock fd drops at return).
    let kind = SlotKind::from_slot_dir_name(&canonical_slot);
    release_claim(&canonical_slot, claim_token, outcome, kind)
}

/// Compare identity under the caller's flock before changing metadata.
fn release_claim(
    slot_dir: &Path,
    claim_token: &str,
    outcome: SlotOutcome,
    kind: SlotKind,
) -> Result<ReleaseDisposition, BuildCacheError> {
    if !slot_dir.join(HOLDER_LEAF).exists() {
        return Ok(ReleaseDisposition::AlreadyReleased);
    }
    if claim_token.is_empty()
        || read_holder(slot_dir).is_none_or(|meta| meta.claim_token != claim_token)
    {
        return Ok(ReleaseDisposition::ClaimMismatch);
    }
    release_at_path(slot_dir, outcome, kind)?;
    Ok(ReleaseDisposition::Released)
}

/// Path-level release used by both [`release`] and `reclaim_stale`'s
/// dead-holder cleanup.
fn release_at_path(
    slot_dir: &Path,
    outcome: SlotOutcome,
    kind: SlotKind,
) -> Result<(), BuildCacheError> {
    let holder_path = slot_dir.join(HOLDER_LEAF);
    match fs::remove_file(&holder_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // idempotent no-op
        Err(e) => {
            return Err(BuildCacheError::io(
                format!("failed to remove {}", holder_path.display()),
                e,
            ));
        }
    }
    let now = now_secs();
    write_last_used(slot_dir, now).map_err(|e| {
        BuildCacheError::io(
            format!(
                "failed to write {}",
                slot_dir.join(LAST_USED_LEAF).display()
            ),
            e,
        )
    })?;
    tracing::debug!(slot = %slot_dir.display(), outcome = outcome.as_str(), kind = ?kind, "build-cache slot released");
    Ok(())
}

/// Update `last_used` for a held slot. Per §3.3 `last_used` is normally
/// written only at acquire and release; this exists for the one exception
/// the design allows — a holder that wants to explicitly mark activity
/// (e.g. a long-lived verify slot ahead of a GC run).
pub fn touch(slot: &Slot) -> Result<(), BuildCacheError> {
    if slot.lock.is_none()
        || read_holder(&slot.path).is_none_or(|meta| meta.claim_token != slot.claim_token)
    {
        return Ok(());
    }
    write_last_used(&slot.path, now_secs()).map_err(|e| {
        BuildCacheError::io(
            format!(
                "failed to write {}",
                slot.path.join(LAST_USED_LEAF).display()
            ),
            e,
        )
    })
}

/// Validate the upper bound and convert hours without arithmetic overflow.
/// A zero policy is retained for internal immediate-GC callers; the CLI
/// independently enforces its one-hour minimum.
pub fn stale_window_secs(hours: u64) -> Result<u64, BuildCacheError> {
    hours
        .checked_mul(3600)
        .filter(|_| hours <= MAX_STALE_HOURS)
        .ok_or_else(|| {
            BuildCacheError::InvalidConfig(format!(
                "stale_hours must be <= {MAX_STALE_HOURS} (ten years)"
            ))
        })
}

/// Walk every repo pool under `pool_root` and reclaim stale slots (§3.5 +
/// §6). For each slot:
///
/// 1. try `flock(EX|NB)` — unavailable means a live holder, skip;
/// 2. `holder.json` present → check pid liveness; dead → clear the
///    metadata (then staleness applies), alive → skip;
/// 3. no holder → compare `now - last_used` against the stale window
///    (missing `last_used` reads as 0, maximally stale);
/// 4. past the window → `remove_dir_all(target)` when `policy.apply`.
///
/// Red lines, verbatim from the design: NEVER judge staleness by directory
/// or file mtime; NEVER delete or re-create `.lock`. When anything about a
/// slot is uncertain (unreadable metadata, odd liveness errno), the walk
/// skips it — fail toward leaving cache alone, never toward deleting a
/// slot that may be in use.
pub fn reclaim_stale(
    pool_root: &Path,
    policy: &GcPolicy,
    config: &BuildCacheConfig,
) -> Result<Vec<ReclaimReport>, BuildCacheError> {
    stale_window_secs(policy.stale_hours)?;
    let mut reports = Vec::new();
    if is_symlink(pool_root) {
        return Ok(vec![skipped_report(pool_root.to_path_buf())]);
    }
    let entries = match fs::read_dir(pool_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(reports),
        Err(e) => {
            return Err(BuildCacheError::io(
                format!("failed to read pool root {}", pool_root.display()),
                e,
            ));
        }
    };
    let canonical_root = pool_root
        .canonicalize()
        .map_err(|e| BuildCacheError::io("failed to resolve GC pool root", e))?;
    for entry in entries.flatten() {
        let repo_dir = entry.path();
        // Only directories named like a pool repo dir (12-hex repo key);
        // unrelated content under the root is never touched.
        if super::repo_key::RepoKey::parse(&entry.file_name().to_string_lossy()).is_err() {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&repo_dir) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            reports.push(skipped_report(repo_dir));
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        // D3: enumerate the slot dirs that ACTUALLY exist (read_dir), not
        // merely 1..=configured count — a shrunk `peer_slots` config must not
        // make historical high-numbered slots invisible to GC forever.
        for kind in [SlotKind::Peer, SlotKind::Verify] {
            for dir in existing_slot_dirs(&repo_dir, kind, config) {
                let outcome = reclaim_one(&canonical_root, &dir, kind, policy)?;
                reports.push(ReclaimReport {
                    slot_path: dir,
                    outcome: outcome.0,
                    freed_bytes: outcome.1,
                    would_free_bytes: outcome.2,
                });
            }
        }
    }
    Ok(reports)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn skipped_report(slot_path: PathBuf) -> ReclaimReport {
    ReclaimReport {
        slot_path,
        outcome: ReclaimOutcome::Skipped,
        freed_bytes: 0,
        would_free_bytes: 0,
    }
}

/// Reclaim one slot dir, returning `(outcome, freed_bytes,
/// would_free_bytes)`. `would_free_bytes` is only ever non-zero where the
/// slot actually reached the reclaim arm — the bytes `--apply` would free
/// right now (identical to `freed_bytes` for `Reclaimed`).
fn reclaim_one(
    canonical_root: &Path,
    dir: &Path,
    _kind: SlotKind,
    policy: &GcPolicy,
) -> Result<(ReclaimOutcome, u64, u64), BuildCacheError> {
    // Check directory and bookkeeping entries without following links before
    // reading metadata or taking a lock through any of these paths.
    if is_symlink(dir)
        || [LOCK_LEAF, HOLDER_LEAF, LAST_USED_LEAF, TARGET_LEAF]
            .iter()
            .any(|leaf| is_symlink(&dir.join(leaf)))
    {
        return Ok((ReclaimOutcome::Skipped, 0, 0));
    }
    let canonical_dir = match dir.canonicalize() {
        Ok(path) if path.starts_with(canonical_root) && path != canonical_root => path,
        _ => return Ok((ReclaimOutcome::Skipped, 0, 0)),
    };
    let lock_path = dir.join(LOCK_LEAF);
    let lock = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No lock file: a slot dir without its mutex inode. Treat as
            // unlocked-but-suspicious and skip — we never create it here,
            // because doing so could hand a racing creator's mutex to a
            // reclaimer. Reported as its own outcome (D6), not "fresh".
            return Ok((ReclaimOutcome::NoLock, 0, 0));
        }
        Err(e) => {
            return Err(BuildCacheError::io(
                format!("failed to open {}", lock_path.display()),
                e,
            ));
        }
    };
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok((ReclaimOutcome::Locked, 0, 0)); // live holder
    }
    // Lock held for the duration of this check.
    let holder_path = dir.join(HOLDER_LEAF);
    let mut holder_cleared = false;
    if holder_path.exists() {
        let stale_holder = match fs::read_to_string(&holder_path) {
            Ok(text) => match serde_json::from_str::<HolderMeta>(&text) {
                Ok(meta) => pid_alive(meta.pid).is_none(),
                // Unparsable JSON: the file is ours to write and corrupt —
                // treat as stale rather than permanently leaking the slot.
                Err(_) => true,
            },
            // Unreadable — EACCES/EPERM means the file is (probably) not
            // ours to read, i.e. someone else owns this slot: skip (D4).
            Err(_) => false,
        };
        if !stale_holder {
            return Ok((ReclaimOutcome::Locked, 0, 0)); // holder alive
        }
        let _ = fs::remove_file(&holder_path); // dead holder: clear metadata
        holder_cleared = true;
        // §3.5: a dead holder demotes the slot to ownerless, which then
        // goes through the SAME §6 staleness check below — it is not
        // automatically reclaimable (the cache contents may still be
        // fresh). The stale clock keeps the slot's last_used, so a
        // recently-used but crashed slot survives this pass.
    }
    // No holder: staleness by last_used ONLY (never mtime).
    let last_used = fs::read_to_string(dir.join(LAST_USED_LEAF))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let age_secs = now_secs().saturating_sub(last_used);
    if age_secs <= stale_window_secs(policy.stale_hours)? {
        return Ok(if holder_cleared {
            (ReclaimOutcome::HolderCleared, 0, 0)
        } else {
            (ReclaimOutcome::Fresh, 0, 0)
        });
    }
    let target = dir.join(TARGET_LEAF);
    if !policy.apply || !target.exists() {
        // D1: report-only (or nothing left to delete) must still name the
        // slot as stale and carry the bytes --apply would free, so `octos
        // cache gc` output (and the outer loop consuming it in #6) can see
        // exactly what a subsequent --apply would reclaim.
        return Ok((ReclaimOutcome::Stale, 0, dir_size(&target)));
    }
    // Recheck immediately before removal and delete only a resolved target
    // still belonging to this slot and pool.
    if is_symlink(dir) || is_symlink(&target) {
        return Ok((ReclaimOutcome::Skipped, 0, 0));
    }
    let canonical_target = match target.canonicalize() {
        Ok(path)
            if path.starts_with(canonical_root)
                && path.starts_with(&canonical_dir)
                && path != canonical_dir =>
        {
            path
        }
        _ => return Ok((ReclaimOutcome::Skipped, 0, 0)),
    };
    let freed = dir_size(&canonical_target);
    fs::remove_dir_all(&canonical_target)
        .map_err(|e| BuildCacheError::io(format!("failed to remove {}", target.display()), e))?;
    Ok((ReclaimOutcome::Reclaimed, freed, freed))
}

/// Byte size of a directory tree (best-effort; symlink targets not
/// followed). Used only for reporting in the GC report.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        if !fs::symlink_metadata(&p).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let entries = match fs::read_dir(&p) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Read the holder metadata of a slot dir, for `octos cache status` (#5).
/// `Ok(None)` when there is no `holder.json` (unheld) or it is unreadable.
pub fn read_holder(slot_dir: &Path) -> Option<HolderMeta> {
    let text = fs::read_to_string(slot_dir.join(HOLDER_LEAF)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Read `last_used` (unix seconds) of a slot dir; `0` when missing or
/// unparsable (treated as maximally stale by the GC).
pub fn read_last_used(slot_dir: &Path) -> u64 {
    fs::read_to_string(slot_dir.join(LAST_USED_LEAF))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Slot dirs that ACTUALLY exist under `repo_dir` for one namespace, plus
/// the configured range, capped to slot-shaped names (`<prefix><number>`).
/// Used by reclaim/status/gc so a shrunk `*_slots` config still surfaces
/// (and reclaims) historical high-numbered slots (D3).
fn existing_slot_dirs(repo_dir: &Path, kind: SlotKind, config: &BuildCacheConfig) -> Vec<PathBuf> {
    let mut ns: Vec<u32> = Vec::new();
    // What actually exists on disk — the authoritative set for GC/status.
    if let Ok(entries) = fs::read_dir(repo_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(n) = kind.strip_prefix_of(name) else {
                continue;
            };
            if fs::symlink_metadata(entry.path())
                .is_ok_and(|metadata| metadata.is_dir() || metadata.file_type().is_symlink())
                && !ns.contains(&n)
            {
                ns.push(n);
            }
        }
    }
    // Fallback when read_dir itself failed: probe the configured range, so a
    // permissions hiccup degrades to the old behavior instead of blindness.
    for n in 1..=config.slot_count(kind) {
        if fs::symlink_metadata(slot_dir(repo_dir, kind, n))
            .is_ok_and(|metadata| metadata.is_dir() || metadata.file_type().is_symlink())
            && !ns.contains(&n)
        {
            ns.push(n);
        }
    }
    ns.sort_unstable();
    ns.dedup();
    ns.into_iter()
        .map(|n| slot_dir(repo_dir, kind, n))
        .collect()
}

/// Enumerate the slot dirs of one repo pool (for status output).
pub fn slot_dirs(repo_dir: &Path, config: &BuildCacheConfig) -> Vec<(SlotKind, PathBuf)> {
    let mut out = Vec::new();
    for kind in [SlotKind::Peer, SlotKind::Verify] {
        for dir in existing_slot_dirs(repo_dir, kind, config) {
            out.push((kind, dir));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_cache::repo_key_for_path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn config() -> BuildCacheConfig {
        BuildCacheConfig {
            peer_slots: 2,
            verify_slots: 1,
            min_free_gb: 0, // tests run on arbitrary disks; gate off
            stale_hours: 168,
        }
    }

    fn key(tmp: &tempfile::TempDir) -> RepoKey {
        repo_key_for_path(tmp.path()).unwrap()
    }

    #[test]
    fn acquire_creates_layout_and_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        assert!(slot.target_dir.is_dir());
        assert!(slot.path.join(LOCK_LEAF).is_file());
        assert!(slot.path.join(HOLDER_LEAF).is_file());
        assert!(slot.path.join(LAST_USED_LEAF).is_file());
        let meta = read_holder(&slot.path).unwrap();
        assert_eq!(meta.pid, std::process::id());
        assert_eq!(meta.kind, SlotKind::Peer);
    }

    #[test]
    fn release_is_idempotent_and_keeps_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let mut slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        fs::write(slot.target_dir.join("artifact.bin"), b"cached").unwrap();
        let path = slot.path.clone();
        release(&mut slot, SlotOutcome::Completed).unwrap();
        // Second release (the close-path safety net firing late) is a no-op.
        release(&mut slot, SlotOutcome::Retired).unwrap();
        assert!(!path.join(HOLDER_LEAF).exists());
        // I2: contents survive release for the next peer of this repo.
        assert!(path.join(TARGET_LEAF).join("artifact.bin").exists());
        assert!(path.join(LOCK_LEAF).exists());
    }

    #[test]
    fn slot_is_reusable_after_release() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let mut slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        let first = slot.path.clone();
        release(&mut slot, SlotOutcome::Completed).unwrap();
        let again = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        // Scan order is deterministic: the freed lowest-numbered slot is
        // re-taken first.
        assert_eq!(again.path, first);
    }

    #[test]
    fn detached_live_claims_use_distinct_slots_then_exhaust() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let cfg = BuildCacheConfig {
            verify_slots: 2,
            ..config()
        };
        let first = acquire_detached(&root, &key(&tmp), &cfg, &HolderInfo::default()).unwrap();
        let second = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Verify,
            &cfg,
            &HolderInfo::default(),
        )
        .unwrap();
        assert_ne!(first.path, second.path);
        assert!(matches!(
            acquire_detached(&root, &key(&tmp), &cfg, &HolderInfo::default()),
            Err(BuildCacheError::PoolExhausted { .. })
        ));
    }

    #[test]
    fn old_slot_release_preserves_new_claim_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let mut old = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        release(&mut old, SlotOutcome::Completed).unwrap();
        let new = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        assert_eq!(old.path, new.path);
        write_last_used(&new.path, 123).unwrap();
        let metadata = fs::read(new.path.join(HOLDER_LEAF)).unwrap();
        assert_eq!(
            release(&mut old, SlotOutcome::Completed).unwrap(),
            ReleaseDisposition::AlreadyReleased
        );
        assert_eq!(fs::read(new.path.join(HOLDER_LEAF)).unwrap(), metadata);
        assert_eq!(read_last_used(&new.path), 123);
        touch(&old).unwrap();
        assert_eq!(read_last_used(&new.path), 123);
    }

    #[test]
    fn old_detached_release_preserves_new_claim_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let old = acquire_detached(&root, &key(&tmp), &config(), &HolderInfo::default()).unwrap();
        release_detached(&root, &old.path, &old.claim_token, SlotOutcome::Completed).unwrap();
        let new = acquire_detached(&root, &key(&tmp), &config(), &HolderInfo::default()).unwrap();
        assert_eq!(old.path, new.path);
        write_last_used(&new.path, 123).unwrap();
        let metadata = fs::read(new.path.join(HOLDER_LEAF)).unwrap();
        assert_ne!(old.claim_token, new.claim_token);
        assert_eq!(
            release_detached(&root, &old.path, &old.claim_token, SlotOutcome::Completed).unwrap(),
            ReleaseDisposition::ClaimMismatch
        );
        assert_eq!(fs::read(new.path.join(HOLDER_LEAF)).unwrap(), metadata);
        assert_eq!(read_last_used(&new.path), 123);
    }

    #[cfg(unix)]
    fn gc_link_fixture(component: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source-pool");
        let mut source = acquire(
            &source_root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        release(&mut source, SlotOutcome::Completed).unwrap();
        write_last_used(&source.path, 0).unwrap();
        let artifact = source.target_dir.join("artifact");
        fs::write(&artifact, b"cached content").unwrap();
        let pool_root = tmp.path().join("pool");
        let repo = pool_root.join(key(&tmp).as_str());
        fs::create_dir_all(&pool_root).unwrap();
        let link = match component {
            "repo" => {
                std::os::unix::fs::symlink(source.path.parent().unwrap(), &repo).unwrap();
                repo
            }
            "slot" => {
                fs::create_dir_all(&repo).unwrap();
                let link = repo.join("slot-1");
                std::os::unix::fs::symlink(&source.path, &link).unwrap();
                link
            }
            "target" => {
                let mut slot = acquire(
                    &pool_root,
                    &key(&tmp),
                    SlotPurpose::Peer,
                    &config(),
                    &HolderInfo::default(),
                )
                .unwrap();
                release(&mut slot, SlotOutcome::Completed).unwrap();
                write_last_used(&slot.path, 0).unwrap();
                fs::remove_dir(&slot.target_dir).unwrap();
                std::os::unix::fs::symlink(&source.target_dir, &slot.target_dir).unwrap();
                slot.target_dir
            }
            _ => unreachable!(),
        };
        let reports = reclaim_stale(
            &pool_root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        assert!(artifact.is_file(), "original fixture content must remain");
        assert!(link.is_symlink(), "GC must leave symbolic links untouched");
        assert!(
            reports
                .iter()
                .any(|report| report.outcome.as_str() == "skipped"),
            "symbolic links must be reported as skipped: {reports:?}"
        );
        assert!(
            reports
                .iter()
                .all(|report| report.freed_bytes == 0 && report.would_free_bytes == 0)
        );
    }

    #[test]
    #[cfg(unix)]
    fn gc_skips_repo_symlink_fixture() {
        gc_link_fixture("repo");
    }

    #[test]
    #[cfg(unix)]
    fn gc_skips_slot_symlink_fixture() {
        gc_link_fixture("slot");
    }

    #[test]
    #[cfg(unix)]
    fn gc_skips_target_symlink_fixture() {
        gc_link_fixture("target");
    }

    #[test]
    fn numeric_excessive_stale_window_is_a_configuration_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let mut slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        release(&mut slot, SlotOutcome::Completed).unwrap();
        for hours in [10 * 366 * 24 + 1, u64::MAX] {
            let result = reclaim_stale(
                &root,
                &GcPolicy {
                    stale_hours: hours,
                    apply: false,
                },
                &config(),
            );
            assert!(
                result.is_err(),
                "invalid stale window {hours} must be rejected"
            );
        }
    }

    #[test]
    fn numeric_excessive_stale_config_is_reported() {
        let cfg = BuildCacheConfig {
            stale_hours: u64::MAX,
            ..config()
        };
        assert!(
            cfg.validate()
                .iter()
                .any(|warning| warning.contains("stale_hours"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn numeric_oversized_pid_is_conservatively_alive() {
        assert_eq!(pid_alive(u32::MAX), Some(false));
        assert_eq!(pid_alive(i32::MAX as u32 + 1), Some(false));
    }

    #[test]
    fn namespaces_do_not_share_slots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let peer = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        let verify = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Verify,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        assert!(peer.path.to_string_lossy().contains("slot-"));
        assert!(verify.path.to_string_lossy().contains("verify-"));
    }

    #[test]
    fn pool_exhaustion_is_typed_error_not_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let cfg = config();
        let mut held = Vec::new();
        for _ in 0..cfg.peer_slots {
            held.push(
                acquire(
                    &root,
                    &key(&tmp),
                    SlotPurpose::Peer,
                    &cfg,
                    &HolderInfo::default(),
                )
                .unwrap(),
            );
        }
        let err = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &cfg,
            &HolderInfo::default(),
        )
        .unwrap_err();
        match &err {
            BuildCacheError::PoolExhausted { kind, .. } => assert_eq!(*kind, SlotKind::Peer),
            other => panic!("expected PoolExhausted, got {other:?}"),
        }
        // Verify namespace still allocates — the point of two namespaces.
        acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Verify,
            &cfg,
            &HolderInfo::default(),
        )
        .unwrap();
    }

    #[test]
    fn concurrent_acquire_never_exceeds_pool_size() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Arc::new(tmp.path().join("pool"));
        let cfg = Arc::new(config());
        let key = Arc::new(key(&tmp));
        let holders: Arc<std::sync::Mutex<Vec<PathBuf>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let exhausted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let (root, cfg, key, holders, exhausted) = (
                Arc::clone(&root),
                Arc::clone(&cfg),
                Arc::clone(&key),
                Arc::clone(&holders),
                Arc::clone(&exhausted),
            );
            joins.push(std::thread::spawn(move || {
                for _ in 0..4 {
                    match acquire(&root, &key, SlotPurpose::Peer, &cfg, &HolderInfo::default()) {
                        Ok(slot) => holders.lock().unwrap().push(slot.path.clone()),
                        Err(BuildCacheError::PoolExhausted { .. }) => {
                            exhausted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(other) => panic!("unexpected error: {other}"),
                    }
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
        let held = holders.lock().unwrap();
        // I1: at most `peer_slots` DISTINCT slot dirs were handed out, even
        // though every thread tried to acquire repeatedly.
        let mut distinct: Vec<&PathBuf> = held.iter().collect();
        distinct.sort();
        distinct.dedup();
        assert!(
            distinct.len() <= cfg.peer_slots as usize,
            "handed out {} distinct slots, pool size {}",
            distinct.len(),
            cfg.peer_slots
        );
        // And the exhaustion was reported, not silently swallowed.
        assert!(exhausted.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn dead_holder_slot_is_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        fs::create_dir_all(slot.target_dir.join("deps")).unwrap();
        let dir = slot.path.clone();
        drop(slot); // simulate a crash: lock gone, holder.json remains

        // Forge a dead holder: spawn a real short-lived process, kill it,
        // and wait — the pid is then verifiably gone (kill -0 → ESRCH),
        // unlike a guessed constant that the kernel may recycle.
        let dead = spawn_dead_pid();
        assert_eq!(pid_alive(dead), None, "test seam must produce a dead pid");
        let meta = HolderMeta {
            kind: SlotKind::Peer,
            pid: dead,
            slug: None,
            goal_id: None,
            task_id: None,
            purpose_note: None,
            acquired_at: 1,
            claim_token: String::new(),
        };
        write_file_atomic(&dir, HOLDER_LEAF, &serde_json::to_string(&meta).unwrap()).unwrap();
        // Age it past the window by backdating last_used.
        write_last_used(&dir, 0).unwrap();

        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        assert!(
            report
                .iter()
                .any(|r| r.outcome == ReclaimOutcome::Reclaimed)
        );
        assert!(!dir.join(TARGET_LEAF).exists());
        assert!(
            dir.join(LOCK_LEAF).exists(),
            "the lock inode must survive GC"
        );
    }

    #[test]
    fn live_holder_is_never_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        fs::write(slot.target_dir.join("live.bin"), b"x").unwrap();
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 0,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        // Held by this very process: the flock is the truth.
        let unexpected: Vec<_> = report
            .iter()
            .filter(|r| r.outcome != ReclaimOutcome::Locked)
            .collect();
        assert!(
            unexpected.is_empty(),
            "held slot rows must all be locked, got {unexpected:?}"
        );
        assert!(slot.target_dir.join("live.bin").exists());
    }

    #[test]
    fn fresh_unheld_slot_is_not_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        fs::write(slot.target_dir.join("warm.bin"), b"x").unwrap();
        let dir = slot.path.clone();
        drop(slot);
        // A crashed-but-live holder would be (correctly) skipped by §3.5;
        // this test targets the pure §6 staleness arm, so clear ownership.
        fs::remove_file(dir.join(HOLDER_LEAF)).unwrap();
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 168, // just released: inside the window
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = report.iter().find(|r| r.slot_path == dir).unwrap();
        assert_eq!(row.outcome, ReclaimOutcome::Fresh);
        assert!(dir.join(TARGET_LEAF).join("warm.bin").exists());
    }

    #[test]
    fn gc_without_apply_only_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        // Put real bytes in target/ so the report can carry a non-zero
        // would-free size (an empty dir measures 0).
        fs::write(slot.target_dir.join("stale.bin"), vec![0u8; 4096]).unwrap();
        let dir = slot.path.clone();
        drop(slot);
        // Crash-simulation with the pid gone: clear the holder so the §6
        // arm runs (a live-pid holder would be skipped, as its own test
        // asserts).
        fs::remove_file(dir.join(HOLDER_LEAF)).unwrap();
        write_last_used(&dir, 0).unwrap();
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 1,
                apply: false,
            },
            &config(),
        )
        .unwrap();
        let row = report.iter().find(|r| r.slot_path == dir).unwrap();
        // Past the window, apply=false → Stale carrying the would-free bytes
        // (D1), never Reclaimed, and the tree survives.
        assert_eq!(row.outcome, ReclaimOutcome::Stale);
        assert!(row.would_free_bytes > 0, "would-free size must be carried");
        assert_eq!(row.freed_bytes, 0);
        assert!(dir.join(TARGET_LEAF).is_dir());
    }

    #[test]
    fn space_gate_low_space_is_typed_error_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        // A threshold no filesystem can satisfy.
        let cfg = BuildCacheConfig {
            min_free_gb: 100_000_000,
            ..config()
        };
        let err = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &cfg,
            &HolderInfo::default(),
        )
        .unwrap_err();
        match &err {
            BuildCacheError::FreeSpaceLow { min_gb, .. } => assert_eq!(*min_gb, 100_000_000),
            other => panic!("expected FreeSpaceLow, got {other:?}"),
        }
        // The refusal left no slot behind: the gate ran before allocation.
        let repo_dir = root.join(key(&tmp).as_str());
        assert!(!repo_dir.join(format!("{SLOT_PREFIX}1")).exists());
    }

    #[test]
    fn space_gate_zero_disables_the_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let cfg = BuildCacheConfig {
            min_free_gb: 0,
            ..config()
        };
        acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &cfg,
            &HolderInfo::default(),
        )
        .unwrap();
    }

    #[test]
    fn space_gate_unknown_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        // A path statvfs cannot answer for: the pool root's parent was
        // removed underneath us. Any ENOENT-style failure must surface as
        // FreeSpaceUnknown (refuse), never as "plenty of space".
        let gone = tmp.path().join("vanishing");
        fs::create_dir_all(&gone).unwrap();
        let probe = gone.join("pool");
        fs::remove_dir_all(&gone).unwrap();
        let err = check_free_space(&probe, 50).unwrap_err();
        match err {
            BuildCacheError::FreeSpaceUnknown { .. } => {}
            other => panic!("expected FreeSpaceUnknown, got {other:?}"),
        }
    }

    #[test]
    fn touch_updates_last_used() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        write_last_used(&slot.path, 1_000).unwrap();
        touch(&slot).unwrap();
        assert!(read_last_used(&slot.path) > 1_000);
    }

    #[test]
    fn holder_metadata_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo {
                slug: Some("kestrel".to_string()),
                goal_id: Some("g-1".to_string()),
                task_id: Some("t-2".to_string()),
                purpose_note: None,
                pid_override: None,
            },
        )
        .unwrap();
        let meta = read_holder(&slot.path).unwrap();
        assert_eq!(meta.slug.as_deref(), Some("kestrel"));
        assert_eq!(meta.goal_id.as_deref(), Some("g-1"));
        assert_eq!(meta.task_id.as_deref(), Some("t-2"));
    }

    #[test]
    fn config_defaults_match_the_design() {
        let cfg = BuildCacheConfig::default();
        assert_eq!(cfg.peer_slots, 2);
        assert_eq!(cfg.verify_slots, 1);
        assert_eq!(cfg.min_free_gb, 50);
        assert_eq!(cfg.stale_hours, 168);
        assert!(cfg.validate().is_empty());
        // Absent section parses to defaults (serde default, like snapshots).
        let parsed: BuildCacheConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn config_rejects_sub_floor_values() {
        let cfg = BuildCacheConfig {
            peer_slots: 0,
            verify_slots: 0,
            stale_hours: 0,
            ..BuildCacheConfig::default()
        };
        let warnings = cfg.validate();
        assert_eq!(warnings.len(), 3);
    }

    // ---- review #3 fixes (D1, D3, D4, D6) ----

    #[test]
    #[cfg(unix)]
    fn pid_alive_distinguishes_eperm_from_esrch() {
        // EPERM branch: pid 1 exists but a normal user may not signal it —
        // test_kill_process must surface EPERM as "alive but not ours",
        // which pid_alive encodes as Some(false), never None (dead).
        // (Running as root this returns Ok — Some(true) — which is still
        // "alive"; both readings keep the slot safe, but assert the EPERM
        // shape when we can observe it.)
        match pid_alive(1) {
            Some(false) | Some(true) => {} // alive either way: never reclaimed
            None => panic!("pid 1 must never be judged dead (EPERM/Ok both mean alive)"),
        }
        // ESRCH branch: a verifiably gone pid (spawned, killed, reaped).
        let dead = spawn_dead_pid();
        assert_eq!(pid_alive(dead), None);
        // Malformed pid (D1's EINVAL arm): 0 is not a valid signal target.
        assert_eq!(pid_alive(0), None);
    }

    #[test]
    #[cfg(unix)]
    fn foreign_live_holder_is_never_reclaimed() {
        // The D1 end-to-end guarantee: a holder.json naming a LIVE pid the
        // reclaimer cannot signal (root's pid 1) keeps the slot even when
        // the stale window has long passed.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        fs::write(slot.target_dir.join("foreign.bin"), b"x").unwrap();
        let dir = slot.path.clone();
        drop(slot); // unlock, metadata stays
        let meta = HolderMeta {
            kind: SlotKind::Peer,
            pid: 1, // alive, not ours (EPERM for an unprivileged reader)
            slug: None,
            goal_id: None,
            task_id: None,
            purpose_note: None,
            acquired_at: 1,
            claim_token: String::new(),
        };
        write_file_atomic(&dir, HOLDER_LEAF, &serde_json::to_string(&meta).unwrap()).unwrap();
        write_last_used(&dir, 0).unwrap(); // maximally stale
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 0,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = report.iter().find(|r| r.slot_path == dir).unwrap();
        assert_eq!(
            row.outcome,
            ReclaimOutcome::Locked,
            "live foreign pid must read as held"
        );
        assert!(dir.join(TARGET_LEAF).join("foreign.bin").exists());
    }

    #[test]
    fn shrunk_config_still_reclaims_historical_slots() {
        // D3: peer_slots was once 3; slot-3's dir survives the config
        // change and must stay visible to GC (status + reclaim).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let cfg3 = BuildCacheConfig {
            peer_slots: 3,
            ..config()
        };
        let slot3 = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &cfg3,
            &HolderInfo::default(),
        )
        .unwrap();
        // acquire takes the lowest slot, so forge the high one instead.
        let _ = slot3;
        let repo_dir = root.join(key(&tmp).as_str());
        let high = repo_dir.join(format!("{SLOT_PREFIX}3"));
        fs::create_dir_all(high.join(TARGET_LEAF)).unwrap();
        File::create(high.join(LOCK_LEAF)).unwrap();
        write_last_used(&high, 0).unwrap();
        // Now the config is back to 2 slots — slot-3 must still be listed
        // by status and reclaimed by gc.
        assert!(
            slot_dirs(&repo_dir, &config())
                .iter()
                .any(|(_, d)| d == &high)
        );
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        assert!(
            report
                .iter()
                .any(|r| r.slot_path == high && r.outcome == ReclaimOutcome::Reclaimed)
        );
        assert!(!high.join(TARGET_LEAF).exists());
    }

    #[test]
    fn slot_without_lock_reports_no_lock_not_fresh() {
        // D6: a structurally broken slot (no .lock) must not be labeled
        // "fresh" — it is its own outcome, and it is never deleted.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        let dir = slot.path.clone();
        drop(slot);
        fs::remove_file(dir.join(LOCK_LEAF)).unwrap();
        write_last_used(&dir, 0).unwrap();
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 0,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = report.iter().find(|r| r.slot_path == dir).unwrap();
        assert_eq!(row.outcome, ReclaimOutcome::NoLock);
        assert!(dir.join(TARGET_LEAF).is_dir(), "no_lock never deletes");
    }

    #[test]
    fn unreadable_holder_json_is_skipped_not_stale() {
        // D4: an EACCES on holder.json means the slot is probably someone
        // else's — conservative skip, not "stale holder".
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pool");
        let slot = acquire(
            &root,
            &key(&tmp),
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        let dir = slot.path.clone();
        drop(slot);
        let holder = dir.join(HOLDER_LEAF);
        let meta = HolderMeta {
            kind: SlotKind::Peer,
            pid: spawn_dead_pid(),
            slug: None,
            goal_id: None,
            task_id: None,
            purpose_note: None,
            acquired_at: 1,
            claim_token: String::new(),
        };
        write_file_atomic(&dir, HOLDER_LEAF, &serde_json::to_string(&meta).unwrap()).unwrap();
        // chmod 000: unreadable to everyone (root sees through it; the
        // assertion below holds for both the skip and the root case).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&holder, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let report = reclaim_stale(
            &root,
            &GcPolicy {
                stale_hours: 0,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = report.iter().find(|r| r.slot_path == dir).unwrap();
        // Either skipped-as-held (unprivileged: EACCES→skip) or cleared
        // (root reads through 0000 and finds the dead pid). Both are safe;
        // neither may have deleted the target.
        assert!(matches!(
            row.outcome,
            ReclaimOutcome::Locked | ReclaimOutcome::HolderCleared
        ));
    }
}
