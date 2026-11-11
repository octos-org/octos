//! Synchronous, model-free direct operations on MEMORY.md entries
//! (T-0548, UPCR-2026-RD1002E7E).
//!
//! The `memory/entries`, `memory/edit` and `memory/delete` RPCs land here.
//! Unlike `memory/request` (which only ADMITS a note for the background
//! consolidator), these operations read, mutate and republish MEMORY.md in
//! the caller's request — no model call, and no dependency on the
//! memory-refresh toggle or worker.
//!
//! Mutual exclusion with background consolidation: every writer — a direct
//! edit/delete here AND [`super::run_consolidation`] — holds the profile
//! memory dir's `.write.lock` flock for its whole read-modify-publish
//! critical section. A direct edit therefore runs entirely before or
//! entirely after a consolidation run; a consolidation plan built from a
//! pre-edit snapshot can never be published over the edit (no lost edit, no
//! resurrected entry). The `.refresh.lock` stays the service-ownership
//! lock; `.write.lock` is the per-mutation lock and is held only for the
//! duration of one pass.
//!
//! Ordering with already-queued `memory/request` notes on the same id:
//! staged notes are NOT rewritten by a direct operation. A queued `forget`
//! for a directly deleted id is consumed by the next consolidation's
//! already-satisfied pre-pass (no-op); a queued `correct` whose target a
//! direct delete removed is retired by the engine's orphan-correction rule;
//! a queued `correct` whose target a direct edit rewrote still rides the
//! next merge — user asks apply in submission order, the direct edit first.
//! All of those are pre-existing engine behaviors; the direct operation
//! only guarantees the file state they start from.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use eyre::{Result, WrapErr};
use fs2::FileExt;

use super::apply::{self, ApplyPlan, ScrubTarget};
use super::entry::{self, Entry, ParsedMemory, sha256_hex};
use super::ops::finalize_entry_text;

/// Name of the per-profile memory write lock file (inside the memory dir).
const WRITE_LOCK_FILE: &str = ".write.lock";

/// Held flock on `memory/.write.lock`. Releases on drop.
#[derive(Debug)]
pub struct MemoryWriteLock {
    file: std::fs::File,
}

impl Drop for MemoryWriteLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            tracing::warn!(%error, "failed to release memory write lock");
        }
    }
}

/// Blocking variant of [`lock_memory_writes`] for callers already on a
/// blocking thread (the direct RPC handlers run the whole mutation in one
/// `spawn_blocking` closure).
pub fn lock_memory_writes_blocking(memory_dir: &Path) -> Result<MemoryWriteLock> {
    std::fs::create_dir_all(memory_dir)
        .wrap_err_with(|| format!("failed to create {}", memory_dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(memory_dir.join(WRITE_LOCK_FILE))
        .wrap_err("failed to open memory write lock")?;
    // Block until the current holder (a consolidation pass or another
    // direct mutation) publishes and releases. Waiting — instead of
    // failing — is what makes a direct edit/delete during an in-progress
    // consolidation land on the PUBLISHED state rather than error out or,
    // worse, interleave with the apply.
    file.lock_exclusive().wrap_err("memory write lock failed")?;
    Ok(MemoryWriteLock { file })
}

/// Acquire the profile memory dir's write lock without blocking the async
/// executor (consolidation entry point).
pub async fn lock_memory_writes(memory_dir: &Path) -> Result<MemoryWriteLock> {
    let memory_dir = memory_dir.to_path_buf();
    tokio::task::spawn_blocking(move || lock_memory_writes_blocking(&memory_dir))
        .await
        .wrap_err("memory write lock task panicked")?
}

/// How a direct edit/delete can fail. The RPC layer maps these onto honest
/// wire errors; the engine changes NOTHING unless it returns `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectError {
    /// The id is not a live MEMORY.md entry in this profile — unknown id,
    /// wrong profile, or a file without id-addressable entries (legacy).
    NotFound,
    /// The caller's `version` precondition does not match the stored entry.
    /// Carries the CURRENT version so the caller can refresh and retry.
    VersionConflict { current_version: String },
    /// The content cannot be represented as one MEMORY.md entry block.
    InvalidContent(String),
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "memory entry not found"),
            Self::VersionConflict { .. } => write!(f, "memory entry version conflict"),
            Self::InvalidContent(reason) => write!(f, "invalid entry content: {reason}"),
        }
    }
}

impl std::error::Error for DirectError {}

