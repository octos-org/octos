use base64::Engine;
use octos_core::Message;
use octos_llm::{
    ChatConfig, LlmProvider, media_transport::MediaTransportMode, openai::OpenAIProvider,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn reply() -> Value {
    json!({"id":"test", "model":"future-vision", "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}, "choices":[{"index":0,
        "message":{"role":"assistant","content":"seen"},"finish_reason":"stop"}]})
}

fn provider(server: &MockServer, key: &str, model: &str) -> OpenAIProvider {
    OpenAIProvider::new(key, model)
        .with_base_url(server.uri())
        .with_media_transport(MediaTransportMode::MoonshotFiles)
}

fn message(file: &std::path::Path) -> Message {
    let mut m = Message::user("Read this image");
    m.media = vec![file.display().to_string()];
    m
}

async fn mocks(server: &MockServer, uploads: u64, chats: u64) {
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"image-1"})))
        .expect(uploads)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(chats)
        .mount(server)
        .await;
}

#[tokio::test]
async fn should_reuse_one_upload_across_turns_and_model_names() {
    let server = MockServer::start().await;
    mocks(&server, 1, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("test.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nimmutable image").unwrap();
    for model in ["future-vision-a", "future-vision-b"] {
        provider(&server, "same-key", model)
            .chat(&[message(&image)], &[], &ChatConfig::default())
            .await
            .unwrap();
    }
    let requests = server.received_requests().await.unwrap();
    let chats: Vec<Value> = requests
        .iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .map(|r| r.body_json().unwrap())
        .collect();
    for body in chats {
        assert!(
            body["messages"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["image_url"]["url"] == "ms://image-1")
        );
        assert!(!body.to_string().contains("base64"));
    }
}

#[tokio::test]
async fn should_isolate_credentials_and_reupload_changed_bytes() {
    let server = MockServer::start().await;
    mocks(&server, 3, 3).await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("test.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nfirst image").unwrap();
    for key in ["tenant-a", "tenant-b"] {
        provider(&server, key, "future-vision")
            .chat(&[message(&image)], &[], &ChatConfig::default())
            .await
            .unwrap();
    }
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nsecond image").unwrap();
    provider(&server, "tenant-a", "future-vision")
        .chat(&[message(&image)], &[], &ChatConfig::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn should_coordinate_concurrent_workers_with_one_upload() {
    let server = MockServer::start().await;
    mocks(&server, 1, 18).await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("test.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nworker shared image").unwrap();
    futures::future::join_all((0..18).map(|_| async {
        provider(&server, "key", "future-vision")
            .chat(&[message(&image)], &[], &ChatConfig::default())
            .await
            .unwrap();
    }))
    .await;
}

#[tokio::test]
async fn should_keep_inline_images_for_unknown_endpoints_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("test.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nimage").unwrap();
    OpenAIProvider::new("key", "k3.1-pro")
        .with_base_url(server.uri())
        .chat(&[message(&image)], &[], &ChatConfig::default())
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().unwrap();
    assert!(
        body.to_string()
            .contains(&base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nimage"))
    );
}

#[tokio::test]
async fn streaming_and_chat_should_share_the_same_transport_cache() {
    use futures::StreamExt;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"stream-image"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(|r:&wiremock::Request| {
            let body:Value=r.body_json().unwrap();
            assert!(body.to_string().contains("ms://stream-image"));
            if body["stream"]==true {
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream")
                    .set_body_string("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"seen\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n")
            } else { ResponseTemplate::new(200).set_body_json(reply()) }
        }).expect(2).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("stream.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nstream").unwrap();
    provider(&server, "stream-key", "future-model")
        .chat(&[message(&image)], &[], &ChatConfig::default())
        .await
        .unwrap();
    let mut stream = provider(&server, "stream-key", "future-model")
        .chat_stream(&[message(&image)], &[], &ChatConfig::default())
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            octos_llm::StreamEvent::TextDelta(delta) => text.push_str(&delta),
            octos_llm::StreamEvent::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(text, "seen");
}

#[tokio::test]
async fn model_capability_can_disable_references_without_disabling_vision() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("inline.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\ninline").unwrap();
    let hints = octos_llm::openai::ModelHints {
        image_file_references: Some(false),
        ..Default::default()
    };
    provider(&server, "opt-out-key", "future-model")
        .with_hints(hints)
        .chat(&[message(&image)], &[], &ChatConfig::default())
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .body_json::<Value>()
            .unwrap()
            .to_string()
            .contains("data:image/png;base64,")
    );
}

#[tokio::test]
async fn same_account_on_different_endpoints_must_upload_separately() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    mocks(&a, 1, 1).await;
    mocks(&b, 1, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shared.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nshared").unwrap();
    for server in [&a, &b] {
        provider(server, "endpoint-isolation-key", "future-model")
            .chat(&[message(&image)], &[], &ChatConfig::default())
            .await
            .unwrap();
    }
}
