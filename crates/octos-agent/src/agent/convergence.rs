//! Opt-in standalone convergence checkpoints for interactive turns.
//!
//! By default, observed repetition is reported inline by the tool loop. Calls,
//! tokens, elapsed time and edit counts alone do not justify another model
//! request. The old tools-disabled reflection is available only in explicitly
//! selected periodic mode for compatibility and controlled comparisons.

use std::time::{Duration, Instant};

use octos_core::TokenUsage;

const DEFAULT_LLM_CALL_INTERVAL: u32 = 20;
const DEFAULT_ACTIVE_TOKEN_INTERVAL: u64 = 100_000;
const DEFAULT_ELAPSED_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ReflectionMode {
    #[default]
    Inline,
    Periodic,
}

impl ReflectionMode {
    fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("periodic") => Self::Periodic,
            _ => Self::Inline,
        }
    }
}

/// Typed chronological envelope for a reflection. It is recorded once as a
/// USER-role context event, never as a System row: Anthropic hoists every
/// System row into the `system` field and OpenAI-compatible caches key on the
/// exact serialized prefix, so a System row whose text changes at each
/// checkpoint would rewrite the stable prefix inside the epoch (ADR "Stable
/// versus volatile prompt content"). `authority="background"` marks the block
/// as working memory rather than a user instruction.
const CHECKPOINT_CONTEXT_OPEN: &str =
    "<context_event kind=\"convergence_checkpoint\" authority=\"background\">";
const CHECKPOINT_CONTEXT_CLOSE: &str = "</context_event>";
const CHECKPOINT_CONTEXT_PREFIX: &str = "[internal convergence checkpoint]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CheckpointReason {
    LlmCalls {
        calls: u32,
    },
    ActiveTokens {
        tokens: u64,
    },
    Elapsed {
        elapsed: Duration,
    },
    FileChurn {
        path: String,
        edits: usize,
        escalation: bool,
    },
    PeerPolling {
        tool_name: String,
    },
}

impl CheckpointReason {
    fn describe(&self) -> String {
        match self {
            Self::LlmCalls { calls } => {
                format!("{calls} LLM action calls completed without a convergence checkpoint")
            }
            Self::ActiveTokens { tokens } => {
                format!("{tokens} active input/output tokens accumulated")
            }
            Self::Elapsed { elapsed } => {
                format!("{} elapsed", format_elapsed(*elapsed))
            }
            Self::PeerPolling { tool_name } => {
                format!("{tool_name} returned the same asynchronous peer snapshot three times")
            }
            Self::FileChurn {
                path,
                edits,
                escalation,
            } => {
                let escalation_note = if *escalation {
                    "; provider/model escalation requested"
                } else {
                    ""
                };
                format!("{path} was modified {edits} times{escalation_note}")
            }
        }
    }
}

/// Per-turn state for optional, non-terminal reflection checkpoints.
pub(super) struct ConvergenceController {
    mode: ReflectionMode,
    started_at: Instant,
    last_checkpoint_at: Instant,
    llm_call_interval: u32,
    active_token_interval: u64,
    elapsed_interval: Duration,
    /// Action calls COMPLETED so far in this turn, recorded by the loop after
    /// each successful action response. Reflection calls are never recorded,
    /// so they cannot accelerate the next checkpoint.
    completed_action_calls: u32,
    /// `completed_action_calls` at the last checkpoint (0 at turn start).
    action_calls_at_checkpoint: u32,
    /// Action-call active tokens observed at the last checkpoint.
    active_tokens_at_checkpoint: u64,
    /// Active tokens consumed by the reflection calls themselves. The turn
    /// records them as real spend; the thresholds must not.
    reflection_active_tokens: u64,
    pending_reason: Option<CheckpointReason>,
    pending_reflection: Option<String>,
    checkpoints: u32,
}

