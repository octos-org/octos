//! UPCR-2026-RBC0874AA — the declared inputs of a handed-off peer, seen where
//! the peer's model sees them: in the HTTP request the provider sends.
//!
//! The handoff goes through the real `peer_handoff` tool and host callback,
//! the peer's turns through `turn/start`, and the model is a local server
//! speaking the OpenAI-compatible chat API that behaves as a model that needs
//! its material does: while a file it needs is not in the conversation it
//! asks `read_file` for it, and once everything is there it answers. Every
//! assertion is on a captured request body or on the peer's recorded report;
//! none looks at a prompt template or a path list.
use super::peer_host_tools_tests::dev_profile;
use super::tests::test_support::ws_connection_for_test;
use super::*;

use base64::Engine as _;
use octos_agent::Tool as _;
use octos_core::task_inputs::{InputOutcome, InputReport, OmitReason};
use octos_core::ui_protocol::PeerStagedEvent;
use octos_llm::openai::{ModelHints, OpenAIProvider};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// See `peer_host_tools_tests::TURN_ARRIVAL_BUDGET`: an arrival wait, not a
/// latency contract.
const TURN_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

const BRIEF: &str = "Review slide 3 against its data (pages/page-03.json, renders/page-03.png) \
                     and say what does not match.";
const PAGE_DATA: &str = r#"{"page":3,"title":"Roadmap","bullets":["Q1 pilot","Q2 rollout"]}"#;

/// A 1×1 PNG, standing in for a rendered slide.
const PAGE_IMAGE: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn image_url(bytes: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// What the model needs before it can answer, and how to tell from a request
/// that it is already in the conversation: a line only that file has, or an
/// image's `data:` URL.
struct Need {
    path: &'static str,
    evidence: String,
    image: bool,
}

fn page_needs() -> Vec<Need> {
    vec![
        Need {
            path: "pages/page-03.json",
            evidence: PAGE_DATA.to_owned(),
            image: false,
        },
        Need {
            path: "renders/page-03.png",
            evidence: image_url(PAGE_IMAGE),
            image: true,
        },
    ]
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
        request_images(body).contains(&need.evidence)
    } else {
        request_text(body).contains(&need.evidence)
    }
}

/// Start the model. It asks for whatever it still needs, all at once (the
/// best a model without the files can do), and answers when it has it all.
async fn model_needing(needs: Vec<Need>) -> MockServer {
    let server = MockServer::start().await;
    now_needing(&server, needs).await;
    server
}

/// From here on the model behind `server` needs `needs`, and the requests it
/// has had so far are forgotten.
async fn now_needing(server: &MockServer, needs: Vec<Need>) {
    server.reset().await;
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
        .mount(server)
        .await;
}

/// The chat requests the provider sent, one by one. A provider may also ask
/// the endpoint about its model; those are not requests of the conversation.
async fn sent(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

/// How many `read_file` calls the conversation needed, read off its last
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

struct Fx {
    tmp: tempfile::TempDir,
    state: Arc<AppState>,
    data_dir: PathBuf,
    /// The master session's workspace; an unfenced peer works in it too.
    workspace: PathBuf,
    master: SessionKey,
    server: MockServer,
    active: SharedActiveTurns,
}

/// A served profile whose model is the real OpenAI-compatible provider,
/// pointed at a local server that needs `needs`. `context_window` shrinks
/// what the provider says its model can take.
async fn fixture(needs: Vec<Need>, context_window: Option<u32>) -> Fx {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let server = model_needing(needs).await;
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let mut llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(
        OpenAIProvider::new("test-key", "deepseek-v4-flash")
            .with_base_url(server.uri())
            .with_hints(ModelHints::detect("deepseek-v4-flash")),
    );
    if let Some(window) = context_window {
        llm = Arc::new(octos_llm::ContextWindowOverride::new(llm, window));
    }
    let runtime = {
        let base = crate::runtime::ProfileRuntime::bootstrap(
            &dev_profile("FIRST_INPUTS_TEST_KEY", "k"),
            &data_dir,
            None,
            crate::runtime::BootstrapRole::Serve,
        )
        .await
        .expect("bootstrap");
        let Ok(mut runtime) = Arc::try_unwrap(base) else {
            panic!("the freshly bootstrapped runtime has one owner");
        };
        runtime.llm = llm;
        // A turn-end hook that keeps the payload it is handed.
        #[cfg(unix)]
        {
            let keep = format!("cat > '{}'", data_dir.join(TURN_END_PAYLOAD).display());
            let hook: octos_agent::HookConfig = serde_json::from_value(json!({
                "event": "on_turn_end",
                "command": ["sh", "-c", keep],
            }))
            .unwrap();
            runtime.hook_executor = Some(Arc::new(octos_agent::HookExecutor::new(vec![hook])));
        }
        Arc::new(runtime)
    };
    let mut state = AppState::empty_for_tests();
    state.profiles.insert("dev".to_string(), runtime.clone());
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(&data_dir).unwrap(),
    )));
    let chat = format!(
        "first-inputs-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let master = SessionKey::with_profile_topic("dev", "api", &chat, "main");
    let workspace = crate::runtime::SessionRuntime::bootstrap(&runtime, master.clone(), None)
        .await
        .unwrap()
        .workspace_root
        .clone();
    std::fs::create_dir_all(&workspace).unwrap();
    Fx {
        tmp,
        state: Arc::new(state),
        data_dir,
        workspace,
        master,
        server,
        active: Arc::new(TokioMutex::new(HashMap::new())),
    }
}

