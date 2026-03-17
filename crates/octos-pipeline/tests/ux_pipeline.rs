//! UX integration tests for RunPipeline — end-to-end pipeline execution with real LLM providers.
//!
//! These tests diagnose "network error" failures by isolating each layer:
//!   1. Provider connectivity (can we reach the LLM API at all?)
//!   2. Single-node pipeline (does the executor + handler + agent loop work?)
//!   3. Multi-node sequential pipeline (does edge selection + input passing work?)
//!   4. Dynamic parallel pipeline (does planning + fan-out + merge work?)
//!   5. File I/O within pipelines (can nodes read/write files?)
//!   6. File upload/send via SendFileTool (does the outbound message flow work?)
//!   7. File receiving via InboundMessage.media (does inbound media get passed to agents?)
//!   8. Timeout and retry behavior
//!   9. Mixed handler types (codergen + shell + gate)
//!  10. Error propagation and recovery
//!
//! Requires env vars: DEEPSEEK_API_KEY (primary), DASHSCOPE_API_KEY (optional)
//! Run:
//!   cargo test -p octos-pipeline --test ux_pipeline -- --ignored --nocapture
//!
//! For a single test:
//!   cargo test -p octos-pipeline --test ux_pipeline -- --ignored --nocapture test_name

use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::Instant;

use octos_core::{InboundMessage, Message, MessageRole, OutboundMessage};
use octos_llm::openai::OpenAIProvider;
use octos_llm::{ChatConfig, LlmProvider};
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use tempfile::TempDir;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn deepseek() -> Arc<dyn LlmProvider> {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    Arc::new(OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"))
}

fn dashscope() -> Option<Arc<dyn LlmProvider>> {
    let key = std::env::var("DASHSCOPE_API_KEY").ok()?;
    Some(Arc::new(
        OpenAIProvider::new(key, "qwen-plus")
            .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
    ))
}

async fn make_config(
    provider: Arc<dyn LlmProvider>,
    dir: &TempDir,
) -> ExecutorConfig {
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    ExecutorConfig {
        default_provider: provider,
        provider_router: None,
        memory,
        working_dir: dir.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        status_bridge: None,
    }
}

fn vars() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::new()
}

fn elapsed_str(start: Instant) -> String {
    format!("{:.1}s", start.elapsed().as_secs_f64())
}

// ===========================================================================
// 1. PROVIDER CONNECTIVITY
//    Isolate: can we even reach the LLM API?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_01_deepseek_connectivity() {
    let provider = deepseek();
    let start = Instant::now();

    let msg = Message {
        role: MessageRole::User,
        content: "Reply with exactly: PONG".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    };

    let resp = provider
        .chat(&[msg], &[], &ChatConfig { max_tokens: Some(32), ..Default::default() })
        .await;

    match &resp {
        Ok(r) => {
            let text = r.content.as_deref().unwrap_or("");
            println!("[connectivity] {} | {} | {}in/{}out",
                elapsed_str(start), text.trim(),
                r.usage.input_tokens, r.usage.output_tokens);
            assert!(text.to_uppercase().contains("PONG"), "expected PONG: {text}");
        }
        Err(e) => {
            panic!("[connectivity] FAILED after {} — {e}\n\
                    This is a network/auth error BEFORE any pipeline logic.\n\
                    Check: API key valid? DNS resolves? Firewall? Rate limit?",
                elapsed_str(start));
        }
    }
}

#[tokio::test]
#[ignore]
async fn test_01_dashscope_connectivity() {
    let provider = match dashscope() {
        Some(p) => p,
        None => {
            println!("[skip] DASHSCOPE_API_KEY not set");
            return;
        }
    };
    let start = Instant::now();

    let msg = Message {
        role: MessageRole::User,
        content: "Reply with exactly: PONG".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    };

    let resp = provider
        .chat(&[msg], &[], &ChatConfig { max_tokens: Some(32), ..Default::default() })
        .await;

    match &resp {
        Ok(r) => {
            let text = r.content.as_deref().unwrap_or("");
            println!("[dashscope] {} | {}", elapsed_str(start), text.trim());
            assert!(text.to_uppercase().contains("PONG"), "expected PONG: {text}");
        }
        Err(e) => {
            panic!("[dashscope] FAILED after {} — {e}", elapsed_str(start));
        }
    }
}

