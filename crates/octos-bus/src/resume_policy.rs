//! Structured resume pipeline (M8.6).
//!
//! When octos reloads a session from JSONL at startup or after a crash, the
//! transcript may contain state that the provider will reject (unresolved
//! tool uses → 400), orphaned thinking-only assistant messages, whitespace-
//! only assistant messages, or stale worktree references. [`ResumePolicy`]
//! sanitizes the loaded [`Message`] list before the session actor picks it
//! up, emitting a typed [`SessionSanitizeReport`] for observability.
//!
//! The policy is a pure data-layer transform: it operates on `Vec<Message>`
//! that has already been loaded from disk. The JSONL format is not touched —
//! every filter is pass-through for legacy messages; filters only prune.
//!
//! # Filter passes
//!
//! 1. [`filter_unresolved_tool_uses`] — walks the list, collects all
//!    `tool_call_id` values on assistant `tool_calls`, then drops
//!    tool-result messages whose id is not in that set and drops assistant
//!    tool-call messages whose ids have no matching tool result (unless the
//!    call is referenced by pending retry state).
//! 2. [`filter_orphaned_thinking_only_messages`] — drops assistant messages
//!    that have `reasoning_content` but empty `content` and no tool calls.
//!    A thinking-only message at the tail of the transcript is preserved
//!    ONLY when `preserve_in_flight_tail` is set — i.e. a live in-process
//!    retry is under way ([`ResumePolicy::sanitize`] derives this from a
//!    non-`None` `retry_state`). On a cold reload from disk there is no
//!    in-flight turn by construction, so the interrupted thinking-only tail
//!    is failed (dropped) rather than resurrected and resumed.
//! 3. [`filter_whitespace_only_assistant_messages`] — drops assistant
//!    messages whose `content.trim().is_empty()` and no tool calls and no
//!    reasoning content.
//! 4. [`reconstruct_content_replacement_state`] — collects file paths
//!    referenced by tool results into [`ReplacementStateRef`] entries for
//!    M8.4 `FileStateCache` integration (stub).
//!
//! # Worktree check
//!
//! When `workspace_root` is provided, the policy stats the path. If it no
//! longer exists, the report's `worktree_missing` flag is set and an
//! `Err` is returned so the caller can decide to refuse resume or create a
//! new session. When present, a marker file is touched inside the worktree
//! to bump the containing directory's mtime — this prevents stale-cleanup
//! races where a concurrent GC sweep removes the worktree while the session
//! is mid-load (Claude Code issue #22355).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use octos_core::{Message, MessageRole};
use tracing::warn;

/// Name of the marker file written inside a sub-agent worktree on resume to
/// bump the directory's mtime. The contents are a human-readable RFC3339
/// timestamp so operators can see when the session last resumed.
pub const RESUME_MTIME_MARKER: &str = ".octos_resume_mtime";

// Compatibility re-exports: these DTOs live in the dependency-free core so
// octos-agent no longer needs a reverse dependency on the message bus.
pub use octos_core::{ReplacementStateRef, SanitizeError, SanitizeOutcome, SessionSanitizeReport};

/// Abstract view of in-flight retry state — a set of tool_call_ids that
/// must not be dropped even if they lack a matching tool result. Concrete
/// retry-state types in `octos-agent` (e.g. a future `LoopRetryState` with
/// pending id tracking) implement this to bridge into the policy without
/// introducing a reverse crate dependency.
///
/// WARNING (#2204): a value handed to [`ResumePolicy::sanitize`] must reflect
/// state that is LIVE in the current process. `sanitize` treats a non-`None`
/// `retry_state` as "a turn is in flight" and, on that basis, preserves a
/// trailing thinking-only turn. A persisted retry-state sidecar reloaded from
/// disk is NOT live — passing it in on a cold reload would resurrect an
/// interrupted turn. Pass `None` on every cold reload.
pub trait RetryStateView {
    /// Returns `true` when the given tool_call_id is pinned by an in-flight
    /// retry (e.g. the harness is about to replay the call after a provider
    /// hiccup). The policy keeps these calls in the transcript even when
    /// their result is missing.
    fn contains_tool_call(&self, tool_call_id: &str) -> bool;
}

