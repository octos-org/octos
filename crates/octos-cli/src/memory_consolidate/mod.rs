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
//! 5. one `provider.chat()` (strict JSON; one corrective re-ask max). A
//!    failed call fails the merge and counts against the batch's asks;
//! 6. validate every op against the authority gates — any violation rejects
//!    the WHOLE merge, staging stays intact;
//! 7. apply in the sensitive-safe order (`apply.rs`), then delete consumed
//!    staging files.

pub mod apply;
pub mod direct;
pub mod entry;
pub mod ops;
pub mod pending;
pub mod prompt;
pub mod staging;

#[cfg(test)]
mod consolidate_test_support;

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
/// An ask the model must honor (a host note other than a forget, or any
/// user_request) is never quarantined: after this many consecutive failed
/// merges it is parked in `staging/failed/` with a reason its requester can
/// read (#3253). Retrying it forever spent provider tokens on every pass and
/// told nobody.
pub const STUCK_ASK_THRESHOLD: u32 = 3;
/// Where a stuck ask is parked, beside its reason (`<stem>.json`).
const FAILED_ASKS_DIR: &str = "failed";

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
    /// When false, run only the no-provider phases (INIT persist, expiry,
    /// satisfied-forget consumption, crash-recovery re-hide) and stop
    /// before the merge call — the service sets this when the daily budget
    /// is exhausted so `pending_confirm_days` stays honored on busy
    /// profiles.
    pub allow_merge: bool,
    /// Injected "today" — keeps prompts and stamps deterministic for tests;
    /// [`ConsolidateParams::new`] uses the current UTC date.
    pub today: NaiveDate,
    /// Injected wall clock for the backoff of a failing ask.
    pub now: DateTime<Utc>,
}