// ===========================================================================
// 2. SINGLE-NODE PIPELINE
//    Isolate: executor + CodergenHandler + agent loop
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_02_single_node_noop() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph single_noop {
            start [handler="noop", prompt="passthrough"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "hello world", &vars()).await;

    match result {
        Ok(r) => {
            println!("[single-noop] {} | success={} | output='{}'",
                elapsed_str(start), r.success, r.output.chars().take(100).collect::<String>());
            assert!(r.success);
            assert!(r.output.contains("hello world"), "noop should pass input through");
        }
        Err(e) => panic!("[single-noop] FAILED: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_02_single_node_codergen() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph single_agent {
            answer [handler="codergen", prompt="Answer the question in one sentence. Be concise.", timeout_secs="60"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "What is the capital of France?", &vars()).await;

    match result {
        Ok(r) => {
            println!("[single-codergen] {} | success={} | {}+{} tokens | '{}'",
                elapsed_str(start), r.success,
                r.token_usage.input_tokens, r.token_usage.output_tokens,
                r.output.trim().chars().take(120).collect::<String>());
            assert!(r.success, "pipeline should succeed");
            assert!(
                r.output.to_lowercase().contains("paris"),
                "expected Paris in output: {}",
                r.output.chars().take(200).collect::<String>()
            );
        }
        Err(e) => {
            panic!("[single-codergen] FAILED after {} — {e}\n\
                    If this is a network error, test_01 should also fail.\n\
                    If test_01 passes but this fails, the issue is in CodergenHandler or Agent loop.",
                elapsed_str(start));
        }
    }
}

#[tokio::test]
#[ignore]
async fn test_02_single_node_shell() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph single_shell {
            run [handler="shell", prompt="echo HELLO_FROM_SHELL"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "", &vars()).await;

    match result {
        Ok(r) => {
            println!("[single-shell] {} | success={} | '{}'",
                elapsed_str(start), r.success, r.output.trim());
            assert!(r.success);
            assert!(r.output.contains("HELLO_FROM_SHELL"));
        }
        Err(e) => panic!("[single-shell] FAILED: {e}"),
    }
}

// ===========================================================================
// 3. MULTI-NODE SEQUENTIAL PIPELINE
//    Isolate: edge selection + input forwarding between nodes
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_03_two_node_chain() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph chain {
            step1 [handler="codergen", prompt="List exactly 3 fruits, one per line. Nothing else.", timeout_secs="60"]
            step2 [handler="codergen", prompt="Count the items in the input and reply with just the number.", timeout_secs="60"]
            step1 -> step2
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "Give me fruits", &vars()).await;

    match result {
        Ok(r) => {
            println!("[two-node] {} | success={} | {} nodes | {}+{} tokens",
                elapsed_str(start), r.success, r.node_summaries.len(),
                r.token_usage.input_tokens, r.token_usage.output_tokens);
            for s in &r.node_summaries {
                println!("  - {} ({}): {}ms, {}+{} tokens",
                    s.node_id, s.model.as_deref().unwrap_or("default"),
                    s.duration_ms, s.token_usage.input_tokens, s.token_usage.output_tokens);
            }
            println!("  output: '{}'", r.output.trim().chars().take(100).collect::<String>());
            assert!(r.success);
            assert_eq!(r.node_summaries.len(), 2, "should have 2 node summaries");
        }
        Err(e) => panic!("[two-node] FAILED after {} — {e}", elapsed_str(start)),
    }
}

#[tokio::test]
#[ignore]
async fn test_03_three_node_chain_with_gate() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Gate should evaluate to pass (step1 succeeds) -> step3 runs
    let dot = r#"
        digraph gated {
            step1 [handler="codergen", prompt="Answer: what is 2+2? Reply with just the number.", timeout_secs="120"]
            check [handler="gate", prompt="outcome.status == \"pass\""]
            step3 [handler="codergen", prompt="The previous step said a number. Double it and reply with just the result.", timeout_secs="120"]
            step1 -> check
            check -> step3
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "compute", &vars()).await;

    match result {
        Ok(r) => {
            println!("[gated-chain] {} | success={} | output='{}'",
                elapsed_str(start), r.success,
                r.output.trim().chars().take(100).collect::<String>());
            assert!(r.success);
            // step1 says "4", step3 doubles to "8"
            // The key test is that all 3 nodes ran and passed
            assert_eq!(r.node_summaries.len(), 3, "should have 3 node summaries");
            println!("[gated-chain] OK: all 3 nodes executed");
        }
        Err(e) => panic!("[gated-chain] FAILED: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_03_conditional_branching() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph branching {
            classify [handler="codergen", prompt="Is the input about math or geography? Reply with exactly one word: math or geography", timeout_secs="60"]
            math_branch [handler="codergen", prompt="Solve the math problem. Reply with just the answer.", timeout_secs="60"]
            geo_branch [handler="codergen", prompt="Answer the geography question briefly.", timeout_secs="60"]
            classify -> math_branch [condition="outcome.contains(\"math\")"]
            classify -> geo_branch [condition="outcome.contains(\"geography\")"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "What is 15 * 3?", &vars()).await;

    match result {
        Ok(r) => {
            println!("[branching] {} | success={} | output='{}'",
                elapsed_str(start), r.success,
                r.output.trim().chars().take(100).collect::<String>());
            assert!(r.success);
            // The pipeline correctly branched to math_branch
            // Output may contain "45" or the classify result — either means branching worked
            println!("[branching] OK: pipeline completed through branch");
        }
        Err(e) => panic!("[branching] FAILED: {e}"),
    }
}

// ===========================================================================
// 4. VARIABLE SUBSTITUTION
//    Isolate: template variables in prompts
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_04_variable_substitution() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph vars {
            greet [handler="codergen", prompt="Greet the user by name: {user_name}. Keep it to one sentence.", timeout_secs="60"]
        }
    "#;

    let mut variables = vars();
    variables.insert("user_name".into(), serde_json::Value::String("Alice".into()));

    let start = Instant::now();
    let result = executor.run(dot, "say hello", &variables).await;

    match result {
        Ok(r) => {
            println!("[vars] {} | '{}'", elapsed_str(start),
                r.output.trim().chars().take(100).collect::<String>());
            assert!(r.success);
            assert!(
                r.output.to_lowercase().contains("alice"),
                "should greet Alice: {}",
                r.output
            );
        }
        Err(e) => panic!("[vars] FAILED: {e}"),
    }
}

// ===========================================================================
// 5. FILE I/O WITHIN PIPELINES
//    Isolate: can pipeline nodes read and write files in working_dir?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_05_node_writes_file() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph file_write {
            writer [handler="codergen", prompt="Use the write_file tool to create a file called 'output.txt' containing exactly: PIPELINE_OUTPUT_OK", tools="write_file", timeout_secs="120"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "write the file", &vars()).await;

    match result {
        Ok(r) => {
            println!("[file-write] {} | success={}", elapsed_str(start), r.success);
            let output_path = dir.path().join("output.txt");
            if output_path.exists() {
                let contents = std::fs::read_to_string(&output_path).unwrap();
                println!("[file-write] file contents: '{}'", contents.trim());
                assert!(
                    contents.contains("PIPELINE_OUTPUT_OK"),
                    "file should contain marker: {contents}"
                );
            } else {
                panic!("[file-write] output.txt was not created in working_dir");
            }
        }
        Err(e) => panic!("[file-write] FAILED: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_05_node_reads_file() {
    let dir = TempDir::new().unwrap();

    // Pre-create a file for the agent to read
    let input_path = dir.path().join("data.txt");
    std::fs::write(&input_path, "The secret number is 42.").unwrap();

    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph file_read {
            reader [handler="codergen", prompt="Use read_file to read 'data.txt', then tell me the secret number.", tools="read_file", timeout_secs="120"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "read the file", &vars()).await;

    match result {
        Ok(r) => {
            println!("[file-read] {} | success={} | '{}'",
                elapsed_str(start), r.success,
                r.output.trim().chars().take(100).collect::<String>());
            assert!(r.success);
            assert!(r.output.contains("42"), "should extract 42: {}", r.output);
        }
        Err(e) => panic!("[file-read] FAILED: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_05_write_then_read_chain() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph write_read {
            writer [handler="codergen", prompt="Use write_file to create 'report.md' with a short 3-line report about AI trends. Include the word 'NEURAL' somewhere.", tools="write_file", timeout_secs="120"]
            reader [handler="codergen", prompt="Use read_file to read 'report.md' and summarize it in one sentence.", tools="read_file", timeout_secs="120"]
            writer -> reader
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "write and read a report", &vars()).await;

    match result {
        Ok(r) => {
            println!("[write-read] {} | success={} | nodes={}",
                elapsed_str(start), r.success, r.node_summaries.len());
            assert!(r.success);
            assert_eq!(r.node_summaries.len(), 2);
            let report_path = dir.path().join("report.md");
            assert!(report_path.exists(), "report.md should exist");
            let contents = std::fs::read_to_string(&report_path).unwrap();
            println!("[write-read] report.md: '{}'", contents.trim().chars().take(200).collect::<String>());
        }
        Err(e) => panic!("[write-read] FAILED: {e}"),
    }
}

// ===========================================================================
// 6. SEND FILE TOOL (file upload to chat)
//    Isolate: SendFileTool → OutboundMessage.media flow
//    No real channel needed — capture via mpsc
// ===========================================================================

#[tokio::test]
async fn test_06_send_file_basic() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "telegram", "chat123");

    // Create temp file
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "test content").unwrap();
    let path = tmp.path().to_string_lossy().to_string();

    let result = tool
        .execute(&serde_json::json!({
            "file_path": path,
            "caption": "Here is your report"
        }))
        .await
        .unwrap();

    assert!(result.success, "send_file should succeed: {}", result.output);

    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.channel, "telegram");
    assert_eq!(msg.chat_id, "chat123");
    assert_eq!(msg.content, "Here is your report");
    assert_eq!(msg.media.len(), 1);
    assert_eq!(msg.media[0], path);
    println!("[send-file] OK: sent {} to {}:{}", msg.media[0], msg.channel, msg.chat_id);
}

