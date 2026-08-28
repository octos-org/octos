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
}

/// One peer row. Field names are part of the machine contract.
#[derive(Debug, Serialize)]
pub(crate) struct PeerListRow {
    pub slug: String,
    /// running | done | closed (a closed peer is reported even if results
    /// exist — closed is the terminal, operator-visible truth).
    pub status: String,
    pub has_brief: bool,
    pub result_versions: u32,
    pub name: Option<String>,
    pub model_lane: Option<String>,
}

/// Assemble the peer list straight from the `peers/` directory. Symlinked
/// or unstaged entries are skipped (same safety gate as serve's scans).
pub(crate) fn list_peers(data_dir: &Path) -> Vec<PeerListRow> {
    let peers_root = data_dir.join("peers");
    let mut rows = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(&peers_root) else {
        return rows; // no peers dir at all -> empty list, not an error
    };
    for entry in read_dir.flatten() {
        let slug = entry.file_name().to_string_lossy().into_owned();
        let Some(dir) = crate::peers::staged_peer_dir(&peers_root, &slug) else {
            continue;
        };
        use crate::peers::peer_io as io;
        let has_brief = io::peer_regular_file_exists(&dir, "brief.md");
        if !has_brief {
            continue; // not a staged peer (the staging contract)
        }
        let closed = io::peer_regular_file_exists(&dir, "closed");
        let result_versions = crate::peers::count_peer_result_versions(&dir);
        // A turn's latest finding lands in the bare `result.md`
        // (overwritten per terminal); numbered `result-<n>.md` are older
        // versions. Either proves delivery.
        let has_result = result_versions > 0 || io::peer_regular_file_exists(&dir, "result.md");
        let status = if closed {
            "closed"
        } else if has_result {
            "done"
        } else {
            "running"
        };
        let name = io::read_peer_file(&dir, "name", io::PEER_FILE_READ_CAP_SMALL)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let model_lane = io::read_peer_file(&dir, "model", io::PEER_FILE_READ_CAP_SMALL)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        rows.push(PeerListRow {
            slug,
            status: status.to_owned(),
            has_brief,
            result_versions,
            name,
            model_lane,
        });
    }
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    rows
}

fn print_table(rows: &[PeerListRow]) {
    if rows.is_empty() {
        println!("(no staged peers)");
        return;
    }
    println!("{:<24} {:<8} {:<8} NAME", "SLUG", "STATUS", "RESULTS");
    for row in rows {
        println!(
            "{:<24} {:<8} {:<8} {}",
            row.slug,
            row.status,
            row.result_versions,
            row.name.as_deref().unwrap_or("-")
        );
    }
}

impl Executable for PeerCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            PeerAction::List(args) => {
                // 整改: shared per-instance profile data root (see goal.rs).
                let data_dir = super::obs::resolve_profile_data_root(
                    &super::resolve_data_dir(None)?,
                    &std::env::current_dir()?,
                    super::obs::DEFAULT_PROFILE_ID,
                );
                let data_dir = args.data_dir.unwrap_or(data_dir);
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
                let rows = list_peers(&data_dir);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_peer(data_dir: &Path, slug: &str) -> PathBuf {
        let dir = data_dir.join("peers").join(slug);
        std::fs::create_dir_all(&dir).expect("peer dir");
        std::fs::write(dir.join("brief.md"), "task brief").expect("brief");
        dir
    }

    /// Contract: peers/ 目录直读 — brief staged, result versions counted,
    /// closed marker is terminal. No serve involved.
    #[test]
    fn olp_obs_peer_list_reads_peers_dir_states() {
        let temp = tempfile::tempdir().expect("tempdir");
        // running: brief only
        stage_peer(temp.path(), "alpha");
        // done: brief + result
        let done_dir = stage_peer(temp.path(), "beta");
        std::fs::write(done_dir.join("result.md"), "findings").expect("result");
        // closed: brief + result + closed marker
        let closed_dir = stage_peer(temp.path(), "gamma");
        std::fs::write(closed_dir.join("result.md"), "findings").expect("result");
        std::fs::write(closed_dir.join("closed"), "x").expect("closed");
        // unstaged junk: no brief -> skipped
        std::fs::create_dir_all(temp.path().join("peers").join("junk")).expect("junk");

        let rows = list_peers(temp.path());
        assert_eq!(rows.len(), 3);
        let by_slug = |s: &str| rows.iter().find(|r| r.slug == s).expect("row");
        assert_eq!(by_slug("alpha").status, "running");
        assert_eq!(by_slug("beta").status, "done");
        assert_eq!(by_slug("beta").result_versions, 0); // bare result.md, no numbered versions
        assert_eq!(by_slug("gamma").status, "closed");
        // JSON shape: valid array, contract field names.
        let json = serde_json::to_value(&rows).expect("json");
        assert!(json.is_array());
        assert!(json[0].get("slug").is_some());
        assert!(json[0].get("status").is_some());
    }

    /// Empty / missing peers dir -> empty JSON array, exit-0 shape.
    #[test]
    fn olp_obs_peer_list_empty_dir_is_empty_array() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rows = list_peers(temp.path());
        assert!(rows.is_empty());
        assert_eq!(serde_json::to_string(&rows).expect("json"), "[]");
    }
}
