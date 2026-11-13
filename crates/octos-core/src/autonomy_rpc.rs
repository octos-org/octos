//! Typed autonomy-domain RPC contracts (T-0118 A-08).
//!
//! One registry — [`AUTONOMY_METHODS`] — is the single source of truth for
//! the coding-autonomy wire surface: each entry names the method, the
//! capability feature group that gates it, and the typed param decoder. The
//! serve/gateway transports derive BOTH their capability gate
//! (`autonomy_method_available`) and their dispatcher param parsing from
//! these entries, so a method can no longer be added to one surface and
//! forgotten on the other.
//!
//! Wire contract: the param DTOs below are byte-compatible with the CLI's
//! previous private `Raw*Params` shapes — identical field names, defaults,
//! serde aliases, and (deliberately) NO `deny_unknown_fields`, and the
//! decoder keeps the published error format `"{method} params: {err}"`.
//! Clients that pinned the raw JSON see zero behavioral drift.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::SessionKey;
use crate::ui_protocol::{RpcError, methods};

/// `agent/list`, `loop/list` and `monitor/list` params — all fields
/// optional; a null params object decodes to the default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutonomyListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// Session-scoped autonomy params (`goal/get`, `goal/clear`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomySessionParams {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// `agent/status.read`, `agent/artifact/list`, `agent/interrupt` and
/// `agent/close` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// `agent/output.read` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOutputParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// `agent/artifact/read` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentArtifactReadParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// Param shape for the `task/artifact/list` alias. Accepts either
/// `agent_id` (legacy `agent/artifact/*` shape) or `task_id` (the
/// UPCR-2026-019 / M13 spec-conforming shape). Spec-conforming clients
/// reach the artifact surface via `task/list`, which exposes `task_id`,
/// so the alias must accept that synonym. Codex P1 follow-up to #1094.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAgentParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl TaskAgentParams {
    /// Prefer the legacy `agent_id` when both are set so existing clients
    /// keep their semantics; accept `task_id` as a synonym otherwise.
    ///
    /// Codex P1 re-review on #1121: the `task_id` path MUST require an
    /// explicit `session_id` so the session/parent-child ownership
    /// check still runs. Without it, `ensure_agent_control_scope`
    /// silently downgrades to profile-only matching, and a
    /// same-profile connection that knows another task's UUID could
    /// read its artifacts. The legacy `agent_id` shape keeps the
    /// optional-session behavior for backwards compat.
    pub fn resolve_agent_id(&self, method: &str) -> Result<String, RpcError> {
        resolve_agent_or_task_id(
            self.agent_id.as_deref(),
            self.task_id.as_deref(),
            self.session_id.as_ref(),
            method,
        )
    }
}

/// Param shape for the `task/artifact/read` alias. Mirrors
/// [`AgentArtifactReadParams`] but accepts either `agent_id` or
/// `task_id` like [`TaskAgentParams`]. Codex P1 follow-up to #1094.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactReadParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl TaskArtifactReadParams {
    /// See [`TaskAgentParams::resolve_agent_id`] — the `task_id` path
    /// requires `session_id` so the session/parent-child ownership check
    /// still runs.
    pub fn resolve_agent_id(&self, method: &str) -> Result<String, RpcError> {
        resolve_agent_or_task_id(
            self.agent_id.as_deref(),
            self.task_id.as_deref(),
            self.session_id.as_ref(),
            method,
        )
    }
}

fn resolve_agent_or_task_id(
    agent_id: Option<&str>,
    task_id: Option<&str>,
    session_id: Option<&SessionKey>,
    method: &str,
) -> Result<String, RpcError> {
    match (agent_id, task_id) {
        (Some(agent_id), _) => Ok(agent_id.to_owned()),
        (None, Some(task_id)) => {
            if session_id.is_none() {
                return Err(RpcError::invalid_params(format!(
                    "{method} params: `task_id` requires `session_id` so artifact access can be scoped to the session that owns the task"
                )));
            }
            Ok(task_id.to_owned())
        }
        (None, None) => Err(RpcError::invalid_params(format!(
            "{method} params: missing field `agent_id` or `task_id`"
        ))),
    }
}

/// `session/goal/set` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalSetParams {
    pub session_id: SessionKey,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition_actor: Option<String>,
}

