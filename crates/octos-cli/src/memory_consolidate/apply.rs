//! Disk mutation layer — the sensitive-safe, id-exact apply order.
//!
//! For a validated merge:
//! 1. hard-delete scrub: delete authorizing/originating note files, delete
//!    `.bak`/`.prev` backups outright (never scrub-edit them), scrub
//!    LINE-EXACT matches from archive and bank entity files, whole-file
//!    delete any staging file containing an exact entry line;
//! 2. atomically write the new MEMORY.md (temp + fsync + rename, same dir;
//!    `.bak` copy of the previous file ONLY on merges without hard deletes);
//! 3. append archived entries to `memory/archive/YYYY-MM.md`;
//! 4. delete consumed staging files and write pending-note rewrites.
//!
//! A crash anywhere leaves staging intact until step 4, so a re-run is safe.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use eyre::{Result, WrapErr};

use super::entry::{Entry, fold_whitespace, render_memory_md, sha256_hex, split_blocks};
use super::staging::NoteFile;

/// One hard-deleted entry's scrub set.
#[derive(Debug, Clone)]
pub struct ScrubTarget {
    pub entry_id: String,
    /// Whitespace-folded lines of the entry's own stored text (LINE-EXACT
    /// matching only — never tokens or substrings).
    pub folded_lines: Vec<String>,
    /// The id-bound host forget note that authorized this delete.
    pub authorizing_note: PathBuf,
    /// Pending notes resolved by this delete (confirmations).
    pub originating_pending: Vec<PathBuf>,
}

impl ScrubTarget {
    fn line_matches(&self, line: &str) -> bool {
        if line.contains(&self.entry_id) {
            return true;
        }
        // Content-level comparison: the candidate line is stripped of
        // bookkeeping exactly like the stored scrub lines, so the same
        // fact under another id/stamp still matches.
        super::entry::content_folded_lines(line)
            .first()
            .is_some_and(|folded| self.folded_lines.contains(folded))
    }
}

/// The full validated plan, resolved down to file operations.
#[derive(Debug, Default)]
pub struct ApplyPlan {
    /// MEMORY.md content after every op, restore and interim archive.
    pub final_entries: Vec<Entry>,
    /// Blocks to append to this month's archive file (archive ops, superseded
    /// texts, interim-archived pending candidates). Real stamps preserved.
    pub archive_appends: Vec<String>,
    /// Hard-delete scrub sets (empty on merges without hard deletes).
    pub hard_deletes: Vec<ScrubTarget>,
    /// Exact archive blocks to remove because they were restored into
    /// MEMORY.md (confirmation path).
    pub archive_block_removals: Vec<String>,
    /// Consumed staging files to delete in step 4.
    pub consumed_files: Vec<PathBuf>,
    /// Pending-note rewrites (path → full new content) in step 4.
    pub pending_rewrites: Vec<(PathBuf, String)>,
    /// Resolved pending notes to delete.
    pub pending_deletes: Vec<PathBuf>,
}

/// What the apply pass actually did (informational).
#[derive(Debug, Default)]
pub struct ApplyReport {
    /// Staging files whole-file-deleted because they contained an exact line
    /// of a hard-deleted entry.
    pub scrub_deleted_staging: Vec<PathBuf>,
}

/// Atomically write `content` to `path`: same-dir temp file, fsync, rename.
pub fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre::eyre!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(dir).wrap_err_with(|| format!("failed to create {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .wrap_err_with(|| format!("failed to create temp file in {}", dir.display()))?;
    tmp.write_all(content.as_bytes())
        .wrap_err("failed to write temp file")?;
    tmp.as_file().sync_all().wrap_err("failed to fsync")?;
    tmp.persist(path)
        .wrap_err_with(|| format!("failed to rename into {}", path.display()))?;
    Ok(())
}

/// Write MEMORY.md atomically. When `backup` is set, the previous file is
/// copied to `MEMORY.md.bak` first (merges without hard deletes).
pub fn write_memory_md(memory_dir: &Path, entries: &[Entry], backup: bool) -> Result<()> {
    let path = memory_dir.join("MEMORY.md");
    if backup && path.exists() {
        std::fs::copy(&path, memory_dir.join("MEMORY.md.bak"))
            .wrap_err("failed to copy MEMORY.md.bak")?;
    }
    atomic_write(&path, &render_memory_md(entries))
}

pub(super) fn remove_file_if_exists(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).wrap_err_with(|| format!("failed to delete {}", path.display())),
    }
}

