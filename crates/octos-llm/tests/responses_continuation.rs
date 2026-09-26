use futures::StreamExt;
use octos_core::Message;
use octos_llm::openai_responses::OpenAIResponsesProvider;
use octos_llm::{ChatConfig, LlmProvider, RouterContext, StreamEvent, with_router_context};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn provider(server: &MockServer) -> OpenAIResponsesProvider {
    OpenAIResponsesProvider::new("test", "qwen3.8-27b")
        .with_base_url(server.uri())
        .with_response_continuation(true)
}

fn scope(session: &str) -> RouterContext {
    RouterContext {
        session_id: Some(session.into()),
        turn_id: None,
    }
}

fn response(id: &str) -> Value {
    json!({"id": id, "status": "completed", "output": [{"type": "message",
        "content": [{"type": "output_text", "text": "OK"}]}],
        "usage": {"input_tokens":100, "output_tokens":2,
            "input_tokens_details":{"cached_tokens":75,"cache_write_tokens":5}}})
}

fn initial() -> Vec<Message> {
    vec![Message::system("Stable system"), Message::user("Reference")]
}

fn followup() -> Vec<Message> {
    let mut messages = initial();
    messages.extend([Message::assistant("OK"), Message::user("Next app")]);
    messages
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn continuation_sends_only_delta_and_accounts_for_cached_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response("resp_first")))
        .mount(&server)
        .await;
    let provider = provider(&server);
    let config = ChatConfig::default();
    let first = with_router_context(scope("one"), provider.chat(&initial(), &[], &config))
        .await
        .unwrap();
    assert_eq!(first.usage.input_tokens, 20);
    assert_eq!(first.usage.cache_read_tokens, 75);
    assert_eq!(first.usage.cache_write_tokens, 5);
    with_router_context(scope("one"), provider.chat(&followup(), &[], &config))
        .await
        .unwrap();
    let requests = bodies(&server).await;
    assert_eq!(requests[0]["store"], true);
    assert!(requests[0].get("previous_response_id").is_none());
    assert_eq!(requests[1]["previous_response_id"], "resp_first");
    assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(requests[1]["input"][0]["content"][0]["text"], "Next app");
}

#[tokio::test]
async fn continuation_never_crosses_sessions_edits_compaction_or_settings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response("resp_first")))
        .mount(&server)
        .await;
    for case in [
        "session",
        "system",
        "history",
        "compaction",
        "settings",
        "anonymous",
        "one_shot",
    ] {
        let provider = provider(&server);
        let mut config = ChatConfig::default();
        with_router_context(scope("one"), provider.chat(&initial(), &[], &config))
            .await
            .unwrap();
        let mut messages = followup();
        let mut context = scope("one");
        match case {
            "session" => context = scope("two"),
            "system" => messages[0] = Message::system("Changed system"),
            "history" => messages[2] = Message::assistant("Edited output"),
            "compaction" => messages = vec![Message::user("Compacted summary")],
            "settings" => config.max_tokens = Some(123),
            "anonymous" => context = RouterContext::default(),
            "one_shot" => config.cache_retention = octos_llm::CacheRetention::None,
            _ => unreachable!(),
        }
        with_router_context(context, provider.chat(&messages, &[], &config))
            .await
            .unwrap();
        let requests = bodies(&server).await;
        assert!(
            requests
                .last()
                .unwrap()
                .get("previous_response_id")
                .is_none(),
            "{case}"
        );
        if matches!(case, "anonymous" | "one_shot") {
            assert!(requests.last().unwrap().get("store").is_none(), "{case}");
        }
    }
}

#[tokio::test]
async fn missing_response_retries_full_history_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("previous_response_id").is_some() {
                ResponseTemplate::new(404).set_body_json(
                    json!({"error": {"message": "Previous response resp_first not found"}}),
                )
            } else {
                ResponseTemplate::new(200).set_body_json(response("resp_first"))
            }
        })
        .mount(&server)
        .await;
    let provider = provider(&server);
    let config = ChatConfig::default();
    with_router_context(scope("one"), provider.chat(&initial(), &[], &config))
        .await
        .unwrap();
    with_router_context(scope("one"), provider.chat(&followup(), &[], &config))
        .await
        .unwrap();
    let requests = bodies(&server).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["previous_response_id"], "resp_first");
    assert!(requests[2].get("previous_response_id").is_none());
    assert_eq!(requests[2]["input"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn unrelated_errors_are_not_replayed() {
    for status in [400, 404, 500] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                if body.get("previous_response_id").is_some() {
                    ResponseTemplate::new(status)
                        .set_body_json(json!({"error": {"message": "Model unavailable"}}))
                } else {
                    ResponseTemplate::new(200).set_body_json(response("resp_first"))
                }
            })
            .mount(&server)
            .await;
        let provider = provider(&server);
        let config = ChatConfig::default();
        with_router_context(scope("one"), provider.chat(&initial(), &[], &config))
            .await
            .unwrap();
        assert!(
            with_router_context(scope("one"), provider.chat(&followup(), &[], &config))
                .await
                .is_err()
        );
        assert_eq!(bodies(&server).await.len(), 2);
    }
}

