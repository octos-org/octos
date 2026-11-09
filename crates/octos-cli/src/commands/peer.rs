//! `octos peer list` — read-only peer observability (OLP L1, slice 3).
//!
//! Contract: task-req-olp-obs-cli.spec.md — peers/ 目录直读
//! (brief/result/closed 状态). Reads `<data_dir>/peers/<slug>/` directly:
//! a peer is `staged` (brief.md present), `done` (result files exist), or
//! `closed` (the `closed` marker exists). No serve process required.
//! `--json` and the human table share one assembly layer.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use eyre::Result;
use serde::Serialize;

use super::Executable;

#[derive(Debug, Args)]
pub struct PeerCommand {
    #[command(subcommand)]
    pub action: PeerAction,
}

#[derive(Debug, Subcommand)]
pub enum PeerAction {
    /// List staged peers with their lifecycle state.
    List(PeerListArgs),
}

#[derive(Debug, Args)]
pub struct PeerListArgs {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Data-dir override (defaults to the standard resolution).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Profile id used to validate peer lifetime projections
    /// (task-evo-peer-turn-status). The lifetime's registry_key must match
    /// `<profile>:peer:<slug>`; the profile is NEVER derived from the
    /// data-dir path. Defaults to the standard default profile ("octos").
    #[arg(long, value_name = "ID")]
    pub profile: Option<String>,
}

/// One peer row. Field names are part of the machine contract
/// (docs/peer-status-interface.json).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PeerListRow {
    pub slug: String,
    /// running | done | closed (a closed peer is reported even if results
    /// exist — closed is the terminal, operator-visible truth).
    pub status: String,
    /// queued | running | idle | failed | closed | unknown — the CURRENT
    /// execution state from the trusted lifetime projection (fail-closed;
    /// unknown whenever no trusted authority exists).
    pub execution: String,
    /// completed | errored | interrupted | rate_limited | null — the most
    /// recent TERMINATED round's outcome, from the strict terminal evidence
    /// (turns.txt tail × highest result-<n>.md cross-check).
    pub last_outcome: Option<String>,
    /// Current round (queued/running: delivered+1; else the terminated
    /// round's number).
    pub round: u32,
    /// Delivered rounds = count(result-<n>.md), floored at 1 for a bare
    /// result.md (#2024).
    pub rounds_delivered: u32,
    pub has_brief: bool,
    pub result_versions: u32,
    pub name: Option<String>,
    pub model_lane: Option<String>,
    /// Trusted-lifetime identity (anti cross-runtime/same-slug fencing);
    /// null when the projection is untrusted. The `closed` branch runs the
    /// SAME full trust check (not bypassed by the marker): a trusted
    /// lifetime keeps its identity for terminal-event correlation; a
    /// missing/forged/foreign/corrupt one stays null.
    pub master_session_id: Option<String>,
    /// T-0454: the staging originator session id (regrouping attribution;
    /// never feeds `master_session_id`).
    pub originator: Option<String>,
    pub task_id: Option<String>,
    pub generation: Option<u64>,
    pub turn_id: Option<String>,
}

