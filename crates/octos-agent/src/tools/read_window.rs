//! Flag-gated windowed `read_file` enforcement (#1638). **Off by default.**
//!
//! Armed via `OCTOS_READ_WINDOW=1`. When armed, `read_file` returns at most
//! [`WINDOW_MAX_LINES`] lines and at most [`WINDOW_MAX_BYTES`] bytes of
//! formatted output — whichever limit is hit first — with a footer naming the
//! limit that fired, the range actually returned, the file's totals, and the
//! exact next call. Unarmed behaviour is byte-identical to before this module
//! existed.
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
//! tool_output_limit("read_file")` so lowering the loop budget cannot silently
//! re-expose windowed reads to the blind backstop.
//!
//! ## The partial-view ledger
//!
//! The second half of this module tracks, per absolute path, how much of the
//! file the model has actually been shown — so `write_file` (armed) can refuse
//! to overwrite a file wholesale from a partial view. The patch tools
//! (`edit_file`, `diff_edit`, `apply_patch`) are safe — they re-read the whole
//! file themselves — but `write_file` reconstructs from whatever the model
//! saw, and the slides workflow mandates exactly read → rebuild → `write_file`.
//!
//! Coverage is a contiguous-from-line-1 high-water mark, keyed by mtime:
//! paging 1..2000, 2001..4000, 4001..EOF under one mtime marks the path
//! COMPLETE (so the refusal's "page through it first" advice is truthful);
//! any mtime change resets coverage to the new read alone. Reading out of
//! order (tail before head) under-counts and stays partial — conservative in
//! the safe direction, and the refusal always offers the patch tools as the
//! unconditional escape hatch.
//!
//! The ledger is a process-global map, NOT threaded through `ToolContext`:
//! the guard is a data-loss check and must hold on every entry path,
//! including legacy `Tool::execute` calls that carry a zero context (the
//! `FileStateCache` on `ToolContext` is optional and absent on those paths,
//! which is why it was not reused). It records only when armed, so the
//! unarmed process pays nothing. This is deliberately NOT the #2126 probe's
//! `LAST_READ` map — that one is observe-only, keyed to the probe's own env
//! flag and 500/24KiB canary, and its per-path counters are `cfg(test)`.
//!
//! ## Known limitation: a single line larger than the window
//!
//! `read_file` has no byte-offset parameter, so it cannot page WITHIN a line
//! (matching pi, which ships the same limitation and points at a shell
//! fallback instead — `read.ts`'s `firstLineExceedsLimit` branch). A giant
//! line is delivered via the advice footer's `sed | head -c` command, which
//! the ledger cannot observe — so a file containing one can never reach
//! COMPLETE through `read_file` alone, and a whole-file overwrite of it stays
//! refused. The refusal's `edit_file`/`apply_patch` escape hatch covers that
//! case; a `byte_offset` continuation parameter is the future fix if the
//! probe's `unpageable_long_line` counter ever shows it matters.

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

/// Typed prefix on the armed `write_file` refusal, so the model (and any
/// harness) can match it structurally — same convention as
/// `[FILE_UNCHANGED]`.
pub(crate) const PARTIAL_VIEW_OVERWRITE_PREFIX: &str = "[PARTIAL_VIEW_OVERWRITE]";

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

/// How much of one path the model has been shown, contiguously from line 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewCoverage {
    /// Lines 1..=`seen_through` have been shown under `mtime`.
    pub(crate) seen_through: usize,
    /// Total lines the file had when last read.
    pub(crate) total_lines: usize,
}

/// One path's ledger record.
#[derive(Clone, Copy, Debug)]
struct ViewRecord {
    /// mtime the coverage was accumulated under; `None` = unknown (never
    /// stitches).
    mtime: Option<SystemTime>,
    seen_through: usize,
    total_lines: usize,
}

/// Per-path record of what the model has been shown. See the module docs for
/// why this is process-global rather than `ToolContext`-threaded.
static LAST_VIEW: Mutex<Option<HashMap<PathBuf, ViewRecord>>> = Mutex::new(None);

/// Which window limit clamped an armed read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClamp {
    /// [`WINDOW_MAX_LINES`] fired first.
    Lines,
    /// [`WINDOW_MAX_BYTES`] fired first.
    Bytes,
}

