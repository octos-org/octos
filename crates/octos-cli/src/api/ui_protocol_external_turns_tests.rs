//! T-0637: external turns against a real serve runtime, its durable ledger
//! and existing chats, with a scripted model.
use super::super::tests::test_support::*;
use super::*;

/// A model that answers each prompt with `reply: <prompt>`; on `hold` it
/// waits for `release` first. It records the prompts it saw and the tools it
/// was offered.
pub(crate) struct ScriptedLlm {
    pub(crate) prompts: StdMutex<Vec<String>>,
    tools: StdMutex<Vec<String>>,
    pub(crate) release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for ScriptedLlm {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let prompt = messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.prompts.lock().unwrap().push(prompt.clone());
        (self.tools.lock().unwrap()).extend(tools.iter().map(|tool| tool.name.clone()));
        if prompt.contains("hold") {
            self.release.acquire().await.unwrap().forget();
        }
        Ok(stub_reply(Some(&format!("reply: {prompt}")), Vec::new()))
    }
    fn model_id(&self) -> &str {
        "external-turns-stub"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

pub(crate) fn scripted() -> Arc<ScriptedLlm> {
    Arc::new(ScriptedLlm {
        prompts: StdMutex::default(),
        tools: StdMutex::default(),
        release: tokio::sync::Semaphore::new(0),
    })
}

pub(crate) struct Fixture {
    pub(crate) state: Arc<AppState>,
    pub(crate) ledger: Arc<UiProtocolLedger>,
    pub(crate) contracts: Arc<UiProtocolContractStores>,
    pub(crate) entry: ExternalTurns,
    pub(crate) profile: BoundProfile,
}

/// A serve process over `dir`: a second one over the same directory is the
/// same server after a restart.
pub(crate) async fn serve(dir: &Path, llm: Arc<ScriptedLlm>) -> Fixture {
    let (state, _) = state_with_profile_llm(dir, CHAT_PROFILE, llm).await;
    let ledger = event_ledger(&state).await;
    let contracts = Arc::new(UiProtocolContractStores::default());
    let entry = ExternalTurns::new(state.clone(), ledger.clone(), contracts.clone());
    let profile = BoundProfile::verify(&state, CHAT_PROFILE).await.unwrap();
    Fixture {
        state,
        ledger,
        contracts,
        entry,
        profile,
    }
}

/// `serve` again over `dir`, as after a restart. A new process finds the
/// episode store closed. In this process the last serve's turn tasks can
/// still hold it for a moment after their turns ended, and nothing signals
/// when they let go, so wait for its lock first.
pub(crate) async fn restart(dir: &Path, llm: Arc<ScriptedLlm>) -> Fixture {
    let data_dir = dir.join("profiles").join(CHAT_PROFILE).join("data");
    for _ in 0..1000 {
        let released = octos_memory::EpisodeStore::open(&data_dir).await.is_ok();
        if released {
            return serve(dir, llm).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the last serve still holds {}", data_dir.display());
}

/// A chat id no other test uses: the turn registry is process-wide.
pub(crate) fn unique(name: &str) -> SessionKey {
    chat_key(&format!("{name}-{}", uuid::Uuid::new_v4().simple()))
}

impl Fixture {
    pub(crate) async fn chat(&self, key: &SessionKey) -> BoundChat {
        create_chat(&self.state, key.clone()).await.unwrap();
        BoundChat::verify(self.profile.clone(), key.clone()).unwrap()
    }

    async fn ended(&self, chat: &BoundChat, turn_id: &TurnId) -> TurnRecord {
        for _ in 0..2000 {
            match self.entry.record(chat, turn_id).await.unwrap() {
                TurnRecord::Running | TurnRecord::NotStarted => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                ended => return ended,
            }
        }
        panic!("turn {} did not end", turn_id.0);
    }

    /// The turn's durable rows: each method, and an envelope's payload type
    /// and text.
    fn rows(&self, chat: &BoundChat, turn_id: &TurnId) -> Vec<String> {
        let events = self
            .ledger
            .snapshot_events_for_turn(chat.session_id(), turn_id);
        let rows = events.unwrap().into_iter().map(|event| match event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope)) => {
                let payload = serde_json::to_value(&envelope.envelope.payload).unwrap();
                format!("{} {}", payload["type"], payload["data"]["text"])
            }
            other => ledger_event_method(&other).to_owned(),
        });
        rows.collect()
    }
}

#[tokio::test]
async fn only_an_existing_chat_of_a_served_profile_can_be_bound() {
    let dir = tempfile::tempdir().unwrap();
    let fx = serve(dir.path(), scripted()).await;
    let error = BoundProfile::verify(&fx.state, "nobody").await.unwrap_err();
    assert_eq!(error.data.unwrap()["kind"], "profile_unresolved");

    let own = unique("own");
    fx.chat(&own).await;
    let broken = unique("broken");
    let folder = fx.chat(&broken).await;
    std::fs::remove_dir_all(
        chat_workspace::lookup(&fx.profile.data_dir, folder.session_id())
            .unwrap()
            .unwrap()
            .root,
    )
    .unwrap();
    let code = record_session(&fx.state, unique("code")).await;
    let other_profile =
        SessionKey::with_profile(MAIN_PROFILE_ID, "api", own.0.rsplit(':').next().unwrap());
    let refused = [
        (other_profile, "not named as a session of the bound profile"),
        (
            SessionKey("web-1".into()),
            "not named as a session of the bound profile",
        ),
        (SessionKey(format!("{}#notes", own.0)), "it has a topic"),
        (code, "it is not a chat"),
        (unique("never-made"), "it is not a chat"),
        (broken, "its chat folder is unusable"),
    ];
    for (session, why) in refused {
        let error = BoundChat::verify(fx.profile.clone(), session.clone()).unwrap_err();
        assert!(
            error.message.contains(why),
            "{}: {}",
            session.0,
            error.message
        );
    }
    let bound = BoundChat::verify(fx.profile.clone(), own.clone()).unwrap();
    // A chat deleted after it was bound runs nothing: its id cannot come
    // back as an ordinary session.
    assert!(delete_session_as_admin(&fx.state, &own).await.is_success());
    let turn = TurnId::new();
    assert!(fx.entry.start(&bound, turn.clone(), "hello").await.is_err());
    assert_eq!(
        fx.entry.record(&bound, &turn).await.unwrap(),
        TurnRecord::NotStarted
    );
    // A refused start leaves nothing to reconcile.
    assert!(fx.entry.reconcile(&fx.profile).await.is_empty());
}

#[tokio::test]
async fn a_turn_on_the_ledger_connection_writes_what_an_external_socket_turn_writes() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let (by_socket, by_entry) = (
        fx.chat(&unique("socket")).await,
        fx.chat(&unique("entry")).await,
    );
    let (socket_turn, entry_turn) = (TurnId::new(), TurnId::new());

