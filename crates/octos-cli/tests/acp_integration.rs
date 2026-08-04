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
    ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    SessionNotification, SessionUpdate, StopReason, TextContent,
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

/// Pull the text out of a user-message `session/update`, if that's what it is.
fn user_message_text(update: &SessionUpdate) -> Option<String> {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

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

    // Read stdout in a thread and report each response promptly. Waiting for
    // a response rather than sleeping for a fixed startup interval avoids
    // racing a cold binary start on loaded CI workers.
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !line.trim().is_empty() && line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut lines = Vec::new();
    while lines.len() < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match line_rx.recv_timeout(remaining) {
            Ok(line) => lines.push(line),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait(); // reap the child so it can't linger as a zombie
    drop(line_rx);
    let _ = handle.join();

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

/// A conversation must survive the process that created it.
///
/// ACP sessions were memory-only: `AcpSession.history` was a `Mutex<Vec<Message>>`
/// and nothing wrote it anywhere. A kill -9 or a supervisor restart left the agent
/// with no idea what it had been doing, while the fleet reported it healthy —
/// silent amnesia mid-task, which is worse than staying down.
///
/// This drives two independent transports over one store: the first holds a
/// conversation, the second loads that session id and must see the earlier turns.
#[tokio::test]
async fn should_restore_a_conversation_through_session_load() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&memory_dir).unwrap();

    // ---- first process: one turn, then drop the transport entirely ----
    let seen_one: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm_one: Arc<dyn octos_llm::LlmProvider> = Arc::new(RecordingLlm {
        seen: seen_one.clone(),
        replies: vec!["REMEMBER_THIS".to_string()],
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let factory_one = TestAgentFactory::new(llm_one, memory_dir.clone(), cwd.clone())
        .with_session_store(&sessions_dir);

    let cwd_one = cwd.clone();
    let session_id = Client
        .builder()
        .name("octos-acp-persist-1")
        .on_receive_notification(
            async move |_n: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory_one),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(cwd_one.clone()))
                    .block_task()
                    .await?;
                let id = new_session.session_id.clone();
                connection
                    .send_request(PromptRequest::new(
                        id.clone(),
                        vec![ContentBlock::from("FIRST_QUESTION")],
                    ))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(id)
            },
        )
        .await
        .expect("first session");

    // ---- second process: same store, same id, fresh everything else ----
    let seen_two: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm_two: Arc<dyn octos_llm::LlmProvider> = Arc::new(RecordingLlm {
        seen: seen_two.clone(),
        replies: vec!["SECOND_REPLY".to_string()],
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let factory_two =
        TestAgentFactory::new(llm_two, memory_dir, cwd.clone()).with_session_store(&sessions_dir);

    let cwd_two = cwd.clone();
    let id_two = session_id.clone();
    Client
        .builder()
        .name("octos-acp-persist-2")
        .on_receive_notification(
            async move |_n: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory_two),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(LoadSessionRequest::new(id_two.clone(), cwd_two.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        id_two.clone(),
                        vec![ContentBlock::from("SECOND_QUESTION")],
                    ))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            },
        )
        .await
        .expect("second session");

    // The second process's LLM must have been handed the first turn as context.
    let calls = seen_two.lock().await;
    let last = calls.last().expect("the reloaded session ran a turn");
    let joined = last.join("\n");
    assert!(
        joined.contains("FIRST_QUESTION"),
        "session/load did not restore the earlier user turn; saw: {joined}"
    );
    assert!(
        joined.contains("REMEMBER_THIS"),
        "session/load did not restore the earlier assistant turn; saw: {joined}"
    );
}

