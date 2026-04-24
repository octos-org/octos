//! Integration test for M8.7 `SubAgentOutputRouter` + `AgentSummaryGenerator`.
//!
//! Exercises the full happy path:
//! 1. A mock sub-agent emits 1 MB of textual output, routed to disk.
//! 2. `AgentSummaryGenerator` ticks three times with a mock cheap-lane LLM.
//! 3. Each tick fires a `HarnessEvent::SubagentProgress`, which updates
//!    `BackgroundTask.runtime_detail` via the `TaskSupervisor`.
//!
//! Run with `cargo test -p octos-agent --test subagent_output_and_summary`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{AgentSummaryGenerator, AppendResult, SubAgentOutputRouter, TaskSupervisor};
use octos_core::Message;
use octos_llm::{
    ChatConfig, ChatResponse, ChatStream, LlmProvider, StopReason, TokenUsage, ToolSpec,
};

struct StepProvider {
    responses: Arc<Vec<String>>,
    call_count: Arc<AtomicU32>,
}

impl StepProvider {
    fn new(responses: Vec<&str>) -> (Self, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        (
            Self {
                responses: Arc::new(responses.into_iter().map(|s| s.to_string()).collect()),
                call_count: Arc::clone(&calls),
            },
            calls,
        )
    }
}

#[async_trait]
impl LlmProvider for StepProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
        let content = self
            .responses
            .get(idx)
            .cloned()
            .unwrap_or_else(|| "continuing work".to_string());
        Ok(ChatResponse {
            content: Some(content),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatStream> {
        unimplemented!("step provider does not stream")
    }

    fn model_id(&self) -> &str {
        "mock-cheap"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn end_to_end_router_and_summary_cover_one_mb_with_three_ticks() {
    let tmp = tempfile::TempDir::new().unwrap();
    let router =
        Arc::new(SubAgentOutputRouter::new(tmp.path()).with_max_bytes_per_task(2 * 1024 * 1024));

    // 1. Write ~1 MB in 1 KB chunks so the router actually streams.
    let session_id = "api:integration";
    let task_id = "task-int-1";
    let chunk = vec![b'X'; 1024];
    let mut total = 0u64;
    for i in 0..1024 {
        let mut line = format!("chunk-{i:04}: ").into_bytes();
        line.extend_from_slice(&chunk);
        line.push(b'\n');
        let r = router.append(session_id, task_id, &line).unwrap();
        assert_eq!(r, AppendResult::Ok);
        total += line.len() as u64;
    }
    assert!(total >= 1024 * 1024);
    assert_eq!(router.bytes_written(task_id), total);

    // 2. Wire the summary generator to a mock provider that returns three
    //    distinct summaries.
    let (provider, call_count) = StepProvider::new(vec![
        "indexing incoming chunks",
        "compressing buffered payload",
        "uploading artifact",
    ]);
    let supervisor = TaskSupervisor::new();
    let sup_task_id = supervisor.register("mock_worker", "call-1", Some(session_id));
    supervisor.mark_running(&sup_task_id);
    let generator =
        AgentSummaryGenerator::new(Arc::new(provider), Arc::clone(&router), supervisor.clone())
            .with_llm_timeout(Duration::from_secs(2));

    // 3. Run three ticks synchronously — this exercises the router tail,
    //    the LLM call, the harness event emit, and the runtime_detail fold.
    let s1 = generator
        .summarize_once(session_id, &sup_task_id, 1)
        .await
        .expect("first summary");
    let s2 = generator
        .summarize_once(session_id, &sup_task_id, 2)
        .await
        .expect("second summary");
    let s3 = generator
        .summarize_once(session_id, &sup_task_id, 3)
        .await
        .expect("third summary");

    assert_eq!(s1, "indexing incoming chunks");
    assert_eq!(s2, "compressing buffered payload");
    assert_eq!(s3, "uploading artifact");
    assert_eq!(call_count.load(Ordering::SeqCst), 3);

    // runtime_detail reflects the latest summary.
    let task = supervisor.get_task(&sup_task_id).unwrap();
    let detail: serde_json::Value =
        serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["kind"], "subagent_progress");
    assert_eq!(detail["summary"], "uploading artifact");
    assert_eq!(detail["tick"], 3);
    // coarse lifecycle state stays Running while summaries fire
    assert_eq!(task.status, octos_agent::TaskStatus::Running);

    // router has the data on disk
    let disk_path = router.path_for(session_id, task_id);
    assert!(disk_path.exists());
    let meta = std::fs::metadata(&disk_path).unwrap();
    assert_eq!(meta.len(), total);
}
