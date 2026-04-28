//! M7.6 — integration tests for the swarm dispatch dashboard backend.
//!
//! These exercise the five REST endpoints + the typed
//! `HarnessEventPayload::SwarmReviewDecision` event emission. A stub
//! [`McpAgentBackend`] drives the primitive so the tests never spawn a
//! real MCP subprocess.

#![cfg(feature = "api")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use octos_agent::cost_ledger::{CostAttributionEvent, CostLedger, PersistentCostLedger};
use octos_cli::api::swarm::CostAttributionsResponse;
use octos_cli::api::{
    AppState, SwarmDispatchDetail, SwarmDispatchResponse, SwarmDispatchesResponse,
    SwarmReviewRequest, SwarmReviewResponse, build_router, build_test_swarm_state,
};
use serde_json::json;
use tempfile::TempDir;
use tower::util::ServiceExt;

async fn build_app_state_async(dir: &TempDir) -> (Arc<AppState>, Arc<PersistentCostLedger>) {
    let cost_ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let swarm_state = Arc::new(
        build_test_swarm_state(dir.path().join("swarm"), cost_ledger.clone())
            .await
            .unwrap(),
    );
    let mut state = AppState::empty_for_tests();
    state.swarm_state = Some(swarm_state);
    (Arc::new(state), cost_ledger)
}

fn dispatch_body(dispatch_id: &str, contract_id: &str) -> String {
    serde_json::to_string(&json!({
        "schema_version": 1,
        "dispatch_id": dispatch_id,
        "contract_id": contract_id,
        "contracts": [
            {
                "contract_id": "sub-1",
                "tool_name": "claude_code/run_task",
                "task": { "prompt": "write a haiku" },
                "label": "haiku-1",
            },
            {
                "contract_id": "sub-2",
                "tool_name": "claude_code/run_task",
                "task": { "prompt": "write another haiku" },
                "label": "haiku-2",
            },
        ],
        "topology": {
            "kind": "parallel",
            "max_concurrency": 2,
        },
        "budget": {},
        "context": {
            "session_id": "api:swarm-test",
            "task_id": "task-1",
            "workflow": "swarm",
            "phase": "dispatch",
        },
    }))
    .unwrap()
}

async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).expect("valid JSON body")
}

#[tokio::test]
async fn should_dispatch_swarm_and_return_dispatch_id() {
    let dir = TempDir::new().unwrap();
    let (state, _ledger) = build_app_state_async(&dir).await;
    let app = build_router(state);

    let body = dispatch_body("d1", "contract-a");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swarm/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: SwarmDispatchResponse = read_json(resp).await;
    assert_eq!(parsed.dispatch_id, "d1");
    assert_eq!(parsed.total_subtasks, 2);
    assert_eq!(parsed.completed_subtasks, 2);
    assert_eq!(parsed.outcome, "success");
}

#[tokio::test]
async fn should_list_recent_dispatches_with_cost_rollup() {
    let dir = TempDir::new().unwrap();
    let (state, _ledger) = build_app_state_async(&dir).await;
    let app = build_router(state);

    let body = dispatch_body("d2", "contract-b");
    let dispatch_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swarm/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dispatch_resp.status(), StatusCode::OK);

    let list_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/swarm/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list: SwarmDispatchesResponse = read_json(list_resp).await;
    assert_eq!(list.dispatches.len(), 1);
    let row = &list.dispatches[0];
    assert_eq!(row.dispatch_id, "d2");
    assert_eq!(row.contract_id, "contract-b");
    assert_eq!(row.topology, "parallel");
    assert_eq!(row.outcome, "success");
    assert_eq!(row.total_subtasks, 2);
    assert_eq!(row.completed_subtasks, 2);
}

#[tokio::test]
async fn should_return_full_dispatch_detail_with_subtasks_validators_attribution() {
    let dir = TempDir::new().unwrap();
    let (state, ledger) = build_app_state_async(&dir).await;
    let app = build_router(state);

    let dispatch_id = "d3";
    let body = dispatch_body(dispatch_id, "contract-c");
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swarm/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // Write a real cost attribution to the PERSISTENT ledger so the
    // detail endpoint surfaces a live rollup — NOT a cached snapshot.
    ledger
        .record(
            CostAttributionEvent::new(
                "api:swarm-test",
                dispatch_id, // key matches the dashboard's per-dispatch query
                "task-1",
                "claude-haiku",
                1_000,
                500,
                0.001_5,
            )
            .with_workflow(Some("swarm".into()), Some("dispatch".into())),
        )
        .await
        .unwrap();

    let detail_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/swarm/dispatches/{dispatch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail_resp.status(), StatusCode::OK);
    let detail: SwarmDispatchDetail = read_json(detail_resp).await;
    assert_eq!(detail.dispatch_id, dispatch_id);
    assert_eq!(detail.total_subtasks, 2);
    assert_eq!(detail.completed_subtasks, 2);
    assert_eq!(detail.subtasks.len(), 2);
    assert_eq!(detail.subtasks[0].status, "completed");
    assert_eq!(detail.topology, "parallel");
    assert!(detail.finalized);
    assert_eq!(detail.cost_attributions.len(), 1);
    assert!((detail.total_cost_usd - 0.0015).abs() < 1e-9);
    // Validator evidence list exists (empty today — the dispatch
    // record does not persist per-round validator outcomes).
    assert!(detail.validator_evidence.is_empty());
}

