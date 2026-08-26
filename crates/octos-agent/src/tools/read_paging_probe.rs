//! Observe-only probe for the read-paging question. Changes no behaviour.
//!
//! ## The question it exists to settle
//!
//! Forcing `read_file` to page is NOT a token optimisation. If the model ends
//! up consuming the whole file anyway, paging is strictly worse — the same
//! content split across more calls re-sends the conversation prefix each time,
//! measured at roughly +13% cost for a 50 KB file and +23% for a 200 KB one.
//! Paging wins only because models often stop after the first page.
//!
//! So the decision turns on one number nobody here has measured: **how often
//! does a model stop after page one?** This probe measures it before anyone
//! changes what tools return.
//!
//! ## And one safety number
//!
//! Silently handing back a window where callers expect a whole file can
//! destroy data. The patch tools are safe — `edit_file`, `diff_edit` and
//! `apply_patch` each independently re-read the complete file. But
//! `write_file` overwrites, and the slides workflow instructs the model to
//! read a script, reconstruct it whole, and write it back. A partial view
//! there becomes a destructive partial overwrite of a tail the model never saw.
//!
//! This probe therefore also counts read-then-overwrite-same-path, which is
//! the alarm that would veto the change regardless of the token numbers.
//!
//! ## What it does NOT do
//!
//! - Never alters a tool's arguments, output, or success.
//! - Records nothing unless armed (`OCTOS_READ_PAGING_PROBE=1`).
//! - Does not price anything. Reported spend is not cache-write-aware today,
//!   so any cost conclusion drawn from it would be wrong; this records raw
//!   shape instead and leaves pricing to a separate fix.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Canary window under evaluation: the size a forced page WOULD be.
///
/// 500 lines / 24 KiB comes from a scan of this checkout — 57.4% of Rust files
/// are at or below 500 lines and 66.8% at or below 24 KiB — so most reads would
/// still return whole. That is a repository distribution, not usage-weighted
/// production evidence, which is exactly what this probe is for.
pub(crate) const CANARY_MAX_LINES: usize = 500;
/// Byte half of the canary window. Bytes are the real bound: the execution
/// loop's cap counts `String::len()`, so a 500-line window of minified
/// JavaScript still overflows it.
pub(crate) const CANARY_MAX_BYTES: usize = 24 * 1024;

/// Reads that supplied no range at all — the population the change would affect.
static UNBOUNDED_READS: AtomicUsize = AtomicUsize::new(0);
/// Of those, how many were large enough that a forced window would have paged.
static UNBOUNDED_READS_THAT_WOULD_PAGE: AtomicUsize = AtomicUsize::new(0);
/// Reads that continued from a later offset on a path already read — the
/// model voluntarily paging forward. This is the numerator of
/// "pages consumed per initial unbounded read".
static CONTINUATION_READS: AtomicUsize = AtomicUsize::new(0);
/// THE ALARM: a path was read, then overwritten wholesale by `write_file`,
/// having been large enough that a forced window would have shown only part.
static PARTIAL_READ_THEN_OVERWRITE: AtomicUsize = AtomicUsize::new(0);
/// Reads whose longest single line alone exceeds the canary byte window. These
/// cannot be paged by line offset at all and need a byte cursor.
static UNPAGEABLE_LONG_LINE: AtomicUsize = AtomicUsize::new(0);

/// Per-path record of the most recent read, for the overwrite alarm.
static LAST_READ: Mutex<Option<HashMap<String, ReadShape>>> = Mutex::new(None);

/// Per-path counts, so a test can assert on the file IT created.
///
/// The aggregate counters above are process-global, and the suite runs ~2,600
/// tests in parallel — several of which call `read_file` while an armed probe
/// is counting. Asserting an exact global total is therefore a race that
/// passes on one platform and fails on another (it did: macOS 1, Windows 2).
/// Tests assert here instead; operators still read the aggregates.
#[cfg(test)]
static PER_PATH: Mutex<Option<HashMap<String, PathCounts>>> = Mutex::new(None);

