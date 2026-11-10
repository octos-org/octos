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
    /// Consecutive failed delivery attempts for the current occurrence.
    #[serde(default)]
    pub delivery_attempts: u32,
    /// Last queue/delivery error. Cleared when the session queue accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Original scheduled fire time for the in-flight occurrence. Retry
    /// backoff changes next_run_at_ms but must not change the dedupe key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_fire_at_ms: Option<i64>,
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

fn is_cron_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_cron_name(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphabetic())
}

/// Match the cron crate's lexical specifier grammar without allowing its
/// optional whitespace to turn one submitted token into multiple fields.
fn is_cron_specifier(value: &str, allow_any: bool) -> bool {
    if value == "*" || (allow_any && value == "?") || is_cron_number(value) {
        return true;
    }
    let mut range = value.split('-');
    let (Some(start), Some(end), None) = (range.next(), range.next(), range.next()) else {
        return false;
    };
    (is_cron_number(start) && is_cron_number(end)) || (is_cron_name(start) && is_cron_name(end))
}

fn is_cron_root_specifier(value: &str, allow_any: bool) -> bool {
    let mut period = value.split('/');
    let base = period.next().unwrap_or_default();
    let step = period.next();
    if period.next().is_some() {
        return false;
    }
    // A bare named point (MON) cannot have a step; named ranges can.
    let valid_base = is_cron_specifier(base, allow_any) || (step.is_none() && is_cron_name(base));
    valid_base && step.is_none_or(is_cron_number)
}

fn is_cron_field(value: &str, allow_any: bool) -> bool {
    !value.is_empty()
        && value
            .split(',')
            .all(|root| is_cron_root_specifier(root, allow_any))
}

/// Validate a newly submitted cron expression. New jobs use six fields with
/// seconds first, no explicit year, and at least one upcoming occurrence in the
/// requested timezone on or before 2099-12-31. Every whitespace token must be
/// exactly one lexical cron field; the parser must not reinterpret glued input
/// as an extra field.
/// Stored jobs keep their historical `compute_next_run`.
pub fn is_valid_cron_expression(expr: &str, timezone: Option<&str>) -> bool {
    use cron::TimeUnitSpec;

    let fields = expr.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 6
        || !fields
            .iter()
            .enumerate()
            .all(|(index, field)| is_cron_field(field, matches!(index, 3 | 5)))
    {
        return false;
    }
    let Ok(schedule) = expr.parse::<cron::Schedule>() else {
        return false;
    };
    let Ok(timezone) = timezone.unwrap_or("UTC").parse::<chrono_tz::Tz>() else {
        return false;
    };
    schedule.years().is_all()
        && schedule.upcoming(timezone).next().is_some_and(|next| {
            use chrono::Datelike;
            // Product boundary: every timezone supports runs through 2099 only.
            // A later or uncomputable result is not a schedulable new job.
            next.year() <= 2099
        })
}

/// Validate a newly submitted IANA timezone without silently falling back to UTC.
pub fn is_valid_cron_timezone(timezone: &str) -> bool {
    timezone.parse::<chrono_tz::Tz>().is_ok()
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
    fn cron_expression_rejects_glued_or_extra_field_syntax() {
        for expr in [
            "0 0 9 * * **",
            "0 0 9 ** * *",
            "0 0 9 *1 * *",
            "*0 0 9 * * *",
            "0 0 9 * * *1970-2100",
            "0 0 9 * * *2028",
            "0 0 9 * * MON2030",
            "0 0 9 * * 1-52030",
        ] {
            assert!(
                !is_valid_cron_expression(expr, Some("UTC")),
                "must reject non-field syntax: {expr}"
            );
        }
        for expr in [
            "0 0 9 * * *",
            "0 0 0 29 2 *",
            "0 0 9 * JAN-FEB MON,FRI",
            "0 0 0 1 1 1",
            "0 0 0 29 2 MON",
        ] {
            assert!(
                is_valid_cron_expression(expr, Some("UTC")),
                "valid six-field expression regressed: {expr}"
            );
        }
    }

    #[test]
    fn cron_expression_upcoming_check_uses_requested_timezone() {
        let gap_only = "0 30 2 8-13 3 SUN";
        assert!(!is_valid_cron_expression(
            gap_only,
            Some("America/New_York")
        ));
        assert!(is_valid_cron_expression(gap_only, Some("UTC")));
        assert!(is_valid_cron_expression(gap_only, Some("Asia/Shanghai")));

        use chrono::Datelike;
        for (expr, timezone) in [
            ("0 30 2 8-14 3 SUN", "America/New_York"),
            ("0 30 1 25-31 3 SUN", "Europe/London"),
        ] {
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap();
            let next = expr
                .parse::<cron::Schedule>()
                .unwrap()
                .upcoming(tz)
                .next()
                .unwrap();
            assert_eq!(next.year(), 2100, "DST-only result exceeds product limit");
            assert!(
                !is_valid_cron_expression(expr, Some(timezone)),
                "runs after 2099-12-31 must be rejected: {expr} {timezone}"
            );
        }
        for (expr, timezone) in [
            ("0 30 2 1-7 3 SUN", "America/New_York"),
            ("0 30 2 15-21 3 SUN", "America/New_York"),
            ("0 30 1 18-24 3 SUN", "Europe/London"),
            ("0 30 1 1-7 4 SUN", "Europe/London"),
        ] {
            assert!(
                is_valid_cron_expression(expr, Some(timezone)),
                "adjacent valid DST occurrence regressed: {expr} {timezone}"
            );
        }
        for expr in ["0 0 9 * * *", "0 0 0 1 1 *"] {
            for timezone in ["UTC", "America/New_York"] {
                assert!(
                    is_valid_cron_expression(expr, Some(timezone)),
                    "ordinary current-clock daily/New Year schedule regressed: {expr} {timezone}"
                );
            }
        }
    }
}
