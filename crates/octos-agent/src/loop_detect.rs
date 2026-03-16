//! Loop detection for the agent tool execution loop.
//!
//! Tracks tool call signatures (name + argument hash) and detects
//! repeating patterns in the last N calls. When a cycle is detected,
//! returns a warning message that should be injected as a system message.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Tracks tool call patterns and detects loops.
pub struct LoopDetector {
    /// Ring buffer of recent tool call signatures.
    signatures: Vec<u64>,
    /// Maximum window size to check for patterns.
    window: usize,
}

impl LoopDetector {
    /// Create a new detector with the given window size.
    pub fn new(window: usize) -> Self {
        Self {
            signatures: Vec::with_capacity(window * 2),
            window,
        }
    }

    /// Record a tool call and check for repeating patterns.
    /// Returns a warning message if a loop is detected.
    pub fn record(&mut self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let sig = Self::signature(tool_name, args);
        self.signatures.push(sig);

        // Trim to bounded size (actual ring buffer behavior)
        if self.signatures.len() > self.window * 2 {
            let drain_to = self.signatures.len() - self.window;
            self.signatures.drain(..drain_to);
        }

        // Only check once we have enough history
        if self.signatures.len() < 4 {
            return None;
        }

        let len = self.signatures.len();
        let check_len = len.min(self.window);
        let window = &self.signatures[len - check_len..];

        // Check for cycles of length 1, 2, and 3
        for cycle_len in 1..=3 {
            if check_len >= cycle_len * 3 && Self::is_repeating(window, cycle_len) {
                return Some(format!(
                    "[LOOP DETECTED] The last {} tool calls follow a repeating pattern \
                     (cycle length {cycle_len}). Try a different approach or break the cycle.",
                    check_len
                ));
            }
        }

        None
    }

    /// Compute a signature hash for a tool call.
    fn signature(name: &str, args: &serde_json::Value) -> u64 {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        // Hash the JSON string representation for stability
        let args_str = args.to_string();
        args_str.hash(&mut hasher);
        hasher.finish()
    }

    /// Check if the window contains a repeating pattern of the given cycle length.
    /// Requires at least 3 full repetitions of the cycle.
    fn is_repeating(window: &[u64], cycle_len: usize) -> bool {
        if window.len() < cycle_len * 3 {
            return false;
        }
        let tail = &window[window.len() - cycle_len * 3..];
        let pattern = &tail[..cycle_len];
        tail[cycle_len..cycle_len * 2] == *pattern && tail[cycle_len * 2..] == *pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_not_detect_on_few_calls() {
        let mut d = LoopDetector::new(10);
        assert!(d.record("shell", &json!({"command": "ls"})).is_none());
        assert!(d.record("shell", &json!({"command": "ls"})).is_none());
        assert!(d.record("shell", &json!({"command": "ls"})).is_none());
    }

    #[test]
    fn should_detect_single_call_loop() {
        let mut d = LoopDetector::new(10);
        let args = json!({"command": "cat foo.txt"});
        // Need 4 identical calls for 3 repetitions of cycle-1 pattern
        for _ in 0..3 {
            assert!(d.record("read_file", &args).is_none());
        }
        let warning = d.record("read_file", &args);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("LOOP DETECTED"));
    }

    #[test]
    fn should_detect_two_call_cycle() {
        let mut d = LoopDetector::new(10);
        let a = json!({"path": "a.rs"});
        let b = json!({"path": "b.rs"});
        // a, b, a, b, a, b = 3 repetitions of (a,b) cycle
        for _ in 0..2 {
            assert!(d.record("read_file", &a).is_none());
            assert!(d.record("read_file", &b).is_none());
        }
        assert!(d.record("read_file", &a).is_none());
        let warning = d.record("read_file", &b);
        assert!(warning.is_some());
    }

    #[test]
    fn should_not_detect_varied_calls() {
        let mut d = LoopDetector::new(10);
        for i in 0..20 {
            let args = json!({"command": format!("cmd_{}", i)});
            assert!(d.record("shell", &args).is_none());
        }
    }

    #[test]
    fn should_detect_three_call_cycle() {
        let mut d = LoopDetector::new(15);
        let a = json!({"x": 1});
        let b = json!({"x": 2});
        let c = json!({"x": 3});
        // a,b,c repeated 3 times = 9 calls
        for _ in 0..2 {
            assert!(d.record("t", &a).is_none());
            assert!(d.record("t", &b).is_none());
            assert!(d.record("t", &c).is_none());
        }
        assert!(d.record("t", &a).is_none());
        assert!(d.record("t", &b).is_none());
        let warning = d.record("t", &c);
        assert!(warning.is_some());
    }
}
