//! Wire-parameter DTOs extracted from the UI Protocol transport (A-02).
//! Pure serde data shapes only; validation, dispatch, lifecycle, and
//! durable-delivery behavior remain in the parent module.

use super::*;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawClientHelloParams {
    #[serde(default)]
    pub(crate) transport: Option<String>,
    #[serde(default, alias = "features", alias = "requested_features")]
    pub(crate) supported_features: Vec<String>,
    #[serde(default)]
    pub(crate) client: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawProfileParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawVoiceAdmitParams {
    pub(crate) session_id: SessionKey,
    pub(crate) request_id: String,
    pub(crate) turn_id: TurnId,
    pub(crate) media: Vec<FileRef>,
    #[serde(default)]
    pub(crate) topic: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawVoiceCommitAdmissionParams {
    pub(crate) admission_id: String,
    #[serde(default)]
    pub(crate) supersedes_turn_id: Option<TurnId>,
    pub(crate) turn: TurnStartParams,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawProfileSkillsListParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSkillActionListParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) surface: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSkillActionInvokeParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) action_id: String,
    #[serde(default)]
    pub(crate) arguments: Value,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSkillActionJobListParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) batch_id: Option<String>,
    #[serde(default)]
    pub(crate) action_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSkillActionJobReadParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) job_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawProfileSkillsRegistrySearchParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default, alias = "query")]
    pub(crate) q: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawProfileSkillsInstallParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) repo: String,
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) force: bool,
}

/// UPCR-2026-R48CB467B `profile/skills/install_reviewed` params. Every
/// identity field is an EXPECTED value from the review; unknown fields are
/// rejected so a client can never smuggle a second source of truth
/// (`deny_unknown_fields`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileSkillsInstallReviewedParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) package: String,
    pub(crate) repo: String,
    pub(crate) source_commit: String,
    pub(crate) reviewed_at: String,
    pub(crate) platform: String,
    #[serde(default)]
    pub(crate) artifact: Option<RawReviewedArtifactExpectation>,
    #[serde(default)]
    pub(crate) force: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawReviewedArtifactExpectation {
    pub(crate) sha256: String,
    pub(crate) signature: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawProfileSkillsRemoveParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) name: String,
}

/// One provider route on the AppUI `profile/llm/*` wire (#2166 typed
/// schema). Unknown keys are rejected by [`reject_unknown_llm_upsert_fields`]
/// before serde sees them (so the error lists EVERY rejected field), and
/// `deny_unknown_fields` here is the belt-and-braces second layer.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLlmRoute {
    #[serde(default)]
    pub(crate) route_id: Option<String>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
    #[serde(default)]
    pub(crate) api_key_env: Option<String>,
    #[serde(default)]
    pub(crate) api_type: Option<String>,
}

/// The typed main-model selection + inference-parameter schema shared by
/// `profile/llm/upsert`, `profile/llm/test`, and `profile/llm/fetch_models`
/// (#2166). Test and Save parse the identical shape, so a payload that
/// probes successfully is byte-for-byte the payload that persists.
///
/// Absent/null semantics for every optional inference field: `absent ≡
/// null ≡ inherit` (clear any prior override — the upsert payload is the
/// COMPLETE inference configuration for the addressed selection); an
/// explicit value is an override. Omitted fields always mean "defer to the
/// next tier of the precedence chain", never "silently keep serving a value
/// the caller cannot see in the list response".
///
/// Ownership (what this schema deliberately does NOT accept):
/// - `max_output_tokens` → owned by the profile gateway contract
///   (`[gateway] max_output_tokens`); rejected with
///   `kind: "llm_param_owned_elsewhere"`.
/// - Per-session reasoning overrides → owned by the durable
///   session/turn contract (`ui_protocol_reasoning_effort.rs`); this schema
///   only sets the per-MODEL default tier.
/// - Arbitrary provider request-body keys → owned by the gateway
///   `llm_sampling_params` passthrough (#2176); rejected here as unknown.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLlmSelection {
    #[serde(default)]
    pub(crate) family_id: Option<String>,
    #[serde(default)]
    pub(crate) model_id: Option<String>,
    #[serde(default)]
    pub(crate) route: RawLlmRoute,
    /// Typed compatibility metadata (mirrors
    /// [`octos_llm::openai::ModelHints`]) — never a request-body bag.
    #[serde(default)]
    pub(crate) model_hints: Option<octos_llm::openai::ModelHints>,
    /// Local runtime context budget override (#2142). Reaches the runtime
    /// `ContextWindowOverride` via the durable selection; NOT an upstream
    /// request field. Carried as a raw integer on the wire so negative /
    /// over-u32 values reach the typed range check (#2187) instead of a
    /// generic serde error; validated into the `u32` store type by
    /// [`validate_llm_inference_fields`].
    #[serde(default)]
    pub(crate) context_window: Option<WireContextWindow>,
    /// Per-model default sampling temperature (finite, 0.0..=2.0).
    #[serde(default)]
    pub(crate) temperature: Option<f64>,
    /// Per-model default nucleus-sampling ceiling (finite, 0.0..=1.0).
    #[serde(default)]
    pub(crate) top_p: Option<f64>,
    /// Per-model default reasoning effort (the session/turn override tier
    /// wins over this).
    #[serde(default)]
    pub(crate) reasoning_effort: Option<octos_llm::ReasoningEffort>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileLlmSelectParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) family_id: Option<String>,
    #[serde(default)]
    pub(crate) model_id: Option<String>,
    #[serde(default)]
    pub(crate) route_id: Option<String>,
}