fn sse(complete: bool) -> String {
    let mut text = format!(
        "data: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"OK"})
    );
    if complete {
        text.push_str(&format!(
            "data: {}\n\n",
            json!({"type":"response.completed", "response":response("resp_stream")})
        ));
    }
    text
}

#[tokio::test]
async fn only_completed_streams_create_a_continuation() {
    for complete in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(complete)),
            )
            .mount(&server)
            .await;
        let provider = provider(&server);
        let config = ChatConfig::default();
        let events: Vec<_> =
            with_router_context(scope("one"), provider.chat_stream(&initial(), &[], &config))
                .await
                .unwrap()
                .collect()
                .await;
        assert_eq!(
            events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            !complete
        );
        if complete {
            assert!(events.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 20 && u.cache_read_tokens == 75 && u.cache_write_tokens == 5)));
        }
        let _: Vec<_> = with_router_context(
            scope("one"),
            provider.chat_stream(&followup(), &[], &config),
        )
        .await
        .unwrap()
        .collect()
        .await;
        let requests = bodies(&server).await;
        assert_eq!(requests[1].get("previous_response_id").is_some(), complete);
    }
}

#[tokio::test]
async fn dropped_stream_does_not_publish_response_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(true)),
        )
        .mount(&server)
        .await;
    let provider = provider(&server);
    let config = ChatConfig::default();
    let mut stream =
        with_router_context(scope("one"), provider.chat_stream(&initial(), &[], &config))
            .await
            .unwrap();
    assert!(matches!(
        stream.next().await,
        Some(StreamEvent::TextDelta(_))
    ));
    drop(stream);
    let _: Vec<_> = with_router_context(
        scope("one"),
        provider.chat_stream(&followup(), &[], &config),
    )
    .await
    .unwrap()
    .collect()
    .await;
    assert!(
        bodies(&server).await[1]
            .get("previous_response_id")
            .is_none()
    );
}

#[tokio::test]
async fn standard_tool_events_keep_parallel_calls_separate_and_replay_explicitly() {
    let server = MockServer::start().await;
    let events = [
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"weather"}}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"stock"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"symbol\":\"AAPL\"}"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"city\":\"Tokyo\"}"}),
        json!({"type":"response.completed","response":{"id":"resp_tools","status":"completed","output":[
            {"type":"function_call","id":"fc_a","call_id":"call_a","name":"weather","arguments":"{\"city\":\"Tokyo\"}"},
            {"type":"function_call","id":"fc_b","call_id":"call_b","name":"stock","arguments":"{\"symbol\":\"AAPL\"}"}
        ]}}),
    ];
    let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let provider = provider(&server);
    let config = ChatConfig::default();
    let mut stream =
        with_router_context(scope("one"), provider.chat_stream(&initial(), &[], &config))
            .await
            .unwrap();
    let mut accumulator = octos_llm::StreamAccumulator::new();
    while let Some(event) = stream.next().await {
        accumulator.process(&event);
    }
    let result = accumulator.finish();
    assert_eq!(result.tool_calls[0].id, "call_a");
    assert_eq!(result.tool_calls[0].arguments["city"], "Tokyo");
    assert_eq!(result.tool_calls[1].arguments["symbol"], "AAPL");
    let mut messages = initial();
    let mut assistant = Message::assistant("");
    assistant.tool_calls = Some(result.tool_calls);
    messages.push(assistant);
    let _: Vec<_> =
        with_router_context(scope("one"), provider.chat_stream(&messages, &[], &config))
            .await
            .unwrap()
            .collect()
            .await;
    assert!(
        bodies(&server).await[1]
            .get("previous_response_id")
            .is_none()
    );
}

#[tokio::test]
async fn failed_json_response_is_an_error_and_cannot_seed_continuation() {
    let server = MockServer::start().await;
    let mut failed = response("resp_failed");
    failed["status"] = "failed".into();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(failed))
        .mount(&server)
        .await;
    let provider = provider(&server);
    let config = ChatConfig::default();
    assert!(
        with_router_context(scope("one"), provider.chat(&initial(), &[], &config))
            .await
            .is_err()
    );
    assert!(
        with_router_context(scope("one"), provider.chat(&followup(), &[], &config))
            .await
            .is_err()
    );
    assert!(
        bodies(&server).await[1]
            .get("previous_response_id")
            .is_none()
    );
}