#[tokio::test]
async fn should_serve_cost_attributions_via_persistent_ledger() {
    let dir = TempDir::new().unwrap();
    let (state, ledger) = build_app_state_async(&dir).await;
    let app = build_router(state);

    let dispatch_id = "d4";
    ledger
        .record(CostAttributionEvent::new(
            "api:swarm-test",
            dispatch_id,
            "task-x",
            "claude-haiku",
            750,
            125,
            0.000_8,
        ))
        .await
        .unwrap();
    ledger
        .record(CostAttributionEvent::new(
            "api:swarm-test",
            dispatch_id,
            "task-x",
            "claude-haiku",
            250,
            75,
            0.000_2,
        ))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/cost/attributions/{dispatch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let parsed: CostAttributionsResponse = read_json(resp).await;
    assert_eq!(parsed.dispatch_id, dispatch_id);
    assert_eq!(parsed.count, 2);
    assert_eq!(parsed.total_tokens_in, 1_000);
    assert_eq!(parsed.total_tokens_out, 200);
    assert!((parsed.total_cost_usd - 0.001).abs() < 1e-9);
    assert_eq!(parsed.attributions.len(), 2);
    assert!(
        parsed
            .attributions
            .iter()
            .all(|a| a.model == "claude-haiku")
    );
}

#[tokio::test]
async fn should_accept_review_decision_and_emit_typed_event() {
    let dir = TempDir::new().unwrap();
    let (state, _ledger) = build_app_state_async(&dir).await;
    // Subscribe to the SSE broadcaster BEFORE dispatching so the typed
    // event lands in the receiver's queue.
    let mut rx = state.broadcaster.subscribe();
    let app = build_router(state);

    let dispatch_id = "d5";
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swarm/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(dispatch_body(dispatch_id, "contract-e")))
                .unwrap(),
        )
        .await
        .unwrap();

    let review = json!({
        "schema_version": 1,
        "accepted": true,
        "reviewer": "ychen@futurewei.com",
        "notes": "LGTM — reviewed aggregate output",
    });
    let review_body = serde_json::to_string(&review).unwrap();
    let review_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/swarm/dispatches/{dispatch_id}/review"))
                .header("content-type", "application/json")
                .body(Body::from(review_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(review_resp.status(), StatusCode::OK);
    let parsed: SwarmReviewResponse = read_json(review_resp).await;
    assert!(parsed.accepted);
    assert_eq!(parsed.reviewer, "ychen@futurewei.com");

    // Confirm the typed event made it to the broadcaster. We expect a
    // JSON frame with `kind == "swarm_review_decision"` and the same
    // dispatch_id, proving the review was emitted as a typed event and
    // not stringly-typed JSON.
    let frame = rx
        .recv()
        .await
        .expect("expected a broadcast frame for the review event");
    let parsed: serde_json::Value = serde_json::from_str(&frame).expect("frame is JSON");
    assert_eq!(
        parsed.get("kind").and_then(|v| v.as_str()),
        Some("swarm_review_decision"),
        "expected typed event kind, got frame: {frame}"
    );
    assert_eq!(
        parsed.get("dispatch_id").and_then(|v| v.as_str()),
        Some(dispatch_id)
    );
    assert_eq!(parsed.get("accepted").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        parsed.get("reviewer").and_then(|v| v.as_str()),
        Some("ychen@futurewei.com")
    );

    // Follow-up: list the dispatch and verify review_accepted was
    // recorded on the index row so the dashboard's list view can
    // render the accepted marker without a detail fetch.
    let list_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/swarm/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list: SwarmDispatchesResponse = read_json(list_resp).await;
    let row = list
        .dispatches
        .iter()
        .find(|r| r.dispatch_id == dispatch_id)
        .expect("dispatch should be in the list");
    assert_eq!(row.review_accepted, Some(true));
}

#[tokio::test]
async fn should_reject_review_for_unknown_dispatch() {
    let dir = TempDir::new().unwrap();
    let (state, _ledger) = build_app_state_async(&dir).await;
    let app = build_router(state);

    let review = SwarmReviewRequest {
        schema_version: 1,
        accepted: false,
        reviewer: "ychen@futurewei.com".into(),
        notes: None,
    };
    let body = serde_json::to_string(&review).unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swarm/dispatches/does-not-exist/review")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
