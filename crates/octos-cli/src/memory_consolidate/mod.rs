//! Memory consolidation engine (memory-refresh design, PR-4).
//!
//! Merges staging notes (`memory/staging/notes/`) and extraction files
//! (`memory/staging/extract/`) into `MEMORY.md` through ONE LLM merge call,
//! under machine-enforced authority gates: the model proposes ops, Rust
//! decides what is allowed based exclusively on host-written metadata.
//!
//! The engine is a leaf: everything arrives as parameters, all file IO stays
//! under the profile's memory directory. Wiring into schedulers/CLI lands in
//! a later integration PR (the PR-3 `memory_refresh` service is the caller).
//!
//! Run shape:
//! 1. load staging + MEMORY.md (INIT-migrate a legacy file in place);
//! 2. cancel expired pending-confirm notes (restore interim archives);
//! 3. skip early when there is nothing consumable — zero provider calls;
//! 4. bind free-text host forget notes to candidate entries (Rust-side);
//! 5. one `provider.chat()` (strict JSON; one corrective re-ask max);
//! 6. validate every op against the authority gates — any violation rejects
//!    the WHOLE merge, staging stays intact;
//! 7. apply in the sensitive-safe order (`apply.rs`), then delete consumed
//!    staging files.

pub mod apply;
pub mod entry;
pub mod ops;
pub mod pending;
pub mod prompt;
pub mod staging;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Utc};
use eyre::{Result, WrapErr};
use octos_core::Message;
use octos_llm::{ChatConfig, LlmProvider, TokenUsage};

use apply::{ApplyPlan, ScrubTarget, find_archive_block_by_hash};
use entry::{Entry, ParsedMemory, estimate_tokens, render_memory_md};
use ops::{CheckedOp, ValidationCtx};
use staging::{NoteFile, NoteKind, NoteOrigin, PendingCandidate, StagingBatch};

/// A staging file that participated in this many consecutive failed batches
/// is signalled for quarantine (the CALLER moves it; host/user_request notes
/// are never signalled).
const QUARANTINE_THRESHOLD: u32 = 2;
/// Failure-count sidecar (not `.md`, so staging scans ignore it).
const FAILURE_TRACKER_FILE: &str = ".consolidate_failures.json";

/// Parameters for one consolidation run.
#[derive(Debug, Clone)]
pub struct ConsolidateParams {
    /// The profile's memory directory (`<data_dir>/memory`).
    pub memory_dir: PathBuf,
    /// Token budget for the rendered MEMORY.md (CJK-aware local estimate).
    pub max_memory_file_tokens: usize,
    /// Age (days) past which a real `(updated:)` stamp allows auto-archive.
    pub unused_days: u32,
    /// Lifetime of a pending-confirm note before it cancels.
    pub pending_confirm_days: u32,
    /// Injected "today" — keeps prompts and stamps deterministic for tests;
    /// [`ConsolidateParams::new`] uses the current UTC date.
    pub today: NaiveDate,
}

impl ConsolidateParams {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self {
            memory_dir,
            max_memory_file_tokens: 8000,
            unused_days: 30,
            pending_confirm_days: 7,
            today: Utc::now().date_naive(),
        }
    }
}

/// Lifecycle state of a pending-confirm note as surfaced in the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// Parked this run (candidates bound, expiry stamped).
    Created,
    /// Still waiting for an id-bound confirmation or expiry.
    Waiting,
    /// Confirmed this run — the named candidate was hard-deleted, the others
    /// restored.
    Confirmed,
    /// Expired this run — interim-archived candidates restored, note deleted.
    Expired,
    /// A candidate hash failed verification — no destructive action was
    /// taken; candidates were recomputed and re-surfaced.
    HashMismatch,
}

/// One pending note surfaced in the outcome. Content is intentionally NOT
/// carried (sensitive notes stay scrubbed); only ids and metadata.
#[derive(Debug, Clone)]
pub struct PendingStatus {
    pub note_id: String,
    pub state: PendingState,
    pub candidate_ids: Vec<String>,
    pub sensitive: bool,
    pub expires_at: Option<DateTime<FixedOffset>>,
}

/// What one consolidation run did.
#[derive(Debug, Default)]
pub struct ConsolidateOutcome {
    /// Nothing to do at all: no staging, no pending, no INIT — zero provider
    /// calls, zero writes.
    pub skipped_clean: bool,
    /// This run migrated a legacy MEMORY.md to id form.
    pub init_performed: bool,
    /// The merge was validated and applied to disk.
    pub merge_applied: bool,
    /// Entry ids per applied op kind.
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub superseded: Vec<String>,
    pub archived: Vec<String>,
    pub hard_deleted: Vec<String>,
    /// Staging items the model dropped, with reasons.
    pub dropped: Vec<(String, String)>,
    /// Staging files deleted after a successful apply.
    pub consumed_staging_files: usize,
    /// Staging files whole-file-deleted by the hard-delete scrub.
    pub scrub_deleted_staging: Vec<PathBuf>,
    /// Every pending-confirm note this run touched or is still waiting on.
    pub pending_notes: Vec<PendingStatus>,
    /// Staging files that failed [`QUARANTINE_THRESHOLD`] consecutive
    /// batches — the caller moves them to `staging/quarantine/`. Never
    /// contains host or user_request notes.
    pub quarantine_candidates: Vec<PathBuf>,
    /// Combined provider usage (both calls when a re-ask happened).
    pub token_usage: TokenUsage,
    /// Everything that went wrong, human-readable. A rejected merge shows up
    /// here with `merge_applied == false` while staging stays intact.
    pub errors: Vec<String>,
}

fn pending_status(note: &NoteFile, state: PendingState) -> PendingStatus {
    PendingStatus {
        note_id: note.id.clone(),
        state,
        candidate_ids: note
            .candidates
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|c| c.entry_id.clone())
            .collect(),
        sensitive: note.sensitive,
        expires_at: note.expires_at,
    }
}

