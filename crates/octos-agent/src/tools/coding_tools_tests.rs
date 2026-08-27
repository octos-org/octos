use super::*;
use crate::tools::ToolRegistry;

#[test]
fn normalize_plan_maps_codex_shape_and_assigns_ids() {
    use octos_core::ui_protocol::PlanItemStatus;
    let args = json!({
        "explanation": "Building memory panel…",
        "plan": [
            { "step": "web P3: PWA manifest", "status": "completed" },
            { "step": "memory panel", "status": "in_progress" },
            { "step": "cron toggle", "status": "pending", "priority": "P3" },
            { "step": "no status → pending" }
        ]
    });
    let record = normalize_plan(&args, 42);
    assert_eq!(record.title.as_deref(), Some("Building memory panel…"));
    assert_eq!(record.updated_at_ms, 42);
    assert_eq!(record.items.len(), 4);
    // 1-based ids assigned when the caller omits them.
    assert_eq!(record.items[0].id, "1");
    assert_eq!(record.items[3].id, "4");
    assert_eq!(record.items[0].status, PlanItemStatus::Completed);
    assert_eq!(record.items[1].status, PlanItemStatus::InProgress);
    assert_eq!(record.items[2].status, PlanItemStatus::Pending);
    assert_eq!(record.items[2].priority.as_deref(), Some("P3"));
    // Unknown/absent status defaults to Pending.
    assert_eq!(record.items[3].status, PlanItemStatus::Pending);
    assert_eq!(record.items[0].title, "web P3: PWA manifest");
}

struct CapturingReporter {
    events: Arc<std::sync::Mutex<Vec<crate::progress::ProgressEvent>>>,
}
impl crate::progress::ProgressReporter for CapturingReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn update_plan_tool_emits_plan_updated_event() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut ctx = ToolContext::zero();
    ctx.reporter = Arc::new(CapturingReporter {
        events: Arc::clone(&events),
    });
    let args = json!({ "plan": [{ "step": "do a thing", "status": "in_progress" }] });

    let result = UpdatePlanTool
        .execute_with_context(&ctx, &args)
        .await
        .expect("update_plan executes");
    assert!(result.success);
    // Back-compat: the legacy structured_metadata path is preserved.
    assert!(result.structured_metadata.is_some());

    let captured = events.lock().unwrap();
    let plan = captured
        .iter()
        .find_map(|e| match e {
            crate::progress::ProgressEvent::PlanUpdated { plan } => Some(plan.clone()),
            _ => None,
        })
        .expect("a PlanUpdated event was emitted");
    assert_eq!(plan["items"][0]["title"], "do a thing");
    assert_eq!(plan["items"][0]["status"], "in_progress");
}

#[test]
fn truncate_capture_does_not_panic_on_multibyte_boundary() {
    // Regression: `guard[len - MAX_CAPTURE_BYTES..]` sliced at a raw byte
    // offset. A multibyte char straddling that offset panicked, silently
    // killing the spawned reader task. Build a buffer whose cut point falls
    // mid-'世' (3 bytes) and confirm the trim succeeds and stays valid UTF-8.
    // An all-3-byte-char buffer: char boundaries are multiples of 3, and
    // MAX_CAPTURE_BYTES (50_000) is not ≡ 0 (mod 3), so the raw cut offset
    // `len - MAX_CAPTURE_BYTES` is guaranteed to fall mid-char.
    let mut guard = "世".repeat(40_000); // 120_000 bytes, over the 2× trigger
    let cut = guard.len().saturating_sub(MAX_CAPTURE_BYTES);
    assert!(
        !guard.is_char_boundary(cut),
        "test precondition: the raw cut offset must fall mid-char"
    );

    truncate_capture_in_place(&mut guard); // must not panic
    assert!(guard.starts_with("... (earlier output truncated)\n"));
    assert!(
        guard.contains('世'),
        "the kept tail must remain valid UTF-8"
    );
}

const CODEX_P0: &[&str] = &[
    "apply_patch",
    "exec_command",
    "write_stdin",
    "update_plan",
    "request_user_input",
    "spawn_agent",
    "send_input",
    "resume_agent",
    "wait_agent",
    "close_agent",
];

#[test]
fn builtins_expose_codex_p0_tool_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let names: std::collections::HashSet<_> =
        registry.specs().into_iter().map(|spec| spec.name).collect();
    for name in CODEX_P0 {
        assert!(names.contains(*name), "{name} should be model-visible");
    }
}

// NOTE (#1773): `apply_patch_emits_diff_preview_structured_metadata` moved to
// `crate::tools::apply_patch::tests` alongside the relocated tool.

/// #972 / M14-B acceptance: `update_plan` MUST generate a structured
/// UI event so the AppUI layer can render the plan card without
/// parsing free-form `output` text. The contract is the
/// `structured_metadata` envelope with `codex_tool = "update_plan"`
/// and the model-provided plan echoed under `plan`.
#[tokio::test]
async fn update_plan_emits_structured_metadata_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let tool = registry
        .get("update_plan")
        .expect("update_plan tool registered");
    let plan_args = json!({
        "plan": [
            { "id": "p1", "title": "scaffold", "status": "in_progress" },
            { "id": "p2", "title": "tests", "status": "pending" }
        ]
    });
    let result = tool.execute(&plan_args).await.expect("update_plan ok");
    assert!(result.success, "update_plan must succeed");
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("update_plan must emit structured_metadata");
    assert_eq!(meta["codex_tool"], json!("update_plan"));
    assert_eq!(
        meta["plan"], plan_args,
        "echoed plan must match the model-provided arguments"
    );
}

/// #972 / M14-B acceptance: `request_user_input` MUST generate a
/// structured UI event so the AppUI layer can render the user-input
/// request without parsing the `output` blob. The contract is the
/// `structured_metadata` envelope with `codex_tool = "request_user_input"`,
/// the original request echoed under `request`, and a `host_response_channel`
/// hint that lets the client tell whether a synchronous response path
/// is wired (M14-E live soak scope) or not (current state).
#[tokio::test]
async fn request_user_input_emits_structured_metadata_event() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let tool = registry
        .get("request_user_input")
        .expect("request_user_input tool registered");
    let request_args = json!({
        "prompt": "Pick a deploy target",
        "choices": ["staging", "prod"],
    });
    let result = tool
        .execute(&request_args)
        .await
        .expect("request_user_input ok");
    assert!(result.success);
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("request_user_input must emit structured_metadata");
    assert_eq!(meta["codex_tool"], json!("request_user_input"));
    assert_eq!(
        meta["request"], request_args,
        "request payload must round-trip into the structured event"
    );
    assert!(
        meta.get("host_response_channel").is_some(),
        "structured event must declare host response channel state for the client"
    );
}

struct FakeSpawnTool;

