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

use chrono::{DateTime, Duration, FixedOffset, NaiveDate};
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
    /// When false, run only the no-provider phases (INIT persist, expiry,
    /// satisfied-forget consumption, crash-recovery re-hide) and stop
    /// before the merge call — the service sets this when the daily budget
    /// is exhausted so `pending_confirm_days` stays honored on busy
    /// profiles.
    pub allow_merge: bool,
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
            allow_merge: true,
            // Local date — budgets, daily notes, and extraction dates all
            // use the profile machine's local calendar; UTC would stamp
            // evening runs with tomorrow's date on western timezones.
            today: chrono::Local::now().date_naive(),
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
    let mut batch = batch;
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

    // --- 3. skip early when nothing is consumable ------------------------
    // (Also the stop line when the caller disallowed the merge: every
    // no-provider phase — INIT persist, expiry, satisfied consumption,
    // recovery re-hide — has already run by this point.)
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

    outcome.provider_attempts += 1;
    let response = match provider.chat(&messages, &[], &config).await {
        Ok(response) => response,
        Err(e) => {
            // A sensitive ask must hide its data even when the merge never
            // happens — park before surfacing the transport error.
            park_free_text_forgets(
                memory_dir,
                params,
                &batch,
                &entries,
                &mut outcome,
                true,
                &mut Vec::new(),
            )?;
            return Err(e);
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
                    park_free_text_forgets(
                        memory_dir,
                        params,
                        &batch,
                        &entries,
                        &mut outcome,
                        true,
                        &mut Vec::new(),
                    )?;
                    return Err(e);
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
    // empty and consolidation proceeds exactly as before. First PRUNE the
    // sidecar to live ids (current entries + bank slugs) so it can't grow
    // unbounded as entries are archived/deleted (codex #1614 P2); entries
    // archived in THIS run are pruned next cycle (bounded steady state).
    let usage_store = octos_memory::MemoryStore::at_memory_dir(memory_dir);
    let mut live: std::collections::HashSet<String> =
        entries.iter().map(|e| e.id.clone()).collect();
    if let Ok(bank) = usage_store.list_entities().await {
        live.extend(bank.into_iter().map(|(slug, _)| slug));
    }
    usage_store.prune_usage(&live).await;
    let usage = usage_store.load_usage().await;

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
                params,
                &entries,
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

fn finish_failed(
    outcome: &mut ConsolidateOutcome,
    memory_dir: &Path,
    params: &ConsolidateParams,
    entries: &[Entry],
    batch: &StagingBatch,
    waiting: &[NoteFile],
    reason: String,
) -> Result<()> {
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
            allow_merge: true,
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
            r#"{"ops":[{"op":"update","id":"^maaaaaa","new_text":"Lives in Seattle. (updated: 2026-07-06)","sources":["01ex#0"]}],"consumed_ids":["01ex#0"],"dropped":[]}"#,
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

        // Sensitive-first isolation: parking happens Rust-side BEFORE any
        // provider call — the sensitive body and entry never ride a merge
        // prompt, and with nothing else staged no merge runs at all.
        let provider = ScriptedProvider::new(&[]);
        let outcome = run(&provider, &params(dir.path())).await;

        assert_eq!(
            provider.call_count(),
            0,
            "sensitive content must not reach the provider"
        );
        assert!(!outcome.merge_applied);
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
            false,
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

    #[tokio::test]
    async fn should_scrub_archived_only_target_when_forget_names_it() {
        // The entry was archived/superseded earlier; only archive blocks
        // carry its id. A forget naming it must scrub the archive — not be
        // treated as satisfied — and needs no merge call when it is the
        // only staging item.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-05.md"),
            "Old wifi password was letmein99. (updated: 2026-05-01) ^msecret\n\nUnrelated fact. (updated: 2026-04-01) ^mother1\n",
        )
        .unwrap();
        let note_path = write_note(
            memory_dir,
            "01fg-archived",
            "host",
            "forget",
            "please forget id:^msecret",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0);
        assert!(!note_path.exists(), "note consumed after the scrub");
        assert_eq!(outcome.hard_deleted, vec!["^msecret"]);
        let archived = std::fs::read_to_string(memory_dir.join("archive/2026-05.md")).unwrap();
        assert!(
            !archived.contains("letmein99"),
            "archived secret must be gone"
        );
        assert!(archived.contains("^mother1"), "unrelated block survives");
    }

    #[tokio::test]
    async fn should_re_hide_live_interim_candidates_when_recovering_from_crash() {
        // Crash window: binding persisted (interim_archived=true) and the
        // archive copy exists, but MEMORY.md was never rewritten — the
        // sensitive entry is still live. Recovery must hide it again and a
        // later confirmation must still resolve by archive hash.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive location detail. (updated: 2026-06-01) ^msenstv";
        let keep = "Keeps bonsai. (updated: 2026-06-01) ^mcccccc";
        write_memory(memory_dir, &[secret, keep]);
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(memory_dir.join("archive/2026-07.md"), format!("{secret}\n")).unwrap();
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the location",
            true,
            &format!(
                r#"[{{"entry_id":"^msenstv","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(secret)
            ),
            "2026-07-20T10:00:00+00:00",
        );
        // A model fact note so the batch is non-empty and the merge runs.
        write_note(memory_dir, "02fact", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["02fact"]}],"consumed_ids":["02fact"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied);

        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^msenstv"),
            "recovery must re-hide the still-live interim candidate: {memory}"
        );
        assert!(memory.contains("^mcccccc"), "unrelated entry survives");
        let archived = std::fs::read_to_string(memory_dir.join("archive/2026-07.md")).unwrap();
        assert_eq!(
            archived.matches("^msenstv").count(),
            1,
            "no duplicate archive copies"
        );
    }

    #[tokio::test]
    async fn should_re_hide_before_early_exit_when_parked_note_is_only_staging() {
        // The crash-recovery privacy hole: the parked note is the ONLY
        // staging state, so the run exits early with no merge — the
        // sensitive entry must be re-hidden and PERSISTED anyway.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive location detail. (updated: 2026-06-01) ^msenstv";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(memory_dir.join("archive/2026-07.md"), format!("{secret}\n")).unwrap();
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the location",
            true,
            &format!(
                r#"[{{"entry_id":"^msenstv","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(secret)
            ),
            "2026-07-20T10:00:00+00:00",
        );

        let provider = ScriptedProvider::new(&[]);
        run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0, "no merge needed");
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^msenstv"),
            "re-hide must persist even on the early-exit path: {memory}"
        );
        assert!(memory.contains("^mcccccc"));
    }

    #[tokio::test]
    async fn should_reject_update_when_only_source_is_id_bound_forget_note() {
        // A delete confirmation must not double as edit authority.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "Target entry text. (updated: 2026-06-01) ^maaaaaa",
                "Unrelated entry. (updated: 2026-06-02) ^mbbbbbb",
            ],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^maaaaaa",
            false,
        );

        // The reply uses the forget note as the sole source for editing the
        // UNRELATED entry (and never performs the authorized delete).
        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"update","id":"^mbbbbbb","new_text":"Rewritten by a delete confirmation.","sources":["01fg-confirm"]}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
            r#"{"ops":[{"op":"update","id":"^mbbbbbb","new_text":"Rewritten by a delete confirmation.","sources":["01fg-confirm"]}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();

        assert!(
            !outcome.merge_applied,
            "forget-sourced edit must be rejected"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("Unrelated entry."),
            "entry must be untouched"
        );
    }

    #[tokio::test]
    async fn should_not_rearchive_scrubbed_content_when_merge_mixes_delete_and_supersede() {
        // One merge hard-deletes ^msecret and supersedes ^mquoter, whose OLD
        // text quotes the secret line. The superseded text is archived —
        // but the quoted secret line must not ride back into the archive.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        // The secret entry is multi-line; the quoter's OLD text repeats one
        // of its EXACT lines (the line-exact scrub contract — embedded
        // substrings are the documented paraphrase residual).
        let secret =
            "The vault code is 9137.\nKept in the red notebook. (updated: 2026-06-01) ^msecret";
        let quoter = "The vault code is 9137.\nAlso likes tea. (updated: 2026-06-02) ^mquoter";
        write_memory(memory_dir, &[secret, quoter]);
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );
        write_note(
            memory_dir,
            "02req",
            "host",
            "user_request",
            "tea note is outdated",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"},{"op":"supersede","id":"^mquoter","replacement":"Likes tea.","reason":"cleanup","sources":["02req"]}],"consumed_ids":["01fg-confirm","02req"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied);
        assert_eq!(outcome.hard_deleted, vec!["^msecret"]);

        let mut archived = String::new();
        for entry in std::fs::read_dir(memory_dir.join("archive")).unwrap() {
            archived.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
        }
        assert!(
            !archived.contains("9137"),
            "hard-deleted content must not be re-archived by the supersede: {archived}"
        );
        assert!(
            archived.contains("Also likes tea."),
            "the non-secret remainder of the superseded text is archived"
        );
    }

    #[tokio::test]
    async fn should_reject_add_when_sourced_from_forget_note() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-freetext",
            "host",
            "forget",
            "forget my embarrassing hobby",
            false,
        );

        // The reply re-adds the forgotten content citing the forget note.
        let reply = r#"{"ops":[{"op":"add","section":null,"text":"Has an embarrassing hobby.","sources":["01fg-freetext"]}],"consumed_ids":[],"dropped":[]}"#;
        let provider = ScriptedProvider::new(&[reply, reply]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();

        assert!(
            !outcome.merge_applied,
            "forget-sourced add must be rejected"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("embarrassing"),
            "forgotten content must not return"
        );
    }

    #[tokio::test]
    async fn should_honor_both_notes_when_two_forgets_name_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "Secret thing. (updated: 2026-06-01) ^msecret",
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        write_note(
            memory_dir,
            "01fg-a",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );
        write_note(
            memory_dir,
            "02fg-b",
            "host",
            "forget",
            "please delete id:^msecret",
            false,
        );

        // ONE hard_delete satisfies both notes; both are consumed.
        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-a"}],"consumed_ids":["01fg-a","02fg-b"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(
            outcome.merge_applied,
            "double forget must be satisfiable: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.hard_deleted, vec!["^msecret"]);
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("^msecret"));
    }

    #[tokio::test]
    async fn should_consume_confirm_when_only_stale_pending_metadata_remains() {
        // Crash between MEMORY.md publication and staging cleanup: the entry
        // is gone, but the stale pending note (non-interim candidates) and
        // the id-bound confirm note both survive. The confirm must be
        // consumed as satisfied — candidate metadata is not liveness.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_pending_note(
            memory_dir,
            "01fg-stale",
            "forget the secret",
            false,
            &format!(
                r#"[{{"entry_id":"^msecret","content_hash":"{}","interim_archived":false}}]"#,
                sha256_hex("Secret thing. (updated: 2026-06-01) ^msecret")
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^msecret",
            false,
        );

        let provider = ScriptedProvider::new(&[]);
        run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 0);
        assert!(
            !confirm.exists(),
            "confirm note must be consumed as satisfied despite stale pending metadata"
        );
    }

    #[tokio::test]
    async fn should_handle_mixed_live_and_archived_ids_in_one_forget_note() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "Live secret. (updated: 2026-06-01) ^mlivsec",
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-05.md"),
            "Old archived secret. (updated: 2026-05-01) ^marcsec\n",
        )
        .unwrap();
        write_note(
            memory_dir,
            "01fg-mixed",
            "host",
            "forget",
            "delete id:^mlivsec and id:^marcsec",
            true,
        );

        // Sensitive-first isolation handles BOTH ids Rust-side — the
        // archived-only scrub (2.5) plus the id-bound executor (2.7) —
        // with zero provider involvement.
        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0);
        assert!(outcome.hard_deleted.contains(&"^marcsec".to_string()));
        assert!(outcome.hard_deleted.contains(&"^mlivsec".to_string()));
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("^mlivsec"));
        let archived =
            std::fs::read_to_string(memory_dir.join("archive/2026-05.md")).unwrap_or_default();
        assert!(
            !archived.contains("^marcsec"),
            "archived-only id scrubbed: {archived}"
        );
    }

    #[tokio::test]
    async fn should_not_scrub_host_notes_when_they_quote_deleted_lines() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "The vault code is 9137. (updated: 2026-06-01) ^msecret";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );
        // A separate free-text host forget QUOTING the deleted line.
        let quoting = write_note(
            memory_dir,
            "02fg-quoting",
            "host",
            "forget",
            "also forget that The vault code is 9137.",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert!(
            quoting.exists(),
            "a host ask must survive the content scrub (its own lifecycle consumes it)"
        );
    }

    #[tokio::test]
    async fn should_park_sensitive_forget_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive location detail. (updated: 2026-06-01) ^msenstv";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let note = write_note(
            memory_dir,
            "01fg-sens",
            "host",
            "forget",
            "forget the sensitive location detail",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let mut p = params(memory_dir);
        p.allow_merge = false; // daily budget exhausted
        let outcome = run_consolidation(provider.clone(), &p).await.unwrap();

        assert_eq!(provider.call_count(), 0);
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^msenstv"),
            "sensitive candidates must hide immediately, not when the budget resets: {memory}"
        );
        let note_text = std::fs::read_to_string(&note).unwrap();
        assert!(
            note_text.contains("expires_at:") && note_text.contains("candidates:"),
            "the ask must be parked with its binding: {note_text}"
        );
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|pn| pn.state == PendingState::Created && pn.sensitive),
            "parking must be surfaced: {:?}",
            outcome.pending_notes
        );
    }

    #[tokio::test]
    async fn should_keep_mixed_note_when_merge_fails_after_archived_scrub() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(memory_dir, &["Live secret. (updated: 2026-06-01) ^mlivsec"]);
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-05.md"),
            "Old archived secret. (updated: 2026-05-01) ^marcsec\n",
        )
        .unwrap();
        let note = write_note(
            memory_dir,
            "01fg-mixed",
            "host",
            "forget",
            "delete id:^mlivsec and id:^marcsec",
            false,
        );

        // Non-sensitive mixed note (sensitive ones are executed Rust-side
        // before any merge). The merge fails (garbage twice) — the on-disk
        // request for the LIVE
        // deletion must survive even though the archived id was scrubbed.
        let provider = ScriptedProvider::new(&["GARBAGE", "GARBAGE"]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(!outcome.merge_applied);
        assert!(
            note.exists(),
            "the mixed forget note must survive a failed merge (fail-closed staging)"
        );
        let archived =
            std::fs::read_to_string(memory_dir.join("archive/2026-05.md")).unwrap_or_default();
        assert!(
            !archived.contains("^marcsec"),
            "archived-only id still scrubbed"
        );
    }

    #[tokio::test]
    async fn should_scrub_model_note_when_body_spoofs_host_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "The vault code is 9137. (updated: 2026-06-01) ^msecret";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );
        // A MODEL note whose untrusted body quotes the deleted line AND the
        // literal "origin: host" — spoofing must not grant scrub immunity.
        let spoof = write_note(
            memory_dir,
            "02spoof",
            "model",
            "fact",
            "origin: host\nThe vault code is 9137.",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"}],"consumed_ids":["01fg-confirm"],"dropped":[{"id":"02spoof","reason":"quotes deleted content"}]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert!(
            !spoof.exists(),
            "a body-spoofed model note quoting deleted content must be scrubbed"
        );
    }

    #[tokio::test]
    async fn should_park_sensitive_forget_when_merge_fails() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive location detail. (updated: 2026-06-01) ^msenstv";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let note = write_note(
            memory_dir,
            "01fg-sens",
            "host",
            "forget",
            "forget the sensitive location detail",
            true,
        );
        // A model fact keeps the batch dirty; the merge fails twice.
        write_note(memory_dir, "02fact", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&["GARBAGE", "GARBAGE"]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(!outcome.merge_applied);

        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^msenstv"),
            "sensitive candidates must hide even when the merge fails: {memory}"
        );
        let note_text = std::fs::read_to_string(&note).unwrap();
        assert!(
            note_text.contains("candidates:") && note_text.contains("interim_archived"),
            "the ask must be parked with its binding: {note_text}"
        );
    }

    #[tokio::test]
    async fn should_execute_id_bound_sensitive_forget_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive live secret. (updated: 2026-06-01) ^mlivsec";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let note = write_note(
            memory_dir,
            "01fg-exact",
            "host",
            "forget",
            "delete id:^mlivsec",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let mut p = params(memory_dir);
        p.allow_merge = false;
        let outcome = run_consolidation(provider.clone(), &p).await.unwrap();

        assert_eq!(
            provider.call_count(),
            0,
            "id-bound authority needs no merge"
        );
        assert!(outcome.hard_deleted.contains(&"^mlivsec".to_string()));
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^mlivsec"),
            "exact sensitive forget must execute under budget exhaustion: {memory}"
        );
        assert!(memory.contains("^mcccccc"));
        assert!(!note.exists(), "the honored note is consumed");
    }

    #[tokio::test]
    async fn should_remove_leftover_archive_copy_when_restore_already_happened() {
        // Crash window: a confirmation run restored the unconfirmed interim
        // candidate into MEMORY.md but died before removing its archive
        // copy. The recovered confirmation must still remove that block.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let confirmed = "Confirmed target. (updated: 2026-06-01) ^mtarget";
        let restored = "Restored survivor. (updated: 2026-06-02) ^msurviv";
        // Crash state: survivor is LIVE again AND its copy is still archived.
        write_memory(
            memory_dir,
            &[restored, "Keeps bonsai. (updated: 2026-06-03) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-06.md"),
            format!("{confirmed}\n\n{restored}\n"),
        )
        .unwrap();
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget those things",
            true,
            &format!(
                r#"[{{"entry_id":"^mtarget","content_hash":"{}","interim_archived":true}},{{"entry_id":"^msurviv","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(confirmed),
                sha256_hex(restored)
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^mtarget",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^mtarget","authorized_by":"02fg-confirm"}],"consumed_ids":["02fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert!(!confirm.exists());

        let archived =
            std::fs::read_to_string(memory_dir.join("archive/2026-06.md")).unwrap_or_default();
        assert!(!archived.contains("^mtarget"), "confirmed target scrubbed");
        assert!(
            !archived.contains("^msurviv"),
            "the already-restored survivor's archive copy must be removed too: {archived}"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(memory.contains("^msurviv"), "survivor stays live");
    }

    #[tokio::test]
    async fn should_reject_merge_when_add_repeats_hard_deleted_line() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "The vault code is 9137. (updated: 2026-06-01) ^msecret";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );

        // The reply deletes the entry AND re-adds its text under a new id.
        let reply = r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"},{"op":"add","section":null,"text":"The vault code is 9137.","sources":[]}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#;
        let provider = ScriptedProvider::new(&[reply, reply]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(
            !outcome.merge_applied,
            "add-back of deleted content must reject the merge"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("^msecret"),
            "nothing applies on a rejected merge"
        );
    }

    #[tokio::test]
    async fn should_delete_multiple_ids_without_panic_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "First target. (updated: 2026-06-01) ^maaaaaa",
                "Middle keeper. (updated: 2026-06-02) ^mcccccc",
                "Second target. (updated: 2026-06-03) ^mbbbbbb",
            ],
        );
        // Named in reverse entry order to exercise the index-shift hazard.
        let note = write_note(
            memory_dir,
            "01fg-multi",
            "host",
            "forget",
            "delete id:^mbbbbbb and id:^maaaaaa",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let mut p = params(memory_dir);
        p.allow_merge = false;
        let outcome = run_consolidation(provider, &p).await.unwrap();

        assert!(outcome.hard_deleted.contains(&"^maaaaaa".to_string()));
        assert!(outcome.hard_deleted.contains(&"^mbbbbbb".to_string()));
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(memory.contains("^mcccccc"), "keeper survives: {memory}");
        assert!(!memory.contains("^maaaaaa") && !memory.contains("^mbbbbbb"));
        assert!(!note.exists(), "fully honored note consumed");
    }

    #[tokio::test]
    async fn should_keep_partially_honored_note_when_some_ids_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let frozen_entry = "Frozen candidate. (updated: 2026-06-01) ^mfrozen";
        write_memory(
            memory_dir,
            &["Live target. (updated: 2026-06-01) ^mlivsec", frozen_entry],
        );
        // ^mfrozen is a candidate of a waiting pending note → frozen. Its
        // binding hash is STALE (the entry changed since parking), so the
        // Rust-side confirmation path refuses destructive action and the
        // id is genuinely deferred to a merged run.
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the frozen thing",
            false,
            &format!(
                r#"[{{"entry_id":"^mfrozen","content_hash":"{}","interim_archived":false}}]"#,
                sha256_hex("An older version of the frozen entry. ^mfrozen")
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let note = write_note(
            memory_dir,
            "02fg-mixed",
            "host",
            "forget",
            "delete id:^mlivsec and id:^mfrozen",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let mut p = params(memory_dir);
        p.allow_merge = false;
        let outcome = run_consolidation(provider, &p).await.unwrap();

        assert!(outcome.hard_deleted.contains(&"^mlivsec".to_string()));
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("^mlivsec"));
        assert!(
            memory.contains("^mfrozen"),
            "frozen id deferred, not deleted"
        );
        assert!(
            note.exists(),
            "a partially honored note must survive as the confirmation for the frozen id"
        );
    }

    #[tokio::test]
    async fn should_reject_add_back_of_interim_archived_content() {
        // The hidden (interim-archived) entry's TEXT must power the
        // add-back guard — a merge may not confirm the delete and re-add
        // the same sensitive content in one reply.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Hidden sensitive fact. (updated: 2026-06-01) ^msenstv";
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(memory_dir.join("archive/2026-07.md"), format!("{secret}\n")).unwrap();
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the hidden fact",
            true,
            &format!(
                r#"[{{"entry_id":"^msenstv","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(secret)
            ),
            "2026-07-20T10:00:00+00:00",
        );
        write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^msenstv",
            true,
        );

        let reply = r#"{"ops":[{"op":"hard_delete","id":"^msenstv","authorized_by":"02fg-confirm"},{"op":"add","section":null,"text":"Hidden sensitive fact.","sources":[]}],"consumed_ids":["02fg-confirm"],"dropped":[]}"#;
        let provider = ScriptedProvider::new(&[reply, reply]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(
            !outcome.merge_applied,
            "interim add-back must be rejected: {:?}",
            outcome.errors
        );
    }

    #[tokio::test]
    async fn should_self_heal_pending_note_when_all_candidates_dead() {
        // Crash window: the interim copy was scrubbed and the entry deleted,
        // but the originating pending note survived. It can never verify or
        // expire — it must self-heal instead of wedging.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let parked = write_pending_note(
            memory_dir,
            "01fg-orphan",
            "forget the thing",
            true,
            &format!(
                r#"[{{"entry_id":"^mgone00","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex("Long gone entry. (updated: 2026-05-01) ^mgone00")
            ),
            "2026-07-20T10:00:00+00:00",
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 0);
        assert!(!parked.exists(), "orphaned pending note must self-heal");
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.note_id == "01fg-orphan" && p.state == PendingState::Confirmed),
            "self-heal must be surfaced: {:?}",
            outcome.pending_notes
        );
    }

    #[tokio::test]
    async fn should_scrub_shared_line_from_surviving_entry_when_hard_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "The vault code is 9137.";
        write_memory(
            memory_dir,
            &[
                &format!("{secret_line}\nKept in the red notebook. (updated: 2026-06-01) ^msecret"),
                &format!("{secret_line}\nAlso likes tea. (updated: 2026-06-02) ^msharer"),
            ],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);

        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("9137"),
            "the shared secret line must leave MEMORY.md entirely: {memory}"
        );
        assert!(
            memory.contains("Also likes tea.") && memory.contains("^msharer"),
            "the surviving entry keeps its other content"
        );
    }

    #[tokio::test]
    async fn should_reject_add_back_when_copy_carries_stale_id_token() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "The vault code is 9137. (updated: 2026-06-01) ^msecret";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            true,
        );

        // Verbatim copy INCLUDING the old ^m token and stamp.
        let reply = r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"},{"op":"add","section":null,"text":"The vault code is 9137. (updated: 2026-06-01) ^msecret","sources":[]}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#;
        let provider = ScriptedProvider::new(&[reply, reply]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(
            !outcome.merge_applied,
            "verbatim add-back with stale id token must be rejected: {:?}",
            outcome.errors
        );
    }

    #[tokio::test]
    async fn should_scrub_daily_notes_and_bookkeeping_variant_copies() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "The vault code is 9137.";
        write_memory(
            memory_dir,
            &[
                &format!("{secret_line} (updated: 2026-06-01) ^msecret"),
                // Same FACT under a different id and stamp.
                &format!("{secret_line} (updated: 2026-05-15) ^mcopyaa"),
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        // The secret also sits in an injectable daily note.
        std::fs::write(
            memory_dir.join("2026-07-07.md"),
            format!("## 2026-07-07\n\n{secret_line}\nharmless other line\n"),
        )
        .unwrap();
        write_note(
            memory_dir,
            "01fg-confirm",
            "host",
            "forget",
            "delete id:^msecret",
            false,
        );

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"hard_delete","id":"^msecret","authorized_by":"01fg-confirm"}],"consumed_ids":["01fg-confirm"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);

        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("9137"),
            "bookkeeping-variant copies must be scrubbed too: {memory}"
        );
        assert!(memory.contains("^mcccccc"));
        let note = std::fs::read_to_string(memory_dir.join("2026-07-07.md")).unwrap();
        assert!(
            !note.contains("9137"),
            "daily notes are injectable and must be scrubbed"
        );
        assert!(note.contains("harmless other line"));
    }

    #[tokio::test]
    async fn should_keep_sensitive_content_out_of_merge_prompts() {
        // The core round-16 property: when other staging forces a merge,
        // the prompt must not contain the sensitive entry text or the
        // sensitive note body — they were parked/executed Rust-side first.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret = "Sensitive location detail. (updated: 2026-06-01) ^msenstv";
        write_memory(
            memory_dir,
            &[secret, "Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01fg-sens",
            "host",
            "forget",
            "forget the sensitive location detail",
            true,
        );
        // A model fact keeps the batch dirty so a merge DOES run.
        write_note(memory_dir, "02fact", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["02fact"]}],"consumed_ids":["02fact"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);

        assert_eq!(provider.call_count(), 1);
        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("Sensitive location detail"),
            "the sensitive entry text must not ride the merge prompt"
        );
        assert!(
            !prompt_text.contains("forget the sensitive location detail"),
            "the sensitive note body must not ride the merge prompt"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("^msenstv"),
            "candidates hidden before the merge"
        );
    }

    #[tokio::test]
    async fn should_expire_pending_note_on_its_expiry_day() {
        // expires_at 10:00 on TODAY: day-granularity must expire it during
        // today's passes, not tomorrow's.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        let parked = write_pending_note(
            memory_dir,
            "01fg-today",
            "forget something vague",
            false,
            "[]",
            &format!("{}T10:00:00+00:00", TODAY),
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(!parked.exists(), "note must expire on its expiry day");
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|p| p.note_id == "01fg-today" && p.state == PendingState::Expired),
            "{:?}",
            outcome.pending_notes
        );
    }

    #[test]
    fn should_skip_symlinked_dirs_when_deleting_backups() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious.bak"), "outside data").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("inside.bak"), "inside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

        super::apply::delete_backups_for_test(dir.path()).unwrap();

        assert!(
            !dir.path().join("inside.bak").exists(),
            "in-tree backups deleted"
        );
        assert!(
            outside.path().join("precious.bak").exists(),
            "the scrub must not follow symlinks out of the memory tree"
        );
    }

    #[tokio::test]
    async fn should_prune_scrubbed_staging_from_merge_prompt() {
        // A sensitive id-bound forget executes Rust-side; a model note
        // quoting the secret is disk-scrubbed by that apply. The later
        // merge (forced by a clean second note) must not ship the quoting
        // note's content to the provider.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "The vault code is 9137.";
        write_memory(
            memory_dir,
            &[
                &format!("{secret_line} (updated: 2026-06-01) ^msecret"),
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        write_note(
            memory_dir,
            "01fg-exact",
            "host",
            "forget",
            "delete id:^msecret",
            true,
        );
        // The quote is a full exact line (the scrub contract is line-exact;
        // substring embeds are the documented paraphrase residual).
        write_note(
            memory_dir,
            "02quoter",
            "model",
            "fact",
            &format!("Overheard this:\n{secret_line}"),
            false,
        );
        write_note(memory_dir, "03clean", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["03clean"]}],"consumed_ids":["03clean"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert_eq!(provider.call_count(), 1);
        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("9137"),
            "scrubbed staging content must not ride the merge prompt"
        );
    }

    #[tokio::test]
    async fn should_reject_consuming_host_request_without_applying_it() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        write_note(
            memory_dir,
            "01req",
            "host",
            "user_request",
            "remember that I deploy on Fridays",
            false,
        );

        // ops:[] but consumed — a silent drop of an explicit ask.
        let reply = r#"{"ops":[],"consumed_ids":["01req"],"dropped":[]}"#;
        let provider = ScriptedProvider::new(&[reply, reply]);
        let outcome = run_consolidation(provider, &params(memory_dir))
            .await
            .unwrap();
        assert!(
            !outcome.merge_applied,
            "unapplied consumption must be rejected"
        );
        assert!(
            memory_dir.join("staging/notes/01req.md").exists(),
            "the request stays durable"
        );
    }

    #[tokio::test]
    async fn should_consume_duplicate_sensitive_notes_naming_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "Sensitive live secret. (updated: 2026-06-01) ^mlivsec",
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        let a = write_note(
            memory_dir,
            "01fg-a",
            "host",
            "forget",
            "delete id:^mlivsec",
            true,
        );
        let b = write_note(
            memory_dir,
            "02fg-b",
            "host",
            "forget",
            "delete id:^mlivsec",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();

        assert_eq!(
            provider.call_count(),
            0,
            "no prompt may carry either note body"
        );
        assert!(outcome.hard_deleted.contains(&"^mlivsec".to_string()));
        assert!(!a.exists() && !b.exists(), "both duplicate asks consumed");
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("^mlivsec"));
    }

    #[tokio::test]
    async fn should_prune_staging_scrubbed_by_archived_only_forget() {
        // An archived-only forget disk-deletes a model note exact-quoting
        // the archived line; the later merge prompt must not contain it.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let archived_line = "Old archived secret.";
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-01) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-05.md"),
            format!("{archived_line} (updated: 2026-05-01) ^marcsec\n"),
        )
        .unwrap();
        write_note(
            memory_dir,
            "01fg-arc",
            "host",
            "forget",
            "delete id:^marcsec",
            true,
        );
        write_note(
            memory_dir,
            "02quoter",
            "model",
            "fact",
            &format!("Overheard:\n{archived_line}"),
            false,
        );
        write_note(memory_dir, "03clean", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["03clean"]}],"consumed_ids":["03clean"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert_eq!(provider.call_count(), 1);
        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("Old archived secret"),
            "archived-only scrubbed staging must not ride the merge prompt"
        );
    }

    #[tokio::test]
    async fn should_consume_non_sensitive_sibling_when_sensitive_deletes_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        write_memory(
            memory_dir,
            &[
                "Sensitive live secret. (updated: 2026-06-01) ^mlivsec",
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        let sens = write_note(
            memory_dir,
            "01fg-sens",
            "host",
            "forget",
            "delete id:^mlivsec",
            true,
        );
        let plain = write_note(
            memory_dir,
            "02fg-plain",
            "host",
            "forget",
            "delete id:^mlivsec",
            false,
        );

        let provider = ScriptedProvider::new(&[]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0);
        assert!(outcome.hard_deleted.contains(&"^mlivsec".to_string()));
        assert!(
            !sens.exists() && !plain.exists(),
            "both siblings consumed — the non-sensitive one must not wedge the validator"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(!memory.contains("^mlivsec"));
    }

    #[tokio::test]
    async fn should_return_scrubbed_entries_after_rust_side_delete() {
        // A surviving entry shares an exact line with the deleted one. The
        // Rust-side apply scrubs it from MEMORY.md — the entries used for
        // the SAME run's merge prompt must reflect that, or the line leaks
        // to the provider and gets written back by the final apply.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "The vault code is 9137.";
        write_memory(
            memory_dir,
            &[
                &format!("{secret_line}\nKept in the notebook. (updated: 2026-06-01) ^msecret"),
                &format!("{secret_line}\nAlso likes tea. (updated: 2026-06-02) ^msharer"),
            ],
        );
        write_note(
            memory_dir,
            "01fg-exact",
            "host",
            "forget",
            "delete id:^msecret",
            true,
        );
        write_note(memory_dir, "02clean", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["02clean"]}],"consumed_ids":["02clean"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);

        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("9137"),
            "the scrubbed shared line must not ride the merge prompt"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            !memory.contains("9137"),
            "the final apply must not write the scrubbed line back: {memory}"
        );
        assert!(memory.contains("Also likes tea."));
    }

    #[tokio::test]
    async fn should_not_misremove_notes_when_scrub_prunes_earlier_paths() {
        // A model note that sorts BEFORE the authorizing forget quotes the
        // secret exact-line: the scrub prune shifts batch positions, and
        // consumption must still remove the RIGHT notes (by identity).
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let secret_line = "The vault code is 9137.";
        write_memory(
            memory_dir,
            &[
                &format!("{secret_line} (updated: 2026-06-01) ^msecret"),
                "Keeps bonsai. (updated: 2026-06-01) ^mcccccc",
            ],
        );
        // "00quoter" sorts before "01fg-exact".
        write_note(
            memory_dir,
            "00quoter",
            "model",
            "fact",
            &format!("Overheard:\n{secret_line}"),
            false,
        );
        write_note(
            memory_dir,
            "01fg-exact",
            "host",
            "forget",
            "delete id:^msecret",
            true,
        );
        let keeper = write_note(memory_dir, "02keeper", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["02keeper"]}],"consumed_ids":["02keeper"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert!(
            !keeper.exists(),
            "the keeper was consumed by the merge, not misremoved earlier"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("Likes tea."),
            "keeper's fact landed: {memory}"
        );
    }

    #[tokio::test]
    async fn should_confirm_pending_forget_when_budget_exhausted() {
        // A parked sensitive pending (two interim candidates) + the user's
        // id-bound confirmation for ONE of them, arriving while the merge
        // budget is exhausted: the confirmation is deterministic and must
        // execute now — confirmed id scrubbed, the other candidate
        // restored, both note files consumed, zero provider calls.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let confirmed = "Confirmed target. (updated: 2026-06-01) ^mtarget";
        let survivor = "Restored survivor. (updated: 2026-06-02) ^msurviv";
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-03) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(
            memory_dir.join("archive/2026-06.md"),
            format!("{confirmed}\n\n{survivor}\n"),
        )
        .unwrap();
        let parked = write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget those things",
            true,
            &format!(
                r#"[{{"entry_id":"^mtarget","content_hash":"{}","interim_archived":true}},{{"entry_id":"^msurviv","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(confirmed),
                sha256_hex(survivor)
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^mtarget",
            true,
        );

        let provider = ScriptedProvider::new(&[]);
        let mut p = params(memory_dir);
        p.allow_merge = false;
        let outcome = run_consolidation(provider.clone(), &p).await.unwrap();

        assert_eq!(provider.call_count(), 0);
        assert!(outcome.hard_deleted.contains(&"^mtarget".to_string()));
        assert!(!parked.exists(), "confirmed pending note consumed");
        assert!(!confirm.exists(), "confirm note consumed");
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("^msurviv"),
            "unconfirmed candidate restored: {memory}"
        );
        assert!(!memory.contains("^mtarget"));
        let archived =
            std::fs::read_to_string(memory_dir.join("archive/2026-06.md")).unwrap_or_default();
        assert!(
            !archived.contains("^mtarget"),
            "confirmed target scrubbed from archive"
        );
        assert!(
            !archived.contains("^msurviv"),
            "restored block removed from archive"
        );
        assert!(
            outcome
                .pending_notes
                .iter()
                .any(|pn| pn.note_id == "01fg-parked" && pn.state == PendingState::Confirmed),
            "{:?}",
            outcome.pending_notes
        );
    }

    #[tokio::test]
    async fn should_confirm_sensitive_pending_rust_side_with_budget_available() {
        // Budget available + dirty batch: the merge RUNS, but the sensitive
        // confirmation of a frozen interim candidate executes Rust-side
        // first — the prompt carries neither the note body nor the id.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let hidden = "Hidden sensitive fact. (updated: 2026-06-01) ^mtarget";
        write_memory(
            memory_dir,
            &["Keeps bonsai. (updated: 2026-06-03) ^mcccccc"],
        );
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        std::fs::write(memory_dir.join("archive/2026-07.md"), format!("{hidden}\n")).unwrap();
        let parked = write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the hidden fact",
            true,
            &format!(
                r#"[{{"entry_id":"^mtarget","content_hash":"{}","interim_archived":true}}]"#,
                sha256_hex(hidden)
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^mtarget",
            true,
        );
        write_note(memory_dir, "03clean", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["03clean"]}],"consumed_ids":["03clean"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);
        assert!(outcome.hard_deleted.contains(&"^mtarget".to_string()));
        assert!(
            !parked.exists() && !confirm.exists(),
            "both notes consumed Rust-side"
        );

        assert_eq!(provider.call_count(), 1);
        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("^mtarget") && !prompt_text.contains("yes: id:"),
            "the sensitive confirmation must not ride the merge prompt"
        );
        let archived =
            std::fs::read_to_string(memory_dir.join("archive/2026-07.md")).unwrap_or_default();
        assert!(!archived.contains("^mtarget"), "confirmed target scrubbed");
    }

    #[tokio::test]
    async fn should_keep_stale_sensitive_confirm_out_of_prompt() {
        // A sensitive confirmation whose binding hash is stale defers
        // fail-closed — but its body still may not ride the merge prompt.
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path();
        let frozen_entry = "Frozen candidate. (updated: 2026-06-01) ^mfrozen";
        write_memory(
            memory_dir,
            &[frozen_entry, "Keeps bonsai. (updated: 2026-06-03) ^mcccccc"],
        );
        write_pending_note(
            memory_dir,
            "01fg-parked",
            "forget the frozen thing",
            false,
            &format!(
                r#"[{{"entry_id":"^mfrozen","content_hash":"{}","interim_archived":false}}]"#,
                sha256_hex("An older frozen version. ^mfrozen")
            ),
            "2026-07-20T10:00:00+00:00",
        );
        let confirm = write_note(
            memory_dir,
            "02fg-confirm",
            "host",
            "forget",
            "yes: id:^mfrozen",
            true,
        );
        write_note(memory_dir, "03clean", "model", "fact", "likes tea", false);

        let provider = ScriptedProvider::new(&[
            r#"{"ops":[{"op":"add","section":null,"text":"Likes tea.","sources":["03clean"]}],"consumed_ids":["03clean"],"dropped":[]}"#,
        ]);
        let outcome = run_consolidation(provider.clone(), &params(memory_dir))
            .await
            .unwrap();
        assert!(outcome.merge_applied, "{:?}", outcome.errors);

        assert!(confirm.exists(), "stale confirmation defers fail-closed");
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("^mfrozen"),
            "no destructive action on stale hash"
        );
        let prompt_text: String = provider
            .call_messages(0)
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !prompt_text.contains("yes: id:^mfrozen"),
            "deferred sensitive confirmation must not ride the merge prompt"
        );
    }
}