/// `session/goal/operator_transition` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalOperatorTransitionParams {
    pub session_id: SessionKey,
    pub goal_id: String,
    pub action: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// `loop/create` params. The recurring command accepts both the canonical
/// `command` field and the `input` synonym used by earlier clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopCreateParams {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(
        default,
        alias = "command",
        alias = "input",
        skip_serializing_if = "Option::is_none"
    )]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// `loop/delete`, `loop/pause`, `loop/resume` and `loop/fire_now` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopIdParams {
    pub loop_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// #1977 `monitor/create` params. `argv` is the sandboxed probe command;
/// `mode` selects poll (default, with `interval_seconds`) or stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorCreateParams {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_events_per_hour: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
}

/// `monitor/pause`, `monitor/resume` and `monitor/delete` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorIdParams {
    pub monitor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// Typed autonomy command: one variant per registry method, carrying the
/// decoded params. Transports match on this enum and keep only their
/// authorization/business mapping.
#[derive(Debug, Clone, PartialEq)]
pub enum AutonomyCommand {
    AgentList(AutonomyListParams),
    AgentStatusRead(AgentParams),
    AgentOutputRead(AgentOutputParams),
    AgentArtifactList(AgentParams),
    AgentArtifactRead(AgentArtifactReadParams),
    TaskArtifactList(TaskAgentParams),
    TaskArtifactRead(TaskArtifactReadParams),
    AgentInterrupt(AgentParams),
    AgentClose(AgentParams),
    SessionGoalGet(AutonomySessionParams),
    SessionGoalSet(GoalSetParams),
    SessionGoalClear(AutonomySessionParams),
    SessionGoalOperatorTransition(GoalOperatorTransitionParams),
    LoopCreate(LoopCreateParams),
    LoopList(AutonomyListParams),
    LoopDelete(LoopIdParams),
    LoopPause(LoopIdParams),
    LoopResume(LoopIdParams),
    LoopFireNow(LoopIdParams),
    MonitorCreate(MonitorCreateParams),
    MonitorList(AutonomyListParams),
    MonitorPause(MonitorIdParams),
    MonitorResume(MonitorIdParams),
    MonitorDelete(MonitorIdParams),
}

/// Capability feature group gating a registry method. Transports map each
/// group to their negotiated-feature predicate; the method → group mapping
/// itself lives ONLY here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyFeature {
    AgentControl,
    GoalRuntime,
    LoopRuntime,
    MonitorRuntime,
}

/// One typed method entry: wire name + capability feature group + param
/// decoder. Drives BOTH capability gating and dispatch parsing.
pub struct AutonomyMethodSpec {
    pub method: &'static str,
    pub feature: AutonomyFeature,
    parse: fn(&str, Value) -> Result<AutonomyCommand, RpcError>,
}

impl AutonomyMethodSpec {
    /// Decode `params` into this entry's typed command variant, preserving
    /// the published `"{method} params: {err}"` error format.
    pub fn parse_command(&self, method: &str, params: Value) -> Result<AutonomyCommand, RpcError> {
        (self.parse)(method, params)
    }
}

/// Decode params for a method whose param object is REQUIRED (a null
/// params value is a serde type error, as before).
fn decode_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|err| RpcError::invalid_params(format!("{method} params: {err}")))
}

/// Decode params for a method whose param object is OPTIONAL: null decodes
/// to `Default`, anything else parses normally.
fn decode_optional_params<T: serde::de::DeserializeOwned + Default>(
    method: &str,
    params: Value,
) -> Result<T, RpcError> {
    if params.is_null() {
        return Ok(T::default());
    }
    decode_params(method, params)
}