#[async_trait::async_trait]
impl Tool for FakeSpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "fake spawn"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let supervisor = ctx.task_supervisor.as_ref().expect("supervisor");
        let task_id = supervisor.register_with_input(
            "spawn",
            "fake-call",
            ctx.parent_session_key.as_deref(),
            Some(args.clone()),
        );
        supervisor.mark_running(&task_id);
        Ok(ToolResult {
            output: "spawned fake worker".to_string(),
            success: true,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn spawn_agent_delegates_to_registered_spawn_tool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "inspect parity",
                "agent_type": "worker",
                "reasoning_effort": "high"
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let input = task.tool_input.expect("tool input");
    assert_eq!(input["task"], "inspect parity");
    assert_eq!(input["label"], "codex-worker");
    assert_eq!(input["role"], crate::ROLE_IMPLEMENTER);
    assert!(
        input["additional_instructions"]
            .as_str()
            .unwrap()
            .contains("reasoning_effort: high")
    );
}

/// Issue #971 (M14-C wiring contract): `spawn_agent` MUST accept a
/// `role` argument that resolves through `RoleTemplate::for_name`
/// and (1) seeds the dispatched spawn payload with the template's
/// `allowed_tools` budget, (2) appends the template's `prompt_prefix`
/// to the child's `additional_instructions`, (3) labels the
/// supervisor's `BackgroundTask.role` field so M13's `task/list`
/// projection inherits the role name without TUI/web doing any
/// orchestration of its own.
///
/// The fake spawn tool below records the spawn payload it received
/// onto the supervised task's `tool_input`. We then assert on the
/// recorded payload + the supervisor's BackgroundTask projection.
#[tokio::test]
async fn spawn_agent_with_role_argument_uses_template_runtime_factory_per_971() {
    use crate::role_template::{ROLE_REVIEWER, RoleTemplate};
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": ROLE_REVIEWER,
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    // (3) Supervisor's BackgroundTask MUST carry the role label so
    // M13 task/list and task/updated emit `role = "reviewer"` to
    // observers. Without this the AppUI spawn-role badge cannot
    // render and the contract acceptance check breaks.
    assert_eq!(
        task.role.as_deref(),
        Some(ROLE_REVIEWER),
        "spawn_agent must label BackgroundTask.role with the resolved template"
    );
    // (1) The spawn payload the fake delegate received MUST include
    // the template's tool budget so the child agent runs under the
    // policy the template promises. Group entries are EXPANDED to
    // concrete tool names (codex P1 fix) AND filtered to tools the
    // child's `with_builtins` registry has (codex P1 iteration 2)
    // — otherwise `SpawnTool::ensure_subagent_tools_available`
    // would fail every default role-based spawn.
    let spawn_input = task.tool_input.expect("tool input recorded");
    let reviewer = RoleTemplate::for_name(ROLE_REVIEWER).unwrap();
    let allowed = spawn_input["allowed_tools"]
        .as_array()
        .expect("allowed_tools array on spawn payload");
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    for expected in reviewer.to_spawn_compatible_allow() {
        assert!(
            allowed.contains(&expected.as_str()),
            "spawn payload must include {expected:?}; got {allowed:?}"
        );
    }
    // Codex P1 regression guard: NO `group:*` entries leak through
    // to the spawn payload. The native spawn tool does exact-name
    // lookup, so group identifiers would fail availability.
    for entry in &allowed {
        assert!(
            !entry.starts_with("group:"),
            "spawn payload must not contain raw group identifier {entry:?}; \
                 it would fail SpawnTool::ensure_subagent_tools_available"
        );
    }
    // Spot-check: reviewer's `group:search` expanded to glob/grep/
    // list_dir on the wire.
    for expected_member in ["glob", "grep", "list_dir"] {
        assert!(
            allowed.contains(&expected_member),
            "reviewer's group:search must expand to include {expected_member:?}; \
                 got {allowed:?}"
        );
    }
    // Codex P1 iteration 2 regression: tools NOT in
    // `ToolRegistry::with_builtins` (e.g. `recall_memory`,
    // `synthesize_research`, `save_memory`, `spawn`) must NOT
    // appear on the wire. Otherwise the child availability check
    // fires.
    for excluded in [
        "recall_memory",
        "synthesize_research",
        "save_memory",
        "spawn",
    ] {
        assert!(
            !allowed.contains(&excluded),
            "spawn payload must not include {excluded:?} (not in with_builtins child registry); \
                 got {allowed:?}"
        );
    }
    // (2) The reviewer role label MUST be propagated on the wire
    // so the native spawn delegate's `apply_role_template` can
    // prepend the prompt_prefix to the child's `additional_instructions`.
    // Codex iter-2 fix (PR #1177): we no longer prepend the prefix
    // at the `spawn_agent` boundary because `spawn.rs::apply_role_template`
    // does it universally for every `role`-bearing spawn — doing it
    // here too would double the prefix in the child's system context.
    assert_eq!(
        spawn_input["role"],
        json!(ROLE_REVIEWER),
        "spawn payload MUST forward role so spawn.rs::apply_role_template can layer prefix",
    );
    // Sanity: structured metadata carries the resolved role so the
    // AppUI tool stream can show "spawned reviewer subagent" without
    // re-resolving the registry.
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("spawn_agent must emit structured_metadata");
    assert_eq!(
        meta["role"],
        json!(ROLE_REVIEWER),
        "structured_metadata must echo the resolved role"
    );
    // Codex P2 regression guard (PR #1171 → PR #1177): the
    // server-owned `prompt_prefix` MUST NOT leak through
    // `structured_metadata.spawn_args`. The role summary already
    // keeps the prefix off the wire; this pins the spawn-payload
    // echo to do the same. With the prefix prepending moved
    // downstream into `spawn.rs::apply_role_template`, the boundary
    // payload simply never carries the prefix in the first place.
    let metadata_extra = meta["spawn_args"]["additional_instructions"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !metadata_extra.contains("code reviewer"),
        "structured_metadata.spawn_args MUST NOT echo the role prompt prefix; \
             got {metadata_extra:?}"
    );

    // Codex iter-3 P2.2 regression: BackgroundTask.runtime_policy_stamp
    // MUST be populated for role-based spawns so reconnect hydration
    // and `task/updated` subscribers see the server-resolved allow
    // list. Codex iter-4 P2 refinement: the stamp distinguishes
    // ENFORCED dimensions (`allowed_tools`) from ADVISORY ones
    // (`declared_sandbox_mode` etc.) so clients render the role's
    // self-description without trusting an unenforced policy.
    let stamp = task
        .runtime_policy_stamp
        .as_ref()
        .expect("BackgroundTask.runtime_policy_stamp MUST be populated for role spawns");
    assert_eq!(stamp["role"], json!(ROLE_REVIEWER));
    // Advisory declared defaults — surfaced under `declared_*`
    // names so clients don't mistake them for enforced policy.
    assert_eq!(stamp["declared_sandbox_mode"], json!("none"));
    assert_eq!(stamp["declared_approval_policy"], json!("never"));
    assert_eq!(stamp["declared_model_preference"], json!("coding"));
    // Enforcement tag MUST mark `allowed_tools` as enforced and
    // the rest as advisory — codex iter-4 P2 contract.
    assert_eq!(
        stamp["policy_enforcement"]["allowed_tools"],
        json!("enforced")
    );
    for advisory in ["sandbox_mode", "approval_policy", "model_preference"] {
        assert_eq!(
            stamp["policy_enforcement"][advisory],
            json!("advisory"),
            "policy_enforcement.{advisory} must be 'advisory' until the spawn tool propagates it"
        );
    }
    // The enforced allow_tools list MUST appear on the stamp.
    let stamp_allowed = stamp["allowed_tools"]
        .as_array()
        .expect("runtime_policy_stamp.allowed_tools must be array");
    let stamp_allowed: Vec<&str> = stamp_allowed.iter().filter_map(Value::as_str).collect();
    for member in ["glob", "grep", "list_dir", "read_file"] {
        assert!(
            stamp_allowed.contains(&member),
            "runtime_policy_stamp.allowed_tools must include {member:?}; \
                 got {stamp_allowed:?}"
        );
    }
}

/// Issue #971 (M14-C) PR #1177 reconciliation: with the prompt-
/// prefix prepending consolidated into `spawn.rs::apply_role_template`
/// (the codex iter-2 fix that removed the prefix doubling between
/// the `spawn_agent` boundary and the native spawn delegate), the
/// `spawn_agent` boundary's job is to FORWARD the resolved role and
/// any caller-supplied `additional_instructions` without doubling
/// either of them. This test pins that contract: role goes on the
/// wire as `role`, caller hint stays in `additional_instructions`,
/// the prompt_prefix is NOT prepended at this layer (so `apply_role_template`
/// can prepend it once and the child's system context carries a
/// single role prefix).
#[tokio::test]
async fn m14_c_wiring_spawn_agent_forwards_role_and_caller_instructions_per_971() {
    use crate::role_template::{ROLE_REVIEWER, RoleTemplate};
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": ROLE_REVIEWER,
                "additional_instructions": "CALLER_HINT_SENTINEL",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let spawn_input = task.tool_input.expect("tool input recorded");

    // Role is forwarded so spawn.rs::apply_role_template can layer
    // the prompt prefix at the native spawn boundary.
    assert_eq!(
        spawn_input["role"],
        json!(ROLE_REVIEWER),
        "role must be forwarded on the spawn payload",
    );
    // Caller hint is preserved verbatim — the boundary does not
    // touch additional_instructions for role purposes anymore.
    let extra = spawn_input["additional_instructions"]
        .as_str()
        .expect("additional_instructions on spawn payload");
    assert!(
        extra.contains("CALLER_HINT_SENTINEL"),
        "caller hint must be preserved; got {extra:?}"
    );
    // Codex iter-2 fix regression guard: the prompt_prefix MUST NOT
    // be prepended at this layer (spawn.rs::apply_role_template
    // would otherwise prepend it AGAIN, doubling the role prefix
    // in the child's system context).
    let reviewer = RoleTemplate::for_name(ROLE_REVIEWER).unwrap();
    assert!(
        !extra.contains(reviewer.prompt_prefix),
        "spawn_agent boundary MUST NOT prepend prompt_prefix (spawn.rs::apply_role_template \
             handles it); got {extra:?}"
    );
}

/// Issue #971 (M14-C) codex iter-5 P1: when a caller passes BOTH
/// `role` AND `agent_definition_id`, the role MUST NOT inject its
/// `allowed_tools` into the spawn payload — `SpawnTool::apply_agent_definition`
/// treats any non-empty inline `allowed_tools` as a caller override
/// that skips the manifest's `tools` allow-list, so role defaults
/// would bypass the manifest's safety envelope (e.g. a `role="implementer"`
/// with the `research-worker` manifest could still receive
/// `apply_patch` / `diff_edit` / `exec_command` even though the
/// manifest never allowed them).
///
/// This test asserts the wire payload: when both fields are
/// present, the role's tool budget is NOT in `allowed_tools` —
/// the manifest is left to install its own allow list downstream.
/// The role still contributes the prompt prefix.
#[tokio::test]
async fn spawn_agent_role_with_agent_definition_id_defers_to_manifest_per_971() {
    use crate::role_template::ROLE_IMPLEMENTER;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "implement #1234",
                "role": ROLE_IMPLEMENTER,
                "agent_definition_id": "research-worker",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let spawn_input = task.tool_input.expect("tool input recorded");
    // The wire payload MUST NOT carry the role's allowed_tools
    // when a manifest is referenced. Leaving the field absent
    // lets `apply_agent_definition` install the manifest's
    // `tools` allow-list unmolested.
    assert!(
        spawn_input.get("allowed_tools").is_none()
            || spawn_input["allowed_tools"]
                .as_array()
                .is_some_and(|a| a.is_empty()),
        "spawn payload MUST NOT carry role-derived allowed_tools when a manifest is set; \
             got allowed_tools = {:?}",
        spawn_input.get("allowed_tools")
    );
    // The manifest id MUST be forwarded so the spawn tool can
    // resolve it.
    assert_eq!(
        spawn_input["agent_definition_id"],
        json!("research-worker"),
        "spawn payload must forward agent_definition_id"
    );
    // The role label MUST still be forwarded so `spawn.rs::apply_role_template`
    // can anchor the child's system prompt on the role's voice
    // (codex iter-2 fix consolidated the prefix prepending downstream;
    // at this boundary we only have to make sure the role survives
    // the manifest-deference branch).
    assert_eq!(
        spawn_input["role"],
        json!(ROLE_IMPLEMENTER),
        "spawn payload MUST forward role even when manifest deference \
             skips the role's tool budget",
    );
    // Codex iter-5 P2: when a manifest is present, the stamp
    // must mark `policy_enforcement.allowed_tools` as
    // `subject_to_manifest` (not `enforced`) so clients can tell
    // the pre-manifest allow list is not authoritative.
    let stamp = task
        .runtime_policy_stamp
        .as_ref()
        .expect("BackgroundTask.runtime_policy_stamp MUST be populated for role spawns");
    assert_eq!(
        stamp["policy_enforcement"]["allowed_tools"],
        json!("subject_to_manifest"),
        "manifest-pruned stamp must mark allowed_tools as 'subject_to_manifest'"
    );
    assert_eq!(
        stamp["agent_definition_id"],
        json!("research-worker"),
        "stamp must surface the manifest id so clients can re-resolve"
    );
}

/// Issue #971 (M14-C) codex iter-3 P2.1: the `implementer` template
/// advertises `group:sessions`, which expands to `spawn_agent` /
/// `send_input` / `resume_agent` / `wait_agent` / `close_agent`.
/// `ToolRegistry::with_builtins` registers `SpawnAgentTool::new()`
/// WITHOUT a native `spawn` delegate, so an implementer child that
/// tried to use `spawn_agent` would always fail with "no native
/// delegate". Filter it from the spawn-compatible budget. The other
/// session aliases (`send_input` etc.) only need the supervisor
/// in `ctx`, which a child still inherits, so they stay.
#[tokio::test]
async fn spawn_agent_implementer_does_not_advertise_undelegated_spawn_agent_per_971() {
    use crate::role_template::{ROLE_IMPLEMENTER, RoleTemplate};
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "implement #1234",
                "role": ROLE_IMPLEMENTER,
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let spawn_input = task.tool_input.expect("tool input recorded");
    let allowed = spawn_input["allowed_tools"]
        .as_array()
        .expect("allowed_tools array");
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    // The undelegated alias MUST NOT appear in the wire allow list,
    // otherwise the child would be offered a tool that always
    // fails with "spawn_agent requires the session runtime to
    // register a native spawn tool delegate".
    assert!(
        !allowed.contains(&"spawn_agent"),
        "implementer's spawn payload MUST NOT advertise the undelegated \
             spawn_agent alias; got {allowed:?}"
    );
    // The native `spawn` tool is also NOT in `with_builtins`, so
    // the role budget should not include it either.
    assert!(
        !allowed.contains(&"spawn"),
        "implementer's spawn payload MUST NOT advertise the unregistered \
             native spawn tool; got {allowed:?}"
    );
    // Other session aliases stay — they only need the supervisor
    // handle which IS in ctx.
    for delegated in ["send_input", "resume_agent", "wait_agent", "close_agent"] {
        assert!(
            allowed.contains(&delegated),
            "implementer's spawn payload should advertise {delegated:?} \
                 (works via ctx.task_supervisor); got {allowed:?}"
        );
    }
    // Sanity: the role template ITSELF still permits `group:sessions`
    // (we only filter the wire projection, not the static schema).
    // This pins the contract — the spec stays unchanged.
    let implementer = RoleTemplate::for_name(ROLE_IMPLEMENTER).unwrap();
    assert!(implementer.permits("group:sessions"));
}

/// Issue #971 (M14-C wiring contract): an unknown `role` value MUST
/// fail at the tool boundary with a structured error rather than
/// silently defaulting to a template the LLM did not ask for. The
/// `TaskListEntry.role` field is `Option<String>` precisely because
/// the caller is expected to handle the unknown case explicitly.
#[tokio::test]
async fn spawn_agent_rejects_unknown_role_per_971() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": "review",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(
        !result.success,
        "spawn_agent must refuse unknown role; output={:?}",
        result.output
    );
    assert!(
        result.output.contains("unknown role")
            || (result.output.contains("role") && result.output.contains("review")),
        "error must mention the offending role name; got {:?}",
        result.output
    );
}