/// Where the fixture's turn-end hook leaves its payload, under the data dir.
const TURN_END_PAYLOAD: &str = "turn-end.json";

fn write(root: &Path, name: &str, bytes: impl AsRef<[u8]>) {
    let file = root.join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, bytes).unwrap();
}

/// The public sample: one slide's data and its rendering.
fn page_workspace(fx: &Fx) {
    write(&fx.workspace, "pages/page-03.json", PAGE_DATA);
    write(&fx.workspace, "renders/page-03.png", PAGE_IMAGE);
}

/// Hand the page review to a peer named `name`, as a master's model would:
/// through the `peer_handoff` tool, staged by the host callback. Returns the
/// tool's result and the `peer/staged` event a client would open the peer on.
async fn hand_off(
    fx: &Fx,
    name: &str,
    inputs: Option<Value>,
) -> (octos_agent::ToolResult, Option<PeerStagedEvent>) {
    hand_off_into(fx, name, inputs, false).await
}

/// [`hand_off`], to a peer that works in the master's directory or, `fenced`,
/// in a git worktree of its own.
async fn hand_off_into(
    fx: &Fx,
    name: &str,
    inputs: Option<Value>,
    fenced: bool,
) -> (octos_agent::ToolResult, Option<PeerStagedEvent>) {
    let staged: Arc<std::sync::Mutex<Vec<PeerStagedEvent>>> = Arc::default();
    let seen = staged.clone();
    let stage = crate::peers::build_peer_handoff_callback(
        fx.data_dir.join("peers"),
        fx.workspace.clone(),
        fx.master.clone(),
        "dev".to_owned(),
        Some(Arc::new(
            octos_core::SessionScope::solo(fx.workspace.clone(), Vec::new()).unwrap(),
        )),
        Vec::new(),
        Arc::new(AtomicU32::new(0)),
        Arc::new(move |event| seen.lock().unwrap().push(event)),
    );
    let mut args = json!({ "name": name, "brief": BRIEF, "worktree": fenced });
    if let Some(inputs) = inputs {
        args["inputs"] = inputs;
    }
    let result = octos_agent::PeerHandoffTool::new(stage)
        .execute(&args)
        .await
        .unwrap();
    let event = staged.lock().unwrap().pop();
    (result, event)
}