    let (ws, _frames) = ws_connection_for_test(1024);
    ws.set_external(true);
    handle_turn_start(
        &ws,
        &fx.state,
        &fx.ledger,
        &fx.contracts,
        &active_turns_registry(),
        &Arc::default(),
        Some(CHAT_PROFILE),
        None,
        ConnectionUiFeatures::default(),
        "start".into(),
        turn_start_params(by_socket.session_id(), &socket_turn, "hello"),
    )
    .await;
    let started = fx.entry.start(&by_entry, entry_turn.clone(), "hello").await;
    assert_eq!(started.unwrap(), Started::Now);

    assert_eq!(
        fx.ended(&by_socket, &socket_turn).await,
        TurnRecord::Completed
    );
    assert_eq!(
        fx.ended(&by_entry, &entry_turn).await,
        TurnRecord::Completed
    );
    let rows = fx.rows(&by_entry, &entry_turn);
    assert_eq!(rows, fx.rows(&by_socket, &socket_turn));
    assert!(
        rows.contains(&r#""assistant_persisted" "reply: hello""#.to_owned()),
        "{rows:?}"
    );
    // Both turns are external: no tool outside the external allowlist.
    let offered = llm.tools.lock().unwrap().clone();
    let allowed = |tool: &String| {
        crate::api::host_managed::external_turn_tool_allowed(tool)
            || ["run_tool", "discover_tools"].contains(&tool.as_str())
    };
    assert!(
        !offered.is_empty() && offered.iter().all(allowed),
        "{offered:?}"
    );
}

#[tokio::test]
async fn a_turn_id_runs_once_and_only_in_its_own_chat() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let (chat, other) = (
        fx.chat(&unique("once")).await,
        fx.chat(&unique("other")).await,
    );
    let turn = TurnId::new();
    let start = || fx.entry.start(&chat, turn.clone(), "hold once");
    assert_eq!(start().await.unwrap(), Started::Now);
    assert_eq!(
        start().await.unwrap(),
        Started::Already(TurnRecord::Running)
    );
    llm.release.add_permits(1);
    assert_eq!(fx.ended(&chat, &turn).await, TurnRecord::Completed);
    assert_eq!(
        start().await.unwrap(),
        Started::Already(TurnRecord::Completed)
    );
    assert_eq!(*llm.prompts.lock().unwrap(), ["hold once"]);
    // The same id is no turn of another chat.
    assert_eq!(
        fx.entry.record(&other, &turn).await.unwrap(),
        TurnRecord::NotStarted
    );
}

