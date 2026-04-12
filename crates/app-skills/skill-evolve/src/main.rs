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

/// Maximum applied notes kept in SKILL.md `## Learned Notes` section.
/// Oldest notes are dropped when this limit is exceeded.
const MAX_APPLIED_NOTES: usize = 10;

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
    let suggestion =
        match generate_suggestion(&skill_name, &tool_name, &error_output, &skill_content) {
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
    let _ = fs::write(
        &store_path,
        serde_json::to_string_pretty(&store).unwrap_or_default(),
    );
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
        "consolidate" => cmd_consolidate(&skills_dirs, &tool_args.skill),
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
            output.push_str(&format!(
                "## {} ({} pending)\n",
                skill_name,
                store.patches.len()
            ));
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

    // Rebuild SKILL.md with capped Learned Notes section.
    let skill_md_path = skill_dir.join("SKILL.md");
    let content = fs::read_to_string(&skill_md_path).unwrap_or_default();

    // Split content into body (before ## Learned Notes) and existing notes.
    let (body, existing_notes) = split_learned_notes(&content);

    // Collect existing + new notes, dedup, cap at MAX_APPLIED_NOTES (keep newest).
    let mut all_notes: Vec<String> = existing_notes;
    for patch in &store.patches {
        let note = patch.suggestion.clone();
        // Dedup: skip if normalized text matches an existing note exactly.
        // We only deduplicate exact matches (after lowercasing + trimming) to avoid
        // false positives from aggressive substring matching.
        let note_normalized = note.trim().to_lowercase();
        let is_duplicate = all_notes
            .iter()
            .any(|existing| existing.trim().to_lowercase() == note_normalized);
        if !is_duplicate {
            all_notes.push(note);
        }
    }
    // Keep only the last MAX_APPLIED_NOTES (newest).
    if all_notes.len() > MAX_APPLIED_NOTES {
        all_notes.drain(0..all_notes.len() - MAX_APPLIED_NOTES);
    }

    // Reassemble SKILL.md.
    let mut new_content = body.to_string();
    if !all_notes.is_empty() {
        new_content.push_str("\n\n## Learned Notes\n");
        for note in &all_notes {
            new_content.push_str(&format!("- {}\n", note));
        }
    }

    if fs::write(&skill_md_path, &new_content).is_err() {
        print_result(false, "failed to write SKILL.md");
        return;
    }

    // Clear store.
    let count = store.patches.len();
    let _ = fs::write(
        &store_path,
        serde_json::to_string_pretty(&EvolutionStore::default()).unwrap_or_default(),
    );
    print_result(
        true,
        &format!("Applied {count} patches to {skill}/SKILL.md"),
    );
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

fn cmd_consolidate(skills_dirs: &[PathBuf], skill: &str) {
    if skill.is_empty() {
        print_result(false, "skill name required for consolidate");
        return;
    }

    let Some(skill_dir) = find_skill_dir(skills_dirs, skill) else {
        print_result(false, &format!("skill '{skill}' not found"));
        return;
    };

    let skill_md_path = skill_dir.join("SKILL.md");
    let content = fs::read_to_string(&skill_md_path).unwrap_or_default();
    let (body, notes) = split_learned_notes(&content);

    if notes.len() < 3 {
        print_result(true, "Not enough notes to consolidate (need at least 3).");
        return;
    }

    let (endpoint, key, model) = match resolve_llm_config() {
        Some(c) => c,
        None => {
            print_result(false, "no LLM API key found in environment");
            return;
        }
    };

    let notes_text = notes
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{}. {}", i + 1, n))
        .collect::<Vec<_>>()
        .join("\n");

    let body_trunc = truncate(body, MAX_SKILL_MD_LEN);

    let prompt = format!(
        r#"You are consolidating learned notes for an AI skill called "{skill}".

The skill's original instructions (DO NOT contradict or alter these):
```
{body_trunc}
```

These supplementary notes were accumulated from runtime tool failures:

{notes_text}

Rules for consolidation:
1. Merge duplicates and combine closely related notes.
2. Remove notes that are already covered by the original instructions above.
3. Preserve ALL specific values (timeouts, formats, model names, parameter names) — do not generalize away concrete details.
4. Do not invent new rules. Only rephrase or merge existing notes.
5. Keep each rule to 1 sentence.

Return ONLY a JSON array of strings, each being one consolidated rule. Example:
["Rule one.", "Rule two."]

Return at most 5 rules. If all notes are redundant with the original instructions, return an empty array []."#
    );

    let llm_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 500,
        "temperature": 0.3,
    });

    let response = match reqwest::blocking::Client::new()
        .post(format!("{endpoint}/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&llm_body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
    {
        Ok(r) if r.status().is_success() => r,
        _ => {
            print_result(false, "LLM call failed");
            return;
        }
    };

    let json: serde_json::Value = match response.json() {
        Ok(j) => j,
        Err(_) => {
            print_result(false, "failed to parse LLM response");
            return;
        }
    };

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("[]")
        .trim();

    // Parse the JSON array from LLM response.
    let consolidated: Vec<String> = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            // Try to extract JSON array from markdown code block.
            let cleaned = text
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();
            match serde_json::from_str(cleaned) {
                Ok(v) => v,
                Err(_) => {
                    print_result(false, &format!("LLM returned unparseable response: {text}"));
                    return;
                }
            }
        }
    };

    if consolidated.is_empty() {
        print_result(false, "LLM returned empty consolidation");
        return;
    }

    // Rewrite SKILL.md with consolidated notes.
    let mut new_content = body.to_string();
    new_content.push_str("\n\n## Learned Notes\n");
    for note in &consolidated {
        new_content.push_str(&format!("- {}\n", note));
    }

    if fs::write(&skill_md_path, &new_content).is_err() {
        print_result(false, "failed to write SKILL.md");
        return;
    }

    print_result(
        true,
        &format!(
            "Consolidated {} notes into {} rules for '{skill}'",
            notes.len(),
            consolidated.len()
        ),
    );
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
        ("KIMI_API_KEY", "https://api.moonshot.ai/v1", "kimi-2.5"),
        (
            "DASHSCOPE_API_KEY",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen-plus",
        ),
        ("OPENAI_API_KEY", "https://api.openai.com/v1", "gpt-4o-mini"),
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
                        let name = manifest["name"].as_str().unwrap_or("unknown").to_string();
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

