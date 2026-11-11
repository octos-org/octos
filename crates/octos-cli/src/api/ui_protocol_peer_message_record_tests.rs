//! UPCR-2026-RF506E103: a delivered workspace peer message is recorded in the
//! recipient's history as a row attributed to the sending agent, announced by
//! `peer/team/message_received`, and shown only to connections that
//! negotiated `peer.team_message_record.v1`.
use super::*;

use octos_core::ui_protocol::{
    PeerMessageRecord, PeerTeamMessageReceivedEvent,
    UI_PROTOCOL_FEATURE_PEER_TEAM_MESSAGE_RECORD_V1 as RECORD_FEATURE,
    UI_PROTOCOL_STDIO_DEFAULT_FEATURES,
};
use postturn_context_batch::PromptAttribution;

const SENDER: &str = "workspace-1";

fn envelope(message: &str, assignment: bool) -> String {
    json!({
        "from_session": "team:local:tui#coding-a",
        "from_agent": SENDER,
        "to_session": "team:local:tui#coding-b",
        "workspace": "/repo",
        "coordinator": SENDER,
        "revision": 3,
        "assignment": assignment,
        "message": message,
    })
    .to_string()
}

fn record_features() -> ConnectionUiFeatures {
    ConnectionUiFeatures::from_requested_feature_tokens([RECORD_FEATURE], false)
}

/// A session store whose commits project through the real commit observer.
struct Store {
    dir: tempfile::TempDir,
    ledger: Arc<UiProtocolLedger>,
    /// Registration is weak: the store must keep the observer alive.
    _observer: octos_bus::MessageCommitObserver,
    sessions: Arc<TokioMutex<octos_bus::SessionManager>>,
    context: Arc<StdMutex<ContextManager>>,
    session: SessionKey,
}

impl Store {
    fn open(session: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey(session.into());
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let observer = message_commit_observer(ledger.clone());
        octos_bus::session::set_scoped_message_commit_observer(dir.path(), &observer);
        let sessions = Arc::new(TokioMutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ));
        let context = Arc::new(StdMutex::new(ContextManager::new(
            session.to_string(),
            None,
        )));
        Self {
            dir,
            ledger,
            _observer: observer,
            sessions,
            context,
            session,
        }
    }

    async fn history(&self) -> Vec<Message> {
        octos_bus::SessionManager::open(self.dir.path())
            .unwrap()
            .get_or_create(&self.session)
            .await
            .messages
            .clone()
    }

    fn projected(&self) -> Vec<UiNotification> {
        self.ledger
            .replay_after(
                &self.session,
                Some(&UiCursor {
                    stream: self.session.to_string(),
                    seq: 0,
                }),
            )
            .unwrap()
            .into_iter()
            .filter_map(|row| match row.event {
                UiProtocolLedgerEvent::Notification(notification) => Some(notification),
                UiProtocolLedgerEvent::Progress(_) => None,
            })
            .collect()
    }
}

fn records(projected: &[UiNotification]) -> Vec<&PeerTeamMessageReceivedEvent> {
    projected
        .iter()
        .filter_map(|notification| match notification {
            UiNotification::PeerTeamMessageReceived(event) => Some(event),
            _ => None,
        })
        .collect()
}

fn typed_user_echoes(projected: &[UiNotification]) -> usize {
    projected
        .iter()
        .filter(|notification| {
            matches!(notification, UiNotification::EnvelopeV2(envelope)
                if matches!(envelope.envelope.payload, PayloadV2::UserMessage { .. }))
        })
        .count()
}

#[tokio::test]
async fn should_record_attributed_row_and_announce_it_when_peer_message_prompt_commits() {
    let store = Store::open("local:record-commit");
    let body = "Check the parser tests.\nThen report {status}.";
    let envelope = envelope(body, true);
    let prompt = workspace_team::message_prompt(&envelope);
    let attribution = PromptAttribution::workspace_peer_message(&envelope);
    let thread = TurnId::new().0.to_string();

    let seq = postturn_context_batch::persist_turn_prompt(
        &store.sessions,
        &store.session,
        &thread,
        postturn_context_batch::prompt_row(&prompt, Some(&attribution)),
        store.dir.path(),
        &store.context,
        false,
    )
    .await
    .unwrap()
    .expect("a new dispatch appends its prompt");

    let history = store.history().await;
    assert_eq!(history.len(), 1);
    let row = &history[seq];
    assert_eq!(row.role, MessageRole::User);
    assert_eq!(row.source.as_deref(), Some("workspace_peer_message"));
    assert_eq!(row.participant.as_deref(), Some(SENDER));
    assert_eq!(
        row.content, prompt,
        "later turns replay exactly the attributed prompt the model was given"
    );
    assert_eq!(row.thread_id.as_deref(), Some(thread.as_str()));

    let projected = store.projected();
    assert_eq!(
        typed_user_echoes(&projected),
        0,
        "a peer's message is never projected as the user's own prompt"
    );
    let announced = records(&projected);
    assert_eq!(announced.len(), 1);
    let event = announced[0];
    assert_eq!(event.session_id, store.session);
    assert_eq!(event.thread_id, thread);
    assert_eq!(event.seq, seq as u64);
    assert_eq!(event.persisted_at, row.timestamp);
    assert_eq!(
        event.message_id,
        format!(
            "{}:{seq}:{}",
            store.session.0,
            row.timestamp.timestamp_nanos_opt().unwrap()
        ),
        "same identity session/hydrate gives the row"
    );
    assert_eq!(
        event.peer_message,
        PeerMessageRecord {
            from_agent: SENDER.into(),
            text: body.into(),
            assignment: true,
        }
    );
}

