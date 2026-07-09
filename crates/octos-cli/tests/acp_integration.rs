//! End-to-end integration test for the `octos acp` bridge.
//!
//! Drives the real ACP agent (the exact handler wiring `octos acp` uses) with a
//! `MockLlm` — a canned [`LlmProvider`] that returns a fixed assistant reply —
//! through the full `initialize -> session/new -> session/prompt` round-trip,
//! and asserts the streamed `session/update` sequence plus the final stop
//! reason.
//!
//! No network, no subprocess, no OS pipes: the ACP client and the octos ACP
//! agent are wired together **in-process**. [`OctosAcpAgentTransport`] exposes
//! the octos agent as a `ConnectTo<Client>` transport; the client's
//! `connect_with` closure drives the protocol while the agent's handlers run in
//! the background — all on one tokio runtime, exchanging typed JSON-RPC messages
//! directly.

use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Client, ConnectionTo};

use async_trait::async_trait;
use octos_cli::commands::{OctosAcpAgentTransport, TestAgentFactory};
use tokio::sync::Mutex;

/// A canned LLM that always returns the same assistant text and ends the turn —
/// mirrors the `MockLlm` pattern from `chat.rs`'s unit tests.
struct MockLlm {
    reply: String,
}

#[async_trait]
impl octos_llm::LlmProvider for MockLlm {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some(self.reply.clone()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn provider_name(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "mock-1"
    }
}

/// A `MockLlm` that returns a per-call assistant reply and records, for every
/// `chat()` call, the full set of message *contents* it was handed (system +
/// accumulated history + the current user turn). The integration test inspects
/// these snapshots to prove multi-turn history accumulates across prompts.
struct RecordingLlm {
    /// One entry per `chat()` call: the `content` of every incoming message.
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    /// Assistant reply text keyed by call index (0-based); falls back to a
    /// generic reply if the call index is beyond the vec.
    replies: Vec<String>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl octos_llm::LlmProvider for RecordingLlm {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let idx = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen
            .lock()
            .await
            .push(messages.iter().map(|m| m.content.clone()).collect());
        let reply = self
            .replies
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("reply-{idx}"));
        Ok(octos_llm::ChatResponse {
            content: Some(reply),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn provider_name(&self) -> &str {
        "recording-mock"
    }

    fn model_id(&self) -> &str {
        "recording-1"
    }
}

/// Pull the text out of an assistant-message `session/update`, if that's what it
/// is.
fn agent_message_text(update: &SessionUpdate) -> Option<String> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_stream_assistant_message_and_end_turn_when_driven_through_acp_initialize_new_session_prompt()
 {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let reply = "Hello from octos ACP";
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(MockLlm {
        reply: reply.to_string(),
    });
    let factory = TestAgentFactory::new(llm, memory_dir, cwd.clone());
    let transport = OctosAcpAgentTransport::new(factory);

    // Records every `session/update` the agent streams to the client.
    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = updates.clone();

    // The captured stop reason from the prompt turn.
    let stop_reason: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
    let stop_reason_for_main = stop_reason.clone();

    let prompt_cwd = cwd.clone();

    // Build the in-process ACP CLIENT and drive the protocol from its
    // `connect_with` closure. The octos ACP agent is the transport.
    let client_result = Client
        .builder()
        .name("octos-acp-test-client")
        .on_receive_notification(
            async move |notif: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| {
                updates_for_handler.lock().await.push(notif.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            transport,
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                // 1) initialize
                let init = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V1);
                // octos advertises text-only prompt capabilities.
                assert!(!init.agent_capabilities.prompt_capabilities.image);

                // 2) session/new
                let new_session = connection
                    .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;

                // 3) session/prompt
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("hello"))],
                    ))
                    .block_task()
                    .await?;

