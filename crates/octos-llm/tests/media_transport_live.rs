//! Opt-in smoke test for an operator-supplied Files-capable chat endpoint.
//! Credentials are read from the environment and never printed. Use a small,
//! non-sensitive synthetic PNG; this test uploads it to the selected provider.

use futures::StreamExt;
use octos_core::Message;
use octos_llm::{ChatConfig, LlmProvider, ReasoningEffort, StreamEvent, openai::OpenAIProvider};

#[tokio::test]
#[ignore = "requires an explicitly configured live provider and synthetic image"]
async fn live_chat_and_stream_share_an_uploaded_image() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("octos::media_transport=debug")
        .with_ansi(false)
        .try_init();
    let key = std::env::var("OCTOS_MEDIA_PROBE_API_KEY").expect("probe API key");
    let endpoint = std::env::var("OCTOS_MEDIA_PROBE_BASE_URL").expect("probe endpoint");
    let model = std::env::var("OCTOS_MEDIA_PROBE_MODEL").expect("probe model");
    let path = std::env::var("OCTOS_MEDIA_PROBE_IMAGE").expect("synthetic image path");
    let expected = std::env::var("OCTOS_MEDIA_PROBE_EXPECTED_TEXT").expect("image challenge text");
    let mut message = Message::user(
        "Read the large code and name the color of the filled circle. Answer in one short line in English.",
    );
    message.media = vec![path];
    let messages = vec![message];
    let config = ChatConfig {
        max_tokens: Some(1024),
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    };
    // Separate provider instances model independent workers. Auto selects the
    // endpoint adapter, not a branch on the model string.
    let first = OpenAIProvider::new(&key, &model).with_base_url(&endpoint);
    let response = first
        .chat(&messages, &[], &config)
        .await
        .expect("live chat");
    let text = response.content.unwrap_or_default();
    assert!(text.contains(&expected), "synthetic challenge was not read");
    println!(
        "chat: {}",
        serde_json::json!({"text":text,"usage":response.usage})
    );

    let second = OpenAIProvider::new(&key, &model).with_base_url(&endpoint);
    let mut stream = second
        .chat_stream(&messages, &[], &config)
        .await
        .expect("live stream");
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::Usage(usage) => println!("stream usage: {}", serde_json::json!(usage)),
            StreamEvent::Error(error) => panic!("stream failed: {error}"),
            _ => {}
        }
    }
    assert!(
        text.contains(&expected),
        "reused reference did not deliver the image"
    );
    println!("stream: {}", serde_json::json!({"text":text}));
}