impl<T: RetryStateView + ?Sized> RetryStateView for &T {
    fn contains_tool_call(&self, tool_call_id: &str) -> bool {
        (*self).contains_tool_call(tool_call_id)
    }
}

impl RetryStateView for HashSet<String> {
    fn contains_tool_call(&self, tool_call_id: &str) -> bool {
        self.contains(tool_call_id)
    }
}

/// Top-level resume sanitizer. Stateless entry point; see module docs for
/// the full pass ordering and semantics.
pub struct ResumePolicy;

impl ResumePolicy {
    /// Sanitize a just-loaded transcript and report what changed.
    ///
    /// `retry_state` pins in-flight tool_call_ids so we don't drop a call
    /// the harness is about to replay. `workspace_root` when provided is
    /// stat'd and mtime-bumped — a missing path short-circuits to
    /// [`SanitizeError::WorktreeMissing`] after the transcript has been
    /// sanitized (the report is still populated so callers can log it).
    pub fn sanitize(
        messages: Vec<Message>,
        retry_state: Option<&dyn RetryStateView>,
        workspace_root: Option<&Path>,
    ) -> Result<SanitizeOutcome, SanitizeError> {
        let mut report = SessionSanitizeReport {
            input_len: messages.len(),
            ..Default::default()
        };

        // Pass 1: drop unresolved tool_use/tool_result pairs.
        let (messages, dropped_tool_use) = filter_unresolved_tool_uses(messages, retry_state);
        report.unresolved_tool_uses_dropped = dropped_tool_use;

        // Pass 2: drop orphan thinking-only assistant messages.
        //
        // A thinking-only tail is preserved only during a live in-process
        // retry. `retry_state` is `Some` only in that case; on every cold
        // reload from disk it is `None`, and the process that produced the
        // tail is gone — so the interrupted turn is failed (dropped) here
        // rather than resurrected and resumed by the session actor.
        //
        // INVARIANT (#2204): `retry_state` here means "a turn is live in this
        // process", NOT merely "retry state exists on disk". `LoopRetryState`
        // is persisted to a sidecar and survives a cold reload — it must NEVER
        // be loaded from disk and threaded into `sanitize`, because `is_some()`
        // would then be `true` on a cold reload and resurrect exactly the dead
        // thinking-only tail this pass exists to fail. Cold-reload callers pass
        // `None`; a live-retry caller passes the in-memory pending-id set.
        let preserve_in_flight_tail = retry_state.is_some();
        let (messages, dropped_thinking) =
            filter_orphaned_thinking_only_messages(messages, preserve_in_flight_tail);
        report.orphan_thinking_dropped = dropped_thinking;

        // Pass 3: drop whitespace-only assistant messages.
        let (messages, dropped_ws) = filter_whitespace_only_assistant_messages(messages);
        report.whitespace_only_dropped = dropped_ws;

        // Pass 4: collect content-replacement refs from tool results.
        //
        // TODO(M8.4): after FileStateCache lands, populate its entries from
        // these refs when the cache is non-empty post-load. The integration
        // point is the caller of `ResumePolicy::sanitize` — it should feed
        // `outcome.content_replacements` into the file-state cache before
        // handing the messages to the session actor.
        let content_replacements = reconstruct_content_replacement_state(&messages);
        report.content_replacements_restored = content_replacements.len();

        report.output_len = messages.len();

        // Worktree existence check + mtime bump.
        if let Some(root) = workspace_root {
            match check_and_bump_worktree(root) {
                WorktreeStatus::Present => {}
                WorktreeStatus::Missing => {
                    report.worktree_missing = true;
                    return Err(SanitizeError::WorktreeMissing {
                        path: root.to_path_buf(),
                        report,
                    });
                }
                WorktreeStatus::BumpFailed { error } => {
                    report
                        .warnings
                        .push(format!("mtime bump failed for {}: {error}", root.display()));
                }
            }
        }

        Ok(SanitizeOutcome {
            messages,
            report,
            content_replacements,
        })
    }
}