/// Read and parse MEMORY.md into entries. A missing file or a LEGACY file
/// (no id-bearing blocks — INIT has not run yet) yields an empty list: no
/// id-addressable entries exist either way. A mixed/duplicate-id file fails
/// closed, exactly as the consolidation engine refuses to guess.
pub fn list_entries(memory_dir: &Path) -> Result<Vec<Entry>> {
    let content = match std::fs::read_to_string(memory_dir.join("MEMORY.md")) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).wrap_err("failed to read MEMORY.md"),
    };
    match entry::parse_memory_md(&content)? {
        ParsedMemory::Entries(entries) => Ok(entries),
        ParsedMemory::Legacy(_) => Ok(Vec::new()),
    }
}

/// The version precondition an edit/delete advertises: SHA-256 hex of the
/// stored entry text (id token included) — same digest the consolidation
/// engine uses for hash-bound restores.
pub fn entry_version(entry: &Entry) -> String {
    entry.content_hash()
}

/// Validate direct-edit content into the stored entry text: the caller's
/// body verbatim, with the kernel's standard stamp/id finalization
/// (`(updated: <today>)` appended only when the body carries no stamp — the
/// same rule a merge `update` op follows). Rejects blank content and any
/// content containing a blank line: a blank line would split the block, and
/// the tail block would lack an id — the next parse would fail closed on a
/// "mixed" file.
pub fn build_edited_text(
    content: &str,
    today: NaiveDate,
    entry_id: &str,
) -> Result<String, DirectError> {
    if content.trim().is_empty() {
        return Err(DirectError::InvalidContent(
            "content must be non-blank".to_string(),
        ));
    }
    if content.lines().any(|line| line.trim().is_empty()) {
        return Err(DirectError::InvalidContent(
            "content must be a single block: empty lines would split the entry".to_string(),
        ));
    }
    Ok(finalize_entry_text(content, false, today, entry_id))
}

/// Find the live entry and check the caller's version precondition.
/// Shared by edit and delete so both fail identically on stale views.
fn find_versioned<'e>(
    entries: &'e [Entry],
    entry_id: &str,
    expected_version: &str,
) -> Result<(usize, &'e Entry), DirectError> {
    let (index, entry) = entries
        .iter()
        .enumerate()
        .find(|(_, e)| e.id == entry_id)
        .ok_or(DirectError::NotFound)?;
    let current_version = entry_version(entry);
    if current_version != expected_version {
        return Err(DirectError::VersionConflict { current_version });
    }
    Ok((index, entry))
}

/// What a direct edit stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReceipt {
    pub entry_id: String,
    /// The entry's NEW version — SHA-256 of the stored text after the
    /// stamp/id finalization.
    pub version: String,
}

/// Synchronously replace one entry's body with `content`, verbatim.
/// The id and position are unchanged; the freshness stamp follows the
/// kernel's existing `finalize_entry_text` rule. The previous file is kept
/// as `MEMORY.md.bak`, matching a merge without hard deletes.
///
/// The CALLER holds the write lock. On `Err` the file is untouched
/// (the publish itself is atomic temp+rename).
pub fn edit_entry(
    memory_dir: &Path,
    entry_id: &str,
    expected_version: &str,
    content: &str,
    today: NaiveDate,
) -> Result<Result<EditReceipt, DirectError>> {
    let new_text = match build_edited_text(content, today, entry_id) {
        Ok(text) => text,
        Err(error) => return Ok(Err(error)),
    };
    let mut entries = list_entries(memory_dir)?;
    let (index, _) = match find_versioned(&entries, entry_id, expected_version) {
        Ok(found) => found,
        Err(error) => return Ok(Err(error)),
    };
    let version = sha256_hex(&new_text);
    entries[index] = Entry {
        id: entry_id.to_string(),
        text: new_text,
    };
    apply::write_memory_md(memory_dir, &entries, true)?;
    Ok(Ok(EditReceipt {
        entry_id: entry_id.to_string(),
        version,
    }))
}

/// What a direct delete removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteReceipt {
    pub entry_id: String,
    /// The version (SHA-256 of the stored text) that was deleted.
    pub version: String,
}

