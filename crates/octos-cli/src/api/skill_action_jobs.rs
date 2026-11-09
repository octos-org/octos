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
