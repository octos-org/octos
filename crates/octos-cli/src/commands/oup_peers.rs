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
