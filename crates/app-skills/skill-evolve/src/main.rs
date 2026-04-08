//! Skill evolution: online self-correction for plugin skills.
//!
//! Two modes:
//! - **Hook mode** (`--hook`): receives `after_tool_call` payload on stdin,
//!   detects failures, generates SKILL.md improvement patches via LLM.
//! - **Tool mode** (standard plugin protocol): `./main skill_evolve < json`
//!   for listing, applying, or discarding pending patches.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Subset of the hook payload we care about.
#[derive(Deserialize)]
struct HookPayload {
    tool_name: Option<String>,
    result: Option<String>,
    success: Option<bool>,
}

/// A pending evolution patch (stored in evolutions.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvolutionPatch {
    tool_name: String,
    error_excerpt: String,
    suggestion: String,
    timestamp: String,
}

/// Per-skill evolution store.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EvolutionStore {
    #[serde(default)]
    patches: Vec<EvolutionPatch>,
}

/// Tool invocation arguments.
#[derive(Deserialize)]
struct ToolArgs {
    action: String,
    #[serde(default)]
    skill: String,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum pending patches per skill before oldest are evicted.
const MAX_PATCHES: usize = 20;

/// Minimum seconds between patches for the same skill.
const COOLDOWN_SECS: i64 = 600; // 10 minutes

/// Maximum characters of error output sent to the LLM.
const MAX_ERROR_LEN: usize = 800;

/// Maximum characters of SKILL.md sent to the LLM.
const MAX_SKILL_MD_LEN: usize = 4000;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--hook") {
        run_hook();
    } else {
        run_tool(&args);
    }
}

// ---------------------------------------------------------------------------
// Hook mode
// ---------------------------------------------------------------------------

fn run_hook() {
    let payload = match read_stdin_payload::<HookPayload>() {
        Some(p) => p,
        None => return,
    };

    // Fast path: skip successes.
    if payload.success.unwrap_or(true) {
        return;
    }

    let tool_name = match payload.tool_name {
        Some(ref n) if !n.is_empty() => n.clone(),
        _ => return,
    };

    let error_output = match payload.result {
        Some(ref r) if !r.is_empty() => r.clone(),
        _ => return,
    };

    // Locate the skills directories.
    let skills_dirs = resolve_skills_dirs();
    if skills_dirs.is_empty() {
        return;
    }

    // Reverse-map tool_name -> (skill_name, skill_dir).
    let (skill_name, skill_dir) = match find_skill_for_tool(&skills_dirs, &tool_name) {
        Some(v) => v,
        None => return, // not a plugin tool
    };

    // Read SKILL.md.
    let skill_md_path = skill_dir.join("SKILL.md");
    let skill_content = fs::read_to_string(&skill_md_path).unwrap_or_default();
    if skill_content.is_empty() {
        return;
    }

    // Cooldown check.
    let store_path = skill_dir.join("evolutions.json");
    let store = load_store(&store_path);
    if is_on_cooldown(&store) {
        return;
    }

    // Call LLM.
    let suggestion = match generate_suggestion(&skill_name, &tool_name, &error_output, &skill_content)
    {
        Some(s) => s,
        None => return,
    };

    // Persist patch.
    let patch = EvolutionPatch {
        tool_name,
        error_excerpt: truncate(&error_output, 200),
        suggestion,
        timestamp: Utc::now().to_rfc3339(),
    };

    let mut store = store;
    store.patches.push(patch);
    if store.patches.len() > MAX_PATCHES {
        store.patches.drain(0..store.patches.len() - MAX_PATCHES);
    }
    let _ = fs::write(&store_path, serde_json::to_string_pretty(&store).unwrap_or_default());
}

fn is_on_cooldown(store: &EvolutionStore) -> bool {
    let Some(last) = store.patches.last() else {
        return false;
    };
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&last.timestamp) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(ts);
    age.num_seconds() < COOLDOWN_SECS
}

// ---------------------------------------------------------------------------
// Tool mode
// ---------------------------------------------------------------------------

fn run_tool(args: &[String]) {
    let tool_name = args.get(1).map(String::as_str).unwrap_or("");
    if tool_name != "skill_evolve" {
        print_result(false, &format!("unknown tool: {tool_name}"));
        return;
    }

    let tool_args: ToolArgs = match read_stdin_payload() {
        Some(a) => a,
        None => {
            print_result(false, "failed to parse input");
            return;
        }
    };

    let skills_dirs = resolve_skills_dirs();

    match tool_args.action.as_str() {
        "list" => cmd_list(&skills_dirs),
        "apply" => cmd_apply(&skills_dirs, &tool_args.skill),
        "discard" => cmd_discard(&skills_dirs, &tool_args.skill),
        other => print_result(false, &format!("unknown action: {other}")),
    }
}

