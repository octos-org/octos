//! `octos inbox` — machine-addressable inbox notes paths (OLP L1, slice 1).
//!
//! Contract: task-req-olp-obs-cli.spec.md — "外部进程查询 inbox 路径".
//! External consumers (the outer loop) must NEVER re-implement the inbox
//! filename hash (`DefaultHasher` is explicitly unstable across Rust
//! releases); they address notes files through this command. The hash
//! itself stays an internal detail — we REUSE
//! [`crate::autonomy::hash_session_for_inbox`] so the printed path is by
//! construction identical to the path serve writes wake notes to.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use eyre::Result;

use super::Executable;

#[derive(Debug, Args)]
pub struct InboxCommand {
    #[command(subcommand)]
    pub action: InboxAction,
}

#[derive(Debug, Subcommand)]
pub enum InboxAction {
    /// Print the absolute path of a session's inbox notes file.
    Path(InboxPathArgs),
}

#[derive(Debug, Args)]
pub struct InboxPathArgs {
    /// Session key (e.g. `octos:local:tui#coding`).
    #[arg(long)]
    pub session: String,
    /// Data-dir override (defaults to the standard resolution:
    /// `OCTOS_HOME` > `~/.octos`).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

/// Resolve the inbox notes path for `session` under `data_dir` —
/// `<data_dir>/inbox/<hash_session_for_inbox(session)>.notes`. Pure: no
/// filesystem access, so the "matches serve" test can compare against the
/// real writer's path computation directly.
pub(crate) fn inbox_notes_path(data_dir: &std::path::Path, session: &str) -> PathBuf {
    data_dir.join("inbox").join(format!(
        "{}.notes",
        crate::autonomy::hash_session_for_inbox(session)
    ))
}

impl Executable for InboxCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            InboxAction::Path(args) => {
                let data_dir = super::resolve_data_dir(args.data_dir)?;
                let path = inbox_notes_path(&data_dir, &args.session);
                // Absolute path, one line, nothing else — safe for $(...)
                // capture. The path is always absolute because
                // resolve_data_dir returns an absolute dir.
                println!("{}", path.display());
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Contract scenario "外部进程查询 inbox 路径": the path this command
    /// prints must equal the path serve actually writes wake notes to.
    /// Serve computes `data_dir.join("inbox").join(format!("{safe}.notes"))`
    /// with `safe = hash_session_for_inbox(session)` — the same building
    /// blocks this command uses. Assert both the shape and the exact hash
    /// so a future hash-algorithm change breaks BOTH sides' tests at once.
    #[test]
    fn olp_obs_inbox_path_matches_serve() {
        let data_dir = PathBuf::from("/tmp/olp-obs-test");
        let session = "octos:local:tui#coding";
        let via_command = inbox_notes_path(&data_dir, session);
        // Reproduce serve's write path EXACTLY as in
        // `api/ui_protocol_transport.rs::write_goal_wake_note`.
        let serve_side = data_dir.join("inbox").join(format!(
            "{}.notes",
            crate::autonomy::hash_session_for_inbox(session)
        ));
        assert_eq!(via_command, serve_side);
        assert!(via_command.is_absolute());
        assert!(
            via_command
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .ends_with(".notes")
        );
    }
}