/// Run one consolidation pass. See the module docs for the run shape; every
/// failure mode is fail-closed (staging intact, reasons in
/// [`ConsolidateOutcome::errors`]). IO errors and provider transport errors
/// are `Err`; model misbehavior is a reported, tracked non-error.
pub async fn run_consolidation(
    provider: Arc<dyn LlmProvider>,
    params: &ConsolidateParams,
) -> Result<ConsolidateOutcome> {
    let memory_dir = params.memory_dir.as_path();
    std::fs::create_dir_all(memory_dir)
        .wrap_err_with(|| format!("failed to create {}", memory_dir.display()))?;
    let mut outcome = ConsolidateOutcome::default();

    // --- 1. load staging + MEMORY.md -----------------------------------
    let batch = staging::load_staging(memory_dir)?;
    for (path, err, _) in &batch.parse_failures {
        outcome
            .errors
            .push(format!("staging parse failure {}: {err}", path.display()));
    }

    let memory_path = memory_dir.join("MEMORY.md");
    let memory_content = match std::fs::read_to_string(&memory_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).wrap_err("failed to read MEMORY.md"),
    };
    let mut taken_ids: HashSet<String> = HashSet::new();
    let mut init_mode = false;
    let mut entries = match entry::parse_memory_md(&memory_content)? {
        ParsedMemory::Entries(entries) => entries,
        ParsedMemory::Legacy(blocks) => {
            // INIT: assign ids + `(updated: unknown, imported: <today>)`,
            // persisted immediately — a later merge failure must not undo
            // the 0-loss migration.
            let entries = entry::init_entries(&blocks, params.today, &mut taken_ids);
            apply::write_memory_md(memory_dir, &entries, true)?;
            init_mode = true;
            entries
        }
    };
    taken_ids.extend(entries.iter().map(|e| e.id.clone()));
    outcome.init_performed = init_mode;

    // --- 2. expiry pre-pass ---------------------------------------------
    let now: DateTime<FixedOffset> = params
        .today
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .fixed_offset();
    let mut waiting: Vec<NoteFile> = Vec::new();
    for note in &batch.pending {
        let expires_at = note.expires_at.expect("pending notes carry expires_at");
        if now > expires_at {
            let result = apply::apply_expiry(memory_dir, note, &mut entries)?;
            outcome.errors.extend(result.errors);
            taken_ids.extend(result.restored.iter().cloned());
            if result.note_deleted {
                outcome
                    .pending_notes
                    .push(pending_status(note, PendingState::Expired));
            } else {
                // Restore could not be verified — note kept, still freezing.
                outcome
                    .pending_notes
                    .push(pending_status(note, PendingState::Waiting));
                waiting.push(note.clone());
            }
        } else {
            waiting.push(note.clone());
        }
    }

    // --- 2.5 consume already-satisfied id-bound forgets -------------------
    // Crash recovery: the apply order deletes the authorizing note only
    // AFTER the new MEMORY.md is published. A crash in between leaves an
    // id-bound forget note whose target no longer exists anywhere — the
    // request was completed. Without this, the note would wedge every
    // future merge (the gates forbid dropping or pending it).
    let mut batch = batch;
    let mut satisfied: Vec<usize> = Vec::new();
    for (i, note) in batch.notes.iter().enumerate() {
        if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
            continue;
        }
        let named = note.named_entry_ids();
        if named.is_empty() {
            continue;
        }
        let any_alive = named.iter().any(|id| {
            entries.iter().any(|e| &e.id == id)
                || waiting.iter().any(|w| {
                    w.candidates
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|c| &c.entry_id == id)
                })
        });
        if !any_alive {
            satisfied.push(i);
        }
    }
    for i in satisfied.into_iter().rev() {
        let note = batch.notes.remove(i);
        apply::remove_file_if_exists(&note.path)?;
        outcome.dropped.push((
            note.id.clone(),
            "already satisfied: no named entry exists (completed by a prior run)".to_string(),
        ));
        tracing::info!(note = %note.path.display(), "satisfied forget note consumed");
    }

    // --- 3. skip early when nothing is consumable ------------------------
    let has_batch = !batch.notes.is_empty() || !batch.extractions.is_empty();
    if !has_batch {
        for note in &waiting {
            outcome
                .pending_notes
                .push(pending_status(note, PendingState::Waiting));
        }
        let parse_failed: Vec<PathBuf> = batch
            .parse_failures
            .iter()
            .map(|(p, _, _)| p.clone())
            .collect();
        let protected = protected_paths(&batch);
        outcome.quarantine_candidates =
            record_failures(memory_dir, &parse_failed, &[], &protected)?;
        outcome.skipped_clean = batch.is_clean() && !init_mode;
        return Ok(outcome);
    }

    // --- 4. bind free-text host forget notes ----------------------------
    // (idx into batch.notes, candidates). Frozen from this moment on.
    let mut new_pending: Vec<(usize, Vec<PendingCandidate>)> = Vec::new();
    for (i, note) in batch.notes.iter().enumerate() {
        if !note.is_free_text_forget() {
            continue;
        }
        let candidates = pending::compute_candidates(&note.content, &entries)
            .into_iter()
            .filter_map(|(entry_id, _)| {
                entries
                    .iter()
                    .find(|e| e.id == entry_id)
                    .map(|e| PendingCandidate {
                        entry_id,
                        content_hash: e.content_hash(),
                        interim_archived: false,
                    })
            })
            .collect();
        new_pending.push((i, candidates));
    }

    // --- 5. the merge call ------------------------------------------------
    let pending_for_prompt: Vec<(String, Vec<String>)> = waiting
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                n.candidates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| c.entry_id.clone())
                    .collect(),
            )
        })
        .chain(new_pending.iter().map(|(i, cands)| {
            (
                batch.notes[*i].id.clone(),
                cands.iter().map(|c| c.entry_id.clone()).collect(),
            )
        }))
        .collect();
    let mut frozen: HashSet<String> = pending_for_prompt
        .iter()
        .flat_map(|(_, ids)| ids.iter().cloned())
        .collect();
    let mut frozen_sorted: Vec<String> = frozen.iter().cloned().collect();
    frozen_sorted.sort();

    let user_message = prompt::build_user_message(&prompt::PromptInputs {
        entries: &entries,
        notes: &batch.notes,
        extractions: &batch.extractions,
        pending: &pending_for_prompt,
        frozen: &frozen_sorted,
        today: params.today,
        max_memory_file_tokens: params.max_memory_file_tokens,
        init_mode,
    });
    let config = ChatConfig::default();
    let mut messages = vec![
        Message::system(prompt::MEMORY_CONSOLIDATION_PROMPT),
        Message::user(user_message),
    ];

    let response = provider.chat(&messages, &[], &config).await?;
    accumulate_usage(&mut outcome.token_usage, &response.usage);
    let content = response.content.unwrap_or_default();

    let output = match ops::parse_model_output(&content) {
        Ok(output) => output,
        Err(first_err) => {
            // Exactly ONE corrective re-ask, then abort keeping staging.
            messages.push(Message::assistant(content));
            messages.push(Message::user(prompt::corrective_message(&first_err)));
            let retry = provider.chat(&messages, &[], &config).await?;
            accumulate_usage(&mut outcome.token_usage, &retry.usage);
            let retry_content = retry.content.unwrap_or_default();
            match ops::parse_model_output(&retry_content) {
                Ok(output) => output,
                Err(second_err) => {
                    finish_failed(
                        &mut outcome,
                        memory_dir,
                        &batch,
                        &waiting,
                        format!("model output unparseable after one re-ask: {second_err}"),
                    )?;
                    return Ok(outcome);
                }
            }
        }
    };

    // Model-suggested pending candidates may only ADD entries that also pass
    // the Rust-side binding check; they extend the frozen set BEFORE ops are
    // validated.
    for suggestion in &output.pending {
        let Some((idx, cands)) = new_pending
            .iter_mut()
            .find(|(i, _)| batch.notes[*i].id == suggestion.note_id)
            .map(|(i, c)| (*i, c))
        else {
            continue; // Suggestions for waiting notes are ignored (already bound).
        };
        let note = &batch.notes[idx];
        for entry_id in &suggestion.entry_ids {
            if cands.iter().any(|c| c.entry_id == *entry_id) {
                continue;
            }
            let Some(e) = entries.iter().find(|e| e.id == *entry_id) else {
                continue;
            };
            if pending::binding_score(&note.content, e).is_some() {
                cands.push(PendingCandidate {
                    entry_id: entry_id.clone(),
                    content_hash: e.content_hash(),
                    interim_archived: false,
                });
                frozen.insert(entry_id.clone());
            }
        }
    }

    // --- 6. validate -------------------------------------------------------
    let notes_map: HashMap<String, &NoteFile> =
        batch.notes.iter().map(|n| (n.id.clone(), n)).collect();
    let items_map: HashMap<String, &staging::ExtractionItem> = batch
        .extractions
        .iter()
        .flat_map(|e| e.items.iter())
        .map(|i| (i.id.clone(), i))
        .collect();
    let interim: HashMap<String, String> = waiting
        .iter()
        .flat_map(|n| n.candidates.as_deref().unwrap_or_default())
        .filter(|c| c.interim_archived)
        .map(|c| (c.entry_id.clone(), c.content_hash.clone()))
        .collect();

    let ctx = ValidationCtx {
        entries: &entries,
        interim: &interim,
        frozen: &frozen,
        notes: &notes_map,
        items: &items_map,
        init_mode,
        today: params.today,
        unused_days: params.unused_days,
    };
    let validated = match ops::validate(&output, &ctx) {
        Ok(validated) => validated,
        Err(reason) => {
            finish_failed(
                &mut outcome,
                memory_dir,
                &batch,
                &waiting,
                format!("merge rejected: {reason}"),
            )?;
            return Ok(outcome);
        }
    };

    // --- 7. confirmations (hash-verified) --------------------------------
    let confirmed_ids: HashSet<String> = validated
        .ops
        .iter()
        .filter_map(|op| match op {
            CheckedOp::HardDelete { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();

    let mut confirmed_note_ids: HashSet<String> = HashSet::new();
    // entry id → resolved pending note paths (for the scrub step).
    let mut originating: HashMap<String, Vec<PathBuf>> = HashMap::new();
    // entry id → archived block text for interim hard-delete targets.
    let mut interim_texts: HashMap<String, String> = HashMap::new();
    // Restores: entry id → (block text, exact archive block to remove).
    let mut restores: HashMap<String, String> = HashMap::new();

    for note in &waiting {
        let cands = note.candidates.as_deref().unwrap_or_default();
        let confirmed_here: Vec<&PendingCandidate> = cands
            .iter()
            .filter(|c| confirmed_ids.contains(&c.entry_id))
            .collect();
        if confirmed_here.is_empty() {
            continue;
        }
        // Hash-verify every candidate we are about to act on (the confirmed
        // ones AND the interim ones we must restore). Any mismatch → no
        // destructive action; recompute + re-surface.
        let mut mismatch: Option<String> = None;
        for cand in cands {
            let confirmed = confirmed_ids.contains(&cand.entry_id);
            if cand.interim_archived {
                match find_archive_block_by_hash(memory_dir, &cand.content_hash)? {
                    Some((_, block)) => {
                        if confirmed {
                            interim_texts.insert(cand.entry_id.clone(), block);
                        } else {
                            restores.insert(cand.entry_id.clone(), block);
                        }
                    }
                    None => {
                        mismatch = Some(cand.entry_id.clone());
                        break;
                    }
                }
            } else if confirmed {
                let live = entries.iter().find(|e| e.id == cand.entry_id);
                if live.is_none_or(|e| e.content_hash() != cand.content_hash) {
                    mismatch = Some(cand.entry_id.clone());
                    break;
                }
            }
        }
        if let Some(bad_id) = mismatch {
            // Recompute non-interim candidates against the live entries and
            // rewrite the note in place; interim ones keep their archive
            // binding untouched.
            let mut recomputed: Vec<PendingCandidate> = cands
                .iter()
                .filter(|c| c.interim_archived)
                .cloned()
                .collect();
            for (entry_id, _) in pending::compute_candidates(&note.content, &entries) {
                if recomputed.iter().any(|c| c.entry_id == entry_id) {
                    continue;
                }
                if let Some(e) = entries.iter().find(|e| e.id == entry_id) {
                    recomputed.push(PendingCandidate {
                        entry_id,
                        content_hash: e.content_hash(),
                        interim_archived: false,
                    });
                }
            }
            let expires_at = note.expires_at.expect("pending note has expiry");
            apply::atomic_write(&note.path, &note.render_pending(&recomputed, &expires_at))?;
            let mut status = pending_status(note, PendingState::HashMismatch);
            status.candidate_ids = recomputed.iter().map(|c| c.entry_id.clone()).collect();
            outcome.pending_notes.push(status);
            let remaining: Vec<NoteFile> = waiting
                .iter()
                .filter(|w| w.id != note.id)
                .cloned()
                .collect();
            finish_failed(
                &mut outcome,
                memory_dir,
                &batch,
                &remaining,
                format!(
                    "confirmation hash mismatch on candidate {bad_id} of pending note {} — \
                     no destructive action taken, candidates recomputed",
                    note.id
                ),
            )?;
            return Ok(outcome);
        }

        confirmed_note_ids.insert(note.id.clone());
        for cand in &confirmed_here {
            originating
                .entry(cand.entry_id.clone())
                .or_default()
                .push(note.path.clone());
        }
    }
    // A restore target that is itself being hard-deleted stays deleted.
    restores.retain(|id, _| !confirmed_ids.contains(id));

    // --- 8. apply ops in memory -------------------------------------------
    let mut working = entries.clone();
    let mut plan = ApplyPlan::default();

    for op in &validated.ops {
        match op {
            CheckedOp::Add { text, unverified } => {
                let id = entry::generate_id(&taken_ids);
                taken_ids.insert(id.clone());
                let text = ops::finalize_entry_text(text, *unverified, params.today, &id);
                working.push(Entry {
                    id: id.clone(),
                    text,
                });
                outcome.added.push(id);
            }
            CheckedOp::Update { id, new_text } => {
                let entry = working
                    .iter_mut()
                    .find(|e| e.id == *id)
                    .expect("validated update target exists");
                entry.text = ops::finalize_entry_text(new_text, false, params.today, id);
                outcome.updated.push(id.clone());
            }
            CheckedOp::Supersede {
                id, replacement, ..
            } => {
                let pos = working
                    .iter()
                    .position(|e| e.id == *id)
                    .expect("validated supersede target exists");
                plan.archive_appends.push(working[pos].text.clone());
                match replacement {
                    Some(replacement) => {
                        working[pos].text =
                            ops::finalize_entry_text(replacement, false, params.today, id);
                    }
                    None => {
                        working.remove(pos);
                    }
                }
                outcome.superseded.push(id.clone());
            }
            CheckedOp::Archive { id, .. } => {
                let pos = working
                    .iter()
                    .position(|e| e.id == *id)
                    .expect("validated archive target exists");
                plan.archive_appends.push(working[pos].text.clone());
                working.remove(pos);
                outcome.archived.push(id.clone());
            }
            CheckedOp::HardDelete { id, authorized_by } => {
                let text = match working.iter().position(|e| e.id == *id) {
                    Some(pos) => working.remove(pos).text,
                    None => interim_texts
                        .get(id)
                        .cloned()
                        .expect("interim hard-delete target resolved during confirmation"),
                };
                let scrub_entry = Entry {
                    id: id.clone(),
                    text,
                };
                let authorizing_note = notes_map
                    .get(authorized_by)
                    .expect("validated authorizing note exists")
                    .path
                    .clone();
                plan.hard_deletes.push(ScrubTarget {
                    entry_id: id.clone(),
                    folded_lines: scrub_entry.folded_lines(),
                    authorizing_note,
                    originating_pending: originating.get(id).cloned().unwrap_or_default(),
                });
                outcome.hard_deleted.push(id.clone());
            }
        }
    }

    // Confirmed pendings: restore every unconfirmed interim candidate
    // byte-identically and schedule the note files for deletion.
    for (entry_id, block) in &restores {
        if working.iter().any(|e| e.id == *entry_id) {
            continue; // already restored by a previous crashed run
        }
        working.push(Entry {
            id: entry_id.clone(),
            text: block.clone(),
        });
        taken_ids.insert(entry_id.clone());
        plan.archive_block_removals.push(block.clone());
    }
    for note in &waiting {
        if confirmed_note_ids.contains(&note.id) {
            plan.pending_deletes.push(note.path.clone());
            outcome
                .pending_notes
                .push(pending_status(note, PendingState::Confirmed));
        } else {
            outcome
                .pending_notes
                .push(pending_status(note, PendingState::Waiting));
        }
    }

    // --- 9. park new free-text forget notes as pending-confirm -----------
    for (idx, candidates) in &new_pending {
        let note = &batch.notes[*idx];
        let mut candidates: Vec<PendingCandidate> = candidates
            .iter()
            // A candidate hard-deleted this very run (by an id-bound note)
            // is gone; drop its stale binding.
            .filter(|c| working.iter().any(|e| e.id == c.entry_id))
            .cloned()
            .collect();
        if note.sensitive {
            // Interim-archive: hide candidates from MEMORY.md pending the
            // user's confirmation; blocks land in the archive for the
            // hash-verified restore.
            for cand in &mut candidates {
                let pos = working
                    .iter()
                    .position(|e| e.id == cand.entry_id)
                    .expect("candidate retained above");
                plan.archive_appends.push(working[pos].text.clone());
                working.remove(pos);
                cand.interim_archived = true;
            }
        }
        let expires_at = note.created_at + Duration::days(i64::from(params.pending_confirm_days));
        plan.pending_rewrites.push((
            note.path.clone(),
            note.render_pending(&candidates, &expires_at),
        ));
        outcome.pending_notes.push(PendingStatus {
            note_id: note.id.clone(),
            state: PendingState::Created,
            candidate_ids: candidates.iter().map(|c| c.entry_id.clone()).collect(),
            sensitive: note.sensitive,
            expires_at: Some(expires_at),
        });
    }

    // --- 10. budget gate ----------------------------------------------------
    let rendered = render_memory_md(&working);
    let tokens = estimate_tokens(&rendered);
    if tokens > params.max_memory_file_tokens {
        // Confirmed/Created statuses describe an apply that will not happen;
        // Expired ones already persisted in the pre-pass and must survive.
        // Waiting ones are re-added by `finish_failed`.
        outcome
            .pending_notes
            .retain(|p| p.state == PendingState::Expired);
        finish_failed(
            &mut outcome,
            memory_dir,
            &batch,
            &waiting,
            format!(
                "merge rejected: rendered MEMORY.md is {tokens} tokens (budget {})",
                params.max_memory_file_tokens
            ),
        )?;
        return Ok(outcome);
    }

    // --- 11. apply to disk ---------------------------------------------------
    let free_text_paths: HashSet<&PathBuf> = new_pending
        .iter()
        .map(|(i, _)| &batch.notes[*i].path)
        .collect();
    plan.consumed_files = batch
        .notes
        .iter()
        .filter(|n| !free_text_paths.contains(&n.path))
        .map(|n| n.path.clone())
        .chain(batch.extractions.iter().map(|e| e.path.clone()))
        .collect();
    plan.final_entries = working;

    let report = apply::apply_plan(memory_dir, params.today, &plan)?;
    outcome.consumed_staging_files = plan.consumed_files.len();
    outcome.scrub_deleted_staging = report.scrub_deleted_staging;
    outcome.dropped = validated
        .dropped
        .iter()
        .map(|d| (d.id.clone(), d.reason.clone()))
        .collect();
    outcome.merge_applied = true;

    // --- 12. failure tracking -------------------------------------------------
    let parse_failed: Vec<PathBuf> = batch
        .parse_failures
        .iter()
        .map(|(p, _, _)| p.clone())
        .collect();
    let succeeded: Vec<PathBuf> = batch
        .notes
        .iter()
        .map(|n| n.path.clone())
        .chain(batch.extractions.iter().map(|e| e.path.clone()))
        .collect();
    let protected = protected_paths(&batch);
    outcome.quarantine_candidates =
        record_failures(memory_dir, &parse_failed, &succeeded, &protected)?;

    Ok(outcome)
}

fn accumulate_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_tokens += usage.reasoning_tokens;
    total.cache_read_tokens += usage.cache_read_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
}

/// Staging paths that must NEVER be signalled for quarantine: host notes and
/// user_request notes (parsed), plus parse failures whose raw content claims
/// either.
fn protected_paths(batch: &StagingBatch) -> HashSet<PathBuf> {
    batch
        .notes
        .iter()
        .filter(|n| {
            n.origin == staging::NoteOrigin::Host || n.kind == staging::NoteKind::UserRequest
        })
        .map(|n| n.path.clone())
        .chain(
            batch
                .parse_failures
                .iter()
                .filter(|(_, _, protected)| *protected)
                .map(|(p, _, _)| p.clone()),
        )
        .collect()
}

/// Common tail for every rejected merge: record the failure for quarantine
/// tracking and surface the still-waiting pending notes.
fn finish_failed(
    outcome: &mut ConsolidateOutcome,
    memory_dir: &Path,
    batch: &StagingBatch,
    waiting: &[NoteFile],
    reason: String,
) -> Result<()> {
    outcome.errors.push(reason);
    outcome.merge_applied = false;
    for note in waiting {
        outcome
            .pending_notes
            .push(pending_status(note, PendingState::Waiting));
    }
    let failed: Vec<PathBuf> = batch
        .notes
        .iter()
        .map(|n| n.path.clone())
        .chain(batch.extractions.iter().map(|e| e.path.clone()))
        .chain(batch.parse_failures.iter().map(|(p, _, _)| p.clone()))
        .collect();
    let protected = protected_paths(batch);
    outcome.quarantine_candidates = record_failures(memory_dir, &failed, &[], &protected)?;
    Ok(())
}

fn tracker_path(memory_dir: &Path) -> PathBuf {
    memory_dir.join("staging").join(FAILURE_TRACKER_FILE)
}

fn tracker_key(memory_dir: &Path, path: &Path) -> String {
    path.strip_prefix(memory_dir.join("staging"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Update the consecutive-failure counts: increment `failed`, clear
/// `succeeded`, prune entries whose files are gone. Returns the quarantine
/// candidates among `failed` (threshold reached, not protected).
fn record_failures(
    memory_dir: &Path,
    failed: &[PathBuf],
    succeeded: &[PathBuf],
    protected: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if failed.is_empty() && succeeded.is_empty() {
        return Ok(Vec::new());
    }
    let path = tracker_path(memory_dir);
    let mut counts: HashMap<String, u32> = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(e).wrap_err("failed to read failure tracker"),
    };

    for p in succeeded {
        counts.remove(&tracker_key(memory_dir, p));
    }
    let mut quarantine = Vec::new();
    for p in failed {
        let key = tracker_key(memory_dir, p);
        let count = counts.entry(key).or_insert(0);
        *count += 1;
        if *count >= QUARANTINE_THRESHOLD && !protected.contains(p) {
            quarantine.push(p.clone());
        }
    }
    counts.retain(|key, _| memory_dir.join("staging").join(key).exists());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    apply::atomic_write(&path, &serde_json::to_string(&counts)?)?;
    Ok(quarantine)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use octos_core::MessageRole;
    use octos_llm::{ChatResponse, StopReason, ToolSpec};

    use super::entry::sha256_hex;
    use super::*;

    // --- scripted provider ------------------------------------------------

    struct ScriptedProvider {
        replies: Mutex<VecDeque<String>>,
        calls: Mutex<Vec<Vec<Message>>>,
    }

    impl ScriptedProvider {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn call_messages(&self, idx: usize) -> Vec<Message> {
            self.calls.lock().unwrap()[idx].clone()
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            self.calls.lock().unwrap().push(messages.to_vec());
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| eyre::eyre!("unexpected LLM call"))?;
            Ok(ChatResponse {
                content: Some(reply),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "scripted"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    // --- fixtures -----------------------------------------------------------

    const TODAY: &str = "2026-07-07";

    fn params(dir: &Path) -> ConsolidateParams {
        ConsolidateParams {
            memory_dir: dir.to_path_buf(),
            max_memory_file_tokens: 8000,
            unused_days: 30,
            pending_confirm_days: 7,
            today: NaiveDate::parse_from_str(TODAY, "%Y-%m-%d").unwrap(),
        }
    }

    fn write_memory(dir: &Path, blocks: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut content = blocks.join("\n\n");
        content.push('\n');
        std::fs::write(dir.join("MEMORY.md"), content).unwrap();
    }

    fn read_memory(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("MEMORY.md")).unwrap_or_default()
    }

    fn write_note(
        dir: &Path,
        name: &str,
        origin: &str,
        kind: &str,
        content: &str,
        sensitive: bool,
    ) -> PathBuf {
        let notes = dir.join("staging/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let mut raw =
            format!("---\norigin: {origin}\nkind: {kind}\ncreated_at: 2026-07-01T10:00:00+00:00\n");
        if sensitive {
            raw.push_str("sensitive: true\n");
        }
        raw.push_str(&format!("---\n\n{content}\n"));
        let path = notes.join(format!("{name}.md"));
        std::fs::write(&path, raw).unwrap();
        path
    }

    fn write_extract(dir: &Path, name: &str, items_json: &str) -> PathBuf {
        let extract = dir.join("staging/extract");
        std::fs::create_dir_all(&extract).unwrap();
        let raw = format!(
            "---\nextracted_at: 2026-07-06T08:00:00+00:00\nmodel: \"test-model\"\n---\n{{\"items\":{items_json}}}\n"
        );
        let path = extract.join(format!("{name}.md"));
        std::fs::write(&path, raw).unwrap();
        path
    }

    /// Write an already-pending note file (candidates + expires_at stamped).
    fn write_pending_note(
        dir: &Path,
        name: &str,
        content: &str,
        sensitive: bool,
        candidates_json: &str,
        expires_at: &str,
    ) -> PathBuf {
        let notes = dir.join("staging/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let mut raw = String::from(
            "---\norigin: host\nkind: forget\ncreated_at: 2026-06-30T10:00:00+00:00\n",
        );
        if sensitive {
            raw.push_str("sensitive: true\n");
        }
        raw.push_str(&format!(
            "candidates: {candidates_json}\nexpires_at: {expires_at}\n---\n\n{content}\n"
        ));
        let path = notes.join(format!("{name}.md"));
        std::fs::write(&path, raw).unwrap();
        path
    }

    async fn run(
        provider: &Arc<ScriptedProvider>,
        params: &ConsolidateParams,
    ) -> ConsolidateOutcome {
        run_consolidation(provider.clone() as Arc<dyn LlmProvider>, params)
            .await
            .expect("run_consolidation must not hard-error in tests")
    }

    // --- tests ---------------------------------------------------------------

    #[tokio::test]
    async fn should_skip_with_zero_provider_calls_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let provider = ScriptedProvider::new(&[]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.skipped_clean);
        assert!(!outcome.merge_applied);
        assert_eq!(provider.call_count(), 0);
        assert_eq!(outcome.token_usage.input_tokens, 0);
        assert_eq!(
            read_memory(dir.path()),
            "Fact. (updated: 2026-06-01) ^maaaaaa\n",
            "no writes on a clean skip"
        );
    }

    #[tokio::test]
    async fn should_apply_update_and_preserve_other_entries_when_evidence_qualifies() {
        let dir = tempfile::tempdir().unwrap();
        let stale = "Lives in Portland. (updated: 2026-05-01) ^maaaaaa";
        let keep = "Prefers tabs over spaces\nfor all langs. (updated: 2026-06-01) ^mbbbbbb";
        write_memory(dir.path(), &[stale, keep]);
        let extract_path = write_extract(
            dir.path(),
            "01ex-sess",
            r#"[{"kind":"correction","content":"moved to Seattle","evidence_kind":"user_said","evidence_idx":[3],"date":"2026-07-06"}]"#,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"update","id":"^maaaaaa","new_text":"Lives in Seattle. (updated: 2026-07-06)","sources":["01ex-sess#0"]}],"consumed_ids":["01ex-sess#0"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.updated, vec!["^maaaaaa"]);
        assert_eq!(provider.call_count(), 1);
        assert_eq!(outcome.token_usage.input_tokens, 100);

        let memory = read_memory(dir.path());
        assert!(memory.contains("Lives in Seattle. (updated: 2026-07-06) ^maaaaaa"));
        assert!(memory.contains(keep), "untouched entry byte-preserved");
        assert!(!memory.contains("Portland"));
        // Previous content in .bak (merge without hard deletes).
        let bak = std::fs::read_to_string(dir.path().join("MEMORY.md.bak")).unwrap();
        assert!(bak.contains("Portland"));
        assert!(
            !extract_path.exists(),
            "consumed staging deleted after successful apply"
        );
        assert_eq!(outcome.consumed_staging_files, 1);
    }

    #[tokio::test]
    async fn should_render_unverified_marker_when_add_sources_are_model_notes_only() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        write_note(
            dir.path(),
            "01no-rust",
            "model",
            "fact",
            "likes rust",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes Rust.","sources":["01no-rust"]}],"consumed_ids":["01no-rust"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.added.len(), 1);
        let memory = read_memory(dir.path());
        assert!(
            memory.contains("Likes Rust. (unverified) (updated: 2026-07-07)"),
            "weakly-sourced add must carry (unverified): {memory}"
        );
        let parsed = entry::parse_memory_md(&memory).unwrap();
        assert!(matches!(parsed, ParsedMemory::Entries(e) if e.len() == 2));
    }

    #[tokio::test]
    async fn should_reject_merge_when_update_sourced_only_from_model_note() {
        let dir = tempfile::tempdir().unwrap();
        let original = "Lives in Portland. (updated: 2026-05-01) ^maaaaaa";
        write_memory(dir.path(), &[original]);
        let note_path = write_note(
            dir.path(),
            "01no-move",
            "model",
            "fact",
            "moved to Seattle",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"update","id":"^maaaaaa","new_text":"Lives in Seattle.","sources":["01no-move"]}],"consumed_ids":["01no-move"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(!outcome.merge_applied);
        assert_eq!(
            provider.call_count(),
            1,
            "validation failures get NO re-ask"
        );
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("validated authority")),
            "errors: {:?}",
            outcome.errors
        );
        assert_eq!(read_memory(dir.path()), format!("{original}\n"));
        assert!(note_path.exists(), "staging intact after rejected merge");
    }

    #[tokio::test]
    async fn should_reject_merge_when_hard_delete_authorized_by_model_note() {
        let dir = tempfile::tempdir().unwrap();
        let original = "Secret fact. (updated: 2026-05-01) ^maaaaaa";
        write_memory(dir.path(), &[original]);
        let note_path = write_note(
            dir.path(),
            "01no-forget",
            "model",
            "user_request",
            "user asked to forget id:^maaaaaa",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^maaaaaa","authorized_by":"01no-forget"}],"consumed_ids":["01no-forget"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(!outcome.merge_applied);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("not a host forget note")),
            "errors: {:?}",
            outcome.errors
        );
        assert_eq!(read_memory(dir.path()), format!("{original}\n"));
        assert!(note_path.exists());
    }

    #[tokio::test]
    async fn should_park_free_text_forget_as_pending_with_hash_bound_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let seattle = "Bought the seattle house in 2019. (updated: 2026-06-01) ^maaaaaa";
        let jazz = "Enjoys jazz records. (updated: 2026-06-02) ^mbbbbbb";
        write_memory(dir.path(), &[seattle, jazz]);
        let note_path = write_note(
            dir.path(),
            "01fg-house",
            "host",
            "forget",
            "forget the seattle house",
            false,
        );

        let provider = ScriptedProvider::new(&[r#"{"ops":[],"consumed_ids":[],"dropped":[]}"#]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.pending_notes.len(), 1);
        let status = &outcome.pending_notes[0];
        assert_eq!(status.state, PendingState::Created);
        assert_eq!(status.candidate_ids, vec!["^maaaaaa"]);

        // Note rewritten in place with hash-bound candidates + expiry.
        let rewritten = std::fs::read_to_string(&note_path).unwrap();
        let note = staging::parse_note(&note_path, &rewritten).unwrap();
        assert!(note.is_pending());
        let cands = note.candidates.as_deref().unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].entry_id, "^maaaaaa");
        assert_eq!(cands[0].content_hash, sha256_hex(seattle));
        assert!(!cands[0].interim_archived);
        // expires_at = created_at (2026-07-01T10:00) + 7 days.
        assert_eq!(
            note.expires_at.unwrap(),
            DateTime::parse_from_rfc3339("2026-07-08T10:00:00+00:00").unwrap()
        );
        // Non-sensitive: MEMORY.md untouched.
        assert!(read_memory(dir.path()).contains(seattle));
    }

    #[tokio::test]
    async fn should_ignore_pending_suggestions_when_binding_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let seattle = "Bought the seattle house in 2019. (updated: 2026-06-01) ^maaaaaa";
        let jazz = "Enjoys jazz records. (updated: 2026-06-02) ^mcccccc";
        write_memory(dir.path(), &[seattle, jazz]);
        let note_path = write_note(
            dir.path(),
            "01fg-house",
            "host",
            "forget",
            "forget the seattle house",
            false,
        );

        // Model suggests an unrelated entry as a candidate — it does not
        // pass the Rust-side binding check and must be ignored.
        let provider = ScriptedProvider::new(&[
            r#"{"ops":[],"consumed_ids":[],"dropped":[],"pending":[{"note_id":"01fg-house","entry_ids":["^mcccccc"]}]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        let rewritten = std::fs::read_to_string(&note_path).unwrap();
        let note = staging::parse_note(&note_path, &rewritten).unwrap();
        let candidate_ids: Vec<&str> = note
            .candidates
            .as_deref()
            .unwrap()
            .iter()
            .map(|c| c.entry_id.as_str())
            .collect();
        assert_eq!(
            candidate_ids,
            ["^maaaaaa"],
            "suggestion failing the binding check is ignored"
        );
    }

    #[tokio::test]
    async fn should_interim_archive_candidates_when_sensitive_forget_parked() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "Allergic to bees, keeps epipen. (updated: 2026-06-01) ^maaaaaa";
        let keep = "Enjoys jazz records. (updated: 2026-06-02) ^mbbbbbb";
        write_memory(dir.path(), &[secret, keep]);
        let note_path = write_note(
            dir.path(),
            "01fg-bees",
            "host",
            "forget",
            "forget the allergic to bees thing",
            true,
        );

        let provider = ScriptedProvider::new(&[r#"{"ops":[],"consumed_ids":[],"dropped":[]}"#]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        // Candidate hidden from MEMORY.md, parked in the archive.
        let memory = read_memory(dir.path());
        assert!(!memory.contains("bees"));
        assert!(memory.contains(keep));
        let archived = std::fs::read_to_string(dir.path().join("archive/2026-07.md")).unwrap();
        assert!(archived.contains(secret));

        let rewritten = std::fs::read_to_string(&note_path).unwrap();
        let note = staging::parse_note(&note_path, &rewritten).unwrap();
        let cands = note.candidates.as_deref().unwrap();
        assert_eq!(cands.len(), 1);
        assert!(cands[0].interim_archived);
        assert_eq!(cands[0].content_hash, sha256_hex(secret));
    }

    #[tokio::test]
    async fn should_delete_scrub_and_restore_when_id_bound_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let block_a = "Allergic to bees, keeps epipen. (updated: 2026-05-01) ^maaaaaa";
        let block_b = "Second sensitive detail here. (updated: 2026-05-02) ^mbbbbbb";
        let keep = "Keeps bonsai. (updated: 2026-06-01) ^mcccccc";
        write_memory(memory_dir, &[keep]);

        // Interim-archived candidates from the earlier sensitive parking.
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-06.md"),
            format!("{block_a}\n\n{block_b}\n"),
        )
        .unwrap();
        // Stale backups + a bank entity holding a copy of the secret line.
        std::fs::write(memory_dir.join("MEMORY.md.bak"), format!("{block_a}\n")).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md.prev"), "old").unwrap();
        let bank = memory_dir.join("bank/entities");
        std::fs::create_dir_all(&bank).unwrap();
        std::fs::write(
            bank.join("user.md"),
            format!("Loves gardening.\n{block_a}\n"),
        )
        .unwrap();

        let pending_path = write_pending_note(
            memory_dir,
            "01fg-bees",
            "forget the allergic to bees thing",
            true,
            &format!(
                r#"[{{"entry_id":"^maaaaaa","content_hash":"{}","interim_archived":true}},{{"entry_id":"^mbbbbbb","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(block_a),
                sha256_hex(block_b)
            ),
            "2026-07-10T10:00:00+00:00",
        );
        // A model staging note quoting the secret line verbatim.
        let quoting = write_note(memory_dir, "02no-quote", "model", "fact", block_a, false);
        // The id-bound confirmation.
        let confirm = write_note(
            memory_dir,
            "03fg-confirm",
            "host",
            "forget",
            "confirmed, forget id:^maaaaaa",
            true,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^maaaaaa","authorized_by":"03fg-confirm"}],"consumed_ids":["03fg-confirm"],"dropped":[{"id":"02no-quote","reason":"quotes deleted content"}]}"#,
        ]);
        let outcome = run(&provider, &params(memory_dir)).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.hard_deleted, vec!["^maaaaaa"]);

        // MEMORY.md: keep + byte-identical restore of the unconfirmed
        // candidate; deleted content gone; NO .bak after a hard delete.
        let memory = read_memory(memory_dir);
        assert!(memory.contains(keep));
        assert!(memory.contains(block_b), "unconfirmed candidate restored");
        assert!(!memory.contains("bees"));
        assert!(!memory_dir.join("MEMORY.md.bak").exists());
        assert!(!memory_dir.join("MEMORY.md.prev").exists());
        let restored = entry::parse_memory_md(&memory).unwrap();
        assert!(matches!(restored, ParsedMemory::Entries(e) if e.len() == 2));

        // Archive: confirmed block scrubbed, restored block moved out.
        assert!(
            !memory_dir.join("archive/2026-06.md").exists(),
            "archive file emptied by scrub + restore is deleted"
        );
        // Bank entity: secret line scrubbed, rest kept.
        let entity = std::fs::read_to_string(bank.join("user.md")).unwrap();
        assert!(entity.contains("Loves gardening."));
        assert!(!entity.contains("bees"));

        // Note files: authorizing + originating pending + quoting all gone.
        assert!(!confirm.exists());
        assert!(!pending_path.exists());
        assert!(!quoting.exists());
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.state == PendingState::Confirmed && p.note_id == "01fg-bees")
        );
    }

    #[tokio::test]
    async fn should_reject_update_when_target_is_frozen_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let frozen_entry = "Bought the seattle house in 2019. (updated: 2026-06-01) ^maaaaaa";
        write_memory(dir.path(), &[frozen_entry]);
        write_pending_note(
            dir.path(),
            "01fg-house",
            "forget the seattle house",
            false,
            &format!(
                r#"[{{"entry_id":"^maaaaaa","content_hash":"{}","interim_archived":false}}]"#,
                sha256_hex(frozen_entry)
            ),
            "2026-07-10T10:00:00+00:00",
        );
        let note_path = write_note(
            dir.path(),
            "02no-host",
            "host",
            "fact",
            "sold the seattle house",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"update","id":"^maaaaaa","new_text":"Sold the seattle house.","sources":["02no-host"]}],"consumed_ids":["02no-host"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(!outcome.merge_applied);
        assert!(
            outcome.errors.iter().any(|e| e.contains("frozen")),
            "errors: {:?}",
            outcome.errors
        );
        assert!(read_memory(dir.path()).contains(frozen_entry));
        assert!(note_path.exists());
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.state == PendingState::Waiting)
        );
    }

    #[tokio::test]
    async fn should_cancel_and_restore_when_pending_note_expires() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let hidden = "Allergic to bees, keeps epipen. (updated: 2026-05-01) ^maaaaaa";
        let keep = "Keeps bonsai. (updated: 2026-06-01) ^mcccccc";
        write_memory(memory_dir, &[keep]);
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(memory_dir.join("archive/2026-06.md"), format!("{hidden}\n")).unwrap();

        let pending_path = write_pending_note(
            memory_dir,
            "01fg-bees",
            "forget the allergic to bees thing",
            true,
            &format!(
                r#"[{{"entry_id":"^maaaaaa","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(hidden)
            ),
            "2026-07-01T10:00:00+00:00", // already past (today = 2026-07-07)
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run(&provider, &params(memory_dir)).await;

        assert_eq!(provider.call_count(), 0, "expiry needs no model");
        assert!(!outcome.skipped_clean);
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.state == PendingState::Expired)
        );
        assert!(!pending_path.exists(), "expired note deleted");
        let memory = read_memory(memory_dir);
        assert!(memory.contains(hidden), "byte-identical restore");
        assert!(memory.contains(keep));
        assert!(
            !memory_dir.join("archive/2026-06.md").exists(),
            "restored block removed from archive"
        );
    }

    #[tokio::test]
    async fn should_assign_ids_and_unknown_stamps_when_init_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MEMORY.md"),
            "Old fact one.\n\nOld fact two\nwith a second line.\n",
        )
        .unwrap();

        let provider = ScriptedProvider::new(&[]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.init_performed);
        assert!(!outcome.skipped_clean);
        assert_eq!(provider.call_count(), 0);
        let memory = read_memory(dir.path());
        assert!(memory.contains("Old fact one. (updated: unknown, imported: 2026-07-07) ^m"));
        assert!(memory.contains("Old fact two\nwith a second line. (updated: unknown"));
        let ParsedMemory::Entries(entries) = entry::parse_memory_md(&memory).unwrap() else {
            panic!("INIT output must parse as id-bearing entries");
        };
        assert_eq!(entries.len(), 2, "0-loss");
    }

    #[tokio::test]
    async fn should_reject_lossy_output_when_init_run_with_staging() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "Old fact one.\n\nOld two.\n").unwrap();
        let note_path = write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);

        // Model tries to archive during INIT (any non-add is lossy).
        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"archive","id":"^mzzzzzz","reason":"cleanup","sources":[]}],"consumed_ids":["01no-x"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.init_performed, "INIT itself persists");
        assert!(!outcome.merge_applied, "lossy INIT output rejected");
        assert!(
            outcome.errors.iter().any(|e| e.contains("0-loss")),
            "errors: {:?}",
            outcome.errors
        );
        assert!(note_path.exists(), "staging intact");
        let memory = read_memory(dir.path());
        assert!(memory.contains("Old fact one."));
        assert!(memory.contains("Old two."));
    }

    #[tokio::test]
    async fn should_apply_adds_when_init_run_with_staging() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "Old fact one.\n").unwrap();
        write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["01no-x"]}],"consumed_ids":["01no-x"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.init_performed);
        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        let memory = read_memory(dir.path());
        assert!(memory.contains("Old fact one. (updated: unknown"));
        assert!(memory.contains("Likes tea. (unverified) (updated: 2026-07-07)"));
    }

    #[tokio::test]
    async fn should_reject_merge_when_over_half_of_entries_removed() {
        let dir = tempfile::tempdir().unwrap();
        let blocks: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    "Fact number {i} here. (updated: 2026-01-01) ^maaaaa{}",
                    char::from(b'a' + i)
                )
            })
            .collect();
        let refs: Vec<&str> = blocks.iter().map(|s| s.as_str()).collect();
        write_memory(dir.path(), &refs);
        write_note(dir.path(), "01no-x", "model", "fact", "irrelevant", false);

        // All 5 archives are individually age-qualified, but 5/8 > 50%.
        let ops: Vec<String> = (0..5)
            .map(|i| {
                format!(
                    r#"{{"op":"archive","id":"^maaaaa{}","reason":"stale","sources":[]}}"#,
                    char::from(b'a' + i)
                )
            })
            .collect();
        let reply = format!(
            r#"{{"ops":[{}],"consumed_ids":["01no-x"],"dropped":[]}}"#,
            ops.join(",")
        );
        let provider = ScriptedProvider::new(&[&reply]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(!outcome.merge_applied);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("entry-loss guard")),
            "errors: {:?}",
            outcome.errors
        );
        let ParsedMemory::Entries(entries) =
            entry::parse_memory_md(&read_memory(dir.path())).unwrap()
        else {
            panic!("expected entries");
        };
        assert_eq!(entries.len(), 8, "nothing was removed");
    }

    #[tokio::test]
    async fn should_reask_exactly_once_then_abort_when_parse_fails() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let note_path = write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);

        let provider =
            ScriptedProvider::new(&["I consolidated everything, boss!", "still not json"]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert_eq!(provider.call_count(), 2, "exactly one re-ask");
        assert!(!outcome.merge_applied);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("unparseable after one re-ask")),
            "errors: {:?}",
            outcome.errors
        );
        assert!(note_path.exists(), "staging intact after abort");

        // The re-ask appends the bad reply + a corrective user message.
        let retry_messages = provider.call_messages(1);
        assert_eq!(retry_messages.len(), 4);
        assert_eq!(retry_messages[2].role, MessageRole::Assistant);
        assert_eq!(retry_messages[3].role, MessageRole::User);
        assert!(retry_messages[3].content.contains("rejected"));
    }

    #[tokio::test]
    async fn should_apply_merge_when_reask_recovers() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            "```oops not json",
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["01no-x"]}],"consumed_ids":["01no-x"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert_eq!(provider.call_count(), 2);
        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.token_usage.input_tokens, 200, "both calls counted");
        assert!(read_memory(dir.path()).contains("Likes tea."));
    }

    #[tokio::test]
    async fn should_reject_merge_when_host_note_not_consumed() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let note_path = write_note(
            dir.path(),
            "01no-host",
            "host",
            "fact",
            "works at Acme now",
            false,
        );

        let provider = ScriptedProvider::new(&[r#"{"ops":[],"consumed_ids":[],"dropped":[]}"#]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(!outcome.merge_applied);
        assert!(
            outcome
                .errors
                .iter()
                .any(|e| e.contains("was not consumed")),
            "errors: {:?}",
            outcome.errors
        );
        assert!(note_path.exists());
    }

    #[tokio::test]
    async fn should_archive_superseded_text_when_supersede_applied() {
        let dir = tempfile::tempdir().unwrap();
        let old = "Uses a 2019 MacBook. (updated: 2026-01-01) ^maaaaaa";
        write_memory(dir.path(), &[old]);
        write_note(
            dir.path(),
            "01no-host",
            "host",
            "correction",
            "replaced the laptop",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"supersede","id":"^maaaaaa","replacement":null,"reason":"device replaced","sources":["01no-host"]}],"consumed_ids":["01no-host"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.superseded, vec!["^maaaaaa"]);
        assert!(!read_memory(dir.path()).contains("MacBook"));
        let archived = std::fs::read_to_string(dir.path().join("archive/2026-07.md")).unwrap();
        assert!(
            archived.contains(old),
            "superseded text archived with its real stamps"
        );
    }

    #[tokio::test]
    async fn should_signal_quarantine_after_two_failed_batches_except_protected() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let model_note = write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);
        let host_note = write_note(dir.path(), "02no-h", "host", "fact", "at Acme", false);

        // Two runs, both ending in double parse failure.
        for _ in 0..2 {
            let provider = ScriptedProvider::new(&["junk", "junk"]);
            let outcome = run(&provider, &params(dir.path())).await;
            assert!(!outcome.merge_applied);
            if outcome.quarantine_candidates.is_empty() {
                continue;
            }
            assert!(outcome.quarantine_candidates.contains(&model_note));
            assert!(
                !outcome.quarantine_candidates.contains(&host_note),
                "host notes are never quarantine candidates"
            );
        }

        let provider = ScriptedProvider::new(&["junk", "junk"]);
        let outcome = run(&provider, &params(dir.path())).await;
        assert!(outcome.quarantine_candidates.contains(&model_note));
        assert!(!outcome.quarantine_candidates.contains(&host_note));
        assert!(model_note.exists(), "engine only signals; caller moves");
    }

    #[tokio::test]
    async fn should_reject_merge_when_token_budget_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let note_path = write_note(dir.path(), "01no-x", "model", "fact", "likes tea", false);

        let mut p = params(dir.path());
        p.max_memory_file_tokens = 10;
        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"A very long new entry that obviously blows the tiny budget configured for this test.","sources":["01no-x"]}],"consumed_ids":["01no-x"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &p).await;

        assert!(!outcome.merge_applied);
        assert!(
            outcome.errors.iter().any(|e| e.contains("budget")),
            "errors: {:?}",
            outcome.errors
        );
        assert!(note_path.exists());
        assert!(!read_memory(dir.path()).contains("very long new entry"));
    }

    #[tokio::test]
    async fn should_take_no_destructive_action_and_recompute_when_confirm_hash_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        // The live entry was edited since the pending note bound it, so the
        // stored candidate hash no longer matches.
        let live = "Bought the seattle house in 2019. (updated: 2026-06-05) ^maaaaaa";
        write_memory(memory_dir, &[live]);
        let pending_path = write_pending_note(
            memory_dir,
            "01fg-house",
            "forget the seattle house",
            false,
            &format!(
                r#"[{{"entry_id":"^maaaaaa","content_hash":"{}","interim_archived":false}}]"#,
                sha256_hex("Bought the seattle house in 2019. (updated: 2026-06-01) ^maaaaaa")
            ),
            "2026-07-10T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes, forget id:^maaaaaa",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^maaaaaa","authorized_by":"02fg-confirm"}],"consumed_ids":["02fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run(&provider, &params(memory_dir)).await;

        assert!(!outcome.merge_applied, "no destructive action on mismatch");
        assert!(
            outcome.errors.iter().any(|e| e.contains("hash mismatch")),
            "errors: {:?}",
            outcome.errors
        );
        // The entry is untouched, the confirmation note stays for a retry.
        assert!(read_memory(memory_dir).contains(live));
        assert!(confirm.exists());
        // The pending note was recomputed in place: fresh hash of the live
        // text, still pending.
        let rewritten = std::fs::read_to_string(&pending_path).unwrap();
        let note = staging::parse_note(&pending_path, &rewritten).unwrap();
        assert!(note.is_pending());
        let cands = note.candidates.as_deref().unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].content_hash, sha256_hex(live));
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.state == PendingState::HashMismatch)
        );
    }

    #[tokio::test]
    async fn should_surface_dropped_items_when_merge_applies() {
        let dir = tempfile::tempdir().unwrap();
        write_memory(dir.path(), &["Fact. (updated: 2026-06-01) ^maaaaaa"]);
        let note_path = write_note(dir.path(), "01no-x", "model", "fact", "noise", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[],"consumed_ids":[],"dropped":[{"id":"01no-x","reason":"transient noise"}]}"#,
        ]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert!(outcome.merge_applied, "errors: {:?}", outcome.errors);
        assert_eq!(
            outcome.dropped,
            vec![("01no-x".to_string(), "transient noise".to_string())]
        );
        assert!(
            !note_path.exists(),
            "dropped-with-reason files are consumed"
        );
    }

    #[tokio::test]
    async fn should_consume_satisfied_forget_note_when_target_already_gone() {
        // Crash-window recovery: MEMORY.md was published without the entry,
        // but the authorizing id-bound note survived. The note must be
        // consumed pre-merge (zero provider calls) instead of wedging every
        // future batch.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let note_path = write_note(
            memory_dir,
            "01fg-satisfied",
            "host",
            "forget",
            "confirmed delete of id:^mzzzzzz",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let params = params(memory_dir);
        let outcome = run_consolidation(provider.clone(), &params).await.unwrap();

        assert_eq!(
            provider.call_count(),
            0,
            "satisfied note must not need a merge"
        );
        assert!(!note_path.exists(), "satisfied note must be consumed");
        assert!(
            outcome
                .dropped
                .iter()
                .any(|(id, reason)| id == "01fg-satisfied" && reason.contains("satisfied")),
            "outcome must surface the completion: {:?}",
            outcome.dropped
        );
        // The untouched entry survives byte-identically.
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(memory.contains("^mcccccc"));
    }

    #[tokio::test]
    async fn should_remove_whole_archived_block_when_older_version_scrubbed() {
        // An archived OLDER version of a hard-deleted entry has lines that
        // differ from the current text; line-exact scrubbing would leave
        // those sensitive remnants behind. The whole block must go.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let current = "Wifi password is hunter2. (updated: 2026-07-01) ^msecret";
        write_memory(
            memory_dir,
            &[current, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-05.md"),
            "Old wifi password was letmein99.\nRouter in the hallway closet. (updated: 2026-05-01) ^msecret\n\nUnrelated archived fact. (updated: 2026-04-01) ^mother1\n",
        )
        .unwrap();
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            true,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
        ]);
        let params = params(memory_dir);
        let outcome = run_consolidation(provider, &params).await.unwrap();
        assert_eq!(outcome.hard_deleted, vec!["^msecret"]);

        let archived = std::fs::read_to_string(memory_dir.join("archive/2026-05.md")).unwrap();
        assert!(
            !archived.contains("letmein99") && !archived.contains("hallway closet"),
            "older archived version must be removed whole: {archived}"
        );
        assert!(
            archived.contains("^mother1"),
            "unrelated archived block must survive"
        );
    }
}
