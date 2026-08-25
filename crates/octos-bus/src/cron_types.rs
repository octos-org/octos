//! Cron job data types.

use serde::{Deserialize, Serialize};

/// How a cron job is scheduled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CronSchedule {
    /// Fire once at a specific timestamp.
    At { at_ms: i64 },
    /// Fire repeatedly at a fixed interval.
    Every { every_ms: i64 },
    /// Fire on a cron expression schedule (e.g. "0 0 9 * * * *").
    Cron { expr: String },
}

/// What a cron job does when it fires.
///
/// Defaults to [`CronMode::Agent`], which is the behaviour every job had before
/// this existed: the message is pushed onto the bus as an `InboundMessage` and
/// the gateway answers it with a full model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CronMode {
    /// Hand the message to the agent and let it answer. One model turn per fire.
    #[default]
    Agent,
    /// Deliver the message text verbatim. No model, no tokens, no cost.
    ///
    /// For jobs whose whole purpose is to say a fixed thing on a schedule.
    /// A "println hello" reminder firing every 60s does not need a model to
    /// decide what "println hello" means, but before this it got one anyway —
    /// 1440 turns a day for a string the job already knew.
    Notify,
}

/// What a cron job delivers when it fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronPayload {
    pub message: String,
    #[serde(default)]
    pub deliver: bool,
    pub channel: Option<String>,
    pub chat_id: Option<String>,
    /// Whether firing runs a model turn or just delivers `message`.
    ///
    /// `#[serde(default)]` rather than `Option`: a job written before this field
    /// existed loads as `Agent`, which is exactly what it did yesterday. No
    /// migration, and no silent behaviour change on upgrade.
    #[serde(default)]
    pub mode: CronMode,
}

/// What created a cron job.
///
/// Cron jobs are user-level objects with their own lifecycle — one you asked for
/// deliberately should outlive the conversation that created it. But a job
/// created *by* an autonomous loop is different: when the loop is deleted,
/// nothing is left that wants the job, and nothing was recording the connection,
/// so it kept firing. Six such jobs on 5s schedules once produced 55,122
/// executions and 29,490 failed deliveries in a single day, and the only way to
/// find them was to read `cron.json` by hand.
///
/// Every field is optional and skipped when empty, so this is invisible in the
/// stored JSON for jobs that have no origin to record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronOrigin {
    /// Session the creating tool call ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Autonomous loop whose turn created this job, when there was one.
    /// This is the key `loop/delete` reaps by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<String>,
    /// Profile the creating session belonged to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl CronOrigin {
    /// True when nothing was recorded — the shape a hand-made job has.
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.loop_id.is_none() && self.profile_id.is_none()
    }
}

/// Runtime state of a cron job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronJobState {
    pub next_run_at_ms: Option<i64>,
    pub last_run_at_ms: Option<i64>,
    pub last_status: Option<String>,
}

/// A single cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule: CronSchedule,
    pub payload: CronPayload,
    pub state: CronJobState,
    pub created_at_ms: i64,
    pub delete_after_run: bool,
    /// IANA timezone for cron expressions (e.g. "America/Los_Angeles").
    /// If None, defaults to UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// What created this job. Empty for jobs created before this field existed
    /// and for jobs a user made directly.
    #[serde(default, skip_serializing_if = "CronOrigin::is_empty")]
    pub origin: CronOrigin,
}

impl CronJob {
    /// True when this job was created by `loop_id`'s turn.
    ///
    /// Matched on the recorded id, never on `name`. Names are derived from the
    /// message text, so a job called `loop_03_hi` may have nothing to do with
    /// `loop_03` — reaping on a name would delete whatever a user happened to
    /// mention in a message.
    pub fn belongs_to_loop(&self, loop_id: &str) -> bool {
        self.origin.loop_id.as_deref() == Some(loop_id)
    }
}

/// Persistent store format for all cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronStore {
    pub version: u32,
    pub jobs: Vec<CronJob>,
}

impl Default for CronStore {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: Vec::new(),
        }
    }
}

