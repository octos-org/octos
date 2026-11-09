//! Flag-gated windowed `read_file` enforcement (#1638). **Off by default.**
//!
//! Armed via `OCTOS_READ_WINDOW=1`. When armed, `read_file` returns at most
//! [`WINDOW_MAX_LINES`] lines and at most [`WINDOW_MAX_BYTES`] bytes of
//! formatted output — whichever limit is hit first — with a footer naming the
//! limit that fired, the range actually returned, the file's totals, and the
//! exact next call. A `byte_offset`/`byte_limit` raw mode pages content that
//! line offsets cannot reach (single lines larger than the window). Unarmed
//! behaviour is byte-identical to before this module existed.
//!
//! ## Parameter provenance
//!
//! The 2000-line half is the `pi` harness's field-tested default, verbatim
//! (`truncate.ts: DEFAULT_MAX_LINES = 2000`). An earlier 500-line/24KiB
//! proposal was rejected as too aggressive — 57.4% of this repo's files are
//! ≤500 lines, so ~43% of reads would have paged.
//!
//! The byte half adapts pi's 50KB (`DEFAULT_MAX_BYTES = 50 * 1024`) to two
//! local facts pi does not have:
//!
//! 1. Our output carries a line-number gutter, so the bytes that actually
//!    enter the context are the FORMATTED bytes, and that is what this budget
//!    counts (the execution loop's cap counts `String::len()` of the same
//!    string).
//! 2. That loop cap — `octos_core::tool_output_limit("read_file")` = 50,000 —
//!    is a blind head/tail backstop (#2124). The whole point of an advising
//!    window is that the tool's own cut is the ONLY cut, so the window plus
//!    its footer must provably fit under the loop cap. 50 * 1024 = 51,200
//!    does not. 48 KiB (49,152) plus a [`FOOTER_RESERVE`]-byte footer does.
//!
//! A tripwire test pins `WINDOW_MAX_BYTES + FOOTER_RESERVE <=
//! tool_output_limit("read_file")` so lowering the loop budget cannot
//! silently re-expose windowed reads to the blind backstop, and every armed
//! return path is clamped to `WINDOW_MAX_BYTES + FOOTER_RESERVE` at RUNTIME
//! (not a `debug_assert`, which release builds drop). None of the armed
//! returns interpolates unbounded caller input — path spellings can be
//! arbitrarily long, so model-facing messages clamp them.
//!
//! ## Why raw byte paging instead of a shell fallback
//!
//! pi's answer to a line larger than the window is a `sed | head -c` shell
//! fallback. That is self-defeating under OUR constraints, twice over: the
//! shell tool's own output cap is 30,000 bytes
//! (`octos_core::tool_output_limit("shell")`), so the advised `head -c 49152`
//! can never arrive intact; and the loop sanitizer redacts exactly the
//! content giant lines are made of (base64 data URIs, long hex —
//! `sanitize.rs`), so what survives the cap is then redacted. Hence the
//! in-tool `byte_offset`/`byte_limit` mode: raw slices, no gutter, footer
//! naming the next `byte_offset`, and the bytes count toward the same
//! coverage ledger as line reads.
//!
//! ## The view ledger: fail-closed overwrite protection
//!
//! The second half of this module tracks, per `(session, path)`, how much of
//! the file the model has actually been shown — so `write_file` (armed) can
//! refuse to reconstruct a file wholesale from a view the model does not
//! have. The slides workflow mandates exactly read → rebuild → `write_file`,
//! and its prompt forbids the patch tools, so this guard is the only thing
//! between a windowed read and a truncated script.
//!
//! The rule is **fail-closed** (codex review of the first draft, which was
//! fail-open on four axes): overwriting an EXISTING file larger than
//! [`WINDOW_MAX_BYTES`] requires a COMPLETE mark from THIS session; no entry
//! means refuse-and-read-first. Absence being the refusing state makes a
//! process restart safe by construction (the model must re-read, which is
//! correct anyway), makes bounded eviction safe (evicting can only cause a
//! re-read, never an unseen overwrite), and closes the giant-first-line hole
//! (the advice branch records nothing, and nothing now means NO). Files at
//! or under the byte window can still be blind-overwritten exactly as today
//! — a file that size is returned whole by a single unbounded read, so there
//! is no partial-view illusion to protect against; but once ANY armed read
//! recorded a view of it, that record is honoured (a partial or tainted or
//! stale view refuses regardless of size).
//!
//! Coverage is tracked in BYTES — one coordinate system for line-mode reads
//! (converted to their byte spans) and raw byte-mode reads, so paging past a
//! giant line stitches naturally. The mark is a contiguous-from-byte-0
//! high-water line keyed by an epoch of `(mtime, size, ctime, inode)` on Unix
//! (`(mtime, size)` off-Unix): any epoch change resets coverage to the new
//! read alone, and `write_file` re-validates the recorded epoch against the
//! CURRENT file at write time, so a file replaced after reading refuses as
//! stale instead of trusting dead coverage. A same-mtime-same-size IN-PLACE
//! rewrite IS detected on Unix — ctime advances on any write and userspace
//! cannot roll it back — and a rename-over swap is detected by the inode.
//! (Off-Unix the scheme falls back to `(mtime, size)`, a documented weaker
//! guarantee for this Unix-primary flag.)
//!
//! A view only counts if the model actually received the bytes: the recorded
//! output is checked against the SAME sanitizer the execution loop applies
//! afterwards — the credentials-only scrub for file-content tools (T-0380),
//! the full `sanitize_tool_output` pipeline otherwise — and a view the
//! sanitizer would alter is recorded TAINTED: the epoch can then never reach
//! COMPLETE, because a whole-file rewrite would replace redacted content
//! with redaction placeholders. Reads that fit no ledger (metadata
//! unavailable) record an epoch-less entry that likewise never completes.
//!
//! The ledger is a process-global map, NOT threaded through `ToolContext`
//! state: the guard is a data-loss check and must hold on every entry path,
//! including legacy `Tool::execute` calls that carry a zero context (the
//! `FileStateCache` on `ToolContext` is optional and absent on those paths,
//! which is why it was not reused). Entries are keyed by
//! `(parent_session_key, canonical path)`: one session's COMPLETE never
//! authorizes another's, and canonicalization keeps `read_file` and
//! `write_file` agreeing over path aliases (`/var` vs `/private/var`). The map
//! is bounded at [`MAX_LEDGER_ENTRIES`] (oldest-recorded evicted first — safe,
//! because absence refuses).
//!
//! R4 (codex round 3): a missing/empty session key is reachable in production
//! (agents default to `None`; FFI and fleet workers; plain CLI chat without
//! `--goals`), so an empty key must NOT share a bucket with another keyless
//! task. Empty-session reads and writes therefore record NOTHING — a keyless
//! task can never establish COMPLETE, so the write guard fails closed for its
//! over-window overwrites (`record_view`/`note_full_write` are the single
//! enforcement point; the guard adds only a clearer keyless message). This is
//! deliberately NOT the #2126 probe's `LAST_READ` map — that one is
//! observe-only, keyed to the probe's own env flag and 500/24KiB canary, and
//! its per-path counters are `cfg(test)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Line half of the armed window: pi's field-tested default, verbatim.
pub(crate) const WINDOW_MAX_LINES: usize = 2000;

