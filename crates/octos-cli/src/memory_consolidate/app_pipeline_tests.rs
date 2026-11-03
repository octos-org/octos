//! Model-independent checks for the app-record -> staging -> Knowledge path.
//! The scripted provider verifies orchestration, not real-model extraction quality.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_core::Message;
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::{MemoryStore, RecallConfig, RecallStore, Record, RecordKind, StagingNote};

use super::{ConsolidateParams, run_consolidation, staging};

struct ScriptedMerge {
    replies: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedMerge {
    fn new(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

#[async_trait]
impl LlmProvider for ScriptedMerge {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.prompts.lock().unwrap().push(messages.to_vec());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| eyre::eyre!("unexpected provider call"))?;
        Ok(ChatResponse {
            content: Some(reply),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "scripted-app-pipeline"
    }
    fn provider_name(&self) -> &str {
        "test"
    }
}

struct AppStore {
    _dir: tempfile::TempDir,
    bank: MemoryStore,
    recall: Arc<RecallStore>,
    params: ConsolidateParams,
}

impl AppStore {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bank = MemoryStore::open(dir.path()).await.unwrap();
        let recall = Arc::new(
            RecallStore::open(
                dir.path(),
                RecallConfig {
                    dimension: 4,
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let params = ConsolidateParams::new(dir.path().join("memory"));
        Self {
            _dir: dir,
            bank,
            recall,
            params,
        }
    }

    fn ingest(&self, id: &str, abstract_text: &str, body: &str, version: &str) {
        let mut record = Record::new(
            id,
            RecordKind::Document,
            "reader",
            chrono::Utc::now(),
            "Reading preferences",
            abstract_text,
        );
        record.body = Some(body.to_string());
        record.fingerprint = version.to_string();
        // The same storage operation used after memory/ingest validation;
        // transport authorization and embedding are deliberately out of scope.
        self.recall.upsert(vec![record], vec![None]).unwrap();
    }

    async fn nominate(&self, min_visits: u32) -> Vec<Record> {
        octos_agent::memory_index::nominate_for_promotion(
            &self.bank,
            &self.recall,
            min_visits,
            20,
            None,
        )
        .await
        .unwrap()
    }

    fn notes(&self) -> Vec<staging::NoteFile> {
        staging::load_staging(&self.params.memory_dir)
            .unwrap()
            .notes
    }

    async fn seed_memory(&self) {
        let date = self.params.today;
        std::fs::write(self.params.memory_dir.join("MEMORY.md"), format!(
            "Prefers long chapters. (updated: {date}) ^maaaaaa\n\nLikes nature books. (updated: {date}) ^mbbbbbb\n"
        )).unwrap();
    }
}

#[tokio::test]
async fn should_require_actual_loads_before_nominating_app_documents() {
    let store = AppStore::new().await;
    store.ingest(
        "doc:reader:1",
        "Prefers short chapters.",
        "Full document.",
        "v1",
    );
    assert!(
        store.nominate(0).await.is_empty(),
        "zero threshold still requires one real load"
    );
    assert!(store.notes().is_empty());
    assert!(store.bank.read_long_term().await.unwrap().is_empty());
    store.recall.touch("doc:reader:1").unwrap();
    assert!(store.nominate(2).await.is_empty());
    store.recall.touch("doc:reader:1").unwrap();
    assert_eq!(store.nominate(2).await.len(), 1);
}

#[tokio::test]
async fn should_consolidate_promoted_app_records_without_an_agent_turn() {
    let store = AppStore::new().await;
    store.ingest(
        "doc:reader:1",
        "Prefers short chapters.",
        "Full document.",
        "v1",
    );
    store.recall.touch("doc:reader:1").unwrap();
    store.recall.touch("doc:reader:1").unwrap();
    assert_eq!(store.nominate(2).await.len(), 1);
    let notes = store.notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].content.contains("provenance: doc:reader:1"));
    assert_eq!(notes[0].origin, staging::NoteOrigin::Host);
    assert_eq!(notes[0].kind, staging::NoteKind::Fact);
    assert!(
        store.bank.read_long_term().await.unwrap().is_empty(),
        "staging is not Knowledge"
    );
    let reply = serde_json::json!({
        "ops": [{"op":"add", "section":null, "text":"Prefers short chapters.", "sources":[notes[0].id]}],
        "consumed_ids":[notes[0].id], "dropped":[]
    }).to_string();
    let provider = ScriptedMerge::new(vec![reply]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.merge_applied, "{:?}", outcome.errors);
    assert_eq!(provider.calls(), 1);
    assert!(
        store
            .bank
            .read_long_term()
            .await
            .unwrap()
            .contains("Prefers short chapters.")
    );
    assert!(store.notes().is_empty());
    assert!(
        store.nominate(1).await.is_empty(),
        "same record is not nominated twice"
    );
}

#[tokio::test]
async fn should_use_only_the_short_abstract_when_nominating_an_app_record() {
    let store = AppStore::new().await;
    let important_detail = "User explicitly asks for diagrams before any formula.";
    store.ingest(
        "doc:reader:1",
        "Completed a reading session.",
        important_detail,
        "v1",
    );
    store.recall.touch("doc:reader:1").unwrap();
    store.nominate(1).await;
    let notes = store.notes();
    assert!(notes[0].content.contains("Completed a reading session."));
    assert!(
        !notes[0].content.contains(important_detail),
        "body is omitted by existing promotion"
    );
    assert!(
        store
            .recall
            .get("doc:reader:1")
            .unwrap()
            .unwrap()
            .body
            .unwrap()
            .contains(important_detail)
    );
}

#[tokio::test]
async fn should_not_renominate_changed_documents_that_were_already_promoted() {
    let store = AppStore::new().await;
    store.ingest(
        "doc:reader:1",
        "Prefers long chapters.",
        "Old preference.",
        "v1",
    );
    store.recall.touch("doc:reader:1").unwrap();
    store.nominate(1).await;
    store.ingest(
        "doc:reader:1",
        "Now prefers short chapters.",
        "New preference.",
        "v2",
    );
    let updated = store.recall.get("doc:reader:1").unwrap().unwrap();
    assert_eq!(updated.abstract_, "Now prefers short chapters.");
    assert!(
        updated.promoted,
        "promotion flag survives changed-content ingest"
    );
    assert!(store.nominate(1).await.is_empty());
    assert!(store.notes()[0].content.contains("Prefers long chapters."));
    assert!(
        !store.notes()[0]
            .content
            .contains("Now prefers short chapters.")
    );
}

#[tokio::test]
async fn should_keep_failed_consolidation_staged_for_a_later_retry() {
    let store = AppStore::new().await;
    store.seed_memory().await;
    store.ingest(
        "doc:reader:1",
        "Prefers short chapters.",
        "Full document.",
        "v1",
    );
    store.recall.touch("doc:reader:1").unwrap();
    store.nominate(1).await;
    let notes = store.notes();
    let invalid = serde_json::json!({
        "ops":[{"op":"hard_delete", "id":"^mbbbbbb", "authorized_by":notes[0].id}],
        "consumed_ids":[notes[0].id], "dropped":[]
    })
    .to_string();
    let rejected = run_consolidation(ScriptedMerge::new(vec![invalid]), &store.params)
        .await
        .unwrap();
    assert!(
        !rejected.merge_applied,
        "an app fact cannot authorize destruction"
    );
    assert!(!rejected.errors.is_empty());
    assert!(
        store
            .bank
            .read_long_term()
            .await
            .unwrap()
            .contains("^mbbbbbb")
    );
    assert_eq!(
        store.notes().len(),
        1,
        "failed merge retains captured evidence"
    );
    let valid = serde_json::json!({
        "ops":[{"op":"update", "id":"^maaaaaa", "new_text":"Prefers short chapters.", "sources":[notes[0].id]}],
        "consumed_ids":[notes[0].id], "dropped":[]
    }).to_string();
    let retried = run_consolidation(ScriptedMerge::new(vec![valid]), &store.params)
        .await
        .unwrap();
    assert!(retried.merge_applied, "{:?}", retried.errors);
    assert!(store.notes().is_empty());
}

#[tokio::test]
async fn should_apply_an_existing_host_correction_and_keep_unrelated_memory() {
    let store = AppStore::new().await;
    store.seed_memory().await;
    store
        .bank
        .write_staging_note(&StagingNote {
            origin: octos_memory::NoteOrigin::Host,
            kind: octos_memory::NoteKind::Correction,
            content: "Now prefers short chapters.".into(),
            session_key: None,
            sensitive: false,
            replaces_id: Some("^maaaaaa".into()),
        })
        .await
        .unwrap();
    let notes = store.notes();
    let reply = serde_json::json!({
        "ops":[{"op":"update", "id":"^maaaaaa", "new_text":"Prefers short chapters.", "sources":[notes[0].id]}],
        "consumed_ids":[notes[0].id], "dropped":[]
    }).to_string();
    let outcome = run_consolidation(ScriptedMerge::new(vec![reply]), &store.params)
        .await
        .unwrap();
    assert!(outcome.merge_applied, "{:?}", outcome.errors);
    let memory = store.bank.read_long_term().await.unwrap();
    assert!(memory.contains("Prefers short chapters."));
    assert!(memory.contains("Likes nature books."));
    assert!(!memory.contains("Prefers long chapters."));
}

#[tokio::test]
async fn should_apply_an_exact_host_forget_without_spending_model_budget() {
    let mut store = AppStore::new().await;
    store.seed_memory().await;
    store.params.allow_merge = false;
    store
        .bank
        .write_staging_note(&StagingNote {
            origin: octos_memory::NoteOrigin::Host,
            kind: octos_memory::NoteKind::Forget,
            content: "id:^maaaaaa".into(),
            session_key: None,
            sensitive: false,
            replaces_id: None,
        })
        .await
        .unwrap();
    let provider = ScriptedMerge::new(vec![]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.hard_deleted.contains(&"^maaaaaa".to_string()));
    assert_eq!(provider.calls(), 0);
    let memory = store.bank.read_long_term().await.unwrap();
    assert!(!memory.contains("Prefers long chapters."));
    assert!(memory.contains("Likes nature books."));
}

#[tokio::test]
async fn should_cancel_a_host_correction_after_its_target_is_forgotten() {
    let mut store = AppStore::new().await;
    store.seed_memory().await;
    let correction = StagingNote {
        origin: octos_memory::NoteOrigin::Host,
        kind: octos_memory::NoteKind::Correction,
        content: "Now prefers short chapters.".into(),
        session_key: None,
        sensitive: false,
        replaces_id: Some("^maaaaaa".into()),
    };
    store
        .bank
        .write_staging_request("correct-before-forget", &correction)
        .await
        .unwrap();
    let correction_id = store.notes()[0].id.clone();
    let mut unrelated = correction.clone();
    unrelated.content = "Now likes history books.".into();
    unrelated.replaces_id = Some("^mbbbbbb".into());
    store.bank.write_staging_note(&unrelated).await.unwrap();
    store
        .bank
        .write_staging_note(&StagingNote {
            origin: octos_memory::NoteOrigin::Host,
            kind: octos_memory::NoteKind::Forget,
            content: "id:^maaaaaa".into(),
            session_key: None,
            sensitive: false,
            replaces_id: None,
        })
        .await
        .unwrap();
    store.params.allow_merge = false;
    let provider = ScriptedMerge::new(vec![]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.hard_deleted.contains(&"^maaaaaa".to_string()));
    assert!(
        outcome
            .dropped
            .iter()
            .any(|(id, reason)| id == &correction_id && reason.contains("target no longer exists"))
    );
    assert_eq!(provider.calls(), 0);
    let remaining = store.notes();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].replaces_id.as_deref(), Some("^mbbbbbb"));
    // Retrying the original accepted request must not bring the cancelled ask back.
    store
        .bank
        .write_staging_request("correct-before-forget", &correction)
        .await
        .unwrap();
    assert_eq!(store.notes().len(), 1);
    store.params.allow_merge = true;
    let reply = serde_json::json!({
        "ops":[{"op":"update", "id":"^mbbbbbb", "new_text":"Now likes history books.", "sources":[remaining[0].id]}],
        "consumed_ids":[remaining[0].id], "dropped":[]
    }).to_string();
    let provider = ScriptedMerge::new(vec![reply]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.merge_applied, "{:?}", outcome.errors);
    assert_eq!(provider.calls(), 1);
    assert!(store.notes().is_empty());
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.errors.is_empty());
    assert_eq!(
        provider.calls(),
        1,
        "later passes must not retry the deleted target"
    );
    let memory = store.bank.read_long_term().await.unwrap();
    assert!(!memory.contains("^maaaaaa"));
    assert!(memory.contains("Now likes history books."));
}

