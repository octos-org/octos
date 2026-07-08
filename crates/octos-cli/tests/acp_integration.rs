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