/// Pass 1: drop unresolved tool_use / tool_result pairs.
///
/// Walks the list once to collect:
///   - `result_ids`: every `tool_call_id` on a Tool-role message (i.e. every
///     id for which a result already exists).
///
/// Then walks again and keeps:
///   - Non-assistant / non-tool messages as-is.
///   - Tool-role messages whose `tool_call_id` is in `result_ids` (trivially
///     always true, but the guard catches malformed entries).
///   - Assistant messages whose `tool_calls` (if any) all have matching
///     results OR are pinned by `retry_state`. An assistant message with
///     tool_calls all-missing AND unpinned is dropped — unless it also has
///     non-empty text content, in which case we keep the text but strip the
///     unresolved tool_calls so the provider accepts the request.
///
/// Preserves message order. Returns the filtered list plus the count of
/// assistant tool-call messages affected (either fully dropped or had their
/// tool_calls stripped).
pub fn filter_unresolved_tool_uses(
    messages: Vec<Message>,
    retry_state: Option<&dyn RetryStateView>,
) -> (Vec<Message>, usize) {
    let mut result_ids: HashSet<String> = HashSet::new();
    for msg in &messages {
        if !matches!(msg.role, MessageRole::Tool) {
            continue;
        }
        if let Some(id) = msg.tool_call_id.as_deref() {
            result_ids.insert(id.to_string());
        }
    }

    let mut dropped = 0_usize;
    let mut kept = Vec::with_capacity(messages.len());

    for msg in messages.into_iter() {
        match msg.role {
            MessageRole::Tool => {
                if let Some(id) = msg.tool_call_id.as_deref() {
                    // A tool_result whose tool_call_id has no matching
                    // assistant tool_call would also be orphaned, but
                    // we can only detect this if we also track call_ids
                    // on assistant messages. Do that here.
                    if result_has_matching_call(&kept, id) {
                        kept.push(msg);
                    } else {
                        dropped += 1;
                    }
                } else {
                    // Tool-role message with no id is malformed — drop.
                    dropped += 1;
                }
            }
            MessageRole::Assistant => {
                let Some(calls) = msg.tool_calls.as_ref() else {
                    kept.push(msg);
                    continue;
                };
                if calls.is_empty() {
                    kept.push(msg);
                    continue;
                }
                // M8.6 fix-first item 2: per-call filtering, not all-or-
                // nothing. Walk the assistant's tool_calls and keep the
                // ones whose ids are resolved (matching Tool message
                // present) or retry-pinned. Drop only the unresolved
                // ones. This preserves valid tool results from the same
                // assistant turn that would otherwise be orphaned when a
                // sibling call lacked a matching result.
                let has_text = !msg.content.trim().is_empty();
                let mut kept_calls: Vec<octos_core::ToolCall> = Vec::with_capacity(calls.len());
                let mut had_unresolved = false;
                for call in calls.iter() {
                    let resolved = result_ids.contains(call.id.as_str())
                        || retry_state
                            .map(|state| state.contains_tool_call(&call.id))
                            .unwrap_or(false);
                    if resolved {
                        kept_calls.push(call.clone());
                    } else {
                        had_unresolved = true;
                    }
                }
                if had_unresolved {
                    dropped += 1;
                }
                if !kept_calls.is_empty() {
                    // At least one call survived — keep the assistant
                    // message with the filtered call set so its matching
                    // Tool results don't get dropped as orphans.
                    let mut filtered = msg;
                    filtered.tool_calls = Some(kept_calls);
                    kept.push(filtered);
                } else if has_text {
                    // No surviving calls but the message has prose — keep
                    // the prose so the conversation flow stays intact.
                    let mut stripped = msg;
                    stripped.tool_calls = None;
                    kept.push(stripped);
                } else {
                    // No prose, no surviving calls — the assistant
                    // message has nothing left to keep.
                    // (already counted in `dropped`)
                }
            }
            _ => kept.push(msg),
        }
    }

    (kept, dropped)
}

