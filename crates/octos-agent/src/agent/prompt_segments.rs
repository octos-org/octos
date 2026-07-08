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
        let mut out = String::new();
        for seg in &self.segments {
            if seg.content.is_empty() {
                continue;
            }
            match seg.name {
                // Anonymous content carries its own separators verbatim.
                None => out.push_str(&seg.content),
                // Named content gets a separator unless it opens the prompt.
                Some(_) => {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(&seg.content);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing property: a prompt built only with base + appends
    /// renders byte-identically to the legacy single-String implementation.
    #[test]
    fn should_render_identically_to_legacy_string_when_only_appends() {
        let mut legacy = String::from("base prompt");
        let mut segs = PromptSegments::from_base("base prompt".to_string());
        for extra in ["bootstrap", "memory block", "skill fragment"] {
            legacy.push_str("\n\n");
            legacy.push_str(extra);
            segs.append(extra);
        }
        assert_eq!(segs.render(), legacy);
    }

    #[test]
    fn should_render_identically_when_memory_becomes_named_segment() {
        // Legacy: base + append(bootstrap) + append(memory) + append(skills)
        let legacy = "base\n\nbootstrap\n\nmemory v1\n\nskills";
        let mut segs = PromptSegments::from_base("base".to_string());
        segs.append("bootstrap");
        segs.set_named("memory", "memory v1".to_string());
        segs.append("skills");
        assert_eq!(segs.render(), legacy);
    }

    #[test]
    fn should_replace_in_place_when_named_segment_refreshed() {
        let mut segs = PromptSegments::from_base("base".to_string());
        segs.append("bootstrap");
        segs.set_named("memory", "memory v1".to_string());
        segs.append("skills");

        segs.set_named("memory", "memory v2".to_string());
        assert_eq!(segs.render(), "base\n\nbootstrap\n\nmemory v2\n\nskills");
    }

    #[test]
    fn should_start_new_anonymous_tail_when_appending_after_named() {
        let mut segs = PromptSegments::from_base("base".to_string());
        segs.set_named("memory", "m".to_string());
        segs.append("after");
        // Refresh must not eat the append.
        segs.set_named("memory", "M".to_string());
        assert_eq!(segs.render(), "base\n\nM\n\nafter");
    }

    #[test]
    fn should_hide_named_segment_when_content_empty() {
        let mut segs = PromptSegments::from_base("base".to_string());
        segs.set_named("memory", String::new());
        segs.append("after");
        assert_eq!(segs.render(), "base\n\nafter");
        // ...and it can come back in position later.
        segs.set_named("memory", "mem".to_string());
        assert_eq!(segs.render(), "base\n\nmem\n\nafter");
    }

    #[test]
    fn should_drop_named_segments_when_replace_all() {
        let mut segs = PromptSegments::from_base("base".to_string());
        segs.set_named("memory", "mem".to_string());
        segs.replace_all("fresh full prompt".to_string());
        assert_eq!(segs.render(), "fresh full prompt");
    }

    #[test]
    fn should_keep_legacy_bytes_when_base_is_empty() {
        // Legacy behavior: append onto an empty prompt still emits the
        // leading separator.
        let mut segs = PromptSegments::from_base(String::new());
        segs.append("only content");
        assert_eq!(segs.render(), "\n\nonly content");
    }
}
