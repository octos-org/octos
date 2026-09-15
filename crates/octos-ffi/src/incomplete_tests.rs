//! Offline tests drive the actual Agent loop through the native and C facades.
use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

struct FixtureProvider(Mutex<VecDeque<octos_llm::ChatResponse>>);

#[async_trait::async_trait]
impl LlmProvider for FixtureProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| eyre::eyre!("fixture exhausted"))
    }

    fn model_id(&self) -> &str {
        "ffi-offline-fixture"
    }
    fn provider_name(&self) -> &str {
        "fixture"
    }
}

fn fixture_runtime(final_text: &str, stop_reason: octos_llm::StopReason) -> OctosRuntime {
    // Explicit fake key bypasses the host's auth store; the real adapter is
    // replaced before any request. Tool access is only to this runtime's scratch.
    let mut runtime = OctosRuntime::from_config(RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-fixture-not-a-real-key".into()),
        max_iterations: Some(4),
        ..RuntimeConfig::default()
    })
    .unwrap();
    runtime.cwd = runtime.scratch_dir_for_test();
    runtime.llm = Arc::new(FixtureProvider(Mutex::new(VecDeque::from([
        octos_llm::ChatResponse {
            content: Some("Inspecting scratch directory.".into()),
            reasoning_content: None,
            tool_calls: vec![octos_core::ToolCall {
                id: "fixture-list".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "."}),
                metadata: None,
            }],
            stop_reason: octos_llm::StopReason::ToolUse,
            usage: octos_llm::TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
                reasoning_tokens: 2,
                cache_read_tokens: 1,
                cache_write_tokens: 2,
                ..Default::default()
            },
            provider_index: None,
        },
        octos_llm::ChatResponse {
            content: Some(final_text.into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason,
            usage: octos_llm::TokenUsage {
                input_tokens: 11,
                output_tokens: 5,
                reasoning_tokens: 4,
                cache_read_tokens: 6,
                cache_write_tokens: 3,
                ..Default::default()
            },
            provider_index: None,
        },
    ]))));
    runtime
}

#[test]
fn should_preserve_partial_when_native_conversation_is_truncated() {
    let text = partial_text();
    let runtime = fixture_runtime(&text, octos_llm::StopReason::MaxTokens);
    let error = runtime
        .run_task(&TaskBrief {
            prompt: "Inspect scratch then answer".into(),
            max_iterations: None,
        })
        .expect_err("truncation must remain a failure");
    assert!(!error.to_string().contains("MODEL_PAYLOAD"));
    let CoreError::Incomplete { partial } = error else {
        panic!("the failed run discarded its consumed partial response: {error:?}");
    };
    assert_eq!(partial.output, text);
    assert_eq!(partial.iterations, 2);
    assert_eq!(partial.tokens.input, 18);
    assert_eq!(partial.tokens.output, 8);
    assert_eq!(partial.tokens.reasoning, 6);
    assert_eq!(partial.tokens.cache_read, 7);
    assert_eq!(partial.tokens.cache_write, 5);
}

fn partial_text() -> String {
    // Long but nonrepetitive: this tests lossless FFI transfer, not the Agent's
    // separate repetition-recovery policy (which intentionally retries loops).
    let rows = (0..100)
        .map(|index| format!("数据 {index:04}: {:08x};\n", index * 7919))
        .collect::<String>();
    // Agent inline-invoke normalization already trims outer whitespace;
    // interior whitespace/NUL and Unicode must survive the FFI unchanged.
    format!("MODEL_PAYLOAD 中文\n  \0{rows}END")
}

fn fail_c_task(text: &str) {
    let mut runtime = fixture_runtime(text, octos_llm::StopReason::MaxTokens);
    let brief = CString::new(r#"{"prompt":"Inspect scratch then answer"}"#).unwrap();
    // This internal fixture owns the live runtime for the whole call.
    assert!(octos_run_task(&mut runtime, brief.as_ptr()).is_null());
}

fn take_partial_json() -> Option<serde_json::Value> {
    let pointer = octos_take_last_partial_result();
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the accessor transfers this allocation; read then free once.
    let json = unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .unwrap()
        .to_owned();
    octos_string_free(pointer);
    Some(serde_json::from_str(&json).unwrap())
}

#[test]
fn should_transfer_c_partial_once_without_putting_body_in_last_error() {
    let text = partial_text();
    fail_c_task(&text);
    // SAFETY: read-only thread-local pointer copied before a fallible call.
    let diagnostic = unsafe { CStr::from_ptr(octos_last_error()) }
        .to_str()
        .unwrap()
        .to_owned();
    assert!(diagnostic.contains("incomplete"));
    assert!(diagnostic.len() <= MAX_ERROR_LEN + 16);
    assert!(!diagnostic.contains("MODEL_PAYLOAD"));
    assert!(!octos_version().is_null());
    octos_string_free(ptr::null_mut());
    octos_runtime_free(ptr::null_mut());
    let partial = take_partial_json().expect("explicit partial result, despite NULL task return");
    assert_eq!(partial["output"], text);
    assert_eq!(partial["iterations"], 2);
    assert_eq!(
        partial["tokens"],
        json!({
            "input":18,"output":8,"reasoning":6,"cache_read":7,"cache_write":5,
        })
    );
    assert!(take_partial_json().is_none());
    // SAFETY: taking/freeing output preserves the thread-local diagnostic.
    assert_eq!(
        unsafe { CStr::from_ptr(octos_last_error()) }
            .to_str()
            .unwrap(),
        diagnostic
    );
}

#[test]
fn should_clear_c_partial_on_new_failure_or_success() {
    for operation in ["run", "new", "embed", "success"] {
        fail_c_task("old partial");
        match operation {
            "run" => {
                assert!(octos_run_task(ptr::null_mut(), ptr::null()).is_null());
            }
            "new" => {
                assert!(octos_runtime_new(ptr::null()).is_null());
            }
            "embed" => {
                assert!(octos_embed(ptr::null_mut(), ptr::null()).is_null());
            }
            "success" => {
                let mut runtime = fixture_runtime("genuine final", octos_llm::StopReason::EndTurn);
                let brief = CString::new(r#"{"prompt":"Inspect then answer"}"#).unwrap();
                let result = octos_run_task(&mut runtime, brief.as_ptr());
                assert!(!result.is_null());
                // SAFETY: result is a fresh caller-owned allocation.
                let result_json: serde_json::Value =
                    serde_json::from_str(unsafe { CStr::from_ptr(result) }.to_str().unwrap())
                        .unwrap();
                assert_eq!(result_json["output"], "genuine final");
                assert_eq!(result_json["tokens"]["reasoning"], 6);
                octos_string_free(result);
                assert!(octos_last_error().is_null());
            }
            _ => unreachable!(),
        }
        assert!(
            take_partial_json().is_none(),
            "stale partial after {operation}"
        );
    }
}

#[test]
fn should_keep_c_partial_thread_local_and_clear_it_on_panic() {
    fail_c_task("parent partial");
    std::thread::spawn(|| {
        assert!(take_partial_json().is_none());
        fail_c_task("child partial");
        assert_eq!(take_partial_json().unwrap()["output"], "child partial");
    })
    .join()
    .unwrap();
    assert_eq!(take_partial_json().unwrap()["output"], "parent partial");
    fail_c_task("must not survive a new error");
    guard((), "fixture_panic", || panic!("bounded fixture panic"));
    assert!(take_partial_json().is_none());
}