impl ConvergenceController {
    pub(super) fn from_env(started_at: Instant) -> Self {
        let llm_call_interval = env_u64(
            "OCTOS_CONVERGENCE_LLM_CALLS",
            u64::from(DEFAULT_LLM_CALL_INTERVAL),
            2,
            10_000,
        ) as u32;
        let active_token_interval = env_u64(
            "OCTOS_CONVERGENCE_ACTIVE_TOKENS",
            DEFAULT_ACTIVE_TOKEN_INTERVAL,
            1_000,
            100_000_000,
        );
        let elapsed_interval = Duration::from_secs(env_u64(
            "OCTOS_CONVERGENCE_SECS",
            DEFAULT_ELAPSED_INTERVAL_SECS,
            10,
            86_400,
        ));
        let mut controller = Self::new(
            started_at,
            llm_call_interval,
            active_token_interval,
            elapsed_interval,
        );
        controller.mode =
            ReflectionMode::from_setting(std::env::var("OCTOS_CONVERGENCE_MODE").ok().as_deref());
        controller
    }

    /// Explicit thresholds select the legacy periodic mode (also used by the
    /// deterministic test seam). Production defaults go through `from_env`.
    pub(super) fn new(
        started_at: Instant,
        llm_call_interval: u32,
        active_token_interval: u64,
        elapsed_interval: Duration,
    ) -> Self {
        Self {
            mode: ReflectionMode::Periodic,
            started_at,
            last_checkpoint_at: started_at,
            llm_call_interval,
            active_token_interval,
            elapsed_interval,
            completed_action_calls: 0,
            action_calls_at_checkpoint: 0,
            active_tokens_at_checkpoint: 0,
            reflection_active_tokens: 0,
            pending_reason: None,
            pending_reflection: None,
            checkpoints: 0,
        }
    }

    /// Queue an early checkpoint only in legacy periodic mode. Forced signals
    /// precede periodic thresholds; edit counts alone do not prove stalled work.
    pub(super) fn force(&mut self, reason: CheckpointReason) {
        if self.standalone_enabled() {
            self.pending_reason = Some(reason);
        }
    }

    pub(super) fn standalone_enabled(&self) -> bool {
        self.mode == ReflectionMode::Periodic
    }

    /// Record one COMPLETED action call. Call this after a successful action
    /// response, never for a checkpoint's reflection call.
    pub(super) fn record_action_call(&mut self) {
        self.completed_action_calls = self.completed_action_calls.saturating_add(1);
    }

    /// Called at the top of a loop iteration, BEFORE its action call. The call
    /// axis therefore counts COMPLETED action calls (via
    /// [`Self::record_action_call`]), never the pre-call iteration index, so
    /// an N-call checkpoint fires only once N action calls have finished and
    /// the reflection runs before action call N+1.
    pub(super) fn due(&mut self, usage: &TokenUsage) -> Option<CheckpointReason> {
        // Repetition evidence already accompanies the real tool result. Do
        // not duplicate it with a separate history replay/tool-choice change.
        if !self.standalone_enabled() {
            return None;
        }
        if let Some(reason) = self.pending_reason.take() {
            return Some(reason);
        }

        // Thresholds count completed ACTION calls and ACTION tokens only: a
        // checkpoint's own reflection call is real spend for the turn but is
        // never recorded as an action call and its tokens are subtracted.
        let calls_since_checkpoint = self
            .completed_action_calls
            .saturating_sub(self.action_calls_at_checkpoint);
        let action_tokens = self.action_active_tokens(usage);
        if calls_since_checkpoint >= self.llm_call_interval {
            return Some(CheckpointReason::LlmCalls {
                calls: calls_since_checkpoint,
            });
        }
        if action_tokens.saturating_sub(self.active_tokens_at_checkpoint)
            >= self.active_token_interval
        {
            return Some(CheckpointReason::ActiveTokens {
                tokens: action_tokens,
            });
        }
        if self.last_checkpoint_at.elapsed() >= self.elapsed_interval {
            return Some(CheckpointReason::Elapsed {
                elapsed: self.started_at.elapsed(),
            });
        }
        None
    }