/// Returns `true` when any already-kept assistant message has a tool_call
/// with id == `id`. Used as the inverse check for orphaned tool results.
fn result_has_matching_call(kept: &[Message], id: &str) -> bool {
    kept.iter().any(|msg| {
        matches!(msg.role, MessageRole::Assistant)
            && msg
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().any(|call| call.id == id))
                .unwrap_or(false)
    })
}

/// Pass 2: drop orphaned thinking-only assistant messages.
///
/// An assistant message is "thinking-only" when:
///   - `reasoning_content` is `Some(non-empty)`.
///   - `content.trim().is_empty()`.
///   - `tool_calls` is None or empty.
///
/// Such messages are dropped. The one exception is the tail of the transcript
/// when `preserve_in_flight_tail` is `true`: that represents a live in-process
/// reasoning turn the harness is about to continue, so it is kept. When
/// `preserve_in_flight_tail` is `false` (a cold reload from disk, where no turn
/// is in flight by construction) the tail is treated like any other orphan and
/// dropped — failing the interrupted turn instead of resurrecting it on resume.
pub fn filter_orphaned_thinking_only_messages(
    messages: Vec<Message>,
    preserve_in_flight_tail: bool,
) -> (Vec<Message>, usize) {
    let total = messages.len();
    let mut dropped = 0_usize;
    let mut kept = Vec::with_capacity(total);

    for (idx, msg) in messages.into_iter().enumerate() {
        // The tail thinking-only message is kept only when a live in-process
        // retry is under way (`preserve_in_flight_tail`). On a cold reload it
        // is an interrupted turn with no live task behind it, so it is dropped
        // like any other orphan rather than resumed.
        let is_tail = idx + 1 == total;
        let is_live_tail = is_tail && preserve_in_flight_tail;
        if !is_live_tail && is_thinking_only(&msg) {
            dropped += 1;
        } else {
            kept.push(msg);
        }
    }

    (kept, dropped)
}

fn is_thinking_only(msg: &Message) -> bool {
    if !matches!(msg.role, MessageRole::Assistant) {
        return false;
    }
    let reasoning = msg
        .reasoning_content
        .as_deref()
        .map(|r| !r.trim().is_empty())
        .unwrap_or(false);
    if !reasoning {
        return false;
    }
    let empty_content = msg.content.trim().is_empty();
    let empty_calls = msg
        .tool_calls
        .as_ref()
        .map(|calls| calls.is_empty())
        .unwrap_or(true);
    empty_content && empty_calls
}

/// Pass 3: drop assistant messages that carry no useful payload.
///
/// Criteria: role=Assistant AND `content.trim().is_empty()` AND no
/// `tool_calls` AND no `reasoning_content`. The message contributes nothing
/// to the transcript and some providers reject it outright.
pub fn filter_whitespace_only_assistant_messages(messages: Vec<Message>) -> (Vec<Message>, usize) {
    let mut dropped = 0_usize;
    let mut kept = Vec::with_capacity(messages.len());

    for msg in messages.into_iter() {
        if is_whitespace_only_assistant(&msg) {
            dropped += 1;
        } else {
            kept.push(msg);
        }
    }

    (kept, dropped)
}