/// Recursively delete `*.bak` / `*.prev` files under `dir`. Backups are
/// deleted outright on hard-delete merges — never scrub-edited.
#[cfg(test)]
pub(super) fn delete_backups_for_test(dir: &Path) -> Result<()> {
    delete_backups(dir)
}

fn delete_backups(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).wrap_err_with(|| format!("failed to list {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // symlink_metadata: the scrub must stay scoped to the profile's
        // memory tree — following a symlinked directory would delete
        // backups elsewhere on the filesystem.
        let file_type = std::fs::symlink_metadata(&path)
            .wrap_err_with(|| format!("failed to stat {}", path.display()))?
            .file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            delete_backups(&path)?;
        } else if path.extension().is_some_and(|e| e == "bak" || e == "prev") {
            remove_file_if_exists(&path)?;
        }
    }
    Ok(())
}

fn list_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e).wrap_err_with(|| format!("failed to list {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "md") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// All archive month files.
pub fn archive_files(memory_dir: &Path) -> Result<Vec<PathBuf>> {
    list_md_files(&memory_dir.join("archive"))
}

/// Find a block in any archive file whose exact text hashes to
/// `content_hash`. Byte-identical restore starts here.
pub fn find_archive_block_by_hash(
    memory_dir: &Path,
    content_hash: &str,
) -> Result<Option<(PathBuf, String)>> {
    for path in archive_files(memory_dir)? {
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        for block in split_blocks(&content) {
            if sha256_hex(&block) == content_hash {
                return Ok(Some((path, block)));
            }
        }
    }
    Ok(None)
}

/// Rewrite one block file: drop exact blocks in `block_removals`, drop lines
/// matching any scrub target, delete the file entirely when nothing is left.
///
/// `drop_blocks_naming_id`: for ARCHIVE files, any block carrying a
/// hard-deleted entry's `^m…` id token is an archived VERSION of that
/// entry — older versions have lines that differ from the current entry
/// text, so line-exact scrubbing would leave sensitive remnants. Such
/// blocks are removed whole. Bank entity pages keep line-exact scrubbing
/// (their blocks are not entry versions; whole-block removal would be
/// collateral).
fn rewrite_block_file(
    path: &Path,
    block_removals: &[String],
    scrubs: &[ScrubTarget],
    drop_blocks_naming_id: bool,
) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let mut kept_blocks: Vec<String> = Vec::new();
    for block in split_blocks(&content) {
        if block_removals.contains(&block) {
            continue;
        }
        if drop_blocks_naming_id && scrubs.iter().any(|s| block.contains(&s.entry_id)) {
            continue;
        }
        let kept: Vec<&str> = block
            .lines()
            .filter(|line| !scrubs.iter().any(|s| s.line_matches(line)))
            .collect();
        if !kept.is_empty() {
            kept_blocks.push(kept.join("\n"));
        }
    }
    if kept_blocks.is_empty() {
        remove_file_if_exists(path)?;
        return Ok(());
    }
    let mut out = kept_blocks.join("\n\n");
    out.push('\n');
    if out != content {
        atomic_write(path, &out)?;
    }
    Ok(())
}

/// Injectable daily-note files (`YYYY-MM-DD.md`) in the memory root —
/// they feed `get_injectable_context` and must honor hard-delete scrubs.
fn daily_note_files(memory_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(memory_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).wrap_err("failed to read memory dir"),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let is_date = stem.len() == 10
            && stem.chars().enumerate().all(|(i, c)| match i {
                4 | 7 => c == '-',
                _ => c.is_ascii_digit(),
            });
        if is_date && path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(out)
}

/// Line-scrub one daily-note file; delete it when nothing is left.
fn scrub_daily_note(path: &Path, scrubs: &[ScrubTarget]) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let kept: Vec<&str> = content
        .lines()
        .filter(|line| !scrubs.iter().any(|s| s.line_matches(line)))
        .collect();
    let mut out = kept.join("\n");
    if out.trim().is_empty() {
        remove_file_if_exists(path)?;
        return Ok(());
    }
    out.push('\n');
    if out != content {
        atomic_write(path, &out)?;
    }
    Ok(())
}

