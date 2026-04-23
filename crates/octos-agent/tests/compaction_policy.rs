//! Integration tests for contract-gated compaction policy (harness M6.3).
//!
//! Covers the typed [`CompactionPolicy`], the [`Summarizer`] seam, preflight
//! compaction before the first LLM call, typed tool-result placeholders, and
//! the post-compaction validator rail that gates on artifact preservation.
//!
//! Run with `cargo test -p octos-agent --test compaction_policy`.

use std::collections::BTreeMap;

use octos_agent::compaction::{
    CompactionPhase, CompactionPolicy, CompactionRunner, ExtractiveSummarizer, PreservedArtifact,
    Summarizer, TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION, ToolResultPlaceholder,
    ToolResultPlaceholderError,
};
use octos_agent::workspace_policy::{
    WorkspaceArtifactsPolicy, WorkspacePolicy, WorkspacePolicyKind, WorkspacePolicyWorkspace,
    WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy, WorkspaceVersionControlPolicy,
    WorkspaceVersionControlProvider,
};
use octos_agent::{COMPACTION_POLICY_SCHEMA_VERSION, WORKSPACE_POLICY_SCHEMA_VERSION};
use octos_core::{Message, MessageRole, ToolCall};

fn user_msg(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn assistant_msg(content: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn assistant_tool_call(tool_name: &str, tool_id: &str, args: serde_json::Value) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![ToolCall {
            id: tool_id.to_string(),
            name: tool_name.to_string(),
            arguments: args,
            metadata: None,
        }]),
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn tool_result(tool_id: &str, content: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: Some(tool_id.to_string()),
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn system_msg(content: &str) -> Message {
    Message {
        role: MessageRole::System,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn policy_with_compaction(compaction: CompactionPolicy) -> WorkspacePolicy {
    WorkspacePolicy {
        schema_version: WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Sites,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: false,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: false,
        },
        tracking: WorkspaceTrackingPolicy { ignore: vec![] },
        validation: Default::default(),
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([
                ("primary".into(), "output/deck.pptx".into()),
                ("previews".into(), "output/slide-*.png".into()),
            ]),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: Some(compaction),
    }
}

#[test]
fn should_keep_existing_extractive_behavior_when_policy_absent() {
    // With no policy, the existing compaction::compact_messages continues to
    // work unchanged: summary header, first-line extraction, budget enforcement.
    let messages = vec![
        user_msg("Please summarise foo.txt"),
        assistant_msg("Will do."),
        user_msg("Now read bar.txt"),
        assistant_tool_call(
            "read_file",
            "tc1",
            serde_json::json!({"path": "/secret/bar.txt"}),
        ),
        tool_result("tc1", "bar contents line"),
    ];

    let summary = octos_agent::compaction::compact_messages(&messages, 10_000);
    assert!(summary.contains("Conversation Summary"));
    assert!(summary.contains("> User: Please summarise foo.txt"));
    assert!(summary.contains("Called read_file"));
    // Strips untrusted tool arguments.
    assert!(!summary.contains("/secret/bar.txt"));
}

#[test]
fn should_fire_preflight_compaction_when_context_exceeds_threshold() {
    let policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 1_000,
        preflight_threshold: Some(200),
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec!["primary".into()],
        preserved_invariants: vec![],
        summarizer: Default::default(),
    };
    let runner = CompactionRunner::new(policy).with_summarizer(ExtractiveSummarizer::new());
    let big = "x".repeat(3_000);
    let mut messages = vec![
        system_msg("system prompt"),
        user_msg(&format!("long user message {big}")),
        assistant_msg(&format!("long assistant reply {big}")),
        user_msg("trigger"),
    ];

    let decision = runner.needs_preflight(&messages);
    assert!(
        decision.is_some(),
        "preflight should fire when tokens > threshold"
    );

    let outcome = runner.run(&mut messages, CompactionPhase::Preflight);
    assert!(outcome.performed, "preflight should execute compaction");
    assert!(
        messages.len() < 4 || outcome.messages_dropped > 0,
        "preflight should drop/compact at least one old message"
    );
}

#[test]
fn should_preserve_declared_artifacts_through_compaction() {
    let policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 2_000,
        preflight_threshold: None,
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec!["primary".into()],
        preserved_invariants: vec!["output/deck.pptx".into()],
        summarizer: Default::default(),
    };
    let workspace = policy_with_compaction(policy.clone());
    let runner = CompactionRunner::new(policy)
        .with_summarizer(ExtractiveSummarizer::new())
        .with_workspace_policy(&workspace);

    // Messages mention the declared artifact path; compaction must not lose it.
    let mut messages = vec![
        system_msg("system prompt"),
        user_msg("create the deck"),
        assistant_msg(
            "I wrote output/deck.pptx successfully and you can find previews at output/slide-1.png.",
        ),
        assistant_tool_call("shell", "tc1", serde_json::json!({"command": "ls output"})),
        tool_result("tc1", "output/deck.pptx\noutput/slide-1.png\n"),
        assistant_msg("Done."),
        user_msg("thanks"),
    ];

    let outcome = runner.run(&mut messages, CompactionPhase::TurnEnd);
    let ledger = runner
        .check_preserved(&messages, &workspace)
        .expect("preservation check");
    assert!(
        ledger.all_preserved(),
        "declared artifact path should survive compaction ({:?})",
        ledger
    );
    assert!(outcome.performed || outcome.messages_dropped == 0);
}

#[test]
fn should_gate_on_validator_failure_when_invariants_not_preserved() {
    let policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 100, // forcibly tiny budget
        preflight_threshold: None,
        prune_tool_results_after_turns: None,
        preserved_artifacts: vec!["primary".into()],
        preserved_invariants: vec!["NEVER_MENTIONED_STRING".into()],
        summarizer: Default::default(),
    };
    let workspace = policy_with_compaction(policy.clone());
    let runner = CompactionRunner::new(policy).with_summarizer(ExtractiveSummarizer::new());

    let mut messages = vec![
        system_msg("system prompt"),
        user_msg("create the deck"),
        assistant_msg("ok"),
        user_msg("done"),
    ];
    // Force compaction: (compaction will summarise nothing truly; still run validator)
    let _ = runner.run(&mut messages, CompactionPhase::TurnEnd);
    let ledger = runner
        .check_preserved(&messages, &workspace)
        .expect("preservation check");
    assert!(
        !ledger.all_preserved(),
        "missing invariant should fail validator gate"
    );
    assert!(
        !ledger.missing.is_empty(),
        "ledger should list missing invariants"
    );
}