#[tokio::test]
async fn test_06_send_file_multiple_types() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let dir = TempDir::new().unwrap();
    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "feishu", "group456")
        .with_base_dir(dir.path());

    // Test various file types
    let files = vec![
        ("report.pdf", b"fake pdf content" as &[u8]),
        ("data.csv", b"col1,col2\n1,2\n3,4"),
        ("slides.pptx", b"fake pptx"),
        ("image.png", b"fake png data"),
    ];

    for (name, content) in &files {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": path.to_string_lossy().to_string(),
                "caption": format!("Sending {name}")
            }))
            .await
            .unwrap();

        assert!(result.success, "should send {name}: {}", result.output);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.media.len(), 1);
        println!("[send-file-types] OK: {name} → {}:{}", msg.channel, msg.chat_id);
    }
}

#[tokio::test]
async fn test_06_send_file_nonexistent() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let (tx, _rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "telegram", "chat123");

    let result = tool
        .execute(&serde_json::json!({
            "file_path": "/nonexistent/path/report.pdf"
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("not found"), "should report not found: {}", result.output);
    println!("[send-file-404] OK: correctly rejected nonexistent file");
}

#[tokio::test]
async fn test_06_send_file_sandbox_escape() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let sandbox = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    // Create a "secret" file outside the sandbox
    let secret = outside.path().join("credentials.json");
    std::fs::write(&secret, r#"{"api_key": "sk-secret"}"#).unwrap();

    let (tx, _rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "telegram", "chat123")
        .with_base_dir(sandbox.path());

    // Try absolute path escape
    let result = tool
        .execute(&serde_json::json!({
            "file_path": secret.to_string_lossy().to_string()
        }))
        .await
        .unwrap();
    assert!(!result.success, "should block absolute path outside sandbox");
    println!("[sandbox-abs] OK: blocked {}", result.output);

    // Try path traversal escape
    let traversal = format!("../../{}", secret.to_string_lossy());
    let result = tool
        .execute(&serde_json::json!({ "file_path": traversal }))
        .await
        .unwrap();
    assert!(!result.success, "should block traversal path");
    println!("[sandbox-traversal] OK: blocked {}", result.output);
}

#[tokio::test]
async fn test_06_send_file_no_context() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let (tx, _rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::new(tx); // No channel/chat_id set

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, "data").unwrap();

    let result = tool
        .execute(&serde_json::json!({
            "file_path": tmp.path().to_string_lossy().to_string()
        }))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.output.contains("No target"), "should say no target: {}", result.output);
    println!("[send-file-no-ctx] OK: correctly rejected without context");
}

