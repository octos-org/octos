//! Exercise the public registry and the actual HTTP payload, not just model detection.
use futures::StreamExt;
use octos_core::Message;
use octos_llm::openai::ModelHints;
use octos_llm::registry::{CreateParams, lookup};
use octos_llm::{ChatConfig, ReasoningEffort, ToolSpec};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool() -> ToolSpec {
    ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{}}),
    }
}

#[tokio::test]
async fn should_send_complete_kimi_contract_through_registry_after_resume() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":1}
        })))
        .mount(&server)
        .await;

    for model in ["k3.1-pro", "k3.1-pro-highspeed", "k3", "k3-256k"] {
        // This is the same partial hint override used by the PPTX trial.
        let partial: ModelHints = serde_json::from_value(json!({
            "retain_tool_images":true, "fixed_temperature":true,
            "reasoning_style":"effort_low_high_max"
        }))
        .unwrap();
        for hints in [None, Some(partial)] {
            let provider = (lookup("moonshot-coding").unwrap().create)(CreateParams {
                api_key: Some("test-only-key".into()),
                model: Some(model.into()),
                base_url: Some(server.uri()),
                model_hints: hints,
                llm_timeout_secs: None,
                llm_connect_timeout_secs: None,
            })
            .unwrap();
            let mut previous = Message::assistant("Previous answer without a tool call");
            previous.reasoning_content = Some("fixture continuation state\n原样保留".into());
            let history = vec![previous, Message::user("Continue")];
            // History goes through the same Message serialization used on disk.
            let restored: Vec<Message> =
                serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
            provider
                .chat(
                    &restored,
                    &[tool()],
                    &ChatConfig {
                        temperature: Some(0.2),
                        reasoning_effort: Some(ReasoningEffort::High),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let body: Value = requests.last().unwrap().body_json().unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(
                body["messages"][0]["reasoning_content"], "fixture continuation state\n原样保留",
                "{model}: correct destination must not lose continuation state"
            );
            assert_eq!(body["reasoning_effort"], "high", "{model}");
            assert!(body.get("temperature").is_none(), "{model}");
            assert!(body.get("thinking").is_none(), "{model}");
        }
    }
}

#[test]
fn should_resolve_kimi_limits_for_coding_ids_and_qualified_ids() {
    for (model, expected) in [
        ("k3.1-pro", 1_048_576),
        ("k3.1-pro-highspeed", 1_048_576),
        ("moonshot-coding/k3.1-pro", 1_048_576),
        ("moonshot-coding@proxy/k3.1-pro-highspeed", 1_048_576),
        ("k3-256k", 262_144),
        ("kimi-for-coding-highspeed", 262_144),
    ] {
        assert_eq!(
            octos_llm::context::context_window_tokens(model),
            expected,
            "{model}"
        );
    }
}

#[tokio::test]
async fn should_preserve_streamed_kimi_state_through_tool_result_and_resume() {
    use wiremock::matchers::body_partial_json;
    for model in ["k3.1-pro", "k3.1-pro-highspeed"] {
        let server = MockServer::start().await;
        let chunks = [
            json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"fixture "},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"reasoning_content":"continuation 原样保留","tool_calls":[{"index":0,"id":"call-fixture","type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
        ];
        let mut sse = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>();
        sse.push_str("data: [DONE]\n\n");
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"stream":true})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"model":model})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":1}
            })))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;
        let provider = (lookup("moonshot-coding").unwrap().create)(CreateParams {
            api_key: Some("test-only-key".into()),
            model: Some(model.into()),
            base_url: Some(server.uri()),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .unwrap();
        let tools = [tool()];
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let mut history = vec![Message::user("Inspect the file")];
        let mut stream = provider
            .chat_stream(&history, &tools, &config)
            .await
            .unwrap();
        let mut accumulator = octos_llm::StreamAccumulator::new();
        while let Some(event) = stream.next().await {
            accumulator.process(&event);
        }
        let response = accumulator.finish();
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("fixture continuation 原样保留")
        );
        let mut assistant = Message::assistant(response.content.unwrap_or_default());
        assistant.reasoning_content = response.reasoning_content;
        assistant.tool_calls = Some(response.tool_calls);
        history.push(assistant);
        let mut result = Message::user("file contents");
        result.role = octos_core::MessageRole::Tool;
        result.tool_call_id = Some("call-fixture".into());
        history.push(result);
        let restored: Vec<Message> =
            serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
        provider.chat(&restored, &tools, &config).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests.last().unwrap().body_json().unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(
            body["messages"][1]["reasoning_content"],
            "fixture continuation 原样保留"
        );
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call-fixture");
        assert_eq!(body["messages"][2]["tool_call_id"], "call-fixture");
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }
}

#[test]
fn should_declare_a_complete_wire_contract_for_every_coding_catalog_model() {
    let catalog: Value = serde_json::from_str(include_str!("../../../model_catalog.json")).unwrap();
    for row in catalog["models"].as_array().unwrap().iter().filter(|r| {
        r["provider"]
            .as_str()
            .unwrap()
            .starts_with("moonshot-coding/")
    }) {
        let key = row["provider"].as_str().unwrap();
        let hints: ModelHints = serde_json::from_value(row["model_hints"].clone())
            .expect("new models require a wire contract beside their id");
        assert!(
            hints.reasoning_history.is_some(),
            "{key}: declare continuation policy"
        );
        assert!(
            row["context_window"].as_u64().unwrap() > 0,
            "{key}: declare context limit"
        );
        assert_eq!(ModelHints::detect(key), hints);
        assert_eq!(ModelHints::detect(key.split_once('/').unwrap().1), hints);
    }
}