#[test]
fn should_replace_old_tool_results_with_typed_placeholder() {
    let policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 4_000,
        preflight_threshold: None,
        prune_tool_results_after_turns: Some(2),
        preserved_artifacts: vec![],
        preserved_invariants: vec![],
        summarizer: Default::default(),
    };
    let runner = CompactionRunner::new(policy);
    let big_output = "result body ".repeat(500);
    let mut messages = vec![
        system_msg("prompt"),
        user_msg("run tool"),
        assistant_tool_call("read_file", "tc1", serde_json::json!({})),
        tool_result("tc1", &big_output),
        assistant_msg("reviewed tc1"),
        user_msg("now another"),
        assistant_tool_call("read_file", "tc2", serde_json::json!({})),
        tool_result("tc2", &big_output),
        assistant_msg("reviewed tc2"),
        user_msg("and a third"),
        assistant_tool_call("read_file", "tc3", serde_json::json!({})),
        tool_result("tc3", &big_output),
        assistant_msg("reviewed tc3"),
        user_msg("current turn"),
    ];

    let report = runner.prune_tool_results(&mut messages);
    assert!(
        report.replaced >= 1,
        "at least one old tool result should be replaced: {report:?}"
    );

    // The first old tool result is now a placeholder.
    let first_tool = messages
        .iter()
        .find(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("tc1"))
        .expect("tc1 tool result present");
    let placeholder = ToolResultPlaceholder::from_placeholder_content(&first_tool.content)
        .expect("old tool should carry typed placeholder");
    assert_eq!(
        placeholder.schema_version,
        TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION
    );
    assert_eq!(placeholder.tool_call_id, "tc1");
    assert_eq!(placeholder.tool_name, "read_file");
    assert!(placeholder.original_byte_len.unwrap_or(0) > 0);
}

