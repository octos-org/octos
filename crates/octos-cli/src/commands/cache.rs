//! `octos cache`: inspect and reclaim the build-cache pool
//! (design §5/§6; outer-loop #5).
//!
//! * `status` — every repository pool under `<data_dir>/build-cache/`, its
//!   slots with holder / `last_used` / on-disk `target/` size, plus the
//!   remaining free space of the pool root's filesystem. Read-only.
//! * `gc` — DEFAULT REPORTS ONLY. `--apply` is the only path that deletes,
//!   and it only ever deletes `target/` trees of unheld slots whose
//!   `last_used` is past the stale window (never `.lock`, never by mtime —
//!   both invariants live in the pool core this command calls).
//! * `gate` — non-zero exit when free space is below the threshold, and
//!   fail-closed (non-zero) when the measurement itself fails.
//!
//! Thresholds default from the `[build_cache]` section of the resolved
//! config (`stale_hours`, `min_free_gb`); flags override per invocation and
//! never write the config back.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, WrapErr};

use super::Executable;
use crate::build_cache::pool::{
    self, BuildCacheConfig, BuildCacheError, GcPolicy, HolderInfo, HolderMeta, ReclaimOutcome,
    ReclaimReport, SlotKind, SlotOutcome,
};

/// `octos cache` — inspect and reclaim the build-cache pool.
#[derive(Debug, Args)]
pub struct CacheCommand {
    #[command(subcommand)]
    action: Option<CacheAction>,

    /// Data directory hosting the pool root (`<data_dir>/build-cache`).
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Working directory (for project-local config resolution).
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum CacheAction {
    /// List every repository pool, its slots, and remaining free space.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reclaim stale unheld slots (DEFAULT: report only; pass --apply to delete).
    Gc {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Override `[build_cache] stale_hours` for this run.
        #[arg(long, value_name = "N")]
        stale_hours: Option<u64>,
        /// Actually delete stale `target/` trees (report-only without it).
        #[arg(long)]
        apply: bool,
    },
    /// Exit non-zero when free space is below the threshold.
    Gate {
        /// Emit machine-readable JSON (the measurement itself).
        #[arg(long)]
        json: bool,
        /// Override `[build_cache] min_free_gb` for this run.
        #[arg(long, value_name = "N")]
        min_free_gb: Option<u64>,
    },
    /// Acquire an outer-loop verification slot and print it in a
    /// script-parseable form (see `octoloop` / OLP_OUTER_BOOT).
    Acquire {
        /// Emit machine-readable JSON instead of the SLOT/TARGET/RELEASE
        /// lines.
        #[arg(long)]
        json: bool,
        /// The MAIN repository the verification will build (repo-key
        /// derivation input — the pool is shared per repository).
        #[arg(long, value_name = "DIR")]
        repo: PathBuf,
        /// Slot namespace. Only `verify` is exposed on the CLI today:
        /// peer slots are acquired internally by serve and must not be
        /// taken by hand (§1.3 namespaces).
        #[arg(long, default_value = "verify")]
        purpose: CacheAcquirePurpose,
        /// Free-form label recorded in `holder.json` (shown by
        /// `octos cache status`).
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// The pid whose lifetime the slot must span (#6 truth model:
        /// `holder.json` + pid liveness is what keeps a CLI-acquired slot
        /// held between processes). Defaults to the PARENT of this CLI
        /// process, i.e. the shell/loop invoking `octos cache acquire`.
        #[arg(long, value_name = "PID")]
        pid: Option<u32>,
    },
    /// Release a verification slot acquired by `octos cache acquire`.
    Release {
        /// The slot directory exactly as printed by `acquire`.
        #[arg(long, value_name = "DIR")]
        slot: PathBuf,
        /// Acquisition identity printed in the RELEASE command.
        #[arg(long, value_name = "TOKEN")]
        token: String,
    },
}

/// The one purpose the CLI may acquire (`octos cache acquire` is the
/// outer-loop entry, §1.3); keeps clap from accepting `--purpose peer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum CacheAcquirePurpose {
    Verify,
}

impl Executable for CacheCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            Some(CacheAction::Status { json }) => self.run_status(json),
            Some(CacheAction::Gc {
                json,
                stale_hours,
                apply,
            }) => self.run_gc(json, stale_hours, apply),
            Some(CacheAction::Gate { json, min_free_gb }) => self.run_gate(json, min_free_gb),
            Some(CacheAction::Acquire {
                json,
                ref repo,
                ref note,
                ref pid,
                ..
            }) => self.run_acquire(json, repo, note.clone(), *pid),
            Some(CacheAction::Release {
                ref slot,
                ref token,
            }) => self.run_release(slot, token),
            // Bare `octos cache`: same "no wizard" posture as `octos config`
            // — print a read-only overview pointing at the subcommands.
            None => {
                println!(
                    "octos cache — inspect and reclaim the build-cache pool (read-only overview)
  octos cache status   list pools, slots, holders, sizes, free space
  octos cache gc       report stale unheld slots (--apply to actually delete)
  octos cache gate     exit non-zero when free space is below the threshold"
                );
                Ok(())
            }
        }
    }
}

impl CacheCommand {
    /// Whether any subcommand was asked for JSON output — drives stdout
    /// reservation (tracing logs must never corrupt a JSON stream).
    pub(crate) fn emits_json(&self) -> bool {
        match &self.action {
            Some(CacheAction::Status { json })
            | Some(CacheAction::Gc { json, .. })
            | Some(CacheAction::Gate { json, .. })
            | Some(CacheAction::Acquire { json, .. }) => *json,
            // `release` prints exactly one human line; parseable text, not
            // JSON, is its contract.
            Some(CacheAction::Release { .. }) => false,
            None => false,
        }
    }

    /// Resolve the config the same way every other command does, then keep
    /// only the `[build_cache]` section (defaults when absent).
    fn build_cache_config(&self) -> Result<BuildCacheConfig> {
        let cwd = self
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let config = crate::config::Config::load_with_context(&cwd, &ctx)
            .wrap_err("failed to load config for [build_cache]")?;
        Ok(config.build_cache.clone().unwrap_or_default())
    }

    /// The pool root: `<data_dir>/build-cache`, the same derivation every
    /// acquire/release site uses (`crate::peers::build_cache_peer::pool_root`).
    fn pool_root(&self) -> Result<PathBuf> {
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        Ok(ctx.data_dir.join("build-cache"))
    }

    // ------------------------------------------------------------------
    // status
    // ------------------------------------------------------------------

