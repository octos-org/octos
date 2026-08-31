//! Loop detection for the agent tool execution loop.
//!
//! Tracks tool call signatures (name + argument hash) and detects
//! repeating patterns in the last N calls. When a cycle is detected,
//! returns a warning message that should be injected as a system message.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Soft "no-progress" hint appended to a tool result when the same
/// (name, args, result) triple has been seen 3 times in a row. The
/// LLM sees this on its next iteration as part of the most recent
/// tool result — it does not terminate the turn. Hard cycle detection
/// (existing logic) catches anything that survives this nudge.
///
/// This is the OpenClaw lesson — distinguish "no progress" (same args
/// AND same result) from legitimate polling (same args, different
/// result over time). The production loop on mini3 session 8w2ime had
/// kimi-k2.5 calling `check_workspace_contract` 5 times with the same
/// args, all returning identical 4 KB trees. With result hashing, that
/// fires at iter 3 — early enough to nudge before the hard cycle
/// detector terminates the turn at iter 4. Legitimate polls like
/// `check_background_tasks` (which return different statuses while a
/// background job runs) are unaffected.
pub const NO_PROGRESS_HINT: &str = "\n\n[NO PROGRESS] You have now called this tool 3 times in a row with identical arguments AND received identical results. Calling it again will produce the same result. To make progress, either switch to a different tool (read_file / list_dir / view_image for file content) or finish the turn with the information you already have.";

/// #1765: number of consecutive identical tool calls (same name +
/// identical arguments JSON) that trips the doom-loop guard. When the
/// LLM issues the SAME call this many times in a row, the conversation
/// loop aborts the turn with a clear model-and-user-facing message
/// instead of issuing the next LLM call — each further retry would only
/// burn tokens. Mirrors opencode's `DOOM_LOOP_THRESHOLD = 3`
/// (`packages/opencode/src/session/processor.ts:29`).
///
/// Scope: the guard is wired into the CONVERSATION loop
/// (`process_message_inner`) only. The background task loop
/// deliberately keeps its softer treatment (the `record_result`
/// no-progress hint) because unattended tasks legitimately poll
/// status tools with identical arguments while a background job runs.
/// Verifier-configured agents are likewise exempt — the verifier lane
/// injects a `verdict: Repeating` note at this exact streak length and
/// the planner self-corrects, which is a richer recovery than an abort.
pub const DOOM_LOOP_THRESHOLD: usize = 3;

/// Tracks tool call patterns and detects loops.
pub struct LoopDetector {
    /// Ring buffer of recent tool call signatures (name + args).
    /// Used by `record()` for hard cycle detection.
    signatures: Vec<u64>,
    /// Ring buffer of recent (name + args + result) signatures.
    /// Used by `record_result()` for the OpenClaw-style "no progress"
    /// soft hint, which only fires when both args AND result repeat —
    /// distinguishing stuck loops from legitimate polling.
    result_signatures: Vec<u64>,
    /// Maximum window size to check for patterns.
    window: usize,
    /// #1765: signature of the most recent call recorded by
    /// [`Self::record_doom`], used to detect consecutive identical calls.
    doom_last_signature: Option<u64>,
    /// #1765: length of the current consecutive-identical-call streak.
    doom_streak: usize,
}

impl LoopDetector {
    /// Create a new detector with the given window size.
    pub fn new(window: usize) -> Self {
        Self {
            signatures: Vec::with_capacity(window * 2),
            result_signatures: Vec::with_capacity(window * 2),
            window,
            doom_last_signature: None,
            doom_streak: 0,
        }
    }