fn is_whitespace_only_assistant(msg: &Message) -> bool {
    if !matches!(msg.role, MessageRole::Assistant) {
        return false;
    }
    if !msg.content.trim().is_empty() {
        return false;
    }
    let has_calls = msg
        .tool_calls
        .as_ref()
        .map(|calls| !calls.is_empty())
        .unwrap_or(false);
    if has_calls {
        return false;
    }
    let has_reasoning = msg
        .reasoning_content
        .as_deref()
        .map(|r| !r.trim().is_empty())
        .unwrap_or(false);
    if has_reasoning {
        return false;
    }
    true
}

fn roots_user_turn(msg: &Message) -> bool {
    matches!(msg.role, MessageRole::User) && !crate::session::is_peer_message_record(msg)
}

/// Count the number of distinct user turns in `messages`.
///
/// A "user turn" is the message group sharing one `User` message's
/// [`Message::thread_id`] (M8.10 thread grouping). System messages and the
/// assistant/tool replies that inherit a turn's thread_id never start a new
/// turn, so the count equals the number of distinct thread_ids rooted by a
/// `User` message.
///
/// A recorded workspace peer message (UPCR-2026-RF506E103) is a `User` row
/// another agent wrote. It roots no user turn: a client that did not ask for
/// those records never sees the row, so counting it would move the turn its
/// `num_turns` names. The turn it started is kept like any other turn no
/// person's message roots.
pub(crate) fn count_user_turns(messages: &[Message]) -> u32 {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut count = 0_u32;
    for msg in messages {
        if roots_user_turn(msg) {
            if let Some(tid) = msg.thread_id.as_deref() {
                if seen.insert(tid) {
                    count = count.saturating_add(1);
                }
            }
        }
    }
    count
}

/// Drop the last `n` user turns from `messages` in place, returning the number
/// of turns actually removed (clamped to the transcript's turn count).
///
/// A user turn is the set of messages sharing one `User` message's
/// [`Message::thread_id`] (M8.10). Removing the last `n` turns deletes those
/// groups' user + assistant + tool messages — i.e. everything from the
/// `n`-from-last user message onward. Leading `System` messages and any
/// compaction-summary `System` message carry `thread_id == None`, are never
/// part of a user turn, and always survive; dropping every user turn therefore
/// returns the transcript to its pre-first-user state.
///
/// Deterministic and idempotent when driven by the append-only rollback marker:
/// applied at the marker's position in the JSONL log, replaying the same log
/// always yields the same trimmed transcript. `n == 0` is a no-op.
pub(crate) fn drop_last_n_user_turns(messages: &mut Vec<Message>, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    // Ordered, distinct thread_ids rooted by a `User` message.
    let mut user_threads: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        if roots_user_turn(msg) {
            if let Some(tid) = msg.thread_id.clone() {
                if seen.insert(tid.clone()) {
                    user_threads.push(tid);
                }
            }
        }
    }
    let drop_count = (n as usize).min(user_threads.len());
    if drop_count == 0 {
        return 0;
    }
    let to_drop: HashSet<String> = user_threads
        .split_off(user_threads.len() - drop_count)
        .into_iter()
        .collect();
    messages.retain(|msg| match msg.thread_id.as_deref() {
        Some(tid) => !to_drop.contains(tid),
        None => true,
    });
    drop_count as u32
}

/// Pass 4: collect content-replacement refs from tool results.
///
/// Scans every tool-role message for file paths. Heuristic: parse the tool
/// result body as JSON and look for top-level `path` or `file` fields, OR
/// fall back to a line-based scan for `path: <value>` / `file: <value>`.
/// The output is a list of [`ReplacementStateRef`] with `content_hash:
/// None` — M8.4 will populate hashes once the `FileStateCache` restore
/// step is in place.
pub fn reconstruct_content_replacement_state(messages: &[Message]) -> Vec<ReplacementStateRef> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();

    for msg in messages {
        if !matches!(msg.role, MessageRole::Tool) {
            continue;
        }

        // First try structured parse: if content is JSON, look for known
        // field names.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
            extract_paths_from_json(&value, &mut |path| {
                push_unique(&mut refs, &mut seen, path);
            });
        }

        // Also fall back to line-based scan for tool results that emit
        // plaintext like "wrote 12 bytes to <path>" or "read <path>".
        for line in msg.content.lines() {
            if let Some(path) = extract_path_from_line(line) {
                push_unique(&mut refs, &mut seen, path);
            }
        }
    }

    refs
}