/// Byte half of the armed window, counted on FORMATTED output bytes.
///
/// 48 KiB, not pi's 50 KiB — see the module docs: the window plus its footer
/// must provably fit under the execution loop's 50,000-byte `read_file` cap
/// so the blind backstop (#2124) can never fire on an armed read.
pub(crate) const WINDOW_MAX_BYTES: usize = 48 * 1024;

/// Bytes reserved above [`WINDOW_MAX_BYTES`] for the window footer.
pub(crate) const FOOTER_RESERVE: usize = 400;

/// Ledger capacity. Eviction is oldest-recorded-first and SAFE by the
/// fail-closed rule: a missing entry refuses an over-window overwrite (one
/// re-read), it can never authorize one.
pub(crate) const MAX_LEDGER_ENTRIES: usize = 512;

/// Which window limit clamped an armed read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClamp {
    /// [`WINDOW_MAX_LINES`] fired first.
    Lines,
    /// [`WINDOW_MAX_BYTES`] fired first.
    Bytes,
}

/// Typed prefix on the armed `write_file` refusal, so the model (and any
/// harness) can match it structurally — same convention as
/// `[FILE_UNCHANGED]`.
pub(crate) const PARTIAL_VIEW_OVERWRITE_PREFIX: &str = "[PARTIAL_VIEW_OVERWRITE]";

