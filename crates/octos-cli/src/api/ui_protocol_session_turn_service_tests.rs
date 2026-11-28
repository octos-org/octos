//! T-0636: `SessionTurnService` against the WebSocket handlers it was taken
//! out of. Each case builds the same fixture twice and sends the request
//! through the handler on one and straight to the service, with no socket, on
//! the other; the replies, the ledger rows and the stores must match.

use super::super::tests::test_support::*;
use super::*;

#[tokio::test]
async fn approval_respond_matches_the_handler_for_each_caller_and_a_late_repeat() {
    use crate::api::host_managed::{
        EXTERNAL_TURN_DENIED as NOT_OWN, HOST_OWNED_PEER_ANSWER_DENIED as PEER,
    };
    // (external, raised by its own turn, session topic, connection profile,
    // refusal kind). UPCR-2026-036: an external client answers only its own
    // turn's approval, never another turn's on the shared system
    // conversation, and never a host-owned app peer's (UPCR-2026-034).
    let cases = [
        (false, false, "", MAIN_PROFILE_ID, None),
        (true, true, "system", MAIN_PROFILE_ID, None),
        (true, false, "system", MAIN_PROFILE_ID, Some(NOT_OWN)),
        (true, true, "peer-rinx", MAIN_PROFILE_ID, Some(PEER)),
        (
            true,
            true,
            "peerctx-rinx.app-a",
            MAIN_PROFILE_ID,
            Some(PEER),
        ),
        (false, false, "", "alpha", Some("auth_scope_violation")),
    ];
    let turn = TurnId::new();
    for case @ (external, _, _, _, refusal) in cases {
        let ws = approval_once(case, true, &turn).await;
        assert_eq!(ws, approval_once(case, false, &turn).await, "{case:?}");
        match refusal {
            None => assert!(ws.answered(), "{case:?}: {ws:?}"),
            // The refusal names `kind`, as its kind or as a flag.
            Some(kind) => {
                let data = &ws.replies[0]["error"]["data"];
                assert!(data["kind"] == kind || data[kind] == true, "{ws:?}");
            }
        }
        // An answer reaches the turn and is one `approval/decided` row; a
        // refused approval stays parked.
        let answered = refusal.is_none();
        let decision = if answered { "Some(Approve)" } else { "None" };
        assert_eq!(ws.state["decision"], decision, "{case:?}");
        assert_eq!(ws.rows(), Some(usize::from(answered)));
        // An external answer never records a session-wide scope.
        assert!(!external || ws.state["scopes"] == 0, "{case:?}: {ws:?}");
        assert!(
            ws.replies[1].get("error").is_some(),
            "a repeat never answers: {ws:?}"
        );
    }
}

#[tokio::test]
async fn turn_interrupt_matches_the_handler_on_each_registry_state_and_a_late_repeat() {
    // (entry, external caller, first reply)
    let cases = [
        (Entry::Missing, false, "error"),
        (Entry::OtherTurn, false, "result"),
        (Entry::Running { own: true }, false, "result"),
        (Entry::Running { own: true }, true, "result"),
        (Entry::Running { own: false }, true, "error"),
        (Entry::Finished, false, "result"),
    ];
    let turn = TurnId::new();
    for case in cases {
        let ws = interrupt_once(case, true, &turn).await;
        assert_eq!(ws, interrupt_once(case, false, &turn).await, "{case:?}");
        assert!(ws.replies[0].get(case.2).is_some(), "{case:?}: {ws:?}");
    }
}

