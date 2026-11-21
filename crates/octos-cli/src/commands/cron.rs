//! Cron CLI subcommands for managing scheduled jobs.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_bus::cron_types::{CronJob, CronPayload, CronSchedule, CronStore};

use super::Executable;

/// Manage scheduled cron jobs.
#[derive(Debug, Args)]
pub struct CronCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    #[command(subcommand)]
    pub subcommand: CronSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum CronSubcommand {
    /// List scheduled jobs.
    List {
        /// Include disabled jobs.
        #[arg(short, long)]
        all: bool,
    },
    /// Add a new scheduled job.
    Add {
        /// Job name.
        #[arg(short, long)]
        name: String,
        /// Message to send when the job fires.
        #[arg(short, long)]
        message: String,
        /// Run every N seconds (recurring).
        #[arg(long)]
        every: Option<i64>,
        /// Cron expression with seconds first; weekday 0/7=Sun, 1-5=Mon-Fri.
        #[arg(long)]
        cron: Option<String>,
        /// Run once at this ISO timestamp.
        #[arg(long)]
        at: Option<String>,
        /// Deliver response to channel.
        #[arg(short, long)]
        deliver: bool,
        /// Target channel name.
        #[arg(long)]
        channel: Option<String>,
        /// Target chat ID.
        #[arg(long)]
        to: Option<String>,
    },
    /// Remove a scheduled job.
    Remove {
        /// Job ID to remove.
        job_id: String,
    },
    /// Enable or disable a job.
    Enable {
        /// Job ID.
        job_id: String,
        /// Disable instead of enable.
        #[arg(long)]
        disable: bool,
    },
}

impl Executable for CronCommand {
    fn execute(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let store_path = cwd.join(".octos").join("cron.json");

        match self.subcommand {
            CronSubcommand::List { all } => cmd_list(&store_path, all),
            CronSubcommand::Add {
                name,
                message,
                every,
                cron,
                at,
                deliver,
                channel,
                to,
            } => cmd_add(
                &store_path,
                name,
                message,
                every,
                cron,
                at,
                deliver,
                channel,
                to,
            ),
            CronSubcommand::Remove { job_id } => cmd_remove(&store_path, &job_id),
            CronSubcommand::Enable { job_id, disable } => {
                cmd_enable(&store_path, &job_id, !disable)
            }
        }
    }
}

fn load_store(path: &std::path::Path) -> CronStore {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_store(path: &std::path::Path, store: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err("failed to create cron store directory")?;
    }
    let json = serde_json::to_string_pretty(store).wrap_err("failed to serialize cron store")?;
    octos_bus::write_cron_json_atomic(path, &json).wrap_err("failed to write cron store")
}

/// Add/remove preserve untouched job bytes, including omitted defaults and
/// fields written by newer versions. Only an absent store starts empty.
#[derive(serde::Deserialize, serde::Serialize)]
struct EditableCronStore {
    version: u32,
    jobs: Vec<Box<serde_json::value::RawValue>>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl EditableCronStore {
    fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).wrap_err("failed to parse cron store"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: CronStore::default().version,
                jobs: Vec::new(),
                extra: Default::default(),
            }),
            Err(error) => Err(error).wrap_err("failed to read cron store"),
        }
    }

    fn job_ids(&self) -> Result<Vec<String>> {
        self.jobs
            .iter()
            .map(|raw| {
                serde_json::from_str::<CronJob>(raw.get())
                    .map(|job| job.id)
                    .wrap_err("failed to parse stored cron job")
            })
            .collect()
    }
}

fn unused_id(store: &EditableCronStore, mut candidate: String) -> Result<String> {
    let occupied: std::collections::HashSet<_> = store.job_ids()?.into_iter().collect();
    while occupied.contains(&candidate) {
        candidate = short_id();
    }
    Ok(candidate)
}

fn format_schedule(schedule: &CronSchedule) -> String {
    match schedule {
        CronSchedule::At { .. } => "one-time".into(),
        CronSchedule::Every { every_ms } => format!("every {}s", every_ms / 1000),
        CronSchedule::Cron { expr } => octos_bus::cron_expression::display_cron_expression(expr),
    }
}

fn format_next_run(ms: Option<i64>) -> String {
    match ms {
        Some(ts) => {
            let dt = DateTime::<Utc>::from_timestamp_millis(ts);
            dt.map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".into())
        }
        None => "-".into(),
    }
}

