//! Local peer presentation: open OUP child sessions, never assemble agents.

use std::collections::HashMap;
use std::path::Path;
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
                self.open_peer(event.clone());
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

    fn open_peer(
        &self,
        event: PeerStagedEvent,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<(), String>>> {
        let key = SessionKey(format!("{}#{}", event.session_id.base_key(), event.topic));
        let mut peers = self.peers.lock().unwrap();
        // A finished transport may reopen, but a live writer is never replaced.
        if peers.get(&key).is_some_and(|(_, task)| !task.is_finished()) {
            return None;
        }
        let state = self.state.clone();
        let permissions = self.permissions;
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let (ready, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(error) = serve_peer(state, event, permissions, stopped, ready).await {
                eprintln!("Peer session failed: {error}");
            }
        });
        peers.insert(key, (stop, task));
        Some(receiver)
    }

    /// Restore transports for this terminal's own staged peers before accepting
    /// another master turn. Keep the peer identity, transcript and budget; do
    /// not synthesize a new task, adopt another host, or replay an old brief.
    pub(crate) async fn restore(&self, master: &SessionKey, workspace: &Path) -> Result<usize> {
        use crate::peers::{self, peer_io};
        let profile_id = master
            .profile_id()
            .ok_or_else(|| eyre::eyre!("peer restore requires a profile-scoped session"))?;
        let runtime = self
            .state
            .profiles
            .get(profile_id)
            .ok_or_else(|| eyre::eyre!("peer restore profile is unavailable"))?;
        let root = runtime.data_dir.join("peers");
        let workspace = workspace.canonicalize()?;
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut restored = 0;
        for entry in entries {
            let slug = entry?.file_name().to_string_lossy().into_owned();
            let Some(dir) = peers::staged_peer_dir(&root, &slug) else {
                continue;
            };
            if peers::peer_send_input_authorized(&root, &slug, &master.0).is_err()
                || peers::peer_is_closed(&root, &slug)
                || peers::app_binding::peer_is_host_owned(&root, &slug)
                || peers::peer_trusted_session(profile_id, &slug).is_some()
            {
                continue;
            }
            let Some(recorded) =
                peer_io::read_peer_file(&dir, "workspace-root", peer_io::PEER_FILE_READ_CAP_SMALL)
            else {
                eprintln!("Peer '{slug}' has no workspace binding; automatic restore skipped");
                continue;
            };
            if Path::new(&recorded).canonicalize().ok().as_ref() != Some(&workspace) {
                continue;
            }
            let Some(brief) =
                peer_io::read_peer_file(&dir, "brief.md", peer_io::PEER_FILE_READ_CAP_LARGE)
            else {
                continue;
            };
            let cwd = match peer_io::read_peer_file(
                &dir,
                "workspace-mode",
                peer_io::PEER_FILE_READ_CAP_SMALL,
            )
            .as_deref()
            {
                Some("shared") => workspace.clone(),
                Some("fenced") => {
                    let fenced = dir.join("wt");
                    let metadata = std::fs::symlink_metadata(&fenced)?;
                    eyre::ensure!(
                        metadata.is_dir() && !metadata.file_type().is_symlink(),
                        "peer '{slug}' has an invalid fenced workspace"
                    );
                    fenced
                }
                _ => {
                    eyre::bail!("peer '{slug}' has no valid workspace mode; cannot restore safely")
                }
            };
            let event = PeerStagedEvent {
                session_id: master.clone(),
                topic: format!("peer-{slug}"),
                brief,
                brief_path: dir.join("brief.md").to_string_lossy().into_owned(),
                worktree_branch: (cwd != workspace).then(|| format!("peer/{slug}")),
                cwd: cwd.to_string_lossy().into_owned(),
                slug,
                profile_id: profile_id.to_owned(),
            };
            if let Some(ready) = self.open_peer(event) {
                ready
                    .await
                    .map_err(|_| eyre::eyre!("peer transport closed before restore completed"))?
                    .map_err(|error| eyre::eyre!(error))?;
                restored += 1;
            }
        }
        Ok(restored)
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
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> Result<()> {
    let key = SessionKey(format!("{}#{}", event.session_id.base_key(), event.topic));
    let session =
        match OupSession::open(state, key, std::path::Path::new(&event.cwd), permissions).await {
            Ok(session) => session,
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return Err(error);
            }
        };
    let frontend = PeerFrontend(event.slug.clone());
    let work = async {
        // A reopened, already-started peer must not replay its initial brief.
        let initial = session
            .hydrate()
            .await?
            .messages
            .unwrap_or_default()
            .is_empty();
        let _ = ready.send(Ok(()));
        if initial {
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
                usage: octos_llm::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
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
    async fn terminal_restore_reconnects_same_peer_without_replaying_brief() {
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
        let master = SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "cli", "restore-peer");
        let staged = crate::peers::stage_peer_with_budget(
            &root,
            workspace.path(),
            "maker",
            Some("maker"),
            Some(&master.0),
            "make a document",
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let permissions = octos_agent::EffectivePermissions::workspace_write();
        let first = OupPeerHost::new(state.clone(), permissions);
        assert_eq!(first.restore(&master, workspace.path()).await.unwrap(), 1);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !root.join(&staged.slug).join("result.md").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        first.close().await;
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
        let second = OupPeerHost::new(state, permissions);
        assert_eq!(second.restore(&master, workspace.path()).await.unwrap(), 1);
        assert_eq!(second.restore(&master, workspace.path()).await.unwrap(), 0);
        assert_eq!(
            crate::peers::peer_trusted_session(octos_core::MAIN_PROFILE_ID, &staged.slug),
            Some(SessionKey(format!("{}#peer-{}", master.0, staged.slug)))
        );
        assert_eq!(
            model.0.load(Ordering::SeqCst),
            1,
            "restoring a completed peer must not run the original brief again"
        );
        let peer_key =
            crate::peers::peer_trusted_session(octos_core::MAIN_PROFILE_ID, &staged.slug).unwrap();
        let queued = crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .enqueue_peer_send_input_continuation(
                &peer_key,
                octos_core::MAIN_PROFILE_ID,
                &staged.slug,
                "restore-followup",
                "check the existing document",
            );
        assert_eq!(
            queued,
            crate::autonomy::agent_orchestrator::PeerSendInputEnqueueOutcome::Queued
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !root.join(&staged.slug).join("result-2.md").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the restored peer must receive follow-up input on its original session");
        assert_eq!(model.0.load(Ordering::SeqCst), 2);
        second.close().await;
    }

    #[tokio::test]
    async fn terminal_restore_ignores_foreign_closed_and_other_workspace_peers() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let other_workspace = tempfile::tempdir().unwrap();
        let model = Arc::new(Model(AtomicUsize::new(0)));
        let factory = TestAgentFactory::new(
            model.clone(),
            data.path().to_owned(),
            workspace.path().to_owned(),
        );
        let state = factory.oup_state().await.unwrap();
        let root = data.path().join("peers");
        let master = SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "cli", "owner");
        for (name, owner, cwd) in [
            ("foreign", "another-session", workspace.path()),
            ("closed", master.0.as_str(), workspace.path()),
            ("other-workspace", master.0.as_str(), other_workspace.path()),
        ] {
            let staged = crate::peers::stage_peer_with_budget(
                &root,
                cwd,
                name,
                Some(name),
                Some(owner),
                "do not execute",
                false,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            if name == "closed" {
                crate::peers::peer_io::write_peer_file_atomic(
                    &root.join(staged.slug),
                    "closed",
                    "closed by user",
                )
                .unwrap();
            }
        }
        let host = OupPeerHost::new(state, octos_agent::EffectivePermissions::workspace_write());
        assert_eq!(host.restore(&master, workspace.path()).await.unwrap(), 0);
        assert_eq!(model.0.load(Ordering::SeqCst), 0);
        host.close().await;
    }

    #[tokio::test]
    async fn terminal_restore_never_guesses_legacy_or_missing_fenced_workspace() {
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
        let master = SessionKey::with_profile(octos_core::MAIN_PROFILE_ID, "cli", "legacy-owner");
        let legacy = crate::peers::stage_peer_with_budget(
            &root,
            workspace.path(),
            "legacy",
            Some("legacy"),
            Some(&master.0),
            "do not execute",
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        std::fs::remove_file(root.join(&legacy.slug).join("workspace-root")).unwrap();
        let host = OupPeerHost::new(state, octos_agent::EffectivePermissions::workspace_write());
        assert_eq!(host.restore(&master, workspace.path()).await.unwrap(), 0);
        let fenced = crate::peers::stage_peer_with_budget(
            &root,
            workspace.path(),
            "lost-fence",
            Some("lost-fence"),
            Some(&master.0),
            "do not execute",
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        crate::peers::peer_io::write_peer_file_atomic(
            &root.join(&fenced.slug),
            "workspace-mode",
            "fenced",
        )
        .unwrap();
        assert!(
            host.restore(&master, workspace.path()).await.is_err(),
            "a lost fence must not fall back to the master's workspace"
        );
        assert_eq!(model.0.load(Ordering::SeqCst), 0);
        host.close().await;
    }
}