/// True when the file's FRONTMATTER declares `origin: host`. Only the
/// fenced header counts — untrusted note bodies could contain the literal
/// and must not gain scrub immunity.
pub(super) fn frontmatter_declares_host(content: &str) -> bool {
    let Some(after) = content.strip_prefix("---") else {
        return false;
    };
    let Some(rest) = after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix('\n'))
    else {
        return false;
    };
    for line in rest.lines() {
        if line.trim_end() == "---" {
            return false;
        }
        if line.trim() == "origin: host" {
            return true;
        }
    }
    false
}

/// Does this staging file contain an exact (whitespace-folded) line of any
/// hard-deleted entry? Whole-file-delete if so — staging bodies are
/// untrusted and cannot be safely partially edited.
fn staging_file_matches(content: &str, scrubs: &[ScrubTarget]) -> bool {
    content
        .lines()
        .any(|line| scrubs.iter().any(|s| s.line_matches(line)))
}

/// Execute the apply order.
///
/// Crash-safety invariants the ordering guarantees:
/// - Archived/interim content is appended (step 2) BEFORE the pending
///   binding claims `interim_archived` (step 2.5) and BEFORE the entries
///   leave MEMORY.md (step 3) — the confirmation path resolves interim
///   candidates by hash in the archive, so the copy must exist first;
///   content is never in neither place, and re-runs skip identical
///   blocks instead of duplicating.
/// - The hash-bound pending metadata (step 2.5) is durable BEFORE any
///   entry is hidden from MEMORY.md — a crash can never orphan
///   interim-archived candidates without their binding record; the
///   orchestrator re-hides still-live interim candidates on recovery.
/// - The authorizing forget note (step 4) outlives the MEMORY.md write —
///   a crash before publication leaves the durable request in staging and
///   the next run re-plans the delete; a re-run whose target id is
///   already gone treats the note as satisfied (see the orchestrator).
/// - Restored blocks leave the archive (step 4) only AFTER they are back
///   in MEMORY.md.
pub fn apply_plan(memory_dir: &Path, today: NaiveDate, plan: &ApplyPlan) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    let has_hard_deletes = !plan.hard_deletes.is_empty();
    if !has_hard_deletes && !plan.archive_block_removals.is_empty() {
        // Restores can only originate from confirmations (which imply hard
        // deletes) — reaching here is a plan-construction bug.
        eyre::bail!("archive block removals without hard deletes");
    }

    // --- step 1: hard-delete content scrub -------------------------------
    // (Content copies only. The authorizing/pending NOTE files are the
    // durable record of the request and are deleted in step 4, after the
    // new MEMORY.md is published.)
    if has_hard_deletes {
        // Backups deleted outright — a .bak must never outlive the entry it
        // preserves.
        delete_backups(memory_dir)?;

        for path in archive_files(memory_dir)? {
            // Whole-block removal for blocks naming the deleted id: an
            // archived OLDER version's lines differ from the current entry
            // text, so line-exact scrubbing would leave remnants.
            rewrite_block_file(&path, &[], &plan.hard_deletes, true)?;
        }
        for path in list_md_files(&memory_dir.join("bank").join("entities"))? {
            rewrite_block_file(&path, &[], &plan.hard_deletes, false)?;
        }
        // Daily notes are injected into prompts too — scrub them.
        for path in daily_note_files(memory_dir)? {
            scrub_daily_note(&path, &plan.hard_deletes)?;
        }

        for dir in ["notes", "extract"] {
            for path in list_md_files(&memory_dir.join("staging").join(dir))? {
                // The request notes themselves survive until step 4.
                let is_request_note = plan
                    .hard_deletes
                    .iter()
                    .any(|t| t.authorizing_note == path || t.originating_pending.contains(&path));
                if is_request_note {
                    continue;
                }
                let content = std::fs::read_to_string(&path)
                    .wrap_err_with(|| format!("failed to read {}", path.display()))?;
                // ALL host notes are durable asks with their own lifecycle
                // (consumption / pending-confirm / expiry) — whole-file
                // scrub-deleting one that merely quotes a deleted line
                // would orphan its parked candidates (binding written in
                // step 2.5) or silently drop the ask. FRONTMATTER decides:
                // an untrusted body containing the literal gains nothing.
                if dir == "notes" && frontmatter_declares_host(&content) {
                    continue;
                }
                if staging_file_matches(&content, &plan.hard_deletes) {
                    remove_file_if_exists(&path)?;
                    report.scrub_deleted_staging.push(path);
                }
            }
        }
    }

    // --- step 2: archive appends (before MEMORY.md loses the entries) ----
    if !plan.archive_appends.is_empty() {
        let dir = memory_dir.join("archive");
        std::fs::create_dir_all(&dir)
            .wrap_err_with(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join(format!("{}.md", today.format("%Y-%m")));
        let existing = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e).wrap_err_with(|| format!("failed to read {}", path.display()));
            }
        };
        let mut blocks = split_blocks(&existing);
        for block in &plan.archive_appends {
            // A merge can contain BOTH a hard_delete and archive/supersede
            // ops; appended blocks must honor the scrub set or step 2 would
            // write hard-deleted content right back into the archive.
            if plan
                .hard_deletes
                .iter()
                .any(|t| block.contains(&t.entry_id))
            {
                continue;
            }
            let cleaned: Vec<&str> = block
                .lines()
                .filter(|line| !plan.hard_deletes.iter().any(|t| t.line_matches(line)))
                .collect();
            if cleaned.is_empty() {
                continue;
            }
            let cleaned = cleaned.join("\n");
            // Duplicate guard: a crash between append and the MEMORY.md
            // write re-plans the same append on the next run.
            if !blocks.contains(&cleaned) {
                blocks.push(cleaned);
            }
        }
        let mut out = blocks.join("\n\n");
        out.push('\n');
        atomic_write(&path, &out)?;
    }

    // --- step 2.5: persist pending bindings -------------------------------
    // AFTER the archive copies exist (the confirmation path resolves
    // interim candidates by hash in the archive) and BEFORE MEMORY.md
    // hides the entries (a binding must never be orphaned).
    let scrubbed_so_far: HashSet<&PathBuf> = report.scrub_deleted_staging.iter().collect();
    for (path, content) in &plan.pending_rewrites {
        // A pending note that quoted a hard-deleted entry line was already
        // whole-file-deleted in step 1 — do not resurrect it.
        if scrubbed_so_far.contains(path) {
            continue;
        }
        atomic_write(path, content)?;
    }

    // --- step 3: MEMORY.md -------------------------------------------------
    // The scrub set applies to the PRIMARY file too: another live/restored
    // entry can carry an exact line of the deleted entry (shared facts) —
    // publishing final_entries unfiltered would keep the sensitive line in
    // MEMORY.md after the confirmation.
    let final_entries: Vec<super::entry::Entry> = if has_hard_deletes {
        plan.final_entries
            .iter()
            .filter_map(|entry| {
                let kept: Vec<&str> = entry
                    .text
                    .lines()
                    .filter(|line| !plan.hard_deletes.iter().any(|t| t.line_matches(line)))
                    .collect();
                if kept.is_empty() {
                    return None;
                }
                let mut text = kept.join("\n");
                // An entry reduced to bookkeeping only carries no content.
                if super::pending::strippable_entry_text(&super::entry::Entry {
                    id: entry.id.clone(),
                    text: text.clone(),
                })
                .trim()
                .is_empty()
                {
                    return None;
                }
                // The scrub may have removed the id-bearing line; an id-less
                // block would parse as mixed/legacy next run and brick the
                // engine. Re-stamp the survivor.
                if !text.contains(&entry.id) {
                    text.push_str(&format!(
                        "\n(updated: {}) {}",
                        today.format("%Y-%m-%d"),
                        entry.id
                    ));
                }
                Some(super::entry::Entry {
                    id: entry.id.clone(),
                    text,
                })
            })
            .collect()
    } else {
        plan.final_entries.clone()
    };
    write_memory_md(memory_dir, &final_entries, !has_hard_deletes)?;

    // --- step 4: consume request notes + restored archive blocks ---------
    if has_hard_deletes {
        for target in &plan.hard_deletes {
            remove_file_if_exists(&target.authorizing_note)?;
            for pending in &target.originating_pending {
                remove_file_if_exists(pending)?;
            }
        }
        if !plan.archive_block_removals.is_empty() {
            for path in archive_files(memory_dir)? {
                rewrite_block_file(&path, &plan.archive_block_removals, &[], false)?;
            }
        }
    }
    for path in &plan.pending_deletes {
        remove_file_if_exists(path)?;
    }

    // --- step 5: staging cleanup ----------------------------------------
    let scrubbed: HashSet<&PathBuf> = report.scrub_deleted_staging.iter().collect();
    for path in &plan.consumed_files {
        if !scrubbed.contains(path) {
            remove_file_if_exists(path)?;
        }
    }

    Ok(report)
}