/// Split SKILL.md content into (body before `## Learned Notes`, vec of note strings).
fn split_learned_notes(content: &str) -> (&str, Vec<String>) {
    let marker = "## Learned Notes";
    let Some(pos) = content.find(marker) else {
        return (content.trim_end(), Vec::new());
    };
    let body = content[..pos].trim_end();
    let notes_section = &content[pos + marker.len()..];
    let notes: Vec<String> = notes_section
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- ")
                .map(|stripped| stripped.to_string())
        })
        .collect();
    (body, notes)
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
        assert!(
            result.is_none(),
            "should skip self to avoid evolution loops"
        );
    }

    #[test]
    fn should_split_learned_notes() {
        let content = "# Skill\n\nBody.\n\n## Learned Notes\n- Note one\n- Note two\n";
        let (body, notes) = split_learned_notes(content);
        assert_eq!(body, "# Skill\n\nBody.");
        assert_eq!(notes, vec!["Note one", "Note two"]);
    }

    #[test]
    fn should_split_when_no_learned_notes() {
        let content = "# Skill\n\nBody only.\n";
        let (body, notes) = split_learned_notes(content);
        assert_eq!(body, "# Skill\n\nBody only.");
        assert!(notes.is_empty());
    }

    #[test]
    fn should_cap_applied_notes_at_max() {
        // Simulate a SKILL.md with existing notes + new patches that exceed MAX_APPLIED_NOTES.
        let mut existing = "# Skill\n\nBody.\n\n## Learned Notes\n".to_string();
        for i in 0..MAX_APPLIED_NOTES {
            existing.push_str(&format!("- Old note {}\n", i));
        }
        let (body, mut all_notes) = split_learned_notes(&existing);
        assert_eq!(all_notes.len(), MAX_APPLIED_NOTES);

        // Add 3 new notes.
        all_notes.push("New note A".to_string());
        all_notes.push("New note B".to_string());
        all_notes.push("New note C".to_string());

        // Cap.
        if all_notes.len() > MAX_APPLIED_NOTES {
            all_notes.drain(0..all_notes.len() - MAX_APPLIED_NOTES);
        }
        assert_eq!(all_notes.len(), MAX_APPLIED_NOTES);
        // Oldest should be dropped, newest kept.
        assert!(all_notes.last().unwrap().contains("New note C"));
        assert!(!all_notes.iter().any(|n| n.contains("Old note 0")));

        // Reassemble.
        let mut new_content = body.to_string();
        new_content.push_str("\n\n## Learned Notes\n");
        for note in &all_notes {
            new_content.push_str(&format!("- {}\n", note));
        }
        assert!(new_content.contains("# Skill"));
        assert!(new_content.contains("New note C"));
        assert!(!new_content.contains("Old note 0"));
    }

    #[test]
    fn should_dedup_exact_match_case_insensitive() {
        let all_notes: Vec<String> = vec!["Use JSON format".into()];
        let note_normalized = "use json format";
        let is_dup = all_notes
            .iter()
            .any(|existing| existing.trim().to_lowercase() == note_normalized);
        assert!(is_dup, "should detect case-insensitive exact match");
    }

    #[test]
    fn should_not_dedup_different_notes() {
        let all_notes: Vec<String> = vec!["Use JSON format".into()];
        let note_normalized = "use json format for all responses";
        let is_dup = all_notes
            .iter()
            .any(|existing| existing.trim().to_lowercase() == note_normalized);
        assert!(!is_dup, "different notes should not be deduped");
    }
}
