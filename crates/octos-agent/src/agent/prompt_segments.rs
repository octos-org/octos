//! Ordered system-prompt segments.
//!
//! The system prompt is a sequence of segments rendered by concatenation.
//! Two kinds exist:
//!
//! - **Anonymous** segments carry the legacy append stream: every
//!   [`PromptSegments::append`] writes `"\n\n" + text` into the current
//!   anonymous tail, so a prompt built purely from `append` calls renders
//!   byte-identically to the old single-`String` implementation.
//! - **Named** segments are replace-in-place slots (e.g. `"memory"`).
//!   They keep their insertion position when their content is refreshed,
//!   which is what lets a long-lived session see an updated memory block
//!   without disturbing the bootstrap-before / skills-after ordering.
//!
//! An `append` issued after a named segment starts a fresh anonymous tail
//! rather than mutating the named slot, so later appends never get wiped
//! by a segment refresh.

/// Provider for a named prompt segment that can refresh between turns
/// (e.g. the memory block re-read when `MEMORY.md` changes on disk).
///
/// [`crate::Agent::refresh_prompt_segments`] runs every registered
/// provider at conversation-turn start.
#[async_trait::async_trait]
pub trait PromptSegmentProvider: Send + Sync {
    /// Name of the segment this provider maintains (e.g. `"memory"`).
    fn segment_name(&self) -> &str;

    /// Return `Some(content)` when the segment changed since the last call
    /// (including the first call); `None` when unchanged. Implementations
    /// must keep the unchanged path cheap — typically a single `stat`.
    async fn refresh(&self) -> Option<String>;

    /// [`Self::refresh`] with the upcoming turn's user text, for providers
    /// that select content by relevance. Default ignores the query.
    async fn refresh_for(&self, query: Option<&str>) -> Option<String> {
        let _ = query;
        self.refresh().await
    }
}

/// One prompt segment: optional name + raw content.
///
/// Anonymous segments store their bytes exactly as rendered (including any
/// leading `"\n\n"` from `append`). Named segments store bare content; the
/// renderer adds the `"\n\n"` separator so replacements can't corrupt
/// spacing.
#[derive(Debug, Clone)]
struct PromptSegment {
    name: Option<String>,
    content: String,
}

/// Ordered segment list backing the agent system prompt.
///
/// Public for visibility reasons only (the `Agent.system_prompt` field
/// names it); construct and mutate exclusively through `Agent` methods.
#[derive(Debug, Clone)]
pub struct PromptSegments {
    segments: Vec<PromptSegment>,
}

impl PromptSegments {
    /// A prompt consisting of a single anonymous base segment.
    pub(super) fn from_base(base: String) -> Self {
        Self {
            segments: vec![PromptSegment {
                name: None,
                content: base,
            }],
        }
    }

    /// Replace the WHOLE prompt with a single anonymous base segment.
    ///
    /// This is the semantic of the legacy `set_system_prompt` /
    /// `with_system_prompt` full-replace: named segments are dropped and
    /// must be re-set by the caller afterwards if still wanted.
    pub(super) fn replace_all(&mut self, prompt: String) {
        self.segments = vec![PromptSegment {
            name: None,
            content: prompt,
        }];
    }

    /// Append to the current anonymous tail (legacy `append_system_prompt`
    /// semantics: `"\n\n"` separator + text). Starts a new anonymous
    /// segment when the tail is named, so named slots stay replaceable.
    pub(super) fn append(&mut self, extra: &str) {
        match self.segments.last_mut() {
            Some(seg) if seg.name.is_none() => {
                seg.content.push_str("\n\n");
                seg.content.push_str(extra);
            }
            _ => self.segments.push(PromptSegment {
                name: None,
                content: format!("\n\n{extra}"),
            }),
        }
    }

    /// Insert (first call) or replace in place (subsequent calls) the named
    /// segment. Content is stored bare; the renderer emits a `"\n\n"`
    /// separator before non-first, non-empty segments. Setting empty
    /// content effectively hides the segment while keeping its position.
    pub(super) fn set_named(&mut self, name: &str, content: String) {
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|seg| seg.name.as_deref() == Some(name))
        {
            seg.content = content;
        } else {
            self.segments.push(PromptSegment {
                name: Some(name.to_string()),
                content,
            });
        }
    }

    /// Render the full prompt.
    pub(super) fn render(&self) -> String {
        self.render_with_named_replacement(None)
    }

    /// Render a snapshot while replacing one named segment without mutating
    /// the live Agent. OUP uses this to keep stable memory policy in System
    /// while projecting the refreshed memory payload as tail context.
    pub(super) fn render_replacing_named(&self, name: &str, replacement: &str) -> String {
        self.render_with_named_replacement(Some((name, replacement)))
    }

    pub(super) fn named_content(&self, name: &str) -> Option<String> {
        self.segments
            .iter()
            .find(|segment| segment.name.as_deref() == Some(name))
            .map(|segment| segment.content.clone())
    }

    fn render_with_named_replacement(&self, replacement: Option<(&str, &str)>) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            let content = replacement
                .filter(|(name, _)| seg.name.as_deref() == Some(*name))
                .map(|(_, content)| content)
                .unwrap_or(&seg.content);
            if content.is_empty() {
                continue;
            }
            match seg.name {
                // Anonymous content carries its own separators verbatim.
                None => out.push_str(content),
                // Named content gets a separator unless it opens the prompt.
                Some(_) => {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(content);
                }
            }
        }
        out
    }
}
