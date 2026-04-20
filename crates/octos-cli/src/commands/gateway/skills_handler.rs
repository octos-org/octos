//! /skills command handler for inline skill management.

use std::sync::Arc;

/// Handle /skills command — list, install, remove skills.
pub async fn handle_skills_command(
    args: &str,
    profile_id: Option<&str>,
    data_dir: &std::path::Path,
    profile_store: &Option<Arc<crate::profiles::ProfileStore>>,
) -> String {
    // Resolve skills directory: profile-based or data_dir fallback
    let skills_dir = if let (Some(pid), Some(store)) = (profile_id, profile_store) {
        match crate::commands::skills::resolve_profile_skills_dir(store, pid) {
            Ok(d) => d,
            Err(e) => return format!("Error resolving skills dir: {e}"),
        }
    } else {
        data_dir.join("skills")
    };

    let parts: Vec<&str> = args.splitn(3, ' ').collect();
    match parts.first().copied().unwrap_or("list") {
        "" | "list" => match crate::commands::skills::list_skills(&skills_dir) {
            Ok(entries) if entries.is_empty() => {
                "No skills installed.\nInstall with: /skills install <user/repo | git-url | local-path>".to_string()
            }
            Ok(entries) => {
                let mut lines = vec![format!("{} skill(s) installed:", entries.len())];
                for e in &entries {
                    let ver = e
                        .version
                        .as_deref()
                        .map(|v| format!(" v{v}"))
                        .unwrap_or_default();
                    let src = e
                        .source_repo
                        .as_deref()
                        .map(|s| format!(" (from {s})"))
                        .unwrap_or_default();
                    let tools = if e.tool_count > 0 {
                        format!(" [{} tool(s)]", e.tool_count)
                    } else {
                        String::new()
                    };
                    lines.push(format!("  {}{}{}{}", e.name, ver, tools, src));
                }
                lines.join("\n")
            }
            Err(e) => format!("Error: {e}"),
        },

        "install" => {
            let repo = parts.get(1).copied().unwrap_or("").trim();
            if repo.is_empty() {
                return "Usage: /skills install <user/repo | git-url | local-path>".to_string();
            }
            let skills_dir_c = skills_dir.clone();
            let repo_c = repo.to_string();
            match tokio::task::spawn_blocking(move || {
                crate::commands::skills::install_skill(&skills_dir_c, &repo_c, false, "main")
            })
            .await
            {
                Ok(Ok(result)) => {
                    let mut parts = Vec::new();
                    if !result.installed.is_empty() {
                        parts.push(format!("Installed: {}", result.installed.join(", ")));
                    }
                    if !result.deps_installed.is_empty() {
                        parts.push(format!(
                            "Dependencies: {}",
                            result.deps_installed.join(", ")
                        ));
                    }
                    if !result.skipped.is_empty() {
                        parts.push(format!(
                            "Skipped (already exists): {}",
                            result.skipped.join(", ")
                        ));
                    }
                    if parts.is_empty() {
                        "No skills found in repository.".to_string()
                    } else {
                        parts.join("\n")
                    }
                }
                Ok(Err(e)) => format!("Install failed: {e}"),
                Err(e) => format!("Install task failed: {e}"),
            }
        }

        "remove" => {
            let name = parts.get(1).copied().unwrap_or("").trim();
            if name.is_empty() {
                return "Usage: /skills remove <name>".to_string();
            }
            match crate::commands::skills::remove_skill(&skills_dir, name) {
                Ok(()) => format!("Removed skill: {name}"),
                Err(e) => format!("Error: {e}"),
            }
        }

        other => format!(
            "Unknown /skills subcommand: {other}\nUsage:\n  /skills — list installed skills\n  /skills install <user/repo | git-url | local-path> — install a skill\n  /skills remove <name> — remove a skill"
        ),
    }
}
