//! Loop detection for the agent tool execution loop.
//!
//! Tracks tool call signatures (name + argument hash) and detects
//! repeating patterns in the last N calls. When a cycle is detected,
//! returns a warning message that should be injected as a system message.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::progress_evidence::{EvidenceDelta, ProgressEvidence};
use crate::workspace_progress::WorkspaceProgress;

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

const PEER_POLLING_HINT: &str = "\n\n[PEER POLLING] Three consecutive reads returned the same peer snapshot. Peer work is asynchronous: this does not prove failure or that a later read cannot change. Do not busy-wait. Reflect on the reported peer state, do independent work or use an available bounded wait, and gather fresh evidence before claiming completion.";

fn is_peer_polling_tool(tool_name: &str) -> bool {
    matches!(tool_name, "peer_gather" | "peer_list")
}

/// #1765: number of consecutive identical tool calls (same name +
/// identical arguments JSON) that trips the doom-loop guard. When the
/// LLM issues the SAME call this many times in a row, the conversation
/// loop aborts the turn with a clear model-and-user-facing message
/// instead of issuing the next LLM call — each further retry would only
/// burn tokens. Mirrors opencode's `DOOM_LOOP_THRESHOLD = 3`
/// (`packages/opencode/src/session/processor.ts:29`).
///
/// Scope: the guard is wired into the CONVERSATION loop
/// (`process_message_inner`) only. Asynchronous peer reads use result-aware
/// reflection instead: identical arguments cannot prove an immutable result.
/// The background task loop
/// deliberately keeps its softer treatment (the `record_result`
/// no-progress hint) because unattended tasks legitimately poll
/// status tools with identical arguments while a background job runs.
/// Verifier-configured agents are likewise exempt — the verifier lane
/// injects a `verdict: Repeating` note at this exact streak length and
/// the planner self-corrects, which is a richer recovery than an abort.
pub const DOOM_LOOP_THRESHOLD: usize = 3;

