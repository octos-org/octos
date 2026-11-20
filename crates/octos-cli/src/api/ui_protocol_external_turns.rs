//! External turns (T-0637, `docs/IM_REMOTE_CONTROL.md` §14): the entry an
//! ingress without a socket starts, stops and answers turns through. The IM
//! bridge (T-0653) is its first user; nothing in `octos serve` calls it yet.
//!
//! Every call names a [`BoundChat`]: a profile this server serves and an
//! existing chat of that profile, both checked when the binding is made.
//! Nothing here takes an optional profile, falls back to `_main`, searches
//! other profiles, or opens a session other than the one named. A Code
//! session cannot be bound.
//!
//! The caller chooses each turn's id. [`ExternalTurns::start`] records it on
//! disk before the turn runs, and answers a second start of the same id with
//! what became of the first instead of running it again. After a restart,
//! [`ExternalTurns::reconcile`] reports each recorded turn by its exact id:
//! never started, running, ended, or orphaned by the restart. A record stays
//! until the caller acknowledges the ended turn. The server neither picks nor
//! deduplicates turn ids, so nothing is matched by text or time.
//!
//! Turns run on a [`LedgerConnection`] and are external (UPCR-2026-036):
//! they keep the external tool allowlist, a session-wide approval granted
//! elsewhere does not approve for them, and this entry can interrupt and
//! answer only its own turns and approvals, never recording a scope.
#![cfg_attr(not(test), allow(dead_code))]

use super::session_turn_service::{
    Refused, SessionTurnService, TurnCaller, TurnRun, TurnRunner, run_admitted_turn,
};
use super::turn_connection::LedgerConnection;
use super::*;
use crate::runtime::chat_workspace;
use sha2::Digest;

/// Where a profile keeps the ids of the external turns it was asked to run.
const JOURNAL_DIR: &str = "external_turns";

/// A profile this server serves, with the data directory its chats live in.
#[derive(Debug, Clone)]
pub(crate) struct BoundProfile {
    id: String,
    data_dir: PathBuf,
}

impl BoundProfile {
    pub(crate) async fn verify(state: &AppState, profile_id: &str) -> Result<Self, RpcError> {
        ensure_known_profile(state, profile_id)?;
        let runtime = ensure_session_profile_runtime(state, Some(profile_id))
            .await?
            .filter(|runtime| runtime.profile_id == profile_id)
            .ok_or_else(|| {
                runtime_unavailable_error(profile_runtime_unavailable_message(state, profile_id))
            })?;
        Ok(Self {
            id: profile_id.to_owned(),
            data_dir: runtime.data_dir.clone(),
        })
    }
}

/// An existing chat of a [`BoundProfile`], named by its own session id.
#[derive(Debug, Clone)]
pub(crate) struct BoundChat {
    profile: BoundProfile,
    session_id: SessionKey,
}

impl BoundChat {
    pub(crate) fn verify(profile: BoundProfile, session_id: SessionKey) -> Result<Self, RpcError> {
        let refuse = |why: &str| {
            Err(
                RpcError::invalid_params(format!("{} cannot be bound: {why}", session_id.0))
                    .with_data(json!({ "kind": "external_turn_session_invalid" })),
            )
        };
        if session_id.profile_id() != Some(profile.id.as_str()) {
            return refuse("it is not named as a session of the bound profile");
        }
        if session_id.topic().is_some() {
            return refuse("it has a topic");
        }
        match chat_workspace::lookup(&profile.data_dir, &session_id) {
            Ok(Some(chat)) if chat.profile_id == profile.id => Ok(Self {
                profile,
                session_id,
            }),
            Ok(Some(_)) => refuse("its chat belongs to another profile"),
            Ok(None) => refuse("it is not a chat; Code sessions cannot be bound"),
            Err(_) => refuse("its chat folder is unusable"),
        }
    }

    pub(crate) fn session_id(&self) -> &SessionKey {
        &self.session_id
    }
}

/// What became of a turn, by its exact id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnRecord {
    /// Not being admitted, not running, nothing in the ledger: it never ran.
    NotStarted,
    /// Being admitted or running.
    Running,
    Completed,
    Interrupted,
    Failed,
    /// It started and the server stopped before it ended. Part of its work
    /// may be done, so it is not run again.
    Orphaned,
}

/// The answer to [`ExternalTurns::start`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Started {
    /// The turn was admitted and runs now.
    Now,
    /// A turn with this id already exists; nothing new runs.
    Already(TurnRecord),
}

/// The ledger's account of `turn_id` on `session_id`.
fn ledger_record(
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    turn_id: &TurnId,
) -> Result<TurnRecord, RpcError> {
    let mut record = TurnRecord::NotStarted;
    for event in ledger.snapshot_events_for_turn(session_id, turn_id)? {
        let UiProtocolLedgerEvent::Notification(notification) = event.event else {
            continue;
        };
        record = match notification {
            UiNotification::TurnStarted(started)
                if started.turn_id == *turn_id && record == TurnRecord::NotStarted =>
            {
                TurnRecord::Running
            }
            UiNotification::TurnCompleted(completed) if completed.turn_id == *turn_id => {
                TurnRecord::Completed
            }
            UiNotification::TurnError(error) if error.turn_id == *turn_id => {
                match error.code.as_str() {
                    "interrupted" => TurnRecord::Interrupted,
                    "orphaned_by_restart" => TurnRecord::Orphaned,
                    _ => TurnRecord::Failed,
                }
            }
            _ => continue,
        };
    }
    Ok(record)
}