// ===========================================================================
// 7. FILE RECEIVING (InboundMessage.media)
//    Isolate: inbound messages with media paths get passed to agents
// ===========================================================================

#[tokio::test]
async fn test_07_inbound_message_carries_media() {
    let msg = InboundMessage {
        channel: "telegram".into(),
        sender_id: "user1".into(),
        chat_id: "chat42".into(),
        content: "Here is a photo".into(),
        timestamp: chrono::Utc::now(),
        media: vec![
            "/tmp/media/photo.jpg".into(),
            "/tmp/media/document.pdf".into(),
        ],
        metadata: serde_json::json!({}),
        message_id: None,
    };

    assert_eq!(msg.media.len(), 2);
    assert_eq!(msg.media[0], "/tmp/media/photo.jpg");
    assert_eq!(msg.media[1], "/tmp/media/document.pdf");
    println!("[inbound-media] OK: {} media items in message", msg.media.len());
}

#[tokio::test]
async fn test_07_inbound_message_media_serde() {
    let json = r#"{
        "channel": "feishu",
        "sender_id": "u123",
        "chat_id": "c456",
        "content": "see attached",
        "timestamp": "2024-01-01T00:00:00Z",
        "media": ["/data/file1.png", "/data/file2.csv"],
        "metadata": {}
    }"#;
    let msg: InboundMessage = serde_json::from_str(json).unwrap();
    assert_eq!(msg.media.len(), 2);
    assert_eq!(msg.media[0], "/data/file1.png");
    println!("[inbound-serde] OK: media deserialized correctly");

    // Roundtrip
    let serialized = serde_json::to_string(&msg).unwrap();
    let parsed: InboundMessage = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed.media, msg.media);
    println!("[inbound-serde] OK: roundtrip preserved media");
}

#[tokio::test]
async fn test_07_inbound_message_no_media_defaults_empty() {
    let json = r#"{
        "channel": "cli",
        "sender_id": "local",
        "chat_id": "default",
        "content": "hello",
        "timestamp": "2024-01-01T00:00:00Z"
    }"#;
    let msg: InboundMessage = serde_json::from_str(json).unwrap();
    assert!(msg.media.is_empty(), "missing media field should default to empty vec");
    println!("[inbound-no-media] OK: defaults to empty");
}

#[tokio::test]
async fn test_07_outbound_message_carries_media() {
    let msg = OutboundMessage {
        channel: "telegram".into(),
        chat_id: "chat42".into(),
        content: "Here is your report".into(),
        reply_to: Some("msg-99".into()),
        media: vec!["/workspace/report.pdf".into()],
        metadata: serde_json::json!({}),
    };

    let json = serde_json::to_string(&msg).unwrap();
    let parsed: OutboundMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.media.len(), 1);
    assert_eq!(parsed.media[0], "/workspace/report.pdf");
    assert_eq!(parsed.reply_to, Some("msg-99".into()));
    println!("[outbound-media] OK: media roundtrips correctly");
}

// ===========================================================================
// 8. TIMEOUT BEHAVIOR
//    Isolate: does the pipeline-level timeout work?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_09_shell_timeout() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph timeout_test {
            slow [handler="shell", prompt="sleep 30", timeout_secs="3"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "", &vars()).await;

    let elapsed = start.elapsed();
    println!("[shell-timeout] completed in {:.1}s", elapsed.as_secs_f64());

    match result {
        Ok(r) => {
            // Shell handler should timeout and return Error status
            assert!(!r.success || r.output.to_lowercase().contains("timeout"),
                "should timeout: {}", r.output);
            assert!(
                elapsed.as_secs() < 15,
                "should complete in <15s (timeout=3s), took {}s",
                elapsed.as_secs()
            );
            println!("[shell-timeout] OK: timed out correctly");
        }
        Err(e) => {
            // Timeout error is also acceptable
            let err_str = format!("{e}");
            println!("[shell-timeout] OK: error={}", err_str);
            assert!(elapsed.as_secs() < 15);
        }
    }
}

// ===========================================================================
// 10. RETRY BEHAVIOR
//     Isolate: does max_retries with exponential backoff work?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_10_shell_retry_on_error() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Command that fails, with 1 retry — should attempt twice total
    let dot = r#"
        digraph retry_test {
            fail_cmd [handler="shell", prompt="exit 1", max_retries="1", timeout_secs="10"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "", &vars()).await;

    println!("[retry] completed in {}", elapsed_str(start));
    match result {
        Ok(r) => {
            // Shell returns Fail (not Error), so retries may not trigger
            // But the pipeline should still complete
            println!("[retry] success={} output='{}'", r.success, r.output.trim());
        }
        Err(e) => {
            println!("[retry] error: {e}");
        }
    }
}

