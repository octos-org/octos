//! Loop detection for the agent tool execution loop.
//!
//! Default feedback compares executed tool calls and their text results.
//! Repetition hints accompany tool results; they do not terminate a turn or
//! prove a lack of semantic progress. Legacy pre-execution guards remain
//! available to callers that explicitly opt into them.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Soft repetition hint appended after three identical executed
/// (name, args, text result) triples. It does not claim semantic failure,
/// terminate the turn, or add a separate LLM request. Default execution
/// relies on configured budgets and explicit tool contracts for hard limits.
pub const NO_PROGRESS_HINT: &str = "\n\n[NO PROGRESS] The last 3 tool calls had identical names, arguments and returned text. This is a repetition signal, not proof of failure or that future results cannot change. If waiting for asynchronous work, use an available bounded wait or do independent work before checking again. If changing state, verify the actual effect before repeating; otherwise choose an evidence-based next action. Claim completion only from actual task evidence.";

const PEER_POLLING_HINT: &str = "\n\n[PEER POLLING] Three consecutive reads returned the same peer snapshot. Peer work is asynchronous: this does not prove failure or that a later read cannot change. Do not busy-wait. Reflect on the reported peer state, do independent work or use an available bounded wait, and gather fresh evidence before claiming completion.";

/// UPCR-2026-R3DB2ECCA: executed tool results without a progress report after
/// which the model is reminded, once, that it can send one.
pub const PROGRESS_REPORT_NUDGE_EVERY: usize = 8;

/// Soft reminder appended to a real tool result. A model told about
/// `report_progress` only in its system prompt weighs the instruction at the
/// end of the task, where a report is redundant with the reply; this puts the
/// question in front of it while the work is still going on. It forces
/// nothing and adds no LLM request.
pub const PROGRESS_REPORT_NUDGE: &str = "\n\n[PROGRESS] 8 tool results since your last progress report to the user. If you have finished a stage or found the cause of a problem since then, send one report_progress now (through run_tool when it is not in your tool list) and continue. If nothing meaningful changed, just continue.";

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
/// (`process_message_inner`) only in opt-in legacy periodic mode.
/// Asynchronous peer reads use result-aware
/// reflection instead: identical arguments cannot prove an immutable result.
/// The background task loop
/// deliberately keeps its softer treatment (the `record_result`
/// no-progress hint) because unattended tasks legitimately poll
/// status tools with identical arguments while a background job runs.
/// Verifier-configured agents are likewise exempt — the verifier lane
/// injects a `verdict: Repeating` note at this exact streak length and
/// the planner self-corrects, which is a richer recovery than an abort.
pub const DOOM_LOOP_THRESHOLD: usize = 3;