#[tokio::test]
async fn should_keep_host_corrections_whose_target_survives_in_the_archive() {
    let mut store = AppStore::new().await;
    std::fs::create_dir_all(store.params.memory_dir.join("archive")).unwrap();
    std::fs::write(
        store.params.memory_dir.join("archive/2026-10.md"),
        "Prefers long chapters. (updated: 2026-10-20) ^maaaaaa\n",
    )
    .unwrap();
    store
        .bank
        .write_staging_note(&StagingNote {
            origin: octos_memory::NoteOrigin::Host,
            kind: octos_memory::NoteKind::Correction,
            content: "Now prefers short chapters.".into(),
            session_key: None,
            sensitive: false,
            replaces_id: Some("^maaaaaa".into()),
        })
        .await
        .unwrap();
    store.params.allow_merge = false;
    let provider = ScriptedMerge::new(vec![]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.dropped.is_empty());
    assert_eq!(store.notes().len(), 1);
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn should_only_cancel_host_corrections_with_an_explicit_missing_target() {
    let mut store = AppStore::new().await;
    for (origin, kind, target) in [
        (
            octos_memory::NoteOrigin::Host,
            octos_memory::NoteKind::UserRequest,
            None,
        ),
        (
            octos_memory::NoteOrigin::Host,
            octos_memory::NoteKind::Correction,
            None,
        ),
        (
            octos_memory::NoteOrigin::Model,
            octos_memory::NoteKind::Correction,
            Some("^maaaaaa".into()),
        ),
    ] {
        store
            .bank
            .write_staging_note(&StagingNote {
                origin,
                kind,
                content: "Now prefers short chapters.".into(),
                session_key: None,
                sensitive: false,
                replaces_id: target,
            })
            .await
            .unwrap();
    }
    store.params.allow_merge = false;
    let provider = ScriptedMerge::new(vec![]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.dropped.is_empty());
    assert_eq!(store.notes().len(), 3);
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn should_retire_an_already_orphaned_host_correction_without_calling_a_model() {
    let store = AppStore::new().await;
    store.seed_memory().await;
    store
        .bank
        .write_staging_note(&StagingNote {
            origin: octos_memory::NoteOrigin::Host,
            kind: octos_memory::NoteKind::Correction,
            content: "Now prefers short chapters.".into(),
            session_key: None,
            sensitive: false,
            replaces_id: Some("^maaaaaa".into()),
        })
        .await
        .unwrap();
    // Simulate a target deleted by a previous run, leaving its correction behind.
    std::fs::write(
        store.params.memory_dir.join("MEMORY.md"),
        format!(
            "Likes nature books. (updated: {}) ^mbbbbbb\n",
            store.params.today
        ),
    )
    .unwrap();
    let provider = ScriptedMerge::new(vec![]);
    let outcome = run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.dropped.len(), 1);
    assert!(store.notes().is_empty());
    assert_eq!(provider.calls(), 0);
    run_consolidation(provider.clone(), &store.params)
        .await
        .unwrap();
    assert_eq!(provider.calls(), 0);
    assert!(
        store
            .bank
            .read_long_term()
            .await
            .unwrap()
            .contains("^mbbbbbb")
    );
}