#[tokio::test]
async fn turn_start_matches_the_handler_on_admission_and_each_refusal() {
    // (prompt, profile runtime, another turn running, external, work-secret
    // grant, connection profile, admitted): admitted; no text; no runtime;
    // another profile; occupied, for an ordinary and an external caller; a
    // revoked grant.
    const REVOKED: Option<u64> = Some(u64::MAX);
    let cases = [
        ("hello", true, false, false, None, MAIN_PROFILE_ID, true),
        ("", true, false, false, None, MAIN_PROFILE_ID, false),
        ("hello", false, false, false, None, MAIN_PROFILE_ID, false),
        ("hello", true, false, false, None, "alpha", false),
        ("hello", true, true, false, None, MAIN_PROFILE_ID, false),
        ("hello", true, true, true, None, MAIN_PROFILE_ID, false),
        ("hello", true, false, false, REVOKED, MAIN_PROFILE_ID, false),
    ];
    for case in cases {
        let (ws, _) = start_once(case, true).await;
        let (service, ran) = start_once(case, false).await;
        assert_eq!(ws, service, "{case:?}");
        // An admitted turn runs once, through the caller's runner.
        assert_eq!(ran, Vec::from_iter(case.6.then(|| case.0.to_owned())));
        assert_eq!(ws.answered(), case.6, "{case:?}: {ws:?}");
    }
}

#[tokio::test]
async fn grant_read_error_fails_closed_for_ingress_but_not_browser_turns() {
    let (temp, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let (mut state, _profile, _cleanup) =
        state_with_profile_llm(temp.path(), MAIN_PROFILE_ID, Arc::new(NoopLlm)).await;
    // Make the stable lock parent unusable, so every admission snapshot fails
    // with the same storage error.
    let bad_store_dir = temp.path().join("grant-store-is-a-file");
    std::fs::write(&bad_store_dir, "not a directory").expect("bad store marker");
    let store = octos_agent::bridge::work_secret::WorkSecretGrantStore::new(&bad_store_dir);
    Arc::get_mut(&mut state)
        .expect("unique test state")
        .work_secret_store = Arc::new(store);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::default();
    let connection_turns: SharedConnectionTurns = Arc::default();

    for (ingress_generation, admitted) in [(None, true), (Some(0), false)] {
        let session = twin_session("grant-read-error");
        let caller = RecordingCaller::new(false, ingress_generation);
        let turn = TurnId::new();
        let start = SessionTurnService::new(&state, &ledger)
            .start_turn(
                &caller,
                &contracts,
                &active_turns,
                &connection_turns,
                Some(MAIN_PROFILE_ID),
                None,
                ConnectionUiFeatures::default(),
                turn_start_params(&session, &turn, "read with an unavailable grant store"),
                None,
            )
            .await;
        match (ingress_generation, start.outcome) {
            (None, Ok(turn)) => {
                turn.start();
                for _ in 0..200 {
                    if !caller.runs.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert_eq!(
                    caller.runs.lock().unwrap().as_slice(),
                    ["read with an unavailable grant store"],
                    "a browser turn keeps the existing non-ingress read fallback"
                );
            }
            (Some(_), Err(Refused::Error(error))) => {
                assert_eq!(
                    error.message, "Cannot verify the session's background-work generation",
                    "an ingress caller must not take the browser fallback"
                );
                assert!(caller.runs.lock().unwrap().is_empty());
            }
            (ingress_generation, outcome) => panic!(
                "unexpected outcome for ingress_generation={ingress_generation:?}: admitted={}",
                outcome.is_ok()
            ),
        }
        assert_eq!(
            active_turns.lock().await.contains_key(&session),
            admitted,
            "caller admission state does not match ingress_generation={ingress_generation:?}"
        );
        active_turns.lock().await.remove(&session);
    }
}

#[tokio::test]
async fn session_open_matches_the_handler_for_ordinary_ingress_and_foreign_scope() {
    // (connection profile, work-secret ingress, opened)
    for case in [
        (None, false, true),
        (None, true, true),
        (Some("alpha"), false, false),
    ] {
        let ws = open_once(case, true).await;
        assert_eq!(ws, open_once(case, false).await, "{case:?}");
        assert_eq!(ws.answered(), case.2, "{case:?}: {ws:?}");
        // An open is recorded as its `session/open` row.
        assert_eq!(ws.rows(), Some(usize::from(case.2)));
    }
}