impl CronJob {
    /// Compute the next run time based on schedule and current state.
    pub fn compute_next_run(&mut self, now_ms: i64) {
        match &self.schedule {
            CronSchedule::At { at_ms } => {
                if self.state.last_run_at_ms.is_some() {
                    // Already ran, no next run
                    self.state.next_run_at_ms = None;
                } else {
                    self.state.next_run_at_ms = Some(*at_ms);
                }
            }
            CronSchedule::Every { every_ms } => {
                let base = self.state.last_run_at_ms.unwrap_or(now_ms);
                self.state.next_run_at_ms = Some(base + every_ms);
            }
            CronSchedule::Cron { expr } => {
                use std::str::FromStr;
                if let Ok(schedule) = cron::Schedule::from_str(expr) {
                    let next_ms = if let Some(tz_name) = &self.timezone {
                        if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
                            schedule.upcoming(tz).next().map(|t| t.timestamp_millis())
                        } else {
                            // Invalid timezone, fall back to UTC
                            schedule
                                .upcoming(chrono::Utc)
                                .next()
                                .map(|t| t.timestamp_millis())
                        }
                    } else {
                        schedule
                            .upcoming(chrono::Utc)
                            .next()
                            .map(|t| t.timestamp_millis())
                    };
                    self.state.next_run_at_ms = next_ms;
                } else {
                    self.state.next_run_at_ms = None;
                }
            }
        }
    }

    /// Returns true if this job is due to run.
    pub fn is_due(&self, now_ms: i64) -> bool {
        self.enabled
            && self
                .state
                .next_run_at_ms
                .map(|t| t <= now_ms)
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_every_job(every_ms: i64) -> CronJob {
        CronJob {
            id: "j1".into(),
            name: "test".into(),
            enabled: true,
            schedule: CronSchedule::Every { every_ms },
            payload: CronPayload {
                message: "hi".into(),
                deliver: false,
                channel: None,
                chat_id: None,
                mode: Default::default(),
            },
            state: CronJobState::default(),
            created_at_ms: 1000,
            delete_after_run: false,
            timezone: None,
            origin: Default::default(),
        }
    }

    fn make_at_job(at_ms: i64) -> CronJob {
        CronJob {
            id: "j2".into(),
            name: "once".into(),
            enabled: true,
            schedule: CronSchedule::At { at_ms },
            payload: CronPayload {
                message: "fire!".into(),
                deliver: true,
                channel: Some("telegram".into()),
                chat_id: None,
                mode: Default::default(),
            },
            state: CronJobState::default(),
            created_at_ms: 1000,
            delete_after_run: true,
            origin: Default::default(),
            timezone: None,
        }
    }

    #[test]
    fn test_compute_next_run_every() {
        let mut job = make_every_job(5000);
        job.compute_next_run(10_000);
        assert_eq!(job.state.next_run_at_ms, Some(15_000));

        // After running
        job.state.last_run_at_ms = Some(15_000);
        job.compute_next_run(15_000);
        assert_eq!(job.state.next_run_at_ms, Some(20_000));
    }

    #[test]
    fn test_compute_next_run_at() {
        let mut job = make_at_job(20_000);
        job.compute_next_run(10_000);
        assert_eq!(job.state.next_run_at_ms, Some(20_000));

        // After running once
        job.state.last_run_at_ms = Some(20_000);
        job.compute_next_run(20_000);
        assert_eq!(job.state.next_run_at_ms, None);
    }

    #[test]
    fn test_is_due() {
        let mut job = make_every_job(5000);
        job.state.next_run_at_ms = Some(10_000);

        assert!(!job.is_due(9_999));
        assert!(job.is_due(10_000));
        assert!(job.is_due(11_000));

        job.enabled = false;
        assert!(!job.is_due(11_000));
    }

    #[test]
    fn test_compute_next_run_cron() {
        let mut job = CronJob {
            id: "j3".into(),
            name: "daily".into(),
            enabled: true,
            schedule: CronSchedule::Cron {
                expr: "0 0 9 * * * *".into(),
            },
            payload: CronPayload {
                message: "morning".into(),
                deliver: false,
                channel: None,
                chat_id: None,
                mode: Default::default(),
            },
            state: CronJobState::default(),
            created_at_ms: 1000,
            delete_after_run: false,
            timezone: None,

            origin: Default::default(),
        };
        job.compute_next_run(0);
        assert!(job.state.next_run_at_ms.is_some());
        // Next run should be in the future
        let now_ms = chrono::Utc::now().timestamp_millis();
        assert!(job.state.next_run_at_ms.unwrap() > now_ms - 1000);
    }

    #[test]
    fn test_serde_round_trip() {
        let store = CronStore {
            version: 1,
            jobs: vec![make_every_job(1000), make_at_job(2000)],
        };
        let json = serde_json::to_string(&store).unwrap();
        let restored: CronStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.jobs.len(), 2);
        assert_eq!(restored.version, 1);
    }
}