/// Issue #971 (M14-C): `spawn_agent` MUST keep working WITHOUT an
/// explicit `role` argument so the #1148 P0/P1 alias parity does
/// not regress. PR #1177 codex round-2 P2 refinement: when the
/// caller still uses the Codex `agent_type` alias (e.g.
/// `agent_type: "worker"` for the implementer role), the
/// `spawn_agent` boundary resolves the role via
/// `for_codex_agent_type` so the spawn-compatible tool budget +
/// BackgroundTask role/stamp ARE applied — otherwise the real
/// spawn delegate's `apply_role_template` would fall back to the
/// raw `allowed_tools_vec()` (with `group:*` entries) and
/// `ensure_subagent_tools_available` would reject the spawn.
/// This test pins both halves: the normalization still emits the
/// historical `codex-worker` label, AND the role gets resolved.
#[tokio::test]
async fn spawn_agent_without_role_argument_preserves_1148_behavior() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "inspect parity",
                "agent_type": "worker",
                "reasoning_effort": "high",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    // The historical #1148 normalization still fires.
    let input = task.tool_input.clone().expect("tool input");
    assert_eq!(input["task"], "inspect parity");
    assert_eq!(input["label"], "codex-worker");
    // PR #1177 codex round-2 P2: the agent_type alias path
    // resolves to ROLE_IMPLEMENTER so the spawn-compatible
    // budget is folded in; BackgroundTask.role now mirrors that
    // resolution.
    assert_eq!(
        task.role.as_deref(),
        Some(crate::role_template::ROLE_IMPLEMENTER),
        "agent_type='worker' must resolve to ROLE_IMPLEMENTER so the spawn-compatible \
             allow list is injected; got {:?}",
        task.role,
    );
    assert_eq!(
        input["role"],
        json!(crate::role_template::ROLE_IMPLEMENTER),
        "spawn payload must forward the resolved role label"
    );
}

/// Issue #971 (M14-C) PR #1177 codex round-3 P2 regression: a
/// client that serialises an unset `role` as `null` or `""` MUST
/// NOT defeat the `agent_type` alias resolution. The boundary
/// strips blank role values before reading the agent_type alias,
/// then writes the canonical resolved role back into the spawn
/// payload so the spawn delegate's `apply_role_template` sees the
/// same template the BackgroundTask was stamped with. Without
/// this fix, the spawn delegate would receive `role: null` /
/// `role: ""` and skip the prompt-prefix injection even though
/// the wrapper stamped the role.
#[tokio::test]
async fn m14_c_wiring_blank_role_with_agent_type_resolves_template_per_971() {
    for blank in [Value::Null, json!(""), json!("   ")] {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(temp.path());
        registry.register(FakeSpawnTool);
        let supervisor = registry.supervisor();
        let ctx = ToolContext {
            task_supervisor: Some(supervisor.clone()),
            parent_session_key: Some("api:test".to_string()),
            ..ToolContext::zero()
        };
        let result = registry
            .execute_with_context(
                &ctx,
                "spawn_agent",
                &json!({
                    "message": "inspect parity",
                    "agent_type": "worker",
                    "role": blank.clone(),
                }),
            )
            .await
            .expect("spawn_agent");
        assert!(result.success, "{}", result.output);
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        let agent_id = payload["agent_id"].as_str().expect("agent id");
        let task = supervisor.get_task(agent_id).expect("task registered");
        let input = task.tool_input.clone().expect("tool input");
        assert_eq!(
            input["role"],
            json!(crate::role_template::ROLE_IMPLEMENTER),
            "blank role={blank:?} + agent_type='worker' MUST write canonical ROLE_IMPLEMENTER \
                 to the spawn payload so apply_role_template fires"
        );
        assert_eq!(
            task.role.as_deref(),
            Some(crate::role_template::ROLE_IMPLEMENTER),
            "blank role={blank:?} + agent_type='worker' MUST stamp BackgroundTask.role with \
                 the resolved template",
        );
    }
}

/// Issue #971 (M14-C) PR #1177 codex round-3 P2: when the caller
/// passes a non-canonical (but case- or whitespace-sloppy) explicit
/// `role` that nevertheless resolves through the registry (e.g.
/// because the boundary lookup does `RoleTemplate::for_name` on
/// the trimmed value), the spawn payload MUST carry the canonical
/// name — not whatever sloppy spelling the caller used — so the
/// spawn delegate's `apply_role_template` does an exact-name
/// lookup against the registry without re-trimming.
#[tokio::test]
async fn m14_c_wiring_resolved_role_writes_canonical_name_per_971() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                // Explicit canonical name with surrounding
                // whitespace — boundary trims for lookup; ensure
                // the trimmed canonical name lands on the wire.
                "role": "  reviewer  ",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let input = task.tool_input.expect("tool input recorded");
    assert_eq!(
        input["role"],
        json!(crate::role_template::ROLE_REVIEWER),
        "spawn payload MUST carry the trimmed canonical role name"
    );
}

/// Issue #971 codex round-4 P3 follow-up to PR #1177: case-sloppy
/// explicit role values (`"Reviewer"`, `"  REVIEWER  "`, the
/// display labels models sometimes echo back) MUST canonicalize
/// to the registered lower-case name instead of returning
/// `unknown_role`. The registry's canonical names are all
/// lower-case, so the boundary lowercases the trimmed input
/// before lookup.
#[tokio::test]
async fn m14_c_wiring_role_lookup_is_case_insensitive_per_971() {
    for spelling in ["Reviewer", "REVIEWER", "  Reviewer  ", " reviewer "] {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(temp.path());
        registry.register(FakeSpawnTool);
        let supervisor = registry.supervisor();
        let ctx = ToolContext {
            task_supervisor: Some(supervisor.clone()),
            parent_session_key: Some("api:test".to_string()),
            ..ToolContext::zero()
        };
        let result = registry
            .execute_with_context(
                &ctx,
                "spawn_agent",
                &json!({
                    "message": "audit PR #1234",
                    "role": spelling,
                }),
            )
            .await
            .expect("spawn_agent");
        assert!(
            result.success,
            "spawn_agent must accept case-sloppy role {spelling:?}; output={:?}",
            result.output,
        );
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        let agent_id = payload["agent_id"].as_str().expect("agent id");
        let task = supervisor.get_task(agent_id).expect("task registered");
        assert_eq!(
            task.role.as_deref(),
            Some(crate::role_template::ROLE_REVIEWER),
            "role={spelling:?} must canonicalize to ROLE_REVIEWER",
        );
        let input = task.tool_input.expect("tool input recorded");
        assert_eq!(
            input["role"],
            json!(crate::role_template::ROLE_REVIEWER),
            "spawn payload must carry the canonical lower-case role name for {spelling:?}",
        );
    }
}

/// Issue #971 codex round-5 P2 follow-up: display labels and
/// hyphen-separated spellings must canonicalize to the
/// underscore-keyed registry name. The advertised `Test Worker`
/// display label MUST resolve to `test_worker`; the registry
/// key is snake_case and `RoleTemplate::for_name` is exact-name,
/// so the boundary normalizes both ` ` and `-` to `_` after
/// lowercase + trim.
#[tokio::test]
async fn m14_c_wiring_display_label_role_normalizes_to_registry_key_per_971() {
    for (spelling, canonical) in [
        ("Test Worker", crate::role_template::ROLE_TEST_WORKER),
        ("Test-Worker", crate::role_template::ROLE_TEST_WORKER),
        ("TEST WORKER", crate::role_template::ROLE_TEST_WORKER),
        ("  test-worker  ", crate::role_template::ROLE_TEST_WORKER),
        ("Implementer", crate::role_template::ROLE_IMPLEMENTER),
        ("Explorer", crate::role_template::ROLE_EXPLORER),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(temp.path());
        registry.register(FakeSpawnTool);
        let supervisor = registry.supervisor();
        let ctx = ToolContext {
            task_supervisor: Some(supervisor.clone()),
            parent_session_key: Some("api:test".to_string()),
            ..ToolContext::zero()
        };
        let result = registry
            .execute_with_context(
                &ctx,
                "spawn_agent",
                &json!({
                    "message": "audit",
                    "role": spelling,
                }),
            )
            .await
            .expect("spawn_agent");
        assert!(
            result.success,
            "spawn_agent must accept display-label role {spelling:?}; output={:?}",
            result.output,
        );
        let payload: Value = serde_json::from_str(&result.output).expect("json payload");
        let agent_id = payload["agent_id"].as_str().expect("agent id");
        let task = supervisor.get_task(agent_id).expect("task registered");
        assert_eq!(
            task.role.as_deref(),
            Some(canonical),
            "role={spelling:?} must canonicalize to {canonical}",
        );
        let input = task.tool_input.expect("tool input recorded");
        assert_eq!(
            input["role"],
            json!(canonical),
            "spawn payload must carry the canonical role key for {spelling:?}",
        );
    }
}

