//! Session project templates: scaffolding and prompt injection for structured
//! session types like `/new slides <name>`.

use std::path::{Path, PathBuf};

use tracing::info;

/// Slugify a project name for use as a directory name.
fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Directory where session prompt overrides are stored.
const SESSION_PROMPTS_DIR: &str = "session_prompts";

// ── Slides project ─────────────────────────────────────────────────────────

/// Scaffold a slides project directory under `data_dir/slides/<slug>/`.
///
/// Creates the following structure:
/// ```text
/// slides/<slug>/
///   history/       — versioned script snapshots
///   output/        — generated PPTX files
///   assets/        — images, logos, branding
///   memory.md      — project-level memory
///   changelog.md   — edit history
///   script.js      — slide generation script template
/// ```
pub fn scaffold_slides_project(data_dir: &Path, project_name: &str) -> PathBuf {
    let slug = slugify(project_name);
    let project_dir = data_dir.join("slides").join(&slug);
    std::fs::create_dir_all(project_dir.join("history")).ok();
    std::fs::create_dir_all(project_dir.join("output")).ok();
    std::fs::create_dir_all(project_dir.join("assets")).ok();

    // Only write template files if they don't exist yet — avoid
    // overwriting LLM-written content on session actor restart.
    let memory_path = project_dir.join("memory.md");
    if !memory_path.exists() {
        let today = chrono::Utc::now().format("%Y-%m-%d");
        let memory = format!(
            "# {} -- Slides Project\n\n## Style decisions\n\n## User preferences\n\n## Current state\n- Created: {}\n- Slides: 0\n",
            project_name, today
        );
        std::fs::write(&memory_path, &memory).ok();
    }

    if !project_dir.join("changelog.md").exists() {
        std::fs::write(project_dir.join("changelog.md"), "# Changelog\n\n").ok();
    }

    // Empty script.js — LLM MUST write real content before mofa_slides can run.
    let script_path = project_dir.join("script.js");
    if !script_path.exists() {
        let template = format!(
            r#"// {} -- Slides Generation Script
// EMPTY: The agent must write slide content here before generating.
// Use mofa_slides with input pointing to this file after writing content.
//
// Example format:
// module.exports = [
//   {{ prompt: "Cover slide description", style: "cover" }},
//   {{ prompt: "Content slide description", style: "normal" }},
// ];

module.exports = [];
"#,
            project_name
        );
        std::fs::write(&script_path, &template).ok();
    }

    info!(project = %project_name, slug = %slug, "scaffolded slides project");
    project_dir
}

/// Build the user-facing reply after scaffolding a slides project.
pub fn slides_creation_reply(project_name: &str) -> String {
    let slug = slugify(project_name);
    format!(
        "Slides project \"{project_name}\" created!\n\n\
         Project directory: slides/{slug}/\n\
         Script: slides/{slug}/script.js\n\
         Memory: slides/{slug}/memory.md\n\n\
         Let me help you design your slides. I'll check available style templates first,\n\
         then we'll design the content together."
    )
}

/// Generate the slides-specific system prompt for a session.
fn slides_system_prompt(project_name: &str) -> String {
    let slug = slugify(project_name);
    format!(
        r#"You are a slides designer for the "{project_name}" project.
Project dir: slides/{slug}/

ON FIRST MESSAGE:
1. glob("styles/*.toml") — list available style templates with their [meta].description
2. Ask the user: topic, style (pick template or describe custom), slide count, any branding/images

WORKFLOW (follow in order):
1. STYLE — if user picks a template, use it. If custom, create styles/{{name}}.toml first.
2. DESIGN — write slides/{slug}/script.js. Show outline to user. Wait for confirmation.
3. GENERATE — on user confirmation ("生成"/"generate"/"go"), call mofa_slides.

RULES:
- ALWAYS use mofa_slides TOOL. NEVER shell to run mofa. NEVER.
- BEFORE calling mofa_slides: run shell("node --check slides/{slug}/script.js") to validate syntax. Fix any errors before proceeding.
- ALWAYS use input parameter: mofa_slides(input="slides/{slug}/script.js", out="slides/{slug}/output/deck.pptx", slide_dir="slides/{slug}/output/imgs")
- NEVER pass slides array inline. ALWAYS use the input file.
- On failure: report error, do NOT retry via shell.
- Read slides/{slug}/memory.md before each response for context.
- After edits: update memory.md. Before edits: copy script.js to history/v{{NNN}}_{{desc}}.js.

STYLE TOML — create at styles/{{name}}.toml when user wants a custom style:
```toml
[meta]
name = "{{name}}"
display_name = "Display Name"
description = "One-line description"
category = "custom"
tags = ["custom"]

[variants]
default = "normal"

[variants.normal]
prompt = """
Create a slide image. 1920×1080, 16:9 landscape.
BACKGROUND: <hex colors, gradients>
TYPOGRAPHY: <fonts, weights, sizes, hex colors>
LAYOUT: <margins in px, alignment>
ELEMENTS: <decorations, shapes — specific>
Text must be PIXEL-PERFECT and EXACTLY as specified.
"""

[variants.cover]
prompt = """
Create a cover slide. 1920×1080, 16:9.
<dramatic title layout, same palette>
"""

[variants.data]
prompt = """
Create a data slide. 1920×1080, 16:9.
<tables, charts layout, same palette>
"""
```
Prompts are Gemini image-gen instructions — use hex colors, px margins, font names. Be concrete.
Custom styles persist in styles/ and appear as templates for future projects.

INCREMENTAL UPDATES:
- script.js is the SINGLE SOURCE OF TRUTH — never recreate, always edit
- To update slides: read → edit changed slides only → delete their cached PNGs → regenerate
  shell("rm -f slides/{slug}/output/imgs/slide-NN.png") for each changed slide N
  (slide-01.png = slides[0], slide-02.png = slides[1], etc.)
- Skipping PNG deletion causes mofa to reuse stale images
- New slides need no PNG deletion (no cache yet)

Tools: mofa_slides, read_file, write_file, edit_file, shell, glob, send_file
"#
    )
}

