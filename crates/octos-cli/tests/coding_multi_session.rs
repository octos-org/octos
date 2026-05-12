//! M11-G — Coding-agent multi-session isolation regression gate
//! (issue #877, closes after M11-D + M11-E land).
//!
//! End-to-end test proving the N-sessions-per-profile invariant: two
//! AppUI sessions opened on the SAME profile with DIFFERENT
//! `workspace_hint`s must each observe their own files and never see
//! each other's. The test drives the public `POST /api/chat` route via
//! `build_router` + `tower::ServiceExt::oneshot` so the assertions
//! exercise the production handler chain
//! (`chat` → `chat_sync` → `chat_sync_via_session_runtime` →
//! `SessionRuntimeCache::get_or_init` → `Agent::process_message`).
//!
//! # Step-by-step → invariant map
//!
//! Every assertion below is paired with the production code path it
//! catches. The mapping is deliberate: a single mutation experiment
//! (e.g. deleting `workspace_hint` handling in
//! `SessionRuntime::bootstrap`) must surface as a SPECIFIC assertion
//! failure, not a generic smoke regression.
//!
//! 1. Pre-warm the session cache for session A with hint = `repo-A`
//!    → exercises [`SessionRuntime::bootstrap`]'s `workspace_hint`
//!    handling. If the hint is dropped, the bootstrap falls back to
//!    `<data_dir>/users/<encoded base>/workspace` and the
//!    "session A reads its own a.txt" assertion fails because the
//!    workspace_root does not contain `a.txt`.
//! 2. Drive `POST /api/chat` for session A with `read_file:a.txt`
//!    → exercises the M11-D dispatcher and the workspace-bound
//!    [`ToolRegistry`] cloned by [`SessionRuntime::bootstrap`].
//!    Assertion: response body contains `"hello-A"` (the per-session
//!    cwd was actually honored by the tool call).
//! 3. Drive `POST /api/chat` for session B with `read_file:a.txt`
//!    → the cross-read MUST fail. Assertion: response body contains
//!    a "not found" / "outside working directory" marker. If the two
//!    sessions shared a workspace, B would see A's file and this
//!    assertion would fail. This is the multi-tenant-leak gate
//!    codex flagged on PR #868.
//! 4. Drive `POST /api/chat` for session B with `read_file:b.txt`
//!    → response body contains `"hello-B"`. Symmetric check that B's
//!    own workspace is wired correctly.
//! 5. Assert independent canonical JSONL chat history files exist
//!    under each session's `user_key` directory.
//!    → exercises `persist_chat_message_through_canonical` writing
//!    per-session paths derived from `SessionKey`.
//! 6. Assert per-session `.octos-workspace.toml` files exist at
//!    `repo-A/.octos-workspace.toml` AND `repo-B/.octos-workspace.toml`.
//!    → exercises [`SessionRuntime::bootstrap`]'s
//!    `write_workspace_policy_if_absent` step. If the policy write is
//!    skipped, the M11-D yangmi gap re-opens.
//!
//! The mutation experiment required by the issue: delete the
//! `workspace_hint` path in
//! `crates/octos-cli/src/runtime/session.rs::resolve_workspace_root`
//! (so the function always returns the data-dir-relative default).
//! Step 2's "hello-A" assertion fails first because session A's
//! workspace becomes the data-dir-relative path (no `a.txt` present);
//! step 6's policy-at-repo-A path also disappears because the bootstrap
//! writes the policy under the default workspace, not `repo-A`. The
//! failing transcript is pasted into the PR description.

#![cfg(feature = "api")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use octos_cli::api::{AppState, build_router};
use octos_cli::runtime::ProfileRuntime;
use octos_core::{MAIN_PROFILE_ID, Message, MessageRole, SessionKey, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use serde_json::json;
use tempfile::TempDir;
use tower::util::ServiceExt;

/// Stub LLM that simulates a coding-agent turn.
///
/// On every `chat()` invocation:
///
/// - If the message history already contains a [`MessageRole::Tool`]
///   message, the stub returns its content verbatim as the final
///   assistant reply. The agent loop will then exit with `EndTurn` and
///   `process_message` returns a `ConversationResponse.content`
///   carrying the tool output.
/// - Otherwise (first turn, no tool result yet), the stub emits a
///   single `read_file` tool call whose `path` argument is parsed from
///   the last user message of the form `read_file:<path>`. The agent
///   loop will execute the call against the session's workspace-bound
///   [`octos_agent::ToolRegistry`] and re-enter the LLM with the tool
///   result appended — that second pass takes the first branch above.
///
/// This is the minimal shape needed to drive a `read_file` turn
/// through the M11-D `/api/chat` dispatcher without any external API
/// keys or real LLM provider, mirroring the
/// `qos_catalog::StubProvider` and the M11-C/D `EchoLlm` pattern.
struct ReadFileStubLlm;

#[async_trait::async_trait]
impl LlmProvider for ReadFileStubLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        // Second-pass: the agent loop already ran the tool and appended
        // a Tool message. Echo it verbatim so the test can assert on
        // the final `ConversationResponse.content`.
        if let Some(tool_msg) = messages.iter().rev().find(|m| m.role == MessageRole::Tool) {
            return Ok(ChatResponse {
                content: Some(tool_msg.content.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 5,
                    ..Default::default()
                },
                provider_index: None,
            });
        }

        // First-pass: extract the file path from the last user message
        // (`read_file:<path>` form) and emit a single read_file call.
        let path = messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .and_then(|m| m.content.strip_prefix("read_file:"))
            .map(str::trim)
            .unwrap_or("a.txt")
            .to_string();

        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "m11g-tc-1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({ "path": path }),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "m11g-readfile-stub"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }

    fn context_window(&self) -> u32 {
        64_000
    }
}