/// Synchronously delete one entry with the SAME effect as a completed
/// id-bound forget: the entry leaves MEMORY.md, archived blocks naming the
/// id are removed whole, bank/daily-note copies are line-scrubbed,
/// `.bak`/`.prev` backups are deleted, and untrusted staging copies are
/// removed — nothing of the entry is left. Host-authored staging notes are
/// preserved by the apply layer and retire on the next consolidation (see
/// the module docs for the ordering contract).
///
/// The CALLER holds the write lock. A partial failure (e.g. an archive
/// rewrite succeeds and the MEMORY.md publish then fails) is reported as
/// `Err` — never as success; the error message from the RPC layer says so.
pub fn delete_entry(
    memory_dir: &Path,
    entry_id: &str,
    expected_version: &str,
    today: NaiveDate,
) -> Result<Result<DeleteReceipt, DirectError>> {
    let entries = list_entries(memory_dir)?;
    let (index, entry) = match find_versioned(&entries, entry_id, expected_version) {
        Ok(found) => found,
        Err(error) => return Ok(Err(error)),
    };
    let version = entry_version(entry);
    let mut remaining = entries.clone();
    remaining.remove(index);
    let plan = ApplyPlan {
        final_entries: remaining,
        hard_deletes: vec![ScrubTarget {
            entry_id: entry_id.to_string(),
            folded_lines: entry.folded_lines(),
            // No authorizing note: the RPC caller IS the authority. The
            // apply layer treats an empty path as a no-op delete and still
            // runs the full scrub pipeline.
            authorizing_note: PathBuf::new(),
            originating_pending: Vec::new(),
        }],
        ..ApplyPlan::default()
    };
    apply::apply_plan(memory_dir, today, &plan)?;
    Ok(Ok(DeleteReceipt {
        entry_id: entry_id.to_string(),
        version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        (dir, memory_dir)
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()
    }

    fn seed(blocks: &[&str]) -> (tempfile::TempDir, PathBuf, Vec<Entry>) {
        let (dir, memory_dir) = memory_dir();
        let rendered = format!("{}\n", blocks.join("\n\n"));
        let entries = match entry::parse_memory_md(&rendered).unwrap() {
            ParsedMemory::Entries(entries) => entries,
            ParsedMemory::Legacy(_) => panic!("fixture blocks must carry ids"),
        };
        apply::write_memory_md(&memory_dir, &entries, false).unwrap();
        (dir, memory_dir, entries)
    }

    fn read_texts(memory_dir: &Path) -> Vec<String> {
        list_entries(memory_dir)
            .unwrap()
            .into_iter()
            .map(|e| e.text)
            .collect()
    }

    fn assert_memory_unchanged(memory_dir: &Path, before: &str) {
        assert_eq!(
            std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap(),
            before,
            "a refused direct operation must not change MEMORY.md"
        );
    }

    const E1: &str = "likes green tea (updated: 2026-01-01) ^m4k2abq";
    const E2: &str = "lives in Lisbon (updated: 2026-01-05) ^mzzzzz2";

    #[test]
    fn should_list_no_entries_for_missing_or_legacy_files() {
        let (_dir, memory_dir) = memory_dir();
        assert!(list_entries(&memory_dir).unwrap().is_empty());
        std::fs::write(memory_dir.join("MEMORY.md"), "no ids here\n\nyet\n").unwrap();
        assert!(list_entries(&memory_dir).unwrap().is_empty());
    }

    #[test]
    fn should_land_edit_verbatim_with_id_unchanged() {
        let (_dir, memory_dir, _) = seed(&[E1, E2]);
        let version = sha256_hex(E1);
        let receipt = edit_entry(&memory_dir, "^m4k2abq", &version, "prefers oolong", today())
            .unwrap()
            .expect("edit succeeds");
        let texts = read_texts(&memory_dir);
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[1], E2, "other entries untouched");
        // Body is the user's text verbatim; the id is unchanged and the
        // kernel stamp rule appended `(updated: <today>)`.
        assert_eq!(
            texts[0],
            format!("prefers oolong (updated: 2026-11-09) ^m4k2abq")
        );
        assert_eq!(receipt.version, sha256_hex(&texts[0]));
        assert_eq!(receipt.entry_id, "^m4k2abq");
        // The receipt version is the precondition for a follow-up edit.
        edit_entry(
            &memory_dir,
            "^m4k2abq",
            &receipt.version,
            "third edit",
            today(),
        )
        .unwrap()
        .expect("chained edit succeeds");
        assert!(read_texts(&memory_dir)[0].starts_with("third edit "));
    }

    #[test]
    fn should_report_not_found_for_unknown_id_and_change_nothing() {
        let (_dir, memory_dir, _) = seed(&[E1]);
        let before = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        let result = edit_entry(&memory_dir, "^maaaaaa", &sha256_hex("v"), "x", today()).unwrap();
        assert_eq!(result, Err(DirectError::NotFound));
        let result = delete_entry(&memory_dir, "^maaaaaa", &sha256_hex("v"), today()).unwrap();
        assert_eq!(result, Err(DirectError::NotFound));
        assert_memory_unchanged(&memory_dir, &before);
    }

    #[test]
    fn should_reject_content_that_cannot_form_one_entry_block() {
        let (_dir, memory_dir, _) = seed(&[E1]);
        let version = sha256_hex(E1);
        let before = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        for bad in ["", "   ", "two\n\nblocks", "trailing blank\n \nlines"] {
            let result = edit_entry(&memory_dir, "^m4k2abq", &version, bad, today()).unwrap();
            assert!(
                matches!(result, Err(DirectError::InvalidContent(_))),
                "{bad:?} must be rejected"
            );
        }
        assert_memory_unchanged(&memory_dir, &before);
    }

    #[test]
    fn should_delete_entry_and_cascade_to_archives_and_backups() {
        let (_dir, memory_dir, _) = seed(&[E1, E2]);
        // An archived OLDER version of E1 (different text, same id), an
        // unrelated archived block, and a .bak from an earlier merge.
        let archive_dir = memory_dir.join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("2026-01.md"),
            "old tea note (updated: 2025-12-01) ^m4k2abq\n\narchived other (updated: 2025-12-02) ^mqqqqq3\n",
        )
        .unwrap();
        std::fs::write(memory_dir.join("MEMORY.md.bak"), E1).unwrap();
        let version = sha256_hex(E1);
        let receipt = delete_entry(&memory_dir, "^m4k2abq", &version, today())
            .unwrap()
            .expect("delete succeeds");
        assert_eq!(receipt.entry_id, "^m4k2abq");
        assert_eq!(receipt.version, version);
        assert_eq!(read_texts(&memory_dir), vec![E2.to_string()]);
        let archive = std::fs::read_to_string(archive_dir.join("2026-01.md")).unwrap();
        assert!(
            !archive.contains("^m4k2abq"),
            "archived versions of the deleted id are removed whole: {archive}"
        );
        assert!(archive.contains("^mqqqqq3"), "unrelated blocks survive");
        assert!(
            !memory_dir.join("MEMORY.md.bak").exists(),
            "a delete must not leave a .bak holding the removed entry"
        );
    }

    #[test]
    fn should_keep_other_entries_intact_when_delete_scrubs_shared_lines() {
        // An entry sharing an exact fact line with the deleted entry loses
        // that line (engine forget semantics) but survives on its own line.
        let shared = "likes green tea";
        let e3 = format!("{shared}\nalso bikes (updated: 2026-02-02) ^mbbbbb2");
        let (_dir, memory_dir, _) = seed(&[E1, &e3]);
        let version = sha256_hex(E1);
        delete_entry(&memory_dir, "^m4k2abq", &version, today())
            .unwrap()
            .expect("delete succeeds");
        let texts = read_texts(&memory_dir);
        assert_eq!(texts.len(), 1);
        assert!(
            !texts[0].contains(shared) && texts[0].contains("also bikes"),
            "shared line scrubbed, own content kept: {}",
            texts[0]
        );
    }

    #[test]
    fn should_refuse_stale_delete_and_change_nothing() {
        let (_dir, memory_dir, _) = seed(&[E1]);
        let before = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        let result = delete_entry(&memory_dir, "^m4k2abq", &sha256_hex("stale"), today()).unwrap();
        assert!(matches!(result, Err(DirectError::VersionConflict { .. })));
        assert_memory_unchanged(&memory_dir, &before);
    }

    /// Stub provider holding the merge call open until released, then
    /// asking to UPDATE the entry — a plan built from a pre-delete snapshot
    /// that must never be published over a direct delete.
    struct SlowUpdate {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for SlowUpdate {
        fn model_id(&self) -> &str {
            "slow-update-stub"
        }

        fn provider_name(&self) -> &str {
            "stub"
        }

        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(octos_llm::ChatResponse {
                content: Some(
                    "{\"ops\":[{\"op\":\"update\",\"id\":\"^m4k2abq\",\"new_text\":\"model rewrite\",\"sources\":[\"req-1\"]}],\"consumed_ids\":[\"req-1\"]}"
                        .to_string(),
                ),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
    }

    fn stage_correction(memory_dir: &Path) {
        let notes = memory_dir.join("staging").join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join("req-1.md"),
            "---\norigin: host\nkind: correction\ncreated_at: 2026-11-09T00:00:00+00:00\nreplaces_id: \"^m4k2abq\"\n---\nplease fix the tea entry\n",
        )
        .unwrap();
    }

    fn consolidation_params(memory_dir: &Path) -> super::super::ConsolidateParams {
        let mut params = super::super::ConsolidateParams::new(memory_dir.to_path_buf());
        params.today = today();
        params
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_never_clobber_a_direct_delete_with_a_concurrent_consolidation() {
        let (_dir, memory_dir, _) = seed(&[E1]);
        // A queued host correction for the same id keeps the merge
        // consumable; the merge's planned update must lose to the delete.
        stage_correction(&memory_dir);

        let provider = std::sync::Arc::new(SlowUpdate {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let params = consolidation_params(&memory_dir);
        let consolidation = {
            let provider = provider.clone();
            tokio::spawn(async move { super::super::run_consolidation(provider, &params).await })
        };
        // Wait until the merge call is in flight — the run holds the write
        // lock — then delete. The delete must wait for the run to publish
        // and then honestly CONFLICT (its precondition version is stale):
        // changed nothing, never overwritten back to the pre-delete state.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.started.notified(),
        )
        .await
        .expect("merge call started");
        let version = sha256_hex(E1);
        let memory_dir2 = memory_dir.clone();
        let delete = tokio::task::spawn_blocking(move || {
            let _lock = lock_memory_writes_blocking(&memory_dir2).unwrap();
            delete_entry(&memory_dir2, "^m4k2abq", &version, today())
        });
        // Give the delete a moment to prove it is blocked behind the lock,
        // then let the merge finish.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !delete.is_finished(),
            "delete waits for the in-progress consolidation"
        );
        provider.release.notify_one();
        let outcome = consolidation.await.unwrap().expect("consolidation ran");
        let deleted = delete.await.unwrap().expect("delete ran");

        let current = list_entries(&memory_dir).unwrap();
        let entry = current
            .iter()
            .find(|e| e.id == "^m4k2abq")
            .expect("the merge published its update first");
        assert!(outcome.merge_applied);
        assert!(entry.text.starts_with("model rewrite"));
        let current_version = match deleted {
            Err(DirectError::VersionConflict { current_version }) => {
                assert_eq!(current_version, entry_version(entry));
                current_version
            }
            other => panic!("expected version conflict, got {other:?}"),
        };
        // Retrying with the fresh version succeeds — no lost delete, no
        // resurrected old text.
        let memory_dir3 = memory_dir.clone();
        let retry = tokio::task::spawn_blocking(move || {
            let _lock = lock_memory_writes_blocking(&memory_dir3).unwrap();
            delete_entry(&memory_dir3, "^m4k2abq", &current_version, today())
        })
        .await
        .unwrap()
        .expect("retry ran");
        assert!(retry.is_ok());
        assert!(
            read_texts(&memory_dir)
                .iter()
                .all(|t| !t.contains("^m4k2abq")),
            "deleted entry must not be resurrected"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_keep_a_delete_that_landed_before_the_consolidation_started() {
        let (_dir, memory_dir, _) = seed(&[E1]);
        stage_correction(&memory_dir);
        let version = sha256_hex(E1);
        {
            let _lock = lock_memory_writes(&memory_dir).await.unwrap();
            delete_entry(&memory_dir, "^m4k2abq", &version, today())
                .unwrap()
                .expect("delete succeeds");
        }
        // The queued correction's target is gone everywhere: the engine
        // retires the orphaned note and never publishes the stale update.
        let provider = std::sync::Arc::new(SlowUpdate {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let params = consolidation_params(&memory_dir);
        let outcome = super::super::run_consolidation(provider, &params)
            .await
            .expect("consolidation ran");
        assert!(
            !outcome.merge_applied,
            "no merge may resurrect the deleted entry"
        );
        assert!(read_texts(&memory_dir).is_empty());
        assert!(
            !memory_dir
                .join("staging")
                .join("notes")
                .join("req-1.md")
                .exists(),
            "the orphaned correction note is retired"
        );
    }
}