#[tokio::test]
async fn after_a_restart_each_recorded_turn_is_settled_by_its_exact_id() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let (key, unreadable) = (unique("restart"), unique("unreadable"));
    let [finished, unstarted, interrupted, other] = [(); 4].map(|_| TurnId::new());
    // The ledger's session folders: a new one per session that wrote.
    let ledger_root = dir.path().join("ui-protocol");
    let folders = || {
        let entries = std::fs::read_dir(&ledger_root).unwrap();
        entries
            .map(|entry| entry.unwrap().path())
            .collect::<HashSet<_>>()
    };
    let unreadable_folder;
    {
        let fx = serve(dir.path(), llm.clone()).await;
        let chat = fx.chat(&key).await;
        fx.entry
            .start(&chat, finished.clone(), "before")
            .await
            .unwrap();
        assert_eq!(fx.ended(&chat, &finished).await, TurnRecord::Completed);
        // Recorded, then the server stopped before admitting it.
        record_dispatch(&fx.profile.data_dir, &key, &unstarted).unwrap();
        // Recorded and started, then the server stopped mid-turn.
        record_dispatch(&fx.profile.data_dir, &key, &interrupted).unwrap();
        let started = octos_core::ui_protocol::TurnStartedEvent {
            session_id: key.clone(),
            turn_id: interrupted.clone(),
            timestamp: chrono::Utc::now(),
            topic: None,
        };
        fx.ledger
            .try_append_notification(UiNotification::TurnStarted(started))
            .unwrap();
        // Ended in another chat, whose ledger cannot be read after the restart.
        let before = folders();
        let chat = fx.chat(&unreadable).await;
        fx.entry.start(&chat, other.clone(), "other").await.unwrap();
        assert_eq!(fx.ended(&chat, &other).await, TurnRecord::Completed);
        unreadable_folder = folders().difference(&before).next().unwrap().clone();
    }
    let files: Vec<PathBuf> = std::fs::read_dir(&unreadable_folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    for file in &files {
        std::fs::rename(file, file.with_extension("held")).unwrap();
        std::fs::create_dir(file).unwrap();
    }

    let fx = restart(dir.path(), llm.clone()).await;
    let reconciled = async || {
        let mut turns: Vec<_> = (fx.entry.reconcile(&fx.profile).await.into_iter())
            .map(|(_, turn, record)| (turn, record.ok()))
            .collect();
        turns.sort_by_key(|(turn, _)| turn.0);
        turns
    };
    let mut expected = vec![
        (finished.clone(), Some(TurnRecord::Completed)),
        (unstarted.clone(), Some(TurnRecord::NotStarted)),
        (interrupted.clone(), Some(TurnRecord::Orphaned)),
        (other.clone(), None),
    ];
    expected.sort_by_key(|(turn, _)| turn.0);
    // One unreadable session costs no other result, and forgets nothing.
    assert_eq!(reconciled().await, expected);
    for file in &files {
        std::fs::remove_dir(file).unwrap();
        std::fs::rename(file.with_extension("held"), file).unwrap();
    }
    for (_, record) in &mut expected {
        record.get_or_insert(TurnRecord::Completed);
    }
    // Repaired, it reports its turn; the others are still there.
    assert_eq!(reconciled().await, expected);
    for (turn, session) in [
        (&finished, &key),
        (&interrupted, &key),
        (&other, &unreadable),
    ] {
        let acknowledged = fx.entry.acknowledge(&fx.profile, session, turn).await;
        acknowledged.unwrap();
    }
    assert!(
        fx.entry
            .acknowledge(&fx.profile, &key, &unstarted)
            .await
            .is_err()
    );
    assert_eq!(
        reconciled().await,
        [(unstarted.clone(), Some(TurnRecord::NotStarted))]
    );
    let chat = BoundChat::verify(fx.profile.clone(), key.clone()).unwrap();
    let start =
        async |turn: &TurnId, text| fx.entry.start(&chat, turn.clone(), text).await.unwrap();
    assert_eq!(start(&unstarted, "after").await, Started::Now);
    assert_eq!(fx.ended(&chat, &unstarted).await, TurnRecord::Completed);
    assert_eq!(
        start(&interrupted, "again").await,
        Started::Already(TurnRecord::Orphaned)
    );
    assert_eq!(*llm.prompts.lock().unwrap(), ["before", "other", "after"]);
}

