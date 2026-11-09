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
