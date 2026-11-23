//! A fleet task's declared inputs, seen where the worker's model sees them:
//! in the HTTP request the provider sends.
//!
//! The model here is a local server that speaks the OpenAI-compatible chat
//! API and behaves as a model that needs its material does: while a file it
//! needs is not in the conversation it asks `read_file` for it, and once
//! everything is there it answers. Nothing in these tests looks at a prompt
//! template or a path list; every assertion is on a captured request body.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use octos_agent::TokenTracker;
use octos_core::task_inputs::DeclaredInput;
use octos_fleet::{AcceptanceVerdict, FleetKernelStore, TaskSpec};
use octos_llm::openai::{ModelHints, OpenAIProvider};
use octos_llm::{ContextWindowOverride, LlmProvider};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::testutil::*;
use crate::worker::{AttemptOutcome, run_attempt};

const SOURCE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n";
const FAILING_TEST: &str =
    "#[test]\nfn adds_two_numbers() {\n    assert_eq!(demo::add(2, 2), 4);\n}\n";
const PAGE_DATA: &str = r#"{"page":3,"title":"Roadmap","bullets":["Q1 pilot","Q2 rollout"]}"#;

/// A 1×1 PNG, standing in for a rendered slide.
const PAGE_IMAGE: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// What the model needs before it can answer: a file, and how to tell from
/// a request that the file is already in the conversation.
struct Need {
    path: &'static str,
    /// For text: a line only that file has (`read_file` numbers the lines it
    /// returns, so only a single line survives both ways of getting the file).
    /// For an image: its base64 payload.
    evidence: String,
    image: bool,
}

fn text(path: &'static str, line: &str) -> Need {
    assert!(!line.contains('\n'));
    Need {
        path,
        evidence: line.to_owned(),
        image: false,
    }
}

fn image(path: &'static str, bytes: &[u8]) -> Need {
    Need {
        path,
        evidence: base64::engine::general_purpose::STANDARD.encode(bytes),
        image: true,
    }
}

/// Every text the request carries, in message order.
fn request_text(body: &Value) -> String {
    let mut out = String::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        match &message["content"] {
            Value::String(text) => out.push_str(text),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        out.push_str(text);
                    }
                }
            }
            _ => {}
        }
        out.push('\n');
    }
    out
}

/// The `data:` URLs of every image the request carries.
fn request_images(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        for part in message["content"].as_array().into_iter().flatten() {
            if let Some(url) = part["image_url"]["url"].as_str() {
                out.push(url.to_owned());
            }
        }
    }
    out
}

fn has(body: &Value, need: &Need) -> bool {
    if need.image {
        request_images(body)
            .iter()
            .any(|url| url.ends_with(&need.evidence))
    } else {
        request_text(body).contains(&need.evidence)
    }
}

/// Start the model. It asks for whatever it still needs, all at once (the
/// best a model without the files can do), and answers when it has it all.
async fn model_needing(needs: Vec<Need>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let missing: Vec<&Need> = needs.iter().filter(|need| !has(&body, need)).collect();
            let streaming = body["stream"] == true;
            let (message, finish) = if missing.is_empty() {
                (
                    json!({ "role": "assistant", "content": "I have what I need." }),
                    "stop",
                )
            } else {
                let calls: Vec<Value> = missing
                    .iter()
                    .enumerate()
                    .map(|(index, need)| {
                        let mut call = json!({
                            "id": format!("read_{index}"),
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": json!({ "path": need.path }).to_string(),
                            },
                        });
                        if streaming {
                            call["index"] = json!(index);
                        }
                        call
                    })
                    .collect();
                (
                    json!({ "role": "assistant", "content": null, "tool_calls": calls }),
                    "tool_calls",
                )
            };
            if streaming {
                let chunk = json!({
                    "choices": [{ "index": 0, "delta": message, "finish_reason": finish }],
                });
                ResponseTemplate::new(200)
                    .append_header("Content-Type", "text/event-stream")
                    .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{ "message": message, "finish_reason": finish }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
                }))
            }
        })
        .mount(&server)
        .await;
    server
}