pub(crate) struct ExternalTurns {
    state: Arc<AppState>,
    ledger: Arc<UiProtocolLedger>,
    contracts: Arc<UiProtocolContractStores>,
    connection: Arc<LedgerConnection>,
    connection_turns: SharedConnectionTurns,
    /// Held from a start's check to its admission, so two starts of one id
    /// cannot both find it unstarted.
    starting: TokioMutex<()>,
}

impl ExternalTurns {
    pub(crate) fn new(
        state: Arc<AppState>,
        ledger: Arc<UiProtocolLedger>,
        contracts: Arc<UiProtocolContractStores>,
    ) -> Self {
        Self {
            state,
            ledger,
            contracts,
            connection: Arc::new(LedgerConnection::new()),
            connection_turns: Arc::default(),
            starting: TokioMutex::new(()),
        }
    }

    /// What became of `turn_id` in `chat`.
    pub(crate) async fn record(
        &self,
        chat: &BoundChat,
        turn_id: &TurnId,
    ) -> Result<TurnRecord, RpcError> {
        let session_id = &chat.session_id;
        if TurnAdmission::in_progress(session_id, turn_id) {
            return Ok(TurnRecord::Running);
        }
        let registered = {
            let registry = active_turns_registry();
            let registry = registry.lock().await;
            match registry.get(session_id) {
                Some(active) if active.turn_id == *turn_id => {
                    Some(turn_state_to_lifecycle(&*active.state.lock().await))
                }
                _ => None,
            }
        };
        let ledger = ledger_record(&self.ledger, session_id, turn_id)?;
        Ok(match registered {
            Some(TurnLifecycleState::Active | TurnLifecycleState::Interrupting) => {
                TurnRecord::Running
            }
            // The registry may mark a turn ended before the ledger has its end.
            Some(TurnLifecycleState::Completed) if ledger == TurnRecord::Running => {
                TurnRecord::Completed
            }
            Some(TurnLifecycleState::Interrupted) if ledger == TurnRecord::Running => {
                TurnRecord::Interrupted
            }
            Some(TurnLifecycleState::Errored) if ledger == TurnRecord::Running => {
                TurnRecord::Failed
            }
            // A turn the ledger saw start before a restart is closed with
            // `orphaned_by_restart` when the ledger loads it again.
            _ => ledger,
        })
    }

    /// Run `text` as turn `turn_id` in `chat`, unless a turn with that id
    /// already exists. The id is on disk before the turn can run.
    pub(crate) async fn start(
        &self,
        chat: &BoundChat,
        turn_id: TurnId,
        text: &str,
    ) -> Result<Started, RpcError> {
        let _starting = self.starting.lock().await;
        match self.record(chat, &turn_id).await? {
            TurnRecord::NotStarted => {}
            existing => return Ok(Started::Already(existing)),
        }
        record_dispatch(&chat.profile.data_dir, &chat.session_id, &turn_id).map_err(|error| {
            RpcError::internal_error(format!("could not record the turn before it runs: {error}"))
        })?;
        let params = TurnStartParams {
            session_id: chat.session_id.clone(),
            turn_id,
            input: vec![InputItem::Text { text: text.into() }],
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
            reasoning_effort: None,
            tool_context: None,
            live_video: false,
            origin: None,
        };
        let profile = Some(chat.profile.id.as_str());
        let start = SessionTurnService::new(&self.state, &self.ledger)
            .start_turn(
                &self.caller(),
                &self.contracts,
                &active_turns_registry(),
                &self.connection_turns,
                profile,
                profile,
                ConnectionUiFeatures::default(),
                params,
                None,
            )
            .await;
        match start.outcome {
            Ok(admitted) => {
                admitted.start();
                Ok(Started::Now)
            }
            Err(Refused::Scope(error) | Refused::Error(error)) => Err(error),
        }
    }

    /// The turns `profile` recorded, each with what became of it or why that
    /// cannot be told now. Nothing is forgotten here: the caller
    /// [acknowledges](Self::acknowledge) each ended turn once it has
    /// delivered the result, so a failed read or delivery loses none.
    pub(crate) async fn reconcile(
        &self,
        profile: &BoundProfile,
    ) -> Vec<(SessionKey, TurnId, Result<TurnRecord, RpcError>)> {
        let mut turns = Vec::new();
        for (session_id, turn_id) in recorded_dispatches(&profile.data_dir) {
            let chat = BoundChat {
                profile: profile.clone(),
                session_id,
            };
            let record = self.record(&chat, &turn_id).await;
            turns.push((chat.session_id, turn_id, record));
        }
        turns
    }

