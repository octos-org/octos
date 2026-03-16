//! Cron CLI subcommands for managing scheduled jobs.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use colored::Colorize;
use octos_bus::cron_types::{CronJob, CronPayload, CronSchedule, CronStore};
use eyre::{Result, WrapErr};

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
        /// Cron expression (e.g. "0 0 9 * * * *").
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

fn save_store(path: &std::path::Path, store: &CronStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(store).wrap_err("failed to serialize cron store")?;
    std::fs::write(path, json).wrap_err("failed to write cron store")?;
    Ok(())
}

fn format_schedule(schedule: &CronSchedule) -> String {
    match schedule {
        CronSchedule::At { .. } => "one-time".into(),
        CronSchedule::Every { every_ms } => format!("every {}s", every_ms / 1000),
        CronSchedule::Cron { expr } => expr.clone(),
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
    let schedule = if let Some(secs) = every {
        CronSchedule::Every {
            every_ms: secs * 1000,
        }
    } else if let Some(expr) = cron {
        CronSchedule::Cron { expr }
    } else if let Some(at_str) = at {
        let dt = chrono::DateTime::parse_from_rfc3339(&at_str)
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(&at_str, "%Y-%m-%dT%H:%M:%S")
                    .map(|naive| naive.and_utc().fixed_offset())
            })
            .wrap_err("invalid timestamp format (use ISO 8601)")?;
        CronSchedule::At {
            at_ms: dt.timestamp_millis(),
        }
    } else {
        eyre::bail!("must specify --every, --cron, or --at");
    };

    let delete_after_run = matches!(schedule, CronSchedule::At { .. });
    let id = short_id();

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
        },
        state: Default::default(),
        created_at_ms: Utc::now().timestamp_millis(),
        delete_after_run,
        timezone: None,
    };
    job.compute_next_run(Utc::now().timestamp_millis());

    let mut store = load_store(store_path);
    store.jobs.push(job);
    save_store(store_path, &store)?;

    println!("{} Added job '{}' ({})", "OK".green(), name, id.cyan());
    Ok(())
}

fn cmd_remove(store_path: &std::path::Path, job_id: &str) -> Result<()> {
    let mut store = load_store(store_path);
    let before = store.jobs.len();
    store.jobs.retain(|j| j.id != job_id);

    if store.jobs.len() < before {
        save_store(store_path, &store)?;
        println!("{} Removed job {}", "OK".green(), job_id.cyan());
    } else {
        println!("{}", format!("Job {job_id} not found.").red());
    }
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
    } else {
        println!("{}", format!("Job {job_id} not found.").red());
    }
    Ok(())
}

fn short_id() -> String {
    let id = uuid::Uuid::now_v7();
    format!("{:x}", id.as_u128())[..8].to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{end}...")
    }
}