/// Typed prefix for the DISTINCT tainted-overwrite refusal. A tainted view is
/// not a "read more" case — the loop sanitizer redacts every raw byte page
/// too, so paging can never deliver the bytes whole — so it gets its own
/// prefix and a message that says the flag is incompatible with rewriting
/// THIS file, rather than the partial-view "page through it" advice.
pub(crate) const REDACTED_VIEW_OVERWRITE_PREFIX: &str = "[REDACTED_VIEW_OVERWRITE]";

/// Typed prefix for the DISTINCT transformed-view refusal (#2193 R4): the
/// model was shown TEXT decoded from a binary (PDF extraction), never the
/// file's own bytes, so — like a redacted view — paging can never deliver the
/// bytes whole. Its own prefix and a message that says so.
pub(crate) const TRANSFORMED_VIEW_OVERWRITE_PREFIX: &str = "[TRANSFORMED_VIEW_OVERWRITE]";

/// Whether window enforcement is armed by the environment.
///
/// Mirrors the #2126 probe's gate shape (`OCTOS_READ_PAGING_PROBE=1`). The
/// test-side arming override is per-tool-instance
/// (`with_window_enforcement`), NOT a process-global like the probe's
/// `FORCED_ON`: arming CHANGES `read_file`'s output, so a global test switch
/// would leak windowed behaviour into every unarmed test running in parallel
/// (`set_var` is `unsafe` under edition 2024 and this workspace denies
/// unsafe, so tests cannot scope the env var either).
pub(crate) fn armed_from_env() -> bool {
    std::env::var("OCTOS_READ_WINDOW").is_ok_and(|value| value == "1")
}

/// The identity of one on-disk generation of a file (#2193 R4).
///
/// mtime and size alone are NOT identity — a same-size replacement that forges
/// mtime (`utimensat`) compares equal and would authorize an overwrite of
/// content the model never saw. So the epoch also binds:
/// - `ctime`: the inode change time. Userspace has no API to set it backwards;
///   any in-place rewrite bumps it to "now". This defeats the same-size,
///   same-mtime replacement outright on every modern filesystem (nanosecond
///   ctime); only a coarse-granularity FS leaves a sub-tick residual.
/// - `inode`: the inode number, so a rename-over swap to a different inode with
///   the same (mtime, size) is a distinct generation and is refused.
///
/// Both are `None` off Unix (where the windowed-read flag is secondary and the
/// sandbox degrades anyway); there the guarantee falls back to (mtime, size).
/// Build via [`ViewEpoch::from_metadata`], ideally from an ALREADY-OPEN
/// descriptor's `fstat`, so the epoch describes the exact inode whose bytes
/// were read rather than a racy re-resolution of the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewEpoch {
    pub(crate) mtime: SystemTime,
    pub(crate) size: u64,
    pub(crate) ctime: Option<SystemTime>,
    pub(crate) inode: Option<u64>,
}

impl ViewEpoch {
    /// Build the epoch from a file's metadata — pass the metadata of an
    /// already-open descriptor (`file.metadata()`) so it binds to that exact
    /// inode, not a re-resolved path.
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Option<ViewEpoch> {
        let mtime = meta.modified().ok()?;
        // `None` fails closed: no epoch => the view never completes and the
        // checked writers refuse. On Unix that happens only when ctime is not
        // representable (below); off-Unix ctime/inode are legitimately absent.
        let (ctime, inode) = ctime_and_inode(meta)?;
        Some(ViewEpoch {
            mtime,
            size: meta.len(),
            ctime,
            inode,
        })
    }
}

#[cfg(unix)]
fn ctime_and_inode(meta: &std::fs::Metadata) -> Option<(Option<SystemTime>, Option<u64>)> {
    use std::os::unix::fs::MetadataExt;
    // #2193 R4 (codex round 4): a NEGATIVE / unrepresentable ctime must FAIL
    // CLOSED, not degrade to `None`. If it degraded, two in-place generations
    // with unrepresentable ctimes would both carry `ctime: None` and compare
    // equal on (mtime, size, inode) after mtime forgery — the very hole ctime
    // was added to close. Returning `None` here makes `from_metadata` yield no
    // epoch, so such a file can never authorize a windowed overwrite.
    let secs = u64::try_from(meta.ctime()).ok()?;
    let nsecs = meta.ctime_nsec().clamp(0, 999_999_999) as u32;
    let ctime = SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, nsecs);
    Some((Some(ctime), Some(meta.ino())))
}