/// Legacy count-only heuristic. Multiple changes to one file may be productive;
/// this signal does not establish semantic progress or failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChurnSignal {
    pub path: String,
    pub edits: usize,
    /// The second threshold asks the provider/router to move away from the
    /// current model where a stronger/healthier alternative is available.
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
    /// Successful mutating calls per logical file path for the opt-in legacy
    /// churn heuristic. Counts alone do not prove lack of progress.
    file_mutations: HashMap<String, usize>,
    file_churn_enabled: bool,
    file_churn_threshold: usize,
    pending_file_churn: Option<FileChurnSignal>,
    /// An unchanged asynchronous peer snapshot may request legacy reflection.
    /// Its inline hint remains available independently of reflection mode.
    pending_peer_polling: Option<String>,
    /// `Some(n)` while the model can reach `report_progress`: executed tool
    /// results since the last report or reminder.
    results_since_progress_report: Option<usize>,
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
            file_churn_enabled: false,
            file_churn_threshold: std::env::var("OCTOS_FILE_CHURN_THRESHOLD")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(2, 100))
                .unwrap_or(5),
            pending_file_churn: None,
            pending_peer_polling: None,
            results_since_progress_report: None,
        }
    }

    /// Turn the progress-report reminder on for a turn whose model can reach
    /// the `report_progress` tool. Off by default.
    pub fn set_progress_report_nudge(&mut self, enabled: bool) {
        self.results_since_progress_report = enabled.then_some(0);
    }

    /// Count one executed tool result towards the progress-report reminder.
    /// `tool_name` is the logical tool, not the `run_tool` wrapper. Returns
    /// the reminder to append to that result once every
    /// [`PROGRESS_REPORT_NUDGE_EVERY`] results that held no report; a report
    /// or a reminder starts the count again.
    pub fn record_for_progress_report(&mut self, tool_name: &str) -> Option<&'static str> {
        let count = self.results_since_progress_report.as_mut()?;
        if tool_name == "report_progress" {
            *count = 0;
            return None;
        }
        *count += 1;
        if *count < PROGRESS_REPORT_NUDGE_EVERY {
            return None;
        }
        *count = 0;
        Some(PROGRESS_REPORT_NUDGE)
    }

    /// Count-only file churn is a legacy heuristic, not proof of stalled work.
    /// Result-aware repetition hints remain active independently of this flag.
    pub fn set_file_churn_enabled(&mut self, enabled: bool) {
        self.file_churn_enabled = enabled;
        if !enabled {
            self.file_mutations.clear();
            self.pending_file_churn = None;
        }
    }

    /// Record a successful file-mutating tool call. Returns a model-facing
    /// hint at the first threshold and again at each threshold multiple. The
    /// second and later firings request provider/model escalation, but never
    /// terminate the user turn.
    pub fn record_file_mutation(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        success: bool,
    ) -> Option<String> {
        if !success || !self.file_churn_enabled {
            return None;
        }
        let path = mutation_path(tool_name, args)?;
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
            " This is a repeated threshold breach; the harness will request a provider/model escalation."
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
    /// Returns a soft hint when a one-, two- or three-step pattern of
    /// `(name, args, text result)` repeats three times. Changing results
    /// break the pattern. This is textual evidence, not a semantic verdict.
    ///
    /// Fires at most once per "burst" — after firing, the result
    /// signature ring is cleared so a fourth identical call does not
    /// re-fire. Another hint needs a fresh repeated pattern.
    ///
    /// Callers should append the returned hint to the tool result
    /// message's content so the LLM sees it as part of its next-turn
    /// context. Does not terminate the turn.
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
        for cycle_len in 1..=3 {
            if !Self::is_repeating(&self.result_signatures, cycle_len) {
                continue;
            }
            // Fire once per burst. The next warning requires fresh evidence;
            // a fourth identical call alone does not re-fire.
            self.result_signatures.clear();
            if cycle_len == 1 && is_peer_polling_tool(tool_name) {
                self.pending_peer_polling = Some(tool_name.to_owned());
                return Some(PEER_POLLING_HINT.to_owned());
            }
            return Some(if cycle_len == 1 {
                NO_PROGRESS_HINT.to_string()
            } else {
                format!(
                    "\n\n[NO PROGRESS] The last {} tool results repeat a {cycle_len}-step pattern 3 times, including identical names, arguments and returned text. Repetition alone does not prove task failure or completion. Check whether state is changing; for asynchronous work use an available bounded wait or independent work. Otherwise choose an evidence-based next action instead of repeating the cycle blindly.",
                    cycle_len * 3
                )
            });
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
mod progress_report_tests {
    use super::*;

    #[test]
    fn should_remind_once_per_eight_results_when_no_report_was_sent() {
        let mut detector = LoopDetector::new(12);
        assert!(
            (0..20).all(|_| detector.record_for_progress_report("read_file").is_none()),
            "off unless the model can reach the tool"
        );
        detector.set_progress_report_nudge(true);
        let fired: Vec<bool> = (0..16)
            .map(|_| {
                detector
                    .record_for_progress_report("exec_command")
                    .is_some()
            })
            .collect();
        assert_eq!(fired.iter().filter(|hit| **hit).count(), 2);
        assert!(fired[7] && fired[15], "{fired:?}");
        for _ in 0..7 {
            assert!(detector.record_for_progress_report("edit_file").is_none());
        }
        assert!(
            detector
                .record_for_progress_report("report_progress")
                .is_none()
        );
        assert!(
            (0..7).all(|_| detector.record_for_progress_report("edit_file").is_none()),
            "a report starts the count again"
        );
    }
}