/// What one specific path saw.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathCounts {
    pub(crate) unbounded_reads: usize,
    pub(crate) unbounded_reads_that_would_page: usize,
    pub(crate) continuation_reads: usize,
    pub(crate) partial_read_then_overwrite: usize,
    pub(crate) unpageable_long_line: usize,
}

/// Counts recorded for one path.
#[cfg(test)]
pub(crate) fn counts_for(path: &str) -> PathCounts {
    PER_PATH
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .and_then(|map| map.get(path).copied())
        .unwrap_or_default()
}

/// Mutate one path's counts.
#[cfg(test)]
fn bump_path(path: &str, apply: impl FnOnce(&mut PathCounts)) {
    let mut guard = PER_PATH.lock().unwrap_or_else(|p| p.into_inner());
    let entry = guard
        .get_or_insert_with(HashMap::new)
        .entry(path.to_string())
        .or_default();
    apply(entry);
}

/// Test-only arming, so no test has to mutate the environment (`set_var` is
/// `unsafe` under edition 2024 and this workspace denies unsafe).
#[cfg(test)]
static FORCED_ON: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static FORCED_ON: AtomicBool = AtomicBool::new(false);

/// Shape of one observed read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadShape {
    /// A forced canary window would have returned less than the whole file.
    pub(crate) would_page: bool,
}

/// Whether the probe is armed.
pub(crate) fn enabled() -> bool {
    if FORCED_ON.load(Ordering::Relaxed) {
        return true;
    }
    std::env::var("OCTOS_READ_PAGING_PROBE").is_ok_and(|value| value == "1")
}

/// Arm the probe for a test.
#[cfg(test)]
pub(crate) fn arm_for_test() {
    FORCED_ON.store(true, Ordering::Relaxed);
    reset();
}

/// Disarm and clear after a test.
#[cfg(test)]
pub(crate) fn disarm_for_test() {
    FORCED_ON.store(false, Ordering::Relaxed);
    reset();
}