    fn run_status(&self, json: bool) -> Result<()> {
        let config = self.build_cache_config()?;
        let pool_root = self.pool_root()?;
        let free = measure_free_space(&pool_root);
        let repos = collect_pool_status(&pool_root, &config);

        if json {
            let payload = StatusJson::build(pool_root, free, repos);
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }

        println!("{}", "octos cache status".cyan().bold());
        println!(
            "{} {}",
            "Pool root".dimmed(),
            payload_root_display(&pool_root)
        );
        match free {
            Some(bytes) => println!("{} {}", "Free space".dimmed(), format_gib(bytes)),
            None => println!(
                "{} {}",
                "Free space".dimmed(),
                "unknown (measurement failed)".yellow()
            ),
        }
        println!();
        if pool_root_status_empty(&pool_root) {
            println!("{}", "No build-cache pools yet.".dimmed());
            return Ok(());
        }
        for repo in &collect_pool_status(&pool_root, &config) {
            println!("{} {}", "repo".green(), repo.repo_key.bold());
            if repo.slots.is_empty() {
                println!("  {}", "(no slots)".dimmed());
            }
            for slot in &repo.slots {
                let holder = match &slot.holder {
                    Some(meta) => render_holder(meta),
                    None => "free".dimmed().to_string(),
                };
                let label = match slot.kind {
                    SlotKind::Peer => "peer",
                    SlotKind::Verify => "verify",
                };
                println!(
                    "  {:<12} {:<18} last_used {}  {}",
                    label,
                    holder,
                    render_last_used(slot.last_used),
                    format_bytes(slot.target_bytes),
                );
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // gc
    // ------------------------------------------------------------------

    fn run_gc(&self, json: bool, stale_hours: Option<u64>, apply: bool) -> Result<()> {
        let config = self.build_cache_config()?;
        let stale_hours = resolve_stale_hours(stale_hours, config.stale_hours)?;
        let policy = GcPolicy { stale_hours, apply };
        let pool_root = self.pool_root()?;
        let reports = pool::reclaim_stale(&pool_root, &policy, &config)
            .wrap_err("build-cache gc walk failed")?;

        if json {
            let payload = GcJson::build(pool_root, policy.stale_hours, apply, &reports);
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }

        println!("{}", "octos cache gc".cyan().bold());
        println!(
            "{} {}h{}",
            "Stale window".dimmed(),
            policy.stale_hours,
            if apply {
                "  (apply)"
            } else {
                "  (report only)"
            }
            .yellow()
        );
        println!();
        if reports.is_empty() {
            println!("{}", "No build-cache slots found.".dimmed());
            return Ok(());
        }
        let mut reclaimed = 0u64;
        let mut reclaimed_count = 0usize;
        let mut would_free = 0u64;
        let mut stale_count = 0usize;
        for row in &reports {
            let mark = match row.outcome {
                ReclaimOutcome::Reclaimed => {
                    reclaimed += row.freed_bytes;
                    reclaimed_count += 1;
                    "RECLAIMED".red()
                }
                ReclaimOutcome::Stale => {
                    would_free += row.would_free_bytes;
                    stale_count += 1;
                    format!("WOULD-FREE {}", format_bytes(row.would_free_bytes)).yellow()
                }
                ReclaimOutcome::Locked => "locked".dimmed(),
                ReclaimOutcome::HolderCleared => "holder_cleared".yellow(),
                ReclaimOutcome::Fresh => "fresh".dimmed(),
                ReclaimOutcome::NoLock => "no_lock".yellow(),
                ReclaimOutcome::Skipped => "skipped".yellow(),
            };
            let rel = pool_root
                .join(&row.slot_path)
                .strip_prefix(&pool_root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| row.slot_path.display().to_string());
            println!("  {:<32} {}", mark, rel);
        }
        println!();
        if apply {
            println!(
                "{} {} slot(s), {} freed",
                "Reclaimed".green(),
                reclaimed_count,
                format_bytes(reclaimed)
            );
        } else {
            if stale_count > 0 {
                println!(
                    "{} {} stale slot(s), {} reclaimable",
                    "Would free".green(),
                    stale_count,
                    format_bytes(would_free)
                );
            }
            println!(
                "{}",
                "Report only — nothing was deleted. Pass --apply to reclaim stale slots.".yellow()
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // gate
    // ------------------------------------------------------------------

    fn run_gate(&self, json: bool, min_free_gb: Option<u64>) -> Result<()> {
        let config = self.build_cache_config()?;
        let min_free_gb = min_free_gb.unwrap_or(config.min_free_gb);
        let pool_root = self.pool_root()?;

        // D5 premise, same as acquire: a fresh install has no pool root yet,
        // and statvfs needs an existing path. Creating the (empty) root is
        // not an allocation — the refusal-or-pass decision is still made by
        // the pool core's fail-closed gate below.
        std::fs::create_dir_all(&pool_root).wrap_err_with(|| {
            format!("gate: failed to create pool root {}", pool_root.display())
        })?;

        // Fail-closed is decided by the SAME pool-core helper acquire uses
        // (`check_free_space` → `space_gate`): below threshold or
        // measurement failure are both refusals. Render + exit from the
        // typed error so gate can never drift from the acquire path.
        match pool::check_free_space(&pool_root, min_free_gb) {
            Ok(()) => {
                let available = measure_free_space(&pool_root);
                if json {
                    let payload = GateJson::passed_with(min_free_gb, available);
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    println!(
                        "{} free space {} ≥ {} GB",
                        "ok".green(),
                        available
                            .map(|b| format!("{} ({})", format_gib(b), b / GIB))
                            .unwrap_or_else(|| "unknown".to_string()),
                        min_free_gb
                    );
                }
                Ok(())
            }
            Err(err) => {
                if json {
                    // D5: available bytes come straight from the typed
                    // error (measured u64), never re-derived from rounded GB.
                    let payload = GateJson::build(min_free_gb, &Err(&err));
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                // The refusal reason goes to stderr so stdout stays clean
                // for either rendering mode.
                eprintln!("{}", "gate failed".red().bold());
                eprintln!("{err}");
                std::process::exit(2);
            }
        }
    }

    // ------------------------------------------------------------------
    // acquire / release (#6, outer-loop entry)
    // ------------------------------------------------------------------

    /// `octos cache acquire --purpose verify --repo <dir>`
    ///
    /// Output contract (STABLE, script-parsed by OLP_OUTER_BOOT): the
    /// first three stdout lines are exactly
    ///
    /// ```text
    /// SLOT <slot dir>
    /// TARGET <slot dir>/target
    /// RELEASE octos cache release --slot <slot dir> --token <claim token>
    /// ```
    ///
    /// with `--json` carrying the same three fields under those names.
    /// Anything else (human hints) goes to stderr or below the marker
    /// lines, never before or between them.
    fn run_acquire(
        &self,
        json: bool,
        repo: &Path,
        note: Option<String>,
        pid: Option<u32>,
    ) -> Result<()> {
        let config = self.build_cache_config()?;
        let pool_root = self.pool_root()?;
        let repo_key = crate::build_cache::repo_key_for_path(repo).ok_or_else(|| {
            eyre::eyre!(
                "cannot derive a build-cache repo key from {} (canonicalization failed — does \
                 the directory exist?)",
                repo.display()
            )
        })?;

        // Truth model (#6, docs/build-cache-pool.md §3.5 arm 2): the slot
        // must stay held between THIS process's exit and a later
        // `octos cache release`, so the pid recorded in holder.json is the
        // one whose lifetime the verification spans. Default to the
        // parent: the invoking shell / outer-loop driver outlives this
        // one-shot CLI. (rustix `getppid` — std has no getppid; `None`
        // means the parent already exited, in which case the slot would
        // be immediately reap-able, so fall back to OUR pid and say so.)
        let pid = pid.or_else(parent_pid).unwrap_or_else(std::process::id);

        let holder = HolderInfo {
            purpose_note: Some(note.unwrap_or_else(|| "verify".to_owned())),
            pid_override: Some(pid),
            ..HolderInfo::default()
        };
        let slot = pool::acquire_detached(&pool_root, &repo_key, &config, &holder)
            .wrap_err("acquire: could not allocate a verify slot")?;

        let release_cmd = release_command(&slot);
        if json {
            let payload = AcquireJson {
                slot: &slot.path,
                target: &slot.target_dir,
                release: release_cmd.as_str(),
                pid,
            };
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("SLOT {}", slot.path.display());
            println!("TARGET {}", slot.target_dir.display());
            println!("RELEASE {release_cmd}");
            eprintln!(
                "{} export CARGO_TARGET_DIR={} && export CARGO_INCREMENTAL=0",
                "next".dimmed(),
                slot.target_dir.display()
            );
            eprintln!(
                "{} the slot is held in this pool until pid {pid} exits or the RELEASE command \
                 runs (whichever first); gc skips live holders",
                "note".dimmed()
            );
        }
        Ok(())
    }

    /// `octos cache release --slot <dir>` — idempotent (outcome
    /// `Completed`), refuses paths outside the pool root or missing the
    /// slot's `.lock` (see `pool::release_detached`).
    fn run_release(&self, slot: &Path, token: &str) -> Result<()> {
        let pool_root = self.pool_root()?;
        match pool::release_detached(pool_root.as_path(), slot, token, SlotOutcome::Completed) {
            Ok(disposition) => {
                let label = match disposition {
                    pool::ReleaseDisposition::Released => "released",
                    pool::ReleaseDisposition::AlreadyReleased => "already released",
                    pool::ReleaseDisposition::ClaimMismatch => "claim mismatch; unchanged",
                };
                println!("{label} {}", slot.display());
                Ok(())
            }
            Err(BuildCacheError::SlotNotFound { .. }) => {
                // An idempotent double release is a no-op success; a path
                // that never was a slot is surfaced distinctly but still
                // exits non-zero — scripts must not mistake it for done.
                eprintln!("{}", "release: no such slot".red().bold());
                eprintln!("  {}", slot.display());
                eprintln!(
                    "  {}",
                    "the pool never created this slot (missing .lock); check `octos cache status`"
                        .dimmed()
                );
                std::process::exit(3);
            }
            Err(err) => Err(eyre::eyre!(err))
                .wrap_err_with(|| format!("release: could not release {}", slot.display())),
        }
    }
}

fn release_command(slot: &pool::DetachedSlot) -> String {
    format!(
        "octos cache release --slot {} --token {}",
        shell_quoted(&slot.path),
        slot.claim_token
    )
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Resolve the gc stale window: flag over config, with the same `>= 1`
/// floor the config layer enforces (D4) — a 0 window under `--apply` would
/// reclaim every slot last touched, so it is rejected, not silently run.
fn resolve_stale_hours(flag: Option<u64>, config_hours: u64) -> Result<u64> {
    let hours = match flag {
        Some(0) => {
            return Err(eyre::eyre!(
                "`--stale-hours 0` is not allowed — the stale window must be at least 1 hour \
             (same floor as `[build_cache] stale_hours`)"
            ));
        }
        Some(hours) => hours,
        None => config_hours.max(1),
    };
    pool::stale_window_secs(hours)?;
    Ok(hours)
}

/// `fs2::available_space` on the pool root, creating it first when missing
/// (statvfs needs an existing path; a fresh install has no pool yet).
fn measure_free_space(pool_root: &Path) -> Option<u64> {
    if !pool_root.exists() {
        std::fs::create_dir_all(pool_root).ok()?;
    }
    fs2::available_space(pool_root).ok()
}

/// Read-only snapshot of one slot row for `status`.
#[derive(Debug, Clone, serde::Serialize)]
struct SlotStatus {
    /// Namespace of the slot dir (`peer` / `verify`).
    kind: SlotKind,
    /// Slot directory path (`<pool-root>/<repo-key>/slot-N`).
    path: PathBuf,
    /// Holder metadata when `holder.json` exists and parses.
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<HolderMeta>,
    /// `last_used` unix seconds (`0` when missing/unparsable).
    last_used: u64,
    /// Best-effort on-disk size of the slot's `target/` tree.
    target_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct RepoStatus {
    /// The 12-hex repository key (pool directory name).
    repo_key: String,
    slots: Vec<SlotStatus>,
}

#[derive(Debug, serde::Serialize)]
struct FreeJson {
    available_bytes: u64,
    available_gb: u64,
}

#[derive(Debug, serde::Serialize)]
struct StatusJson {
    pool_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    free: Option<FreeJson>,
    repos: Vec<RepoStatus>,
}

impl StatusJson {
    /// The exact payload `octos cache status --json` prints (D3: run path
    /// and tests share one constructor, so the field mapping itself is
    /// under test).
    fn build(pool_root: PathBuf, free: Option<u64>, repos: Vec<RepoStatus>) -> Self {
        Self {
            pool_root,
            free: free.map(|bytes| FreeJson {
                available_bytes: bytes,
                available_gb: bytes / GIB,
            }),
            repos,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct GcSlotJson<'a> {
    slot_path: &'a Path,
    outcome: &'a str,
    /// Bytes actually freed (Reclaimed rows only).
    freed_bytes: u64,
    /// Bytes a subsequent `--apply` would free right now. Equals
    /// `freed_bytes` for reclaimed rows; carries the reclaimable size for
    /// `stale` rows so a report-only run still says WHAT would go (D1).
    would_free_bytes: u64,
}

#[derive(Debug, serde::Serialize)]
struct GcJson<'a> {
    pool_root: PathBuf,
    stale_hours: u64,
    apply: bool,
    reclaimed_bytes: u64,
    /// Total bytes `--apply` would free in this state (D1): meaningful in
    /// report mode, equal to `reclaimed_bytes` under `--apply`.
    would_free_bytes: u64,
    slots: Vec<GcSlotJson<'a>>,
}

impl<'a> GcJson<'a> {
    /// The exact payload `octos cache gc --json` prints (D3).
    fn build(
        pool_root: PathBuf,
        stale_hours: u64,
        apply: bool,
        reports: &'a [ReclaimReport],
    ) -> Self {
        Self {
            pool_root,
            stale_hours,
            apply,
            reclaimed_bytes: reports.iter().map(|r| r.freed_bytes).sum::<u64>(),
            would_free_bytes: reports.iter().map(|r| r.would_free_bytes).sum::<u64>(),
            slots: reports
                .iter()
                .map(|r| GcSlotJson {
                    slot_path: &r.slot_path,
                    outcome: r.outcome.as_str(),
                    freed_bytes: r.freed_bytes,
                    would_free_bytes: r.would_free_bytes,
                })
                .collect(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct AcquireJson<'a> {
    slot: &'a Path,
    target: &'a Path,
    release: &'a str,
    /// The pid recorded in `holder.json` — whose liveness keeps the slot
    /// held (#6 truth model). Mirrored here so scripts can assert the
    /// holder they expect.
    pid: u32,
}

/// Parent pid of this one-shot CLI (the invoking shell / outer-loop driver),
/// used as the default holder pid for `octos cache acquire`. Unix only:
/// std has no `getppid`, and `rustix` is a unix-only dependency; on other
/// platforms the caller falls back to its own pid (or passes `--pid`).
#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    rustix::process::getppid().and_then(|p| u32::try_from(p.as_raw_nonzero().get()).ok())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

/// Shell-quote a path for the RELEASE line so paths with spaces stay one
/// argument. POSIX-single-quote style: everything is literal between the
/// quotes, an embedded quote closes/reopens with an escape.
fn shell_quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~'))
    {
        return text.into_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[derive(Debug, serde::Serialize)]
struct GateJson {
    passed: bool,
    min_free_gb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    available_gb: Option<u64>,
}

impl GateJson {
    /// The exact payload `octos cache gate --json` prints (D3). The fail
    /// arm mirrors run_gate's error mapping: the raw measured bytes come
    /// from the typed `FreeSpaceLow`, never re-derived from rounded GB.
    fn build(min_free_gb: u64, outcome: &Result<&BuildCacheError, &BuildCacheError>) -> Self {
        match outcome {
            // `Ok` carries no measurement here — the pass arm is built by
            // `passed_with` after (re)measuring. Only the mapping from a
            // typed refusal to json fields lives in this fn.
            Ok(_) => Self {
                passed: true,
                min_free_gb,
                available_bytes: None,
                available_gb: None,
            },
            Err(BuildCacheError::FreeSpaceLow {
                available_bytes, ..
            }) => Self {
                passed: false,
                min_free_gb,
                available_bytes: Some(*available_bytes),
                available_gb: Some(available_bytes / GIB),
            },
            Err(_) => Self {
                passed: false,
                min_free_gb,
                available_bytes: None,
                available_gb: None,
            },
        }
    }

    /// Pass-arm payload with the (re)measured free bytes.
    fn passed_with(min_free_gb: u64, available: Option<u64>) -> Self {
        Self {
            passed: true,
            min_free_gb,
            available_bytes: available,
            available_gb: available.map(|b| b / GIB),
        }
    }
}

/// Walk `<pool-root>` for repo-key dirs and read each slot's display state.
/// Pure read (no lock taken, no dir created) so `status` stays safe to run
/// against a live pool.
fn collect_pool_status(pool_root: &Path, config: &BuildCacheConfig) -> Vec<RepoStatus> {
    let entries = match std::fs::read_dir(pool_root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut repos = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if crate::build_cache::repo_key::RepoKey::parse(name).is_err() {
            continue; // unrelated content under the root is never reported
        }
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let repo_dir = entry.path();
        let slots = pool::slot_dirs(&repo_dir, config)
            .into_iter()
            .map(|(kind, dir)| SlotStatus {
                kind,
                path: dir.clone(),
                holder: pool::read_holder(&dir),
                last_used: pool::read_last_used(&dir),
                target_bytes: dir_size_best_effort(&dir.join("target")),
            })
            .collect();
        repos.push(RepoStatus {
            repo_key: name.to_owned(),
            slots,
        });
    }
    repos.sort_by(|a, b| a.repo_key.cmp(&b.repo_key));
    repos
}

/// True when there is nothing at all to report (root missing or empty of
/// repo-key dirs) — drives the human "no pools yet" line.
fn pool_root_status_empty(pool_root: &Path) -> bool {
    collect_pool_status(pool_root, &BuildCacheConfig::default()).is_empty()
}

/// Directory-tree byte size, best-effort (unreadable entries skipped,
/// symlinks not followed). Command-local: the pool core's `dir_size` is
/// private to the reclaim path; `status` only ever reports this number.
fn dir_size_best_effort(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Human holder line: `slug (goal goal-id, pid N)`, or the verify note.
fn render_holder(meta: &HolderMeta) -> String {
    match meta.kind {
        SlotKind::Peer => {
            let who = meta.slug.as_deref().unwrap_or("(unknown peer)");
            let goal = meta
                .goal_id
                .as_deref()
                .map(|g| format!(", goal {g}"))
                .unwrap_or_default();
            let task = meta
                .task_id
                .as_deref()
                .map(|t| format!(", task {t}"))
                .unwrap_or_default();
            format!("{} (pid {}{goal}{task})", who, meta.pid)
        }
        SlotKind::Verify => {
            let note = meta.purpose_note.as_deref().unwrap_or("verify");
            format!("verify: {note} (pid {})", meta.pid)
        }
    }
}

/// `last_used` rendered relative to now, or `-` when absent (`0`).
fn render_last_used(last_used: u64) -> String {
    if last_used == 0 {
        return "-".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_h = now.saturating_sub(last_used) / 3600;
    if age_h == 0 {
        "<1h ago".to_string()
    } else if age_h < 24 {
        format!("{age_h}h ago")
    } else {
        format!("{}d ago", age_h / 24)
    }
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB as f64)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn payload_root_display(pool_root: &Path) -> String {
    pool_root.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_cache::ReclaimReport;
    use crate::build_cache::pool::{HolderInfo, SlotPurpose, acquire, release};
    use crate::build_cache::repo_key_for_path;

    fn config() -> BuildCacheConfig {
        BuildCacheConfig {
            peer_slots: 2,
            verify_slots: 1,
            min_free_gb: 0, // tests run on arbitrary disks; gate off
            stale_hours: 168,
        }
    }

    /// Build a pool under a tempdir: one held slot (live holder), one free
    /// slot, one stale unheld slot (backdated `last_used`, dead pid).
    struct Fixture {
        _tmp: tempfile::TempDir,
        pool_root: PathBuf,
        repo_key: String,
        stale_slot: PathBuf,
        fresh_slot: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let pool_root = tmp.path().join("build-cache");
        let key = repo_key_for_path(tmp.path()).unwrap();

        // Held by THIS process: flock truth, holder.json present.
        let held = acquire(
            &pool_root,
            &key,
            SlotPurpose::Peer,
            &config(),
            &HolderInfo {
                slug: Some("peer-hold".to_owned()),
                goal_id: Some("goal_9".to_owned()),
                ..HolderInfo::default()
            },
        )
        .unwrap();
        std::fs::write(held.target_dir.join("deps.bin"), vec![0u8; 4096]).unwrap();

        // Fresh + released: target kept, last_used = now.
        let fresh = acquire(
            &pool_root,
            &key,
            SlotPurpose::Peer,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        std::fs::write(fresh.target_dir.join("fresh.bin"), vec![0u8; 2048]).unwrap();
        let fresh_slot = fresh.path.clone();
        release(
            &mut { fresh },
            crate::build_cache::pool::SlotOutcome::Completed,
        )
        .unwrap();

        // Stale: released then backdated past any window.
        let stale = acquire(
            &pool_root,
            &key,
            SlotPurpose::Verify,
            &config(),
            &HolderInfo::default(),
        )
        .unwrap();
        std::fs::write(stale.target_dir.join("stale.bin"), vec![0u8; 1024]).unwrap();
        let stale_slot = stale.path.clone();
        release(
            &mut { stale },
            crate::build_cache::pool::SlotOutcome::Completed,
        )
        .unwrap();
        std::fs::write(stale_slot.join("last_used"), b"0\n").unwrap();

        Fixture {
            _tmp: tmp,
            pool_root,
            repo_key: key.as_str().to_owned(),
            stale_slot,
            fresh_slot,
        }
    }

    #[test]
    fn status_lists_all_slots_with_holders_sizes_and_free_space() {
        let fx = fixture();
        let repos = collect_pool_status(&fx.pool_root, &config());
        assert_eq!(repos.len(), 1, "exactly one repo-key dir");
        assert_eq!(repos[0].repo_key, fx.repo_key);
        let slots: Vec<&SlotStatus> = repos[0].slots.iter().collect();
        assert_eq!(slots.len(), 3, "peer x2 + verify x1");

        // Held slot: holder.json parsed through to the slug/goal.
        let held = slots
            .iter()
            .find(|s| s.holder.is_some())
            .expect("one held slot");
        let meta = held.holder.as_ref().unwrap();
        assert_eq!(meta.slug.as_deref(), Some("peer-hold"));
        assert_eq!(meta.goal_id.as_deref(), Some("goal_9"));
        assert_eq!(meta.pid, std::process::id());

        // Sizes: held target > 0, all target trees measured best-effort.
        assert!(held.target_bytes >= 4096);
        for slot in &slots {
            assert!(slot.target_bytes > 0, "target/ size measured for all");
        }

        // Free space of the pool root fs is a positive number on any real
        // filesystem (statvfs of the tempdir's fs).
        assert!(measure_free_space(&fx.pool_root).unwrap_or(0) > 0);
    }

    #[test]
    fn status_json_is_parseable_and_carries_the_same_rows() {
        let fx = fixture();
        // D3: exercise the SAME constructor run_status feeds to serde —
        // the field mapping (free → available_bytes/gb, holder nesting)
        // is what is under test, not a hand-copied payload.
        let payload = StatusJson::build(
            fx.pool_root.clone(),
            measure_free_space(&fx.pool_root),
            collect_pool_status(&fx.pool_root, &config()),
        );
        let text = serde_json::to_string_pretty(&payload).unwrap();
        // Round-trip through serde_json::Value: parseable + slot rows intact.
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let repos = value["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1);
        let slots = repos[0]["slots"].as_array().unwrap();
        assert_eq!(slots.len(), 3);
        let held = slots
            .iter()
            .find(|s| s.get("holder").is_some())
            .expect("held slot serialized with its holder");
        assert_eq!(held["holder"]["slug"], "peer-hold");
        assert_eq!(held["holder"]["goal_id"], "goal_9");
        assert!(value["free"]["available_bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn gc_defaults_to_report_only() {
        let fx = fixture();
        let policy = GcPolicy {
            stale_hours: 1,
            apply: false,
        };
        let reports = pool::reclaim_stale(&fx.pool_root, &policy, &config()).unwrap();
        // The stale slot is REPORTED as stale-and-reclaimable (D1): the
        // row names the bytes --apply would free, the outcome is NOT
        // `reclaimed`, and its target/ must still exist.
        let row = reports.iter().find(|r| r.slot_path == fx.stale_slot);
        assert!(row.is_some(), "stale slot reported");
        let row = row.unwrap();
        assert_eq!(row.outcome, ReclaimOutcome::Stale);
        assert!(row.would_free_bytes > 0, "carries the would-free size");
        assert_eq!(row.freed_bytes, 0, "nothing freed without --apply");
        assert!(
            fx.stale_slot.join("target").is_dir(),
            "report-only GC must not delete target/"
        );
        // No row anywhere claimed a deletion.
        assert!(
            !reports
                .iter()
                .any(|r| r.outcome == ReclaimOutcome::Reclaimed),
            "no deletion without --apply"
        );
    }

    #[test]
    fn gc_apply_reclaims_only_stale_unheld_slots() {
        let fx = fixture();
        let policy = GcPolicy {
            stale_hours: 1,
            apply: true,
        };
        let reports = pool::reclaim_stale(&fx.pool_root, &policy, &config()).unwrap();
        let reclaimed: Vec<&ReclaimReport> = reports
            .iter()
            .filter(|r| r.outcome == ReclaimOutcome::Reclaimed)
            .collect();
        assert_eq!(reclaimed.len(), 1, "exactly the stale slot");
        assert_eq!(reclaimed[0].slot_path, fx.stale_slot);
        assert!(reclaimed[0].freed_bytes > 0);
        assert!(!fx.stale_slot.join("target").exists());
        // The lock inode and the slot dir itself survive (never deleted).
        assert!(fx.stale_slot.join(".lock").is_file());
        // Fresh + held slots untouched.
        assert!(fx.fresh_slot.join("target").is_dir());
        let held_dir = fx.pool_root.join(&fx.repo_key).join("slot-1");
        assert!(held_dir.join("target").is_dir());
    }

    #[test]
    fn gc_json_round_trips() {
        let fx = fixture();
        let reports = pool::reclaim_stale(
            &fx.pool_root,
            &GcPolicy {
                stale_hours: 1,
                apply: false,
            },
            &config(),
        )
        .unwrap();
        // D3: exercise the SAME constructor run_gc feeds to
        // serde_json::to_string_pretty, so the field mapping itself is
        // under test — not a hand-copied payload.
        let payload = GcJson::build(fx.pool_root.clone(), 1, false, &reports);
        let text = serde_json::to_string_pretty(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["apply"], false);
        assert_eq!(value["stale_hours"], 1);
        assert_eq!(value["slots"].as_array().unwrap().len(), 3);
        // Outcomes are the lowercase labels the core defines; D1: the
        // stale slot is labelled `stale` (NOT `fresh`) and carries the
        // would-reclaim bytes so the outer loop (#6) can read what
        // --apply would free.
        let slots = value["slots"].as_array().unwrap();
        let outcomes: Vec<&str> = slots
            .iter()
            .map(|s| s["outcome"].as_str().unwrap())
            .collect();
        assert!(outcomes.contains(&"locked"));
        assert!(
            outcomes.contains(&"stale"),
            "stale slot named as such: {outcomes:?}"
        );
        assert!(
            !outcomes.contains(&"reclaimed"),
            "nothing reclaimed: {outcomes:?}"
        );
        let stale_row = slots
            .iter()
            .find(|s| s["outcome"] == "stale")
            .expect("stale row present");
        assert_eq!(stale_row["freed_bytes"].as_u64(), Some(0));
        assert!(
            stale_row["would_free_bytes"].as_u64().unwrap() > 0,
            "stale row carries would-free bytes"
        );
        assert!(value["reclaimed_bytes"].as_u64().unwrap() == 0);
        assert!(value["would_free_bytes"].as_u64().unwrap() > 0);
        // The stale row is the fixture's stale slot (not some other dir).
        let reported_path = Path::new(stale_row["slot_path"].as_str().unwrap());
        assert_eq!(reported_path, fx.stale_slot);
    }

    #[test]
    fn gate_passes_when_free_space_meets_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let pool_root = tmp.path().join("build-cache");
        std::fs::create_dir_all(&pool_root).unwrap(); // statvfs needs a path
        // A threshold of 0 disables the gate (documented knob semantics).
        pool::check_free_space(&pool_root, 0).unwrap();
        // A tiny positive threshold passes on any real temp filesystem.
        pool::check_free_space(&pool_root, 1).unwrap();
        // D3: the json payload goes through the SAME constructor run_gate
        // feeds to serde (pass arm).
        let available = measure_free_space(&pool_root).unwrap();
        let payload = GateJson::passed_with(1, Some(available));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert_eq!(value["passed"], true);
        assert_eq!(value["min_free_gb"], 1);
        assert_eq!(value["available_bytes"].as_u64(), Some(available));
        assert_eq!(value["available_gb"].as_u64(), Some(available / GIB));
    }

    #[test]
    fn gate_fails_below_threshold_with_low_space_error() {
        let tmp = tempfile::tempdir().unwrap();
        let pool_root = tmp.path().join("build-cache");
        std::fs::create_dir_all(&pool_root).unwrap(); // statvfs needs a path
        let err = pool::check_free_space(&pool_root, 100_000_000).unwrap_err();
        let (min_gb, raw_bytes) = match &err {
            BuildCacheError::FreeSpaceLow {
                min_gb,
                available_bytes,
                ..
            } => (*min_gb, *available_bytes),
            other => panic!("expected FreeSpaceLow, got {other:?}"),
        };
        assert_eq!(min_gb, 100_000_000);
        // D5: the raw measured count is carried on the typed error — the
        // json bytes never come from re-deriving the rounded f64 GB. The
        // two independent measurements below may drift by a few blocks of
        // live disk churn, so compare within a tight window and require
        // the exact carry-through into the payload.
        let true_bytes = fs2::available_space(&pool_root).unwrap();
        assert!(raw_bytes > 0);
        assert!(
            raw_bytes.abs_diff(true_bytes) < GIB,
            "raw bytes {raw_bytes} should track the live measurement {true_bytes}"
        );
        // D3: exercise the SAME constructor run_gate's error arm uses.
        let payload = GateJson::build(min_gb, &Err(&err));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert_eq!(value["passed"], false);
        assert_eq!(value["available_bytes"].as_u64(), Some(raw_bytes));
        assert_eq!(value["available_gb"].as_u64(), Some(raw_bytes / GIB));
    }

    #[test]
    fn gate_fails_closed_when_fs_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // Pool root's parent removed underneath us: statvfs cannot answer.
        let gone = tmp.path().join("vanishing");
        std::fs::create_dir_all(&gone).unwrap();
        let probe = gone.join("pool");
        std::fs::remove_dir_all(&gone).unwrap();
        let err = pool::check_free_space(&probe, 50).unwrap_err();
        match err {
            BuildCacheError::FreeSpaceUnknown { .. } => {}
            other => panic!("expected FreeSpaceUnknown, got {other:?}"),
        }
        // Unknown → omitted (not a fake zero) in the json payload.
        let payload = GateJson::build(50, &Err(&err));
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        assert!(value.get("available_bytes").is_none());
        assert_eq!(value["passed"], false);
    }

    #[test]
    fn stale_hours_flag_respects_the_config_floor() {
        // D4: 0 would make --apply reclaim everything ever touched —
        // rejected with a clear message, same floor as the config layer.
        let err = resolve_stale_hours(Some(0), 168).unwrap_err();
        assert!(err.to_string().contains("at least 1 hour"), "{err}");
        // Valid flags pass through untouched; missing flag falls back to
        // config, itself clamped to the same floor.
        assert_eq!(resolve_stale_hours(Some(24), 168).unwrap(), 24);
        assert_eq!(resolve_stale_hours(None, 168).unwrap(), 168);
        assert_eq!(resolve_stale_hours(None, 0).unwrap(), 1);
        assert_eq!(
            resolve_stale_hours(Some(pool::MAX_STALE_HOURS), 168).unwrap(),
            pool::MAX_STALE_HOURS
        );
        for hours in [pool::MAX_STALE_HOURS + 1, u64::MAX] {
            assert!(resolve_stale_hours(Some(hours), 168).is_err());
            assert!(resolve_stale_hours(None, hours).is_err());
        }
    }

    #[test]
    fn renderers_stay_readable() {
        let meta = HolderMeta {
            kind: SlotKind::Peer,
            pid: 42,
            slug: Some("peer-a".to_owned()),
            goal_id: Some("goal_1".to_owned()),
            task_id: None,
            purpose_note: None,
            acquired_at: 1,
            claim_token: String::new(),
        };
        assert_eq!(render_holder(&meta), "peer-a (pid 42, goal goal_1)");
        let verify = HolderMeta {
            kind: SlotKind::Verify,
            purpose_note: Some("review re-verify".to_owned()),
            ..meta
        };
        assert_eq!(render_holder(&verify), "verify: review re-verify (pid 42)");
        assert_eq!(render_last_used(0), "-");
        assert!(render_last_used(1).ends_with("d ago"));
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert!(format_gib(GIB).starts_with("1.0"));
    }

    #[test]
    fn status_ignores_non_pool_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let pool_root = tmp.path().join("build-cache");
        std::fs::create_dir_all(pool_root.join("not-a-key")).unwrap();
        std::fs::create_dir_all(pool_root.join("deadbeefdeadb")).unwrap(); // 13 chars
        std::fs::write(pool_root.join("aaaaaaaaaaaa.txt"), b"x").unwrap();
        let repos = collect_pool_status(&pool_root, &config());
        assert!(
            repos.is_empty(),
            "unrelated content under the pool root is never reported"
        );
        assert!(pool_root_status_empty(&pool_root));
    }

    /// Script-side parser for the acquire output (#6's contract): three
    /// lines `SLOT <p>` / `TARGET <p>` / `RELEASE <cmd>`, in that order,
    /// nothing before them. Returns None on any deviation — a loop script
    /// would rather fail loudly than cargo into the wrong directory.
    fn parse_acquire_output(stdout: &str) -> Option<(PathBuf, PathBuf, String)> {
        let mut lines = stdout.lines();
        let slot = lines.next()?.strip_prefix("SLOT ")?;
        let target = lines.next()?.strip_prefix("TARGET ")?;
        let release = lines.next()?.strip_prefix("RELEASE ")?;
        if slot.is_empty() || target.is_empty() || release.is_empty() {
            return None;
        }
        Some((
            PathBuf::from(slot),
            PathBuf::from(target),
            release.to_owned(),
        ))
    }

    #[test]
    fn acquire_output_parses_with_the_script_parser() {
        let fx = fixture();
        let pool_root = fx.pool_root;
        let repo_dir = tempfile::tempdir().unwrap();
        let key = repo_key_for_path(repo_dir.path()).unwrap();
        // gate off, as everywhere in this module (arbitrary test disks)
        let holder = HolderInfo {
            purpose_note: Some("outer-loop verify".to_owned()),
            pid_override: Some(std::process::id()),
            ..HolderInfo::default()
        };
        let slot = pool::acquire_detached(&pool_root, &key, &config(), &holder).unwrap();

        // Render exactly what run_acquire prints (same fns, no colors on
        // the three contract lines).
        let stdout = format!(
            "SLOT {}\nTARGET {}\nRELEASE {}\n",
            slot.path.display(),
            slot.target_dir.display(),
            release_command(&slot)
        );
        let (slot_out, target_out, release_out) =
            parse_acquire_output(&stdout).expect("the three contract lines must parse in order");
        assert_eq!(slot_out, slot.path);
        assert_eq!(target_out, slot.path.join("target"));
        assert_eq!(
            release_out,
            format!(
                "octos cache release --slot {} --token {}",
                shell_quoted(&slot.path),
                slot.claim_token
            )
        );
        // Degenerate output must not half-parse.
        assert!(parse_acquire_output("SLOT /a\nTARGET /b\n").is_none());
        assert!(parse_acquire_output("").is_none());
        assert!(parse_acquire_output("note\nSLOT /a\nTARGET /b\nRELEASE octos x\n").is_none());
    }

    #[test]
    fn acquire_release_roundtrip_frees_the_verify_slot_for_reacquire() {
        let pool_root = fixture().pool_root;
        let repo_dir = tempfile::tempdir().unwrap();
        let key = repo_key_for_path(repo_dir.path()).unwrap();
        let holder = HolderInfo {
            pid_override: Some(std::process::id()),
            ..HolderInfo::default()
        };

        // Acquire (CLI semantics: flock intentionally leaked away at
        // process-exit, so from HERE on the slot is held by metadata only).
        let first = pool::acquire_detached(&pool_root, &key, &config(), &holder).unwrap();
        // Held between processes: status must see it with purpose=verify.
        let meta = pool::read_holder(&first.path).expect("holder.json written");
        assert_eq!(meta.kind, SlotKind::Verify);
        assert_eq!(meta.pid, std::process::id());
        // Live detached claims remain exclusive after their flock is dropped.
        assert!(matches!(
            pool::acquire_detached(&pool_root, &key, &config(), &holder),
            Err(BuildCacheError::PoolExhausted { .. })
        ));
        pool::release_detached(
            &pool_root,
            &first.path,
            &first.claim_token,
            SlotOutcome::Completed,
        )
        .unwrap();
        assert!(pool::read_holder(&first.path).is_none(), "holder cleared");
        let second = pool::acquire_detached(&pool_root, &key, &config(), &holder).unwrap();
        assert_eq!(second.path, first.path, "released slot is reusable");
        assert_eq!(second.target_dir, first.target_dir);
        // target contents survive release (I2 — the whole point of the pool)
        assert!(second.target_dir.is_dir());
    }

    #[test]
    fn release_is_idempotent() {
        let pool_root = fixture().pool_root;
        let repo_dir = tempfile::tempdir().unwrap();
        let key = repo_key_for_path(repo_dir.path()).unwrap();
        let holder = HolderInfo {
            pid_override: Some(std::process::id()),
            ..HolderInfo::default()
        };
        let slot = pool::acquire_detached(&pool_root, &key, &config(), &holder).unwrap();
        pool::release_detached(
            &pool_root,
            &slot.path,
            &slot.claim_token,
            SlotOutcome::Completed,
        )
        .unwrap();
        // Second release: no holder.json left ⇒ no-op Ok, never an error —
        // the outer loop may run its cleanup unconditionally.
        pool::release_detached(
            &pool_root,
            &slot.path,
            &slot.claim_token,
            SlotOutcome::Completed,
        )
        .expect("double release is a no-op");
        // And a third after the last_used stamp: still fine.
        pool::release_detached(
            &pool_root,
            &slot.path,
            &slot.claim_token,
            SlotOutcome::Completed,
        )
        .unwrap();
    }

    #[test]
    fn release_rejects_unknown_and_outside_paths_cleanly() {
        let fx = fixture();
        let pool_root = fx.pool_root;

        // Not a slot: exists, under the pool root, but never had a .lock.
        let not_a_slot = pool_root.join("0123456789ab/verify-7");
        std::fs::create_dir_all(&not_a_slot).unwrap();
        assert!(matches!(
            pool::release_detached(
                &pool_root,
                &not_a_slot,
                "unused-token",
                SlotOutcome::Completed
            ),
            Err(BuildCacheError::SlotNotFound { .. })
        ));

        // Outside the pool root entirely (including via a plausible path).
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(matches!(
            pool::release_detached(
                &pool_root,
                elsewhere.path(),
                "unused-token",
                SlotOutcome::Completed
            ),
            Err(BuildCacheError::SlotOutsidePool { .. })
        ));

        // Nonexistent path: canonicalize fails FIRST (the dir doesn't
        // exist), which is reported as SlotNotFound — same script-facing
        // meaning ("this is not a slot"), just detected one step earlier.
        let ghost = pool_root.join("0123456789ab/verify-99");
        assert!(matches!(
            pool::release_detached(&pool_root, &ghost, "unused-token", SlotOutcome::Completed),
            Err(BuildCacheError::SlotNotFound { .. })
        ));
    }

    #[test]
    fn detached_slot_is_held_for_status_between_processes() {
        // The #6 truth model, end to end: acquire in "process A" (flock
        // leaked away = A has exited), then the pool's OWN gc walk — the
        // thing that would reclaim the slot — must treat it as held while
        // the recorded pid is alive, and must reclaim it once that pid dies.
        let pool_root = fixture().pool_root;
        let repo_dir = tempfile::tempdir().unwrap();
        let key = repo_key_for_path(repo_dir.path()).unwrap();
        let dead_pid = {
            // Reap a freshly-exited child so test_kill_process(ESRCH)s.
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let live_holder = HolderInfo {
            pid_override: Some(std::process::id()),
            ..HolderInfo::default()
        };
        let slot = pool::acquire_detached(&pool_root, &key, &config(), &live_holder).unwrap();
        std::fs::write(slot.target_dir.join("dep.bin"), vec![0u8; 2048]).unwrap();
        // Backdate so staleness alone would reclaim it if it were unheld.
        std::fs::write(slot.path.join("last_used"), b"0\n").unwrap();

        // Live pid: gc must not touch the slot (skip = held), even though
        // the flock is long gone (the CLI exited).
        let reports = pool::reclaim_stale(
            &pool_root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = reports
            .iter()
            .find(|r| r.slot_path == slot.path)
            .expect("the verify slot is reported");
        assert_eq!(
            row.outcome,
            ReclaimOutcome::Locked,
            "live pid keeps the slot held"
        );
        assert!(
            slot.target_dir.join("dep.bin").exists(),
            "contents untouched"
        );

        // The live GC pass preserves ownership. Release that claim before
        // exercising the separate dead-holder lifecycle below.
        pool::release_detached(
            &pool_root,
            &slot.path,
            &slot.claim_token,
            SlotOutcome::Completed,
        )
        .unwrap();

        // Dead pid: gc clears the metadata first (HolderCleared — the slot
        // was just re-acquired so last_used is fresh, nothing to delete),
        // and a SECOND backdated pass then reclaims the target. Two steps
        // mirrors §3.5 exactly: dead holder ⇒ demote to ownerless, THEN
        // staleness applies on its own clock.
        let dead_holder = HolderInfo {
            pid_override: Some(dead_pid),
            ..HolderInfo::default()
        };
        let slot2 = pool::acquire_detached(&pool_root, &key, &config(), &dead_holder).unwrap();
        assert_eq!(
            slot2.path, slot.path,
            "explicitly released live claim is reusable"
        );
        let reports = pool::reclaim_stale(
            &pool_root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = reports
            .iter()
            .find(|r| r.slot_path == slot.path)
            .expect("reported again");
        assert_eq!(
            row.outcome,
            ReclaimOutcome::HolderCleared,
            "dead holder ⇒ metadata cleared"
        );
        assert!(
            slot.target_dir.join("dep.bin").exists(),
            "fresh clock: target kept this pass"
        );

        // Backdate past the window and walk once more: now it reclaims.
        std::fs::write(slot.path.join("last_used"), b"0\n").unwrap();
        let reports = pool::reclaim_stale(
            &pool_root,
            &GcPolicy {
                stale_hours: 1,
                apply: true,
            },
            &config(),
        )
        .unwrap();
        let row = reports
            .iter()
            .find(|r| r.slot_path == slot.path)
            .expect("reported a third time");
        assert_eq!(
            row.outcome,
            ReclaimOutcome::Reclaimed,
            "ownerless + stale ⇒ reclaimed"
        );
        assert!(
            !slot.target_dir.join("dep.bin").exists(),
            "target cleared by gc --apply"
        );
    }

    #[test]
    fn clap_subcommands_parse() {
        // Parse through the REAL `octos` Args (the way `octos cache …` is
        // actually invoked) so the subcommand enum wiring is exercised too.
        use clap::Parser;
        let args = crate::commands::Args::try_parse_from([
            "octos",
            "cache",
            "status",
            "--json",
            "--data-dir",
            "/tmp/octos-cache-tmp",
        ])
        .expect("`octos cache status --json` must parse");
        assert!(matches!(args.command, crate::commands::Command::Cache(_)));

        crate::commands::Args::try_parse_from([
            "octos",
            "cache",
            "gc",
            "--stale-hours",
            "24",
            "--apply",
        ])
        .expect("`octos cache gc --stale-hours 24 --apply` must parse");
        crate::commands::Args::try_parse_from([
            "octos",
            "cache",
            "gate",
            "--min-free-gb",
            "10",
            "--json",
        ])
        .expect("`octos cache gate --min-free-gb 10 --json` must parse");
        // #6: the outer-loop acquire/release pair.
        crate::commands::Args::try_parse_from([
            "octos",
            "cache",
            "acquire",
            "--purpose",
            "verify",
            "--repo",
            "/repo/under/verify",
            "--note",
            "outer-loop recheck",
            "--pid",
            "4242",
            "--json",
        ])
        .expect("`octos cache acquire --purpose verify …` must parse");
        crate::commands::Args::try_parse_from([
            "octos",
            "cache",
            "release",
            "--slot",
            "/pool/0123456789ab/verify-1",
            "--token",
            "claim-identity",
        ])
        .expect("`octos cache release --slot … --token …` must parse");
        assert!(
            crate::commands::Args::try_parse_from([
                "octos",
                "cache",
                "release",
                "--slot",
                "/pool/0123456789ab/verify-1"
            ])
            .is_err(),
            "release requires acquisition identity"
        );
        // Peer namespace is NOT acquireable by hand (§1.3 namespaces).
        assert!(
            crate::commands::Args::try_parse_from([
                "octos",
                "cache",
                "acquire",
                "--purpose",
                "peer",
                "--repo",
                "."
            ])
            .is_err()
        );
        // Bare `octos cache` still parses (overview).
        crate::commands::Args::try_parse_from(["octos", "cache"])
            .expect("bare `octos cache` must parse");
        // Rejects an unknown subcommand (typo protection).
        assert!(crate::commands::Args::try_parse_from(["octos", "cache", "stat"]).is_err());
    }

    #[test]
    fn json_flag_reserves_stdout() {
        use clap::Parser;
        let json =
            crate::commands::Args::try_parse_from(["octos", "cache", "gate", "--json"]).unwrap();
        assert!(crate::commands::reserve_stdout(&json.command));
        let human = crate::commands::Args::try_parse_from(["octos", "cache", "status"]).unwrap();
        assert!(!crate::commands::reserve_stdout(&human.command));
    }
}
