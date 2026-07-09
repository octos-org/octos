//! Clean command: remove stale state files.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};

use super::Executable;

/// Clean up stale state and cache files.
#[derive(Debug, Args)]
pub struct CleanCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Remove database files as well.
    #[arg(long)]
    pub all: bool,

    /// Dry run - show what would be deleted without actually deleting.
    #[arg(long)]
    pub dry_run: bool,
}

impl Executable for CleanCommand {
    fn execute(self) -> Result<()> {
        println!("{}", "octos clean".cyan().bold());
        println!();

        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let data_dir = cwd.join(".octos");

        if !data_dir.exists() {
            println!("{}", "No .octos directory found.".yellow());
            return Ok(());
        }

        let mut paths_to_remove = collect_orphaned_worker_worktrees(&cwd, &data_dir)?;
        let mut total_size: u64 = 0;
        for path in &paths_to_remove {
            total_size += path_size(path);
        }

        // Find database files if --all
        if self.all {
            for entry in std::fs::read_dir(&data_dir)? {
                let entry = entry?;
                let path = entry.path();

                if path.is_file() {
                    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
                    // Remove .redb database files
                    if ext.as_deref() == Some("redb") {
                        if let Ok(meta) = entry.metadata() {
                            total_size += meta.len();
                        }
                        paths_to_remove.push(path);
                    }
                }
            }
        }

        if paths_to_remove.is_empty() {
            println!("{}", "Nothing to clean.".green());
            return Ok(());
        }

        // Format size
        let size_str = if total_size > 1024 * 1024 {
            format!("{:.1} MB", total_size as f64 / (1024.0 * 1024.0))
        } else if total_size > 1024 {
            format!("{:.1} KB", total_size as f64 / 1024.0)
        } else {
            format!("{} bytes", total_size)
        };

        println!(
            "{} {} paths ({}):",
            if self.dry_run {
                "Would remove"
            } else {
                "Removing"
            },
            paths_to_remove.len(),
            size_str
        );
        println!();

        for path in &paths_to_remove {
            let relative = path.strip_prefix(&cwd).unwrap_or(path);
            println!("  {}", relative.display());
        }
        println!();

        if self.dry_run {
            println!("{}", "Dry run - no files were deleted.".yellow());
            println!("Run without --dry-run to actually delete files.");
        } else {
            for path in &paths_to_remove {
                if path.is_dir() {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
            }

            println!(
                "{} {} paths, freed {}",
                "Cleaned".green(),
                paths_to_remove.len(),
                size_str
            );
        }

        Ok(())
    }
}

fn collect_orphaned_worker_worktrees(
    cwd: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<Vec<PathBuf>> {
    let work_root = data_dir.join("work");
    if !work_root.exists() {
        return Ok(Vec::new());
    }

    let active = active_git_worktree_paths(cwd).unwrap_or_default();
    let mut orphaned = Vec::new();
    for entry in std::fs::read_dir(&work_root)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let canonical = canonicalize_lossy(&path);
        if !active.contains(&canonical) {
            orphaned.push(path);
        }
    }
    Ok(orphaned)
}

fn active_git_worktree_paths(cwd: &std::path::Path) -> Result<HashSet<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .wrap_err("failed to run git worktree list")?;
    if !output.status.success() {
        return Err(eyre::eyre!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .map(|path| canonicalize_lossy(&path))
        .collect())
}

fn canonicalize_lossy(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn path_size(path: &std::path::Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|meta| meta.len()).unwrap_or(0);
    }
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        total += path_size(&entry.path());
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collects_orphaned_worker_worktrees_only() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        std::fs::create_dir_all(cwd.join(".octos/work/orphan/.octos")).unwrap();
        std::fs::write(cwd.join(".octos/work/orphan/file.txt"), "leftover").unwrap();

        let orphaned = collect_orphaned_worker_worktrees(cwd, &cwd.join(".octos")).unwrap();
        assert_eq!(orphaned, vec![cwd.join(".octos/work/orphan")]);
    }
}