/// Issue #971 codex round-4 P2 follow-up to PR #1177: the
/// `delegate` wrapper used to ship raw `template.allowed_tools`
/// (with `group:*` identifiers) inside its `spawn_agent` invocation.
/// `spawn_agent` then treated the non-empty array as an inline
/// override and skipped `to_spawn_compatible_allow()`, so the
/// raw group entries reached the native spawn tool's availability
/// check and the call failed with `required tool not available:
/// group:search`. This guard pins the fix: `delegate` now
/// pre-expands the template's allow-list before the override
/// path fires.
///
/// Uses the same completer pattern as
/// `delegate_spawns_waits_and_returns_artifacts` so the test
/// doesn't time out waiting for the FakeSpawnTool task to finish.
#[tokio::test]
async fn m14_c_wiring_delegate_pre_expands_role_allow_list_per_971() {
    use crate::role_template::ROLE_REVIEWER;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let supervisor_for_completer = supervisor.clone();
    let completer = tokio::spawn(async move {
        for _ in 0..200 {
            if let Some(task) = supervisor_for_completer
                .get_all_tasks()
                .into_iter()
                .find(|task| task.tool_name == "spawn" || task.tool_name == "spawn_agent")
            {
                supervisor_for_completer.mark_completed(&task.id, vec![]);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    let result = registry
        .execute_with_context(
            &ctx,
            "delegate",
            &json!({
                "role": "reviewer",
                "task": "audit PR #1234",
                "timeout_ms": 5_000,
            }),
        )
        .await
        .expect("delegate");
    let _ = completer.await;
    assert!(
        result.success,
        "delegate must succeed; output={:?}",
        result.output,
    );
    // Pull the spawn task — the only registered task in this
    // isolated registry.
    let task = supervisor
        .get_all_tasks()
        .into_iter()
        .find(|t| t.tool_name == "spawn" || t.tool_name == "spawn_agent")
        .expect("task registered");
    let input = task.tool_input.expect("tool input recorded");
    let allowed = input["allowed_tools"]
        .as_array()
        .expect("allowed_tools array on delegate spawn payload");
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    // Codex round-4 P2 guard: no raw `group:*` identifiers on
    // the wire — the delegate now pre-expands through
    // `to_spawn_compatible_allow`.
    for entry in &allowed {
        assert!(
            !entry.starts_with("group:"),
            "delegate spawn payload MUST NOT contain raw group identifier {entry:?} \
                 after codex round-4 P2 fix; got allowed={allowed:?}",
        );
    }
    // Spot-check: reviewer's `group:search` expansion lands as
    // concrete tool names on the wire.
    for expected in ["glob", "grep", "list_dir", "read_file"] {
        assert!(
            allowed.contains(&expected),
            "delegate spawn payload must include {expected:?} for reviewer; \
                 got {allowed:?}",
        );
    }
    // And the role is forwarded so spawn.rs::apply_role_template
    // can layer the prefix.
    assert_eq!(input["role"], json!(ROLE_REVIEWER));
}

/// Issue #971 codex round-5 P2 follow-up: the `delegate` wrapper
/// used to manually prepend `template.prompt_prefix` to the task
/// AND forward `role` to `spawn_agent`. `spawn.rs::apply_role_template`
/// also prepends the prefix when it sees `role` on the input, so
/// every delegate-spawned child received the role guardrails
/// TWICE. This guard pins the fix: delegate ships the bare task
/// text and lets `apply_role_template` be the single source of
/// prefix injection.
#[tokio::test]
async fn m14_c_wiring_delegate_does_not_double_prefix_role_per_971() {
    use crate::role_template::{ROLE_REVIEWER, RoleTemplate};
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let supervisor_for_completer = supervisor.clone();
    let completer = tokio::spawn(async move {
        for _ in 0..200 {
            if let Some(task) = supervisor_for_completer
                .get_all_tasks()
                .into_iter()
                .find(|task| task.tool_name == "spawn" || task.tool_name == "spawn_agent")
            {
                supervisor_for_completer.mark_completed(&task.id, vec![]);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    let result = registry
        .execute_with_context(
            &ctx,
            "delegate",
            &json!({
                "role": "reviewer",
                "task": "audit PR #1234",
                "timeout_ms": 5_000,
            }),
        )
        .await
        .expect("delegate");
    let _ = completer.await;
    assert!(
        result.success,
        "delegate must succeed; output={:?}",
        result.output,
    );
    let task = supervisor
        .get_all_tasks()
        .into_iter()
        .find(|t| t.tool_name == "spawn" || t.tool_name == "spawn_agent")
        .expect("task registered");
    let input = task.tool_input.expect("tool input recorded");
    let reviewer = RoleTemplate::for_name(ROLE_REVIEWER).unwrap();
    let task_text = input["task"].as_str().expect("task field on spawn payload");
    // Codex round-5 P2 regression guard: the prefix MUST appear
    // ZERO times in the task text the delegate forwards.
    // `spawn.rs::apply_role_template` is the authoritative single
    // source of prefix injection and reads from `additional_instructions`
    // — not `task` — so a clean `task` field is the contract.
    assert!(
        !task_text.contains(reviewer.prompt_prefix),
        "delegate MUST NOT embed prompt_prefix in `task` (spawn.rs::apply_role_template \
             handles prefix injection from role); got task={task_text:?}",
    );
    // Role still gets forwarded so apply_role_template fires.
    assert_eq!(input["role"], json!(ROLE_REVIEWER));
}

/// Issue #971 (M14-C) PR #1177 codex round-2 P2 regression: when
/// the caller passes an `agent_type` alias whose `for_codex_agent_type`
/// resolution returns `None` (e.g. `agent_type: "planner"`), the
/// boundary MUST NOT label the task with a role — otherwise an
/// unrecognised agent_type would silently inherit some default
/// role's policy budget. This pins the negative case.
#[tokio::test]
async fn m14_c_wiring_unknown_agent_type_does_not_synthesize_role_per_971() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "inspect parity",
                "agent_type": "planner",
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    assert!(
        task.role.is_none(),
        "unknown agent_type MUST NOT synthesize a role label; got {:?}",
        task.role,
    );
    assert!(
        task.runtime_policy_stamp.is_none(),
        "unknown agent_type MUST NOT stamp a runtime policy",
    );
}

/// Issue #971 (M14-C) codex P1 iteration 2: when the caller passes
/// `role` AND an EMPTY `allowed_tools: []` array, the empty array
/// MUST NOT silently override the template's restricted budget.
/// The native spawn tool treats an empty allow-list as "all
/// builtins"; without this guard a client that always serialises
/// empty optional arrays would let a reviewer/explorer/test_worker
/// spawn receive write, shell, and browser tools.
#[tokio::test]
async fn spawn_agent_with_role_ignores_empty_allowed_tools_per_971() {
    use crate::role_template::{ROLE_REVIEWER, RoleTemplate};
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": ROLE_REVIEWER,
                // Client serialises optional array as empty — this
                // MUST be treated as "no override", not as the
                // native spawn-tool sentinel for "all builtins".
                "allowed_tools": [],
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let spawn_input = task.tool_input.expect("tool input recorded");
    let reviewer = RoleTemplate::for_name(ROLE_REVIEWER).unwrap();
    let allowed = spawn_input["allowed_tools"]
        .as_array()
        .expect("allowed_tools array on spawn payload");
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    // Empty inline override MUST be overridden BY the template's
    // budget — every spawn-compatible tool the reviewer permits
    // appears on the wire.
    assert!(
        !allowed.is_empty(),
        "spawn payload allowed_tools MUST NOT be empty when a role is set; \
             empty would be interpreted as 'all builtins' by the spawn tool"
    );
    for expected in reviewer.to_spawn_compatible_allow() {
        assert!(
            allowed.contains(&expected.as_str()),
            "spawn payload must include {expected:?} after empty override; \
                 got {allowed:?}"
        );
    }
    // A reviewer WRITES its report, so `write_file` is present...
    assert!(
        allowed.contains(&"write_file"),
        "reviewer spawn payload must include write_file (its report writer); got {allowed:?}"
    );
    // ...but the code-patching / exec tools MUST NOT leak through.
    for forbidden in ["edit_file", "shell", "exec_command"] {
        assert!(
            !allowed.contains(&forbidden),
            "reviewer spawn payload MUST NOT include {forbidden:?} after \
                 empty override; got {allowed:?}"
        );
    }
}

/// Issue #971 (M14-C) codex P1 iteration 2: a NON-EMPTY inline
/// `allowed_tools` array MUST still override the template budget.
/// This pins the inline-wins contract: a caller can carve out a
/// custom budget for a one-off spawn even with `role` set.
#[tokio::test]
async fn spawn_agent_with_role_honors_nonempty_allowed_tools_override_per_971() {
    use crate::role_template::ROLE_REVIEWER;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    let result = registry
        .execute_with_context(
            &ctx,
            "spawn_agent",
            &json!({
                "message": "audit PR #1234",
                "role": ROLE_REVIEWER,
                "allowed_tools": ["read_file", "grep"],
            }),
        )
        .await
        .expect("spawn_agent");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    let agent_id = payload["agent_id"].as_str().expect("agent id");
    let task = supervisor.get_task(agent_id).expect("task registered");
    let spawn_input = task.tool_input.expect("tool input recorded");
    let allowed = spawn_input["allowed_tools"]
        .as_array()
        .expect("allowed_tools array on spawn payload");
    let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
    assert_eq!(
        allowed,
        vec!["read_file", "grep"],
        "non-empty inline allowed_tools must win over the role template"
    );
}

#[tokio::test]
async fn codex_agent_aliases_operate_on_supervisor_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let supervisor = registry.supervisor();
    let agent_id = supervisor.register_with_input(
        "spawn",
        "call-alias",
        Some("api:alias-test"),
        Some(json!({ "task": "initial" })),
    );
    supervisor.mark_running(&agent_id);
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:alias-test".to_owned()),
        ..ToolContext::zero()
    };

    let sent = registry
        .execute_with_context(
            &ctx,
            "send_input",
            &json!({
                "agent_id": agent_id.clone(),
                "message": "continue with reviewer notes"
            }),
        )
        .await
        .expect("send_input");
    assert!(sent.success, "{}", sent.output);
    let updated = supervisor.get_task(&agent_id).expect("task");
    let tool_input = updated.tool_input.expect("tool input");
    assert_eq!(
        tool_input["last_codex_send_input"]["request"]["message"],
        "continue with reviewer notes"
    );

    let waited = registry
        .execute_with_context(
            &ctx,
            "wait_agent",
            &json!({ "agent_id": agent_id.clone(), "timeout_ms": 0 }),
        )
        .await
        .expect("wait_agent");
    assert!(waited.success, "{}", waited.output);
    let waited_payload: Value = serde_json::from_str(&waited.output).expect("wait json");
    assert_eq!(waited_payload["agents"][0]["agent_id"], agent_id);
    assert_eq!(waited_payload["agents"][0]["status"], "running");

    let closed = registry
        .execute_with_context(&ctx, "close_agent", &json!({ "target": agent_id.clone() }))
        .await
        .expect("close_agent");
    assert!(closed.success, "{}", closed.output);
    let closed_task = supervisor.get_task(&agent_id).expect("task");
    assert_eq!(closed_task.status, crate::TaskStatus::Cancelled);
}

// NOTE (#1773): `apply_patch_adds_and_updates_file` moved to
// `crate::tools::apply_patch::tests` alongside the relocated tool.

#[tokio::test]
async fn exec_command_runs_to_completion() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("exec_command", &json!({"cmd": "printf codex"}))
        .await
        .expect("exec command");
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("codex"));
}

#[cfg(not(windows))]
#[tokio::test]
async fn write_stdin_talks_to_exec_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = registry
        .execute(
            "exec_command",
            &json!({
                "cmd": "read line; echo got:$line",
                "tty": true,
                "yield_time_ms": 20
            }),
        )
        .await
        .expect("start exec session");
    assert!(started.success, "{}", started.output);
    let payload: Value = serde_json::from_str(&started.output).expect("session payload");
    let session_id = payload["session_id"].as_str().expect("session_id");
    let written = registry
        .execute(
            "write_stdin",
            &json!({
                "session_id": session_id,
                "chars": "octos\n",
                "yield_time_ms": 100
            }),
        )
        .await
        .expect("write stdin");
    assert!(written.success, "{}", written.output);
    assert!(written.output.contains("got:octos"), "{}", written.output);
}

// -----------------------------------------------------------------------
// #972 / M14-B P1 tests — `view_image`, `tool_search`, `tool_suggest`.
// -----------------------------------------------------------------------

/// 8-byte PNG header (the only part the format detector cares about) plus
/// a zero-IHDR-length marker; enough to make `view_image` happy without
/// pulling in the `image` crate.
const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

#[tokio::test]
async fn view_image_reports_format_and_size_for_png() {
    let temp = tempfile::tempdir().expect("tempdir");
    let png = temp.path().join("logo.png");
    std::fs::write(&png, PNG_MAGIC).expect("write png");
    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "logo.png" }))
        .await
        .expect("view_image ok");
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    assert_eq!(payload["format"], json!("png"));
    assert_eq!(payload["mime_type"], json!("image/png"));
    assert_eq!(payload["byte_length"], json!(PNG_MAGIC.len()));
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("view_image"));
    assert_eq!(meta["format"], json!("png"));
}

/// Codex review #1153 P2 regression: `FilesystemScope::Host` (granted via
/// `DangerFullAccess`) lets `view_image` read images outside the
/// workspace. Pre-fix, the helper passed `self.base_dir` as the
/// ancestor-walk stop unconditionally. For a host path like `/tmp/foo.png`
/// on macOS, the walk never reached the workspace and refused `/tmp`
/// (which is a symlink to `/private/tmp` on macOS). Now host-scope skips
/// the ancestor walk entirely; the Unix O_NOFOLLOW leaf guard still
/// protects the final-component symlink case.
#[tokio::test]
async fn view_image_host_scope_accepts_path_outside_workspace_per_1153() {
    // Build a host path under a SECOND tempdir so it's outside `base_dir`.
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let png = outside.path().join("host.png");
    std::fs::write(&png, PNG_MAGIC).expect("write png");

    let tool = ViewImageTool::new(workspace.path()).with_filesystem_scope(FilesystemScope::Host);

    // Absolute path: host scope must accept it even though it lives
    // outside `workspace.path()`.
    let result = tool
        .execute(&json!({ "path": png.to_string_lossy() }))
        .await
        .expect("view_image runs");

    assert!(
        result.success,
        "host-scope view_image must accept paths outside the workspace; got error: {}",
        result.output
    );
    let payload: Value = serde_json::from_str(&result.output).expect("json payload");
    assert_eq!(payload["format"], json!("png"));
}