#[cfg(not(unix))]
fn ctime_and_inode(_meta: &std::fs::Metadata) -> Option<(Option<SystemTime>, Option<u64>)> {
    // Off-Unix: ctime/inode are not available; the documented weaker
    // (mtime, size) fallback applies (the flag is Unix-primary).
    Some((None, None))
}

/// What the ledger knows about one `(session, path)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewStatus {
    /// Never read in this session (or evicted / restarted): the fail-closed
    /// default for over-window files.
    Unknown,
    /// Some bytes seen, contiguously from 0 up to `seen_through` (exclusive).
    Partial {
        seen_through: usize,
        total_bytes: usize,
    },
    /// Bytes were redacted by the sanitizer before reaching the model; no
    /// amount of further paging makes a faithful whole-file rewrite possible.
    Tainted,
    /// The bytes shown were a DECODE of the on-disk file (e.g. PDF text
    /// extraction), not the file's own bytes. Like `Tainted`, paging can never
    /// deliver the on-disk bytes whole, so a faithful whole-file rewrite is
    /// impossible — but for a different reason (transform, not redaction), so
    /// it gets its own status and write-side message.
    Transformed,
    /// Every byte of this epoch reached the model unredacted.
    Complete { epoch: ViewEpoch },
}

/// One `(session, path)`'s record.
#[derive(Clone, Copy, Debug)]
struct ViewRecord {
    /// Epoch coverage was accumulated under; `None` = metadata was
    /// unavailable at read time, which can never validate at write time and
    /// therefore never completes.
    epoch: Option<ViewEpoch>,
    /// Bytes `0..seen_through` have been shown under `epoch`.
    seen_through: usize,
    /// Raw content length when last read.
    total_bytes: usize,
    /// Sticky per epoch: some shown bytes were altered by the sanitizer.
    tainted: bool,
    /// Sticky per epoch: the shown bytes were a decode of the on-disk file
    /// (PDF extraction), never the file's own bytes.
    transformed: bool,
}

#[derive(Default)]
struct Ledger {
    entries: HashMap<(String, PathBuf), ViewRecord>,
    /// Insertion/update order, oldest first, for bounded eviction.
    order: Vec<(String, PathBuf)>,
}

impl Ledger {
    /// Move `key` to the freshest end of the order and evict the oldest
    /// entries past [`MAX_LEDGER_ENTRIES`]. Eviction is safe by the
    /// fail-closed rule: absence refuses.
    fn touch_and_evict(&mut self, key: (String, PathBuf)) {
        self.order.retain(|existing| existing != &key);
        self.order.push(key);
        while self.entries.len() > MAX_LEDGER_ENTRIES {
            if self.order.is_empty() {
                // entries/order desynchronized — clear rather than loop.
                self.entries.clear();
                break;
            }
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }
}

/// See the module docs for why this is process-global rather than
/// `ToolContext`-threaded.
static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

/// The key both sides of the ledger agree on.
///
/// `read_file` and `write_file` can resolve the same file to different
/// spellings (macOS tempdirs alone alias `/var` to `/private/var`); if they
/// keyed the raw spellings the guard would silently never match. Falls back
/// to the raw path when canonicalization fails (file deleted between check
/// and record) — both sides then fail the same way.
fn ledger_key(session: &str, path: &Path) -> (String, PathBuf) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    (session.to_string(), canonical)
}

