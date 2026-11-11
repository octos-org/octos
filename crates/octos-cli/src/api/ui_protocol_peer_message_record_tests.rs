//! UPCR-2026-RF506E103: a delivered workspace peer message is recorded in the
//! recipient's history as a row attributed to the sending agent. The delivery
//! path, the capability gate and hydrate are covered against a real server in
//! `tests/serve_workspace_team.rs`; these are the cases that test cannot reach.
use super::*;

use octos_core::ui_protocol::{PeerMessageRecord, PeerTeamMessageReceivedEvent};
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