#[tokio::test]
async fn the_entry_answers_and_stops_only_its_own_turns_and_records_no_scope() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = fx.chat(&unique("own")).await;
    let session = chat.session_id().clone();

    let (ws, _frames) = ws_connection_for_test(1024);
    let raise =
        |connection| g1_raise_approval(connection, &fx.ledger, &fx.contracts, &fx.state, &session);
    let (theirs_task, theirs) = raise(&ws).await;
    let (ours_task, ours) = g1_raise_approval(
        &fx.entry.connection,
        &fx.ledger,
        &fx.contracts,
        &fx.state,
        &session,
    )
    .await;
    let approve = |approval_id| octos_core::ui_protocol::ApprovalRespondParams {
        session_id: session.clone(),
        approval_id,
        decision: ApprovalDecision::Approve,
        approval_scope: Some("approve_for_session".into()),
        client_note: None,
    };
    assert!(
        fx.entry
            .respond_approval(&chat, approve(theirs))
            .await
            .is_err()
    );
    fx.entry
        .respond_approval(&chat, approve(ours))
        .await
        .unwrap();
    assert_eq!(ours_task.await.unwrap(), ToolApprovalDecision::Approve);
    assert!(
        fx.contracts
            .scopes
            .lookup(&session, "shell", &TurnId::new())
            .is_none()
    );
    theirs_task.abort();

    // A turn the socket started is not this entry's to stop.
    let socket_turn = TurnId::new();
    handle_turn_start(
        &ws,
        &fx.state,
        &fx.ledger,
        &fx.contracts,
        &active_turns_registry(),
        &Arc::default(),
        Some(CHAT_PROFILE),
        None,
        ConnectionUiFeatures::default(),
        "start".into(),
        turn_start_params(&session, &socket_turn, "hold theirs"),
    )
    .await;
    assert!(
        fx.entry
            .interrupt(&chat, socket_turn.clone())
            .await
            .is_err()
    );
    llm.release.add_permits(1);
    assert_eq!(fx.ended(&chat, &socket_turn).await, TurnRecord::Completed);

    let own_turn = TurnId::new();
    fx.entry
        .start(&chat, own_turn.clone(), "hold ours")
        .await
        .unwrap();
    fx.entry.interrupt(&chat, own_turn.clone()).await.unwrap();
    assert_eq!(fx.ended(&chat, &own_turn).await, TurnRecord::Interrupted);
}