/// #1909: `session/load` must REPLAY the restored transcript as `session/update`
/// notifications — the response carries no history, so without a replay a
/// reconnecting client renders an empty conversation even though the agent
/// restored it (the client then prompts from zero context and the agent
/// answers as if it forgot everything).
///
/// Same two-transport shape as `should_restore_a_conversation_through_session_load`,
/// but the second client RECORDS the updates and asserts the earlier turns were
/// replayed by the time `session/load` responded.
#[tokio::test]
async fn should_replay_stored_history_as_session_updates_on_load() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&memory_dir).unwrap();

    // ---- first process: one persisted turn ----
    let llm_one: Arc<dyn octos_llm::LlmProvider> = Arc::new(MockLlm {
        reply: "REMEMBER_THIS".to_string(),
    });
    let factory_one = TestAgentFactory::new(llm_one, memory_dir.clone(), cwd.clone())
        .with_session_store(&sessions_dir);

    let cwd_one = cwd.clone();
    let session_id = Client
        .builder()
        .name("octos-acp-replay-1")
        .on_receive_notification(
            async move |_n: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory_one),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(cwd_one.clone()))
                    .block_task()
                    .await?;
                let id = new_session.session_id.clone();
                connection
                    .send_request(PromptRequest::new(
                        id.clone(),
                        vec![ContentBlock::from("FIRST_QUESTION")],
                    ))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(id)
            },
        )
        .await
        .expect("first session");

    // ---- second process: same store, load, recording every session/update ----
    let llm_two: Arc<dyn octos_llm::LlmProvider> = Arc::new(MockLlm {
        reply: "SECOND_REPLY".to_string(),
    });
    let factory_two =
        TestAgentFactory::new(llm_two, memory_dir, cwd.clone()).with_session_store(&sessions_dir);

    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = updates.clone();
    let id_two = session_id.clone();
    let cwd_two = cwd.clone();
    Client
        .builder()
        .name("octos-acp-replay-2")
        .on_receive_notification(
            async move |n: SessionNotification, _cx: ConnectionTo<agent_client_protocol::Agent>| {
                updates_for_handler.lock().await.push(n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory_two),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // By the time session/load RESPONDS, the replay must have
                // landed — the frames precede the response on the connection.
                connection
                    .send_request(LoadSessionRequest::new(id_two.clone(), cwd_two.clone()))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            },
        )
        .await
        .expect("second session");

    let recorded = updates.lock().await;
    let user_texts: Vec<String> = recorded.iter().filter_map(user_message_text).collect();
    let agent_texts: Vec<String> = recorded.iter().filter_map(agent_message_text).collect();
    assert!(
        user_texts.iter().any(|t| t.contains("FIRST_QUESTION")),
        "session/load must replay the user turn as a UserMessageChunk; recorded: {recorded:?}"
    );
    assert!(
        agent_texts.iter().any(|t| t.contains("REMEMBER_THIS")),
        "session/load must replay the assistant turn as an AgentMessageChunk; recorded: {recorded:?}"
    );
}

/// `session/load` must not evict a session that is already live.
///
/// A bare insert dropped the existing entry and with it the `shutdown` flag an
/// in-flight turn watches, so a `session/cancel` for that turn flipped a flag
/// nobody read and cancellation silently stopped working. Loading an id the agent
/// already holds is a no-op.
#[tokio::test]
async fn should_not_evict_a_live_session_on_reload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(RecordingLlm {
        seen: seen.clone(),
        replies: vec!["FIRST".to_string(), "SECOND".to_string()],
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let factory =
        TestAgentFactory::new(llm, memory_dir, cwd.clone()).with_session_store(&sessions_dir);

    let prompt_cwd = cwd.clone();
    Client
        .builder()
        .name("octos-acp-reload-client")
        .on_receive_notification(
            async move |_n: SessionNotification,
                        _cx: ConnectionTo<agent_client_protocol::Agent>| { Ok(()) },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let new_session = connection
                    .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                    .block_task()
                    .await?;
                let id = new_session.session_id;

                connection
                    .send_request(PromptRequest::new(
                        id.clone(),
                        vec![ContentBlock::from("TURN_ONE")],
                    ))
                    .block_task()
                    .await?;

                // Reload the id the agent is already holding.
                connection
                    .send_request(LoadSessionRequest::new(id.clone(), prompt_cwd.clone()))
                    .block_task()
                    .await?;

                // The session must still work, and still carry its earlier turn.
                connection
                    .send_request(PromptRequest::new(
                        id.clone(),
                        vec![ContentBlock::from("TURN_TWO")],
                    ))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            },
        )
        .await
        .expect("reload should not break the session");

    let calls = seen.lock().await;
    let last = calls.last().expect("second turn ran");
    let joined = last.join("\n");
    assert!(
        joined.contains("TURN_ONE"),
        "reloading a live session lost its history; saw: {joined}"
    );
}