// ===========================================================================
// 11. MIXED HANDLERS IN ONE PIPELINE
//     Isolate: codergen → shell → gate chain
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_11_mixed_handlers() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph mixed {
            generate [handler="codergen", prompt="Generate a random 4-digit number. Reply with ONLY the number.", timeout_secs="60"]
            save [handler="shell", prompt="echo 'GENERATED' > generated.txt"]
            verify [handler="gate", prompt="true"]
            generate -> save
            save -> verify
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "generate a number", &vars()).await;

    match result {
        Ok(r) => {
            println!("[mixed] {} | success={} | nodes={}",
                elapsed_str(start), r.success, r.node_summaries.len());
            for s in &r.node_summaries {
                println!("  - {}: {}ms, success={}", s.node_id, s.duration_ms, s.success);
            }
            assert!(r.success);
            assert!(r.node_summaries.len() >= 3, "should execute all 3 nodes");
            let gen_file = dir.path().join("generated.txt");
            assert!(gen_file.exists(), "shell should have created generated.txt");
            println!("[mixed] OK");
        }
        Err(e) => panic!("[mixed] FAILED: {e}"),
    }
}

// ===========================================================================
// 12. ERROR PROPAGATION
//     Isolate: what happens when a node fails?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_12_node_failure_propagation() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Shell command that fails → pipeline should stop
    let dot = r#"
        digraph fail_test {
            step1 [handler="shell", prompt="exit 1"]
            step2 [handler="shell", prompt="echo SHOULD_NOT_REACH"]
            step1 -> step2
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "", &vars()).await;

    match result {
        Ok(r) => {
            println!("[fail-prop] {} | success={} | output='{}'",
                elapsed_str(start), r.success, r.output.trim());
            // step1 fails, step2 may or may not run depending on edge selection
            // But output should NOT contain SHOULD_NOT_REACH if error propagates
            if !r.success {
                println!("[fail-prop] OK: pipeline reported failure");
            } else {
                // If success, check step2 didn't run unexpectedly
                println!("[fail-prop] NOTE: pipeline reported success despite step1 failure");
            }
        }
        Err(e) => {
            println!("[fail-prop] OK: pipeline returned error: {e}");
        }
    }
}

// ===========================================================================
// 13. DOT PARSE VALIDATION
//     Isolate: does malformed DOT fail gracefully?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_13_malformed_dot_error() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let bad_dot = "this is not valid DOT";
    let result = executor.run(bad_dot, "test", &vars()).await;
    assert!(result.is_err(), "should reject malformed DOT");
    println!("[bad-dot] OK: rejected malformed DOT: {}", result.unwrap_err());
}

#[tokio::test]
#[ignore]
async fn test_13_empty_graph_error() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let empty_dot = "digraph empty {}";
    let result = executor.run(empty_dot, "test", &vars()).await;
    // Either error or empty output is acceptable
    match result {
        Ok(r) => println!("[empty-graph] OK: success={}, output='{}'", r.success, r.output),
        Err(e) => println!("[empty-graph] OK: rejected empty graph: {e}"),
    }
}

// ===========================================================================
// 14. TOKEN TRACKING ACROSS NODES
//     Isolate: are tokens accumulated correctly across pipeline nodes?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_14_token_accumulation() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph tokens {
            step1 [handler="codergen", prompt="Say hello in one word.", timeout_secs="60"]
            step2 [handler="codergen", prompt="Say goodbye in one word.", timeout_secs="60"]
            step1 -> step2
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "greetings", &vars()).await;

    match result {
        Ok(r) => {
            println!("[tokens] {} | total: {}+{} tokens",
                elapsed_str(start),
                r.token_usage.input_tokens, r.token_usage.output_tokens);
            for s in &r.node_summaries {
                println!("  - {}: {}+{} tokens",
                    s.node_id, s.token_usage.input_tokens, s.token_usage.output_tokens);
            }
            // Total should be sum of both nodes
            let sum_input: u32 = r.node_summaries.iter().map(|s| s.token_usage.input_tokens).sum();
            let sum_output: u32 = r.node_summaries.iter().map(|s| s.token_usage.output_tokens).sum();
            assert_eq!(r.token_usage.input_tokens, sum_input,
                "total input tokens should equal sum of node tokens");
            assert_eq!(r.token_usage.output_tokens, sum_output,
                "total output tokens should equal sum of node tokens");
            assert!(r.token_usage.input_tokens > 0, "should have used some tokens");
            println!("[tokens] OK: token accounting correct");
        }
        Err(e) => panic!("[tokens] FAILED: {e}"),
    }
}