                *stop_reason_for_main.lock().await = Some(prompt.stop_reason);
                Ok(())
            },
        )
        .await;

    client_result.expect("ACP client run should complete cleanly");

    // The turn ended naturally. `StopReason` is `Copy`, so deref instead of clone.
    let got_stop = *stop_reason.lock().await;
    assert!(
        matches!(got_stop, Some(StopReason::EndTurn)),
        "expected StopReason::EndTurn, got {got_stop:?}"
    );

    // The assistant's canned reply was streamed as an AgentMessageChunk.
    let recorded = updates.lock().await;
    let assistant_texts: Vec<String> = recorded.iter().filter_map(agent_message_text).collect();
    assert!(
        assistant_texts.iter().any(|t| t.contains(reply)),
        "expected an AgentMessageChunk containing {reply:?}; recorded updates: {recorded:?}"
    );
}

/// Multi-turn history must ACCUMULATE across prompts: turn N's `process_message`
/// must see the messages from all earlier turns. This is driven over the real
/// ACP handler wiring (`initialize -> session/new -> prompt x3`) with a
/// `RecordingLlm` that snapshots the messages it is handed each turn.
///
/// `ConversationResponse.messages` for a text-only (no-tool) `EndTurn` is just
/// the turn's user message (the assistant's final text is streamed live and
/// carried in `content`, not re-persisted as a history `Message`), so history
/// accumulation is observable via the USER turns surviving.
///
/// The buggy `*h = resp.messages` (replace) only surfaces from the THIRD turn:
/// after turn 1 the stored history is [user1] and after turn 2 it is REPLACED
/// by [user2], dropping user1 — so turn 3 would no longer see user1. A
/// two-prompt drive can't catch it (turn 2 still sees user1 under the bug); we
/// therefore drive three prompts and assert turn 3 still sees user1, and that
/// the incoming message count grows every turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_accumulate_conversation_history_across_multiple_prompts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    // Distinct, greppable replies so we can assert turn-1 content survives.
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(RecordingLlm {
        seen: seen.clone(),
        replies: vec![
            "ASSISTANT_ONE".to_string(),
            "ASSISTANT_TWO".to_string(),
            "ASSISTANT_THREE".to_string(),
        ],
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let factory = TestAgentFactory::new(llm, memory_dir, cwd.clone());
    let transport = OctosAcpAgentTransport::new(factory);

    let prompt_cwd = cwd.clone();

    Client
        .builder()
        .name("octos-acp-history-client")
        .on_receive_notification(
            async move |_notif: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| { Ok(()) },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            transport,
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                    .block_task()
                    .await?;
                let session_id = new_session.session_id;

                for user in ["USER_ONE", "USER_TWO", "USER_THREE"] {
                    let prompt = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new(user))],
                        ))
                        .block_task()
                        .await?;
                    assert!(
                        matches!(prompt.stop_reason, StopReason::EndTurn),
                        "each turn should EndTurn; got {:?}",
                        prompt.stop_reason
                    );
                }
                Ok(())
            },
        )
        .await
        .expect("ACP client run should complete cleanly");

    let snapshots = seen.lock().await;
    assert_eq!(
        snapshots.len(),
        3,
        "exactly one chat() call per prompt turn"
    );

    // Flatten each turn's incoming messages into one blob for content checks.
    let blob = |i: usize| snapshots[i].join("\n");

    // Turn 1 sees only turn-1's user prompt (no prior turns).
    assert!(blob(0).contains("USER_ONE"), "turn 1 sees its own user msg");
    assert!(
        !blob(0).contains("USER_TWO"),
        "turn 1 cannot see a future turn's user msg"
    );

    // Turn 3 MUST still see turn-1's user prompt — the core regression:
    // replacing (instead of appending) history drops user1 by turn 3.
    let t3 = blob(2);
    assert!(
        t3.contains("USER_ONE"),
        "turn 3 must still see turn 1's user msg; history was dropped. turn-3 messages: {:?}",
        snapshots[2]
    );
    assert!(
        t3.contains("USER_TWO"),
        "turn 3 must also see turn 2's user msg. turn-3 messages: {:?}",
        snapshots[2]
    );
    assert!(t3.contains("USER_THREE"), "turn 3 sees its own user msg");

    // codex round-2 regression: the ASSISTANT reply must also persist. A
    // text-only turn carries the final reply in `resp.content`, NOT in
    // `resp.messages`, so without explicitly persisting it the agent remembers
    // what the USER said but not what IT answered. Turns 2 and 3 must see turn
    // 1's assistant reply ("ASSISTANT_ONE").
    assert!(
        blob(1).contains("ASSISTANT_ONE"),
        "turn 2 must see turn 1's ASSISTANT reply; assistant text not persisted. turn-2 messages: {:?}",
        snapshots[1]
    );
    assert!(
        t3.contains("ASSISTANT_ONE") && t3.contains("ASSISTANT_TWO"),
        "turn 3 must see the assistant replies from turns 1 and 2. turn-3 messages: {:?}",
        snapshots[2]
    );

    // The accumulated history the LLM receives grows every turn (user + assistant
    // per turn): [sys, u1] -> [sys, u1, a1, u2] -> [sys, u1, a1, u2, a2, u3].
    assert!(
        snapshots[0].len() < snapshots[1].len() && snapshots[1].len() < snapshots[2].len(),
        "incoming message count must grow across turns: {} < {} < {}",
        snapshots[0].len(),
        snapshots[1].len(),
        snapshots[2].len()
    );
}