    /// Forget a recorded turn whose result the caller has delivered. Only a
    /// turn that has ended can be forgotten.
    pub(crate) async fn acknowledge(
        &self,
        profile: &BoundProfile,
        session_id: &SessionKey,
        turn_id: &TurnId,
    ) -> Result<(), RpcError> {
        let chat = BoundChat {
            profile: profile.clone(),
            session_id: session_id.clone(),
        };
        if let TurnRecord::NotStarted | TurnRecord::Running = self.record(&chat, turn_id).await? {
            return Err(RpcError::invalid_params(format!(
                "turn {} has not ended",
                turn_id.0
            )));
        }
        match std::fs::remove_file(dispatch_path(&profile.data_dir, session_id, turn_id)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(
                RpcError::internal_error(format!("could not forget the turn: {error}")),
            ),
            _ => Ok(()),
        }
    }

    /// Interrupt `turn_id` in `chat`, if this entry started it.
    pub(crate) async fn interrupt(
        &self,
        chat: &BoundChat,
        turn_id: TurnId,
    ) -> Result<TurnInterruptResult, RpcError> {
        let params = TurnInterruptParams {
            session_id: chat.session_id.clone(),
            turn_id,
        };
        SessionTurnService::new(&self.state, &self.ledger)
            .interrupt(
                &self.caller(),
                &active_turns_registry(),
                Some(chat.profile.id.as_str()),
                params,
            )
            .await
    }

    /// Answer an approval of one of this entry's turns in `chat`. The answer
    /// covers this request only; a scope it asks for is dropped.
    pub(crate) async fn respond_approval(
        &self,
        chat: &BoundChat,
        params: octos_core::ui_protocol::ApprovalRespondParams,
    ) -> Result<Value, RpcError> {
        if params.session_id != chat.session_id {
            return Err(RpcError::invalid_params(
                "the approval is not in the bound chat",
            ));
        }
        let caller = self.caller();
        let responded = SessionTurnService::new(&self.state, &self.ledger)
            .respond_approval(
                &caller,
                &self.contracts,
                Some(chat.profile.id.as_str()),
                Some(caller.connection_id()),
                params,
            )
            .await
            .map_err(|(Refused::Scope(error) | Refused::Error(error))| error)?;
        responded.audit(&self.state, &self.contracts).await;
        Ok(responded.result)
    }

    fn caller(&self) -> ExternalCaller {
        ExternalCaller(self.connection.clone())
    }
}

/// Record `turn_id` of `session_id` before it runs: written whole, synced,
/// then renamed into place.
fn record_dispatch(
    data_dir: &Path,
    session_id: &SessionKey,
    turn_id: &TurnId,
) -> std::io::Result<()> {
    use std::io::Write;
    let dir = data_dir.join(JOURNAL_DIR);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir)?;
    let path = dispatch_path(data_dir, session_id, turn_id);
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let record = serde_json::to_vec(&json!({ "session_id": session_id, "turn_id": turn_id }))?;
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&record)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Where `turn_id` of `session_id` is recorded. A turn id names a turn within
/// its session only.
fn dispatch_path(data_dir: &Path, session_id: &SessionKey, turn_id: &TurnId) -> PathBuf {
    let session = format!("{:x}", sha2::Sha256::digest(session_id.0.as_bytes()));
    data_dir
        .join(JOURNAL_DIR)
        .join(format!("{}-{}.json", &session[..16], turn_id.0))
}

/// Every turn recorded under `data_dir`. A record that cannot be read is
/// skipped.
fn recorded_dispatches(data_dir: &Path) -> Vec<(SessionKey, TurnId)> {
    #[derive(Deserialize)]
    struct Dispatch {
        session_id: SessionKey,
        turn_id: TurnId,
    }
    let Ok(entries) = std::fs::read_dir(data_dir.join(JOURNAL_DIR)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let dispatch: Dispatch = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some((dispatch.session_id, dispatch.turn_id))
        })
        .collect()
}

/// The caller of this entry's turns: external, on its ledger connection.
struct ExternalCaller(Arc<LedgerConnection>);

impl TurnCaller for ExternalCaller {
    type Runner = ExternalRunner;

    fn connection_id(&self) -> ConnectionId {
        self.0.connection_id()
    }

    fn is_external(&self) -> bool {
        self.0.is_external()
    }

    fn ingress_generation(&self) -> Option<u64> {
        None
    }

    fn emit_durable(
        &self,
        ledger: &UiProtocolLedger,
        notification: UiNotification,
    ) -> Result<(), SendError> {
        send_notification_durable(&*self.0, ledger, notification)
    }

    fn turn_runner(&self) -> ExternalRunner {
        ExternalRunner(self.0.clone())
    }
}

/// Runs this entry's turns on its ledger connection.
struct ExternalRunner(Arc<LedgerConnection>);

impl TurnRunner for ExternalRunner {
    async fn run(self, run: TurnRun) {
        run_admitted_turn(self.0, run).await;
    }
}

#[cfg(test)]
#[path = "ui_protocol_external_turns_tests.rs"]
mod tests;
