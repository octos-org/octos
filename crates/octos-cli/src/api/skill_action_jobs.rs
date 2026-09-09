use std::path::Path;

use chrono::{DateTime, Utc};
use octos_agent::{BackgroundTask, TaskStatus, TaskSupervisor};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ui_protocol_task_output::task_state_path;

const SKILL_ACTION_PROJECTION_KIND: &str = "skill_action";
const MAX_RETAINED_JOBS_PER_SESSION: usize = 256;
const ORPHANED_ACROSS_RESTART: &str = "orphaned across restart";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkillActionJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Abandoned,
}

impl SkillActionJobStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// Skill-owned fields persisted inside `BackgroundTask::projection_metadata`.
/// The task supervisor remains generic and is the only lifecycle writer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SkillActionProjectionMetadata {
    kind: String,
    pub batch_id: String,
    pub profile_id: String,
    pub session_id: SessionKey,
    pub action_id: String,
    pub skill_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialized_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

impl SkillActionProjectionMetadata {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        batch_id: String,
        profile_id: String,
        session_id: SessionKey,
        action_id: String,
        skill_id: String,
        input_path: Option<String>,
        filename: Option<String>,
        materialized_path: Option<String>,
    ) -> Self {
        Self {
            kind: SKILL_ACTION_PROJECTION_KIND.to_owned(),
            batch_id,
            profile_id,
            session_id,
            action_id,
            skill_id,
            input_path,
            filename,
            materialized_path,
            result: None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("skill action projection metadata is serializable")
    }

    pub(crate) fn from_task(task: &BackgroundTask) -> Option<Self> {
        let value = task.projection_metadata.as_ref()?;
        if value.get("kind").and_then(Value::as_str) != Some(SKILL_ACTION_PROJECTION_KIND) {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SkillActionJobRecord {
    pub job_id: String,
    pub batch_id: String,
    pub profile_id: String,
    pub session_id: SessionKey,
    pub action_id: String,
    pub skill_id: String,
    pub status: SkillActionJobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialized_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn projected_status(task: &BackgroundTask) -> SkillActionJobStatus {
    match task.status {
        TaskStatus::Spawned => SkillActionJobStatus::Queued,
        TaskStatus::Running => SkillActionJobStatus::Running,
        TaskStatus::Completed => SkillActionJobStatus::Succeeded,
        TaskStatus::Cancelled => SkillActionJobStatus::Cancelled,
        // #27c — a PARKED task awaits client re-attachment (peer orphan);
        // project it as the same recoverable "abandoned" view the orphaned
        // error already maps to, not a hard failure.
        TaskStatus::Parked => SkillActionJobStatus::Abandoned,
        TaskStatus::Failed if task.error.as_deref() == Some(ORPHANED_ACROSS_RESTART) => {
            SkillActionJobStatus::Abandoned
        }
        TaskStatus::Failed => SkillActionJobStatus::Failed,
    }
}

pub(crate) fn project_skill_action_job(task: &BackgroundTask) -> Option<SkillActionJobRecord> {
    let metadata = SkillActionProjectionMetadata::from_task(task)?;
    Some(SkillActionJobRecord {
        job_id: task.id.clone(),
        batch_id: metadata.batch_id,
        profile_id: metadata.profile_id,
        session_id: metadata.session_id,
        action_id: metadata.action_id,
        skill_id: metadata.skill_id,
        status: projected_status(task),
        input_path: metadata.input_path,
        filename: metadata.filename,
        materialized_path: metadata.materialized_path,
        output: task.final_output.clone(),
        error: task.error.clone(),
        result: metadata.result,
        created_at: task.started_at,
        updated_at: task.updated_at,
    })
}

pub(crate) fn with_skill_action_result(task: &BackgroundTask, result: Value) -> Option<Value> {
    let mut metadata = SkillActionProjectionMetadata::from_task(task)?;
    metadata.result = Some(result);
    Some(metadata.into_value())
}

/// Restore jobs through the canonical task ledger. `enable_persistence`
/// performs the supervisor's orphan sweep, so queued/running work from a
/// prior process is projected as `abandoned` rather than being mutated by a
/// second job-specific recovery store.
///
/// #2056 round 3 (R4) — this is the runtime-unavailable fallback of the job
/// view: it builds a THROWAWAY supervisor, restores into it, projects, and
/// drops it. That restore is a real one — its orphan sweep marks abandoned
/// work `Failed` and appends those transitions to the durable JSONL — so
/// without the goal-task-row observers it would move the supervisor's side of
/// the world while leaving the goal ledger's rows stale, and the missed-restore
/// mark it left behind would die with the supervisor, unconsumable. Wiring the
/// shared observer pair BEFORE the enable makes this path reconcile like every
/// other restore. `profile_id` is threaded in for exactly that.
pub(crate) fn load_skill_action_jobs(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
) -> std::io::Result<Vec<SkillActionJobRecord>> {
    let supervisor = TaskSupervisor::new();
    crate::autonomy::agent_orchestrator::install_goal_task_row_observers_resolving_at_callback(
        &supervisor,
        session_id,
        profile_id,
        data_dir,
    );
    crate::peers::enable_peer_task_persistence(
        &supervisor,
        task_state_path(data_dir, session_id),
        &data_dir.join("peers"),
        profile_id,
        &session_id.0,
    )?;
    Ok(project_skill_action_jobs(
        supervisor.get_tasks_for_session(&session_id.0),
    ))
}

pub(crate) fn project_skill_action_jobs(tasks: Vec<BackgroundTask>) -> Vec<SkillActionJobRecord> {
    let mut jobs = tasks
        .into_iter()
        .filter_map(|task| project_skill_action_job(&task))
        .collect::<Vec<_>>();
    jobs.sort_by(|left, right| {
        left.updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.job_id.cmp(&right.job_id))
    });
    if jobs.len() > MAX_RETAINED_JOBS_PER_SESSION {
        let active = jobs.iter().filter(|job| job.status.is_active()).count();
        let terminal_budget = MAX_RETAINED_JOBS_PER_SESSION.saturating_sub(active);
        let first_terminal_to_keep = jobs
            .iter()
            .rev()
            .filter(|job| !job.status.is_active())
            .nth(terminal_budget.saturating_sub(1))
            .map(|job| job.updated_at);
        if let Some(cutoff) = first_terminal_to_keep {
            jobs.retain(|job| job.status.is_active() || job.updated_at >= cutoff);
        } else if terminal_budget == 0 {
            jobs.retain(|job| job.status.is_active());
        }
    }
    jobs
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn attach(supervisor: &TaskSupervisor, task_id: &str, session_id: &SessionKey) {
        supervisor.set_projection_metadata(
            task_id,
            SkillActionProjectionMetadata::new(
                "batch-a".into(),
                "profile-a".into(),
                session_id.clone(),
                "source.import".into(),
                "source-skill".into(),
                Some("up://report.pdf".into()),
                Some("report.pdf".into()),
                Some("uploads/report.pdf".into()),
            )
            .into_value(),
        );
    }

    #[test]
    fn task_lifecycle_projects_to_skill_job_contract() {
        let supervisor = TaskSupervisor::new();
        let session_id = SessionKey("local:projection".into());
        let task_id = supervisor.register("source_import", "call-a", Some(&session_id.0));
        attach(&supervisor, &task_id, &session_id);

        let queued = project_skill_action_job(&supervisor.get_task(&task_id).unwrap()).unwrap();
        assert_eq!(queued.job_id, task_id, "job id must equal task id");
        assert_eq!(queued.status, SkillActionJobStatus::Queued);

        supervisor.mark_running(&task_id);
        assert_eq!(
            project_skill_action_job(&supervisor.get_task(&task_id).unwrap())
                .unwrap()
                .status,
            SkillActionJobStatus::Running
        );
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::VerifyingOutputs,
            None,
        );
        assert_eq!(
            project_skill_action_job(&supervisor.get_task(&task_id).unwrap())
                .unwrap()
                .status,
            SkillActionJobStatus::Running,
            "verifying remains the job contract's running state"
        );

        let task = supervisor.get_task(&task_id).unwrap();
        supervisor.set_projection_metadata(
            &task_id,
            with_skill_action_result(&task, json!({"success": true})).unwrap(),
        );
        supervisor.record_final_output(&task_id, "source imported");
        supervisor.mark_completed(&task_id, vec![]);
        let succeeded = project_skill_action_job(&supervisor.get_task(&task_id).unwrap()).unwrap();
        assert_eq!(succeeded.status, SkillActionJobStatus::Succeeded);
        assert_eq!(succeeded.output.as_deref(), Some("source imported"));
        assert_eq!(succeeded.result, Some(json!({"success": true})));
    }

    #[test]
    fn should_restore_completed_peer_when_job_inspection_precedes_first_master_turn() {
        use crate::peers::*;
        let dir = tempfile::tempdir().unwrap();
        let profile = format!("peer-inspection-{}", uuid::Uuid::now_v7());
        let master = SessionKey::with_profile(&profile, "local", "master");
        let peer = SessionKey::with_profile_topic(&profile, "local", "peer", "peer-auditor");
        let peers_root = dir.path().join("peers");
        let peer_dir = peers_root.join("auditor");
        std::fs::create_dir_all(&peer_dir).unwrap();
        peer_io::write_peer_file_atomic(&peer_dir, "brief.md", "review").unwrap();
        peer_io::write_peer_file_atomic(&peer_dir, "originator", &master.0).unwrap();
        let ledger = task_state_path(dir.path(), &master);
        let staging = TaskSupervisor::new();
        staging.enable_persistence(&ledger).unwrap();
        let key = peer_wire_key(&profile, "auditor");
        let task_id = bind_peer_supervised_task(&staging, key.clone(), &master.0).unwrap();
        record_peer_lifetime_binding(&peers_root, &profile, "auditor", &master.0, &task_id)
            .unwrap();
        let token = begin_peer_lifetime_turn(&peers_root, &peer, "completed-turn")
            .unwrap()
            .unwrap();
        peer_io::write_peer_file_atomic(&peer_dir, "result.md", "completed").unwrap();
        finish_peer_lifetime_turn(&token, "completed", true, false).unwrap();
        peer_task_registry().take(&key);
        drop(staging);
        // The real status/inspect fallback is the first path to enable this
        // ledger after boot. It must not append a false Failed before the
        // ordinary foreground factory gets a chance to restore the lifetime.
        assert!(
            load_skill_action_jobs(dir.path(), &profile, &master)
                .unwrap()
                .is_empty()
        );
        let foreground = TaskSupervisor::new();
        enable_peer_task_persistence(&foreground, &ledger, &peers_root, &profile, &master.0)
            .unwrap();
        let row = foreground.get_task(&task_id).unwrap();
        assert!(row.status.is_active());
        assert!(row.error.is_none());
        assert_eq!(
            retire_peer_supervised_task(&foreground, &profile, "auditor"),
            Some(task_id)
        );
    }

    #[test]
    fn cancelled_and_restart_orphans_have_distinct_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionKey("local:recovery".into());
        let ledger = task_state_path(dir.path(), &session_id);
        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger).unwrap();

        let cancelled = supervisor.register("source_import", "cancel", Some(&session_id.0));
        attach(&supervisor, &cancelled, &session_id);
        supervisor.cancel(&cancelled).unwrap();

        let orphan = supervisor.register("source_import", "orphan", Some(&session_id.0));
        attach(&supervisor, &orphan, &session_id);
        supervisor.mark_running(&orphan);
        drop(supervisor);

        let jobs = load_skill_action_jobs(dir.path(), "tenant-skill-jobs", &session_id).unwrap();
        assert_eq!(
            jobs.iter()
                .find(|job| job.job_id == cancelled)
                .unwrap()
                .status,
            SkillActionJobStatus::Cancelled
        );
        assert_eq!(
            jobs.iter().find(|job| job.job_id == orphan).unwrap().status,
            SkillActionJobStatus::Abandoned
        );
    }
}