/// Regression: `octos acp` speaks ACP JSON-RPC on stdout, so NOTHING else may be
/// written there — a single stray log line makes strict clients (Zed) reject the
/// whole stream with a `-32700` parse error. octos's tracing previously defaulted
/// to stdout for no-log-dir commands; this drives the REAL binary through
/// `initialize` + `session/new` (which loads config → emits startup logs) and
/// asserts every stdout line is valid JSON.
#[test]
fn should_emit_only_valid_json_on_stdout_when_running_acp() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let tmp = tempfile::tempdir().expect("tempdir");
    // Fully isolate the child from the developer/CI account: its data, config,
    // and auth dirs resolve to temp subdirs so the test can neither read nor
    // mutate real user state (codex). A fresh config home also still exercises
    // startup logging under RUST_LOG=info, so a stdout leak would surface.
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_octos"))
        .args([
            "acp",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-chat",
            "--cwd",
            tmp.path().to_str().unwrap(),
        ])
        .env("RUST_LOG", "info") // force startup logging so a leak would show
        .env("HOME", &home)
        .env("OCTOS_HOME", &data)
        .env("OCTOS_CONFIG_DIR", &config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn octos acp");

    let mut stdin = child.stdin.take().unwrap();
    // initialize + session/new — the latter triggers config load (the logs that
    // used to leak). No LLM call, so no provider key is needed.
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1,\"clientCapabilities\":{}}}\n")
        .unwrap();
    stdin
        .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{{\"cwd\":\"{}\",\"mcpServers\":[]}}}}\n", tmp.path().to_str().unwrap()).as_bytes())
        .unwrap();
    stdin.flush().unwrap();

    // Read stdout in a thread; stop after a couple of responses or a short wait.
    let stdout = child.stdout.take().unwrap();
    let handle = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                lines.push(line);
            }
            if lines.len() >= 2 {
                break;
            }
        }
        lines
    });
    std::thread::sleep(std::time::Duration::from_secs(3));
    let _ = child.kill();
    let _ = child.wait(); // reap the child so it can't linger as a zombie
    let lines = handle.join().unwrap_or_default();

    assert!(
        !lines.is_empty(),
        "octos acp should have emitted at least the initialize/session responses on stdout"
    );
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
            panic!("non-JSON line on the ACP stdout stream (would -32700 in Zed): {line:?} ({e})")
        });
    }
}