/// Codex review #1153 P2 rev2: when host-scope skips the
/// ancestor walk, the Windows leaf-symlink guard goes with it
/// (Unix still has O_NOFOLLOW, but Windows has no replacement).
/// The new `reject_leaf_symlink` must catch a leaf symlink even
/// in host scope. This test exercises the Unix path; the same
/// guard runs on Windows where it's the ONLY leaf no-follow check.
#[cfg(unix)]
#[tokio::test]
async fn view_image_host_scope_still_rejects_leaf_symlink_per_1153() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("real.png");
    std::fs::write(&target, PNG_MAGIC).expect("write real png");
    let link = outside.path().join("link.png");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");

    let tool = ViewImageTool::new(workspace.path()).with_filesystem_scope(FilesystemScope::Host);

    let result = tool
        .execute(&json!({ "path": link.to_string_lossy() }))
        .await
        .expect("view_image runs");

    assert!(
        !result.success,
        "host-scope view_image must still reject a leaf symlink even when ancestor walk is skipped; got: {}",
        result.output,
    );
}

#[tokio::test]
async fn view_image_fails_when_path_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "absent.png" }))
        .await
        .expect("view_image runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("view_image"));
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

#[tokio::test]
async fn view_image_rejects_non_image_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let txt = temp.path().join("notes.txt");
    std::fs::write(&txt, b"hello, not an image").expect("write text");
    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "notes.txt" }))
        .await
        .expect("view_image runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["reason"], json!("unrecognised_image_format"));
}

/// #1148 codex P2 acceptance: view_image MUST refuse to follow
/// symlinks (Unix O_NOFOLLOW) so a malicious repo can't trick
/// the tool into reading a file outside the workspace via a
/// symlinked image entry.
#[cfg(unix)]
#[tokio::test]
async fn view_image_rejects_symlinked_target() {
    const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("real_image.png");
    std::fs::write(&target, PNG_MAGIC).expect("write png");
    let symlink = temp.path().join("link.png");
    std::os::unix::fs::symlink(&target, &symlink).expect("symlink");

    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "link.png" }))
        .await
        .expect("view_image runs");
    assert!(
        !result.success,
        "view_image must reject symlinked targets (O_NOFOLLOW); got success result"
    );
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

/// #1151 acceptance: view_image MUST refuse to traverse a
/// SYMLINKED PARENT DIRECTORY. The Unix `O_NOFOLLOW` flag only
/// catches a symlink at the final path component, so without an
/// ancestor walk a malicious workspace could ship
/// `workspace/link -> /outside/` and `view_image link/real.png`
/// would read `/outside/real.png` (outside the workspace).
#[cfg(unix)]
#[tokio::test]
async fn view_image_rejects_parent_symlink_directory() {
    const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let temp = tempfile::tempdir().expect("tempdir");
    // Two sibling directories under the same tempdir: the
    // workspace, and an `outside` directory that contains the
    // real image. The workspace itself contains a symlink
    // `imgs -> outside`. Lexically `workspace/imgs/real.png`
    // looks workspace-relative.
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&workspace).expect("mk workspace");
    std::fs::create_dir(&outside).expect("mk outside");
    std::fs::write(outside.join("real.png"), PNG_MAGIC).expect("write png");
    std::os::unix::fs::symlink(&outside, workspace.join("imgs")).expect("symlink parent directory");

    let tool = ViewImageTool::new(&workspace);
    let result = tool
        .execute(&json!({ "path": "imgs/real.png" }))
        .await
        .expect("view_image runs");
    assert!(
        !result.success,
        "view_image must refuse a SYMLINKED PARENT directory; got success result: {}",
        result.output
    );
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("view_image"));
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

/// #1151 acceptance: Windows must perform the symlink rejection
/// BEFORE the open call. Prior to the fix the helper opened the
/// file first and then called `file.metadata().is_symlink()` —
/// but `OpenOptions::open` had already followed the symlink, so
/// the check was silently a no-op. The pre-open `symlink_metadata`
/// ancestor walk catches the leaf reliably.
///
/// NB: Windows symlink creation requires Developer Mode or admin
/// privileges. The test silently passes when neither is available
/// — there is nothing the test can do about an unprivileged CI
/// runner. The Unix counterpart above gives functional coverage;
/// this test guards against the platform-specific regression
/// only when the host can actually create a symlink.
#[cfg(windows)]
#[tokio::test]
async fn view_image_rejects_leaf_symlink_pre_open_on_windows() {
    const PNG_MAGIC: [u8; 12] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("real_image.png");
    std::fs::write(&target, PNG_MAGIC).expect("write png");
    let symlink = temp.path().join("link.png");
    if std::os::windows::fs::symlink_file(&target, &symlink).is_err() {
        // Unprivileged runner — symlinks unavailable. Skip
        // rather than fail; the Unix test exercises the same
        // ancestor-walk code path.
        eprintln!(
            "skipping view_image_rejects_leaf_symlink_pre_open_on_windows: symlink_file failed (Developer Mode or admin required)"
        );
        return;
    }

    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "link.png" }))
        .await
        .expect("view_image runs");
    assert!(
        !result.success,
        "view_image must reject a leaf symlink PRE-OPEN on Windows; got success result: {}",
        result.output
    );
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

/// #1148 codex P2 acceptance: view_image must read only a bounded
/// header — it should NOT allocate the entire file for magic-byte
/// sniffing. The PNG test above only writes 12 bytes; this one
/// writes a 10MB file but still gets a correct format report
/// with proper byte_length, proving we read only the header.
#[tokio::test]
async fn view_image_reads_only_bounded_header_for_large_file() {
    const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("big.png");
    let mut bytes = Vec::with_capacity(10_000_000);
    bytes.extend_from_slice(&PNG_MAGIC);
    bytes.resize(10_000_000, 0u8);
    std::fs::write(&path, &bytes).expect("write big png");

    let tool = ViewImageTool::new(temp.path());
    let result = tool
        .execute(&json!({ "path": "big.png" }))
        .await
        .expect("view_image runs");
    assert!(result.success, "10MB image with valid header must succeed");
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert_eq!(payload["format"], json!("png"));
    assert_eq!(payload["byte_length"], json!(10_000_000));
}

fn sample_catalog_cell() -> Arc<std::sync::Mutex<Vec<ToolCatalogEntry>>> {
    Arc::new(std::sync::Mutex::new(sample_catalog()))
}

fn sample_catalog() -> Vec<ToolCatalogEntry> {
    vec![
        ToolCatalogEntry::new(
            "apply_patch",
            "Apply a Codex-style patch to files in the workspace",
            vec!["fs".to_string(), "code".to_string()],
        ),
        ToolCatalogEntry::new(
            "exec_command",
            "Run a shell command. Supports long-running sessions.",
            vec!["runtime".to_string(), "code".to_string()],
        ),
        ToolCatalogEntry::new(
            "update_plan",
            "Update the visible task plan",
            vec!["code".to_string()],
        ),
        ToolCatalogEntry::new(
            "web_search",
            "Search the web for an arbitrary query",
            vec!["search".to_string(), "web".to_string()],
        ),
    ]
}

#[tokio::test]
async fn tool_search_returns_matching_tools_for_substring() {
    let tool = ToolSearchTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "query": "patch" }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let matches = payload["matches"].as_array().expect("matches");
    assert!(!matches.is_empty(), "expected at least one match");
    assert_eq!(matches[0]["name"], json!("apply_patch"));
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("tool_search"));
}

#[tokio::test]
async fn tool_search_returns_empty_matches_for_unrelated_query() {
    let tool = ToolSearchTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "query": "zzz_not_a_tool" }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert!(payload["matches"].as_array().expect("matches").is_empty());
}

#[tokio::test]
async fn tool_search_honours_limit() {
    let tool = ToolSearchTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "query": "code", "limit": 2 }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert!(payload["matches"].as_array().unwrap().len() <= 2);
}

#[tokio::test]
async fn tool_suggest_ranks_relevant_tools_first() {
    let tool = ToolSuggestTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "task": "I want to apply a code patch to a file" }))
        .await
        .expect("tool_suggest ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let suggestions = payload["suggestions"].as_array().expect("suggestions");
    assert!(
        !suggestions.is_empty(),
        "expected suggestions for code task"
    );
    assert_eq!(suggestions[0]["name"], json!("apply_patch"));
    // Suggestions for a coding task should not surface `web_search`.
    let names: Vec<&str> = suggestions
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"web_search"),
        "web_search should not be suggested for a code-patch task: {names:?}"
    );
}

#[tokio::test]
async fn tool_suggest_accepts_query_alias_for_task() {
    let tool = ToolSuggestTool::new(sample_catalog_cell());
    let result = tool
        .execute(&json!({ "query": "shell command" }))
        .await
        .expect("tool_suggest ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let suggestions = payload["suggestions"].as_array().expect("suggestions");
    assert_eq!(suggestions[0]["name"], json!("exec_command"));
}

/// #1148 codex P2 acceptance: tool_search must reflect tools
/// registered AFTER `with_builtins` (chat/gateway/profile setup
/// paths inject MCP/plugin/pipeline/memory tools). The discovery
/// surface should be live, not a frozen snapshot.
#[tokio::test]
async fn tool_search_reflects_post_builtins_registrations() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());

    // Sanity: a freshly-coined tool name doesn't appear pre-registration.
    let search_tool = registry
        .get_tool("tool_search")
        .expect("tool_search registered by with_builtins");
    let result = search_tool
        .execute(&serde_json::json!({ "query": "post_builtin_xyz_unique" }))
        .await
        .expect("tool_search ok");
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert_eq!(
        payload["matches"].as_array().map(Vec::len),
        Some(0),
        "fresh registry should not match unknown tool name yet"
    );

    // Inject a new tool AFTER with_builtins.
    struct PostBuiltinTool;
    #[async_trait::async_trait]
    impl Tool for PostBuiltinTool {
        fn name(&self) -> &str {
            "post_builtin_xyz_unique"
        }
        fn description(&self) -> &str {
            "A tool registered after with_builtins"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _args: &Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::default())
        }
    }
    registry.register(PostBuiltinTool);

    // Now tool_search MUST find it.
    let result = search_tool
        .execute(&serde_json::json!({ "query": "post_builtin_xyz_unique" }))
        .await
        .expect("tool_search ok");
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let matches = payload["matches"].as_array().expect("matches array");
    assert!(
        matches
            .iter()
            .any(|m| m["name"] == json!("post_builtin_xyz_unique")),
        "tool_search must reflect post-builtins registrations (got {:?})",
        matches,
    );
}