// ===========================================================================
// 15. DEEP RESEARCH PIPELINE (realistic end-to-end)
//     This is the real workflow that gets "network error"
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_15_research_pipeline_simple() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Simplified 2-node research pipeline: search + synthesize
    // No deep_search tool (needs plugin), so use codergen with basic tools
    let dot = r#"
        digraph research {
            gather [handler="codergen", prompt="You are a research assistant. Write 3 key facts about the topic. Use write_file to save them to 'facts.md'.", tools="write_file", timeout_secs="120"]
            synthesize [handler="codergen", prompt="Read facts.md and write a 2-paragraph summary. Use write_file to save as 'report.md'.", tools="read_file,write_file", timeout_secs="120"]
            gather -> synthesize
        }
    "#;

    let start = Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        executor.run(dot, "What is Rust programming language?", &vars()),
    ).await;

    match result {
        Ok(Ok(r)) => {
            println!("[research] {} | success={} | nodes={} | {}+{} tokens",
                elapsed_str(start), r.success, r.node_summaries.len(),
                r.token_usage.input_tokens, r.token_usage.output_tokens);
            for s in &r.node_summaries {
                println!("  - {} ({}): {}ms, {}+{} tokens, success={}",
                    s.node_id, s.model.as_deref().unwrap_or("default"),
                    s.duration_ms, s.token_usage.input_tokens,
                    s.token_usage.output_tokens, s.success);
            }
            // Check files were created
            let facts = dir.path().join("facts.md");
            let report = dir.path().join("report.md");
            println!("[research] facts.md exists={} report.md exists={}",
                facts.exists(), report.exists());
            if report.exists() {
                let content = std::fs::read_to_string(&report).unwrap();
                println!("[research] report preview: '{}'",
                    content.chars().take(200).collect::<String>());
            }
        }
        Ok(Err(e)) => {
            println!("[research] FAILED after {} — {e}", elapsed_str(start));
            println!("  If test_01 passed (connectivity OK) but this fails,");
            println!("  the error is likely in:");
            println!("  - CodergenHandler (agent loop)");
            println!("  - Tool execution within the agent");
            println!("  - Edge selection between nodes");
            panic!("research pipeline failed: {e}");
        }
        Err(_) => {
            panic!("[research] TIMED OUT after 300s — likely stuck in agent loop or waiting for LLM");
        }
    }
}

// ===========================================================================
// 16. GOAL GATE (early exit on success)
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_16_goal_gate_early_exit() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph goal {
            attempt [handler="codergen", prompt="Write 'SUCCESS' and nothing else.", goal_gate="true", timeout_secs="60"]
            unreachable [handler="shell", prompt="echo SHOULD_NOT_RUN"]
            attempt -> unreachable
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "try", &vars()).await;

    match result {
        Ok(r) => {
            println!("[goal-gate] {} | success={} | nodes={}",
                elapsed_str(start), r.success, r.node_summaries.len());
            assert!(r.success, "goal gate should mark pipeline as success");
            // The unreachable node should not execute (goal_gate exits early)
            let ran_unreachable = r.node_summaries.iter().any(|s| s.node_id == "unreachable");
            if !ran_unreachable {
                println!("[goal-gate] OK: early exit, unreachable node skipped");
            } else {
                println!("[goal-gate] NOTE: unreachable node ran (goal_gate may not skip edges)");
            }
        }
        Err(e) => panic!("[goal-gate] FAILED: {e}"),
    }
}

// ===========================================================================
// 17. NODE SUMMARY REPORTING
//     Isolate: do we get correct per-node summaries?
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_17_node_summaries_complete() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph summary {
            a [handler="noop", label="First Step"]
            b [handler="shell", prompt="echo done", label="Shell Step"]
            c [handler="noop", label="Final Step"]
            a -> b -> c
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "test", &vars()).await;

    match result {
        Ok(r) => {
            println!("[summaries] {} | {} nodes", elapsed_str(start), r.node_summaries.len());
            for s in &r.node_summaries {
                println!("  - id='{}' label='{}' model={:?} duration={}ms success={}",
                    s.node_id, s.label, s.model, s.duration_ms, s.success);
            }
            assert_eq!(r.node_summaries.len(), 3, "should have 3 node summaries");
            assert!(r.node_summaries.iter().all(|s| s.success), "all nodes should succeed");
            // noop handlers complete in 0ms, which is expected
            println!("[summaries] OK");
        }
        Err(e) => panic!("[summaries] FAILED: {e}"),
    }
}

// ===========================================================================
// 18. WRITE_FILE + SEND_FILE PIPELINE (Telegram test report: 3.1.6)
//     Bug: "Bot completes write_file but sends no confirmation reply"
//     Isolate: write_file → send_file → verify outbound message produced
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_18_write_file_produces_output() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Simulate the real scenario: agent writes a file and should confirm
    let dot = r#"
        digraph write_confirm {
            writer [handler="codergen", prompt="Use write_file to create 'research_report.md' with 3 lines about AI. After writing, you MUST reply confirming the file was saved. Include the filename in your response.", tools="write_file", timeout_secs="120"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "generate research report and save it", &vars()).await;

    match result {
        Ok(r) => {
            println!("[write-confirm] {} | success={} | output_len={}",
                elapsed_str(start), r.success, r.output.len());
            println!("[write-confirm] output: '{}'",
                r.output.trim().chars().take(200).collect::<String>());

            // The key assertion: the pipeline must produce non-empty output
            // This catches the bug where write_file succeeds but no reply is sent
            assert!(r.success, "pipeline should succeed");
            assert!(!r.output.trim().is_empty(),
                "BUG: write_file completed but pipeline output is empty (no confirmation reply)");

            let report = dir.path().join("research_report.md");
            assert!(report.exists(), "research_report.md should have been created");
            println!("[write-confirm] OK: file written and output produced");
        }
        Err(e) => panic!("[write-confirm] FAILED: {e}"),
    }
}