/// Construct a `ProfileRuntime` wired to the stub LLM and a fresh
/// builtin [`octos_agent::ToolRegistry`] (which includes `read_file`).
async fn make_m11g_profile(profile_id: &str, data_dir: &std::path::Path) -> Arc<ProfileRuntime> {
    std::fs::create_dir_all(data_dir).expect("profile data dir");
    let memory = Arc::new(
        octos_memory::EpisodeStore::open(data_dir)
            .await
            .expect("episode store"),
    );
    let memory_store = Arc::new(
        octos_memory::MemoryStore::open(data_dir)
            .await
            .expect("memory store"),
    );
    let tool_config = Arc::new(
        octos_agent::ToolConfigStore::open(data_dir)
            .await
            .expect("tool config store"),
    );
    let sandbox = octos_agent::SandboxConfig::default();
    let base_tools = octos_agent::ToolRegistry::with_builtins_and_sandbox(
        data_dir,
        octos_agent::create_sandbox(&sandbox),
    );
    Arc::new(ProfileRuntime {
        profile_id: profile_id.to_string(),
        data_dir: data_dir.to_path_buf(),
        llm: Arc::new(ReadFileStubLlm),
        adaptive_router: None,
        runtime_qos_catalog: None,
        primary_model_id: "m11g-readfile-stub".to_string(),
        provider_name: "stub".to_string(),
        credentials: HashMap::new(),
        skills_dir: None,
        plugin_env_template: Vec::new(),
        tool_policy: None,
        default_sandbox: sandbox,
        tool_specs: Arc::new(base_tools),
        plugin_tool_names: Vec::new(),
        plugin_dirs: Vec::new(),
        plugin_prompt_fragments: Vec::new(),
        plugin_hooks: Vec::new(),
        system_prompt: "test-system-prompt".to_string(),
        memory,
        memory_store,
        tool_config,
        cron_service: None,
        hook_executor: None,
    })
}

/// Drive a single `POST /api/chat` request through the assembled
/// router. Returns the response body parsed into the same shape the
/// production [`octos_cli::api::handlers::chat`] handler emits.
async fn post_chat(
    app: &axum::Router,
    session_id: &str,
    message: &str,
) -> (StatusCode, serde_json::Value) {
    let body = serde_json::to_string(&json!({
        "message": message,
        "session_id": session_id,
        "stream": false,
    }))
    .expect("serialize chat request");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("router serves chat request");

    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read response body");
    let parsed = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "expected JSON body, got {err}; raw={}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, parsed)
}