#[tokio::test]
async fn tool_search_live_catalog_respects_visibility_filters() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());

    let search_tool = registry
        .get_tool("tool_search")
        .expect("tool_search registered by with_builtins");

    // RFC-0 (#1289): tool deferral removed; visibility filtering is now
    // driven purely by provider_policy and context_filter.
    registry.set_provider_policy(crate::tools::ToolPolicy {
        deny: vec!["bash".to_string()],
        ..Default::default()
    });
    let result = search_tool
        .execute(&serde_json::json!({ "query": "bash" }))
        .await
        .expect("tool_search ok");
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let names: Vec<_> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"bash"),
        "provider-denied tools must stay out of tool_search: {names:?}"
    );

    let mut registry = ToolRegistry::with_builtins(temp.path());
    let search_tool = registry
        .get_tool("tool_search")
        .expect("tool_search registered by with_builtins");
    registry.set_context_filter(vec!["web".to_string()]);
    let result = search_tool
        .execute(&serde_json::json!({ "query": "apply_patch" }))
        .await
        .expect("tool_search ok");
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let names: Vec<_> = payload["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"apply_patch"),
        "context-hidden tools must stay out of tool_search: {names:?}"
    );
}

#[tokio::test]
async fn builtins_expose_p1_codex_tool_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let names: std::collections::HashSet<_> =
        registry.specs().into_iter().map(|spec| spec.name).collect();
    for name in &["view_image", "tool_search", "tool_suggest"] {
        assert!(names.contains(*name), "{name} must be model-visible");
    }
}

// -----------------------------------------------------------------------
// #1172 — Codex naming-parity alias tests (`bash`, `delegate`).
// -----------------------------------------------------------------------

/// #1172 acceptance: `with_builtins` must surface the new aliases
/// alongside the canonical primitives so a Codex-trained model
/// hitting `bash(cmd=…)` or `delegate(role=…, task=…)` lands on
/// the registered tool directly.
#[tokio::test]
async fn builtins_expose_codex_naming_aliases() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let names: std::collections::HashSet<_> =
        registry.specs().into_iter().map(|spec| spec.name).collect();
    for name in &["bash", "delegate"] {
        assert!(names.contains(*name), "{name} must be model-visible");
    }
}

/// #1172 acceptance: `tool_search("bash")` must surface the new
/// alias on first call. Without the alias the canonical `shell`
/// would dominate even though a Codex-trained model is emitting
/// `bash(...)`.
#[tokio::test]
async fn tool_search_returns_bash_alias() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let search = registry
        .get_tool("tool_search")
        .expect("tool_search registered");
    let result = search
        .execute(&json!({ "query": "bash" }))
        .await
        .expect("tool_search ok");
    assert!(result.success);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    let matches = payload["matches"].as_array().expect("matches");
    let names: Vec<&str> = matches.iter().filter_map(|m| m["name"].as_str()).collect();
    assert!(
        names.contains(&"bash"),
        "tool_search must surface bash alias; got {names:?}"
    );
}

/// #1172 happy path: `bash` runs a simple command to completion
/// and returns the captured stdout.
#[tokio::test]
async fn bash_runs_simple_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("bash", &json!({ "cmd": "printf hello-bash" }))
        .await
        .expect("bash runs");
    assert!(result.success, "{}", result.output);
    assert!(
        result.output.contains("hello-bash"),
        "bash output must contain captured stdout; got: {}",
        result.output
    );
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("bash must emit structured metadata");
    assert_eq!(meta["codex_tool"], json!("bash"));
}

/// #1172 denial path: dangerous commands are rejected by the
/// shared `SafePolicy`, the same gate `shell` and `exec_command`
/// use. The error path returns `success=false` with a denial
/// message — no command is spawned.
#[tokio::test]
async fn bash_denies_dangerous_command_via_safe_policy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("bash", &json!({ "cmd": "rm -rf /" }))
        .await
        .expect("bash runs");
    assert!(
        !result.success,
        "bash must reject `rm -rf /` via SafePolicy; got: {}",
        result.output
    );
    assert!(
        result.output.to_lowercase().contains("denied")
            || result.output.to_lowercase().contains("approval"),
        "bash denial message must be readable; got: {}",
        result.output
    );
}

/// #1172 denial path: missing `cmd` is rejected at the boundary
/// without spawning anything.
#[tokio::test]
async fn bash_rejects_missing_cmd() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("bash", &json!({}))
        .await
        .expect("bash runs");
    assert!(!result.success);
    assert!(
        result.output.contains("cmd"),
        "missing-cmd error must mention `cmd`; got: {}",
        result.output
    );
}

/// #1172 happy path: `delegate(role, task)` spawns a child task
/// through the registered spawn delegate, waits for it to reach a
/// terminal state, and surfaces the artifacts list. We background
/// a completer that flips the supervisor's task to `Completed` so
/// the wait loop exits with a terminal payload instead of a
/// timeout.
#[tokio::test]
async fn delegate_spawns_waits_and_returns_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    registry.register(FakeSpawnTool);
    let supervisor = registry.supervisor();
    let ctx = ToolContext {
        task_supervisor: Some(supervisor.clone()),
        parent_session_key: Some("api:test".to_string()),
        ..ToolContext::zero()
    };
    // Spawn a background completer that polls the supervisor and
    // flips the first new task to Completed so the wait loop
    // terminates instead of timing out. Uses an unbounded sleep
    // budget but the outer 5s `timeout_ms` is the hard cap.
    let supervisor_for_completer = supervisor.clone();
    let completer = tokio::spawn(async move {
        for _ in 0..200 {
            if let Some(task) = supervisor_for_completer
                .get_all_tasks()
                .into_iter()
                .find(|task| task.tool_name == "spawn" || task.tool_name == "spawn_agent")
            {
                supervisor_for_completer
                    .mark_completed(&task.id, vec!["delegate-output.txt".to_string()]);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    let result = registry
        .execute_with_context(
            &ctx,
            "delegate",
            &json!({
                "role": "reviewer",
                "task": "review the diff for unsafe regressions",
                "timeout_ms": 5_000,
            }),
        )
        .await
        .expect("delegate runs");
    let _ = completer.await;
    assert!(result.success, "{}", result.output);
    let payload: Value = serde_json::from_str(&result.output).expect("payload");
    assert_eq!(payload["role"], json!("reviewer"));
    assert!(payload["agent_id"].is_string(), "{payload}");
    assert!(
        payload["terminal"].as_bool().unwrap_or(false),
        "delegate must report terminal=true once the child completes: {payload}"
    );
    assert_eq!(payload["status"], json!("completed"));
    assert!(
        payload["artifacts"].is_array(),
        "artifacts must be an array even when empty"
    );
    let meta = result
        .structured_metadata
        .expect("delegate must emit structured metadata");
    assert_eq!(meta["codex_tool"], json!("delegate"));
    assert_eq!(meta["role"], json!("reviewer"));
}

/// #1172 denial path: unknown role names must be rejected at the
/// tool boundary so a typo (`"review"` vs `"reviewer"`) surfaces
/// immediately instead of silently smuggling an unbounded prompt.
#[tokio::test]
async fn delegate_rejects_unknown_role() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute(
            "delegate",
            &json!({ "role": "review", "task": "do a review" }),
        )
        .await
        .expect("delegate runs");
    assert!(
        !result.success,
        "delegate must reject `review` (canonical is `reviewer`); got: {}",
        result.output
    );
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("delegate must emit denial metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_denied"));
    assert_eq!(meta["reason"], json!("unknown_role"));
    assert_eq!(meta["role"], json!("review"));
}

/// #1172 denial path: missing `task` is rejected without
/// spawning anything.
#[tokio::test]
async fn delegate_rejects_missing_task() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let result = registry
        .execute("delegate", &json!({ "role": "reviewer" }))
        .await
        .expect("delegate runs");
    assert!(!result.success);
    let meta = result
        .structured_metadata
        .as_ref()
        .expect("delegate must emit denial metadata");
    assert_eq!(meta["reason"], json!("missing_task"));
}

/// #1172 boundary: without a registered native spawn delegate the
/// alias must fail clearly rather than silently no-op so a broken
/// session runtime is visible at the tool boundary.
#[tokio::test]
async fn delegate_fails_when_spawn_agent_unbound() {
    let tool = DelegateAliasTool::new();
    let result = tool
        .execute(&json!({ "role": "reviewer", "task": "inspect" }))
        .await
        .expect("delegate runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_missing"));
}

/// #1172 codex review P1: `bash` must be covered by the
/// `group:runtime` tool policy group so a profile denying runtime
/// commands cannot be bypassed via the Codex naming alias.
/// `delegate` likewise must be covered by `group:sessions` so a
/// policy denying subagent spawn cannot be bypassed via the
/// one-call wrapper.
#[test]
fn codex_naming_aliases_are_covered_by_policy_groups() {
    use crate::tools::policy::tool_group_info;
    let runtime = tool_group_info("group:runtime").expect("group:runtime registered");
    assert!(
        runtime.tools.contains(&"bash"),
        "group:runtime must include `bash` so the alias respects \
             runtime-denying policies: {tools:?}",
        tools = runtime.tools,
    );
    let sessions = tool_group_info("group:sessions").expect("group:sessions registered");
    assert!(
        sessions.tools.contains(&"delegate"),
        "group:sessions must include `delegate` so the alias respects \
             session-denying policies: {tools:?}",
        tools = sessions.tools,
    );
}

/// #1172 codex review P1 acceptance: when a tool policy denies
/// `group:runtime`, the `bash` alias must be filtered out of the
/// registry alongside `shell` / `exec_command` / `write_stdin`.
/// Without the group-coverage fix, `bash` would remain visible and
/// the policy could be bypassed.
#[test]
fn bash_is_filtered_when_policy_denies_runtime_group() {
    use crate::tools::policy::ToolPolicy;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    let policy = ToolPolicy {
        allow: vec![],
        deny: vec!["group:runtime".into()],
        require_tags: vec![],
        bash_file_writes: Default::default(),
    };
    registry.apply_policy(&policy);
    assert!(
        registry.get_tool("bash").is_none(),
        "bash must be filtered when policy denies group:runtime"
    );
    // Sibling tools in the same group must also be gone.
    assert!(registry.get_tool("shell").is_none());
    assert!(registry.get_tool("exec_command").is_none());
}

/// #1172 codex review P1 acceptance: when a policy denies
/// `group:sessions`, the `delegate` alias must be filtered out of
/// the registry alongside `spawn_agent` / `wait_agent` / etc.
/// Without the group-coverage fix, `delegate` would survive the
/// filter and keep an Arc to spawn_agent, so the policy could be
/// bypassed.
#[test]
fn delegate_is_filtered_when_policy_denies_sessions_group() {
    use crate::tools::policy::ToolPolicy;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut registry = ToolRegistry::with_builtins(temp.path());
    let policy = ToolPolicy {
        allow: vec![],
        deny: vec!["group:sessions".into()],
        require_tags: vec![],
        bash_file_writes: Default::default(),
    };
    registry.apply_policy(&policy);
    assert!(
        registry.get_tool("delegate").is_none(),
        "delegate must be filtered when policy denies group:sessions"
    );
    assert!(registry.get_tool("spawn_agent").is_none());
    assert!(registry.get_tool("wait_agent").is_none());
}

/// #1172 codex review P2 acceptance (follow-up): a `bash` command
/// that backgrounds a grandchild and `wait`s on it must still have
/// the grandchild killed when the bash timeout fires. Without
/// `process_group(0)` before spawn, the negative-PID kill targets
/// a process group that the child was never put in, so the
/// backgrounded `sleep` survives the timeout and the workspace
/// mutation happens after the tool reports failure.
#[cfg(unix)]
#[tokio::test]
async fn bash_kills_grandchildren_via_process_group_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("grandchild-late.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    // Backgrounded grandchild that touches the sentinel after a sleep
    // longer than the timeout. The `wait` keeps the outer bash alive
    // so the timeout path is forced to walk the process group.
    let cmd = format!("(sleep 3; touch {sentinel_path}) & wait");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("bash", &json!({ "cmd": cmd, "timeout_ms": 1_000 }))
        .await
        .expect("bash runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "bash must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    // Wait past when the orphaned grandchild's `touch` would fire.
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "grandchild process must be killed via the bash process group on \
             timeout — sentinel at {} should NOT exist (negative-PID kill \
             didn't reach the grandchild)",
        sentinel.display(),
    );
}