/// Typed AppUI `profile/llm/upsert` / `test` / `fetch_models` params
/// (#2166). Unknown keys are rejected with their full dotted paths —
/// `never return applied:true after discarding requested settings`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfileLlmUpsertParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) selection: RawLlmSelection,
    #[serde(default)]
    pub(crate) api_key: Option<Value>,
    #[serde(default)]
    pub(crate) set_primary: bool,
}

/// `profile/llm/delete`: remove one configured model (primary or fallback) by
/// its address (family + model + route).
#[derive(Debug, Deserialize)]
pub(crate) struct RawProfileLlmDeleteParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) family_id: String,
    pub(crate) model_id: String,
    pub(crate) route_id: String,
}

/// One named provider lane on the wire (mirrors [`crate::config::SubProviderConfig`]).
/// `key` is required; the rest are optional so the TUI can send whatever the
/// user filled in.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawSubProvider {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) api_key_env: Option<String>,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) default_context_window: Option<u32>,
    #[serde(default)]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) api_type: Option<String>,
}

/// `profile/sub_providers/upsert`: add or replace (by `key`) one named lane.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawProfileSubProvidersUpsertParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) sub_provider: RawSubProvider,
    /// Optional secret to store under the lane's `api_key_env` (never persisted
    /// to plaintext config — relocated to the OS keychain like the llm path).
    #[serde(default)]
    pub(crate) api_key: Option<Value>,
}

/// `profile/sub_providers/remove`: drop the lane with this `key`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawProfileSubProvidersRemoveParams {
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) key: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawAutonomyListParams {
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAutonomySessionParams {
    pub(crate) session_id: SessionKey,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAgentParams {
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAgentOutputParams {
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) cursor: Option<Value>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAgentArtifactReadParams {
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) artifact_id: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

/// Param shape for the `task/artifact/list` alias. Accepts either
/// `agent_id` (legacy `agent/artifact/*` shape) or `task_id` (the
/// UPCR-2026-019 / M13 spec-conforming shape). Spec-conforming clients
/// reach the artifact surface via `task/list`, which exposes `task_id`,
/// so the alias must accept that synonym. Codex P1 follow-up to #1094.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTaskAgentParams {
    #[serde(default)]
    pub(crate) agent_id: Option<String>,
    #[serde(default)]
    pub(crate) task_id: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

/// Param shape for the `task/artifact/read` alias. Mirrors
/// `RawAgentArtifactReadParams` but accepts either `agent_id` or
/// `task_id` like `RawTaskAgentParams`. Codex P1 follow-up to #1094.
#[derive(Debug, Deserialize)]
pub(crate) struct RawTaskArtifactReadParams {
    #[serde(default)]
    pub(crate) agent_id: Option<String>,
    #[serde(default)]
    pub(crate) task_id: Option<String>,
    #[serde(default)]
    pub(crate) artifact_id: Option<String>,
    #[serde(default)]
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawGoalSetParams {
    pub(crate) session_id: SessionKey,
    pub(crate) objective: String,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) token_budget: Option<u64>,
    #[serde(default)]
    pub(crate) transition_actor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawGoalOperatorTransitionParams {
    pub(crate) session_id: SessionKey,
    pub(crate) goal_id: String,
    pub(crate) action: String,
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLoopCreateParams {
    pub(crate) session_id: SessionKey,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) prompt: Option<String>,
    #[serde(default, alias = "command", alias = "input")]
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) interval_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLoopIdParams {
    pub(crate) loop_id: String,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

/// #1977 `monitor/create` params. `argv` is the sandboxed probe command;
/// `mode` selects poll (default, with `interval_seconds`) or stream.
#[derive(Debug, Deserialize)]
pub(crate) struct RawMonitorCreateParams {
    pub(crate) session_id: SessionKey,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) filter_regex: Option<String>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) interval_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) batch_ms: Option<u32>,
    #[serde(default)]
    pub(crate) timeout_secs: Option<u64>,
    #[serde(default)]
    pub(crate) persistent: Option<bool>,
    #[serde(default)]
    pub(crate) max_events_per_hour: Option<u32>,
    #[serde(default)]
    pub(crate) goal_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMonitorIdParams {
    pub(crate) monitor_id: String,
    #[serde(default)]
    pub(crate) session_id: Option<SessionKey>,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawReviewStartParams {
    pub(crate) session_id: SessionKey,
    #[serde(default)]
    pub(crate) profile_id: Option<String>,
    #[serde(default)]
    pub(crate) turn_id: Option<TurnId>,
    #[serde(default)]
    pub(crate) target: Option<Value>,
    #[serde(default)]
    pub(crate) prompt: Option<String>,
    #[serde(default)]
    pub(crate) instructions: Option<String>,
    #[serde(default)]
    pub(crate) delivery: Option<String>,
}

/// `turn/steer` params: `{session_id, expected_turn_id?, input}` —
/// mirrors codex `TurnSteerParams { thread_id, expected_turn_id, input }`
/// with octos's optional-precondition twist: an ABSENT `expected_turn_id`
/// steers whatever turn is live (a mismatch when present is rejected, codex
/// `ExpectedTurnMismatch`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawTurnSteerParams {
    pub(crate) session_id: SessionKey,
    #[serde(default)]
    pub(crate) expected_turn_id: Option<TurnId>,
    #[serde(default)]
    pub(crate) input: Vec<InputItem>,
}