fn provider_at(server: &MockServer) -> Arc<dyn LlmProvider> {
    Arc::new(
        OpenAIProvider::new("test-key", "deepseek-v4-flash")
            .with_base_url(server.uri())
            .with_hints(ModelHints::detect("deepseek-v4-flash")),
    )
}

/// What the provider sent, request by request.
async fn sent(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

/// How many `read_file` calls the conversation needed, read off the last
/// request: each call the model made is an assistant tool call in it.
fn read_file_calls(requests: &[Value]) -> usize {
    requests
        .last()
        .and_then(|body| body["messages"].as_array())
        .into_iter()
        .flatten()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter(|call| call["function"]["name"] == "read_file")
        .count()
}

/// The request in which the model first had everything it needs, from 1.
fn rounds_to_have(requests: &[Value], needs: &[Need]) -> Option<usize> {
    requests
        .iter()
        .position(|body| needs.iter().all(|need| has(body, need)))
        .map(|index| index + 1)
}

fn write(root: &Path, name: &str, bytes: impl AsRef<[u8]>) {
    let file = root.join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, bytes).unwrap();
}

struct Run {
    outcome: AttemptOutcome,
    requests: Vec<Value>,
}

/// Run one attempt of `task` in `work` against the model at `server`.
async fn run(
    task: TaskSpec,
    work: &TempDir,
    provider: Arc<dyn LlmProvider>,
    server: &MockServer,
) -> Run {
    let (_sd, store): (TempDir, Arc<FleetKernelStore>) = fresh_store().await;
    let fleet = create_fleet(store.clone(), "f1", vec![task]).await;
    let attempt = launch(&store, "f1", "a").await;
    let (_md, factory) = factory_for(provider).await;
    let view = fleet
        .view()
        .await
        .unwrap()
        .tasks
        .into_iter()
        .find(|task| task.task_id == "a")
        .unwrap();
    let outcome = run_attempt(
        store.clone(),
        "f1",
        "a",
        &attempt,
        &view,
        &factory,
        work.path(),
        None,
        Duration::from_secs(30),
        EPOCH,
        slot(),
        Arc::new(TokenTracker::new()),
        PROJECTED,
        || NOW,
    )
    .await;
    Run {
        outcome,
        requests: sent(server).await,
    }
}

fn code_task(inputs: Vec<DeclaredInput>) -> TaskSpec {
    TaskSpec {
        title: "Make the failing test pass".into(),
        detail: "`tests/add.rs` fails against `src/lib.rs`. Find the defect.".into(),
        inputs,
        ..task_spec("a", &[], vec![])
    }
}

fn code_workspace() -> TempDir {
    let work = TempDir::new().unwrap();
    write(work.path(), "src/lib.rs", SOURCE);
    write(work.path(), "tests/add.rs", FAILING_TEST);
    work
}

fn terminated_reason(outcome: &AttemptOutcome) -> &str {
    match outcome {
        AttemptOutcome::Completed {
            verdict: AcceptanceVerdict::Terminated { reason },
        } => reason,
        other => panic!("expected a terminated attempt, got {other:?}"),
    }
}

