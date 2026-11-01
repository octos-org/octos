//! Dependency-free DTOs shared by session resume and agent runtime code.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Message;

/// Reference to a file path recovered from a tool result during resume.
///
/// Populated by the resume policy's content-replacement reconstruction;
/// consumed by the agent file-state cache to seed paths that the transcript
/// claims were last read or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacementStateRef {
    /// Absolute or workspace-relative path the tool result referenced.
    pub path: PathBuf,
    /// Content hash when available; None indicates placeholder state pending
    /// cache restore.
    pub content_hash: Option<String>,
}

/// Typed report describing what session resume sanitization dropped.
///
/// Emitted on every resume even if every counter is zero — operators rely on a
/// baseline "transcript clean" signal as much as the interesting drops.
/// `Display` is terse; structured fields should be preferred for dashboards.
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
    /// Count of [`ReplacementStateRef`] entries recovered.
    pub content_replacements_restored: usize,
    /// `true` when `workspace_root` was provided and the directory no longer
    /// exists on disk.
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

/// Successful result of session resume sanitization.
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

/// Reasons session resume refuses to return a clean outcome.
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