/// The key both sides of the ledger agree on.
///
/// `read_file` and `write_file` can resolve the same file to different
/// spellings (macOS tempdirs alone alias `/var` to `/private/var`); if they
/// keyed the raw spellings the guard would silently never match. Falls back
/// to the raw path when canonicalization fails (file deleted between check
/// and record) — both sides then fail the same way.
fn ledger_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Record one armed `read_file` view of `path`: lines `start..=end`
/// (1-indexed, inclusive) of a `total_lines`-line file at `mtime`.
///
/// Same-mtime views extend the contiguous-from-1 high-water mark; a changed
/// (or unknown) mtime resets coverage to this view alone, because coverage
/// stitched across an on-disk edit would claim the model has seen content
/// that no longer exists. Callers gate on arming — this function itself does
/// not consult the env, so tests can drive it through per-instance-armed
/// tools without touching process state.
pub(crate) fn record_view(
    path: &Path,
    mtime: Option<SystemTime>,
    start: usize,
    end: usize,
    total_lines: usize,
) {
    let key = ledger_key(path);
    let mut guard = LAST_VIEW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    // Coverage carries over only within one mtime epoch; `None` mtimes never
    // match (unknown is not evidence).
    let carried = match map.get(&key) {
        Some(prev) if prev.mtime.is_some() && prev.mtime == mtime => prev.seen_through,
        _ => 0,
    };
    let seen_through = if start <= carried.saturating_add(1) {
        carried.max(end)
    } else {
        // A view past the high-water mark leaves a gap; the mark stays.
        carried
    };
    map.insert(
        key,
        ViewRecord {
            mtime,
            seen_through,
            total_lines,
        },
    );
}

/// The coverage record for `path` when the model's most recent knowledge of
/// it is PARTIAL, or `None` when the path is unknown or fully seen.
pub(crate) fn partial_view_of(path: &Path) -> Option<ViewCoverage> {
    let key = ledger_key(path);
    let guard = LAST_VIEW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref()?.get(&key).and_then(|record| {
        if record.seen_through >= record.total_lines {
            None
        } else {
            Some(ViewCoverage {
                seen_through: record.seen_through,
                total_lines: record.total_lines,
            })
        }
    })
}

/// Note a successful whole-file `write_file` of `path`: the on-disk content
/// is now exactly what the model supplied, so any partial mark is obsolete.
pub(crate) fn note_full_write(path: &Path) {
    let key = ledger_key(path);
    let mut guard = LAST_VIEW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // The ledger is process-global and keyed by path; each test uses paths
    // under its own tempdir so parallel tests never observe each other
    // (#2077/#2126: never assert on process-global aggregates).

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
        let mtime = Some(SystemTime::now());

        record_view(&path, mtime, 1, 2000, 5000);
        assert_eq!(
            partial_view_of(&path),
            Some(ViewCoverage {
                seen_through: 2000,
                total_lines: 5000
            }),
            "a windowed view is partial"
        );

        record_view(&path, mtime, 2001, 4000, 5000);
        assert_eq!(
            partial_view_of(&path),
            Some(ViewCoverage {
                seen_through: 4000,
                total_lines: 5000
            }),
            "contiguous pages extend the high-water mark"
        );

        record_view(&path, mtime, 4001, 5000, 5000);
        assert_eq!(
            partial_view_of(&path),
            None,
            "paging through to EOF completes the view — the refusal's advice \
             must be truthful"
        );
    }

    #[test]
    fn should_not_stitch_across_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gappy.txt");
        std::fs::write(&path, "x").unwrap();
        let mtime = Some(SystemTime::now());

        record_view(&path, mtime, 1, 2000, 5000);
        record_view(&path, mtime, 2500, 5000, 5000); // lines 2001-2499 never seen
        assert_eq!(
            partial_view_of(&path),
            Some(ViewCoverage {
                seen_through: 2000,
                total_lines: 5000
            }),
            "a gap means the view is still partial at the gap"
        );
    }

    #[test]
    fn should_reset_coverage_when_the_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edited.txt");
        std::fs::write(&path, "x").unwrap();
        let old = Some(SystemTime::now() - Duration::from_secs(60));
        let new = Some(SystemTime::now());

        record_view(&path, old, 1, 2000, 5000);
        // The file changed on disk; earlier coverage is no longer evidence.
        record_view(&path, new, 2001, 4000, 5000);
        assert_eq!(
            partial_view_of(&path),
            Some(ViewCoverage {
                seen_through: 0,
                total_lines: 5000
            }),
            "coverage accumulated under a different mtime must not survive"
        );
    }

    #[test]
    fn should_forget_a_path_after_a_full_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rewritten.txt");
        std::fs::write(&path, "x").unwrap();

        record_view(&path, Some(SystemTime::now()), 1, 10, 5000);
        assert!(partial_view_of(&path).is_some());

        note_full_write(&path);
        assert_eq!(
            partial_view_of(&path),
            None,
            "after write_file succeeds the on-disk content is exactly what \
             the model supplied — no partial mark may linger"
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

        record_view(&real, Some(SystemTime::now()), 1, 10, 5000);
        assert!(
            partial_view_of(&canonical).is_some(),
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