fn cmd_list(skills_dirs: &[PathBuf]) {
    let mut output = String::new();
    let mut total = 0;

    for dir in skills_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let store_path = entry.path().join("evolutions.json");
            let store = load_store(&store_path);
            if store.patches.is_empty() {
                continue;
            }
            let skill_name = entry.file_name().to_string_lossy().to_string();
            output.push_str(&format!("## {} ({} pending)\n", skill_name, store.patches.len()));
            for patch in &store.patches {
                output.push_str(&format!(
                    "- [{}] tool `{}`: {}\n  Error: {}\n",
                    &patch.timestamp[..10],
                    patch.tool_name,
                    patch.suggestion,
                    patch.error_excerpt,
                ));
            }
            output.push('\n');
            total += store.patches.len();
        }
    }

    if output.is_empty() {
        print_result(true, "No pending evolution patches.");
    } else {
        output.insert_str(0, &format!("Total: {} pending patches\n\n", total));
        print_result(true, &output);
    }
}

fn cmd_apply(skills_dirs: &[PathBuf], skill: &str) {
    if skill.is_empty() {
        print_result(false, "skill name required for apply");
        return;
    }

    let Some(skill_dir) = find_skill_dir(skills_dirs, skill) else {
        print_result(false, &format!("skill '{skill}' not found"));
        return;
    };

    let store_path = skill_dir.join("evolutions.json");
    let store = load_store(&store_path);
    if store.patches.is_empty() {
        print_result(true, &format!("No pending patches for '{skill}'."));
        return;
    }

    // Append to SKILL.md under ## Learned Notes
    let skill_md_path = skill_dir.join("SKILL.md");
    let mut content = fs::read_to_string(&skill_md_path).unwrap_or_default();

    if !content.contains("## Learned Notes") {
        content.push_str("\n\n## Learned Notes\n");
    }
    for patch in &store.patches {
        content.push_str(&format!("- {}\n", patch.suggestion));
    }

    if fs::write(&skill_md_path, &content).is_err() {
        print_result(false, "failed to write SKILL.md");
        return;
    }

    // Clear store.
    let count = store.patches.len();
    let _ = fs::write(
        &store_path,
        serde_json::to_string_pretty(&EvolutionStore::default()).unwrap_or_default(),
    );
    print_result(true, &format!("Applied {count} patches to {skill}/SKILL.md"));
}

fn cmd_discard(skills_dirs: &[PathBuf], skill: &str) {
    if skill.is_empty() {
        print_result(false, "skill name required for discard");
        return;
    }

    let Some(skill_dir) = find_skill_dir(skills_dirs, skill) else {
        print_result(false, &format!("skill '{skill}' not found"));
        return;
    };

    let store_path = skill_dir.join("evolutions.json");
    let _ = fs::write(
        &store_path,
        serde_json::to_string_pretty(&EvolutionStore::default()).unwrap_or_default(),
    );
    print_result(true, &format!("Discarded patches for '{skill}'."));
}

// ---------------------------------------------------------------------------
// LLM call
// ---------------------------------------------------------------------------