impl ConsolidateParams {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self {
            memory_dir,
            max_memory_file_tokens: 8000,
            unused_days: 30,
            pending_confirm_days: 7,
            allow_merge: true,
            // Local date — budgets, daily notes, and extraction dates all
            // use the profile machine's local calendar; UTC would stamp
            // evening runs with tomorrow's date on western timezones.
            today: chrono::Local::now().date_naive(),
            now: Utc::now(),
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
    /// Staging items dropped by the model or retired Rust-side, with reasons.
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
    /// Asks parked in `staging/failed/` this run after
    /// [`STUCK_ASK_THRESHOLD`] failed merges.
    pub failed_asks: Vec<PathBuf>,
    /// Asks left out of this run's batch while they wait out their backoff.
    pub held_back_asks: usize,
    /// Combined provider usage (both calls when a re-ask happened).
    pub token_usage: TokenUsage,
    /// Number of provider chat() calls this run made (0 on skip/no-merge
    /// paths). Lets the caller charge budgets for FAILED merges whose
    /// providers report zero usage.
    pub provider_attempts: u32,
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
/// [`ConsolidateOutcome::errors`]). IO errors are `Err`; a failed model
/// call and model misbehavior are reported, tracked non-errors.
pub async fn run_consolidation(
    provider: Arc<dyn LlmProvider>,
    params: &ConsolidateParams,
) -> Result<ConsolidateOutcome> {
    let memory_dir = params.memory_dir.as_path();
    std::fs::create_dir_all(memory_dir)
        .wrap_err_with(|| format!("failed to create {}", memory_dir.display()))?;
    // Mutual exclusion with direct `memory/edit` / `memory/delete` RPCs
    // (UPCR-2026-RD1002E7E): hold the profile's write lock for the WHOLE
    // run — from the MEMORY.md snapshot below through the final apply — so
    // a direct mutation can never land between the read and the publish
    // (which would lose the edit or resurrect a deleted entry). A direct
    // mutation arriving mid-run waits and then applies on the published
    // state, where its version precondition is re-checked.
    let _write_guard = direct::lock_memory_writes(memory_dir).await?;
    let mut outcome = ConsolidateOutcome::default();

    // --- 1. load staging + MEMORY.md -----------------------------------
    let mut batch = staging::load_staging(memory_dir)?;
    outcome.held_back_asks = hold_back_failing_asks(memory_dir, &mut batch, params.now)?;
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
        // Self-heal: a pending note whose EVERY candidate is dead — no live
        // entry and no restorable archive block — describes an ask that was
        // already completed (e.g. a crash between the interim-copy scrub
        // and the note deletion). It can never be hash-verified or expired;
        // without this it would wedge forever.
        let cands = note.candidates.as_deref().unwrap_or_default();
        let mut all_dead = !cands.is_empty();
        for cand in cands {
            let live = entries.iter().any(|e| e.id == cand.entry_id);
            let archived = cand.interim_archived
                && find_archive_block_by_hash(memory_dir, &cand.content_hash)?.is_some();
            if live || archived {
                all_dead = false;
                break;
            }
        }
        if all_dead {
            apply::remove_file_if_exists(&note.path)?;
            outcome
                .pending_notes
                .push(pending_status(note, PendingState::Confirmed));
            tracing::info!(
                note = %note.path.display(),
                "pending forget completed by a prior run; note self-healed"
            );
            continue;
        }
        let expires_at = note.expires_at.expect("pending notes carry expires_at");
        // Day-granularity: the run's clock is params.today (deterministic),
        // so a note expires on the DAY its expiry falls — comparing full
        // instants against midnight kept notes alive a day too long.
        if now > expires_at || expires_at.date_naive() <= params.today {
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

    // --- 2.4 recovery re-hide (runs BEFORE any fallible/early exit) -------
    // A crash after a sensitive parking persisted its binding (apply step
    // 2.5) and archive copy (step 2) but before MEMORY.md was rewritten
    // (step 3) leaves interim-archived candidates live — and injectable.
    // Re-hide and PERSIST immediately: the parked invariant must not wait
    // for a successful merge (the pending note may be the only staging, or
    // the model may fail validation for the rest of the day).
    {
        let mut re_hidden = false;
        for note in &waiting {
            for cand in note.candidates.as_deref().unwrap_or_default() {
                if !cand.interim_archived {
                    continue;
                }
                if let Some(pos) = entries
                    .iter()
                    .position(|e| e.id == cand.entry_id && e.content_hash() == cand.content_hash)
                {
                    tracing::info!(
                        id = %cand.entry_id,
                        "re-hiding interim-archived candidate left live by a crashed run"
                    );
                    entries.remove(pos);
                    re_hidden = true;
                }
            }
        }
        if re_hidden {
            apply::write_memory_md(memory_dir, &entries, true)?;
        }
    }

    // --- 2.5 consume already-satisfied id-bound forgets -------------------
    // Crash recovery: the apply order deletes the authorizing note only
    // AFTER the new MEMORY.md is published. A crash in between leaves an
    // id-bound forget note whose target no longer exists anywhere — the
    // request was completed. Without this, the note would wedge every
    // future merge (the gates forbid dropping or pending it).
    let mut satisfied: Vec<usize> = Vec::new();
    let mut scrub_deleted_paths: Vec<PathBuf> = Vec::new();
    for (i, note) in batch.notes.iter().enumerate() {
        if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
            continue;
        }
        let named = note.named_entry_ids();
        if named.is_empty() {
            continue;
        }
        let mut any_alive = false;
        let mut archived_only: Vec<String> = Vec::new();
        for id in &named {
            // Candidate METADATA is not liveness: after a crash between
            // MEMORY.md publication and staging cleanup, a stale pending
            // note still lists the id. Only a live entry or a restorable
            // (hash-findable) interim archive copy keeps the ask alive.
            let mut restorable_interim = false;
            for w in &waiting {
                for c in w.candidates.as_deref().unwrap_or_default() {
                    if &c.entry_id == id
                        && c.interim_archived
                        && find_archive_block_by_hash(memory_dir, &c.content_hash)?.is_some()
                    {
                        restorable_interim = true;
                    }
                }
            }
            let live = entries.iter().any(|e| &e.id == id) || restorable_interim;
            if live {
                any_alive = true;
            } else if apply::archive_names_id(memory_dir, id)? {
                // The target survives ONLY as archived version(s): the user
                // asked to forget it, so it must be scrubbed there too —
                // treating it as satisfied would silently retain the data.
                archived_only.push(id.clone());
            }
        }
        // Archived-only ids are scrubbed immediately regardless of the
        // note's OTHER targets: id-bound host authority is complete
        // without a merge, and the model could never satisfy them (they
        // are in neither entries nor interim).
        for id in &archived_only {
            let (found, deleted_staging) = apply::scrub_archived_only_target(memory_dir, id)?;
            if found {
                outcome.hard_deleted.push(id.clone());
                tracing::info!(id, "archived-only forget target scrubbed");
            }
            // Collected here; pruned from the batch after the loop (we are
            // iterating batch.notes right now).
            scrub_deleted_paths.extend(deleted_staging);
        }
        // The note is consumed only when nothing it names is still alive
        // (live ids keep it in the batch for the merge's hard_delete).
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
    // Disk-scrubbed staging must leave the in-memory batch too, or the
    // merge prompt would still ship the deleted content. (After the
    // index-based satisfied removal — retain() shifts positions.)
    if !scrub_deleted_paths.is_empty() {
        let scrubbed: HashSet<&PathBuf> = scrub_deleted_paths.iter().collect();
        batch.notes.retain(|n| !scrubbed.contains(&n.path));
        batch.extractions.retain(|e| !scrubbed.contains(&e.path));
        batch
            .parse_failures
            .retain(|(p, _, _)| !scrubbed.contains(p));
    }

    // --- 2.7 sensitive-first isolation (before ANY provider call) ---------
    // Content the user marked sensitive must never ride a merge prompt:
    // free-text sensitive forgets park (their bodies leave the batch) and
    // id-bound sensitive forgets execute Rust-side — BEFORE section 5
    // builds the prompt from entries + notes. Under an exhausted budget
    // the same treatment extends to non-sensitive host forgets, since no
    // merge will run to honor them.
    for sensitive_only in [true, false] {
        if sensitive_only || !params.allow_merge {
            let mut parked: Vec<NoteFile> = Vec::new();
            entries = park_free_text_forgets(
                memory_dir,
                params,
                &batch,
                &entries,
                &mut outcome,
                sensitive_only,
                &mut parked,
            )?;
            if !parked.is_empty() {
                let parked_ids: HashSet<String> = parked.iter().map(|n| n.id.clone()).collect();
                batch.notes.retain(|n| !parked_ids.contains(&n.id));
                waiting.extend(parked);
            }
            entries = execute_id_bound_forgets(
                memory_dir,
                params,
                &mut batch,
                &entries,
                &waiting,
                &mut outcome,
                sensitive_only,
            )?;
            // Confirmations of parked pending forgets are deterministic
            // (hash-bound host authority): SENSITIVE ones always execute
            // Rust-side (their notes may never ride a merge prompt), and
            // under an exhausted budget the rest do too (no merge will
            // run to honor them).
            entries = confirm_pending_forgets_rust_side(
                memory_dir,
                params,
                &mut batch,
                &mut waiting,
                &entries,
                &mut outcome,
                sensitive_only,
            )?;
        }
    }
    // Isolation backstop: any sensitive id-bound forget still in the batch
    // was deferred (frozen candidate with a stale binding hash). It stays
    // on disk fail-closed for a later pass/expiry, but its body may not be
    // rendered into a merge prompt — drop it from THIS run's batch.
    batch.notes.retain(|n| {
        let deferred_sensitive =
            n.sensitive && n.origin == NoteOrigin::Host && n.kind == NoteKind::Forget;
        if deferred_sensitive {
            tracing::info!(
                note = %n.path.display(),
                "sensitive forget deferred (stale binding); excluded from this run's prompt"
            );
        }
        !deferred_sensitive
    });

    // A host correction cannot update an entry that has already been deleted.
    // Forget processing above deliberately protects host notes from broad
    // content scrubbing; retire only explicitly targeted, now-orphaned
    // corrections here so they cannot keep failing the merge indefinitely.
    // Archives and pending candidates may still restore an entry: preserve
    // their asks rather than treating absence from MEMORY.md alone as deletion.
    let mut obsolete_corrections = HashSet::new();
    for note in &batch.notes {
        if note.origin != NoteOrigin::Host || note.kind != NoteKind::Correction {
            continue;
        }
        let Some(target) = note.replaces_id.as_deref() else {
            continue;
        };
        if entries.iter().any(|e| e.id == target)
            || waiting.iter().any(|pending| {
                pending
                    .candidates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|candidate| candidate.entry_id == target)
            })
            || apply::archive_names_id(memory_dir, target)?
        {
            continue;
        }
        apply::remove_file_if_exists(&note.path)?;
        obsolete_corrections.insert(note.path.clone());
        outcome.dropped.push((
            note.id.clone(),
            "correction cancelled: target no longer exists".to_string(),
        ));
    }
    batch
        .notes
        .retain(|note| !obsolete_corrections.contains(&note.path));

    // --- 3. skip early when nothing is consumable ------------------------
    // (Also the stop line when the caller disallowed the merge: every
    // no-provider phase — INIT persist, expiry, satisfied consumption,
    // recovery re-hide — has already run by this point.)
    // Prune orphaned usage BEFORE any early return so it runs on every
    // invocation, including quiet ones (codex #1614 round-2 P2): an entry
    // archived by a PRIOR run is absent from the freshly-loaded `entries`
    // here and gets dropped now regardless of whether this run does work.
    // The live set is CONSERVATIVE — entries (post-INIT/expiry, so restored
    // candidates are already back in), bank slugs, and every pending-note
    // candidate id — so nothing that is or could be restored is dropped. A
    // bank-listing error skips pruning entirely rather than risk dropping
    // live bank usage.
    {
        let usage_store = octos_memory::MemoryStore::at_memory_dir(memory_dir);
        match usage_store.list_entities().await {
            Ok(bank) => {
                let mut live: std::collections::HashSet<String> =
                    entries.iter().map(|e| e.id.clone()).collect();
                live.extend(bank.into_iter().map(|(slug, _)| slug));
                for note in &waiting {
                    for cand in note.candidates.as_deref().unwrap_or_default() {
                        live.insert(cand.entry_id.clone());
                    }
                }
                usage_store.prune_usage(&live).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "skipping usage prune: bank listing failed");
            }
        }
    }