/// Clear every counter and the per-path record.
#[cfg(test)]
pub(crate) fn reset() {
    for counter in [
        &UNBOUNDED_READS,
        &UNBOUNDED_READS_THAT_WOULD_PAGE,
        &CONTINUATION_READS,
        &PARTIAL_READ_THEN_OVERWRITE,
        &UNPAGEABLE_LONG_LINE,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    *LAST_READ.lock().unwrap_or_else(|p| p.into_inner()) = None;
    *PER_PATH.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Observe one `read_file` call.
///
/// `explicit_start` is the 1-indexed start the caller asked for, if any;
/// `bounded` is whether ANY range parameter was supplied.
pub(crate) fn record_read(
    path: &str,
    bounded: bool,
    explicit_start: Option<usize>,
    total_lines: usize,
    total_bytes: usize,
    max_line_bytes: usize,
) {
    if !enabled() {
        return;
    }
    let would_page = total_lines > CANARY_MAX_LINES || total_bytes > CANARY_MAX_BYTES;

    #[cfg(test)]
    bump_path(path, |counts| {
        if max_line_bytes > CANARY_MAX_BYTES {
            counts.unpageable_long_line += 1;
        }
        if bounded {
            if explicit_start.is_some_and(|start| start > 1) {
                counts.continuation_reads += 1;
            }
        } else {
            counts.unbounded_reads += 1;
            if would_page {
                counts.unbounded_reads_that_would_page += 1;
            }
        }
    });

    if max_line_bytes > CANARY_MAX_BYTES {
        // A single line larger than the whole window: line offsets cannot make
        // progress past it, so this file would need a byte-resumable cursor.
        UNPAGEABLE_LONG_LINE.fetch_add(1, Ordering::Relaxed);
    }

    if bounded {
        // A read that starts past line 1 on a path already seen is the model
        // paging forward of its own accord — the behaviour a forced window
        // would be betting on.
        if explicit_start.is_some_and(|start| start > 1) {
            CONTINUATION_READS.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        UNBOUNDED_READS.fetch_add(1, Ordering::Relaxed);
        if would_page {
            UNBOUNDED_READS_THAT_WOULD_PAGE.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut guard = LAST_READ.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_string(), ReadShape { would_page });
}

/// Observe one whole-file overwrite, returning `true` if it tripped the alarm.
///
/// The alarm fires when the overwritten path was previously read AND was large
/// enough that a forced window would have shown only part of it: under paging,
/// this write would have reconstructed the file from an incomplete view.
pub(crate) fn record_overwrite(path: &str) -> bool {
    if !enabled() {
        return false;
    }
    let guard = LAST_READ.lock().unwrap_or_else(|p| p.into_inner());
    let tripped = guard
        .as_ref()
        .and_then(|map| map.get(path))
        .is_some_and(|shape| shape.would_page);
    if tripped {
        PARTIAL_READ_THEN_OVERWRITE.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        bump_path(path, |counts| counts.partial_read_then_overwrite += 1);
    }
    tripped
}

/// The measurement, for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Findings {
    pub(crate) unbounded_reads: usize,
    pub(crate) unbounded_reads_that_would_page: usize,
    pub(crate) continuation_reads: usize,
    pub(crate) partial_read_then_overwrite: usize,
    pub(crate) unpageable_long_line: usize,
}

/// Read the counters.
pub(crate) fn findings() -> Findings {
    Findings {
        unbounded_reads: UNBOUNDED_READS.load(Ordering::Relaxed),
        unbounded_reads_that_would_page: UNBOUNDED_READS_THAT_WOULD_PAGE.load(Ordering::Relaxed),
        continuation_reads: CONTINUATION_READS.load(Ordering::Relaxed),
        partial_read_then_overwrite: PARTIAL_READ_THEN_OVERWRITE.load(Ordering::Relaxed),
        unpageable_long_line: UNPAGEABLE_LONG_LINE.load(Ordering::Relaxed),
    }
}

/// One-line summary for an operator to log at the end of a run.
///
/// `continuation_reads / unbounded_reads_that_would_page` is the decisive
/// ratio: near zero means models stop after page one and forcing pages wins;
/// near one means they read on and forcing pages costs more than it saves.
pub(crate) fn summary() -> String {
    let f = findings();
    format!(
        "read-paging probe: unbounded={} would_page={} continuations={} \
         OVERWRITE_AFTER_PARTIAL_READ={} unpageable_long_line={}",
        f.unbounded_reads,
        f.unbounded_reads_that_would_page,
        f.continuation_reads,
        f.partial_read_then_overwrite,
        f.unpageable_long_line,
    )
}

/// Logs the probe's running totals when a turn ends, however it ends.
///
/// A Drop guard rather than a call at the end of the loop: `process_message`
/// returns from a dozen places including error paths, and a summary that only
/// prints on the happy path would under-report exactly the runs worth studying.
pub(crate) struct TurnSummaryGuard;

impl TurnSummaryGuard {
    /// Arm a summary for this turn, or `None` when the probe is off.
    pub(crate) fn new() -> Option<Self> {
        enabled().then_some(Self)
    }
}

impl Drop for TurnSummaryGuard {
    fn drop(&mut self) {
        tracing::info!("{}", summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use crate::tools::read_file::ReadFileTool;
    use crate::tools::write_file::WriteFileTool;

    /// A file big enough that the canary window would have paged it.
    fn big_body() -> String {
        (0..CANARY_MAX_LINES + 50)
            .map(|i| format!("line {i}\n"))
            .collect()
    }

    /// Serialises the probe's process-global state across tests.
    ///
    /// A `tokio` mutex rather than `std`: these tests await tool execution
    /// while holding it, and a `std::sync::MutexGuard` held across an await
    /// can block the executor thread (clippy `await_holding_lock` flags it,
    /// correctly).
    async fn probe_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
            std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
        LOCK.lock().await
    }

    #[tokio::test]
    async fn should_count_an_unbounded_read_of_a_large_file_as_one_that_would_page() {
        let _guard = probe_guard().await;
        arm_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), big_body()).unwrap();
        let tool = ReadFileTool::new(dir.path());

        tool.execute(&serde_json::json!({ "path": "big.txt" }))
            .await
            .unwrap();

        let f = counts_for(&dir.path().join("big.txt").to_string_lossy());
        disarm_for_test();
        assert_eq!(f.unbounded_reads, 1, "the read must be observed: {f:?}");
        assert_eq!(
            f.unbounded_reads_that_would_page, 1,
            "a file past the canary window is exactly the population a forced window changes: {f:?}"
        );
    }

    #[tokio::test]
    async fn should_count_a_later_offset_as_the_model_paging_forward() {
        let _guard = probe_guard().await;
        arm_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), big_body()).unwrap();
        let tool = ReadFileTool::new(dir.path());

        tool.execute(&serde_json::json!({ "path": "big.txt", "offset": 200, "limit": 50 }))
            .await
            .unwrap();

        let f = counts_for(&dir.path().join("big.txt").to_string_lossy());
        disarm_for_test();
        assert_eq!(
            f.continuation_reads, 1,
            "reading from a later offset is the behaviour forcing pages bets on: {f:?}"
        );
        assert_eq!(
            f.unbounded_reads, 0,
            "a ranged read is not unbounded: {f:?}"
        );
    }

    /// The safety alarm, end to end through both real tools.
    ///
    /// This is the number that would veto forced paging regardless of any
    /// token maths: the slides workflow reads a script, reconstructs it whole,
    /// and writes it back. Hand that flow a window and the write destroys the
    /// tail nobody saw.
    #[tokio::test]
    async fn should_raise_the_alarm_when_a_large_read_is_followed_by_overwriting_it() {
        let _guard = probe_guard().await;
        arm_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("script.js"), big_body()).unwrap();

        ReadFileTool::new(dir.path())
            .execute(&serde_json::json!({ "path": "script.js" }))
            .await
            .unwrap();
        WriteFileTool::new(dir.path())
            .execute(&serde_json::json!({ "path": "script.js", "content": "rebuilt" }))
            .await
            .unwrap();

        let f = counts_for(&dir.path().join("script.js").to_string_lossy());
        disarm_for_test();
        assert_eq!(
            f.partial_read_then_overwrite, 1,
            "read-then-overwrite of a would-page file is the destructive pattern: {f:?}"
        );
    }

    #[tokio::test]
    async fn should_not_raise_the_alarm_when_the_file_fits_in_one_window() {
        let _guard = probe_guard().await;
        arm_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), "one\ntwo\n").unwrap();

        ReadFileTool::new(dir.path())
            .execute(&serde_json::json!({ "path": "small.txt" }))
            .await
            .unwrap();
        WriteFileTool::new(dir.path())
            .execute(&serde_json::json!({ "path": "small.txt", "content": "rebuilt" }))
            .await
            .unwrap();

        let f = counts_for(&dir.path().join("small.txt").to_string_lossy());
        disarm_for_test();
        assert_eq!(
            f.partial_read_then_overwrite, 0,
            "a file that fits whole is never shown partially, so overwriting it is safe: {f:?}"
        );
    }

    /// Establishes that a zero MEANS something.
    ///
    /// A probe that silently records nothing reports the same all-clear as a
    /// clean run. This pins that the arming switch is what decides.
    #[tokio::test]
    async fn should_record_nothing_when_the_probe_is_disarmed() {
        let _guard = probe_guard().await;
        arm_for_test();
        disarm_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), big_body()).unwrap();

        ReadFileTool::new(dir.path())
            .execute(&serde_json::json!({ "path": "big.txt" }))
            .await
            .unwrap();

        assert_eq!(
            counts_for(&dir.path().join("big.txt").to_string_lossy()),
            PathCounts::default(),
            "a disarmed probe must record nothing, so an armed zero is evidence"
        );
    }

    #[tokio::test]
    async fn should_flag_a_single_line_too_long_to_page_by_offset() {
        let _guard = probe_guard().await;
        arm_for_test();
        record_read(
            "huge-line.min.js",
            false,
            None,
            1,
            CANARY_MAX_BYTES * 2,
            CANARY_MAX_BYTES * 2,
        );
        let f = counts_for("huge-line.min.js");
        disarm_for_test();
        assert_eq!(
            f.unpageable_long_line, 1,
            "one line larger than the window cannot be paged by line offset at all: {f:?}"
        );
    }
}