/// Does any archive block name this entry id?
pub(super) fn archive_names_id(memory_dir: &Path, entry_id: &str) -> Result<bool> {
    for path in archive_files(memory_dir)? {
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        if content.contains(entry_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Scrub a hard-delete target that exists ONLY in the archive (the live
/// entry was archived/superseded earlier). Id-bound host authority is
/// complete without a merge: remove every archive block naming the id,
/// line-scrub bank copies, delete backups, and whole-file-delete staging
/// copies. Returns false when no archive block names the id.
pub(super) fn scrub_archived_only_target(memory_dir: &Path, entry_id: &str) -> Result<bool> {
    // Collect the folded lines of every archived version first — they form
    // the scrub set for bank/staging copies.
    let mut folded_lines: Vec<String> = Vec::new();
    let mut found = false;
    for path in archive_files(memory_dir)? {
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        for block in split_blocks(&content) {
            if block.contains(entry_id) {
                found = true;
                // Content-stripped shape (same as live-entry scrub sets):
                // copies in daily notes / bank pages differ by bookkeeping.
                folded_lines.extend(super::entry::content_folded_lines(&block));
            }
        }
    }
    if !found {
        return Ok(false);
    }
    let target = ScrubTarget {
        entry_id: entry_id.to_string(),
        folded_lines,
        authorizing_note: PathBuf::new(),
        originating_pending: Vec::new(),
    };
    let targets = [target];

    delete_backups(memory_dir)?;
    for path in archive_files(memory_dir)? {
        rewrite_block_file(&path, &[], &targets, true)?;
    }
    for path in list_md_files(&memory_dir.join("bank").join("entities"))? {
        rewrite_block_file(&path, &[], &targets, false)?;
    }
    for path in daily_note_files(memory_dir)? {
        scrub_daily_note(&path, &targets)?;
    }
    for dir in ["notes", "extract"] {
        for path in list_md_files(&memory_dir.join("staging").join(dir))? {
            let content = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            // Same host-ask immunity as the main scrub: a mixed forget note
            // naming this archived id AND a live id must survive until the
            // merge honors the live part — deleting it here would lose the
            // request if that merge fails.
            if dir == "notes" && frontmatter_declares_host(&content) {
                continue;
            }
            if staging_file_matches(&content, &targets) {
                remove_file_if_exists(&path)?;
            }
        }
    }
    Ok(true)
}

/// Outcome of processing one expired pending note.
#[derive(Debug, Default)]
pub struct ExpiryResult {
    /// Entry ids restored byte-identically from the archive.
    pub restored: Vec<String>,
    /// Restore problems (hash not found / mismatch). When non-empty the note
    /// file is KEPT so nothing is lost.
    pub errors: Vec<String>,
    pub note_deleted: bool,
}

/// Expiry = cancel: restore every interim-archived candidate byte-identically
/// (hash-verified), then delete the pending note. `entries` is the live
/// MEMORY.md entry list — restored entries are appended to it and MEMORY.md
/// is written HERE, before the archive copies are removed, so a crash can
/// never leave a restored entry existing nowhere.
///
/// Idempotent across crashes: a candidate already present in MEMORY.md with
/// a matching hash counts as restored (its archive copy, if any, is still
/// removed).
pub fn apply_expiry(
    memory_dir: &Path,
    note: &NoteFile,
    entries: &mut Vec<Entry>,
) -> Result<ExpiryResult> {
    let mut result = ExpiryResult::default();
    let candidates = note.candidates.clone().unwrap_or_default();

    // Resolve first, mutate only if everything checks out.
    let mut to_restore: Vec<Entry> = Vec::new();
    let mut archive_removals: Vec<(PathBuf, String)> = Vec::new();
    for cand in candidates.iter().filter(|c| c.interim_archived) {
        if let Some(existing) = entries.iter().find(|e| e.id == cand.entry_id) {
            if existing.content_hash() == cand.content_hash {
                // Already restored by a previous (crashed) run.
                if let Some(found) = find_archive_block_by_hash(memory_dir, &cand.content_hash)? {
                    archive_removals.push(found);
                }
                continue;
            }
            result.errors.push(format!(
                "pending note {}: candidate {} exists with a different hash — not touching it",
                note.id, cand.entry_id
            ));
            continue;
        }
        match find_archive_block_by_hash(memory_dir, &cand.content_hash)? {
            Some((path, block)) => {
                to_restore.push(Entry {
                    id: cand.entry_id.clone(),
                    text: block.clone(),
                });
                archive_removals.push((path, block));
            }
            None => result.errors.push(format!(
                "pending note {}: no archive block matches candidate {} hash — cannot restore",
                note.id, cand.entry_id
            )),
        }
    }

    if !result.errors.is_empty() {
        // Fail closed: no restore, no note deletion — surface and retry.
        return Ok(result);
    }

    // 1. MEMORY.md gains the restored entries and is persisted BEFORE the
    //    archive copies disappear (no hard deletes here → .bak kept).
    if !to_restore.is_empty() {
        for entry in to_restore {
            result.restored.push(entry.id.clone());
            entries.push(entry);
        }
        write_memory_md(memory_dir, entries, true)?;
    }
    // 2. Remove restored blocks from their archive files.
    for (path, block) in archive_removals {
        rewrite_block_file(&path, std::slice::from_ref(&block), &[], false)?;
    }
    // 3. Delete the pending note (content scrubbed by deletion; the caller
    //    surfaces only the note id for sensitive notes).
    remove_file_if_exists(&note.path)?;
    result.note_deleted = true;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_consolidate::staging::parse_note;

    fn entry(id: &str, text: &str) -> Entry {
        Entry {
            id: id.into(),
            text: text.into(),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 7).unwrap()
    }

    #[test]
    fn should_write_bak_when_merge_has_no_hard_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        std::fs::write(memory_dir.join("MEMORY.md"), "Old. ^maaaaaa\n").unwrap();

        let plan = ApplyPlan {
            final_entries: vec![entry("^maaaaaa", "New. ^maaaaaa")],
            ..Default::default()
        };
        apply_plan(memory_dir, today(), &plan).unwrap();

        assert_eq!(
            std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap(),
            "New. ^maaaaaa\n"
        );
        assert_eq!(
            std::fs::read_to_string(memory_dir.join("MEMORY.md.bak")).unwrap(),
            "Old. ^maaaaaa\n",
            "previous content preserved in .bak"
        );
    }

    #[test]
    fn should_scrub_everywhere_when_hard_delete_applied() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "Has a secret allergy to bees. (updated: 2026-06-01) ^msecret";

        std::fs::write(
            memory_dir.join("MEMORY.md"),
            format!("{secret_line}\n\nKeeps bonsai. ^mkeepit\n"),
        )
        .unwrap();
        std::fs::write(memory_dir.join("MEMORY.md.bak"), "old backup").unwrap();
        std::fs::write(memory_dir.join("MEMORY.md.prev"), "older backup").unwrap();

        let archive_dir = memory_dir.join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("2026-05.md"),
            format!("Unrelated archived. ^mother\n\n{secret_line}\n"),
        )
        .unwrap();

        let bank = memory_dir.join("bank/entities");
        std::fs::create_dir_all(&bank).unwrap();
        std::fs::write(bank.join("user.md"), format!("{secret_line}\n")).unwrap();
        std::fs::write(bank.join("garden.md"), "Grows kale.\n").unwrap();

        let notes = memory_dir.join("staging/notes");
        std::fs::create_dir_all(&notes).unwrap();
        // Authorizing note + an unrelated staging file quoting the entry.
        let auth = notes.join("01-forget.md");
        std::fs::write(&auth, "---\norigin: host\nkind: forget\ncreated_at: 2026-07-01T00:00:00Z\n---\n\nforget id:^msecret\n").unwrap();
        let quoting = notes.join("02-quote.md");
        std::fs::write(
            &quoting,
            format!("---\norigin: model\nkind: fact\ncreated_at: 2026-07-01T00:00:00Z\n---\n\n{secret_line}\n"),
        )
        .unwrap();

        let scrub = ScrubTarget {
            entry_id: "^msecret".into(),
            folded_lines: vec![fold_whitespace(secret_line)],
            authorizing_note: auth.clone(),
            originating_pending: vec![],
        };
        let plan = ApplyPlan {
            final_entries: vec![entry("^mkeepit", "Keeps bonsai. ^mkeepit")],
            hard_deletes: vec![scrub],
            ..Default::default()
        };
        let report = apply_plan(memory_dir, today(), &plan).unwrap();

        // MEMORY.md excludes deleted content by construction; no .bak left.
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("secret allergy"));
        assert!(!memory_dir.join("MEMORY.md.bak").exists());
        assert!(!memory_dir.join("MEMORY.md.prev").exists());

        // Archive keeps the unrelated block, loses the secret line.
        let archived = std::fs::read_to_string(archive_dir.join("2026-05.md")).unwrap();
        assert!(archived.contains("Unrelated archived."));
        assert!(!archived.contains("secret allergy"));

        // Bank entity left empty is deleted; unrelated entity untouched.
        assert!(!bank.join("user.md").exists());
        assert!(bank.join("garden.md").exists());

        // Authorizing note gone; quoting staging file whole-file-deleted.
        assert!(!auth.exists());
        assert!(!quoting.exists());
        assert_eq!(report.scrub_deleted_staging, vec![quoting]);
    }

    #[test]
    fn should_append_archive_blocks_when_plan_archives() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let plan = ApplyPlan {
            final_entries: vec![],
            archive_appends: vec![
                "Old fact. (updated: 2026-01-01) ^molder1".to_string(),
                "Older fact. (updated: 2025-12-01) ^molder2".to_string(),
            ],
            ..Default::default()
        };
        apply_plan(memory_dir, today(), &plan).unwrap();
        let archived = std::fs::read_to_string(memory_dir.join("archive/2026-07.md")).unwrap();
        assert_eq!(
            archived,
            "Old fact. (updated: 2026-01-01) ^molder1\n\nOlder fact. (updated: 2025-12-01) ^molder2\n"
        );

        // Re-applying the same plan (crash-window re-run: archive appended
        // but MEMORY.md not yet published) must NOT duplicate blocks.
        apply_plan(memory_dir, today(), &plan).unwrap();
        let archived2 = std::fs::read_to_string(memory_dir.join("archive/2026-07.md")).unwrap();
        assert_eq!(archived2.matches("Old fact.").count(), 1);
        assert_eq!(archived, archived2);
    }

    #[test]
    fn should_restore_byte_identical_when_expiry_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let block = "Sensitive thing. (updated: 2026-06-01) ^msenstv";
        let hash = sha256_hex(block);

        let archive_dir = memory_dir.join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("2026-06.md"),
            format!("{block}\n\nOther archived. ^mother2\n"),
        )
        .unwrap();

        let notes = memory_dir.join("staging/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let note_path = notes.join("01-forget.md");
        let note_raw = format!(
            "---\norigin: host\nkind: forget\ncreated_at: 2026-06-20T00:00:00Z\nsensitive: true\n\
             candidates: [{{\"entry_id\":\"^msenstv\",\"content_hash\":\"{hash}\",\"interim_archived\":true}}]\n\
             expires_at: 2026-06-27T00:00:00Z\n---\n\nforget the sensitive thing\n"
        );
        std::fs::write(&note_path, &note_raw).unwrap();
        let note = parse_note(&note_path, &note_raw).unwrap();

        let mut entries = vec![entry("^mkeepit", "Keeps bonsai. ^mkeepit")];
        let result = apply_expiry(memory_dir, &note, &mut entries).unwrap();

        assert_eq!(result.restored, vec!["^msenstv"]);
        assert!(result.note_deleted);
        assert!(result.errors.is_empty());
        assert!(!note_path.exists());
        assert_eq!(entries[1].text, block, "byte-identical restore");
        // MEMORY.md was persisted by the expiry itself (before the archive
        // copy was removed).
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(memory.contains(block));
        let archived = std::fs::read_to_string(archive_dir.join("2026-06.md")).unwrap();
        assert!(!archived.contains("Sensitive thing."));
        assert!(archived.contains("Other archived."));
    }

    #[test]
    fn should_keep_note_when_expiry_restore_cannot_verify() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        // No archive block matches the candidate hash.
        let notes = memory_dir.join("staging/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let note_path = notes.join("01-forget.md");
        let note_raw = "---\norigin: host\nkind: forget\ncreated_at: 2026-06-20T00:00:00Z\n\
             candidates: [{\"entry_id\":\"^mgonexx\",\"content_hash\":\"00\",\"interim_archived\":true}]\n\
             expires_at: 2026-06-27T00:00:00Z\n---\n\nforget it\n";
        std::fs::write(&note_path, note_raw).unwrap();
        let note = parse_note(&note_path, note_raw).unwrap();

        let mut entries = vec![];
        let result = apply_expiry(memory_dir, &note, &mut entries).unwrap();
        assert!(!result.note_deleted);
        assert!(!result.errors.is_empty());
        assert!(note_path.exists(), "note kept so nothing is lost");
        assert!(entries.is_empty());
    }
}