fn cmd_list(store_path: &std::path::Path, include_disabled: bool) -> Result<()> {
    let store = load_store(store_path);

    let jobs: Vec<&CronJob> = if include_disabled {
        store.jobs.iter().collect()
    } else {
        store.jobs.iter().filter(|j| j.enabled).collect()
    };

    if jobs.is_empty() {
        println!("No scheduled jobs.");
        return Ok(());
    }

    println!(
        "{:<10} {:<20} {:<22} {:<10} {:<18}",
        "ID".bold(),
        "Name".bold(),
        "Schedule".bold(),
        "Status".bold(),
        "Next Run".bold()
    );
    println!("{}", "-".repeat(80));

    for job in &jobs {
        let status = if job.enabled {
            "enabled".green().to_string()
        } else {
            "disabled".dimmed().to_string()
        };
        println!(
            "{:<10} {:<20} {:<22} {:<10} {:<18}",
            job.id.cyan(),
            truncate(&job.name, 18),
            truncate(&format_schedule(&job.schedule), 20),
            status,
            format_next_run(job.state.next_run_at_ms),
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    store_path: &std::path::Path,
    name: String,
    message: String,
    every: Option<i64>,
    cron: Option<String>,
    at: Option<String>,
    deliver: bool,
    channel: Option<String>,
    to: Option<String>,
) -> Result<()> {
    // Capture a single `now` and reuse it for overflow validation, creation, and
    // scheduling — otherwise a boundary-sized interval can pass a check taken at
    // one instant and then overflow the addition taken a moment later.
    let now_ms = Utc::now().timestamp_millis();

    let schedule = if let Some(secs) = every {
        if secs <= 0 {
            eyre::bail!("--every must be a positive number of seconds (got {secs})");
        }
        let every_ms = secs.checked_mul(1000).ok_or_else(|| {
            eyre::eyre!("--every is too large: {secs} seconds overflows the millisecond interval")
        })?;
        // Use the same checked arithmetic and 2099 product boundary as WS create
        // and subsequent scheduling, with the SAME `now_ms` used below.
        if octos_bus::cron_types::next_every_run_at_ms(now_ms, every_ms).is_none() {
            eyre::bail!(
                "--every is too large: {secs} seconds overflows the next-run timestamp or exceeds 2099-12-31"
            );
        }
        CronSchedule::Every { every_ms }
    } else if let Some(expr) = cron {
        CronSchedule::from_standard_cron(&expr)?
    } else if let Some(at_str) = at {
        let dt = chrono::DateTime::parse_from_rfc3339(&at_str)
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(&at_str, "%Y-%m-%dT%H:%M:%S")
                    .map(|naive| naive.and_utc().fixed_offset())
            })
            .wrap_err("invalid timestamp format (use ISO 8601)")?;
        let at_ms = dt.timestamp_millis();
        // Same 2099-12-31 run-time boundary as WS/agent/Matrix creation.
        if !octos_bus::cron_types::is_supported_cron_run_at_ms(at_ms) {
            eyre::bail!("--at is beyond the supported schedule horizon (2099-12-31): {at_str}");
        }
        CronSchedule::At { at_ms }
    } else {
        eyre::bail!("must specify --every, --cron, or --at");
    };

    let delete_after_run = matches!(schedule, CronSchedule::At { .. });
    let mut store = EditableCronStore::load(store_path)?;
    let id = unused_id(&store, short_id())?;

    let mut job = CronJob {
        id: id.clone(),
        name: name.clone(),
        enabled: true,
        schedule,
        payload: CronPayload {
            message,
            deliver,
            channel,
            chat_id: to,
            mode: Default::default(),
        },
        state: Default::default(),
        created_at_ms: now_ms,
        delete_after_run,
        timezone: None,
        // Made from the CLI by a person, so there is no loop to attribute it to
        // and nothing should ever reap it automatically.
        origin: Default::default(),
    };
    job.compute_first_run(now_ms)?;

    store.jobs.push(serde_json::value::to_raw_value(&job)?);
    save_store(store_path, &store)?;

    println!("{} Added job '{}' ({})", "OK".green(), name, id.cyan());
    Ok(())
}

fn cmd_remove(store_path: &std::path::Path, job_id: &str) -> Result<()> {
    let mut store = EditableCronStore::load(store_path)?;
    let ids = store.job_ids()?;
    let matches: Vec<_> = ids
        .iter()
        .enumerate()
        .filter(|(_, id)| *id == job_id)
        .collect();
    let index = match matches.as_slice() {
        [(index, _)] => *index,
        [] => eyre::bail!(
            "No cron job with id '{job_id}'. Run 'octos cron list' for active ids \
             ('--all' includes disabled ones)."
        ),
        _ => eyre::bail!(
            "ambiguous cron job id '{job_id}': {} stored jobs match; resolve duplicate ids in {} before removing a job (nothing changed)",
            matches.len(),
            store_path.display()
        ),
    };
    store.jobs.remove(index);
    save_store(store_path, &store)?;
    println!("{} Removed job {}", "OK".green(), job_id.cyan());
    Ok(())
}