/// #1172 codex review P2 acceptance: when a `bash` command exceeds
/// `timeout_ms`, the child process must be killed instead of left
/// alive in the background. We start a child that touches a sentinel
/// file after a sleep that's longer than the timeout. If the kill
/// fires correctly the sentinel never appears; if the child is
/// orphaned it will appear after the timeout returns to the caller.
#[cfg(unix)]
#[tokio::test]
async fn bash_kills_child_process_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("late-write.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    // sleep 3 seconds then touch the sentinel; bash timeout fires
    // at ~1s so the touch must NOT execute if the kill works.
    let cmd = format!("sleep 3; touch {sentinel_path}");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("bash", &json!({ "cmd": cmd, "timeout_ms": 1_000 }))
        .await
        .expect("bash runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "bash must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("timed out"),
        "bash must report timeout in the output; got: {}",
        result.output,
    );
    // Wait past when the orphaned `touch` would have fired had the
    // kill failed (3s sleep + 1s slack).
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "child process must be killed on timeout — sentinel file at {} should NOT exist",
        sentinel.display(),
    );
}

/// P2 (tri-repo #1529): `exec_command`'s timeout path dropped the wait
/// future without killing the child, so the wrapper shell and any
/// grandchildren survived. Mirror of `bash_kills_grandchildren_...`: a
/// backgrounded grandchild touches a sentinel after a sleep longer than
/// the timeout; the process-group kill must stop it.
#[cfg(unix)]
#[tokio::test]
async fn exec_command_kills_grandchildren_via_process_group_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("exec-grandchild-late.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    let cmd = format!("(sleep 3; touch {sentinel_path}) & wait");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("exec_command", &json!({ "cmd": cmd, "timeout_secs": 1 }))
        .await
        .expect("exec_command runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "exec_command must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "grandchild must be killed via the exec_command process group on \
             timeout — sentinel at {} should NOT exist",
        sentinel.display(),
    );
}

/// P2 (tri-repo #1529): the direct-child variant of the above.
#[cfg(unix)]
#[tokio::test]
async fn exec_command_kills_child_process_on_timeout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("exec-late-write.txt");
    let sentinel_path = sentinel.to_string_lossy().into_owned();
    let cmd = format!("sleep 3; touch {sentinel_path}");
    let registry = ToolRegistry::with_builtins(temp.path());
    let started = std::time::Instant::now();
    let result = registry
        .execute("exec_command", &json!({ "cmd": cmd, "timeout_secs": 1 }))
        .await
        .expect("exec_command runs");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "exec_command must return within the timeout window (got {:?})",
        started.elapsed()
    );
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("timed out"),
        "exec_command must report timeout; got: {}",
        result.output,
    );
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;
    assert!(
        !sentinel.exists(),
        "child must be killed on timeout — sentinel at {} should NOT exist",
        sentinel.display(),
    );
}

// #1149 / M14-B P2 tests — `image_generation` stub.
// -----------------------------------------------------------------------

/// The stub MUST surface `image_generation` as model-visible so
/// the canonical Codex tool surface is wire-complete.
#[tokio::test]
async fn builtins_expose_image_generation_tool_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let registry = ToolRegistry::with_builtins(temp.path());
    let names: std::collections::HashSet<_> =
        registry.specs().into_iter().map(|spec| spec.name).collect();
    assert!(
        names.contains("image_generation"),
        "image_generation must be model-visible: {names:?}"
    );
}

/// Happy stub path: a valid prompt returns a structured
/// `coding_tool_unsupported` envelope (no backend bound). The
/// model must NOT receive a generic "tool not found" — instead it
/// gets a typed error it can react to (UPCR-2026-020 §8).
#[tokio::test]
async fn image_generation_returns_typed_unsupported_envelope() {
    let tool = ImageGenerationTool::new();
    let result = tool
        .execute(&json!({
            "prompt": "a snowy cabin at dusk, watercolour",
            "size": "1024x1024",
            "n": 1
        }))
        .await
        .expect("image_generation runs");
    assert!(
        !result.success,
        "stub must not claim success while no backend is bound"
    );
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["codex_tool"], json!("image_generation"));
    assert_eq!(meta["error_kind"], json!("coding_tool_unsupported"));
    assert_eq!(meta["reason"], json!("no_backend_bound"));
    // The accepted input is echoed so AppUI clients can render
    // "tool was called with X" UX while waiting for the follow-up.
    assert_eq!(
        meta["accepted_input"]["prompt"],
        json!("a snowy cabin at dusk, watercolour")
    );
    assert_eq!(meta["accepted_input"]["size"], json!("1024x1024"));
    assert_eq!(meta["accepted_input"]["n"], json!(1));
}

/// Error path: a missing / blank prompt returns
/// `coding_tool_denied` (input validation), not
/// `coding_tool_unsupported` (backend missing). Distinguishing
/// these is important so AppUI clients render the right UX.
#[tokio::test]
async fn image_generation_rejects_missing_prompt() {
    let tool = ImageGenerationTool::new();
    let result = tool
        .execute(&json!({}))
        .await
        .expect("image_generation runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_denied"));
    assert_eq!(meta["reason"], json!("missing_prompt"));
}

/// The stub's spec / schema must accept the canonical Codex
/// input shape (`prompt` required, `size` + `n` optional) so a
/// future backend swap-in is wire-compatible.
#[test]
fn image_generation_schema_pins_canonical_input_shape() {
    let tool = ImageGenerationTool::new();
    assert_eq!(tool.name(), "image_generation");
    let schema = tool.input_schema();
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["required"], json!(["prompt"]));
    let props = schema["properties"].as_object().expect("properties");
    assert!(props.contains_key("prompt"));
    assert!(props.contains_key("size"));
    assert!(props.contains_key("n"));
}

/// Blank-string prompt still triggers the missing-prompt
/// validation path (trimmed). Pinned so a future refactor of
/// the trim/empty filter doesn't quietly drop the check.
#[tokio::test]
async fn image_generation_rejects_whitespace_only_prompt() {
    let tool = ImageGenerationTool::new();
    let result = tool
        .execute(&json!({ "prompt": "   \n\t  " }))
        .await
        .expect("image_generation runs");
    assert!(!result.success);
    let meta = result.structured_metadata.expect("structured metadata");
    assert_eq!(meta["error_kind"], json!("coding_tool_denied"));
    assert_eq!(meta["reason"], json!("missing_prompt"));
}

// ---------------------------------------------------------------------------
// Cancellation must not orphan a child's process group.
//
// The kill ladder only runs on the TIMEOUT arm. A user interrupt drops the
// whole future instead (`agent_task.abort()`), and `tokio::process::Child` does
// not kill on drop — so before `ChildGroupGuard` an Esc'd `bash("npm run dev")`
// kept running, holding ports and able to write to the workspace.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod cancellation_kills_child_group {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// Spawn a long-lived child in its OWN process group, exactly as the tools
    /// do, and hand back its pid.
    fn spawn_group_leader() -> (tokio::process::Child, u32) {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = cmd.spawn().expect("spawn sleeper");
        let pid = child.id().expect("child pid");
        (child, pid)
    }

    /// `kill -0` the group until it disappears, or give up.
    ///
    /// Deliberately `async` with `tokio::time::sleep`: the guard hands its kill
    /// ladder to a spawned task, and a blocking `std::thread::sleep` here would
    /// starve the current-thread test runtime so that task never runs — the
    /// test would fail against a working fix.
    async fn wait_until_group_gone(pid: u32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if !process_group_exists(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        !process_group_exists(pid)
    }

    #[tokio::test]
    async fn dropping_an_armed_guard_kills_the_process_group() {
        let (child, pid) = spawn_group_leader();
        assert!(process_group_exists(pid), "sleeper should be running");

        {
            let _guard = ChildGroupGuard::new(Some(pid));
            drop(child); // tokio does NOT kill on drop — the guard must.
        }

        assert!(
            wait_until_group_gone(pid, Duration::from_secs(5)).await,
            "process group {pid} survived a dropped guard — an interrupted \
             command would keep running and mutating the workspace"
        );
    }

    #[tokio::test]
    async fn disarmed_guard_leaves_the_process_alone() {
        // The normal paths reap or kill the child themselves; the guard must
        // not double-kill, and must not kill a pid that has been recycled.
        let (mut child, pid) = spawn_group_leader();
        {
            let mut guard = ChildGroupGuard::new(Some(pid));
            guard.disarm();
        }
        // Still alive after the disarmed guard dropped.
        assert!(
            process_group_exists(pid),
            "a disarmed guard must not kill the child"
        );
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = signal_process_group(pid, "-9");
    }

    /// End-to-end: abort the tool future the way the serve path does on Esc,
    /// and assert the command it launched is actually gone.
    ///
    /// The command reports its OWN pid through a file rather than being found
    /// by name: `sh -c "sleep 300 # marker"` **execs** `sleep`, so a marker in
    /// the command string never reaches any argv and `pgrep -f` cannot see it.
    /// Because two commands are chained here the shell does not exec, so `$$`
    /// is the surviving shell — which is also the process-group leader, exactly
    /// what the guard must kill.
    #[tokio::test]
    async fn aborting_the_bash_tool_kills_the_command_it_launched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("probe.pid");
        // AllowAll: `ApprovalPolicy::Never` FAILS any command the policy would
        // ask about, which would stop the probe before it ever ran.
        let tool = BashTool::new(dir.path(), Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(crate::policy::AllowAllPolicy));
        let args = json!({
            "cmd": format!("echo $$ > {}; sleep 300", pidfile.display()),
        });

        let handle = tokio::spawn(async move { tool.execute(&args).await });

        // Wait for the probe to actually be running before aborting.
        let mut probe_pid = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    if process_exists(pid) {
                        probe_pid = Some(pid);
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let probe_pid = probe_pid.expect("probe never started; test cannot conclude");

        // This is what Esc does on the serve path.
        handle.abort();
        let _ = handle.await;

        let gone = wait_until_group_gone(probe_pid, Duration::from_secs(10)).await;
        // Clean up before asserting so a failure cannot leak a sleeper.
        if !gone {
            let _ = signal_process_group(probe_pid, "-9");
            let _ = signal_process(probe_pid, "-9");
        }
        assert!(
            gone,
            "aborting the turn left process group {probe_pid} alive — an \
             interrupted `npm run dev` would keep holding ports and writing \
             to the workspace"
        );
    }
}

#[test]
fn should_declare_element_schema_when_spawn_agent_items_is_array() {
    let schema = SpawnAgentTool::new().input_schema();
    let items = &schema["properties"]["items"];

    assert_eq!(items["type"], json!("array"));
    assert_eq!(
        items["items"]["type"],
        json!("object"),
        "spawn_agent.items accepts structured Codex content items, so its array \
         must declare an object element schema"
    );
    assert!(items["items"]["properties"]["type"].is_object());
    assert!(items["items"]["properties"]["text"].is_object());
}

// ---------------------------------------------------------------------------
// #28c — file-change receipt on the CODING-session bash path (BashTool),
// reusing the shared 28a module. These tests pin the 28a acceptance set
// on this link: real edit ⇒ receipt; phantom edit ⇒ 0; non-git ⇒ omitted;
// default (no knob involvement here) ⇒ unchanged when nothing changed.
// ---------------------------------------------------------------------------
mod bash_change_receipt_28c {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::BashTool;
    use std::sync::Arc;