    /// #1765: record a tool call for the doom-loop guard and return the
    /// streak length when it reaches [`DOOM_LOOP_THRESHOLD`].
    ///
    /// A call is "identical" when both the tool name and the exact
    /// arguments JSON match the previous call; any non-identical call
    /// resets the streak to 1. Returns `Some(streak)` for every call at
    /// or past the threshold (not just the first) so a caller that
    /// defers the abort once — e.g. because the shell-spiral recovery
    /// path owns the streak — still gets a signal on the next repeat.
    pub fn record_doom(&mut self, tool_name: &str, args: &serde_json::Value) -> Option<usize> {
        let sig = Self::signature(tool_name, args);
        if self.doom_last_signature == Some(sig) {
            self.doom_streak += 1;
        } else {
            self.doom_last_signature = Some(sig);
            self.doom_streak = 1;
        }
        (self.doom_streak >= DOOM_LOOP_THRESHOLD).then_some(self.doom_streak)
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
                    "[LOOP DETECTED] The last {check_len} tool calls follow a repeating pattern \
                     (cycle length {cycle_len}). Try a different approach or break the cycle."
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

    /// Compute a signature hash for a tool call AND its result.
    fn signature_with_result(name: &str, args: &serde_json::Value, result: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        let args_str = args.to_string();
        args_str.hash(&mut hasher);
        result.hash(&mut hasher);
        hasher.finish()
    }

    /// Record a tool call's RESULT after the tool has executed.
    ///
    /// Returns `Some(NO_PROGRESS_HINT)` when the last 3 records for
    /// this `(name, args, result)` triple are identical — meaning the
    /// LLM called the same tool with the same args 3 times in a row
    /// AND got the same result each time. This is the "no progress"
    /// signal that distinguishes stuck loops from legitimate polling.
    ///
    /// Fires at most once per "burst" — after firing, the result
    /// signature ring is cleared so a 4th identical call does not
    /// re-fire (the existing `record()` cycle detector picks up
    /// anything that survives the soft nudge).
    ///
    /// Callers should append the returned hint to the tool result
    /// message's content so the LLM sees it as part of its next-turn
    /// context. Does NOT terminate the turn — that's the hard
    /// detector's job.
    pub fn record_result(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> Option<String> {
        let sig = Self::signature_with_result(tool_name, args, result);
        self.result_signatures.push(sig);

        // Bound the ring buffer
        if self.result_signatures.len() > self.window * 2 {
            let drain_to = self.result_signatures.len() - self.window;
            self.result_signatures.drain(..drain_to);
        }

        let len = self.result_signatures.len();
        if len < 3 {
            return None;
        }
        let last3 = &self.result_signatures[len - 3..];
        if last3[0] == last3[1] && last3[1] == last3[2] {
            // Fire once — clear so a 4th identical call won't re-fire.
            self.result_signatures.clear();
            return Some(NO_PROGRESS_HINT.to_string());
        }
        None
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

    // ----- record_result tests (OpenClaw-style no-progress detection) -----

    #[test]
    fn record_result_quiet_for_first_two_calls() {
        let mut d = LoopDetector::new(10);
        let args = json!({"project": "slides/demo"});
        let same_result = "{\"contracts\":[{\"ready\":true}]}";
        assert!(d.record_result("check", &args, same_result).is_none());
        assert!(d.record_result("check", &args, same_result).is_none());
    }

    #[test]
    fn record_result_fires_after_three_identical_triples() {
        let mut d = LoopDetector::new(10);
        let args = json!({"project": "slides/demo"});
        let result = "{\"contracts\":[{\"ready\":true}]}";
        assert!(d.record_result("check", &args, result).is_none());
        assert!(d.record_result("check", &args, result).is_none());
        let hint = d.record_result("check", &args, result);
        assert!(
            hint.is_some(),
            "expected NO PROGRESS hint after 3 identical"
        );
        assert!(hint.unwrap().contains("NO PROGRESS"));
    }

    #[test]
    fn record_result_silent_when_result_changes_legitimate_poll() {
        let mut d = LoopDetector::new(10);
        let args = json!({});
        // Same tool + args, but result evolves (polling case)
        assert!(d.record_result("poll", &args, "running").is_none());
        assert!(d.record_result("poll", &args, "running").is_none());
        assert!(d.record_result("poll", &args, "completed").is_none());
        // Even after switching back, two same + one different is not a streak of 3
        assert!(d.record_result("poll", &args, "running").is_none());
    }

    #[test]
    fn record_result_silent_when_args_change() {
        let mut d = LoopDetector::new(10);
        let result = "ok";
        assert!(
            d.record_result("read_file", &json!({"path": "a"}), result)
                .is_none()
        );
        assert!(
            d.record_result("read_file", &json!({"path": "b"}), result)
                .is_none()
        );
        assert!(
            d.record_result("read_file", &json!({"path": "c"}), result)
                .is_none()
        );
    }

    #[test]
    fn record_result_fires_once_per_burst() {
        let mut d = LoopDetector::new(10);
        let args = json!({"x": 1});
        let result = "same";
        d.record_result("t", &args, result);
        d.record_result("t", &args, result);
        let first = d.record_result("t", &args, result);
        assert!(first.is_some());
        // 4th identical call should NOT re-fire — buffer was cleared.
        // The hard cycle detector picks up anything that survives.
        let second = d.record_result("t", &args, result);
        assert!(second.is_none());
    }

    // ----- record_doom tests (#1765 doom-loop guard) -----

    #[test]
    fn should_fire_doom_when_third_identical_call_arrives() {
        let mut d = LoopDetector::new(10);
        let args = json!({"path": "a.txt"});
        assert!(d.record_doom("read_file", &args).is_none());
        assert!(d.record_doom("read_file", &args).is_none());
        assert_eq!(d.record_doom("read_file", &args), Some(3));
    }

    #[test]
    fn should_stay_quiet_when_only_two_identical_calls() {
        let mut d = LoopDetector::new(10);
        let args = json!({"cmd": "ls"});
        assert!(d.record_doom("shell", &args).is_none());
        assert!(d.record_doom("shell", &args).is_none());
    }

    #[test]
    fn should_reset_doom_streak_when_arguments_differ() {
        let mut d = LoopDetector::new(10);
        assert!(d.record_doom("read_file", &json!({"path": "a"})).is_none());
        assert!(d.record_doom("read_file", &json!({"path": "a"})).is_none());
        // Different args → streak resets; the 3rd call is NOT doom.
        assert!(d.record_doom("read_file", &json!({"path": "b"})).is_none());
        // Two more identical "b" calls: streak is 3 only now.
        assert!(d.record_doom("read_file", &json!({"path": "b"})).is_none());
        assert_eq!(d.record_doom("read_file", &json!({"path": "b"})), Some(3));
    }

    #[test]
    fn should_reset_doom_streak_when_tool_name_differs() {
        let mut d = LoopDetector::new(10);
        let args = json!({"path": "a"});
        assert!(d.record_doom("read_file", &args).is_none());
        assert!(d.record_doom("read_file", &args).is_none());
        // Same args, different tool → reset.
        assert!(d.record_doom("list_dir", &args).is_none());
        assert!(d.record_doom("list_dir", &args).is_none());
        assert_eq!(d.record_doom("list_dir", &args), Some(3));
    }

    #[test]
    fn should_keep_firing_doom_past_threshold() {
        // ">= threshold" semantics: if the caller chooses to continue
        // (e.g. the shell-spiral recovery path defers the abort), a 4th
        // identical call must fire again rather than go quiet.
        let mut d = LoopDetector::new(10);
        let args = json!({});
        d.record_doom("t", &args);
        d.record_doom("t", &args);
        assert_eq!(d.record_doom("t", &args), Some(3));
        assert_eq!(d.record_doom("t", &args), Some(4));
    }

    #[test]
    fn should_not_fire_doom_for_alternating_calls() {
        let mut d = LoopDetector::new(10);
        for _ in 0..10 {
            assert!(d.record_doom("read_file", &json!({"path": "a"})).is_none());
            assert!(d.record_doom("read_file", &json!({"path": "b"})).is_none());
        }
    }
}
