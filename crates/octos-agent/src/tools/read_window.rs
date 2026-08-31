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
//! high-water line keyed by an epoch of `(mtime, size)`: any epoch change
//! resets coverage to the new read alone, and `write_file` re-validates the
//! recorded epoch against the CURRENT file at write time, so a file replaced
//! after reading refuses as stale instead of trusting dead coverage.
//! Same-mtime-same-size replacement is not detectable by this scheme (no
//! content hash in the epoch); it is also not detectable by the M8.4
//! file-state cache today.
//!
//! A view only counts if the model actually received the bytes: the recorded
//! output is checked against `sanitize_tool_output` (the exact function the
//! execution loop applies afterwards), and a view the sanitizer would alter
//! is recorded TAINTED — the epoch can then never reach COMPLETE, because a
//! whole-file rewrite would replace redacted content with redaction
//! placeholders. Reads that fit no ledger (metadata unavailable) record an
//! epoch-less entry that likewise never completes.
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

/// The identity of one on-disk generation of a file.
///
/// mtime alone is not identity — same-second replacement preserves it on
/// coarse filesystems — so size is checked too. (A same-mtime same-size
/// rewrite still passes; closing that needs a content hash, which the M8.4
/// cache also does not spend on this.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewEpoch {
    pub(crate) mtime: SystemTime,
    pub(crate) size: u64,
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
pub(crate) fn record_view(
    session: &str,
    path: &Path,
    epoch: Option<ViewEpoch>,
    byte_start: usize,
    byte_end: usize,
    total_bytes: usize,
    tainted: bool,
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
    let (carried, carried_taint) = match ledger.entries.get(&key) {
        Some(prev) if prev.epoch.is_some() && prev.epoch == epoch => {
            (prev.seen_through, prev.tainted)
        }
        _ => (0, false),
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
    let epoch = std::fs::metadata(path).ok().and_then(|meta| {
        meta.modified().ok().map(|mtime| ViewEpoch {
            mtime,
            size: meta.len(),
        })
    });
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

/// Drop every entry of one session — simulates a process restart for that
/// session in tests. (The real restart clears all sessions at once, but a
/// whole-ledger clear from one test would wipe parallel tests' entries
/// mid-flight — the same cross-test blast radius that rules out global
/// arming. Restart semantics are per-entry absence, which this preserves
/// exactly for the session under test.)
#[cfg(test)]
pub(crate) fn reset_session_for_test(session: &str) {
    let mut guard = LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(ledger) = guard.as_mut() {
        ledger.entries.retain(|key, _| key.0 != session);
        ledger.order.retain(|key| key.0 != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // The ledger is process-global, keyed by (session, path); each test uses
    // its own tempdir paths and its own session strings, so parallel tests
    // never observe each other (#2077/#2126: never assert on process-global
    // aggregates).

    fn epoch_at(secs_ago: u64, size: u64) -> Option<ViewEpoch> {
        Some(ViewEpoch {
            mtime: SystemTime::now() - Duration::from_secs(secs_ago),
            size,
        })
    }

    #[test]
    fn window_plus_footer_must_fit_under_the_loop_backstop() {
        // The whole point of an advising window is that the tool's own cut
        // is the ONLY cut. If someone lowers the loop's read_file budget
        // below the window, the blind #2124 backstop starts firing on armed
        // reads again and mangling footers — this tripwire makes that a
        // test failure instead of a silent regression.
        assert!(
            WINDOW_MAX_BYTES + FOOTER_RESERVE <= octos_core::tool_output_limit("read_file"),
            "WINDOW_MAX_BYTES ({WINDOW_MAX_BYTES}) + FOOTER_RESERVE ({FOOTER_RESERVE}) must fit \
             under tool_output_limit(\"read_file\") ({})",
            octos_core::tool_output_limit("read_file")
        );
    }

    #[test]
    fn should_report_partial_after_a_window_and_complete_after_paging_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paged.txt");
        std::fs::write(&path, "x").unwrap();
        let epoch = epoch_at(0, 5000);

        record_view("s", &path, epoch, 0, 2000, 5000, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Partial {
                seen_through: 2000,
                total_bytes: 5000
            },
            "a windowed view is partial"
        );

        record_view("s", &path, epoch, 2000, 4000, 5000, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Partial {
                seen_through: 4000,
                total_bytes: 5000
            },
            "contiguous pages extend the high-water mark"
        );

        record_view("s", &path, epoch, 4000, 5000, 5000, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Complete {
                epoch: epoch.unwrap()
            },
            "paging through to EOF completes the view — the refusal's advice \
             must be truthful"
        );
    }

    #[test]
    fn should_not_stitch_across_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gappy.txt");
        std::fs::write(&path, "x").unwrap();
        let epoch = epoch_at(0, 5000);

        record_view("s", &path, epoch, 0, 2000, 5000, false);
        record_view("s", &path, epoch, 2500, 5000, 5000, false); // bytes 2000..2500 never seen
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Partial {
                seen_through: 2000,
                total_bytes: 5000
            },
            "a gap means the view is still partial at the gap"
        );
    }

    #[test]
    fn should_reset_coverage_when_the_epoch_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edited.txt");
        std::fs::write(&path, "x").unwrap();

        record_view("s", &path, epoch_at(60, 5000), 0, 2000, 5000, false);
        // Same mtime second, DIFFERENT size — still a different epoch.
        record_view("s", &path, epoch_at(60, 6000), 2000, 4000, 6000, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Partial {
                seen_through: 0,
                total_bytes: 6000
            },
            "coverage accumulated under a different (mtime, size) must not \
             survive — same-second replacement included"
        );
    }

    #[test]
    fn should_never_complete_a_tainted_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.txt");
        std::fs::write(&path, "x").unwrap();
        let epoch = epoch_at(0, 100);

        record_view("s", &path, epoch, 0, 50, 100, true); // sanitizer altered this view
        record_view("s", &path, epoch, 50, 100, 100, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Tainted,
            "full coverage with redacted bytes is NOT a faithful view — \
             taint is sticky for the epoch"
        );

        // A new epoch starts clean.
        record_view("s", &path, epoch_at(0, 101), 0, 101, 101, false);
        assert!(
            matches!(view_status("s", &path), ViewStatus::Complete { .. }),
            "an untainted re-read of a new epoch completes"
        );
    }

    #[test]
    fn should_never_complete_without_an_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_meta.txt");
        std::fs::write(&path, "x").unwrap();

        record_view("s", &path, None, 0, 100, 100, false);
        assert_eq!(
            view_status("s", &path),
            ViewStatus::Partial {
                seen_through: 100,
                total_bytes: 100
            },
            "an epoch-less view can never be validated at write time, so it \
             must never report Complete"
        );
    }

    #[test]
    fn should_record_nothing_for_an_empty_session() {
        // R4 single enforcement point for isolation: a keyless task (empty
        // session key) must record NOTHING, so two keyless tasks can never
        // cross-authorize via a shared "" bucket. Mutating the empty-session
        // guard in record_view/note_full_write makes this fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyless.txt");
        std::fs::write(&path, "0123456789").unwrap();

        record_view("", &path, epoch_at(0, 10), 0, 10, 10, false);
        assert_eq!(
            view_status("", &path),
            ViewStatus::Unknown,
            "an empty-session read must leave no trace — else two keyless \
             tasks share a bucket"
        );

        note_full_write("", &path, 10);
        assert_eq!(
            view_status("", &path),
            ViewStatus::Unknown,
            "an empty-session write must leave no COMPLETE mark either"
        );
    }

    #[test]
    fn should_scope_views_to_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scoped.txt");
        std::fs::write(&path, "x").unwrap();

        record_view("session-a", &path, epoch_at(0, 10), 0, 10, 10, false);
        assert!(matches!(
            view_status("session-a", &path),
            ViewStatus::Complete { .. }
        ));
        assert_eq!(
            view_status("session-b", &path),
            ViewStatus::Unknown,
            "one session's COMPLETE must never vouch for another session"
        );
    }

    #[test]
    fn should_forget_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgotten.txt");
        std::fs::write(&path, "x").unwrap();

        record_view("s", &path, epoch_at(0, 10), 0, 5, 10, false);
        assert!(matches!(
            view_status("s", &path),
            ViewStatus::Partial { .. }
        ));
        forget("s", &path);
        assert_eq!(view_status("s", &path), ViewStatus::Unknown);
    }

    #[test]
    fn should_mark_complete_after_note_full_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authored.txt");
        std::fs::write(&path, "0123456789").unwrap();

        note_full_write("s", &path, 10);
        match view_status("s", &path) {
            ViewStatus::Complete { epoch } => {
                assert_eq!(epoch.size, 10, "epoch is the post-write stat");
            }
            other => panic!(
                "the model authored every byte it just wrote — must be \
                 Complete, got {other:?}"
            ),
        }
    }

    #[test]
    fn should_not_mark_complete_when_written_bytes_disagree_with_disk() {
        // R1 second half: post-write re-record must verify the bytes on disk
        // equal what we claim to have written before marking COMPLETE. If the
        // caller says it wrote N bytes but the file is a different size (a
        // replacement landed in the write→stat window), the record must NOT
        // be COMPLETE — otherwise a stale COMPLETE authorizes a later
        // overwrite of content the model never produced.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mismatch.txt");
        std::fs::write(&path, "0123456789").unwrap(); // 10 bytes on disk

        // Claim we wrote 25 bytes — disagrees with the 10 on disk.
        note_full_write("s", &path, 25);
        assert_ne!(
            view_status("s", &path),
            ViewStatus::Complete {
                epoch: ViewEpoch {
                    mtime: std::fs::metadata(&path).unwrap().modified().unwrap(),
                    size: 10,
                },
            },
            "a size disagreement must not be recorded as COMPLETE"
        );
        assert!(
            !matches!(view_status("s", &path), ViewStatus::Complete { .. }),
            "any COMPLETE here is a lie — the on-disk bytes are not what was written"
        );
    }

    #[test]
    fn should_bound_the_ledger_and_evict_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        // Unique session name so parallel tests cannot interleave entries
        // into this session's eviction accounting.
        let session = format!("evict-{}", std::process::id());
        let epoch = epoch_at(0, 1);
        // Paths need not exist — ledger_key falls back to the raw path.
        let path_of = |i: usize| dir.path().join(format!("f{i}.txt"));

        for i in 0..MAX_LEDGER_ENTRIES + 1 {
            record_view(&session, &path_of(i), epoch, 0, 1, 1, false);
        }
        assert_eq!(
            view_status(&session, &path_of(0)),
            ViewStatus::Unknown,
            "the oldest entry is evicted once the cap is crossed — safe, \
             because absence refuses"
        );
        assert!(
            matches!(
                view_status(&session, &path_of(MAX_LEDGER_ENTRIES)),
                ViewStatus::Complete { .. }
            ),
            "the newest entry survives"
        );
    }

    #[test]
    fn should_match_symlinked_and_canonical_forms_of_the_same_path() {
        // macOS tempdirs live under /var -> /private/var. If read_file
        // records the canonical form and write_file looks up the symlinked
        // form (or vice versa), the guard silently dies — so the ledger
        // canonicalizes on both sides.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "x").unwrap();
        let canonical = real.canonicalize().unwrap();

        record_view("s", &real, epoch_at(0, 10), 0, 5, 10, false);
        assert!(
            matches!(view_status("s", &canonical), ViewStatus::Partial { .. }),
            "the canonical form must see coverage recorded via the raw form"
        );
    }

    #[test]
    fn armed_from_env_defaults_to_off() {
        // The suite never sets OCTOS_READ_WINDOW (set_var is unsafe under
        // edition 2024), so this pins the shipped default: off.
        assert!(!armed_from_env(), "window enforcement must be opt-in");
    }
}
