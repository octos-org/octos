//! Live Vertex AI integration test.
//!
//! `#[ignore]` — needs real Google credentials, a real image, and network.
//! Run with:
//!
//! ```bash
//! VERTEX_SA_JSON=/path/to/service_account.json \
//! VERTEX_TEST_IMAGE=/path/to/photo.jpg \
//!   cargo test -p octos-llm --test vertex_integration -- --ignored --nocapture
//! ```
//!
//! Use a normal photo (jpg/png/webp). Vertex rejects degenerate images
//! ("Provided image is not valid"), so a 1x1 placeholder won't do.

use octos_core::{Message, MessageRole};
use octos_llm::gemini::GeminiProvider;
use octos_llm::vertex_auth::ServiceAccount;
use octos_llm::{ChatConfig, LlmProvider, ToolSpec};

fn user_with_image(text: &str, image_path: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: text.to_string(),
        media: vec![image_path.to_string()],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }
}

#[tokio::test]
#[ignore = "needs VERTEX_SA_JSON + VERTEX_TEST_IMAGE + network"]
async fn vertex_gemini_answers_a_vision_request() {
    let path = std::env::var("VERTEX_SA_JSON")
        .expect("set VERTEX_SA_JSON to the service-account JSON path");
    let image_path = std::env::var("VERTEX_TEST_IMAGE")
        .expect("set VERTEX_TEST_IMAGE to a real image file path");
    assert!(
        std::path::Path::new(&image_path).is_file(),
        "VERTEX_TEST_IMAGE does not point to a file: {image_path}"
    );

    let sa = ServiceAccount::from_path(std::path::Path::new(&path)).unwrap();
    let provider = GeminiProvider::vertex_from_service_account(sa, "gemini-2.5-flash");

    let messages = vec![user_with_image(
        "Describe this image in one short sentence.",
        &image_path,
    )];

    let no_tools: &[ToolSpec] = &[];
    let resp = provider
        .chat(&messages, no_tools, &ChatConfig::default())
        .await
        .expect("vertex chat should succeed");

    let text = resp.content.unwrap_or_default();
    assert!(!text.trim().is_empty(), "expected a non-empty reply");
    eprintln!("vertex reply: {text}");
}