/// #1909: a stored transcript must be SANITIZED on load, before it touches the
/// LLM or the client. The store is append-only JSONL, so a crash can leave an
/// assistant tool_call whose result never landed — feeding that back to the
/// provider is a 400 ("tool_use without tool_result"), and replaying it renders
/// a tool card that never completes in the client. Every other resume path runs
/// `ResumePolicy`; ACP's `session/load` must too.
#[tokio::test]
async fn should_sanitize_stored_history_when_loading_a_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let session_id = agent_client_protocol::schema::v1::SessionId::new("octos-seeded");
    let key = octos_core::SessionKey::with_profile(
        octos_core::MAIN_PROFILE_ID,
        "acp",
        session_id.0.as_ref(),
    );

    // Seed the store directly: one healthy user + assistant pair, one assistant
    // tool_call with NO matching result (crash residue), one whitespace-only
    // assistant row. All thread-stamped so the fail-closed write path accepts
    // them.
    {
        let mut mgr = octos_bus::session::SessionManager::open(&sessions_dir).expect("open store");
        let msg = |role: octos_core::MessageRole, content: &str| octos_core::Message {
            role,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("t-seed".into()),
            timestamp: chrono::Utc::now(),
        };
        mgr.add_message(&key, msg(octos_core::MessageRole::User, "SEED_USER"))
            .await
            .expect("user row");
        let mut ghost = msg(octos_core::MessageRole::Assistant, "");
        ghost.tool_calls = Some(vec![octos_core::ToolCall {
            id: "ghost-call".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }]);
        mgr.add_message(&key, ghost)
            .await
            .expect("ghost tool-call row");
        mgr.add_message(&key, msg(octos_core::MessageRole::Assistant, "   "))
            .await
            .expect("whitespace row");
        mgr.add_message(
            &key,
            msg(octos_core::MessageRole::Assistant, "SEED_ASSISTANT"),
        )
        .await
        .expect("assistant row");
    }

    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(RecordingLlm {
        seen: seen.clone(),
        replies: vec!["AFTER_LOAD".to_string()],
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let factory =
        TestAgentFactory::new(llm, memory_dir, cwd.clone()).with_session_store(&sessions_dir);

    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_for_handler = updates.clone();
    let id_for_client = session_id.clone();
    let cwd_for_client = cwd.clone();
    Client
        .builder()
        .name("octos-acp-sanitize-client")
        .on_receive_notification(
            async move |n: SessionNotification, _cx: ConnectionTo<agent_client_protocol::Agent>| {
                updates_for_handler.lock().await.push(n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            OctosAcpAgentTransport::new(factory),
            |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                connection
                    .send_request(LoadSessionRequest::new(
                        id_for_client.clone(),
                        cwd_for_client.clone(),
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        id_for_client.clone(),
                        vec![ContentBlock::from("NEXT_QUESTION")],
                    ))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>(())
            },
        )
        .await
        .expect("load + prompt");

    // The replay must not contain a ToolCall frame for the unresolved call —
    // the client would render a tool card that never completes.
    let recorded = updates.lock().await;
    assert!(
        !recorded
            .iter()
            .any(|u| matches!(u, SessionUpdate::ToolCall(_))),
        "the unresolved tool call must be sanitized away before replay; recorded: {recorded:?}"
    );
    let user_texts: Vec<String> = recorded.iter().filter_map(user_message_text).collect();
    let agent_texts: Vec<String> = recorded.iter().filter_map(agent_message_text).collect();
    assert!(
        user_texts.iter().any(|t| t.contains("SEED_USER")),
        "healthy rows still replay; recorded: {recorded:?}"
    );
    assert!(
        agent_texts.iter().any(|t| t.contains("SEED_ASSISTANT")),
        "healthy rows still replay; recorded: {recorded:?}"
    );

    // And the LLM must never see the orphan rows on the next prompt: no
    // empty/whitespace-only payloads in the handed-over transcript.
    let calls = seen.lock().await;
    let last = calls.last().expect("a turn ran after load");
    assert!(
        last.iter().any(|c| c.contains("SEED_USER"))
            && last.iter().any(|c| c.contains("SEED_ASSISTANT")),
        "healthy rows reach the LLM: {last:?}"
    );
    let blanks = last.iter().filter(|c| c.trim().is_empty()).count();
    assert_eq!(
        blanks, 0,
        "sanitized history must not hand the LLM payload-free rows: {last:?}"
    );
}
