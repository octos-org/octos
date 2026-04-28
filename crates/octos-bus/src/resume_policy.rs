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
//!    that have `reasoning_content` but empty `content` and no tool calls,
//!    unless the message is the tail of the transcript (allow in-flight
//!    reasoning).
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
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Name of the marker file written inside a sub-agent worktree on resume to
/// bump the directory's mtime. The contents are a human-readable RFC3339
/// timestamp so operators can see when the session last resumed.
pub const RESUME_MTIME_MARKER: &str = ".octos_resume_mtime";

/// Reference to a file path recovered from a tool result during resume.
///
/// Populated by [`reconstruct_content_replacement_state`]; consumed by
/// M8.4 `FileStateCache` to seed the cache with the paths that the
/// transcript claims were last read/written. The hash field is always
/// `None` in this workstream — it becomes `Some(hash)` once M8.4 lands
/// and the file-state cache actually restores entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementStateRef {
    /// Absolute or workspace-relative path the tool result referenced.
    pub path: PathBuf,
    /// Content hash when available; None indicates placeholder state
    /// pending M8.4 cache restore.
    pub content_hash: Option<String>,
}

/// Typed report describing what [`ResumePolicy::sanitize`] dropped.
///
/// Emitted on every resume even if every counter is zero — operators rely
/// on a baseline "transcript clean" signal as much as the interesting drops.
/// `Display` impl is terse; structured fields should be preferred for
/// dashboards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSanitizeReport {
    /// Number of messages before any filter ran.
    pub input_len: usize,
    /// Number of messages after all filters.
    pub output_len: usize,
    /// Tool-call assistant messages whose tool_call_ids all had no matching
    /// result and were not pinned by retry state.
    pub unresolved_tool_uses_dropped: usize,
    /// Thinking-only assistant messages that were neither the tail nor
    /// followed by a concrete reply.
    pub orphan_thinking_dropped: usize,
    /// Whitespace-only assistant messages with no tool calls or reasoning.
    pub whitespace_only_dropped: usize,
    /// Count of [`ReplacementStateRef`] entries recovered. Not yet wired
    /// into a real cache; see `content_replacements` for the raw refs and
    /// the `TODO(M8.4)` note in [`ResumePolicy::sanitize`].
    pub content_replacements_restored: usize,
    /// `true` when `workspace_root` was provided and the directory no
    /// longer exists on disk.
    pub worktree_missing: bool,
    /// Non-fatal diagnostics the caller may log. Order-preserving.
    pub warnings: Vec<String>,
}

impl std::fmt::Display for SessionSanitizeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SessionSanitizeReport {{ input_len={}, output_len={}, dropped: {{ unresolved_tool={}, orphan_thinking={}, whitespace_only={} }}, content_replacements_restored={}, worktree_missing={}, warnings={} }}",
            self.input_len,
            self.output_len,
            self.unresolved_tool_uses_dropped,
            self.orphan_thinking_dropped,
            self.whitespace_only_dropped,
            self.content_replacements_restored,
            self.worktree_missing,
            self.warnings.len(),
        )
    }
}

/// Outcome of [`ResumePolicy::sanitize`]. The caller must pattern-match on
/// `Ok(SanitizeOutcome)` vs `Err(SanitizeError)` — an error signals the
/// caller should refuse resume (e.g. worktree gone) while a clean outcome
/// is always safe to hand off to the session actor.
#[derive(Debug, Clone)]
pub struct SanitizeOutcome {
    /// Sanitized messages, order-preserving.
    pub messages: Vec<Message>,
    /// Structured report for observability / harness event emission.
    pub report: SessionSanitizeReport,
    /// Content-replacement refs recovered from tool results. Empty when
    /// `messages` contains no tool results with file paths.
    pub content_replacements: Vec<ReplacementStateRef>,
}

/// Reasons [`ResumePolicy::sanitize`] refuses to return a clean outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizeError {
    /// The configured `workspace_root` no longer exists on disk.
    WorktreeMissing {
        path: PathBuf,
        report: SessionSanitizeReport,
    },
}