fn cmd_enable(store_path: &std::path::Path, job_id: &str, enabled: bool) -> Result<()> {
    let mut store = load_store(store_path);
    let now_ms = Utc::now().timestamp_millis();

    if let Some(job) = store.jobs.iter_mut().find(|j| j.id == job_id) {
        job.enabled = enabled;
        if enabled {
            job.compute_next_run(now_ms);
        } else {
            job.state.next_run_at_ms = None;
        }
        let name = job.name.clone();
        save_store(store_path, &store)?;

        let action = if enabled { "Enabled" } else { "Disabled" };
        println!(
            "{} {} job '{}' ({})",
            "OK".green(),
            action,
            name,
            job_id.cyan()
        );
        Ok(())
    } else {
        eyre::bail!(
            "No cron job with id '{job_id}'. Run 'octos cron list' for active ids \
             ('--all' includes disabled ones)."
        );
    }
}

fn short_id() -> String {
    // A v7 prefix is only a timestamp. Use random bits, then check the store.
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[cfg(test)]
#[path = "cron_creation_tests.rs"]
mod creation_tests;

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{end}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = r#"{ "id":"legacy", "name":"old", "enabled":true,
      "schedule":{"kind":"Cron","expr":"0 0 9 * * 2"},
      "payload":{"message":"keep me","channel":null,"chat_id":null},
      "state":{"next_run_at_ms":1800000000000,"last_run_at_ms":null,"last_status":null},
      "created_at_ms":1,"delete_after_run":false,"future_field":{"keep":[1, 2]} }"#;

    fn add(path: &std::path::Path, name: &str) {
        cmd_add(
            path,
            name.into(),
            name.into(),
            Some(60),
            None,
            None,
            false,
            None,
            None,
        )
        .unwrap();
    }

    #[test]
    fn should_create_distinct_ids_and_remove_only_one_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        for name in ["first", "second", "third", "fourth"] {
            add(&path, name);
        }
        let before = load_store(&path);
        let ids: std::collections::HashSet<_> = before.jobs.iter().map(|j| &j.id).collect();
        assert_eq!(ids.len(), 4, "consecutive creations must not share an id");
        cmd_remove(&path, &before.jobs[1].id).unwrap();
        let after = load_store(&path);
        let kept: Vec<_> = before
            .jobs
            .into_iter()
            .filter(|j| j.name != "second")
            .collect();
        assert_eq!(
            serde_json::to_value(after.jobs).unwrap(),
            serde_json::to_value(kept).unwrap()
        );
    }

    #[test]
    fn should_refuse_ambiguous_historical_ids_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let contents = format!("{{\"version\":1,\"jobs\":[{LEGACY},{LEGACY}]}}");
        std::fs::write(&path, &contents).unwrap();
        let error = cmd_remove(&path, "legacy").unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(cmd_remove(&path, "missing").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn should_preserve_other_jobs_raw_bytes_across_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        std::fs::write(&path, format!("{{\"version\":1,\"jobs\":[{LEGACY}]}}")).unwrap();
        add(&path, "new");
        let after_add = std::fs::read_to_string(&path).unwrap();
        assert!(
            after_add.contains(LEGACY),
            "add rewrote an unrelated legacy job"
        );
        let new_id = load_store(&path).jobs[1].id.clone();
        cmd_remove(&path, &new_id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains(LEGACY));
        assert_eq!(load_store(&path).jobs.len(), 1);
    }

    #[test]
    fn should_retry_an_occupied_id_and_refuse_unreadable_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let bytes = format!("{{\"version\":1,\"jobs\":[{LEGACY}]}}");
        std::fs::write(&path, &bytes).unwrap();
        let store = EditableCronStore::load(&path).unwrap();
        let chosen = unused_id(&store, "legacy".into()).unwrap();
        assert_ne!(chosen, "legacy", "occupied candidate must be regenerated");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        std::fs::write(&path, "{broken").unwrap();
        assert!(
            cmd_add(
                &path,
                "new".into(),
                "new".into(),
                Some(60),
                None,
                None,
                false,
                None,
                None
            )
            .is_err()
        );
        assert!(cmd_remove(&path, "legacy").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        assert!(EditableCronStore::load(dir.path()).is_err());
    }
}