/// A semantic progress warning: the model is changing the same file over and
/// over even though the exact tool arguments differ, so signature-based loop
/// detection cannot see the churn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChurnSignal {
    pub path: String,
    pub edits: usize,
    /// The second threshold requests a different diagnostic approach. It does
    /// not authorize changing the user's selected model/provider.
    pub escalation: bool,
}

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
    /// Observed revisions (or explicit-tool fallback calls) per logical file path. This catches CSS/edit
    /// spirals whose replacement text changes on every call and therefore
    /// evades exact argument hashing.
    file_mutations: HashMap<String, usize>,
    file_churn_threshold: usize,
    pending_file_churn: Option<FileChurnSignal>,
    /// An unchanged asynchronous peer snapshot requests reflection, not a
    /// fabricated final or an abort before its next read can observe progress.
    pending_peer_polling: Option<String>,
    workspace: Option<WorkspaceProgress>,
    workspace_attempted: bool,
    evidence: Option<ProgressEvidence>,
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
            file_mutations: HashMap::new(),
            file_churn_threshold: std::env::var("OCTOS_FILE_CHURN_THRESHOLD")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(2, 100))
                .unwrap_or(5),
            pending_file_churn: None,
            pending_peer_polling: None,
            workspace: None,
            workspace_attempted: false,
            evidence: None,
        }
    }

    /// Start once, off the async executor. Native notifications then limit
    /// hashing to changed source-size files; no full-tree scan per tool call.
    pub async fn observe_workspace(&mut self, root: &Path) {
        if self.workspace_attempted {
            return;
        }
        self.workspace_attempted = true;
        self.evidence = ProgressEvidence::new(root).ok();
        let root = root.to_path_buf();
        match tokio::task::spawn_blocking(move || WorkspaceProgress::new(&root)).await {
            Ok(Ok(observer)) => self.workspace = Some(observer),
            _ => tracing::warn!(
                "workspace change observation unavailable; explicit-tool fallback only"
            ),
        }
    }

    pub fn refresh_workspace(&mut self) {
        let changes = self
            .workspace
            .as_mut()
            .map(WorkspaceProgress::drain)
            .unwrap_or_default();
        for path in changes {
            let path = path.to_string_lossy().into_owned();
            if let Some(hint) = self.record_changed_path(path) {
                tracing::info!(hint, "workspace file churn observed");
            }
        }
    }

    pub fn evidence_delta(&mut self) -> EvidenceDelta {
        self.evidence
            .as_mut()
            .map(ProgressEvidence::refresh)
            .unwrap_or_default()
    }

    pub fn change_summary(&self) -> String {
        let mut changes: Vec<_> = self.file_mutations.iter().collect();
        changes.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let changes: Vec<_> = changes.into_iter().take(12).map(|(path, revisions)| {
            serde_json::json!({"path": octos_core::truncated_utf8(path, 256, "…"), "observed_revisions": revisions})
        }).collect();
        serde_json::json!({"changes":changes,
            "observation":"workspace changes, not attribution to a particular actor; source-size files only",
            "incomplete": self.workspace.as_ref().is_none_or(WorkspaceProgress::incomplete)}).to_string()
    }

    /// Record a successful file-mutating tool call. Returns a model-facing
    /// hint at the first threshold and again at each threshold multiple. The
    /// second and later firings request root-cause diagnosis, but never
    /// terminate the user turn.
    pub fn record_file_mutation(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        success: bool,
    ) -> Option<String> {
        // With an observer, actual byte changes are the authority, including
        // writes by failed commands. Do not double-count explicit tool hints.
        if self.workspace.is_some() || !success {
            return None;
        }
        let path = mutation_path(tool_name, args)?;
        self.record_changed_path(path)
    }

    fn record_changed_path(&mut self, path: String) -> Option<String> {
        let edits = self.file_mutations.entry(path.clone()).or_default();
        *edits += 1;
        if *edits < self.file_churn_threshold || *edits % self.file_churn_threshold != 0 {
            return None;
        }

        let signal = FileChurnSignal {
            path: path.clone(),
            edits: *edits,
            escalation: *edits >= self.file_churn_threshold.saturating_mul(2),
        };
        let escalation_note = if signal.escalation {
            " This is a repeated threshold breach; inspect the unresolved evidence and choose a different root-cause test before another patch."
        } else {
            " The harness will run a tools-disabled synthesis checkpoint before more edits."
        };
        self.pending_file_churn = Some(signal);
        Some(format!(
            "\n\n[FILE CHURN] `{path}` has been modified {} times in this turn. Stop patching blindly: inspect the rendered/resulting state, identify the root cause, and choose one bounded next change.{escalation_note}",
            *edits
        ))
    }

    pub fn take_file_churn_signal(&mut self) -> Option<FileChurnSignal> {
        self.refresh_workspace();
        self.pending_file_churn.take()
    }

    pub fn take_peer_polling_signal(&mut self) -> Option<String> {
        self.pending_peer_polling.take()
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
        if is_peer_polling_tool(tool_name) {
            // An asynchronous read breaks a consecutive mutating-call streak;
            // its progress cannot be known before the tool actually executes.
            self.doom_last_signature = None;
            self.doom_streak = 0;
            return None;
        }
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
        if is_peer_polling_tool(tool_name) {
            // Recorded AFTER execution with its result hash instead. Keep the
            // history so genuine mixed mutating-tool cycles remain protected.
            return None;
        }
        let sig = Self::signature(tool_name, args);
        self.push_call_signature(sig);

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

    fn push_call_signature(&mut self, signature: u64) {
        self.signatures.push(signature);
        if self.signatures.len() > self.window * 2 {
            let drain_to = self.signatures.len() - self.window;
            self.signatures.drain(..drain_to);
        }
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
        if is_peer_polling_tool(tool_name) {
            self.push_call_signature(sig);
        }
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
            if is_peer_polling_tool(tool_name) {
                self.pending_peer_polling = Some(tool_name.to_owned());
                return Some(PEER_POLLING_HINT.to_owned());
            }
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

fn mutation_path(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if !matches!(
        tool_name,
        "write_file" | "edit_file" | "diff_edit" | "apply_patch"
    ) {
        return None;
    }
    ["path", "file_path", "filename", "file"]
        .into_iter()
        .find_map(|key| args.get(key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detector_with_churn_threshold(threshold: usize) -> LoopDetector {
        let mut detector = LoopDetector::new(10);
        detector.file_churn_threshold = threshold;
        detector
    }

    #[test]
    fn peer_polling_should_reflect_repeated_snapshots_without_preventing_the_next_read() {
        for name in ["peer_gather", "peer_list"] {
            let mut detector = LoopDetector::new(12);
            for index in 1..=6 {
                assert!(detector.record_doom(name, &json!({})).is_none());
                assert!(detector.record(name, &json!({})).is_none());
                let hint = detector.record_result(name, &json!({}), "still running");
                if index % 3 == 0 {
                    let hint = hint.unwrap();
                    assert!(hint.contains("asynchronous"));
                    assert!(!hint.contains("Calling it again will produce the same result"));
                    assert_eq!(detector.take_peer_polling_signal().as_deref(), Some(name));
                } else {
                    assert!(hint.is_none());
                    assert!(detector.take_peer_polling_signal().is_none());
                }
            }
        }
    }

    #[test]
    fn peer_polling_should_preserve_stuck_mutating_cycles_but_honor_changed_peer_results() {
        for changing in [false, true] {
            let mut detector = LoopDetector::new(12);
            let mutation = json!({"path":"a.txt", "content":"unchanged"});
            for index in 0..3 {
                assert!(detector.record_doom("write_file", &mutation).is_none());
                assert!(detector.record("write_file", &mutation).is_none());
                assert!(detector.record_doom("peer_gather", &json!({})).is_none());
                assert!(detector.record("peer_gather", &json!({})).is_none());
                let result = if changing {
                    format!("progress {index}")
                } else {
                    "unchanged".into()
                };
                let _ = detector.record_result("peer_gather", &json!({}), &result);
            }
            assert_eq!(
                detector.record("write_file", &mutation).is_some(),
                !changing,
                "unchanged peer polling must not erase protection for a real repeated mutation cycle"
            );
        }
    }

    #[test]
    fn same_file_churn_detects_varied_edit_arguments() {
        let mut detector = detector_with_churn_threshold(3);
        for index in 0..2 {
            assert!(
                detector
                    .record_file_mutation(
                        "edit_file",
                        &json!({"path": "app/globals.css", "replacement": index}),
                        true,
                    )
                    .is_none()
            );
        }
        let hint = detector
            .record_file_mutation(
                "edit_file",
                &json!({"path": "app/globals.css", "replacement": 2}),
                true,
            )
            .expect("third successful edit should warn");
        assert!(hint.contains("FILE CHURN"));
        assert_eq!(
            detector.take_file_churn_signal(),
            Some(FileChurnSignal {
                path: "app/globals.css".into(),
                edits: 3,
                escalation: false,
            })
        );
    }

    #[tokio::test]
    async fn should_count_shell_revisions_without_double_counting_explicit_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("generator.py"), "initial").unwrap();
        let mut detector = detector_with_churn_threshold(3);
        detector.observe_workspace(dir.path()).await;
        for revision in 1..=3 {
            std::fs::write(
                dir.path().join("generator.py"),
                format!("revision {revision}"),
            )
            .unwrap();
            // Native events can arrive after the process returns. Production
            // drains on tool/model boundaries; the test waits for that event.
            let end = std::time::Instant::now() + std::time::Duration::from_secs(4);
            while detector
                .file_mutations
                .get("generator.py")
                .copied()
                .unwrap_or(0)
                < revision
                && std::time::Instant::now() < end
            {
                detector.refresh_workspace();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
            assert_eq!(detector.file_mutations.get("generator.py"), Some(&revision));
            assert!(
                detector
                    .record_file_mutation("edit_file", &json!({"path":"generator.py"}), true)
                    .is_none()
            );
        }
        let signal = detector
            .take_file_churn_signal()
            .expect("third actual revision triggers diagnosis");
        assert_eq!(signal.edits, 3);
        assert_eq!(signal.path, "generator.py");
    }

    #[test]
    fn second_file_churn_threshold_requests_escalation() {
        let mut detector = detector_with_churn_threshold(2);
        for index in 0..4 {
            let _ = detector.record_file_mutation(
                "write_file",
                &json!({"path": "app.css", "content": index}),
                true,
            );
        }
        let signal = detector
            .take_file_churn_signal()
            .expect("fourth edit should replace pending signal");
        assert_eq!(signal.edits, 4);
        assert!(signal.escalation);
    }

    #[test]
    fn failed_or_read_only_calls_do_not_count_as_file_churn() {
        let mut detector = detector_with_churn_threshold(2);
        assert!(
            detector
                .record_file_mutation("edit_file", &json!({"path": "a.rs"}), false)
                .is_none()
        );
        assert!(
            detector
                .record_file_mutation("read_file", &json!({"path": "a.rs"}), true)
                .is_none()
        );
        assert!(detector.take_file_churn_signal().is_none());
    }

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