fn push_unique(refs: &mut Vec<ReplacementStateRef>, seen: &mut HashSet<String>, path: String) {
    if path.is_empty() {
        return;
    }
    if seen.insert(path.clone()) {
        refs.push(ReplacementStateRef {
            path: PathBuf::from(path),
            content_hash: None,
        });
    }
}

fn extract_paths_from_json(value: &serde_json::Value, push: &mut dyn FnMut(String)) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if matches!(key.as_str(), "path" | "file" | "file_path" | "filename") {
                    if let serde_json::Value::String(s) = val {
                        push(s.clone());
                    }
                }
                extract_paths_from_json(val, push);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                extract_paths_from_json(item, push);
            }
        }
        _ => {}
    }
}

fn extract_path_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["path:", "file:", "wrote ", "read "] {
        let Some(rest) = trimmed.strip_prefix(prefix) else {
            continue;
        };
        let candidate = rest.trim().trim_matches('"').trim_matches('\'');
        if candidate.contains(['/', '\\']) && candidate.len() < 512 {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Internal result of the worktree existence + mtime bump helper.
enum WorktreeStatus {
    Present,
    Missing,
    BumpFailed { error: String },
}

/// Stat the worktree and, when present, touch a marker file inside it to
/// bump the containing directory's mtime. The marker is written
/// non-atomically — a concurrent resume is fine because both writes are
/// idempotent (the file is overwritten with the current timestamp).
///
/// Returns [`WorktreeStatus::Missing`] if `root` does not exist (caller
/// escalates to refuse resume). Returns [`WorktreeStatus::BumpFailed`] if
/// the stat succeeds but writing the marker fails — non-fatal, logged as a
/// report warning.
fn check_and_bump_worktree(root: &Path) -> WorktreeStatus {
    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => bump_mtime_marker(root),
        Ok(_) => {
            warn!(path = %root.display(), "worktree root is not a directory");
            WorktreeStatus::Missing
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorktreeStatus::Missing,
        Err(error) => WorktreeStatus::BumpFailed {
            error: error.to_string(),
        },
    }
}

fn bump_mtime_marker(root: &Path) -> WorktreeStatus {
    let marker = root.join(RESUME_MTIME_MARKER);
    let timestamp = Utc::now().to_rfc3339();
    match std::fs::write(&marker, timestamp.as_bytes()) {
        Ok(()) => WorktreeStatus::Present,
        Err(error) => WorktreeStatus::BumpFailed {
            error: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use octos_core::{Message, MessageRole, ToolCall};

    fn user(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            source: None,
            participant: None,
        }
    }

    fn assistant_with_calls(content: &str, call_ids: &[&str]) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: Some(
                call_ids
                    .iter()
                    .map(|id| ToolCall {
                        id: (*id).to_string(),
                        name: "shell".into(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    })
                    .collect(),
            ),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 2).unwrap(),
            source: None,
            participant: None,
        }
    }

    fn tool_result(tool_call_id: &str, body: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: body.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 3).unwrap(),
            source: None,
            participant: None,
        }
    }

    // -----------------------------------------------------------------------
    // Item 2 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24: per-call filtering
    // for partial resolution. The legacy `all_resolved` branch dropped or
    // stripped the entire `tool_calls` vector if any sibling was unresolved,
    // orphaning matching tool results. The fix keeps resolved/pinned calls
    // and drops only the unresolved ones.
    // -----------------------------------------------------------------------

    fn assistant_with_mixed_calls(content: &str, call_ids: &[&str]) -> Message {
        // Builder shared by the partial-resolution tests. Distinct from
        // `assistant_with_calls` only in that it is reused for clarity.
        assistant_with_calls(content, call_ids)
    }

    #[test]
    fn retry_pinned_unresolved_call_does_not_delete_resolved_sibling_result() {
        // Three calls: call_a is resolved by a Tool result, call_b is
        // retry-pinned (still pending), call_c is fully unresolved. The
        // sanitizer must keep call_a and call_b on the assistant message,
        // drop call_c, preserve the prose, and keep the matching Tool
        // result for call_a.
        let messages = vec![
            user("kick off"),
            assistant_with_mixed_calls("ack", &["call_a", "call_b", "call_c"]),
            tool_result("call_a", r#"{"output": "a-done"}"#),
        ];

        let mut retry: HashSet<String> = HashSet::new();
        retry.insert("call_b".into());

        let (filtered, dropped) =
            filter_unresolved_tool_uses(messages, Some(&retry as &dyn RetryStateView));

        assert_eq!(
            dropped, 1,
            "exactly one assistant message had a dropped call"
        );
        let kept_ids: Vec<&str> = filtered[1]
            .tool_calls
            .as_ref()
            .expect("kept calls must remain")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert!(
            kept_ids.contains(&"call_a"),
            "resolved call_a must survive: kept_ids={kept_ids:?}"
        );
        assert!(
            kept_ids.contains(&"call_b"),
            "retry-pinned call_b must survive: kept_ids={kept_ids:?}"
        );
        assert!(
            !kept_ids.contains(&"call_c"),
            "unresolved call_c must be removed: kept_ids={kept_ids:?}"
        );
        let tool_results: Vec<&Message> = filtered
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(
            tool_results.len(),
            1,
            "matching Tool result for call_a must not be orphaned by partial-resolution sanitize"
        );
    }

    fn in_thread(mut message: Message, thread: &str) -> Message {
        message.thread_id = Some(thread.into());
        message
    }

    fn peer_message_record(thread: &str) -> Message {
        let mut record = in_thread(user("[workspace-peer-message] ..."), thread);
        record.source = Some(octos_core::ui_protocol::WORKSPACE_PEER_MESSAGE_SOURCE.into());
        record.participant = Some("workspace-1".into());
        record
    }

    fn reply(content: &str, thread: &str) -> Message {
        in_thread(assistant_with_calls(content, &[]), thread)
    }

    /// UPCR-2026-RF506E103: `num_turns` names a turn by counting back from
    /// the person's newest prompt. A client that did not ask for peer message
    /// records never sees the recorded row, so the row must not shift that
    /// count.
    #[test]
    fn should_not_count_a_recorded_peer_message_when_rolling_back_user_turns() {
        let transcript = vec![
            in_thread(user("first prompt"), "t1"),
            reply("first answer", "t1"),
            in_thread(user("second prompt"), "t2"),
            reply("second answer", "t2"),
            peer_message_record("t3"),
            reply("reply to the peer", "t3"),
        ];
        assert_eq!(count_user_turns(&transcript), 2);

        let mut rolled_back = transcript.clone();
        assert_eq!(drop_last_n_user_turns(&mut rolled_back, 1), 1);
        let kept: Vec<&str> = rolled_back.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            kept,
            [
                "first prompt",
                "first answer",
                "[workspace-peer-message] ...",
                "reply to the peer"
            ],
            "the newest turn a person started is dropped; the peer exchange is kept, \
             as its reply was before the message was recorded"
        );

        let mut emptied = transcript;
        assert_eq!(drop_last_n_user_turns(&mut emptied, 9), 2);
        assert_eq!(count_user_turns(&emptied), 0);
        assert_eq!(emptied.len(), 2, "only the peer exchange remains");
    }
}
