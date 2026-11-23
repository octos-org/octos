//! Bridges admitted peer turns and durable receipt-driven workflows. Only native
//! adapters construct contracts. The wire protocol exposes no contract setter.
use super::{
    PeerCheckpointHandoff, QueuedMasterContinuation, peer_send_input_authorized, staged_peer_dir,
};
use crate::{
    autonomy::agent_orchestrator::{
        InProcessAgentOrchestrator, PEER_SEND_INPUT_META_OCCURRENCE, PEER_SEND_INPUT_META_SLUG,
        PeerSendInputEnqueueOutcome, WORKFLOW_ACTIVITY_META, WORKFLOW_EXIT_META,
    },
    peers,
};
use eyre::{Result, ensure, eyre};
use octos_core::SessionKey;
use octos_workflows::activity::{
    ActivityAttempt, Admission, Observation, ReceiptCursor, ReceiptJournal, Resolution, WorkerExit,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(super) const UNSAVED_EXIT: &str = "workflow_unsaved_exit";

fn bytes(root: &Path, path: &Path) -> Result<Option<Vec<u8>>> {
    match octos_llm::vision::read_workspace_bytes_no_follow(root, path) {
        Ok(data) => Ok(Some(data)),
        Err(err)
            if err.chain().any(|e| {
                e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn hash(root: &Path, path: &Path) -> Result<Option<String>> {
    Ok(bytes(root, path)?.map(|b| format!("{:x}", Sha256::digest(b))))
}

fn journal(root: &Path, stream: &Path) -> Result<ReceiptJournal> {
    let data =
        bytes(root, stream)?.ok_or_else(|| eyre!("workflow receipt journal is unavailable"))?;
    Ok(serde_json::from_slice(&data)?)
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct WorkerReturn {
    pub exit: WorkerExit,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PeerActivity {
    worker: PeerCheckpointHandoff,
    label: String,
    run: ActivityAttempt,
    stream: PathBuf,
    cursor: ReceiptCursor,
    plan: BTreeMap<PathBuf, Option<String>>,
    heads: BTreeMap<PathBuf, Option<String>>,
}

impl PeerActivity {
    pub(super) fn bind(
        data_dir: &Path,
        worker: &PeerCheckpointHandoff,
        run: ActivityAttempt,
        stream: PathBuf,
        phase: &str,
        kind: &str,
        admission: Admission,
        plan: BTreeMap<PathBuf, Option<String>>,
        heads: BTreeMap<PathBuf, Option<String>>,
    ) -> Result<Self> {
        let cursor = ReceiptCursor::at(&journal(data_dir, &stream)?, phase, kind, admission)
            .ok_or_else(|| eyre!("workflow no longer awaits {phase}"))?;
        Ok(Self {
            worker: worker.clone(),
            label: run.operation.clone(),
            run,
            stream,
            cursor,
            plan,
            heads,
        })
    }

    pub(super) fn with_label(mut self, label: String) -> Self {
        self.label = label;
        self
    }

    pub(super) fn from_item(item: &QueuedMasterContinuation) -> Result<Option<Self>> {
        let Some(raw) = item.metadata.get(WORKFLOW_ACTIVITY_META) else {
            return Ok(None);
        };
        ensure!(
            item.metadata
                .get(crate::autonomy::agent_orchestrator::PEER_SEND_INPUT_META_MESSAGE)
                .is_some_and(|s| !s.trim().is_empty()),
            "workflow assignment has no instruction"
        );
        let activity: Self = serde_json::from_str(raw)?;
        let occurrence = if item.metadata.contains_key(WORKFLOW_EXIT_META) {
            activity.exit_id()
        } else {
            activity.run.occurrence()
        };
        ensure!(
            item.metadata.get(PEER_SEND_INPUT_META_OCCURRENCE) == Some(&occurrence)
                && item.metadata.get(PEER_SEND_INPUT_META_SLUG) == Some(&activity.worker.slug),
            "workflow delivery identity changed"
        );
        Ok(Some(activity))
    }

    fn exit_id(&self) -> String {
        format!("{}/returned", self.run.occurrence())
    }

    pub(super) fn record_exit(
        &self,
        item: &QueuedMasterContinuation,
        scheduler: &InProcessAgentOrchestrator,
        returned: &WorkerReturn,
    ) -> Result<()> {
        let result = scheduler.enqueue_activity_successor(
            item,
            &self.exit_id(),
            serde_json::to_string(self)?,
            Some(serde_json::to_string(returned)?),
            || Ok(()),
        );
        result
            .into_callback_result(&self.worker.slug)
            .map_err(|e| eyre!("{e}"))?;
        Ok(())
    }

    fn observe(&self, data_dir: &Path, item: &QueuedMasterContinuation) -> Result<Observation> {
        let peer = SessionKey(item.session_id.as_str().to_owned());
        let profile = item.profile_id.as_str();
        let root = data_dir.join("peers");
        let workspace = &self.worker.workspace;
        if PeerCheckpointHandoff::capture(&root, profile, &peer, workspace).as_ref()
            != Some(&self.worker)
            || super::session_workspace_root_for_profile(Some(profile), &self.worker.master)
                .as_ref()
                != Some(workspace)
            || peer_send_input_authorized(&root, &self.worker.slug, &self.worker.master.0).is_err()
        {
            return Ok(Observation::Superseded);
        }
        for (path, expected) in &self.plan {
            if &hash(workspace, path)? != expected {
                return Ok(Observation::Superseded);
            }
        }
        let state = self.cursor.observe(&journal(data_dir, &self.stream)?);
        if state == Observation::Pending {
            for (path, expected) in &self.heads {
                if &hash(workspace, path)? != expected {
                    // A changed workspace file alone cannot settle an operation.
                    // Prefer a concurrently accepted native receipt if it has arrived.
                    let latest = self.cursor.observe(&journal(data_dir, &self.stream)?);
                    return Ok(if latest == Observation::Pending {
                        Observation::Inconsistent
                    } else {
                        latest
                    });
                }
            }
        }
        Ok(state)
    }

    /// Returns None when the worker may start. Receipt and generation are
    /// checked again after dequeue, including deliveries restored after restart.
    pub(super) fn preflight(
        &self,
        data_dir: &Path,
        item: &QueuedMasterContinuation,
        scheduler: &InProcessAgentOrchestrator,
    ) -> Result<Option<&'static str>> {
        if let Some(raw) = item.metadata.get(UNSAVED_EXIT) {
            self.record_exit(item, scheduler, &serde_json::from_str(raw)?)?;
            return Ok(Some("workflow_worker_exit_durable"));
        }
        if let Some(raw) = item.metadata.get(WORKFLOW_EXIT_META) {
            return self
                .finish(data_dir, item, scheduler, serde_json::from_str(raw)?)
                .map(Some);
        }
        if scheduler.peer_occurrence_recorded(
            &SessionKey(item.session_id.as_str().to_owned()),
            &self.exit_id(),
        ) {
            return Ok(Some("workflow_worker_exit_durable"));
        }
        let observed = self.observe(data_dir, item)?;
        if observed == Observation::Pending {
            return Ok(None);
        }
        self.finish_observed(
            data_dir,
            item,
            scheduler,
            observed,
            WorkerExit::Returned,
            None,
        )
        .map(Some)
    }

    fn finish(
        &self,
        data_dir: &Path,
        item: &QueuedMasterContinuation,
        scheduler: &InProcessAgentOrchestrator,
        returned: WorkerReturn,
    ) -> Result<&'static str> {
        self.finish_observed(
            data_dir,
            item,
            scheduler,
            self.observe(data_dir, item)?,
            returned.exit,
            Some(&returned.turn_id),
        )
    }

    fn finish_observed(
        &self,
        data_dir: &Path,
        item: &QueuedMasterContinuation,
        scheduler: &InProcessAgentOrchestrator,
        observed: Observation,
        exit: WorkerExit,
        turn_id: Option<&str>,
    ) -> Result<&'static str> {
        let resolution = self.run.resolve(observed, exit);
        let profile = item.profile_id.as_str();
        let worker = &self.worker;
        let target = SessionKey(item.session_id.as_str().to_owned());
        match resolution {
            Resolution::Settled => Ok("workflow_receipt_accepted"),
            Resolution::Superseded => Ok("workflow_activity_superseded"),
            Resolution::Interrupted => Ok("workflow_activity_interrupted"),
            Resolution::Resume(run) => {
                let root = data_dir.join("peers");
                let Some(dir) = staged_peer_dir(&root, &worker.slug) else {
                    return Ok("workflow_activity_superseded");
                };
                let id = run.occurrence();
                let mut successor = self.clone();
                successor.run = run;
                let contract = serde_json::to_string(&successor)?;
                let outcome = peers::with_peer_input_lifetime(&dir, |advance| {
                    let wire = peers::peer_wire_registry()
                        .by_key
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if wire.get(&peers::peer_wire_key(profile, &worker.slug)) != Some(&target)
                        || PeerCheckpointHandoff::read(&dir, profile, &worker.slug, None).as_ref()
                            != Some(worker)
                    {
                        return None;
                    }
                    Some(scheduler.enqueue_activity_successor(item, &id, contract, None, advance))
                });
                match outcome {
                    None => Ok("workflow_activity_superseded"),
                    Some(PeerSendInputEnqueueOutcome::PersistFailed) => {
                        Err(eyre!("could not persist workflow successor"))
                    }
                    Some(_) => Ok("workflow_successor_durable"),
                }
            }
            Resolution::Blocked(reason) => {
                let id = format!("{}/blocked", self.run.occurrence());
                let checkpoint = serde_json::json!({"version":1,"session":target,"handoff_id":id,
                    "turn_id":turn_id,"is_completion":false,"operation":self.run.operation,"attempt":self.run.attempt,
                    "message":format!("Workflow activity {} is blocked: {reason}.", self.label)});
                let envelope = serde_json::json!({"publication":worker, "checkpoint":checkpoint});
                let result = scheduler.enqueue_peer_checkpoint_handoff(
                    &scheduler.scoped_goal_key(&worker.master),
                    profile,
                    worker.stage_id,
                    &id,
                    envelope.to_string(),
                    || Ok(()),
                );
                result
                    .into_callback_result(&worker.slug)
                    .map_err(|e| eyre!("{e}"))?;
                Ok("workflow_activity_blocked")
            }
        }
    }
}