/// The single autonomy method registry. Keep feature groups aligned with
/// the capability strings: agent/* → `coding.agent_control.v1`,
/// session/goal/* → `coding.goal_runtime.v1`, loop/* →
/// `coding.loop_runtime.v1`, monitor/* → `coding.monitor_runtime.v1`.
pub static AUTONOMY_METHODS: &[AutonomyMethodSpec] = &[
    AutonomyMethodSpec {
        method: methods::AGENT_LIST,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_optional_params(m, p).map(AutonomyCommand::AgentList),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_STATUS_READ,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentStatusRead),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_OUTPUT_READ,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentOutputRead),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_ARTIFACT_LIST,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentArtifactList),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_ARTIFACT_READ,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentArtifactRead),
    },
    AutonomyMethodSpec {
        method: methods::TASK_ARTIFACT_LIST,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::TaskArtifactList),
    },
    AutonomyMethodSpec {
        method: methods::TASK_ARTIFACT_READ,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::TaskArtifactRead),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_INTERRUPT,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentInterrupt),
    },
    AutonomyMethodSpec {
        method: methods::AGENT_CLOSE,
        feature: AutonomyFeature::AgentControl,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::AgentClose),
    },
    AutonomyMethodSpec {
        method: methods::SESSION_GOAL_GET,
        feature: AutonomyFeature::GoalRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::SessionGoalGet),
    },
    AutonomyMethodSpec {
        method: methods::SESSION_GOAL_SET,
        feature: AutonomyFeature::GoalRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::SessionGoalSet),
    },
    AutonomyMethodSpec {
        method: methods::SESSION_GOAL_CLEAR,
        feature: AutonomyFeature::GoalRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::SessionGoalClear),
    },
    AutonomyMethodSpec {
        method: methods::SESSION_GOAL_OPERATOR_TRANSITION,
        feature: AutonomyFeature::GoalRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::SessionGoalOperatorTransition),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_CREATE,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::LoopCreate),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_LIST,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_optional_params(m, p).map(AutonomyCommand::LoopList),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_DELETE,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::LoopDelete),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_PAUSE,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::LoopPause),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_RESUME,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::LoopResume),
    },
    AutonomyMethodSpec {
        method: methods::LOOP_FIRE_NOW,
        feature: AutonomyFeature::LoopRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::LoopFireNow),
    },
    AutonomyMethodSpec {
        method: methods::MONITOR_CREATE,
        feature: AutonomyFeature::MonitorRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::MonitorCreate),
    },
    AutonomyMethodSpec {
        method: methods::MONITOR_LIST,
        feature: AutonomyFeature::MonitorRuntime,
        parse: |m, p| decode_optional_params(m, p).map(AutonomyCommand::MonitorList),
    },
    AutonomyMethodSpec {
        method: methods::MONITOR_PAUSE,
        feature: AutonomyFeature::MonitorRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::MonitorPause),
    },
    AutonomyMethodSpec {
        method: methods::MONITOR_RESUME,
        feature: AutonomyFeature::MonitorRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::MonitorResume),
    },
    AutonomyMethodSpec {
        method: methods::MONITOR_DELETE,
        feature: AutonomyFeature::MonitorRuntime,
        parse: |m, p| decode_params(m, p).map(AutonomyCommand::MonitorDelete),
    },
];

/// Look up the registry entry for a wire method name. `None` means the
/// method is NOT part of the autonomy surface.
pub fn autonomy_method_spec(method: &str) -> Option<&'static AutonomyMethodSpec> {
    AUTONOMY_METHODS.iter().find(|spec| spec.method == method)
}