    pub(super) fn prompt(reason: &CheckpointReason) -> String {
        let escalation_instruction = if matches!(reason, CheckpointReason::PeerPolling { .. }) {
            "\nThis is asynchronous peer polling, not proof the tool cannot change. Identify whether peers are still running, awaiting input, completed, or failed from the actual tool evidence. Do not busy-wait or invent a completed result: choose independent work, address a peer's input request, or use an available bounded wait before gathering again. Only claim completion after seeing the required result."
        } else if matches!(
            reason,
            CheckpointReason::FileChurn {
                escalation: true,
                ..
            }
        ) {
            "\nThis is a repeated churn breach. If you cannot identify a substantially different, root-cause-driven approach, the next action must be to pause and ask the user for direction instead of editing again."
        } else {
            ""
        };
        format!(
            "[CONVERGENCE CHECKPOINT — internal working step, not a user-facing final answer]\n\
             Trigger: {}. Tools are disabled for this call. Briefly synthesize:\n\
             1. the user's actual goal and what is already complete;\n\
             2. concrete evidence gathered and changes made;\n\
             3. failures, uncertainty, or repeated work (especially repeated edits);\n\
             4. whether the current approach is converging;\n\
             5. the smallest 1–3 next actions, or COMPLETE if the goal is satisfied.\n\
             Be concise and factual. Do not address the user and do not call tools.{}",
            reason.describe(),
            escalation_instruction,
        )
    }

    /// Record a completed checkpoint and re-arm the periodic thresholds: the
    /// next call-based checkpoint is due after `llm_call_interval` further
    /// COMPLETED action calls.
    ///
    /// `reflection_usage` is the reflection call's own usage when that call
    /// succeeded; its tokens are excluded from the action counters (the call
    /// itself is never recorded as an action call). `None` means the
    /// checkpoint failed open and the iteration proceeds to its normal action
    /// call, which is recorded when it completes.
    pub(super) fn complete(
        &mut self,
        usage: &TokenUsage,
        reflection_usage: Option<&TokenUsage>,
        reflection: String,
    ) {
        self.checkpoints = self.checkpoints.saturating_add(1);
        self.action_calls_at_checkpoint = self.completed_action_calls;
        if let Some(reflection_usage) = reflection_usage {
            self.reflection_active_tokens = self
                .reflection_active_tokens
                .saturating_add(active_tokens(reflection_usage));
        }
        self.active_tokens_at_checkpoint = self.action_active_tokens(usage);
        self.last_checkpoint_at = Instant::now();
        let reflection = reflection.trim();
        self.pending_reflection = (!reflection.is_empty()).then(|| reflection.to_string());
    }

    /// Turn-cumulative active tokens minus what the reflection calls consumed.
    fn action_active_tokens(&self, usage: &TokenUsage) -> u64 {
        active_tokens(usage).saturating_sub(self.reflection_active_tokens)
    }

    /// Consume a newly completed reflection once. The caller appends it to
    /// chronological history; later actions must not relocate it to the tail.
    pub(super) fn take_context_message(&mut self) -> Option<String> {
        self.pending_reflection.take().map(|reflection| {
            format!(
                "{CHECKPOINT_CONTEXT_OPEN}\n{CHECKPOINT_CONTEXT_PREFIX}\n{reflection}\n\
                 Use this internal reflection to guide the next action. Verify it against the \n\
                 conversation and tool results; it is not a user message.\n\
                 {CHECKPOINT_CONTEXT_CLOSE}"
            )
        })
    }

    pub(super) fn checkpoints(&self) -> u32 {
        self.checkpoints
    }
}

pub(super) fn active_tokens(usage: &TokenUsage) -> u64 {
    u64::from(usage.input_tokens) + u64::from(usage.output_tokens)
}

pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
}