/// Record one armed `read_file` view: bytes `byte_start..byte_end`
/// (half-open) of a `total_bytes` file at `epoch`, `tainted` when the
/// sanitizer would have altered the returned output.
///
/// Same-epoch views extend the contiguous-from-0 high-water mark and OR
/// their taint; an epoch change (or `None` epoch) resets coverage to this
/// view alone. Callers gate on arming — this function itself does not
/// consult the env, so tests can drive it through per-instance-armed tools
/// without touching process state.
// Eight cohesive facts about one read (who, which file+generation, the
// byte span shown, and the two INDEPENDENT non-authorizing flags — a view
// can be both sanitizer-redacted AND a binary decode). Bundling them would
// obscure more than it saves.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_view(
    session: &str,
    path: &Path,
    epoch: Option<ViewEpoch>,
    byte_start: usize,
    byte_end: usize,
    total_bytes: usize,
    tainted: bool,
    transformed: bool,
) {
    // R4: a missing/empty session key must never share a bucket with another
    // keyless task, so keyless tasks record NOTHING — they can then never
    // reach COMPLETE, and the write guard fails closed for their big-file
    // overwrites. This is the single enforcement point for that rule.
    if session.is_empty() {
        return;
    }
    let key = ledger_key(session, path);
    let mut guard = LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ledger = guard.get_or_insert_with(Ledger::default);
    // Coverage and taint carry over only within one known epoch; `None`
    // epochs never match (unknown is not evidence).
    let (carried, carried_taint, carried_transformed) = match ledger.entries.get(&key) {
        Some(prev) if prev.epoch.is_some() && prev.epoch == epoch => {
            (prev.seen_through, prev.tainted, prev.transformed)
        }
        _ => (0, false, false),
    };
    // Half-open [byte_start, byte_end): contiguous means starting at or
    // before the high-water mark; a view past it leaves a gap and the mark
    // stays.
    let seen_through = if byte_start <= carried {
        carried.max(byte_end)
    } else {
        carried
    };
    ledger.entries.insert(
        key.clone(),
        ViewRecord {
            epoch,
            seen_through,
            total_bytes,
            tainted: carried_taint || tainted,
            transformed: carried_transformed || transformed,
        },
    );
    ledger.touch_and_evict(key);
}

/// What this session knows about `path`.
pub(crate) fn view_status(session: &str, path: &Path) -> ViewStatus {
    let key = ledger_key(session, path);
    let guard = LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(record) = guard.as_ref().and_then(|ledger| ledger.entries.get(&key)) else {
        return ViewStatus::Unknown;
    };
    if record.tainted {
        return ViewStatus::Tainted;
    }
    if record.transformed {
        return ViewStatus::Transformed;
    }
    match record.epoch {
        // Full contiguous coverage of a validatable generation.
        Some(epoch) if record.seen_through >= record.total_bytes => ViewStatus::Complete { epoch },
        // An epoch-less view can never be validated at write time, so full
        // coverage of it still reports (and refuses as) Partial.
        _ => ViewStatus::Partial {
            seen_through: record.seen_through,
            total_bytes: record.total_bytes,
        },
    }
}

/// Note a successful, unformatted whole-file `write_file`: the on-disk
/// content is now exactly what the model supplied, so the session's view of
/// it is COMPLETE at the post-write epoch. (The fail-closed rule would
/// otherwise refuse the model's own next overwrite of a big file it just
/// authored.) When the post-write stat fails, the entry is forgotten —
/// absence refuses, which is the safe direction.
pub(crate) fn note_full_write(session: &str, path: &Path, written_bytes: usize) {
    // R4: keyless tasks never establish coverage (see record_view).
    if session.is_empty() {
        return;
    }
    let epoch = std::fs::metadata(path)
        .ok()
        .and_then(|meta| ViewEpoch::from_metadata(&meta));
    match epoch {
        // R1 second half: only mark COMPLETE when the bytes on disk are the
        // bytes we wrote. A size disagreement means a replacement landed in
        // the write->stat window; recording COMPLETE then would authorize a
        // later overwrite of content the model never produced. Forget instead.
        Some(epoch) if epoch.size == written_bytes as u64 => {
            let key = ledger_key(session, path);
            let mut guard = LEDGER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let ledger = guard.get_or_insert_with(Ledger::default);
            ledger.entries.insert(
                key.clone(),
                ViewRecord {
                    epoch: Some(epoch),
                    seen_through: written_bytes,
                    total_bytes: written_bytes,
                    tainted: false,
                    transformed: false,
                },
            );
            ledger.touch_and_evict(key);
        }
        // Stat failed, or on-disk size disagrees with what we wrote: nothing
        // validatable to record — forget, because absence refuses.
        _ => forget(session, path),
    }
}

/// Forget one `(session, path)` — used when the on-disk content diverged
/// from what the model supplied (post-write formatter rewrote the file).
pub(crate) fn forget(session: &str, path: &Path) {
    let key = ledger_key(session, path);
    let mut guard = LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(ledger) = guard.as_mut() {
        ledger.entries.remove(&key);
        ledger.order.retain(|existing| existing != &key);
    }
}