/// Send `text` as the next turn of the peer's own session, as the client
/// that opened it does, and wait until the turn wrote the peer's result.
async fn peer_turn(fx: &Fx, peer: &PeerStagedEvent, text: &str) {
    let session = SessionKey(format!("{}#{}", fx.master.base_key(), peer.topic));
    let turn_id = TurnId::new();
    let (ws, mut rx) = ws_connection_for_test(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let deadline = std::time::Instant::now() + TURN_BUDGET;
    let mut attempt = 0usize;
    loop {
        // The previous turn's result is on disk a moment before its slot is free.
        let admitted = handle_turn_start(
            &ws,
            &fx.state,
            &Arc::new(UiProtocolLedger::new(256)),
            &Arc::new(UiProtocolContractStores::default()),
            &fx.active,
            &connection_turns,
            None,
            None,
            ConnectionUiFeatures::stdio_defaults(),
            format!("turn-{attempt}"),
            TurnStartParams {
                session_id: session.clone(),
                turn_id: turn_id.clone(),
                input: vec![InputItem::Text { text: text.into() }],
                media: Vec::new(),
                topic: None,
                rewrite_for: None,
                reasoning_effort: None,
                tool_context: None,
                live_video: false,
                origin: None,
            },
        )
        .await;
        if admitted {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn was never admitted"
        );
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let result = fx.data_dir.join("peers").join(&peer.slug).join("result.md");
    let needle = format!("turn_id: {}", turn_id.0);
    while !std::fs::read_to_string(&result).is_ok_and(|body| body.contains(&needle)) {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn never wrote the peer's result"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// What the peer's latest message that was due inputs was recorded to carry.
fn recorded(fx: &Fx, peer: &PeerStagedEvent) -> (bool, InputReport) {
    crate::peers::first_inputs::delivery_records(&fx.data_dir.join("peers"), &peer.slug)
        .pop()
        .expect("the turn recorded what it delivered")
}

fn omit_reason(report: &InputReport, path: &str) -> Option<OmitReason> {
    report
        .inputs
        .iter()
        .find(|record| record.path == path)
        .and_then(|record| match &record.outcome {
            InputOutcome::Omitted { reason, .. } => Some(*reason),
            InputOutcome::Delivered { .. } => None,
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_put_the_page_image_and_its_data_in_a_handed_off_peers_first_request() {
    // Before: the brief names the files and the peer's model fetches them.
    let old = fixture(page_needs(), None).await;
    page_workspace(&old);
    let (result, peer) = hand_off(&old, "Page Old", None).await;
    assert!(result.success, "{}", result.output);
    peer_turn(&old, &peer.unwrap(), BRIEF).await;
    let fetched = sent(&old.server).await;
    assert_eq!(rounds_to_have(&fetched, &page_needs()), Some(2));
    assert_eq!(read_file_calls(&fetched), 2);

    // Now: the handoff declares them, with speaker notes too long to skim.
    let new = fixture(page_needs(), None).await;
    page_workspace(&new);
    let notes = format!(
        "NOTES-BEGIN\n{}NOTES-END\n",
        "Say the pilot starts in Q1 and name the two launch regions.\n".repeat(2_000)
    );
    write(&new.workspace, "notes/page-03.md", &notes);
    let (result, peer) = hand_off(
        &new,
        "Page New",
        Some(json!([
            "pages/page-03.json",
            { "path": "renders/page-03.png", "kind": "image" },
            { "path": "notes/page-03.md", "required": false },
        ])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    assert!(
        result
            .output
            .contains("- `renders/page-03.png`: ready (image, 70 bytes)"),
        "{}",
        result.output
    );
    let peer = peer.unwrap();
    // The client sends what it always sent: the brief, and nothing else.
    peer_turn(&new, &peer, BRIEF).await;

    let requests = sent(&new.server).await;
    assert_eq!(
        requests.len(),
        1,
        "one model round, and no file fetched: {requests:#?}"
    );
    assert_eq!(read_file_calls(&requests), 0);
    let first = &requests[0];
    assert_eq!(request_images(first), [image_url(PAGE_IMAGE)]);
    let text = request_text(first);
    assert!(text.contains(BRIEF));
    assert!(text.contains(PAGE_DATA), "the page data, not its path");
    assert!(text.contains(&notes), "all {} bytes of notes", notes.len());

    let (delivered, report) = recorded(&new, &peer);
    assert!(delivered);
    assert_eq!(report.delivered().count(), 3, "{}", report.summary());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_replay_a_peers_inputs_once_in_later_turns_as_they_were_first_read() {
    let fx = fixture(Vec::new(), None).await;
    page_workspace(&fx);
    let (result, peer) = hand_off(
        &fx,
        "Page Replay",
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    let peer = peer.unwrap();
    peer_turn(&fx, &peer, BRIEF).await;

    // The files move on; the conversation keeps what its first message read.
    write(
        &fx.workspace,
        "pages/page-03.json",
        r#"{"page":3,"title":"Changed"}"#,
    );
    write(
        &fx.workspace,
        "renders/page-03.png",
        b"\x89PNG\r\n\x1a\nchanged",
    );
    peer_turn(&fx, &peer, "Anything else?").await;

    let requests = sent(&fx.server).await;
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(request_images(second), [image_url(PAGE_IMAGE)]);
    let text = request_text(second);
    assert_eq!(text.matches(PAGE_DATA).count(), 1, "{text}");
    assert!(!text.contains("Changed"));
    assert!(text.contains("Anything else?"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_refuse_the_handoff_when_a_required_input_is_missing() {
    let fx = fixture(Vec::new(), None).await;
    write(&fx.workspace, "pages/page-03.json", PAGE_DATA);

    let (result, peer) = hand_off(
        &fx,
        "Page Missing",
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
    )
    .await;

    assert!(!result.success);
    assert!(
        result
            .output
            .contains("`renders/page-03.png`: NOT ready (required, missing)"),
        "{}",
        result.output
    );
    assert!(peer.is_none(), "no client was told to open a peer");
    let staged = std::fs::read_dir(fx.data_dir.join("peers"))
        .map(|dir| dir.count())
        .unwrap_or(0);
    assert_eq!(staged, 0, "nothing is left staged");
    assert!(sent(&fx.server).await.is_empty());
}

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fenced peer works in its own git worktree, which holds what is
/// committed. The master's directory also holds the uncommitted rendering,
/// and what the master can read there says nothing of what the peer will find.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_refuse_a_fenced_handoff_whose_required_input_is_not_committed() {
    let fx = fixture(Vec::new(), None).await;
    write(&fx.workspace, "pages/page-03.json", PAGE_DATA);
    git(&fx.workspace, &["init", "-q"]);
    git(&fx.workspace, &["add", "pages"]);
    git(&fx.workspace, &["commit", "-q", "-m", "page data"]);
    write(&fx.workspace, "renders/page-03.png", PAGE_IMAGE);

    let (result, peer) = hand_off_into(
        &fx,
        "Page Fenced",
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
        true,
    )
    .await;

    assert!(!result.success, "{}", result.output);
    assert!(
        result
            .output
            .contains("`renders/page-03.png`: NOT ready (required, missing)"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("holds only committed files"),
        "{}",
        result.output
    );
    assert!(peer.is_none(), "no client was told to open a peer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_tell_the_peer_nothing_was_delivered_when_a_required_input_changed_or_went() {
    let fx = fixture(Vec::new(), None).await;
    page_workspace(&fx);
    let pin = format!("{:x}", Sha256::digest(PAGE_DATA.as_bytes()));

    // Gone between the handoff and the peer's first turn.
    let (result, gone) = hand_off(
        &fx,
        "Page Gone",
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    let gone = gone.unwrap();
    std::fs::remove_file(fx.workspace.join("renders/page-03.png")).unwrap();
    peer_turn(&fx, &gone, BRIEF).await;

    // Rewritten since the handoff pinned its content.
    let (result, changed) = hand_off(
        &fx,
        "Page Changed",
        Some(json!([{ "path": "pages/page-03.json", "sha256": pin }])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    let changed = changed.unwrap();
    write(
        &fx.workspace,
        "pages/page-03.json",
        r#"{"page":3,"title":"Rewritten"}"#,
    );
    peer_turn(&fx, &changed, BRIEF).await;

    let requests = sent(&fx.server).await;
    assert_eq!(requests.len(), 2);
    let text = request_text(&requests[0]);
    assert!(text.contains("## Task inputs — NOT delivered"), "{text}");
    assert!(
        text.contains("`renders/page-03.png` — NOT delivered (required, missing)"),
        "{text}"
    );
    assert!(
        text.contains("`pages/page-03.json` — readable, and withheld with the rest"),
        "{text}"
    );
    assert!(!text.contains(PAGE_DATA), "a partial set is not delivered");
    assert!(request_images(&requests[0]).is_empty());
    let (delivered, report) = recorded(&fx, &gone);
    assert!(!delivered);
    assert_eq!(
        omit_reason(&report, "renders/page-03.png"),
        Some(OmitReason::Missing)
    );

    let text = request_text(&requests[1]);
    assert!(
        text.contains("`pages/page-03.json` — NOT delivered (required, changed)"),
        "{text}"
    );
    assert!(!text.contains("Rewritten"), "{text}");
    let (delivered, report) = recorded(&fx, &changed);
    assert!(!delivered);
    assert_eq!(
        omit_reason(&report, "pages/page-03.json"),
        Some(OmitReason::Changed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_withhold_a_peers_inputs_when_they_exceed_the_models_context() {
    let fx = fixture(Vec::new(), Some(64_000)).await;
    page_workspace(&fx);
    write(
        &fx.workspace,
        "notes/deck.md",
        "DECK-NOTES-LINE the pilot starts in Q1\n".repeat(12_000),
    );

    let (result, peer) = hand_off(
        &fx,
        "Page Budget",
        Some(json!(["pages/page-03.json", "notes/deck.md"])),
    )
    .await;
    // Whether it fits is the peer's model's to say, at its first request.
    assert!(result.success, "{}", result.output);
    let peer = peer.unwrap();
    peer_turn(&fx, &peer, BRIEF).await;

    let requests = sent(&fx.server).await;
    let text = request_text(&requests[0]);
    assert!(
        text.contains("`notes/deck.md` — NOT delivered (required, over budget)"),
        "{text}"
    );
    assert!(!text.contains("DECK-NOTES-LINE"), "no part of it is sent");
    assert!(!text.contains(PAGE_DATA));
    let (delivered, report) = recorded(&fx, &peer);
    assert!(!delivered);
    assert_eq!(
        omit_reason(&report, "notes/deck.md"),
        Some(OmitReason::OverBudget)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_not_read_for_a_peer_what_its_own_session_may_not_read() {
    let fx = fixture(Vec::new(), None).await;
    page_workspace(&fx);
    let outside = fx.tmp.path().join("elsewhere/plan.md");
    write(
        fx.tmp.path(),
        "elsewhere/plan.md",
        "NOT-THE-PEERS-TO-READ\n",
    );

    let (result, peer) = hand_off(
        &fx,
        "Page Denied",
        Some(json!(["pages/page-03.json", outside.to_string_lossy()])),
    )
    .await;
    // Outside the working directory: the peer session's own reach decides.
    assert!(result.success, "{}", result.output);
    assert!(
        result
            .output
            .contains("left for the peer's own file access"),
        "{}",
        result.output
    );
    let peer = peer.unwrap();
    peer_turn(&fx, &peer, BRIEF).await;

    let requests = sent(&fx.server).await;
    let text = request_text(&requests[0]);
    assert!(text.contains("NOT delivered (required, denied)"), "{text}");
    assert!(!text.contains("NOT-THE-PEERS-TO-READ"));
    let (delivered, report) = recorded(&fx, &peer);
    assert!(!delivered);
    assert_eq!(
        omit_reason(&report, &outside.to_string_lossy()),
        Some(OmitReason::Denied)
    );
}

const PAGE_DATA_V2: &str =
    r#"{"page":3,"title":"Roadmap","bullets":["Q1 pilot","Q2 rollout in two regions"]}"#;

/// The slide rendered again after that edit: a 1×1 PNG of another colour.
const PAGE_IMAGE_V2: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xb8, 0x60, 0x60, 0xf0,
    0x1f, 0x00, 0x05, 0x34, 0x02, 0x30, 0x85, 0xe7, 0x51, 0xa2, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const REVIEW: &str = "R2-1: the Q2 bullet is cut off at the right edge; R2-2: the title \
                      overlaps the logo.";
const REPAIR: &str = "Repair slide 3: fix what review/round-2.md lists, against the current \
                      pages/page-03.json and renders/page-03.png.";

fn repair_needs() -> Vec<Need> {
    vec![
        Need {
            path: "pages/page-03.json",
            evidence: PAGE_DATA_V2.to_owned(),
            image: false,
        },
        Need {
            path: "renders/page-03.png",
            evidence: image_url(PAGE_IMAGE_V2),
            image: true,
        },
        Need {
            path: "review/round-2.md",
            evidence: REVIEW.to_owned(),
            image: false,
        },
    ]
}

/// A peer named `name` that has made its first draft of the page, from the
/// data and render its handoff declared, and is open for more input.
async fn drafted(fx: &Fx, name: &str) -> (PeerStagedEvent, SessionKey) {
    page_workspace(fx);
    let (result, peer) = hand_off(
        fx,
        name,
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    let peer = peer.unwrap();
    peer_turn(fx, &peer, BRIEF).await;
    let session = SessionKey(format!("{}#{}", fx.master.base_key(), peer.topic));
    register_peer_wire_session(&fx.state, &session);
    (peer, session)
}

/// Send `message` to an existing peer as its master's model would: through
/// the `peer_send_input` tool and the host's delivery.
async fn send_input(
    fx: &Fx,
    peer: &PeerStagedEvent,
    message: &str,
    inputs: Value,
) -> octos_agent::ToolResult {
    let workspace = fx.workspace.clone();
    let scope = octos_core::SessionScope::solo(workspace, Vec::new()).expect("test scope");
    send_input_to(fx, peer, message, inputs, Some(Arc::new(scope))).await
}

/// [`send_input`], with the peer's session live (`scope` is its reach) or not.
async fn send_input_to(
    fx: &Fx,
    peer: &PeerStagedEvent,
    message: &str,
    inputs: Value,
    scope: Option<Arc<octos_core::SessionScope>>,
) -> octos_agent::ToolResult {
    let peers_root = fx.data_dir.join("peers");
    let origin = fx.master.to_string();
    let turn = TurnId::new();
    let tool = octos_agent::PeerSendInputTool::new(Arc::new(move |request| {
        deliver_peer_send_input("dev", &peers_root, &origin, &turn, request, scope.clone())
    }));
    tool.execute(&json!({ "slug": peer.slug, "message": message, "inputs": inputs }))
        .await
        .unwrap()
}

/// Run the peer's queued input as its next turn, as the server's continuation
/// tick does, and wait until that turn wrote the peer's second result.
async fn run_queued_input(fx: &Fx, session: &SessionKey, peer: &PeerStagedEvent) {
    let (ws, mut rx) = ws_connection_for_test(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let deadline = std::time::Instant::now() + TURN_BUDGET;
    // The previous turn's result is on disk a moment before its slot is free.
    while !maybe_spawn_appui_master_continuation_runner(
        &ws,
        &fx.state,
        &Arc::new(UiProtocolLedger::new(256)),
        &Arc::new(UiProtocolContractStores::default()),
        &fx.active,
        &connection_turns,
        session.clone(),
        session.clone(),
        "dev".to_owned(),
        ConnectionUiFeatures::stdio_defaults(),
    )
    .await
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the queued input was never run"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let result = fx
        .data_dir
        .join("peers")
        .join(&peer.slug)
        .join("result-2.md");
    while !result.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the input's turn never wrote the peer's result"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// One page's repair round: the peer has made its first draft, the page was
/// edited and rendered again, a review came back, and the master sends the
/// repair to the same peer, declaring `inputs`. Returns the requests the
/// repair's turn made.
async fn repair_round(name: &str, inputs: Value) -> (Fx, PeerStagedEvent, Vec<Value>) {
    let fx = fixture(Vec::new(), None).await;
    let (peer, session) = drafted(&fx, name).await;
    write(&fx.workspace, "pages/page-03.json", PAGE_DATA_V2);
    write(&fx.workspace, "renders/page-03.png", PAGE_IMAGE_V2);
    write(&fx.workspace, "review/round-2.md", REVIEW);
    now_needing(&fx.server, repair_needs()).await;

    let queued = send_input(&fx, &peer, REPAIR, inputs).await;
    assert!(queued.success, "{}", queued.output);
    run_queued_input(&fx, &session, &peer).await;
    let requests = sent(&fx.server).await;
    (fx, peer, requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_put_a_repair_rounds_new_files_in_the_first_request_of_an_existing_peers_turn() {
    // Before: the message names the files and the peer's model fetches them.
    let (_old, _, fetched) = repair_round("Repair Old", Value::Null).await;
    assert_eq!(rounds_to_have(&fetched, &repair_needs()), Some(2));
    assert_eq!(read_file_calls(&fetched), 3);

    // Now: the message declares them.
    let (new, peer, requests) = repair_round(
        "Repair New",
        json!([
            "pages/page-03.json",
            "renders/page-03.png",
            "review/round-2.md"
        ]),
    )
    .await;
    assert_eq!(
        requests.len(),
        1,
        "one model round, and no file fetched: {requests:#?}"
    );
    assert_eq!(read_file_calls(&requests), 0);
    let first = &requests[0];
    // The first draft's render stays as it was read then; the repair's
    // message brings the new one, not the old one again.
    assert_eq!(
        request_images(first),
        [image_url(PAGE_IMAGE), image_url(PAGE_IMAGE_V2)]
    );
    let text = request_text(first);
    assert!(text.contains(REPAIR));
    assert!(text.contains(PAGE_DATA_V2), "the page as it is now");
    assert!(text.contains(REVIEW), "the review, not its path");

    let records =
        crate::peers::first_inputs::delivery_records(&new.data_dir.join("peers"), &peer.slug);
    assert_eq!(records.len(), 2, "the draft's inputs, then the repair's");
    assert!(records[1].0);
    assert_eq!(
        records[1].1.delivered().count(),
        3,
        "{}",
        records[1].1.summary()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_refuse_a_message_whose_required_input_is_missing_and_tell_the_peer_if_it_goes_later()
 {
    let fx = fixture(Vec::new(), None).await;
    let (peer, session) = drafted(&fx, "Repair Missing").await;
    let pending = || {
        default_agent_orchestrator()
            .pending_continuation_count_for_session_for_test(&session, "dev")
    };

    // Not there when the message is sent: refused, and nothing is queued.
    let refused = send_input(&fx, &peer, REPAIR, json!(["review/round-2.md"])).await;
    assert!(!refused.success);
    assert!(
        refused.output.contains("input not sent")
            && refused
                .output
                .contains("`review/round-2.md`: NOT ready (required, missing)"),
        "{}",
        refused.output
    );
    assert_eq!(pending(), 0);
    // A host app runs its peer's turns itself; nothing would bring the files.
    let peer_dir = fx.data_dir.join("peers").join(&peer.slug);
    write(
        &peer_dir,
        crate::peers::app_binding::HOST_BINDING_LEAF,
        "{}",
    );
    let refused = send_input(&fx, &peer, REPAIR, json!(["pages/page-03.json"])).await;
    assert!(
        refused.output.contains("run by its host app"),
        "{}",
        refused.output
    );
    assert_eq!(pending(), 0);
    std::fs::remove_file(peer_dir.join(crate::peers::app_binding::HOST_BINDING_LEAF)).unwrap();

    // There when sent, gone when the peer gets to it: the turn runs, and says so.
    write(&fx.workspace, "review/round-2.md", REVIEW);
    let queued = send_input(&fx, &peer, REPAIR, json!(["review/round-2.md"])).await;
    assert!(queued.success, "{}", queued.output);
    std::fs::remove_file(fx.workspace.join("review/round-2.md")).unwrap();
    run_queued_input(&fx, &session, &peer).await;

    let requests = sent(&fx.server).await;
    assert_eq!(requests.len(), 2, "the draft's turn and the repair's");
    let text = request_text(&requests[1]);
    assert!(text.contains(REPAIR));
    assert!(
        text.contains("`review/round-2.md` — NOT delivered (required, missing)"),
        "{text}"
    );
    let (delivered, report) = recorded(&fx, &peer);
    assert!(!delivered);
    assert_eq!(
        omit_reason(&report, "review/round-2.md"),
        Some(OmitReason::Missing)
    );
}

/// From here on the endpoint behind `server` takes no image parts: a request
/// with one is refused, as a text-only proxy in front of a vision model does.
/// With the peer's session not live, nothing looks at the files a message
/// declares, and the sender is not left believing something did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_tell_the_sender_when_a_messages_inputs_were_not_checked() {
    let fx = fixture(page_needs(), None).await;
    let (peer, _session) = drafted(&fx, "Page Unchecked").await;

    let sent = send_input_to(&fx, &peer, REPAIR, json!(["review/not-written.md"]), None).await;

    assert!(sent.success, "{}", sent.output);
    assert!(
        sent.output
            .contains("Its 1 declared input(s) were NOT checked here"),
        "{}",
        sent.output
    );
    assert!(!sent.output.contains("missing"), "{}", sent.output);
}

/// The summary a turn-end hook is handed is of what the turn was asked. The
/// files attached to that message went to the model, not into the summary.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_summarize_a_turn_for_its_end_hook_by_what_it_was_asked_not_by_its_inputs() {
    let fx = fixture(page_needs(), None).await;
    page_workspace(&fx);
    let (result, peer) = hand_off(
        &fx,
        "Page Hooked",
        Some(json!(["pages/page-03.json", "renders/page-03.png"])),
    )
    .await;
    assert!(result.success, "{}", result.output);

    peer_turn(&fx, &peer.unwrap(), BRIEF).await;

    let kept = fx.data_dir.join(TURN_END_PAYLOAD);
    let deadline = std::time::Instant::now() + TURN_BUDGET;
    let payload = loop {
        let read = std::fs::read_to_string(&kept).ok();
        if let Some(payload) = read.and_then(|body| serde_json::from_str::<Value>(&body).ok()) {
            break payload;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no turn-end hook payload"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let summary = payload["turn_summary"].as_str().unwrap();
    assert!(summary.starts_with("Review slide 3"), "{summary}");
    assert!(!summary.contains("Task inputs"), "{summary}");
    assert!(!summary.contains("Roadmap"), "{summary}");
    // The request itself carried the page data.
    assert!(request_text(&sent(&fx.server).await[0]).contains(PAGE_DATA));
}

async fn refusing_images(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("\"image_url\""))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "message": "This model does not support image input" },
        })))
        .with_priority(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_fail_a_peers_turn_for_a_refused_required_image_and_correct_the_report_for_an_optional_one()
 {
    let fx = fixture(Vec::new(), None).await;
    page_workspace(&fx);
    refusing_images(&fx.server).await;
    let peers = fx.data_dir.join("peers");

    // Required: the refused request is the only one, and the turn fails with
    // the reason instead of running without the image.
    let (result, blind) = hand_off(&fx, "Page Blind", Some(json!(["renders/page-03.png"]))).await;
    assert!(result.success, "{}", result.output);
    let blind = blind.unwrap();
    peer_turn(&fx, &blind, BRIEF).await;
    assert_eq!(sent(&fx.server).await.len(), 1);
    let said = std::fs::read_to_string(peers.join(&blind.slug).join("result.md")).unwrap();
    assert!(
        said.contains("outcome: errored")
            && said.contains("required image not delivered: the endpoint refuses image input"),
        "{said}"
    );

    // Optional: the request goes again without it, and the report that called
    // the image delivered is corrected.
    let (result, degraded) = hand_off(
        &fx,
        "Page Degraded",
        Some(json!([
            "pages/page-03.json",
            { "path": "renders/page-03.png", "required": false },
        ])),
    )
    .await;
    assert!(result.success, "{}", result.output);
    let degraded = degraded.unwrap();
    peer_turn(&fx, &degraded, BRIEF).await;
    let requests = sent(&fx.server).await;
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert!(request_images(&requests[2]).is_empty());
    let text = request_text(&requests[2]);
    assert!(text.contains(PAGE_DATA));
    // What that request tells the model matches what it carries.
    assert!(
        text.contains("`renders/page-03.png` — NOT delivered (unsupported): the endpoint refuses"),
        "{text}"
    );
    assert!(!text.contains("delivered as attached image"), "{text}");
    assert!(!text.contains("user-uploaded files"), "{text}");
    let records = crate::peers::first_inputs::delivery_records(&peers, &degraded.slug);
    assert_eq!(records.len(), 2, "as sent, then as it went out");
    assert_eq!(omit_reason(&records[0].1, "renders/page-03.png"), None);
    assert_eq!(
        omit_reason(&records[1].1, "renders/page-03.png"),
        Some(OmitReason::Unsupported)
    );
}