#[tokio::test]
async fn coding_agent_two_sessions_isolated_workspaces() {
    // 1. Boot a `serve` instance with one profile + a stub LLM. We use
    //    `MAIN_PROFILE_ID` because `POST /api/chat` without any routing
    //    header falls back to it (see `api_profile_id_from_headers`).
    let temp = TempDir::new().expect("tempdir");
    let profile_data_dir = temp.path().join("profile-data");
    let profile_runtime = make_m11g_profile(MAIN_PROFILE_ID, &profile_data_dir).await;

    let mut profiles = HashMap::new();
    profiles.insert(MAIN_PROFILE_ID.to_string(), profile_runtime.clone());
    let state = Arc::new(AppState {
        profiles,
        ..AppState::empty_for_tests()
    });
    let app = build_router(state.clone());

    // 2. Pre-seed two distinct "repo" workspaces. Using `tempfile`
    //    instead of literal `/tmp/repo-A` so parallel `cargo test` runs
    //    don't collide.
    let repo_a = temp.path().join("repo-A");
    let repo_b = temp.path().join("repo-B");
    std::fs::create_dir_all(&repo_a).expect("create repo-A");
    std::fs::create_dir_all(&repo_b).expect("create repo-B");
    std::fs::write(repo_a.join("a.txt"), "hello-A\n").expect("seed a.txt");
    std::fs::write(repo_b.join("b.txt"), "hello-B\n").expect("seed b.txt");

    // 3. The session keys POST /api/chat will resolve to (handlers.rs
    //    builds these via `standalone_api_session_key_with_topic` →
    //    `SessionKey::with_profile_topic`). We use them to pre-warm
    //    the session cache with the desired `workspace_hint` per
    //    session — the cache is single-flight per key, so the
    //    subsequent `chat_sync_via_session_runtime` call that passes
    //    `None` for the hint reuses the cached runtime built against
    //    the supplied repo. This mirrors how `session/open` (M11-E)
    //    threads the hint into the cache ahead of any turn.
    let session_a_id = "coding-multi-session-A";
    let session_b_id = "coding-multi-session-B";
    let key_a = SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", session_a_id, "");
    let key_b = SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", session_b_id, "");

    let rt_a = state
        .session_cache
        .get_or_init(&profile_runtime, key_a.clone(), Some(repo_a.clone()))
        .await
        .expect("bootstrap session A with workspace_hint = repo-A");
    let rt_b = state
        .session_cache
        .get_or_init(&profile_runtime, key_b.clone(), Some(repo_b.clone()))
        .await
        .expect("bootstrap session B with workspace_hint = repo-B");

    // Sanity: the two sessions hold DISTINCT `Arc<ToolRegistry>`
    // instances (codex multi-tenant scope note from PR #868). If the
    // workspace_hint handling silently collapsed both sessions onto
    // one registry, this would catch the regression immediately —
    // before any of the read_file assertions run.
    assert!(
        !Arc::ptr_eq(&rt_a.tools, &rt_b.tools),
        "per-session tool registries must be distinct Arcs (multi-tenant scope)",
    );
    assert_ne!(
        rt_a.workspace_root, rt_b.workspace_root,
        "per-session workspace roots must differ when distinct hints are supplied",
    );
    assert_eq!(
        rt_a.workspace_root, repo_a,
        "session A's workspace must be the supplied workspace_hint (repo-A); \
         if this fails, `SessionRuntime::bootstrap` is dropping `workspace_hint`",
    );
    assert_eq!(
        rt_b.workspace_root, repo_b,
        "session B's workspace must be the supplied workspace_hint (repo-B); \
         if this fails, `SessionRuntime::bootstrap` is dropping `workspace_hint`",
    );

    // 4. Session A reads its own a.txt → response carries "hello-A".
    let (status_a, body_a) = post_chat(&app, session_a_id, "read_file:a.txt").await;
    assert_eq!(
        status_a,
        StatusCode::OK,
        "session A read_file(a.txt) must return 200; body={body_a}",
    );
    let content_a = body_a["content"]
        .as_str()
        .unwrap_or_else(|| panic!("session A response missing content field: {body_a}"));
    assert!(
        content_a.contains("hello-A"),
        "session A's read_file(a.txt) must observe its own workspace; \
         expected 'hello-A' in response, got: {content_a}",
    );

    // 5. Session B reads "a.txt" → must FAIL or return a not-found
    //    marker. Session B's workspace is `repo-B`; `a.txt` only
    //    exists under `repo-A`. If the workspace-bound `ToolRegistry`
    //    leaked across sessions, B would observe A's file here.
    let (status_b_cross, body_b_cross) = post_chat(&app, session_b_id, "read_file:a.txt").await;
    assert_eq!(
        status_b_cross,
        StatusCode::OK,
        "request must return 200 — the FAILURE is at the tool layer, \
         carried in the response body; got body={body_b_cross}",
    );
    let content_b_cross = body_b_cross["content"]
        .as_str()
        .unwrap_or_else(|| panic!("session B response missing content field: {body_b_cross}"));
    let content_b_cross_lower = content_b_cross.to_lowercase();
    assert!(
        !content_b_cross.contains("hello-A"),
        "session B MUST NOT observe session A's a.txt content; got: {content_b_cross}",
    );
    assert!(
        content_b_cross_lower.contains("not found")
            || content_b_cross_lower.contains("no such")
            || content_b_cross_lower.contains("outside working directory")
            || content_b_cross_lower.contains("error"),
        "session B's read_file(a.txt) must surface a not-found / error marker \
         (session-A's file is not in session-B's workspace); got: {content_b_cross}",
    );

    // 6. Session B reads its own b.txt → response carries "hello-B".
    let (status_b, body_b) = post_chat(&app, session_b_id, "read_file:b.txt").await;
    assert_eq!(
        status_b,
        StatusCode::OK,
        "session B read_file(b.txt) must return 200; body={body_b}",
    );
    let content_b = body_b["content"]
        .as_str()
        .unwrap_or_else(|| panic!("session B response missing content field: {body_b}"));
    assert!(
        content_b.contains("hello-B"),
        "session B's read_file(b.txt) must observe its own workspace; \
         expected 'hello-B' in response, got: {content_b}",
    );

    // 7. Independent canonical chat history JSONLs under each
    //    session's user_key directory. Layout follows
    //    `persist_chat_message_through_canonical` →
    //    `octos_bus::persist_message_through_canonical_path` →
    //    `<data_dir>/users/<encoded base_key>/sessions/<encoded topic>.jsonl`.
    //    Both files must exist AND contain their respective session's
    //    user prompts — proving the persistence layer is correctly
    //    scoped per `SessionKey`.
    let encoded_a = octos_bus::session::encode_path_component(key_a.base_key());
    let encoded_b = octos_bus::session::encode_path_component(key_b.base_key());
    // `SessionHandle::topic_filename` falls back to `"default"` when
    // the key carries no `#topic` suffix, so a `with_profile_topic(.., "")`
    // key lands in `default.jsonl`. Replicate that here.
    let topic_filename_a = format!(
        "{}.jsonl",
        octos_bus::session::encode_path_component(key_a.topic().unwrap_or("default"))
    );
    let topic_filename_b = format!(
        "{}.jsonl",
        octos_bus::session::encode_path_component(key_b.topic().unwrap_or("default"))
    );
    let jsonl_a: PathBuf = profile_data_dir
        .join("users")
        .join(&encoded_a)
        .join("sessions")
        .join(&topic_filename_a);
    let jsonl_b: PathBuf = profile_data_dir
        .join("users")
        .join(&encoded_b)
        .join("sessions")
        .join(&topic_filename_b);
    assert!(
        jsonl_a.exists(),
        "session A canonical JSONL must exist at {}; check persistence wiring",
        jsonl_a.display()
    );
    assert!(
        jsonl_b.exists(),
        "session B canonical JSONL must exist at {}; check persistence wiring",
        jsonl_b.display()
    );
    assert_ne!(
        jsonl_a, jsonl_b,
        "per-session JSONLs must live under distinct user_key directories",
    );
    let body_a_jsonl = std::fs::read_to_string(&jsonl_a).expect("read session-A JSONL");
    let body_b_jsonl = std::fs::read_to_string(&jsonl_b).expect("read session-B JSONL");
    assert!(
        body_a_jsonl.contains("read_file:a.txt"),
        "session A JSONL must record its own user prompt; got: {body_a_jsonl}",
    );
    assert!(
        body_b_jsonl.contains("read_file:b.txt"),
        "session B JSONL must record its own user prompt; got: {body_b_jsonl}",
    );
    assert!(
        !body_a_jsonl.contains("read_file:b.txt"),
        "session A JSONL must NOT contain session B's prompt — cross-session bleed: {body_a_jsonl}",
    );
    assert!(
        !body_b_jsonl.contains("read_file:a.txt")
            || body_b_jsonl
                .lines()
                .filter(|l| l.contains("read_file:a.txt"))
                .count()
                <= 2,
        "session B JSONL should only carry session B's own a.txt cross-read attempt, \
         not session A's a.txt prompts: {body_b_jsonl}",
    );

    // 8. Per-session `.octos-workspace.toml` policy files. Bootstrapping
    //    each `SessionRuntime` writes a default `WorkspacePolicy` under
    //    `<workspace_root>/.octos-workspace.toml` (idempotent via
    //    `write_workspace_policy_if_absent`). This is the M11 fix for
    //    the live "workspace policy not found" failure surfaced by the
    //    yangmi voice-clone incident on 2026-05-10.
    let policy_a = repo_a.join(octos_agent::WORKSPACE_POLICY_FILE);
    let policy_b = repo_b.join(octos_agent::WORKSPACE_POLICY_FILE);
    assert!(
        policy_a.exists(),
        "session A `.octos-workspace.toml` must exist at {} after `SessionRuntime::bootstrap`",
        policy_a.display(),
    );
    assert!(
        policy_b.exists(),
        "session B `.octos-workspace.toml` must exist at {} after `SessionRuntime::bootstrap`",
        policy_b.display(),
    );

    // 9. Final isolation check: each policy/workspace must contain
    //    EXACTLY its own seeded file, never the sibling's. Catches a
    //    regression where the per-session `ToolRegistry` somehow wrote
    //    to a shared cwd — which would land both `a.txt` and `b.txt`
    //    under one repo.
    assert!(repo_a.join("a.txt").exists());
    assert!(repo_b.join("b.txt").exists());
    assert!(
        !repo_a.join("b.txt").exists(),
        "session-B's b.txt must NOT appear in repo-A (cross-session bleed)",
    );
    assert!(
        !repo_b.join("a.txt").exists(),
        "session-A's a.txt must NOT appear in repo-B (cross-session bleed)",
    );
}