#[tokio::test]
async fn should_keep_record_attributed_when_interrupted_prompt_is_recovered() {
    let store = Store::open("local:record-recovery");
    let envelope = envelope("Rebase onto main first.", false);
    let prompt = workspace_team::message_prompt(&envelope);
    let attribution = PromptAttribution::workspace_peer_message(&envelope);
    let thread = TurnId::new().0.to_string();

    for _ in 0..2 {
        postturn_context_batch::persist_interrupted_turn_prompt(
            &store.sessions,
            &store.session,
            &thread,
            &prompt,
            Some(&attribution),
            store.dir.path(),
            &store.context,
        )
        .await;
    }

    let history = store.history().await;
    assert_eq!(history.len(), 1, "recovery never duplicates the record");
    assert_eq!(
        history[0].source.as_deref(),
        Some(workspace_team::MESSAGE_SOURCE)
    );
    assert_eq!(history[0].participant.as_deref(), Some(SENDER));
    let projected = store.projected();
    assert_eq!(typed_user_echoes(&projected), 0);
    assert_eq!(records(&projected).len(), 1);
}

#[tokio::test]
async fn should_project_typed_user_echo_when_prompt_carries_no_attribution() {
    let store = Store::open("local:record-plain");
    let thread = TurnId::new().0.to_string();
    postturn_context_batch::persist_turn_prompt(
        &store.sessions,
        &store.session,
        &thread,
        postturn_context_batch::prompt_row("Typed by the user", None),
        store.dir.path(),
        &store.context,
        false,
    )
    .await
    .unwrap();

    let history = store.history().await;
    assert!(history[0].source.is_none());
    assert!(history[0].participant.is_none());
    let projected = store.projected();
    assert_eq!(typed_user_echoes(&projected), 1);
    assert!(records(&projected).is_empty());
}

#[test]
fn should_deliver_record_notification_only_when_feature_negotiated() {
    let event = UiProtocolLedgerEvent::Notification(UiNotification::PeerTeamMessageReceived(
        PeerTeamMessageReceivedEvent {
            session_id: SessionKey("local:gate".into()),
            topic: None,
            thread_id: "thread".into(),
            message_id: "local:gate:0:1".into(),
            seq: 0,
            persisted_at: Utc::now(),
            peer_message: PeerMessageRecord {
                from_agent: SENDER.into(),
                text: "hello".into(),
                assignment: false,
            },
        },
    ));
    assert!(live_event_passes_capability_filter(
        &event,
        record_features()
    ));
    for unaware in [
        ConnectionUiFeatures::default(),
        ConnectionUiFeatures::stdio_defaults(),
        ConnectionUiFeatures::from_requested_feature_tokens(
            [UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2],
            false,
        ),
    ] {
        assert!(!live_event_passes_capability_filter(&event, unaware));
    }
}

#[test]
fn should_show_record_row_on_hydrate_only_when_feature_negotiated() {
    let envelope = envelope("hello", false);
    let record = PromptAttribution::workspace_peer_message(&envelope)
        .stamp(Message::user(workspace_team::message_prompt(&envelope)));
    let typed = Message::user("Typed by the user");
    let external = Message {
        source: Some("external_record:whiteboard".into()),
        ..Message::user("An appended record")
    };

    assert!(hydrate_shows_row(&record, record_features()));
    for unaware in [
        ConnectionUiFeatures::default(),
        ConnectionUiFeatures::stdio_defaults(),
    ] {
        assert!(!hydrate_shows_row(&record, unaware));
        assert!(hydrate_shows_row(&typed, unaware));
        assert!(
            hydrate_shows_row(&external, unaware),
            "only the peer message record is gated; other provenance is untouched"
        );
    }
}

#[test]
fn should_read_sender_and_body_back_when_prompt_wording_changes() {
    let body = "line one\nline two with {braces} and \"quotes\"";
    let envelope = envelope(body, true);
    let attribution = PromptAttribution::workspace_peer_message(&envelope);
    let expected = PeerMessageRecord {
        from_agent: SENDER.into(),
        text: body.into(),
        assignment: true,
    };

    let current = attribution.stamp(Message::user(workspace_team::message_prompt(&envelope)));
    assert_eq!(
        workspace_team::message_record(&current),
        Some(expected.clone())
    );

    // Only the last line is the contract: a row written under different
    // guidance wording still names its sender and body.
    let reworded = attribution.stamp(Message::user(format!(
        "[workspace-peer-message]\nGuidance from another release.\nMore guidance.\n{envelope}"
    )));
    assert_eq!(workspace_team::message_record(&reworded), Some(expected));

    // An unreadable envelope still shows who sent it and what the model saw.
    let damaged = attribution.stamp(Message::user("[workspace-peer-message]\nnot json"));
    assert_eq!(
        workspace_team::message_record(&damaged),
        Some(PeerMessageRecord {
            from_agent: SENDER.into(),
            text: "[workspace-peer-message]\nnot json".into(),
            assignment: false,
        })
    );

    assert_eq!(
        workspace_team::message_record(&Message::user(workspace_team::message_prompt(&envelope))),
        None,
        "a row is a record only by its source tag, never by its text"
    );
}