/// Write a session-specific system prompt override file.
///
/// Stored at `data_dir/session_prompts/<topic>.md` where `<topic>` is the
/// session topic name (e.g. "slides my-project").
pub fn write_session_prompt(data_dir: &Path, topic: &str, prompt: &str) -> std::io::Result<()> {
    let dir = data_dir.join(SESSION_PROMPTS_DIR);
    std::fs::create_dir_all(&dir)?;
    let filename = slugify(topic);
    std::fs::write(dir.join(format!("{filename}.md")), prompt)
}

/// Read a session-specific system prompt override, if any.
///
/// Returns `Some(prompt)` if a file exists at `data_dir/session_prompts/<topic>.md`.
pub fn read_session_prompt(data_dir: &Path, topic: &str) -> Option<String> {
    let filename = slugify(topic);
    let path = data_dir
        .join(SESSION_PROMPTS_DIR)
        .join(format!("{filename}.md"));
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        _ => None,
    }
}

/// Handle the slides template: scaffold project, write session prompt, return
/// the creation reply. Called from `handle_new_command` when the topic starts
/// with "slides".
///
/// Returns `Some(reply_text)` if the slides template was activated,
/// `None` if the topic doesn't match the slides pattern.
pub fn try_activate_slides_template(data_dir: &Path, session_topic: &str) -> Option<String> {
    // Extract project name: "slides <name>" or bare "slides"
    let project_name = session_topic.strip_prefix("slides").unwrap_or("").trim();
    let project_name = if project_name.is_empty() {
        "untitled"
    } else {
        project_name
    };

    // NOTE: File scaffolding is done in session_actor.rs (into the per-user
    // workspace) so tools can reach the files.  We only write the session
    // prompt and return the reply text here.

    // Write session-scoped system prompt
    let prompt = slides_system_prompt(project_name);
    if let Err(e) = write_session_prompt(data_dir, session_topic, &prompt) {
        tracing::warn!(error = %e, "failed to write slides session prompt");
    }

    Some(slides_creation_reply(project_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_slugify_project_name() {
        assert_eq!(slugify("My Project"), "my-project");
        assert_eq!(slugify("hello world!"), "hello-world");
        // trim trailing hyphens
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("CamelCase"), "camelcase");
    }

    #[test]
    fn should_scaffold_slides_project_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = scaffold_slides_project(tmp.path(), "test-deck");

        assert!(project_dir.join("history").is_dir());
        assert!(project_dir.join("output").is_dir());
        assert!(project_dir.join("assets").is_dir());
        assert!(project_dir.join("memory.md").is_file());
        assert!(project_dir.join("changelog.md").is_file());
        assert!(project_dir.join("script.js").is_file());

        let memory = std::fs::read_to_string(project_dir.join("memory.md")).unwrap();
        assert!(memory.contains("test-deck"));
        assert!(memory.contains("Slides Project"));

        let script = std::fs::read_to_string(project_dir.join("script.js")).unwrap();
        assert!(script.contains("test-deck"));
        assert!(script.contains("module.exports"));
    }

    #[test]
    fn should_scaffold_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        scaffold_slides_project(tmp.path(), "deck");
        // Modify a file
        let memory_path = tmp.path().join("slides/deck/memory.md");
        std::fs::write(&memory_path, "custom content").unwrap();

        // Re-scaffold overwrites
        scaffold_slides_project(tmp.path(), "deck");
        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert!(content.contains("Slides Project"));
    }

    #[test]
    fn should_roundtrip_session_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        write_session_prompt(tmp.path(), "slides my-project", "test prompt").unwrap();
        let prompt = read_session_prompt(tmp.path(), "slides my-project");
        assert_eq!(prompt.unwrap(), "test prompt");
    }

    #[test]
    fn should_return_none_for_missing_session_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_session_prompt(tmp.path(), "nonexistent").is_none());
    }

    #[test]
    fn should_activate_slides_template() {
        let tmp = tempfile::tempdir().unwrap();
        let reply = try_activate_slides_template(tmp.path(), "slides my-deck");
        assert!(reply.is_some());
        let reply = reply.unwrap();
        assert!(reply.contains("my-deck"));
        assert!(reply.contains("slides/my-deck/"));

        // File scaffolding is now done by session_actor (into workspace),
        // so try_activate_slides_template only writes the session prompt.
        assert!(!tmp.path().join("slides/my-deck/script.js").is_file());

        // Check session prompt was written
        let prompt = read_session_prompt(tmp.path(), "slides my-deck");
        assert!(prompt.is_some());
        assert!(prompt.unwrap().contains("slides designer"));
    }

    #[test]
    fn should_use_untitled_for_bare_slides() {
        let tmp = tempfile::tempdir().unwrap();
        let reply = try_activate_slides_template(tmp.path(), "slides");
        assert!(reply.is_some());
        assert!(reply.unwrap().contains("untitled"));
        // Scaffolding happens in session_actor, not here
        assert!(!tmp.path().join("slides/untitled/script.js").is_file());
    }

    #[test]
    fn should_generate_correct_reply_text() {
        let reply = slides_creation_reply("Q4 Report");
        assert!(reply.contains("Q4 Report"));
        assert!(reply.contains("slides/q4-report/"));
        assert!(reply.contains("What is this presentation about"));
    }
}