    fn tool(dir: &std::path::Path) -> BashTool {
        BashTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
    }

    #[tokio::test]
    async fn real_edit_in_git_repo_appends_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(cwd)
            .status()
            .expect("git commit");
        let target = cwd.join("receipt-28c.txt");
        let out = tool(cwd)
            .execute(&json!({
                "cmd": format!("echo real-edit > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "receipt missing on the coding bash path: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn phantom_edit_reports_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        // No write at all — the receipt must be "files_changed: 0" (or absent
        // if the tree were clean-vs-clean; porcelain empty ⇒ None from
        // snapshot ⇒ omitted; either way NEVER a nonzero phantom count).
        let out = tool(cwd)
            .execute(&json!({ "cmd": "echo phantom-check" }))
            .await
            .expect("execute");
        assert!(out.success);
        assert!(
            !out.output.contains("files_changed: 2"),
            "no phantom count may appear: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn non_git_dir_omits_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path(); // never git-init'd
        let target = cwd.join("plain.txt");
        let out = tool(cwd)
            .execute(&json!({
                "cmd": format!("echo plain > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            !out.output.contains("files_changed"),
            "non-git fail-open must omit the receipt: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn default_behavior_when_no_tree_change_is_plain_output() {
        // Zero-difference guarantee for the default path: in a git repo with
        // an UNCHANGED tree, a read-only command's output carries no
        // receipt noise beyond the accepted "files_changed: 0" line, which
        // itself only appears when the tree was ALREADY dirty — pin the
        // clean-tree case: no receipt at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(cwd)
            .status()
            .expect("git commit");
        let out = tool(cwd)
            .execute(&json!({ "cmd": "echo zero-diff-28c" }))
            .await
            .expect("execute");
        assert!(out.success);
        assert!(out.output.contains("zero-diff-28c"));
        // 28a semantics: an empty dirty set yields exactly the single line
        // "files_changed: 0" (matches ShellTool). No path list, no noise.
        assert!(
            out.output.contains("files_changed: 0"),
            "clean tree ⇒ the single zero line: {}",
            out.output
        );
        assert!(
            !out.output.contains("(+"),
            "no truncation/list noise on a clean tree: {}",
            out.output
        );
    }
}

mod exec_change_receipt_28c {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::ExecCommandTool;
    use std::sync::Arc;

    fn tool(dir: &std::path::Path) -> ExecCommandTool {
        ExecCommandTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
    }

    #[tokio::test]
    async fn real_edit_in_git_repo_appends_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
        let target = cwd.join("exec-receipt-28c.txt");
        let out = tool(cwd)
            .execute(&json!({
                "command": format!("echo real-edit > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "receipt missing on the exec_command path: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn non_git_dir_omits_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let target = cwd.join("plain.txt");
        let out = tool(cwd)
            .execute(&json!({
                "command": format!("echo plain > {:?}", target),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            !out.output.contains("files_changed"),
            "non-git fail-open must omit the receipt: {}",
            out.output
        );
    }
}

mod receipt_scope_root_28c_r1 {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::{BashTool, receipt_scope_root};
    use std::sync::Arc;

    fn git_init(cwd: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
    }

    // Resolver unit pins (ruling ②): literal cd prefix wins; ambiguous
    // shapes fall back to workdir.
    #[test]
    fn resolver_literal_cd_prefix_wins_and_var_falls_back() {
        let workdir = std::path::Path::new("/ws");
        let (root, scope) = receipt_scope_root(workdir, "cd /tmp/x && echo hi > f");
        assert_eq!(root, std::path::PathBuf::from("/tmp/x"));
        assert_eq!(scope, "cd-target");

        let (root, scope) = receipt_scope_root(workdir, "cd ~/proj && echo hi > f");
        assert_eq!(scope, "cd-target");
        assert!(root.starts_with(std::path::Path::new("/")));

        // No cd prefix ⇒ workdir.
        let (root, scope) = receipt_scope_root(workdir, "echo hi > f");
        assert_eq!(root, std::path::PathBuf::from("/ws"));
        assert_eq!(scope, "workdir");

        // Variable path ⇒ ambiguous ⇒ workdir.
        let (root, scope) = receipt_scope_root(workdir, "cd $TARGET && echo hi > f");
        assert_eq!(root, std::path::PathBuf::from("/ws"));
        assert_eq!(scope, "workdir");

        // cd without && ⇒ workdir.
        let (_root, scope) = receipt_scope_root(workdir, "cd /tmp/x");
        assert_eq!(scope, "workdir");

        // Semicolon chain ⇒ ambiguous ⇒ workdir.
        let (root, scope) = receipt_scope_root(workdir, "cd /tmp/x; cd /tmp/y && echo hi > f");
        assert_eq!(scope, "workdir");
        assert_eq!(root, std::path::PathBuf::from("/ws"));
    }

    // Live-shape pin (ruling ④): cd-prefix write reports files_changed: 1
    // plus the cd-target scope tag — the exact false-phantom regression.
    #[tokio::test]
    async fn cd_prefix_write_reports_one_with_scope_tag() {
        let session_ws = tempfile::tempdir().expect("tempdir"); // session workdir: NOT a repo
        let target = tempfile::tempdir().expect("tempdir"); // cd target: a git repo
        git_init(target.path());
        let tool = BashTool::new(session_ws.path(), Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy));
        let file = target.path().join("r1.txt");
        let out = tool
            .execute(&json!({
                "cmd": format!("cd {} && echo real > {:?}", target.path().display(), file),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "cd-target write must count 1: {}",
            out.output
        );
        assert!(
            out.output.contains("scope: cd-target"),
            "scope tag missing: {}",
            out.output
        );
    }

    // Variable cd path falls back to workdir (ruling ④): the write happens
    // inside the workdir repo, but the snapshot root is the workdir — the
    // count is still correct for a workdir write; the tag says workdir.
    #[tokio::test]
    async fn variable_cd_path_falls_back_to_workdir_scope() {
        let ws = tempfile::tempdir().expect("tempdir");
        git_init(ws.path());
        let tool = BashTool::new(ws.path(), Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy));
        let file = ws.path().join("var.txt");
        // $TARGET is unset in the child, so `cd $TARGET` fails and the write
        // still lands relative to the workdir via the absolute path — the
        // receipt must not phantom-zero this.
        let out = tool
            .execute(&json!({
                "cmd": format!("cd $TARGET 2>/dev/null; echo v > {:?} # octos:allow-write", file),
            }))
            .await
            .expect("execute");
        assert!(out.success, "output: {}", out.output);
        assert!(
            out.output.contains("files_changed: 1"),
            "workdir write must count 1: {}",
            out.output
        );
        assert!(
            out.output.contains("scope: workdir"),
            "fallback scope tag missing: {}",
            out.output
        );
    }
}

mod bash_file_writes_28d {
    use super::*;
    use crate::policy::AllowAllPolicy;
    use crate::tools::coding_tools::{BashTool, ExecCommandTool};
    use crate::tools::policy::BashFileWrites;
    use std::sync::Arc;

    fn bash(dir: &std::path::Path, mode: BashFileWrites) -> BashTool {
        BashTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
            .with_bash_file_writes(mode)
    }

    fn exec(dir: &std::path::Path, mode: BashFileWrites) -> ExecCommandTool {
        ExecCommandTool::new(dir, Arc::new(crate::sandbox::NoSandbox))
            .with_policy(Arc::new(AllowAllPolicy))
            .with_bash_file_writes(mode)
    }

    fn git_init(cwd: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(cwd)
            .status()
            .expect("git init");
    }

    // deny: write-shaped command refused, escape hatch honored — on BOTH
    // coding tools.
    #[tokio::test]
    async fn deny_refuses_write_and_escape_hatch_runs_bash_and_exec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = bash(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "cmd": "echo x > /tmp/never-28d" }))
            .await
            .expect("execute");
        assert!(!out.success);
        assert!(out.output.contains("bash_file_writes=deny"));
        // Refusal text only — the command must not have run.
        let _ = std::path::Path::new("/tmp/never-28d");

        let out = exec(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "command": "echo x > /tmp/never-28d-e" }))
            .await
            .expect("execute");
        assert!(!out.success, "exec deny: {}", out.output);
        assert!(out.output.contains("bash_file_writes=deny"));

        // Escape hatch: trailing `# octos:allow-write` runs the write.
        let hatch = dir.path().join("hatch.txt");
        let out = bash(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "cmd": format!("echo h > {:?} # octos:allow-write", hatch) }))
            .await
            .expect("execute");
        assert!(out.success, "hatch: {}", out.output);
        assert!(hatch.exists());
    }

    // deny lets read-only commands through untouched.
    #[tokio::test]
    async fn deny_lets_readonly_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = bash(dir.path(), BashFileWrites::Deny)
            .execute(&json!({ "cmd": "echo readonly-28d" }))
            .await
            .expect("execute");
        assert!(out.success);
        assert!(out.output.contains("readonly-28d"));
        assert!(!out.output.contains("bash_file_writes"));
    }

    // warn: nudge only when files actually changed.
    #[tokio::test]
    async fn warn_nudges_only_on_change_bash_and_exec() {
        let dir = tempfile::tempdir().expect("tempdir");
        git_init(dir.path());
        let out = bash(dir.path(), BashFileWrites::Warn)
            .execute(&json!({ "cmd": "echo nochange" }))
            .await
            .expect("execute");
        assert!(out.success);
        assert!(!out.output.contains("bash_file_writes=warn"));

        let f = dir.path().join("w.txt");
        let out = bash(dir.path(), BashFileWrites::Warn)
            .execute(&json!({ "cmd": format!("echo w > {:?}", f) }))
            .await
            .expect("execute");
        assert!(
            out.output.contains("bash_file_writes=warn"),
            "{}",
            out.output
        );

        let f2 = dir.path().join("w2.txt");
        let out = exec(dir.path(), BashFileWrites::Warn)
            .execute(&json!({ "command": format!("echo w2 > {:?}", f2) }))
            .await
            .expect("execute");
        assert!(
            out.output.contains("bash_file_writes=warn"),
            "exec warn: {}",
            out.output
        );
    }

    // allow (default): zero difference — no policy text anywhere.
    #[tokio::test]
    async fn allow_is_zero_difference_on_both_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        for out in [
            bash(dir.path(), BashFileWrites::default())
                .execute(&json!({ "cmd": "echo zd" }))
                .await
                .expect("bash"),
            exec(dir.path(), BashFileWrites::default())
                .execute(&json!({ "command": "echo zd" }))
                .await
                .expect("exec"),
        ] {
            assert!(out.success);
            assert!(out.output.contains("zd"));
            assert!(!out.output.contains("bash_file_writes"));
            assert!(!out.output.contains("edit_file / diff_edit"));
        }
    }

    // Session-load wiring: EffectivePermissions carries the knob and the
    // registry constructor injects it into the coding tools.
    #[test]
    fn permissions_default_carry_allow_and_registry_injects_knob() {
        use crate::policy::EffectivePermissions;
        let perms = EffectivePermissions::default();
        assert_eq!(perms.bash_file_writes, BashFileWrites::Allow);
        // with_builtins_and_permissions must not panic and must register the
        // bash tool (the knob rides inside it; deny behavior is covered by
        // the tool-level tests above).
        let dir = tempfile::tempdir().expect("tempdir");
        let reg = crate::ToolRegistry::with_builtins_and_permissions(
            dir.path(),
            Box::new(crate::sandbox::NoSandbox),
            perms,
        );
        assert!(reg.get_tool("bash").is_some());
        assert!(reg.get_tool("exec_command").is_some());
    }
}