/// Parse one autonomy request into its typed command. Unknown (non-autonomy)
/// methods fail with `method_not_found`, matching the previous dispatcher
/// default arm.
pub fn parse_autonomy_command(method: &str, params: Value) -> Result<AutonomyCommand, RpcError> {
    let Some(spec) = autonomy_method_spec(method) else {
        return Err(RpcError::method_not_found(method));
    };
    spec.parse_command(method, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_protocol::rpc_error_codes;
    use serde_json::json;

    fn key(value: &str) -> SessionKey {
        SessionKey(value.to_owned())
    }

    #[test]
    fn should_cover_every_autonomy_method_once_with_its_feature_group() {
        let expected: &[(&str, AutonomyFeature)] = &[
            (methods::AGENT_LIST, AutonomyFeature::AgentControl),
            (methods::AGENT_STATUS_READ, AutonomyFeature::AgentControl),
            (methods::AGENT_OUTPUT_READ, AutonomyFeature::AgentControl),
            (methods::AGENT_ARTIFACT_LIST, AutonomyFeature::AgentControl),
            (methods::AGENT_ARTIFACT_READ, AutonomyFeature::AgentControl),
            (methods::TASK_ARTIFACT_LIST, AutonomyFeature::AgentControl),
            (methods::TASK_ARTIFACT_READ, AutonomyFeature::AgentControl),
            (methods::AGENT_INTERRUPT, AutonomyFeature::AgentControl),
            (methods::AGENT_CLOSE, AutonomyFeature::AgentControl),
            (methods::SESSION_GOAL_GET, AutonomyFeature::GoalRuntime),
            (methods::SESSION_GOAL_SET, AutonomyFeature::GoalRuntime),
            (methods::SESSION_GOAL_CLEAR, AutonomyFeature::GoalRuntime),
            (
                methods::SESSION_GOAL_OPERATOR_TRANSITION,
                AutonomyFeature::GoalRuntime,
            ),
            (methods::LOOP_CREATE, AutonomyFeature::LoopRuntime),
            (methods::LOOP_LIST, AutonomyFeature::LoopRuntime),
            (methods::LOOP_DELETE, AutonomyFeature::LoopRuntime),
            (methods::LOOP_PAUSE, AutonomyFeature::LoopRuntime),
            (methods::LOOP_RESUME, AutonomyFeature::LoopRuntime),
            (methods::LOOP_FIRE_NOW, AutonomyFeature::LoopRuntime),
            (methods::MONITOR_CREATE, AutonomyFeature::MonitorRuntime),
            (methods::MONITOR_LIST, AutonomyFeature::MonitorRuntime),
            (methods::MONITOR_PAUSE, AutonomyFeature::MonitorRuntime),
            (methods::MONITOR_RESUME, AutonomyFeature::MonitorRuntime),
            (methods::MONITOR_DELETE, AutonomyFeature::MonitorRuntime),
        ];
        assert_eq!(AUTONOMY_METHODS.len(), expected.len());
        for (method, feature) in expected {
            let spec = autonomy_method_spec(method)
                .unwrap_or_else(|| panic!("registry is missing {method}"));
            assert_eq!(spec.feature, *feature, "wrong feature group for {method}");
        }
    }

    #[test]
    fn should_reject_unknown_methods_as_method_not_found() {
        assert!(autonomy_method_spec("autonomy/unknown").is_none());
        let err = parse_autonomy_command("autonomy/unknown", json!({})).unwrap_err();
        assert_eq!(err.code, rpc_error_codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn should_decode_published_wire_shapes_onto_the_typed_variants() {
        let command = parse_autonomy_command(
            methods::AGENT_OUTPUT_READ,
            json!({
                "agent_id": "a1",
                "session_id": "cli:s1",
                "profile_id": "p1",
                "cursor": {"turn": 3},
                "limit": 50,
            }),
        )
        .unwrap();
        assert_eq!(
            command,
            AutonomyCommand::AgentOutputRead(AgentOutputParams {
                agent_id: "a1".into(),
                session_id: Some(key("cli:s1")),
                profile_id: Some("p1".into()),
                cursor: Some(json!({"turn": 3})),
                limit: Some(50),
            })
        );

        let command = parse_autonomy_command(
            methods::SESSION_GOAL_SET,
            json!({
                "session_id": "cli:s1",
                "objective": "ship it",
                "status": "active",
                "token_budget": 9000,
                "transition_actor": "operator",
            }),
        )
        .unwrap();
        assert!(matches!(
            command,
            AutonomyCommand::SessionGoalSet(GoalSetParams {
                ref objective,
                token_budget: Some(9000),
                ..
            }) if objective == "ship it"
        ));

        let command = parse_autonomy_command(
            methods::MONITOR_CREATE,
            json!({
                "session_id": "cli:s1",
                "name": "probe",
                "argv": ["echo", "hi"],
                "mode": "stream",
                "batch_ms": 250,
                "persistent": true,
                "max_events_per_hour": 120,
                "goal_id": "g1",
            }),
        )
        .unwrap();
        assert!(matches!(
            command,
            AutonomyCommand::MonitorCreate(MonitorCreateParams {
                ref argv,
                batch_ms: Some(250),
                persistent: Some(true),
                ..
            }) if argv == &["echo", "hi"]
        ));

        // Unknown fields remain IGNORED (the published shape never had
        // deny_unknown_fields); tightening this would be a wire break.
        let command = parse_autonomy_command(
            methods::AGENT_LIST,
            json!({"session_id": "cli:s1", "unexpected": true}),
        )
        .unwrap();
        assert_eq!(
            command,
            AutonomyCommand::AgentList(AutonomyListParams {
                session_id: Some(key("cli:s1")),
                profile_id: None,
            })
        );
    }

    #[test]
    fn should_default_optional_list_params_from_null_and_empty_objects() {
        for method in [
            methods::AGENT_LIST,
            methods::LOOP_LIST,
            methods::MONITOR_LIST,
        ] {
            let command = parse_autonomy_command(method, Value::Null).unwrap();
            assert!(matches!(
                command,
                AutonomyCommand::AgentList(_)
                    | AutonomyCommand::LoopList(_)
                    | AutonomyCommand::MonitorList(_)
            ));
            let command = parse_autonomy_command(method, json!({})).unwrap();
            assert!(matches!(
                command,
                AutonomyCommand::AgentList(AutonomyListParams {
                    session_id: None,
                    ..
                }) | AutonomyCommand::LoopList(AutonomyListParams {
                    session_id: None,
                    ..
                }) | AutonomyCommand::MonitorList(AutonomyListParams {
                    session_id: None,
                    ..
                })
            ));
        }
        // Required-param methods still reject a null params object.
        let err = parse_autonomy_command(methods::AGENT_CLOSE, Value::Null).unwrap_err();
        assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
    }

    #[test]
    fn should_accept_loop_create_command_and_input_aliases() {
        let via_command = parse_autonomy_command(
            methods::LOOP_CREATE,
            json!({"session_id": "cli:s1", "command": "tick"}),
        )
        .unwrap();
        let via_input = parse_autonomy_command(
            methods::LOOP_CREATE,
            json!({"session_id": "cli:s1", "input": "tick"}),
        )
        .unwrap();
        assert_eq!(via_command, via_input);
        assert!(matches!(
            via_command,
            AutonomyCommand::LoopCreate(LoopCreateParams {
                command: Some(ref command),
                ..
            }) if command == "tick"
        ));
        // Both spellings at once stay a duplicate-field serde error.
        let err = parse_autonomy_command(
            methods::LOOP_CREATE,
            json!({"session_id": "cli:s1", "command": "a", "input": "b"}),
        )
        .unwrap_err();
        assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
    }

    #[test]
    fn should_report_invalid_params_with_the_published_method_prefix() {
        let err =
            parse_autonomy_command(methods::SESSION_GOAL_SET, json!({"session_id": "cli:s1"}))
                .unwrap_err();
        assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
        assert!(
            err.message.starts_with("session/goal/set params: "),
            "unexpected error format: {}",
            err.message
        );
    }

    #[test]
    fn should_scope_task_artifact_aliases_to_an_explicit_session() {
        // `task_id` without `session_id` fails closed with the published
        // message (#1121); `agent_id` wins when both are set.
        let err =
            parse_autonomy_command(methods::TASK_ARTIFACT_LIST, json!({"task_id": "t1"})).unwrap();
        let AutonomyCommand::TaskArtifactList(params) = err else {
            panic!("wrong variant");
        };
        let err = params
            .resolve_agent_id(methods::TASK_ARTIFACT_LIST)
            .unwrap_err();
        assert_eq!(
            err.message,
            "task/artifact/list params: `task_id` requires `session_id` so artifact access can be scoped to the session that owns the task"
        );

        let command = parse_autonomy_command(
            methods::TASK_ARTIFACT_READ,
            json!({"agent_id": "a1", "task_id": "t1", "session_id": "cli:s1"}),
        )
        .unwrap();
        let AutonomyCommand::TaskArtifactRead(params) = command else {
            panic!("wrong variant");
        };
        assert_eq!(
            params
                .resolve_agent_id(methods::TASK_ARTIFACT_READ)
                .unwrap(),
            "a1"
        );

        let command = parse_autonomy_command(methods::TASK_ARTIFACT_LIST, json!({})).unwrap();
        let AutonomyCommand::TaskArtifactList(params) = command else {
            panic!("wrong variant");
        };
        let err = params
            .resolve_agent_id(methods::TASK_ARTIFACT_LIST)
            .unwrap_err();
        assert_eq!(
            err.message,
            "task/artifact/list params: missing field `agent_id` or `task_id`"
        );
    }
}