impl std::fmt::Display for SanitizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorktreeMissing { path, .. } => {
                write!(f, "worktree gone: {}", path.display())
            }
        }
    }
}

impl std::error::Error for SanitizeError {}

/// Abstract view of in-flight retry state — a set of tool_call_ids that
/// must not be dropped even if they lack a matching tool result. Concrete
/// retry-state types in `octos-agent` (e.g. a future `LoopRetryState` with
/// pending id tracking) implement this to bridge into the policy without
/// introducing a reverse crate dependency.
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
        let (messages, dropped_thinking) = filter_orphaned_thinking_only_messages(messages);
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
/// Such messages are dropped EXCEPT when they are the last message in the
/// transcript — that case represents an in-flight reasoning turn the harness
/// will continue on resume.
pub fn filter_orphaned_thinking_only_messages(messages: Vec<Message>) -> (Vec<Message>, usize) {
    let total = messages.len();
    let mut dropped = 0_usize;
    let mut kept = Vec::with_capacity(total);

    for (idx, msg) in messages.into_iter().enumerate() {
        let is_tail = idx + 1 == total;
        if !is_tail && is_thinking_only(&msg) {
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
    use chrono::{Duration, TimeZone, Utc};
    use octos_core::{Message, MessageRole, ToolCall};
    use tempfile::TempDir;

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
        }
    }

    fn assistant_text(content: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
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
        }
    }

    fn assistant_thinking_only(reasoning: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some(reasoning.into()),
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 4).unwrap(),
        }
    }

    fn assistant_whitespace_only() -> Message {
        Message {
            role: MessageRole::Assistant,
            content: "   \n\t ".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 5).unwrap(),
        }
    }

    #[test]
    fn should_drop_tool_result_without_matching_tool_call() {
        let messages = vec![
            user("hello"),
            assistant_text("hi"),
            // Orphan: tool result with no matching tool_call in any
            // assistant message.
            tool_result("orphan-42", r#"{"output": "oops"}"#),
            assistant_text("done"),
        ];

        let (filtered, dropped) = filter_unresolved_tool_uses(messages, None);

        assert_eq!(dropped, 1, "orphan tool_result should bump dropped");
        assert_eq!(filtered.len(), 3);
        assert!(
            !filtered.iter().any(|m| matches!(m.role, MessageRole::Tool)),
            "the orphan tool_result should be gone"
        );
    }

    #[test]
    fn should_drop_tool_call_without_matching_result() {
        let messages = vec![
            user("run it"),
            // Unresolved tool_call — no tool result follows; no text body
            // so the whole assistant message should be dropped.
            assistant_with_calls("", &["call-1"]),
        ];

        let (filtered, dropped) = filter_unresolved_tool_uses(messages, None);

        assert_eq!(dropped, 1);
        assert_eq!(filtered.len(), 1);
        assert!(matches!(filtered[0].role, MessageRole::User));
    }

    #[test]
    fn should_strip_tool_calls_but_keep_text_when_text_present() {
        let messages = vec![
            user("hi"),
            // Unresolved tool_call, but assistant also wrote prose — keep
            // the prose, strip the tool_calls so the provider accepts it.
            assistant_with_calls("I started doing the thing.", &["call-x"]),
        ];

        let (filtered, dropped) = filter_unresolved_tool_uses(messages, None);

        assert_eq!(dropped, 1, "counts the strip as a drop");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[1].content, "I started doing the thing.");
        assert!(filtered[1].tool_calls.is_none());
    }

    #[test]
    fn should_preserve_tool_call_referenced_by_retry_state() {
        let messages = vec![
            user("run it"),
            // Unresolved tool_call, but retry state says "pending — do
            // not drop".
            assistant_with_calls("", &["pending-1"]),
        ];

        let mut retry: HashSet<String> = HashSet::new();
        retry.insert("pending-1".into());

        let (filtered, dropped) =
            filter_unresolved_tool_uses(messages, Some(&retry as &dyn RetryStateView));

        assert_eq!(dropped, 0);
        assert_eq!(filtered.len(), 2);
        assert!(
            filtered[1]
                .tool_calls
                .as_ref()
                .map(|c| c.len() == 1 && c[0].id == "pending-1")
                .unwrap_or(false)
        );
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
    fn mixed_assistant_message_keeps_resolved_tool_calls_when_one_sibling_is_unresolved() {
        // Assistant emitted three tool calls. Only call_a has a matching
        // tool result; call_b and call_c are orphans. The partial-
        // resolution policy must keep call_a (resolved), drop call_b /
        // call_c (orphans), preserve the assistant's prose, and report
        // exactly one drop.
        let messages = vec![
            user("do three things"),
            assistant_with_mixed_calls("plan: a, b, c", &["call_a", "call_b", "call_c"]),
            tool_result("call_a", r#"{"output": "a-done"}"#),
        ];

        let (filtered, dropped) = filter_unresolved_tool_uses(messages, None);

        assert_eq!(dropped, 1, "one assistant message had unresolved siblings");
        // user + assistant (with filtered call set) + tool result
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[1].content, "plan: a, b, c", "prose must survive");
        let kept_calls = filtered[1]
            .tool_calls
            .as_ref()
            .expect("resolved sibling must keep tool_calls populated");
        assert_eq!(kept_calls.len(), 1);
        assert_eq!(kept_calls[0].id, "call_a");
    }

    #[test]
    fn mixed_assistant_message_preserves_matching_tool_result_after_sanitize() {
        // Same setup as above, but we now assert that the matching tool
        // result for `call_a` survives. Under the all-or-nothing legacy
        // branch the assistant had its tool_calls stripped, then
        // `result_has_matching_call` saw zero matches and the orphan-
        // tool-result pass dropped the result. The fix preserves the
        // pairing.
        let messages = vec![
            user("do two things"),
            assistant_with_mixed_calls("a then b", &["call_a", "call_b"]),
            tool_result("call_a", r#"{"output": "a-done"}"#),
        ];

        let (filtered, _dropped) = filter_unresolved_tool_uses(messages, None);

        let tool_results: Vec<&Message> = filtered
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(
            tool_results.len(),
            1,
            "matching tool result for call_a must survive partial-resolution sanitize"
        );
        assert_eq!(tool_results[0].tool_call_id.as_deref(), Some("call_a"));
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
            "resolved call_a must survive: kept_ids={:?}",
            kept_ids
        );
        assert!(
            kept_ids.contains(&"call_b"),
            "retry-pinned call_b must survive: kept_ids={:?}",
            kept_ids
        );
        assert!(
            !kept_ids.contains(&"call_c"),
            "unresolved call_c must be removed: kept_ids={:?}",
            kept_ids
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

    #[test]
    fn should_drop_orphan_thinking_only_message() {
        let messages = vec![
            user("huh"),
            assistant_thinking_only("<think> ... </think>"),
            // A real reply follows, so the thinking-only one is an orphan.
            assistant_text("here is the answer"),
        ];

        let (filtered, dropped) = filter_orphaned_thinking_only_messages(messages);

        assert_eq!(dropped, 1);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[1].content, "here is the answer");
    }

    #[test]
    fn should_keep_trailing_thinking_only_message() {
        // In-flight reasoning: session crashed mid-think. The tail
        // thinking-only message represents state the harness will
        // continue — keep it.
        let messages = vec![
            user("long one"),
            assistant_thinking_only("still working on it ..."),
        ];

        let (filtered, dropped) = filter_orphaned_thinking_only_messages(messages);

        assert_eq!(dropped, 0);
        assert_eq!(filtered.len(), 2);
        assert!(filtered[1].reasoning_content.is_some());
    }

    #[test]
    fn should_drop_whitespace_only_assistant_message() {
        let messages = vec![
            user("hi"),
            assistant_whitespace_only(),
            assistant_text("oh hey"),
        ];

        let (filtered, dropped) = filter_whitespace_only_assistant_messages(messages);

        assert_eq!(dropped, 1);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[1].content, "oh hey");
    }

    #[test]
    fn should_not_drop_user_whitespace_only() {
        // Only assistant messages are subject to the whitespace filter —
        // users can send whatever they want, we preserve.
        let messages = vec![user("   "), assistant_text("weird")];

        let (filtered, dropped) = filter_whitespace_only_assistant_messages(messages);

        assert_eq!(dropped, 0);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn should_collect_content_replacement_refs_from_tool_results() {
        let messages = vec![
            user("read the file"),
            assistant_with_calls("", &["call-a"]),
            tool_result(
                "call-a",
                r#"{"path": "src/lib.rs", "contents": "fn main() {}"}"#,
            ),
            assistant_with_calls("", &["call-b"]),
            tool_result("call-b", "wrote file\npath: docs/new.md\nbytes: 42\n"),
        ];

        let refs = reconstruct_content_replacement_state(&messages);

        assert_eq!(refs.len(), 2, "two unique file paths recovered");
        let paths: Vec<_> = refs.iter().map(|r| r.path.display().to_string()).collect();
        assert!(paths.iter().any(|p| p == "src/lib.rs"));
        assert!(paths.iter().any(|p| p == "docs/new.md"));
        // content_hash is None — M8.4 stub.
        assert!(refs.iter().all(|r| r.content_hash.is_none()));
    }

    #[test]
    fn should_detect_missing_worktree() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("not-a-real-worktree");

        let outcome = ResumePolicy::sanitize(vec![], None, Some(&missing));

        match outcome {
            Err(SanitizeError::WorktreeMissing { path, report }) => {
                assert_eq!(path, missing);
                assert!(report.worktree_missing);
            }
            other => panic!("expected WorktreeMissing, got {other:?}"),
        }
    }

    #[test]
    fn should_bump_worktree_mtime_when_present() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let outcome =
            ResumePolicy::sanitize(vec![user("hi")], None, Some(root)).expect("worktree present");

        assert!(!outcome.report.worktree_missing);
        let marker = root.join(RESUME_MTIME_MARKER);
        assert!(marker.exists(), "marker file should exist after mtime bump");
        let written = std::fs::read_to_string(&marker).unwrap();
        assert!(
            !written.trim().is_empty(),
            "marker should contain a timestamp"
        );
    }

    #[test]
    fn should_report_zero_drops_for_clean_transcript() {
        let messages = vec![
            user("hello"),
            assistant_with_calls("", &["call-1"]),
            tool_result("call-1", r#"{"output": "ok"}"#),
            assistant_text("all done"),
        ];

        let outcome = ResumePolicy::sanitize(messages, None, None).unwrap();

        assert_eq!(outcome.report.unresolved_tool_uses_dropped, 0);
        assert_eq!(outcome.report.orphan_thinking_dropped, 0);
        assert_eq!(outcome.report.whitespace_only_dropped, 0);
        assert_eq!(outcome.report.input_len, 4);
        assert_eq!(outcome.report.output_len, 4);
        assert!(!outcome.report.worktree_missing);
        assert!(outcome.report.warnings.is_empty());
    }

    #[test]
    fn should_display_report_human_readable() {
        let report = SessionSanitizeReport {
            input_len: 10,
            output_len: 6,
            unresolved_tool_uses_dropped: 2,
            orphan_thinking_dropped: 1,
            whitespace_only_dropped: 1,
            content_replacements_restored: 3,
            worktree_missing: false,
            warnings: vec!["one".into()],
        };
        let shown = report.to_string();
        assert!(shown.contains("input_len=10"));
        assert!(shown.contains("output_len=6"));
        assert!(shown.contains("unresolved_tool=2"));
        assert!(shown.contains("orphan_thinking=1"));
        assert!(shown.contains("whitespace_only=1"));
        assert!(shown.contains("worktree_missing=false"));
    }

    #[test]
    fn should_sanitize_complex_transcript_end_to_end() {
        let now = Utc::now();
        let msgs = vec![
            // User prompt.
            Message {
                timestamp: now,
                ..user("task: refactor the widget")
            },
            // Assistant thinking + real reply.
            Message {
                timestamp: now + Duration::milliseconds(10),
                ..assistant_thinking_only("let me think ...")
            },
            Message {
                timestamp: now + Duration::milliseconds(20),
                ..assistant_text("on it")
            },
            // Assistant tool_call that is resolved.
            Message {
                timestamp: now + Duration::milliseconds(30),
                ..assistant_with_calls("", &["call-1"])
            },
            Message {
                timestamp: now + Duration::milliseconds(40),
                ..tool_result("call-1", r#"{"path": "widget.rs"}"#)
            },
            // Assistant tool_call UNRESOLVED (should be dropped).
            Message {
                timestamp: now + Duration::milliseconds(50),
                ..assistant_with_calls("", &["call-ghost"])
            },
            // Whitespace-only assistant (should be dropped).
            Message {
                timestamp: now + Duration::milliseconds(60),
                ..assistant_whitespace_only()
            },
            // Orphan thinking-only (should be dropped; not tail).
            Message {
                timestamp: now + Duration::milliseconds(70),
                ..assistant_thinking_only("hmm")
            },
            // Final real reply (tail — preserved).
            Message {
                timestamp: now + Duration::milliseconds(80),
                ..assistant_text("done refactoring")
            },
        ];

        let outcome = ResumePolicy::sanitize(msgs, None, None).unwrap();

        // unresolved=1, whitespace=1, orphan_thinking=2 (both thinking-only
        // messages are non-tail once filtering starts — one appears mid-
        // conversation, one appears just before the final reply).
        assert_eq!(outcome.report.unresolved_tool_uses_dropped, 1);
        assert_eq!(outcome.report.orphan_thinking_dropped, 2);
        assert_eq!(outcome.report.whitespace_only_dropped, 1);
        // One content-replacement ref from the resolved tool result.
        assert_eq!(outcome.report.content_replacements_restored, 1);
        // input was 9, output should be 9 - 4 = 5.
        assert_eq!(outcome.report.input_len, 9);
        assert_eq!(outcome.report.output_len, 5);
        // The final real reply must be the tail.
        assert_eq!(outcome.messages.last().unwrap().content, "done refactoring");
    }

    #[test]
    fn should_flag_non_directory_worktree_as_missing() {
        // Claude Code's worktree recovery treats a regular file at the
        // configured path as "gone" because it can't be used as a
        // worktree. We match that.
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("not-a-dir");
        std::fs::write(&file_path, "hi").unwrap();

        let outcome = ResumePolicy::sanitize(vec![user("hi")], None, Some(&file_path));

        match outcome {
            Err(SanitizeError::WorktreeMissing { path, .. }) => {
                assert_eq!(path, file_path);
            }
            other => panic!("expected WorktreeMissing, got {other:?}"),
        }
    }

    #[test]
    fn should_detect_whitespace_only_with_reasoning_not_dropped() {
        // An assistant message with empty content but reasoning content
        // is a thinking-only message, not whitespace-only. The
        // whitespace filter must skip it.
        let messages = vec![
            user("hi"),
            assistant_thinking_only("thinking ..."),
            assistant_text("hi back"),
        ];

        let (filtered, dropped) = filter_whitespace_only_assistant_messages(messages);

        assert_eq!(dropped, 0);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn should_ignore_retry_state_for_unknown_ids() {
        // Retry state that references call IDs not actually in the
        // transcript should be a no-op.
        let messages = vec![user("hi"), assistant_text("hello")];

        let mut retry: HashSet<String> = HashSet::new();
        retry.insert("bogus".into());

        let (filtered, dropped) =
            filter_unresolved_tool_uses(messages, Some(&retry as &dyn RetryStateView));

        assert_eq!(dropped, 0);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn should_handle_empty_transcript() {
        let outcome = ResumePolicy::sanitize(vec![], None, None).unwrap();

        assert_eq!(outcome.messages.len(), 0);
        assert_eq!(outcome.report.input_len, 0);
        assert_eq!(outcome.report.output_len, 0);
    }
}
