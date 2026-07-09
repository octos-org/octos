use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkillActionJobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Abandoned,
}

impl SkillActionJobStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct SkillActionJobStore {
    profile_data_dir: PathBuf,
}

impl SkillActionJobStore {
    pub(crate) fn open(profile_data_dir: impl AsRef<Path>) -> Self {
        Self {
            profile_data_dir: profile_data_dir.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn append(&self, job: &SkillActionJobRecord) -> Result<()> {
        let path = self.session_path(&job.session_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create job store dir {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .wrap_err_with(|| format!("failed to open job store {}", path.display()))?;
        serde_json::to_writer(&mut file, job)
            .wrap_err_with(|| format!("failed to serialize job {}", job.job_id))?;
        file.write_all(b"\n")
            .wrap_err_with(|| format!("failed to append job {}", job.job_id))?;
        Ok(())
    }

    pub(crate) fn list(&self, session_id: &SessionKey) -> Result<Vec<SkillActionJobRecord>> {
        let mut jobs = latest_by_job_id(self.read_session_snapshots(session_id)?);
        jobs.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        Ok(jobs)
    }

    pub(crate) fn read(
        &self,
        session_id: &SessionKey,
        job_id: &str,
    ) -> Result<Option<SkillActionJobRecord>> {
        Ok(self
            .list(session_id)?
            .into_iter()
            .find(|job| job.job_id == job_id))
    }

    pub(crate) fn mark_active_jobs_abandoned(&self) -> Result<usize> {
        let jobs_dir = self.jobs_dir();
        if !jobs_dir.exists() {
            return Ok(0);
        }

        let mut count = 0;
        for entry in fs::read_dir(&jobs_dir)
            .wrap_err_with(|| format!("failed to read job store dir {}", jobs_dir.display()))?
        {
            let entry = entry.wrap_err("failed to read job store entry")?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let snapshots = self.read_snapshots_from_path(&path)?;
            for mut job in latest_by_job_id(snapshots) {
                if !job.status.is_active() {
                    continue;
                }
                job.status = SkillActionJobStatus::Abandoned;
                job.error = Some("job abandoned after server restart".to_string());
                job.updated_at = Utc::now();
                self.append(&job)?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn jobs_dir(&self) -> PathBuf {
        self.profile_data_dir.join("skill-action-jobs")
    }

    fn session_path(&self, session_id: &SessionKey) -> PathBuf {
        self.jobs_dir().join(format!(
            "{}.jsonl",
            octos_bus::session::encode_path_component(&session_id.0)
        ))
    }

    fn read_session_snapshots(&self, session_id: &SessionKey) -> Result<Vec<SkillActionJobRecord>> {
        self.read_snapshots_from_path(&self.session_path(session_id))
    }

    #[cfg(test)]
    pub(crate) fn read_session_snapshots_for_test(
        &self,
        session_id: &SessionKey,
    ) -> Result<Vec<SkillActionJobRecord>> {
        self.read_session_snapshots(session_id)
    }

    fn read_snapshots_from_path(&self, path: &Path) -> Result<Vec<SkillActionJobRecord>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to open job store {}", path.display()));
            }
        };
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line.wrap_err_with(|| {
                format!(
                    "failed to read job store line {} in {}",
                    index + 1,
                    path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let record: SkillActionJobRecord = serde_json::from_str(&line).wrap_err_with(|| {
                format!(
                    "failed to parse job store line {} in {}",
                    index + 1,
                    path.display()
                )
            })?;
            records.push(record);
        }
        Ok(records)
    }
}

fn latest_by_job_id(snapshots: Vec<SkillActionJobRecord>) -> Vec<SkillActionJobRecord> {
    let mut latest = HashMap::<String, SkillActionJobRecord>::new();
    for snapshot in snapshots {
        latest.insert(snapshot.job_id.clone(), snapshot);
    }
    latest.into_values().collect()
}

pub(crate) fn recover_skill_action_jobs_for_profile_start(
    profile_id: &str,
    profile_data_dir: impl AsRef<Path>,
) -> Result<usize> {
    let store = SkillActionJobStore::open(profile_data_dir);
    let abandoned = store.mark_active_jobs_abandoned()?;
    if abandoned > 0 {
        tracing::info!(
            profile_id,
            abandoned_jobs = abandoned,
            "recovered active skill action jobs after profile start"
        );
    }
    Ok(abandoned)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use octos_core::SessionKey;
    use serde_json::json;

    use super::*;

    fn record(
        session_id: &SessionKey,
        job_id: &str,
        status: SkillActionJobStatus,
        updated_offset_secs: i64,
    ) -> SkillActionJobRecord {
        let now = Utc::now();
        SkillActionJobRecord {
            job_id: job_id.to_string(),
            batch_id: "batch-a".to_string(),
            profile_id: "alan0x".to_string(),
            session_id: session_id.clone(),
            action_id: "source.import".to_string(),
            skill_id: "mofa-notebook-source".to_string(),
            status,
            input_path: Some("up://report.md".to_string()),
            filename: Some("report.md".to_string()),
            materialized_path: Some("uploads/report.md".to_string()),
            output: None,
            error: None,
            result: None,
            source_id: None,
            source_path: None,
            metadata_path: None,
            created_at: now,
            updated_at: now + Duration::seconds(updated_offset_secs),
        }
    }

    #[test]
    fn should_list_latest_jobs_when_snapshots_are_appended() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillActionJobStore::open(dir.path());
        let session_id = SessionKey("local:test#topic/unsafe".to_string());

        store
            .append(&record(
                &session_id,
                "job-a",
                SkillActionJobStatus::Queued,
                1,
            ))
            .unwrap();
        store
            .append(&record(
                &session_id,
                "job-b",
                SkillActionJobStatus::Failed,
                2,
            ))
            .unwrap();
        store
            .append(&record(
                &session_id,
                "job-a",
                SkillActionJobStatus::Running,
                3,
            ))
            .unwrap();

        let jobs = store.list(&session_id).unwrap();

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].job_id, "job-b");
        assert_eq!(jobs[0].status, SkillActionJobStatus::Failed);
        assert_eq!(jobs[1].job_id, "job-a");
        assert_eq!(jobs[1].status, SkillActionJobStatus::Running);
        assert_eq!(
            store.session_path(&session_id),
            dir.path()
                .join("skill-action-jobs")
                .join("local%3Atest%23topic%2Funsafe.jsonl")
        );
    }

    #[test]
    fn should_read_latest_job_when_job_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillActionJobStore::open(dir.path());
        let session_id = SessionKey("local:test".to_string());
        let mut queued = record(&session_id, "job-a", SkillActionJobStatus::Queued, 1);
        queued.output = Some("queued".to_string());
        store.append(&queued).unwrap();

        let mut succeeded = record(&session_id, "job-a", SkillActionJobStatus::Succeeded, 2);
        succeeded.output = Some("done".to_string());
        succeeded.result = Some(json!({"source": {"id": "src-1"}}));
        succeeded.source_id = Some("src-1".to_string());
        store.append(&succeeded).unwrap();

        let job = store.read(&session_id, "job-a").unwrap().unwrap();

        assert_eq!(job.status, SkillActionJobStatus::Succeeded);
        assert_eq!(job.output.as_deref(), Some("done"));
        assert_eq!(job.source_id.as_deref(), Some("src-1"));
        assert!(store.read(&session_id, "missing").unwrap().is_none());
    }

    #[test]
    fn should_mark_active_jobs_abandoned_when_recovering_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillActionJobStore::open(dir.path());
        let session_a = SessionKey("local:a".to_string());
        let session_b = SessionKey("local:b".to_string());

        store
            .append(&record(
                &session_a,
                "queued",
                SkillActionJobStatus::Queued,
                1,
            ))
            .unwrap();
        store
            .append(&record(
                &session_a,
                "running",
                SkillActionJobStatus::Running,
                2,
            ))
            .unwrap();
        store
            .append(&record(
                &session_a,
                "succeeded",
                SkillActionJobStatus::Succeeded,
                3,
            ))
            .unwrap();
        store
            .append(&record(
                &session_b,
                "failed",
                SkillActionJobStatus::Failed,
                4,
            ))
            .unwrap();

        let abandoned = store.mark_active_jobs_abandoned().unwrap();

        assert_eq!(abandoned, 2);
        assert_eq!(
            store.read(&session_a, "queued").unwrap().unwrap().status,
            SkillActionJobStatus::Abandoned
        );
        assert_eq!(
            store.read(&session_a, "running").unwrap().unwrap().status,
            SkillActionJobStatus::Abandoned
        );
        assert_eq!(
            store.read(&session_a, "succeeded").unwrap().unwrap().status,
            SkillActionJobStatus::Succeeded
        );
        assert_eq!(
            store.read(&session_b, "failed").unwrap().unwrap().status,
            SkillActionJobStatus::Failed
        );
    }

    #[test]
    fn should_recover_active_jobs_when_profile_starts() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillActionJobStore::open(dir.path());
        let session_id = SessionKey("local:profile-start".to_string());

        store
            .append(&record(
                &session_id,
                "job-queued",
                SkillActionJobStatus::Queued,
                1,
            ))
            .unwrap();
        store
            .append(&record(
                &session_id,
                "job-running",
                SkillActionJobStatus::Running,
                2,
            ))
            .unwrap();
        store
            .append(&record(
                &session_id,
                "job-finished",
                SkillActionJobStatus::Succeeded,
                3,
            ))
            .unwrap();

        let recovered = recover_skill_action_jobs_for_profile_start("alan0x", dir.path()).unwrap();

        assert_eq!(recovered, 2);
        let jobs = store.list(&session_id).unwrap();
        assert_eq!(
            jobs.iter()
                .filter(|job| job.status == SkillActionJobStatus::Abandoned)
                .count(),
            2
        );
        assert!(jobs.iter().any(|job| {
            job.job_id == "job-finished" && job.status == SkillActionJobStatus::Succeeded
        }));
    }
}