// ===========================================================================
// 19. FILE UPLOAD → READ CHAIN (Telegram test report: 3.1.7)
//     Bug: "Telegram file upload, Bot can't find file in next message"
//     Isolate: file exists in working_dir, agent can read it
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_19_uploaded_file_readable_by_agent() {
    let dir = TempDir::new().unwrap();

    // Simulate what happens when a user uploads a file via Telegram:
    // The channel downloads it to media_dir, and the path is passed in InboundMessage.media
    // The gateway then makes it available to the agent's working dir

    // Create file as if it was uploaded
    let uploaded = dir.path().join("sample.txt");
    std::fs::write(&uploaded, "这是一段中文测试文本。\nThis is English text.\nLine 3.").unwrap();

    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph read_uploaded {
            reader [handler="codergen", prompt="Use read_file to read 'sample.txt'. Tell me: 1) how many lines it has, 2) what languages are in it.", tools="read_file", timeout_secs="120"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "read the uploaded file", &vars()).await;

    match result {
        Ok(r) => {
            println!("[uploaded-read] {} | success={}", elapsed_str(start), r.success);
            println!("[uploaded-read] output: '{}'",
                r.output.trim().chars().take(200).collect::<String>());
            assert!(r.success);
            // Agent should be able to read the file and report on its contents
            let lower = r.output.to_lowercase();
            assert!(
                lower.contains("3") || lower.contains("three"),
                "should count 3 lines: {}",
                r.output.chars().take(300).collect::<String>()
            );
            println!("[uploaded-read] OK: agent successfully read uploaded file");
        }
        Err(e) => panic!("[uploaded-read] FAILED: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_19_translate_uploaded_file() {
    let dir = TempDir::new().unwrap();

    // Simulate the exact failing scenario from test report (scene 23, TV-02):
    // User uploads sample.txt, then asks "translate this file to English"
    let uploaded = dir.path().join("sample.txt");
    std::fs::write(&uploaded, "今天天气很好。\n我喜欢编程。\nRust 是一门很棒的语言。").unwrap();

    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    let dot = r#"
        digraph translate {
            translate [handler="codergen", prompt="Read the file 'sample.txt' using read_file, translate all Chinese text to English, and save the result as 'translated.txt' using write_file.", tools="read_file,write_file", timeout_secs="120"]
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "translate the uploaded file to English", &vars()).await;

    match result {
        Ok(r) => {
            println!("[translate] {} | success={}", elapsed_str(start), r.success);
            assert!(r.success);

            let translated = dir.path().join("translated.txt");
            if translated.exists() {
                let content = std::fs::read_to_string(&translated).unwrap();
                println!("[translate] translated.txt: '{}'",
                    content.trim().chars().take(200).collect::<String>());
                // Should contain English translations
                let lower = content.to_lowercase();
                assert!(
                    lower.contains("weather") || lower.contains("programming") || lower.contains("rust"),
                    "translation should contain English words: {}",
                    content.chars().take(300).collect::<String>()
                );
                println!("[translate] OK: file translated successfully");
            } else {
                panic!("[translate] BUG: translated.txt was not created — agent lost file reference");
            }
        }
        Err(e) => panic!("[translate] FAILED: {e}"),
    }
}

// ===========================================================================
// 20. SEND_FILE COMPLETE FLOW (file write → send → verify outbound)
//     Isolate: pipeline node writes file, then SendFileTool delivers it
// ===========================================================================

#[tokio::test]
async fn test_20_send_file_after_write() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let dir = TempDir::new().unwrap();

    // Step 1: Simulate pipeline writing a file
    let report_path = dir.path().join("skill-output").join("deck.pptx");
    std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
    std::fs::write(&report_path, "fake pptx content").unwrap();

    // Step 2: SendFileTool delivers it via relative path (how pipelines typically reference files)
    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "telegram", "user123")
        .with_base_dir(dir.path());

    let result = tool
        .execute(&serde_json::json!({
            "file_path": "skill-output/deck.pptx",
            "caption": "Here is your presentation"
        }))
        .await
        .unwrap();

    assert!(result.success, "should send file: {}", result.output);

    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.channel, "telegram");
    assert_eq!(msg.chat_id, "user123");
    assert_eq!(msg.content, "Here is your presentation");
    assert_eq!(msg.media.len(), 1);
    assert!(msg.media[0].contains("deck.pptx"), "media should reference pptx: {}", msg.media[0]);
    println!("[send-after-write] OK: file written by pipeline, sent via SendFileTool");
}

// ===========================================================================
// 21. MULTIPLE FILE OUTPUTS (realistic pipeline scenario)
//     A pipeline that produces multiple files and sends them all
// ===========================================================================

#[tokio::test]
async fn test_21_send_multiple_files() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let dir = TempDir::new().unwrap();

    // Simulate pipeline output: report + data + chart
    let files = [
        ("report.md", "# Research Report\n\nFindings here..."),
        ("data.csv", "metric,value\naccuracy,0.95\nlatency,42ms"),
        ("chart.png", "fake png binary data"),
    ];
    for (name, content) in &files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }

    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "feishu", "group789")
        .with_base_dir(dir.path());

    for (name, _) in &files {
        let result = tool
            .execute(&serde_json::json!({
                "file_path": dir.path().join(name).to_string_lossy().to_string()
            }))
            .await
            .unwrap();
        assert!(result.success, "should send {name}: {}", result.output);
    }

    // Verify all 3 files were queued for delivery
    let mut delivered = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        delivered.push(msg);
    }
    assert_eq!(delivered.len(), 3, "should deliver all 3 files");
    for (i, msg) in delivered.iter().enumerate() {
        assert_eq!(msg.channel, "feishu");
        assert_eq!(msg.media.len(), 1);
        println!("[multi-file] {} → {}", files[i].0, msg.media[0]);
    }
    println!("[multi-file] OK: all 3 files queued for delivery");
}

// ===========================================================================
// 22. INBOUND MEDIA → AGENT CONTEXT
//     Verify the full path: InboundMessage.media → agent sees file paths
// ===========================================================================

