//! Bounded presentation state for canonical OUP assistant segments.
//!
//! A durable commit may overtake queued provider deltas. Once finalized, a
//! segment ignores those late fragments instead of rendering the answer twice.

use std::collections::{HashMap, VecDeque};

const RETAINED_SEGMENTS: usize = 512;

#[derive(Default)]
struct Segment {
    text: String,
    finalized: bool,
}

#[derive(Default)]
pub(crate) struct AssistantTextProjection {
    segments: HashMap<String, Segment>,
    order: VecDeque<String>,
}

impl AssistantTextProjection {
    fn segment(&mut self, id: &str) -> &mut Segment {
        if !self.segments.contains_key(id) {
            if self.order.len() >= RETAINED_SEGMENTS
                && let Some(expired) = self.order.pop_front()
            {
                self.segments.remove(&expired);
            }
            self.order.push_back(id.to_owned());
        }
        self.segments.entry(id.to_owned()).or_default()
    }

    pub(crate) fn delta(&mut self, id: &str, text: &str) -> String {
        let segment = self.segment(id);
        if segment.finalized {
            return String::new();
        }
        segment.text.push_str(text);
        text.to_owned()
    }

    pub(crate) fn persisted(&mut self, id: &str, text: &str) -> String {
        let segment = self.segment(id);
        let tail = text.strip_prefix(&segment.text).unwrap_or(text).to_owned();
        segment.text = text.to_owned();
        segment.finalized = true;
        tail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_commit_before_queued_deltas_is_rendered_once() {
        let mut projection = AssistantTextProjection::default();
        assert_eq!(projection.persisted("one", "answer"), "answer");
        assert_eq!(projection.delta("one", "ans"), "");
        assert_eq!(projection.delta("one", "wer"), "");
        assert_eq!(projection.persisted("one", "answer"), "");
        assert_eq!(projection.delta("two", "next"), "next");
        assert_eq!(projection.persisted("two", "next answer"), " answer");
    }

    #[test]
    fn partial_stream_gets_only_the_canonical_suffix_and_retention_is_bounded() {
        let mut projection = AssistantTextProjection::default();
        assert_eq!(projection.delta("first", "ans"), "ans");
        assert_eq!(projection.persisted("first", "answer"), "wer");
        assert_eq!(projection.delta("first", "wer"), "");
        for i in 0..RETAINED_SEGMENTS * 2 {
            projection.persisted(&i.to_string(), "text");
        }
        assert_eq!(projection.segments.len(), RETAINED_SEGMENTS);
    }
}