/// Assemble the peer list straight from the `peers/` directory. Symlinked
/// or unstaged entries are skipped (same safety gate as serve's scans).
/// `profile_id` validates the lifetime projection's registry_key — supplied
/// EXPLICITLY by the caller (`--profile` / the resolved default), never
/// derived from the data-dir path (task-evo-peer-turn-status).
pub(crate) fn list_peers(data_dir: &Path, profile_id: &str) -> Vec<PeerListRow> {
    // Single assembly: the CLI rows are a projection of the shared
    // blackboard read (same source as serve's peer_list/peer_gather), so the
    // three consumers can never drift.
    let rows =
        crate::peers::read_peer_blackboard_with_profile(&data_dir.join("peers"), None, profile_id);
    let peers_root = data_dir.join("peers");
    let mut rows: Vec<PeerListRow> = rows
        .into_iter()
        .map(|row| {
            // RAW numbered-version count (the original `result_versions`
            // contract: count(result-<n>.md) with NO #2024 floor) — kept
            // distinct from `rounds_delivered`, which applies the floor.
            let result_versions =
                crate::peers::count_peer_result_versions(&peers_root.join(&row.slug));
            // LEGACY status semantics, preserved EXACTLY (outer-loop
            // review): done = a numbered result-<n>.md exists OR the bare
            // result.md exists. The blackboard row's `result` field only
            // carries the BARE file (an oversized/unreadable bare file
            // yields None there but still proves delivery), so the version
            // count participates too — a numbered-only peer must stay done.
            let has_result = row.execution_facet.rounds_delivered > 0 || row.result.is_some();
            let status = if row.closed {
                "closed"
            } else if has_result {
                "done"
            } else {
                "running"
            };
            // name: the ORIGINAL optional-name semantics — Some only when a
            // non-empty `name` file was recorded (including content == slug,
            // the operator's explicit choice); None when absent/empty. The
            // blackboard collapses a missing name onto the slug for
            // ADDRESSING; the CLI contract keeps the distinction.
            let raw_name = crate::peers::peer_io::read_peer_file(
                &peers_root.join(&row.slug),
                "name",
                crate::peers::peer_io::PEER_FILE_READ_CAP_SMALL,
            )
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
            PeerListRow {
                slug: row.slug.clone(),
                status: status.to_owned(),
                execution: row.execution_facet.execution.to_owned(),
                last_outcome: row.execution_facet.last_outcome,
                round: row.execution_facet.round,
                rounds_delivered: row.execution_facet.rounds_delivered,
                has_brief: true,
                result_versions,
                name: raw_name,
                model_lane: row.model_lane,
                master_session_id: row.execution_facet.master_session_id,
                originator: row.originator.clone(),
                task_id: row.execution_facet.task_id,
                generation: row.execution_facet.generation,
                turn_id: row.execution_facet.turn_id,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    rows
}

/// ONE production renderer for a peer row's table line (shared by
/// `print_table`; asserted directly by the shared-assembly contract test —
/// the test must NOT re-implement the mapping).
fn render_table_line(row: &PeerListRow) -> String {
    // Table rendering maps (interface contract): unknown execution shows
    // "?", a null outcome shows "-" — both asserted by the shared-
    // assembly test via this function.
    let execution = if row.execution == "unknown" {
        "?".to_owned()
    } else {
        row.execution.clone()
    };
    let outcome = row.last_outcome.clone().unwrap_or_else(|| "-".to_owned());
    format!(
        "{:<24} {:<8} {:<8} {:<12} {:<7} {}",
        row.slug,
        row.status,
        execution,
        outcome,
        row.rounds_delivered,
        row.name.as_deref().unwrap_or("-")
    )
}

fn print_table(rows: &[PeerListRow]) {
    if rows.is_empty() {
        println!("(no staged peers)");
        return;
    }
    println!(
        "{:<24} {:<8} {:<8} {:<12} {:<7} NAME",
        "SLUG", "STATUS", "CURRENT", "OUTCOME", "ROUNDS"
    );
    for row in rows {
        println!("{}", render_table_line(row));
    }
}

/// Resolve the CLI routing for `peer list` from the RAW parsed args:
/// returns `(data_dir, profile_id)` exactly as `execute` uses them.
/// Extracted so the `--data-dir` + `--profile` COMBINATION test can exercise
/// the REAL routing (outer-loop review: a test calling `list_peers(temp,
/// profile)` directly bypasses the CLI layer entirely).
fn route_peer_list(
    args: &PeerListArgs,
    state_home: &Path,
    cwd: &Path,
) -> (std::path::PathBuf, String) {
    let profile_id = args
        .profile
        .clone()
        .unwrap_or_else(|| super::obs::DEFAULT_PROFILE_ID.to_owned());
    let data_dir = super::obs::resolve_profile_data_root(state_home, cwd, &profile_id);
    let data_dir = args.data_dir.clone().unwrap_or(data_dir);
    (data_dir, profile_id)
}

impl Executable for PeerCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            PeerAction::List(args) => {
                // 整改: shared per-instance profile data root (see goal.rs).
                // task-e...[credential-redacted]: the profile id for lifetime
                // projection validation comes from --profile (explicit) or
                // the default — NEVER derived from the data-dir path. The
                // SAME id also drives the data-root resolution, so
                // `--profile octosfix` without --data-dir reads the octosfix
                // profile's directory (outer-loop review: passing the
                // constant here made --profile a no-op for resolution).
                let (data_dir, profile_id) = route_peer_list(
                    &args,
                    &super::resolve_data_dir(None)?,
                    &std::env::current_dir()?,
                );
                // 整改要求 2: a missing peers dir is an ERROR with the
                // resolved path (never a silent empty list).
                let peers_root = data_dir.join("peers");
                if !peers_root.is_dir() {
                    let message = format!(
                        "no peers directory at {} (resolved data root: {})",
                        peers_root.display(),
                        data_dir.display()
                    );
                    if args.json {
                        eprintln!(
                            "{}",
                            serde_json::json!({"error": message, "path": peers_root})
                        );
                    } else {
                        eprintln!("error: {message}");
                    }
                    std::process::exit(1);
                }
                let rows = list_peers(&data_dir, &profile_id);
                if args.json {
                    println!("{}", serde_json::to_string(&rows).expect("peers json"));
                } else {
                    print_table(&rows);
                }
                Ok(())
            }
        }
    }
}