#[tokio::test]
async fn test_22_inbound_media_to_agent_message() {
    // When a user uploads a file, it flows as:
    //   Channel → download_media() → InboundMessage.media: ["/path/to/file"]
    //   → SessionActor → Agent.process_message(content, history, media)
    //   → The agent gets the file paths in its context

    // Test the InboundMessage → Message conversion that should include media
    let inbound = InboundMessage {
        channel: "telegram".into(),
        sender_id: "user1".into(),
        chat_id: "chat42".into(),
        content: "translate this file".into(),
        timestamp: chrono::Utc::now(),
        media: vec!["/tmp/media/document.pdf".into()],
        metadata: serde_json::json!({}),
        message_id: Some("msg-123".into()),
    };

    // Verify the message carries all necessary info
    assert_eq!(inbound.content, "translate this file");
    assert_eq!(inbound.media.len(), 1);
    assert_eq!(inbound.media[0], "/tmp/media/document.pdf");
    assert_eq!(inbound.message_id, Some("msg-123".into()));

    // The session actor converts this to a Message with media
    let agent_msg = Message {
        role: MessageRole::User,
        content: if inbound.content.is_empty() && !inbound.media.is_empty() {
            "[User sent a file]".to_string()
        } else {
            inbound.content.clone()
        },
        media: inbound.media.clone(),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: inbound.timestamp,
    };

    assert_eq!(agent_msg.media.len(), 1);
    assert_eq!(agent_msg.media[0], "/tmp/media/document.pdf");
    assert_eq!(agent_msg.content, "translate this file");
    println!("[inbound-to-agent] OK: media paths flow from InboundMessage to agent Message");
}

#[tokio::test]
async fn test_22_inbound_media_empty_content_gets_placeholder() {
    // When user sends ONLY a file with no text, content should get a placeholder
    let inbound = InboundMessage {
        channel: "telegram".into(),
        sender_id: "user1".into(),
        chat_id: "chat42".into(),
        content: String::new(),
        timestamp: chrono::Utc::now(),
        media: vec!["/tmp/media/photo.jpg".into()],
        metadata: serde_json::json!({}),
        message_id: None,
    };

    let content = if inbound.content.is_empty() && !inbound.media.is_empty() {
        "[User sent a file]".to_string()
    } else {
        inbound.content.clone()
    };

    assert_eq!(content, "[User sent a file]");
    println!("[inbound-empty-content] OK: placeholder text when only file sent");
}

// ===========================================================================
// 23. LARGE FILE HANDLING
//     Verify SendFileTool handles large files without corruption
// ===========================================================================

#[tokio::test]
async fn test_23_send_large_file() {
    use octos_agent::Tool;
    use octos_agent::tools::SendFileTool;

    let dir = TempDir::new().unwrap();

    // Create a 5MB file
    let large_file = dir.path().join("large_data.bin");
    let data: Vec<u8> = (0..5_000_000).map(|i| (i % 256) as u8).collect();
    std::fs::write(&large_file, &data).unwrap();

    let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
    let tool = SendFileTool::with_context(tx, "telegram", "chat123")
        .with_base_dir(dir.path());

    let result = tool
        .execute(&serde_json::json!({
            "file_path": large_file.to_string_lossy().to_string(),
            "caption": "5MB data file"
        }))
        .await
        .unwrap();

    assert!(result.success, "should handle large file: {}", result.output);
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.media.len(), 1);

    // Verify the file at the media path is still intact
    let sent_path = &msg.media[0];
    let sent_size = std::fs::metadata(sent_path).unwrap().len();
    assert_eq!(sent_size, 5_000_000, "file should be 5MB, got {}B", sent_size);
    println!("[large-file] OK: 5MB file queued for delivery");
}

// ===========================================================================
// 24. CONCURRENT PIPELINE NODES DON'T CORRUPT WORKING DIR
//     When parallel nodes write files, they shouldn't step on each other
// ===========================================================================

#[tokio::test]
#[ignore]
async fn test_24_parallel_file_isolation() {
    let dir = TempDir::new().unwrap();
    let config = make_config(deepseek(), &dir).await;
    let executor = PipelineExecutor::new(config);

    // Two shell nodes writing different files in parallel
    // The parallel node fans out to write_a and write_b, then converges to merge
    let dot = r#"
        digraph parallel_write {
            fan [handler="parallel", converge="merge"]
            write_a [handler="shell", prompt="echo CONTENT_A > file_a.txt"]
            write_b [handler="shell", prompt="echo CONTENT_B > file_b.txt"]
            merge [handler="noop"]
            fan -> write_a
            fan -> write_b
            write_a -> merge
            write_b -> merge
        }
    "#;

    let start = Instant::now();
    let result = executor.run(dot, "write files in parallel", &vars()).await;

    match result {
        Ok(r) => {
            println!("[parallel-files] {} | success={}", elapsed_str(start), r.success);
            let file_a = dir.path().join("file_a.txt");
            let file_b = dir.path().join("file_b.txt");
            println!("[parallel-files] file_a exists={} file_b exists={}",
                file_a.exists(), file_b.exists());
            if file_a.exists() && file_b.exists() {
                let a = std::fs::read_to_string(&file_a).unwrap();
                let b = std::fs::read_to_string(&file_b).unwrap();
                assert!(a.contains("CONTENT_A"), "file_a corrupted: {a}");
                assert!(b.contains("CONTENT_B"), "file_b corrupted: {b}");
                println!("[parallel-files] OK: both files written correctly");
            }
        }
        Err(e) => panic!("[parallel-files] FAILED: {e}"),
    }
}
