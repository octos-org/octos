//! Validate a skills directory using the crew-plugin SDK.
//!
//! Usage:
//!   cargo run -p crew-plugin --example validate_skills -- /path/to/skills
//!
//! Validates each subdirectory for:
//! - manifest.json presence and parse-ability
//! - Required fields (id, version)
//! - Tool definitions (if type=tool)
//! - SKILL.md presence
//! - Gating requirements against current environment

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crew_plugin::gating;
use crew_plugin::manifest::{PluginManifest, PluginType};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let skills_dir = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        eprintln!("Usage: validate_skills <skills-directory>");
        return ExitCode::FAILURE;
    };

    if !skills_dir.is_dir() {
        eprintln!("Error: {} is not a directory", skills_dir.display());
        return ExitCode::FAILURE;
    }

    let env_vars: HashMap<String, String> = std::env::vars().collect();

    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut warnings = 0;

    // Collect subdirectories, sort for deterministic output.
    let mut entries: Vec<_> = std::fs::read_dir(&skills_dir)
        .expect("cannot read skills directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let dir = entry.path();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();

        // Skip hidden directories.
        if name.starts_with('.') {
            continue;
        }

        let manifest_path = dir.join("manifest.json");
        if !manifest_path.exists() {
            // Not a plugin directory — check if it has SKILL.md (skill-only).
            let skill_md = dir.join("SKILL.md");
            if skill_md.exists() {
                println!("⚠  {name}: SKILL.md present but no manifest.json (skill-only, no tools)",);
                warnings += 1;
            }
            continue;
        }

        total += 1;
        print!("   {name}: ");

        // 1. Parse manifest.
        let manifest = match PluginManifest::from_file(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                println!("FAIL — manifest parse error: {e}");
                failed += 1;
                continue;
            }
        };

        let mut issues: Vec<String> = Vec::new();
        let mut warns: Vec<String> = Vec::new();

        // 2. Check id matches directory name.
        if manifest.id != name {
            warns.push(format!(
                "id '{}' does not match directory name '{name}'",
                manifest.id
            ));
        }

        // 3. Effective type + tool validation.
        let etype = manifest.effective_type();
        match etype {
            PluginType::Tool => {
                if manifest.tools.is_empty() {
                    issues.push("type inferred as Tool but no tools defined".into());
                }
                for tool in &manifest.tools {
                    if tool.description.is_empty() {
                        warns.push(format!("tool '{}' has empty description", tool.name));
                    }
                    if tool.input_schema.is_null() || tool.input_schema == serde_json::json!({}) {
                        warns.push(format!("tool '{}' has empty input_schema", tool.name));
                    }
                }
            }
            PluginType::Skill => {
                // Skills should have SKILL.md
            }
            PluginType::Hook => {
                if manifest.hooks.is_empty() {
                    issues.push("type is Hook but no hooks defined".into());
                }
            }
            PluginType::Channel => {}
        }

        // 4. Check SKILL.md presence.
        let skill_md = dir.join("SKILL.md");
        if !skill_md.exists() {
            warns.push("missing SKILL.md".into());
        }

        // 5. Check .dot pipeline files (mofa-skills convention).
        let dot_files: Vec<_> = std::fs::read_dir(&dir)
            .ok()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .map(|ext| ext == "dot")
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 6. Gating check.
        let gating_info = if let Some(ref reqs) = manifest.requires {
            let result = gating::check_requirements(reqs, &env_vars);
            if !result.passed {
                warns.push(format!("gating: {}", result.summary));
            }
            Some(result)
        } else {
            None
        };

        // 7. Version sanity.
        if manifest.version == "0.0.0" {
            warns.push("version is 0.0.0 (placeholder?)".into());
        }

        // Report.
        if issues.is_empty() {
            println!(
                "OK ({})",
                format_summary(&manifest, &etype, dot_files.len())
            );
            passed += 1;
        } else {
            println!("FAIL");
            for issue in &issues {
                println!("      ✗ {issue}");
            }
            failed += 1;
        }

        for w in &warns {
            println!("      ⚠ {w}");
        }

        if let Some(ref gr) = gating_info {
            for check in &gr.checks {
                let icon = if check.passed { "✓" } else { "✗" };
                println!("      {icon} {}", check.detail);
            }
        }
    }

    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Total: {total}  Passed: {passed}  Failed: {failed}  Warnings: {warnings}");

    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn format_summary(m: &PluginManifest, etype: &PluginType, dot_count: usize) -> String {
    let mut parts = vec![format!("v{}", m.version), format!("{etype:?}")];
    if !m.tools.is_empty() {
        parts.push(format!("{} tool(s)", m.tools.len()));
    }
    if dot_count > 0 {
        parts.push(format!("{dot_count} pipeline(s)"));
    }
    if m.timeout_secs.is_some() {
        parts.push(format!("timeout={}s", m.timeout_secs.unwrap()));
    }
    parts.join(", ")
}
