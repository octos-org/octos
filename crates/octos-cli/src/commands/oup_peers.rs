//! Local peer presentation: open OUP child sessions, never assemble agents.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use eyre::Result;
use octos_core::{SessionKey, ui_protocol::*};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::oup_session::{OupFrontend, OupSession};

pub(crate) struct OupPeerHost {
    state: Arc<crate::api::AppState>,
    permissions: octos_agent::EffectivePermissions,
    peers: Mutex<HashMap<SessionKey, (CancellationToken, JoinHandle<()>)>>,
}

impl OupPeerHost {
    pub(crate) fn new(
        state: Arc<crate::api::AppState>,
        permissions: octos_agent::EffectivePermissions,
    ) -> Self {
        Self {
            state,
            permissions,
            peers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn event(&self, event: &UiNotification) {
        match event {
            UiNotification::PeerStaged(event) => {
                let key = SessionKey(format!("{}#{}", event.session_id.base_key(), event.topic));
                let mut peers = self.peers.lock().unwrap();
                // Durable replay must not run the same brief twice.
                if peers.contains_key(&key) {
                    return;
                }
                let state = self.state.clone();
                let permissions = self.permissions;
                let event = event.clone();
                let stop = CancellationToken::new();
                let stopped = stop.clone();
                let task = tokio::spawn(async move {
                    if let Err(error) = serve_peer(state, event, permissions, stopped).await {
                        eprintln!("Peer session failed: {error}");
                    }
                });
                peers.insert(key, (stop, task));
            }
            UiNotification::PeerClosed(event) => {
                let key = SessionKey(format!("{}#{}", event.session_id.base_key(), event.topic));
                if let Some((stop, _)) = self.peers.lock().unwrap().get(&key) {
                    stop.cancel();
                }
            }
            _ => {}
        }
    }

    pub(crate) async fn close(&self) {
        let peers = std::mem::take(&mut *self.peers.lock().unwrap());
        for (stop, _) in peers.values() {
            stop.cancel();
        }
        for (_, (_, task)) in peers {
            let _ = task.await;
        }
    }
}

impl Drop for OupPeerHost {
    fn drop(&mut self) {
        // Cancellation lets the owner close the OUP transport and settle its
        // server-side work; aborting the task would skip that cleanup.
        for (stop, _) in self.peers.get_mut().unwrap().values() {
            stop.cancel();
        }
    }
}

struct PeerFrontend(String);

#[async_trait::async_trait]
impl OupFrontend for PeerFrontend {
    async fn event(&self, event: UiNotification) -> Result<Option<UiCommand>> {
        // The OUP backend already parked the request in the canonical contract
        // store. Only the master's peer_respond may answer it.
        match event {
            UiNotification::ApprovalRequested(event) => {
                eprintln!("Peer '{}' awaits approval: {}", self.0, event.title)
            }
            UiNotification::UserQuestionRequested(event) => {
                eprintln!("Peer '{}' awaits input: {}", self.0, event.title)
            }
            _ => {}
        }
        Ok(None)
    }
}

async fn serve_peer(
    state: Arc<crate::api::AppState>,
    event: PeerStagedEvent,
    permissions: octos_agent::EffectivePermissions,
    stop: CancellationToken,
) -> Result<()> {
    let key = SessionKey(format!("{}#{}", event.session_id.base_key(), event.topic));
    let session =
        OupSession::open(state, key, std::path::Path::new(&event.cwd), permissions).await?;
    let frontend = PeerFrontend(event.slug.clone());
    let work = async {
        // A reopened, already-started peer must not replay its initial brief.
        if session
            .hydrate()
            .await?
            .messages
            .unwrap_or_default()
            .is_empty()
        {
            session
                .turn(&event.brief, None, &AtomicBool::new(false), &frontend)
                .await?;
        }
        session.listen(&frontend).await
    };
    let result = tokio::select! {
        result = work => result,
        _ = stop.cancelled() => Ok(()),
    };
    session.close().await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::acp::{SessionAgentFactory, TestAgentFactory};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Model(AtomicUsize);
    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for Model {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(octos_llm::ChatResponse {
                content: Some("peer deliverable".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: Default::default(),
                provider_index: None,
            })
        }
        fn provider_name(&self) -> &str {
            "local"
        }
        fn model_id(&self) -> &str {
            "peer-test"
        }
    }

    #[tokio::test]
    async fn local_peer_uses_oup_registration_persistence_and_replay_deduplication() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(Model(AtomicUsize::new(0)));
        let factory = TestAgentFactory::new(
            model.clone(),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let state = factory.oup_state().await.unwrap();
        let root = data.path().join("peers");
        std::fs::create_dir_all(&root).unwrap();
        let master = SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "cli", "peer-test");
        let staged = crate::peers::stage_peer(
            &root,
            workspace.path(),
            "scout",
            Some("scout"),
            Some(&master.0),
            "summarize the repo",
            false,
            None,
            None,
        )
        .unwrap();
        let event = UiNotification::PeerStaged(PeerStagedEvent {
            session_id: master.clone(),
            topic: format!("peer-{}", staged.slug),
            slug: staged.slug.clone(),
            brief: "summarize the repo".into(),
            brief_path: staged.brief_path.to_string_lossy().into_owned(),
            cwd: workspace.path().to_string_lossy().into_owned(),
            worktree_branch: None,
            profile_id: octos_core::MAIN_PROFILE_ID.into(),
        });
        let host = OupPeerHost::new(state, octos_agent::EffectivePermissions::workspace_write());
        host.event(&event);
        host.event(&event);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let rows = crate::peers::read_peer_blackboard(&root, None);
                if rows.iter().any(|row| {
                    row.result.as_deref().is_some_and(|text| {
                        text.ends_with("peer deliverable\n") && text.contains("turn: 1\n")
                    })
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "OUP peer result missing: calls={}, rows={:?}, wire={:?}",
                model.0.load(Ordering::SeqCst),
                crate::peers::read_peer_blackboard(&root, None)
                    .iter()
                    .map(|row| (&row.slug, &row.result))
                    .collect::<Vec<_>>(),
                crate::peers::peer_trusted_session(octos_core::MAIN_PROFILE_ID, &staged.slug)
            )
        });
        assert_eq!(
            model.0.load(Ordering::SeqCst),
            1,
            "replayed staging must not run twice"
        );
        assert_eq!(
            crate::peers::peer_trusted_session(octos_core::MAIN_PROFILE_ID, &staged.slug),
            Some(SessionKey(format!("{}#peer-{}", master.0, staged.slug)))
        );
        host.close().await;
    }
}
