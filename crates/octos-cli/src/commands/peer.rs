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
    /// null when the projection is untrusted — and also for the `closed`
    /// branch, which intentionally reports identity-free (the closed marker
    /// takes precedence over the projection; conservative contract).
    pub master_session_id: Option<String>,
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
                task_id: row.execution_facet.task_id,
                generation: row.execution_facet.generation,
                turn_id: row.execution_facet.turn_id,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    rows
}

/// Test-visible alias for [`list_peers`] — the module is private to
/// `commands`, but the peers-module contract tests need the REAL CLI
/// assembly (status semantics + name preservation) over real dirs.
#[cfg(test)]
pub(crate) fn peer_list_for_test(data_dir: &Path, profile_id: &str) -> Vec<PeerListRow> {
    list_peers(data_dir, profile_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_peer(data_dir: &Path, slug: &str) -> PathBuf {
        let dir = data_dir.join("peers").join(slug);
        std::fs::create_dir_all(&dir).expect("peer dir");
        std::fs::write(dir.join("brief.md"), "task brief").expect("brief");
        dir
    }

    /// Stage a single NON-EMPTY row so the JSON contract test can observe a
    /// real serialized object (an empty list serializes to `[]` and reveals
    /// no field names). REAL done+errored shape (spec critical scenario):
    /// terminal round-1 errored evidence + trusted Failed lifetime — the
    /// row renders execution=failed, last_outcome=errored.
    fn stage_fixture_row_for_contract(data_dir: &Path) -> std::path::PathBuf {
        let dir = stage_peer(data_dir, "contract-fixture");
        let text = "---\nslug: contract-fixture\noutcome: errored\nupdated_unix: 100\nturn: 1\n---\n\nbody\n";
        crate::peers::peer_io::write_peer_file_atomic(&dir, "result-1.md", text).expect("result-1");
        crate::peers::peer_io::write_peer_file_atomic(&dir, "result.md", text).expect("result");
        crate::peers::peer_io::append_peer_line(&dir, "turns.txt", "1 errored 100\n")
            .expect("turns");
        let lifetime = serde_json::json!({
            "version": 1,
            "task_id": "task-contract-fixture",
            "registry_key": crate::peers::peer_wire_key("octos", "contract-fixture"),
            "master": "master-cf",
            "generation": 1,
            "phase": "failed",
            "turn_id": Some("t1"),
            "result_digest": null,
        });
        crate::peers::peer_io::write_peer_file_atomic(&dir, "lifetime.json", &lifetime.to_string())
            .expect("lifetime");
        crate::peers::peer_io::write_peer_file_atomic(&dir, "originator", "master-cf")
            .expect("originator");
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

        let rows = list_peers(temp.path(), "octos");
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

    /// Spec Decisions: `--data-dir` (arbitrary custom root) + `--profile
    /// <non-default>` must COMBINE — exercising the REAL CLI parse and the
    /// production routing (`route_peer_list`, the same fn `execute` calls),
    /// not a direct `list_peers(temp, profile)` call that bypasses the CLI
    /// layer (outer-loop review finding). A lifetime minted for profile
    /// "octosfix" under a custom root: trusted under --profile octosfix,
    /// UNKNOWN under the default — proving the profile reaches the
    /// projection validation intact while --data-dir supplies the root.
    #[test]
    fn peer_list_custom_data_dir_with_explicit_profile_combination() {
        use clap::Parser as _;
        // REAL clap parse of the full CLI surface: octos peer list
        // --data-dir <custom> --profile octosfix
        let custom_root = tempfile::tempdir().expect("custom root");
        let custom_str = custom_root.path().to_str().expect("utf8 path").to_owned();
        let parsed = crate::commands::Args::try_parse_from([
            "octos",
            "peer",
            "list",
            "--data-dir",
            &custom_str,
            "--profile",
            "octosfix",
        ])
        .expect("cli parse");
        let crate::commands::Command::Peer(peer) = parsed.command else {
            panic!("expected peer command");
        };
        #[expect(
            irrefutable_let_patterns,
            reason = "PeerAction is single-variant; the let-else documents the invariant for future subcommands"
        )]
        let crate::commands::peer::PeerAction::List(args) = peer.action else {
            panic!(
                "unreachable: PeerAction has only the List variant today; \
                    kept as a binding so adding a subcommand fails HERE, not silently"
            )
        };
        assert_eq!(args.profile.as_deref(), Some("octosfix"));
        assert_eq!(args.data_dir.as_deref(), Some(custom_root.path()));

        // Production routing: --data-dir wins for the root, --profile flows
        // through for projection validation (cwd/state_home irrelevant when
        // --data-dir is explicit).
        let state_home = tempfile::tempdir().expect("state home");
        let cwd = tempfile::tempdir().expect("cwd");
        let (data_dir, profile_id) = super::route_peer_list(&args, state_home.path(), cwd.path());
        assert_eq!(data_dir, custom_root.path());
        assert_eq!(profile_id, "octosfix");

        // And the routed pair feeds the projection: a lifetime minted for
        // "octosfix" under the custom root is trusted (running); the SAME
        // disk state under the DEFAULT profile fails registry_key
        // validation → fail-closed unknown + identity null.
        let peers_root = custom_root.path().join("peers");
        let dir = peers_root.join("combo");
        std::fs::create_dir_all(&dir).expect("peer dir");
        std::fs::write(dir.join("brief.md"), "task brief").expect("brief");
        std::fs::write(dir.join("originator"), "m1").expect("originator");
        let lifetime = serde_json::json!({
            "version": 1,
            "task_id": "task-combo",
            "registry_key": crate::peers::peer_wire_key("octosfix", "combo"),
            "master": "m1",
            "generation": 1,
            "phase": "running",
            "turn_id": "t-combo-1",
            "result_digest": null
        });
        std::fs::write(
            dir.join("lifetime.json"),
            serde_json::to_string(&lifetime).expect("lifetime json"),
        )
        .expect("lifetime");

        let rows = list_peers(&data_dir, &profile_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].execution, "running");

        let rows_default = list_peers(&data_dir, "octos");
        assert_eq!(rows_default.len(), 1);
        assert_eq!(rows_default[0].execution, "unknown");
        assert!(rows_default[0].master_session_id.is_none());
    }

    /// Empty / missing peers dir -> empty JSON array, exit-0 shape.
    #[test]
    fn olp_obs_peer_list_empty_dir_is_empty_array() {
        let temp = tempfile::tempdir().expect("tempdir");
        let rows = list_peers(temp.path(), "octos");
        assert!(rows.is_empty());
        assert_eq!(serde_json::to_string(&rows).expect("json"), "[]");
    }

    /// Spec filter: peer_list_fields_match_status_interface_contract —
    /// the serialized PeerListRow key set MUST equal the tracked contract
    /// docs/peer-status-interface.json's declared output_fields. A drift in
    /// either direction (missing or extra field) fails, so the JSON machine
    /// contract can never silently diverge from the tracked document.
    #[test]
    fn peer_list_fields_match_status_interface_contract() {
        let contract: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/peer-status-interface.json"
            ))
            .expect("tracked contract file"),
        )
        .expect("contract json");
        let mut expected: Vec<String> = contract["output_fields"]
            .as_object()
            .expect("output_fields object")
            .keys()
            .map(|k| k.to_string())
            .collect();
        expected.sort();

        let temp = tempfile::tempdir().expect("tempdir");
        // Serialize a NON-empty row: field visibility requires real values.
        // (An empty dir serializes to `[]`, so the key set is derived from a
        // staged fixture row.)
        let _dir = stage_fixture_row_for_contract(temp.path());
        let rows = list_peers(temp.path(), "octos");
        assert_eq!(rows.len(), 1);
        let mut actual: Vec<String> = serde_json::to_value(&rows)
            .expect("json")
            .as_array()
            .expect("rows array")
            .first()
            .expect("one row")
            .as_object()
            .expect("row object")
            .keys()
            .map(|k| k.to_string())
            .collect();
        actual.sort();
        assert_eq!(
            actual, expected,
            "PeerListRow serialized keys must equal the tracked contract"
        );
    }

    /// Spec filter: peer_list_json_and_table_share_assembly — --json and the
    /// human table are TWO RENDERINGS of the SAME rows. Calls the PRODUCTION
    /// renderer (render_table_line, the same fn print_table loops over) —
    /// the test never re-implements the mapping (outer-loop review caught a
    /// tautological earlier draft). Fixtures cover all three renderings:
    /// unknown execution → "?", null outcome → "-", real outcome as-is.
    #[test]
    fn peer_list_json_and_table_share_assembly() {
        let temp = tempfile::tempdir().expect("tempdir");
        // done + errored fixture (spec critical scenario): result.md exists
        // AND turns.txt tail is errored with a trusted Failed lifetime.
        let _dir = stage_fixture_row_for_contract(temp.path());
        let rows = list_peers(temp.path(), "octos");
        assert_eq!(rows.len(), 1);

        // JSON rendering: full contract field set present, raw values
        // (execution/last_outcome as-is; no borrow-after-move: serialize a
        // clone).
        let json = serde_json::to_value(&rows).expect("json");
        let obj = json[0].as_object().expect("row object");
        for field in [
            "slug",
            "status",
            "execution",
            "last_outcome",
            "round",
            "rounds_delivered",
            "has_brief",
            "result_versions",
            "name",
            "model_lane",
            "master_session_id",
            "task_id",
            "generation",
            "turn_id",
        ] {
            assert!(obj.contains_key(field), "json missing {field}");
        }

        // PRODUCTION table renderer over the same rows: the errored outcome
        // renders as-is — asserted COLUMN-WISE (tokens in header order:
        // SLUG/STATUS/CURRENT/OUTCOME/ROUNDS/NAME), never `any('-')` (NAME
        // may legitimately be '-' and would mask an OUTCOME mapping error).
        let line = super::render_table_line(&rows[0]);
        let tokens: Vec<&str> = line.split_whitespace().collect();
        assert!(tokens.len() >= 6, "six columns rendered: {line}");
        assert_eq!(tokens[0], "contract-fixture", "SLUG column: {line}");
        assert_eq!(
            tokens[2], "failed",
            "CURRENT column (trusted failed): {line}"
        );
        assert_eq!(
            tokens[3], "errored",
            "OUTCOME column (real outcome as-is): {line}"
        );

        // And the mapped renderings on synthetic rows via the SAME
        // production fn: unknown execution → "?" in CURRENT, null outcome
        // → "-" in OUTCOME — column-positional, so NAME="-" cannot mask it.
        let mut unknown_null = rows[0].clone();
        unknown_null.execution = "unknown".to_owned();
        unknown_null.last_outcome = None;
        unknown_null.name = None; // renders '-' in NAME — must not satisfy OUTCOME
        let mapped = super::render_table_line(&unknown_null);
        let mtokens: Vec<&str> = mapped.split_whitespace().collect();
        assert_eq!(mtokens[2], "?", "CURRENT maps unknown→'?': {mapped}");
        assert_eq!(mtokens[3], "-", "OUTCOME maps null→'-': {mapped}");

        // Shared-assembly invariant: the JSON object was serialized from
        // the SAME rows the renderer consumes.
        assert_eq!(json[0]["slug"].as_str().unwrap(), rows[0].slug);
        assert_eq!(json[0]["execution"].as_str().unwrap(), rows[0].execution);
    }
}