#[test]
fn should_drop_sender_label_when_envelope_names_an_unusable_one() {
    let named = |agent: &str| {
        workspace_team::envelope_participant(&json!({ "from_agent": agent }).to_string())
    };
    assert_eq!(named("workspace-12").as_deref(), Some("workspace-12"));
    assert_eq!(named("  workspace-2 ").as_deref(), Some("workspace-2"));
    for unusable in [
        "",
        "   ",
        "workspace-[2]",
        "workspace (2)",
        "two\nlines",
        &"w".repeat(octos_core::MESSAGE_PARTICIPANT_MAX_BYTES + 1),
    ] {
        assert_eq!(named(unusable), None, "{unusable:?}");
    }
    assert_eq!(workspace_team::envelope_participant("not json"), None);
    assert_eq!(workspace_team::envelope_participant("{}"), None);

    // The row is still recorded and still marked as a peer message.
    let attribution = PromptAttribution::workspace_peer_message("not json");
    assert_eq!(attribution.source, workspace_team::MESSAGE_SOURCE);
    assert_eq!(attribution.participant, None);
}

#[test]
fn should_advertise_record_feature_only_when_requested() {
    let advertises = |capabilities: UiProtocolCapabilities| {
        capabilities
            .supported_features
            .iter()
            .any(|feature| feature == RECORD_FEATURE)
    };
    assert!(advertises(record_features().negotiated_capabilities()));
    assert!(advertises(UiProtocolCapabilities::for_negotiated_features(
        [RECORD_FEATURE]
    )));
    assert!(!advertises(UiProtocolCapabilities::first_server_slice()));
    assert!(!advertises(
        ConnectionUiFeatures::default().negotiated_capabilities()
    ));
    assert!(!advertises(
        ConnectionUiFeatures::stdio_defaults().negotiated_capabilities()
    ));
    assert!(!UI_PROTOCOL_STDIO_DEFAULT_FEATURES.contains(&RECORD_FEATURE));
}

#[test]
fn should_round_trip_record_notification_when_sent_over_the_wire() {
    let method = octos_core::ui_protocol::methods::PEER_TEAM_MESSAGE_RECEIVED;
    assert_eq!(method, "peer/team/message_received");
    assert!(octos_core::ui_protocol::UI_PROTOCOL_NOTIFICATION_METHODS.contains(&method));

    let persisted_at = chrono::DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let notification = UiNotification::PeerTeamMessageReceived(PeerTeamMessageReceivedEvent {
        session_id: SessionKey("team:local:tui#coding-b".into()),
        topic: None,
        thread_id: "thread-1".into(),
        message_id: "team:local:tui#coding-b:4:1".into(),
        seq: 4,
        persisted_at,
        peer_message: PeerMessageRecord {
            from_agent: SENDER.into(),
            text: "hello".into(),
            assignment: false,
        },
    });
    let wire = notification.clone().into_rpc_notification().unwrap();
    assert_eq!(wire.method, method);
    assert_eq!(
        wire.params,
        json!({
            "session_id": "team:local:tui#coding-b",
            "topic": "coding-b",
            "thread_id": "thread-1",
            "message_id": "team:local:tui#coding-b:4:1",
            "seq": 4,
            "persisted_at": "2026-11-10T12:00:00Z",
            "peer_message": { "from_agent": SENDER, "text": "hello" },
        }),
        "`assignment` is omitted unless the coordinator admitted the message"
    );
    let UiNotification::PeerTeamMessageReceived(decoded) =
        UiNotification::from_rpc_notification(wire).unwrap()
    else {
        panic!("decodes as the record notification");
    };
    assert_eq!(decoded.topic.as_deref(), Some("coding-b"));
    assert_eq!(decoded.peer_message.text, "hello");

    let hydrated: HydratedMessage = serde_json::from_value(json!({
        "seq": 4,
        "role": "user",
        "content": "prompt",
        "persisted_at": "2026-11-10T12:00:00Z",
        "peer_message": { "from_agent": SENDER, "text": "hello", "assignment": true },
    }))
    .unwrap();
    assert!(hydrated.peer_message.as_ref().unwrap().assignment);
    let legacy_row: HydratedMessage = serde_json::from_value(json!({
        "seq": 0, "role": "user", "content": "typed", "persisted_at": "2026-11-10T12:00:00Z",
    }))
    .unwrap();
    assert!(legacy_row.peer_message.is_none());
    assert!(
        serde_json::to_value(&legacy_row)
            .unwrap()
            .get("peer_message")
            .is_none(),
        "ordinary rows keep their bytes"
    );
}