fn generate_suggestion(
    skill_name: &str,
    tool_name: &str,
    error: &str,
    skill_md: &str,
) -> Option<String> {
    let (endpoint, key, model) = resolve_llm_config()?;

    let error_trunc = truncate(error, MAX_ERROR_LEN);
    let skill_trunc = truncate(skill_md, MAX_SKILL_MD_LEN);

    let prompt = format!(
        r#"A tool "{tool_name}" from skill "{skill_name}" failed with this error:

```
{error_trunc}
```

The current SKILL.md for this skill is:

```
{skill_trunc}
```

Based on the error, suggest ONE concise instruction (1-2 sentences) to add to SKILL.md that would prevent this failure in the future. Focus on model-specific quirks, input format requirements, or edge cases the LLM should know about.

Reply with ONLY the instruction text, nothing else. If the error is transient (network timeout, rate limit, 429, 503) or not fixable via prompt changes, reply with exactly "SKIP"."#
    );

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 200,
        "temperature": 0.3,
    });

    let response = reqwest::blocking::Client::new()
        .post(format!("{endpoint}/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let json: serde_json::Value = response.json().ok()?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()?
        .trim()
        .to_string();

    if text.eq_ignore_ascii_case("SKIP") || text.is_empty() || text.len() < 10 {
        return None;
    }

    Some(text)
}

/// Try env vars in priority order — prefer cheap/fast models.
fn resolve_llm_config() -> Option<(String, String, String)> {
    let configs: &[(&str, &str, &str)] = &[
        (
            "DEEPSEEK_API_KEY",
            "https://api.deepseek.com/v1",
            "deepseek-chat",
        ),
        (
            "KIMI_API_KEY",
            "https://api.moonshot.ai/v1",
            "kimi-2.5",
        ),
        (
            "DASHSCOPE_API_KEY",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen-plus",
        ),
        (
            "OPENAI_API_KEY",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ),
        (
            "GEMINI_API_KEY",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-2.0-flash",
        ),
    ];
    for &(env_var, endpoint, model) in configs {
        if let Ok(key) = std::env::var(env_var) {
            if !key.is_empty() {
                return Some((endpoint.to_string(), key, model.to_string()));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Skill directory helpers
// ---------------------------------------------------------------------------

/// Resolve all skill directories (bundled + per-profile).
fn resolve_skills_dirs() -> Vec<PathBuf> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return vec![],
    };
    let octos_home = home.join(".octos");
    let mut dirs = Vec::new();

    // Layer 2: bundled app-skills
    let bundled = octos_home.join("bundled-app-skills");
    if bundled.is_dir() {
        dirs.push(bundled);
    }

    // Layer 3: per-profile skills (scan all profiles)
    let profiles_dir = octos_home.join("profiles");
    if let Ok(entries) = fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            let skills = entry.path().join("skills");
            if skills.is_dir() {
                dirs.push(skills);
            }
        }
    }

    // Legacy: direct skills dir
    let legacy = octos_home.join("skills");
    if legacy.is_dir() {
        dirs.push(legacy);
    }

    dirs
}

/// Find which skill owns a given tool by scanning manifest.json files.
fn find_skill_for_tool(skills_dirs: &[PathBuf], tool_name: &str) -> Option<(String, PathBuf)> {
    for dir in skills_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let manifest_path = entry.path().join("manifest.json");
            let data = match fs::read_to_string(&manifest_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let manifest: serde_json::Value = match serde_json::from_str(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Skip our own skill to avoid self-evolution loops.
            if manifest["name"].as_str() == Some("skill-evolve") {
                continue;
            }
            if let Some(tools) = manifest["tools"].as_array() {
                for tool in tools {
                    if tool["name"].as_str() == Some(tool_name) {
                        let name = manifest["name"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_string();
                        return Some((name, entry.path()));
                    }
                }
            }
        }
    }
    None
}

/// Find a skill directory by name.
fn find_skill_dir(skills_dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for dir in skills_dirs {
        let candidate = dir.join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn read_stdin_payload<T: serde::de::DeserializeOwned>() -> Option<T> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    serde_json::from_str(&input).ok()
}

fn load_store(path: &Path) -> EvolutionStore {
    fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn print_result(success: bool, output: &str) {
    let result = serde_json::json!({
        "success": success,
        "output": output,
    });
    println!("{result}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_hook_payload_with_failure() {
        let json = r#"{"tool_name":"web_search","result":"Error: timeout","success":false}"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.tool_name.as_deref(), Some("web_search"));
        assert_eq!(payload.success, Some(false));
        assert_eq!(payload.result.as_deref(), Some("Error: timeout"));
    }

    #[test]
    fn should_skip_successful_payload() {
        let json = r#"{"tool_name":"shell","result":"ok","success":true}"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert!(payload.success.unwrap_or(true));
    }

    #[test]
    fn should_detect_cooldown() {
        let store = EvolutionStore {
            patches: vec![EvolutionPatch {
                tool_name: "test".into(),
                error_excerpt: "err".into(),
                suggestion: "fix".into(),
                timestamp: Utc::now().to_rfc3339(),
            }],
        };
        assert!(is_on_cooldown(&store));
    }

    #[test]
    fn should_not_cooldown_when_empty() {
        let store = EvolutionStore::default();
        assert!(!is_on_cooldown(&store));
    }

    #[test]
    fn should_not_cooldown_when_old() {
        let old = Utc::now() - chrono::Duration::seconds(COOLDOWN_SECS + 60);
        let store = EvolutionStore {
            patches: vec![EvolutionPatch {
                tool_name: "test".into(),
                error_excerpt: "err".into(),
                suggestion: "fix".into(),
                timestamp: old.to_rfc3339(),
            }],
        };
        assert!(!is_on_cooldown(&store));
    }

    #[test]
    fn should_truncate_at_utf8_boundary() {
        let s = "hello 世界 world";
        let t = truncate(s, 8);
        assert!(t.ends_with("..."));
        assert!(t.len() <= 12); // 8 + "..."
    }

    #[test]
    fn should_not_truncate_short_string() {
        let s = "hello";
        assert_eq!(truncate(s, 10), "hello");
    }

    #[test]
    fn should_serialize_evolution_store() {
        let store = EvolutionStore {
            patches: vec![EvolutionPatch {
                tool_name: "web_search".into(),
                error_excerpt: "timeout".into(),
                suggestion: "Use specific keywords".into(),
                timestamp: "2026-04-07T12:00:00Z".into(),
            }],
        };
        let json = serde_json::to_string_pretty(&store).unwrap();
        assert!(json.contains("web_search"));
        assert!(json.contains("Use specific keywords"));
    }

    #[test]
    fn should_deserialize_empty_store() {
        let store: EvolutionStore = serde_json::from_str("{}").unwrap();
        assert!(store.patches.is_empty());
    }

    #[test]
    fn should_cap_patches_at_max() {
        let mut store = EvolutionStore::default();
        for i in 0..MAX_PATCHES + 5 {
            store.patches.push(EvolutionPatch {
                tool_name: format!("tool_{i}"),
                error_excerpt: "err".into(),
                suggestion: "fix".into(),
                timestamp: Utc::now().to_rfc3339(),
            });
        }
        if store.patches.len() > MAX_PATCHES {
            store.patches.drain(0..store.patches.len() - MAX_PATCHES);
        }
        assert_eq!(store.patches.len(), MAX_PATCHES);
        // Oldest should be evicted.
        assert_eq!(store.patches[0].tool_name, "tool_5");
    }

    #[test]
    fn should_parse_tool_args() {
        let json = r#"{"action":"apply","skill":"news"}"#;
        let args: ToolArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.action, "apply");
        assert_eq!(args.skill, "news");
    }

    #[test]
    fn should_parse_tool_args_without_skill() {
        let json = r#"{"action":"list"}"#;
        let args: ToolArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.action, "list");
        assert!(args.skill.is_empty());
    }

    #[test]
    fn should_find_skill_in_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("manifest.json"),
            r#"{"name":"my-skill","version":"1.0","tools":[{"name":"my_tool","description":"test"}]}"#,
        ).unwrap();

        let result = find_skill_for_tool(&[dir.path().to_path_buf()], "my_tool");
        assert!(result.is_some());
        let (name, path) = result.unwrap();
        assert_eq!(name, "my-skill");
        assert_eq!(path, skill_dir);
    }

    #[test]
    fn should_not_find_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_skill_for_tool(&[dir.path().to_path_buf()], "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn should_skip_self_evolution() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skill-evolve");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("manifest.json"),
            r#"{"name":"skill-evolve","version":"0.1","tools":[{"name":"skill_evolve","description":"test"}]}"#,
        ).unwrap();

        let result = find_skill_for_tool(&[dir.path().to_path_buf()], "skill_evolve");
        assert!(result.is_none(), "should skip self to avoid evolution loops");
    }

    #[test]
    fn should_apply_patches_to_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "# Test Skill\n\nDoes stuff.\n").unwrap();
        fs::write(
            skill_dir.join("evolutions.json"),
            serde_json::to_string(&EvolutionStore {
                patches: vec![
                    EvolutionPatch {
                        tool_name: "my_tool".into(),
                        error_excerpt: "timeout".into(),
                        suggestion: "Use timeout of 30s for slow APIs".into(),
                        timestamp: "2026-04-07T12:00:00Z".into(),
                    },
                    EvolutionPatch {
                        tool_name: "my_tool".into(),
                        error_excerpt: "parse error".into(),
                        suggestion: "Always return valid JSON".into(),
                        timestamp: "2026-04-07T13:00:00Z".into(),
                    },
                ],
            })
            .unwrap(),
        )
        .unwrap();

        // Simulate apply.
        let store_path = skill_dir.join("evolutions.json");
        let store = load_store(&store_path);
        let skill_md_path = skill_dir.join("SKILL.md");
        let mut content = fs::read_to_string(&skill_md_path).unwrap();

        if !content.contains("## Learned Notes") {
            content.push_str("\n\n## Learned Notes\n");
        }
        for patch in &store.patches {
            content.push_str(&format!("- {}\n", patch.suggestion));
        }
        fs::write(&skill_md_path, &content).unwrap();

        let result = fs::read_to_string(&skill_md_path).unwrap();
        assert!(result.contains("## Learned Notes"));
        assert!(result.contains("Use timeout of 30s for slow APIs"));
        assert!(result.contains("Always return valid JSON"));
        assert!(result.contains("# Test Skill")); // original content preserved
    }
}