#[test]
fn should_emit_compaction_phase_events() {
    let sink_dir = tempfile::tempdir().expect("tempdir");
    let sink_path = sink_dir.path().join("events.jsonl");

    let policy = CompactionPolicy {
        schema_version: COMPACTION_POLICY_SCHEMA_VERSION,
        token_budget: 400,
        preflight_threshold: Some(100),
        prune_tool_results_after_turns: Some(1),
        preserved_artifacts: vec![],
        preserved_invariants: vec![],
        summarizer: Default::default(),
    };
    let runner = CompactionRunner::new(policy)
        .with_summarizer(ExtractiveSummarizer::new())
        .with_event_sink(sink_path.display().to_string(), "sess-1", "task-1");

    let mut messages = vec![
        system_msg("system"),
        user_msg(&"filler ".repeat(200)),
        assistant_msg(&"reply ".repeat(200)),
        user_msg("current"),
    ];
    let outcome = runner.run(&mut messages, CompactionPhase::Preflight);
    assert!(outcome.performed || outcome.messages_dropped == 0);

    // Collect emitted events.
    let raw = std::fs::read_to_string(&sink_path).expect("read sink file");
    assert!(
        raw.contains("\"kind\":\"phase\""),
        "expected phase events to be written: {raw}"
    );
    assert!(
        raw.contains("compaction"),
        "expected phase name 'compaction' in event payload: {raw}"
    );
}

#[test]
fn summarizer_trait_extractive_default_preserves_behavior() {
    let summarizer = ExtractiveSummarizer::new();
    let messages = vec![
        user_msg("Hello"),
        assistant_msg("Hi there"),
        assistant_tool_call("read_file", "tc1", serde_json::json!({"path": "/tmp/x"})),
        tool_result("tc1", "contents"),
    ];

    let summary = summarizer
        .summarize(&messages, 5_000)
        .expect("extractive should not fail");
    assert!(summary.contains("Conversation Summary"));
    assert!(summary.contains("> User: Hello"));
    assert!(summary.contains("Called read_file"));
}

#[test]
fn compaction_policy_schema_version_is_pinned() {
    assert_eq!(COMPACTION_POLICY_SCHEMA_VERSION, 1);
}

#[test]
fn preserved_artifact_is_named_not_just_a_string() {
    // This is the typed seam — PreservedArtifact carries a stable name plus
    // the raw pattern so the validator can report which one was dropped.
    let art = PreservedArtifact::new("primary", "output/deck.pptx");
    assert_eq!(art.name(), "primary");
    assert_eq!(art.pattern(), "output/deck.pptx");
}

#[test]
fn tool_result_placeholder_roundtrips_through_json() {
    let placeholder = ToolResultPlaceholder {
        schema_version: TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION,
        tool_name: "shell".into(),
        tool_call_id: "call_abc".into(),
        turn_id: Some(3),
        original_byte_len: Some(4096),
        reason: "pruned_after_turns".into(),
    };
    let json = placeholder.to_placeholder_content();
    let parsed = ToolResultPlaceholder::from_placeholder_content(&json).expect("roundtrip");
    assert_eq!(parsed, placeholder);
}

#[test]
fn tool_result_placeholder_rejects_unsupported_schema_version() {
    let raw = serde_json::json!({
        "schema": "octos.tool_result_placeholder.v99",
        "tool_name": "x",
        "tool_call_id": "c",
        "reason": "r"
    })
    .to_string();
    let prefixed = format!("[OCTOS_TOOL_RESULT_PLACEHOLDER]{raw}");
    let err = ToolResultPlaceholder::from_placeholder_content(&prefixed).unwrap_err();
    assert!(matches!(
        err,
        ToolResultPlaceholderError::UnsupportedSchema(_)
    ));
}