#[tokio::test]
async fn should_put_the_source_and_the_failing_test_in_the_first_request_of_a_code_task() {
    let needs = || {
        vec![
            text("src/lib.rs", "a - b"),
            text("tests/add.rs", "assert_eq!(demo::add(2, 2), 4);"),
        ]
    };

    // As tasks are dispatched today: the brief names the files, and the
    // worker reads them.
    let server = model_needing(needs()).await;
    let work = code_workspace();
    let before = run(code_task(vec![]), &work, provider_at(&server), &server).await;
    assert!(!has(&before.requests[0], &needs()[0]));
    assert_eq!(rounds_to_have(&before.requests, &needs()), Some(2));
    assert_eq!(read_file_calls(&before.requests), 2);
    assert_eq!(before.requests.len(), 2);

    // With the two files declared: the first request the provider sends
    // already holds both, whole.
    let server = model_needing(needs()).await;
    let work = code_workspace();
    let after = run(
        code_task(vec![
            DeclaredInput::required("src/lib.rs"),
            DeclaredInput::required("tests/add.rs"),
        ]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;

    let first = request_text(&after.requests[0]);
    assert!(first.contains(SOURCE), "{first}");
    assert!(first.contains(FAILING_TEST), "{first}");
    assert!(
        first.contains("`src/lib.rs` — delivered below as text"),
        "{first}"
    );
    assert!(
        first.contains("`tests/add.rs` — delivered below as text"),
        "{first}"
    );
    // The brief is still there, ahead of the inputs.
    assert!(first.find("# Task: Make the failing test pass") < first.find("## Task inputs"));
    assert_eq!(rounds_to_have(&after.requests, &needs()), Some(1));
    assert_eq!(read_file_calls(&after.requests), 0);
    assert_eq!(after.requests.len(), 1);
    assert!(
        matches!(
            after.outcome,
            AttemptOutcome::Completed {
                verdict: AcceptanceVerdict::Accepted { .. }
            }
        ),
        "{:?}",
        after.outcome
    );
}

#[tokio::test]
async fn should_put_the_page_image_and_its_data_in_the_first_request_of_a_page_task() {
    let needs = || {
        vec![
            image("renders/page-03.png", PAGE_IMAGE),
            text("pages/page-03.json", PAGE_DATA),
        ]
    };
    let page_task = |inputs| TaskSpec {
        title: "Review slide 3 against its source".into(),
        detail: "Compare the render `renders/page-03.png` with `pages/page-03.json`.".into(),
        inputs,
        ..task_spec("a", &[], vec![])
    };
    let page_workspace = || {
        let work = TempDir::new().unwrap();
        write(work.path(), "renders/page-03.png", PAGE_IMAGE);
        write(work.path(), "pages/page-03.json", PAGE_DATA);
        work
    };

    let server = model_needing(needs()).await;
    let work = page_workspace();
    let before = run(page_task(vec![]), &work, provider_at(&server), &server).await;
    assert!(request_images(&before.requests[0]).is_empty());
    assert_eq!(rounds_to_have(&before.requests, &needs()), Some(2));
    assert_eq!(read_file_calls(&before.requests), 2);

    let server = model_needing(needs()).await;
    let work = page_workspace();
    let after = run(
        page_task(vec![
            DeclaredInput::required("renders/page-03.png"),
            DeclaredInput::required("pages/page-03.json"),
        ]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;

    // The pixels themselves are in the first request, as an image part.
    let expected = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(PAGE_IMAGE)
    );
    assert_eq!(request_images(&after.requests[0]), [expected]);
    let first = request_text(&after.requests[0]);
    assert!(first.contains(PAGE_DATA), "{first}");
    assert!(
        first.contains("`renders/page-03.png` — delivered as attached image 1"),
        "{first}"
    );
    assert!(
        first.contains("`pages/page-03.json` — delivered below as data"),
        "{first}"
    );
    assert_eq!(rounds_to_have(&after.requests, &needs()), Some(1));
    assert_eq!(read_file_calls(&after.requests), 0);
    assert_eq!(after.requests.len(), 1);
}

#[tokio::test]
async fn should_send_no_request_when_a_required_input_is_missing() {
    let server = model_needing(vec![]).await;
    let work = code_workspace();
    std::fs::remove_file(work.path().join("tests/add.rs")).unwrap();

    let ran = run(
        code_task(vec![
            DeclaredInput::required("src/lib.rs"),
            DeclaredInput::required("tests/add.rs"),
        ]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;

    assert!(
        ran.requests.is_empty(),
        "a model call was made: {:?}",
        ran.requests
    );
    let reason = terminated_reason(&ran.outcome);
    assert!(reason.contains("`tests/add.rs` (missing"), "{reason}");
    // The report says what WAS readable too, so the planner sees the whole picture.
    assert!(reason.contains("src/lib.rs: delivered as text"), "{reason}");
}

#[tokio::test]
async fn should_send_no_request_when_a_required_input_is_not_the_workers_to_read() {
    let server = model_needing(vec![]).await;
    let work = code_workspace();
    let elsewhere = TempDir::new().unwrap();
    write(elsewhere.path(), "secret.txt", "TOP-SECRET\n");
    let secret = elsewhere.path().join("secret.txt");

    // The minimal grant confines the worker's file tools to its working
    // directory; declaring a path does not widen that.
    let ran = run(
        code_task(vec![DeclaredInput::required(secret.to_string_lossy())]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;

    assert!(ran.requests.is_empty());
    let reason = terminated_reason(&ran.outcome);
    assert!(reason.contains("(denied"), "{reason}");
    assert!(!reason.contains("TOP-SECRET"));
}

#[tokio::test]
async fn should_send_no_request_when_required_inputs_exceed_the_models_context() {
    let server = model_needing(vec![]).await;
    let work = code_workspace();
    write(work.path(), "big.txt", "0123456789abcdef\n".repeat(2_000));
    // A 6,000-token window leaves 2,000 tokens for inputs; the file needs more.
    let small: Arc<dyn LlmProvider> =
        Arc::new(ContextWindowOverride::new(provider_at(&server), 6_000));

    let ran = run(
        code_task(vec![DeclaredInput::required("big.txt")]),
        &work,
        small,
        &server,
    )
    .await;

    assert!(ran.requests.is_empty());
    let reason = terminated_reason(&ran.outcome);
    assert!(reason.contains("`big.txt` (over budget"), "{reason}");
}

#[tokio::test]
async fn should_say_in_the_first_request_which_optional_inputs_were_left_out() {
    let server = model_needing(vec![text("src/lib.rs", "a - b")]).await;
    let work = code_workspace();
    write(work.path(), "big.txt", "0123456789abcdef\n".repeat(2_000));
    let small: Arc<dyn LlmProvider> =
        Arc::new(ContextWindowOverride::new(provider_at(&server), 6_000));

    let ran = run(
        code_task(vec![
            DeclaredInput::required("src/lib.rs"),
            DeclaredInput::optional("big.txt"),
            DeclaredInput::optional("docs/design.md"),
        ]),
        &work,
        small,
        &server,
    )
    .await;

    let first = request_text(&ran.requests[0]);
    assert!(first.contains(SOURCE));
    assert!(
        first.contains("`big.txt` — NOT delivered (optional, over budget)"),
        "{first}"
    );
    assert!(
        first.contains("`docs/design.md` — NOT delivered (optional, missing)"),
        "{first}"
    );
    // Left out whole: not one line of it was sent.
    assert!(!first.contains("0123456789abcdef"));
}

#[tokio::test]
async fn should_never_call_an_image_too_large_for_a_model_request_delivered() {
    // A full-resolution render: larger than any image the read tools show a
    // model (they answer `shown_to_model: false`), and smaller than what the
    // media reader would still load.
    let mut original = PAGE_IMAGE.to_vec();
    original.resize(5 * 1024 * 1024 + 1, 0);
    let page_task = |inputs| TaskSpec {
        title: "Review slide 3 against its source".into(),
        detail: "Compare `renders/original.png` with `pages/page-03.json`.".into(),
        inputs,
        ..task_spec("a", &[], vec![])
    };
    let page_workspace = || {
        let work = TempDir::new().unwrap();
        write(work.path(), "renders/original.png", &original);
        write(work.path(), "pages/page-03.json", PAGE_DATA);
        work
    };

    // Optional: the request goes out without it, and says so in so many words.
    let server = model_needing(vec![]).await;
    let work = page_workspace();
    let ran = run(
        page_task(vec![
            DeclaredInput::required("pages/page-03.json"),
            DeclaredInput::optional("renders/original.png"),
        ]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;
    assert!(request_images(&ran.requests[0]).is_empty());
    let first = request_text(&ran.requests[0]);
    assert!(first.contains(PAGE_DATA), "{first}");
    assert!(
        first.contains("`renders/original.png` — NOT delivered (optional, unsupported)"),
        "{first}"
    );
    assert!(first.contains("over the 5242880-byte limit"), "{first}");

    // Required: no request is made with only its path in it.
    let server = model_needing(vec![]).await;
    let work = page_workspace();
    let ran = run(
        page_task(vec![DeclaredInput::required("renders/original.png")]),
        &work,
        provider_at(&server),
        &server,
    )
    .await;
    assert!(ran.requests.is_empty(), "{:?}", ran.requests);
    let reason = terminated_reason(&ran.outcome);
    assert!(
        reason.contains("`renders/original.png` (unsupported"),
        "{reason}"
    );
}