    let has_batch =
        params.allow_merge && (!batch.notes.is_empty() || !batch.extractions.is_empty());
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
            record_failures(memory_dir, &parse_failed, &[], &protected, params.now, "")?;
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

    outcome.provider_attempts += 1;
    let response = match provider.chat(&messages, &[], &config).await {
        Ok(response) => response,
        Err(e) => {
            // A failed call is a failed merge for the asks in the batch; a
            // sensitive ask still hides its data first (`finish_failed`).
            let failure = MergeFailure::CallFailed(format!("memory model call failed: {e}"));
            finish_failed(
                &mut outcome,
                memory_dir,
                params,
                &entries,
                &batch,
                &waiting,
                failure,
            )?;
            return Ok(outcome);
        }
    };
    accumulate_usage(&mut outcome.token_usage, &response.usage);
    let content = response.content.unwrap_or_default();

    let output = match ops::parse_model_output(&content) {
        Ok(output) => output,
        Err(first_err) => {
            // Exactly ONE corrective re-ask, then abort keeping staging.
            messages.push(Message::assistant(content));
            messages.push(Message::user(prompt::corrective_message(&first_err)));
            outcome.provider_attempts += 1;
            let retry = match provider.chat(&messages, &[], &config).await {
                Ok(retry) => retry,
                Err(e) => {
                    let failure =
                        MergeFailure::CallFailed(format!("memory model call failed: {e}"));
                    finish_failed(
                        &mut outcome,
                        memory_dir,
                        params,
                        &entries,
                        &batch,
                        &waiting,
                        failure,
                    )?;
                    return Ok(outcome);
                }
            };
            accumulate_usage(&mut outcome.token_usage, &retry.usage);
            let retry_content = retry.content.unwrap_or_default();
            match ops::parse_model_output(&retry_content) {
                Ok(output) => output,
                Err(second_err) => {
                    finish_failed(
                        &mut outcome,
                        memory_dir,
                        params,
                        &entries,
                        &batch,
                        &waiting,
                        MergeFailure::Rejected(format!(
                            "model output unparseable after one re-ask: {second_err}"
                        )),
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
    // id → archived block TEXT (resolved by hash). The text powers the
    // add-back guard for interim targets; an unresolvable block keeps the
    // key (the hard_delete existence gate) with empty text — its
    // confirmation will hash-mismatch before anything applies anyway.
    let mut interim: HashMap<String, String> = HashMap::new();
    for note in &waiting {
        for cand in note.candidates.as_deref().unwrap_or_default() {
            if !cand.interim_archived {
                continue;
            }
            let text = find_archive_block_by_hash(memory_dir, &cand.content_hash)?
                .map(|(_, block)| block)
                .unwrap_or_default();
            interim.insert(cand.entry_id.clone(), text);
        }
    }

    // Usage feedback (#1586): recently-used entries are kept alive against
    // age-based auto-archive. Advisory — a missing/corrupt sidecar reads as
    // empty and consolidation proceeds exactly as before. (Pruning runs
    // near the top, before the quiet-run early return, so it happens on
    // EVERY invocation.)
    let usage = octos_memory::MemoryStore::at_memory_dir(memory_dir)
        .load_usage()
        .await;

    let ctx = ValidationCtx {
        entries: &entries,
        interim: &interim,
        frozen: &frozen,
        notes: &notes_map,
        items: &items_map,
        usage: &usage.entries,
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
                params,
                &entries,
                &batch,
                &waiting,
                MergeFailure::Rejected(format!("merge rejected: {reason}")),
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
                params,
                &entries,
                &batch,
                &remaining,
                MergeFailure::Rejected(format!(
                    "confirmation hash mismatch on candidate {bad_id} of pending note {} — \
                     no destructive action taken, candidates recomputed",
                    note.id
                )),
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

    // Confirmed pending entries: restore every unconfirmed interim candidate
    // byte-identically and schedule the note files for deletion.
    for (entry_id, block) in &restores {
        if working.iter().any(|e| e.id == *entry_id) {
            // Already restored by a previous crashed run — but that run may
            // have died before step 4 removed the archive copy; schedule
            // the removal again (idempotent: absent blocks no-op).
            plan.archive_block_removals.push(block.clone());
            continue;
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
            params,
            &entries,
            &batch,
            &waiting,
            MergeFailure::Rejected(format!(
                "merge rejected: rendered MEMORY.md is {tokens} tokens (budget {})",
                params.max_memory_file_tokens
            )),
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
    outcome.quarantine_candidates = record_failures(
        memory_dir,
        &parse_failed,
        &succeeded,
        &protected,
        params.now,
        "",
    )?;

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
/// Rust-only parking of free-text host forget notes (no provider): bind
/// candidates, interim-archive sensitive ones, persist the pending note.
/// Used by the budget-exhausted path and by merge-FAILURE paths so a
/// sensitive ask hides its data even when the merge never succeeds.
/// Returns the entries list after any interim hiding.
/// Rust-side execution of id-bound host forgets on live, unfrozen entries
/// (full scrub pipeline, no provider). `sensitive_only` gates the
/// sensitive-first isolation pass; the budget-exhausted pass runs it for
/// every id-bound host forget. Fully honored notes are consumed; partially
/// honored ones survive as the confirmation for their frozen ids.
fn execute_id_bound_forgets(
    memory_dir: &Path,
    params: &ConsolidateParams,
    batch: &mut StagingBatch,
    entries: &[Entry],
    waiting: &[NoteFile],
    outcome: &mut ConsolidateOutcome,
    sensitive_only: bool,
) -> Result<Vec<Entry>> {
    let frozen_ids: HashSet<String> = waiting
        .iter()
        .flat_map(|w| w.candidates.as_deref().unwrap_or_default())
        .map(|c| c.entry_id.clone())
        .collect();
    let mut plan = ApplyPlan::default();
    let mut working = entries.to_vec();
    let mut executed_notes: Vec<usize> = Vec::new();
    let mut satisfied_no_op: Vec<usize> = Vec::new();
    for (i, note) in batch.notes.iter().enumerate() {
        if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
            continue;
        }
        let named = note.named_entry_ids();
        if named.is_empty() {
            continue;
        }
        // The sensitive filter gates EXECUTION, not satisfaction: a
        // non-sensitive sibling whose ids a sensitive note just deleted
        // must still be consumed here (nothing is left for a merge to
        // hard_delete — the validator would wedge on it).
        if sensitive_only && !note.sensitive {
            let mut all_gone = true;
            for id in &named {
                if frozen_ids.contains(id)
                    || working.iter().any(|e| &e.id == id)
                    || apply::archive_names_id(memory_dir, id)?
                {
                    all_gone = false;
                    break;
                }
            }
            if all_gone {
                satisfied_no_op.push(i);
            }
            continue;
        }
        let mut targets: Vec<usize> = named
            .iter()
            .filter_map(|id| {
                if frozen_ids.contains(id) {
                    return None;
                }
                working.iter().position(|e| &e.id == id)
            })
            .collect();
        // Descending + dedup: note order is arbitrary; removing a lower
        // index first would shift later positions and panic.
        targets.sort_unstable_by(|a, b| b.cmp(a));
        targets.dedup();
        if targets.is_empty() {
            // Nothing left to execute — but if every named id is already
            // gone everywhere (a sibling note deleted it earlier in THIS
            // loop), the ask is honored: consume the note or its body
            // would still ride the merge prompt.
            let mut all_gone = !named.is_empty();
            for id in &named {
                if frozen_ids.contains(id)
                    || working.iter().any(|e| &e.id == id)
                    || apply::archive_names_id(memory_dir, id)?
                {
                    all_gone = false;
                    break;
                }
            }
            if all_gone {
                satisfied_no_op.push(i);
            }
            continue;
        }
        // Covered = frozen nowhere AND either being executed now (live) or
        // already gone everywhere (e.g. an archived-only id scrubbed by the
        // satisfied pre-pass) — such a note is fully honored.
        let mut all_named_covered = true;
        for id in &named {
            if frozen_ids.contains(id) {
                all_named_covered = false;
                break;
            }
            let live = working.iter().any(|e| &e.id == id);
            if !live && apply::archive_names_id(memory_dir, id)? {
                all_named_covered = false;
                break;
            }
        }
        // A partially honored note (some ids frozen/deferred) must SURVIVE
        // as the confirmation for the remaining ids — only a fully honored
        // note is consumed by the apply.
        let authorizing = if all_named_covered {
            note.path.clone()
        } else {
            PathBuf::new()
        };
        for pos in targets {
            let entry = working.remove(pos);
            plan.hard_deletes.push(ScrubTarget {
                entry_id: entry.id.clone(),
                folded_lines: entry.folded_lines(),
                authorizing_note: authorizing.clone(),
                originating_pending: Vec::new(),
            });
            outcome.hard_deleted.push(entry.id.clone());
        }
        if all_named_covered {
            executed_notes.push(i);
        }
    }
    if !plan.hard_deletes.is_empty() || !satisfied_no_op.is_empty() {
        // Resolve consumed notes to IDENTITY (path) BEFORE any batch
        // mutation — retain() below shifts positions and stored numeric
        // indices would remove/delete the wrong note.
        let mut consumed: Vec<(PathBuf, String, bool)> = executed_notes
            .iter()
            .map(|&i| {
                (
                    batch.notes[i].path.clone(),
                    batch.notes[i].id.clone(),
                    false,
                )
            })
            .chain(
                satisfied_no_op
                    .iter()
                    .map(|&i| (batch.notes[i].path.clone(), batch.notes[i].id.clone(), true)),
            )
            .collect();
        consumed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        consumed.dedup_by(|a, b| a.0 == b.0);

        if !plan.hard_deletes.is_empty() {
            plan.final_entries = working.clone();
            let report = apply::apply_plan(memory_dir, params.today, &plan)?;
            // The scrub may have disk-deleted OTHER staging files quoting
            // the removed entry — prune them from the in-memory batch too,
            // or the later merge prompt would still ship the scrubbed
            // content (and validators would demand consumption of
            // vanished files).
            if !report.scrub_deleted_staging.is_empty() {
                let scrubbed: HashSet<&PathBuf> = report.scrub_deleted_staging.iter().collect();
                batch.notes.retain(|n| !scrubbed.contains(&n.path));
                batch.extractions.retain(|e| !scrubbed.contains(&e.path));
                batch
                    .parse_failures
                    .retain(|(p, _, _)| !scrubbed.contains(p));
            }
            // apply_plan may itself have scrubbed exact deleted lines out
            // of SURVIVING entries before publishing MEMORY.md — returning
            // the pre-scrub `working` would leak them into the next merge
            // prompt and write them back on the final apply. Re-read the
            // published state; it is the single source of truth.
            let published = std::fs::read_to_string(memory_dir.join("MEMORY.md"))
                .wrap_err("failed to re-read MEMORY.md after Rust-side apply")?;
            match entry::parse_memory_md(&published).map_err(|e| eyre::eyre!(e))? {
                entry::ParsedMemory::Entries(parsed) => working = parsed,
                // Unreachable in practice: this run just published id'd
                // entries. Keep the pre-scrub set rather than guessing.
                entry::ParsedMemory::Legacy(_) => {}
            }
        }
        // Fully-honored notes were consumed by the apply (authorizing note
        // deletion); drop them from the batch — together with siblings
        // whose named ids were already deleted this run (their files must
        // go too, or the ask would reappear next run).
        for (path, note_id, delete_file) in consumed {
            batch.notes.retain(|n| n.path != path);
            if delete_file {
                apply::remove_file_if_exists(&path)?;
            }
            outcome.dropped.push((
                note_id,
                "executed Rust-side (id-bound host authority)".to_string(),
            ));
        }
    }
    Ok(working)
}

/// Rust-side confirmation of pending forgets (no provider): an id-bound
/// host forget naming a FROZEN id is the user's hash-bound confirmation of
/// a parked pending note. Deterministic, so an exhausted merge budget must
/// not defer it. Mirrors section 7's semantics: verify EVERY candidate of
/// an affected pending note, hard-delete the confirmed ids, restore the
/// unconfirmed interim ones byte-identically, delete the pending + confirm
/// notes. Any hash mismatch → no destructive action (left for a merged
/// run, which also recomputes bindings).
#[allow(clippy::too_many_lines)]
fn confirm_pending_forgets_rust_side(
    memory_dir: &Path,
    params: &ConsolidateParams,
    batch: &mut StagingBatch,
    waiting: &mut Vec<NoteFile>,
    entries: &[Entry],
    outcome: &mut ConsolidateOutcome,
    sensitive_only: bool,
) -> Result<Vec<Entry>> {
    let mut working = entries.to_vec();
    let mut plan = ApplyPlan::default();
    let mut consumed_confirms: Vec<(PathBuf, String)> = Vec::new();
    let mut confirmed_pending_ids: HashSet<String> = HashSet::new();

    for note in &batch.notes {
        if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
            continue;
        }
        if sensitive_only && !note.sensitive {
            continue;
        }
        let named = note.named_entry_ids();
        if named.is_empty() {
            continue;
        }
        // Affected pending notes = waiting notes holding any named id.
        let affected: Vec<&NoteFile> = waiting
            .iter()
            .filter(|w| {
                w.candidates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|c| named.iter().any(|n| n == &c.entry_id))
            })
            .collect();
        if affected.is_empty() {
            continue;
        }
        // Verify every candidate of every affected pending note.
        let mut interim_texts: HashMap<String, String> = HashMap::new();
        let mut restores: HashMap<String, String> = HashMap::new();
        let mut verified = true;
        for w in &affected {
            for cand in w.candidates.as_deref().unwrap_or_default() {
                let confirmed = named.iter().any(|n| n == &cand.entry_id);
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
                            verified = false;
                        }
                    }
                } else if confirmed {
                    let live = working.iter().find(|e| e.id == cand.entry_id);
                    if live.is_none_or(|e| e.content_hash() != cand.content_hash) {
                        verified = false;
                    }
                }
                if !verified {
                    break;
                }
            }
            if !verified {
                break;
            }
        }
        if !verified {
            // Left for a merged run (which also recomputes bindings).
            continue;
        }
        // A restore target that is itself confirmed stays deleted.
        restores.retain(|id, _| !named.iter().any(|n| n == id));

        let affected_paths: Vec<PathBuf> = affected.iter().map(|w| w.path.clone()).collect();
        let affected_ids: Vec<String> = affected.iter().map(|w| w.id.clone()).collect();
        for id in &named {
            let text = match working.iter().position(|e| &e.id == id) {
                Some(pos) => working.remove(pos).text,
                None => match interim_texts.get(id) {
                    Some(text) => text.clone(),
                    // Named id unknown here (e.g. already gone) — the
                    // executor/satisfied passes own that case.
                    None => continue,
                },
            };
            let scrub_entry = Entry {
                id: id.clone(),
                text,
            };
            plan.hard_deletes.push(ScrubTarget {
                entry_id: id.clone(),
                folded_lines: scrub_entry.folded_lines(),
                authorizing_note: note.path.clone(),
                originating_pending: affected_paths.clone(),
            });
            outcome.hard_deleted.push(id.clone());
        }
        for (entry_id, block) in &restores {
            if working.iter().any(|e| e.id == *entry_id) {
                plan.archive_block_removals.push(block.clone());
                continue;
            }
            working.push(Entry {
                id: entry_id.clone(),
                text: block.clone(),
            });
            plan.archive_block_removals.push(block.clone());
        }
        for w in &affected {
            plan.pending_deletes.push(w.path.clone());
            outcome
                .pending_notes
                .push(pending_status(w, PendingState::Confirmed));
        }
        confirmed_pending_ids.extend(affected_ids);
        consumed_confirms.push((note.path.clone(), note.id.clone()));
    }

    if plan.hard_deletes.is_empty() {
        return Ok(working);
    }
    plan.final_entries = working.clone();
    let report = apply::apply_plan(memory_dir, params.today, &plan)?;

    // Post-apply bookkeeping mirrors execute_id_bound_forgets: prune
    // scrub-deleted staging, consume the confirm notes by identity, drop
    // the confirmed pending notes from `waiting`, and re-read the
    // published entries (the apply scrubs shared lines out of survivors).
    if !report.scrub_deleted_staging.is_empty() {
        let scrubbed: HashSet<&PathBuf> = report.scrub_deleted_staging.iter().collect();
        batch.notes.retain(|n| !scrubbed.contains(&n.path));
        batch.extractions.retain(|e| !scrubbed.contains(&e.path));
        batch
            .parse_failures
            .retain(|(p, _, _)| !scrubbed.contains(p));
    }
    for (path, note_id) in consumed_confirms {
        batch.notes.retain(|n| n.path != path);
        outcome.dropped.push((
            note_id,
            "confirmed pending forget Rust-side (id-bound host authority)".to_string(),
        ));
    }
    waiting.retain(|w| !confirmed_pending_ids.contains(&w.id));
    let published = std::fs::read_to_string(memory_dir.join("MEMORY.md"))
        .wrap_err("failed to re-read MEMORY.md after Rust-side confirmation")?;
    if let entry::ParsedMemory::Entries(parsed) =
        entry::parse_memory_md(&published).map_err(|e| eyre::eyre!(e))?
    {
        working = parsed;
    }
    Ok(working)
}

fn park_free_text_forgets(
    memory_dir: &Path,
    params: &ConsolidateParams,
    batch: &StagingBatch,
    entries: &[Entry],
    outcome: &mut ConsolidateOutcome,
    sensitive_only: bool,
    parked_out: &mut Vec<NoteFile>,
) -> Result<Vec<Entry>> {
    let mut plan = ApplyPlan::default();
    let mut working = entries.to_vec();
    let mut parked_any = false;
    for note in &batch.notes {
        if !note.is_free_text_forget() {
            continue;
        }
        if sensitive_only && !note.sensitive {
            continue;
        }
        // Already surfaced as Created this run? Skip double-parking.
        if outcome
            .pending_notes
            .iter()
            .any(|p| p.note_id == note.id && p.state == PendingState::Created)
        {
            continue;
        }
        let mut candidates: Vec<PendingCandidate> =
            pending::compute_candidates(&note.content, &working)
                .into_iter()
                .filter_map(|(entry_id, _)| {
                    working
                        .iter()
                        .find(|e| e.id == entry_id)
                        .map(|e| PendingCandidate {
                            entry_id,
                            content_hash: e.content_hash(),
                            interim_archived: false,
                        })
                })
                .collect();
        if note.sensitive {
            for cand in &mut candidates {
                if let Some(pos) = working.iter().position(|e| e.id == cand.entry_id) {
                    plan.archive_appends.push(working[pos].text.clone());
                    working.remove(pos);
                    cand.interim_archived = true;
                }
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
        let mut parked = note.clone();
        parked.candidates = Some(candidates);
        parked.expires_at = Some(expires_at);
        parked_out.push(parked);
        parked_any = true;
    }
    if parked_any {
        plan.final_entries = working.clone();
        apply::apply_plan(memory_dir, params.today, &plan)?;
    }
    Ok(working)
}

/// A merge that did not happen, and what it is held against.
enum MergeFailure {
    /// The model answered, unusably: every staged file of the batch.
    Rejected(String),
    /// The model call itself failed (#3257 review): only the asks, which back
    /// off and are parked like any failing ask. The rest of staging is not at
    /// fault, so an outage never quarantines it; the refresh budget, charged
    /// for the call, bounds its retries.
    CallFailed(String),
}

fn finish_failed(
    outcome: &mut ConsolidateOutcome,
    memory_dir: &Path,
    params: &ConsolidateParams,
    entries: &[Entry],
    batch: &StagingBatch,
    waiting: &[NoteFile],
    failure: MergeFailure,
) -> Result<()> {
    let (reason, asks_only) = match failure {
        MergeFailure::Rejected(reason) => (reason, false),
        MergeFailure::CallFailed(reason) => (reason, true),
    };
    outcome.errors.push(reason);
    outcome.merge_applied = false;
    // A sensitive ask must hide its data even when the merge failed — park
    // it now (Rust-only) instead of leaving the candidates live until some
    // later successful run.
    park_free_text_forgets(
        memory_dir,
        params,
        batch,
        entries,
        outcome,
        true,
        &mut Vec::new(),
    )?;
    for note in waiting {
        outcome
            .pending_notes
            .push(pending_status(note, PendingState::Waiting));
    }
    let failed: Vec<PathBuf> = if asks_only {
        let asks = batch.notes.iter().filter(|n| is_must_honor(n));
        asks.map(|n| n.path.clone()).collect()
    } else {
        batch
            .notes
            .iter()
            .map(|n| n.path.clone())
            .chain(batch.extractions.iter().map(|e| e.path.clone()))
            .chain(batch.parse_failures.iter().map(|(p, _, _)| p.clone()))
            .collect()
    };
    let protected = protected_paths(batch);
    let error = outcome.errors.last().cloned().unwrap_or_default();
    outcome.quarantine_candidates =
        record_failures(memory_dir, &failed, &[], &protected, params.now, &error)?;
    outcome.failed_asks = park_stuck_asks(memory_dir, batch, params.now)?;
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

/// One staging file's consecutive failed batches. Trackers written before
/// #3253 hold a bare count.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(from = "StoredFailure")]
struct Failure {
    count: u32,
    last_failed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StoredFailure {
    Count(u32),
    Full {
        count: u32,
        #[serde(default)]
        last_failed_at: Option<DateTime<Utc>>,
        #[serde(default)]
        last_error: Option<String>,
    },
}

impl From<StoredFailure> for Failure {
    fn from(stored: StoredFailure) -> Self {
        match stored {
            StoredFailure::Count(count) => Failure {
                count,
                last_failed_at: None,
                last_error: None,
            },
            StoredFailure::Full {
                count,
                last_failed_at,
                last_error,
            } => Failure {
                count,
                last_failed_at,
                last_error,
            },
        }
    }
}

fn read_tracker(memory_dir: &Path) -> Result<HashMap<String, Failure>> {
    match std::fs::read_to_string(tracker_path(memory_dir)) {
        Ok(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).wrap_err("failed to read failure tracker"),
    }
}

/// Update the consecutive-failure counts: increment `failed` (stamping when
/// and why), clear `succeeded`, prune entries whose files are gone. Returns
/// the quarantine candidates among `failed` (threshold reached, not
/// protected).
fn record_failures(
    memory_dir: &Path,
    failed: &[PathBuf],
    succeeded: &[PathBuf],
    protected: &HashSet<PathBuf>,
    now: DateTime<Utc>,
    error: &str,
) -> Result<Vec<PathBuf>> {
    if failed.is_empty() && succeeded.is_empty() {
        return Ok(Vec::new());
    }
    let mut counts = read_tracker(memory_dir)?;
    for p in succeeded {
        counts.remove(&tracker_key(memory_dir, p));
    }
    let mut quarantine = Vec::new();
    for p in failed {
        let failure = counts.entry(tracker_key(memory_dir, p)).or_insert(Failure {
            count: 0,
            last_failed_at: None,
            last_error: None,
        });
        failure.count += 1;
        failure.last_failed_at = Some(now);
        failure.last_error = (!error.is_empty()).then(|| error.to_owned());
        if failure.count >= QUARANTINE_THRESHOLD && !protected.contains(p) {
            quarantine.push(p.clone());
        }
    }
    counts.retain(|key, _| memory_dir.join("staging").join(key).exists());

    let path = tracker_path(memory_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    apply::atomic_write(&path, &serde_json::to_string(&counts)?)?;
    Ok(quarantine)
}

/// An ask the model must honor: a host note other than a forget (forgets
/// are executed or parked Rust-side), or any user_request.
fn is_must_honor(note: &NoteFile) -> bool {
    (note.origin == NoteOrigin::Host && note.kind != NoteKind::Forget)
        || note.kind == NoteKind::UserRequest
}

/// Wait before retrying an ask that failed `failures` merges in a row:
/// 5 minutes, doubling.
fn ask_backoff(failures: u32) -> Duration {
    Duration::minutes(5 << failures.saturating_sub(1).min(8))
}

/// Leave out of this run's batch every ask still waiting out its backoff, so
/// a failing ask costs at most [`STUCK_ASK_THRESHOLD`] merges and the other
/// staging can merge meanwhile. The backoff is read from the on-disk
/// tracker, so it holds across restarts.
fn hold_back_failing_asks(
    memory_dir: &Path,
    batch: &mut StagingBatch,
    now: DateTime<Utc>,
) -> Result<usize> {
    let counts = read_tracker(memory_dir)?;
    let before = batch.notes.len();
    batch.notes.retain(|note| {
        !is_must_honor(note)
            || !counts
                .get(&tracker_key(memory_dir, &note.path))
                .and_then(|f| Some(f.last_failed_at? + ask_backoff(f.count)))
                .is_some_and(|retry_at| now < retry_at)
    });
    Ok(before - batch.notes.len())
}

/// Park every ask of this failed batch that has now failed
/// [`STUCK_ASK_THRESHOLD`] merges: move it to `staging/failed/` with a
/// client-safe reason beside it. The ask is kept, never silently dropped;
/// it is only no longer retried.
fn park_stuck_asks(
    memory_dir: &Path,
    batch: &StagingBatch,
    now: DateTime<Utc>,
) -> Result<Vec<PathBuf>> {
    let counts = read_tracker(memory_dir)?;
    let failed_dir = memory_dir.join("staging").join(FAILED_ASKS_DIR);
    let mut parked = Vec::new();
    for note in batch.notes.iter().filter(|note| is_must_honor(note)) {
        let Some(failure) = counts.get(&tracker_key(memory_dir, &note.path)) else {
            continue;
        };
        if failure.count < STUCK_ASK_THRESHOLD {
            continue;
        }
        let (Some(name), Some(stem)) = (note.path.file_name(), note.path.file_stem()) else {
            continue;
        };
        std::fs::create_dir_all(&failed_dir)
            .wrap_err_with(|| format!("failed to create {}", failed_dir.display()))?;
        let record = FailedAsk {
            reason: failed_ask_reason(failure.last_error.as_deref().unwrap_or_default()),
            attempts: failure.count,
            failed_at: now,
        };
        apply::atomic_write(
            &failed_dir.join(stem).with_extension("json"),
            &serde_json::to_string(&record)?,
        )?;
        std::fs::rename(&note.path, failed_dir.join(name))
            .wrap_err_with(|| format!("failed to park {}", note.path.display()))?;
        tracing::warn!(
            note = %note.path.display(),
            attempts = failure.count,
            error = failure.last_error.as_deref().unwrap_or_default(),
            "memory ask parked as failed; it is no longer retried"
        );
        parked.push(note.path.clone());
    }
    Ok(parked)
}

/// Why an ask was parked, safe to show its requester: no paths, no note
/// content, no raw model output.
fn failed_ask_reason(error: &str) -> String {
    let cause = if error.contains("was not consumed") || error.contains("no op cites it") {
        "the memory model did not save it"
    } else if error.contains("unparseable") {
        "the memory model did not return a usable answer"
    } else if error.contains("model call failed") {
        "the memory model returned an error"
    } else {
        "the memory update was rejected"
    };
    format!("{cause} in {STUCK_ASK_THRESHOLD} attempts; the request is kept but no longer retried")
}

/// A parked ask's state as its requester sees it (#3253).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailedAsk {
    pub reason: String,
    pub attempts: u32,
    pub failed_at: DateTime<Utc>,
}

/// The parked state of the explicit memory request `request_id`
/// (`memory/request`), if consolidation parked it as a failed ask.
pub async fn failed_request(
    store: &octos_memory::MemoryStore,
    request_id: &str,
) -> Option<FailedAsk> {
    let stem = store.staging_request_note(request_id).await.ok()??;
    failed_ask(store.memory_md_path().parent()?, &stem)
}

/// The parked state of the staging note `note_stem`, if it was parked as a
/// failed ask.
pub fn failed_ask(memory_dir: &Path, note_stem: &str) -> Option<FailedAsk> {
    if note_stem.is_empty() || note_stem.contains(['/', '\\', '.']) {
        return None;
    }
    let path = memory_dir
        .join("staging")
        .join(FAILED_ASKS_DIR)
        .join(format!("{note_stem}.json"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod stuck_ask_tests {
    use super::consolidate_test_support::ScriptedMerge;
    use super::*;

    /// glm-5.3 in #3253 kept declining one ask; this model never honors it,
    /// and this one always fails the call (#3257 review). Each run reads the
    /// failure tracker from disk, exactly as after a restart: the cap and the
    /// backoff hold across both.
    #[tokio::test]
    async fn should_park_an_ask_the_model_never_honors_after_three_backed_off_merges() {
        let declines = ScriptedMerge::new(r#"{"ops":[],"consumed_ids":[],"dropped":[]}"#);
        for (provider, reason) in [
            (declines, "did not save it"),
            (ScriptedMerge::failing(), "returned an error"),
        ] {
            let (provider, fails) = (Arc::new(provider), reason.contains("error"));
            let dir = tempfile::tempdir().unwrap();
            let memory_dir = dir.path().join("memory");
            let store = octos_memory::MemoryStore::at_memory_dir(&memory_dir);
            let ask = octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Host,
                kind: octos_memory::NoteKind::UserRequest,
                content: "Remember the build box is called kestrel.".into(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            };
            store.write_staging_request("ask-1", &ask).await.unwrap();
            let start = Utc::now();
            let run_at = async |minute| {
                let mut params = ConsolidateParams::new(memory_dir.clone());
                params.now = start + Duration::minutes(minute);
                run_consolidation(provider.clone(), &params).await.unwrap()
            };
            // (minutes after the first run, provider calls by then, call made this run)
            for (minute, calls, called) in [
                (0, 1, true),
                (4, 1, false),
                (5, 2, true),
                (14, 2, false),
                (15, 3, true),
                (180, 3, false),
            ] {
                let outcome = run_at(minute).await;
                assert_eq!(provider.calls(), calls, "{reason} minute {minute}");
                let spent = outcome.token_usage.input_tokens > 0;
                assert_eq!(spent, called && !fails, "{reason} minute {minute}");
                assert_eq!(outcome.provider_attempts > 0, called, "budget is charged");
                assert!(outcome.quarantine_candidates.is_empty());
            }
            let stem = store.staging_request_note("ask-1").await.unwrap().unwrap();
            assert!(!memory_dir.join(format!("staging/notes/{stem}.md")).exists());
            let parked =
                std::fs::read_to_string(memory_dir.join(format!("staging/failed/{stem}.md")));
            assert!(parked.unwrap().contains("kestrel"), "kept, not dropped");
            let failed = failed_request(&store, "ask-1").await.expect("visible");
            assert_eq!(failed.attempts, STUCK_ASK_THRESHOLD);
            assert!(failed.reason.contains(reason), "{}", failed.reason);
            assert!(!failed.reason.contains("kestrel") && !failed.reason.contains(&stem));
            assert!(!failed.reason.contains("HTTP 400"), "{}", failed.reason);
            if fails {
                // An outage is held against the asks only: a staged fact
                // retries and is never quarantined for it.
                let kind = octos_memory::NoteKind::Fact;
                let origin = octos_memory::NoteOrigin::Model;
                let fact = octos_memory::StagingNote {
                    kind,
                    origin,
                    ..ask
                };
                store.write_staging_note(&fact).await.unwrap();
                for minute in [181, 182] {
                    assert!(run_at(minute).await.quarantine_candidates.is_empty());
                }
                assert_eq!(provider.calls(), STUCK_ASK_THRESHOLD + 2);
            }
        }
    }
}
