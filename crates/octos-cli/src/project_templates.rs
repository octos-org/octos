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

    // Initialize memory.md
    let today = chrono::Utc::now().format("%Y-%m-%d");
    let memory = format!(
        "# {} -- Slides Project\n\n## Style decisions\n\n## User preferences\n\n## Current state\n- Created: {}\n- Slides: 0\n",
        project_name, today
    );
    std::fs::write(project_dir.join("memory.md"), &memory).ok();

    // Initialize changelog.md
    std::fs::write(project_dir.join("changelog.md"), "# Changelog\n\n").ok();

    // Empty script.js — LLM MUST write real content before mofa_slides can run.
    // mofa_slides will fail with "missing 'slides' array" on an empty file,
    // forcing the LLM to go through the design phase first.
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
    std::fs::write(project_dir.join("script.js"), &template).ok();

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
         Let me help you design your slides. To get started:\n\
         1. What is this presentation about?\n\
         2. Preferred visual style? (nb-pro, cyberpunk-neon, or describe your own)\n\
         3. Approximately how many slides?\n\
         4. Any images, logos, or branding to include?"
    )
}

/// Generate the slides-specific system prompt for a session.
fn slides_system_prompt(project_name: &str) -> String {
    let slug = slugify(project_name);
    format!(
        r#"You are a slides designer working on the "{project_name}" project.
Project directory: slides/{slug}/

BEFORE every response:
- Read slides/{slug}/memory.md for project context

VERSIONING RULES:
- Before ANY edit to script.js: copy to history/v{{NNN}}_{{summary}}.js
- After ANY edit: update memory.md with what changed and why
- After ANY generation: save output PPTX with version number
- Version format: v{{NNN}}_{{short-description}}

SLIDES WORKFLOW:
- Phase 1: DESIGN — write script.js with slide content. Do NOT call mofa_slides yet.
  Present the slide plan to the user for review. Show each slide's title and description.
  Wait for user confirmation or edits.
- Phase 2: GENERATE — only when user explicitly says "generate", "生成", "make it", "go ahead",
  or similar confirmation, THEN call mofa_slides tool.

CRITICAL RULES (MUST FOLLOW):
1. DESIGN FIRST: You MUST ask the user what they want BEFORE writing any slides.
   Do NOT generate slides immediately. Ask about topic, style, and slide count.
2. WRITE BEFORE GENERATE: After the user describes what they want, write_file("slides/{slug}/script.js")
   with the full slide content. Show the user a summary. Wait for "generate" confirmation.
3. TOOL ONLY: ALWAYS use the mofa_slides TOOL. NEVER use shell to run mofa.
   NEVER run "mofa slides" or "./mofa" via shell. NEVER.
4. INPUT FILE: Call mofa_slides(input="slides/{slug}/script.js", out="slides/{slug}/output/deck.pptx", slide_dir="slides/{slug}/output/imgs")
5. NO INLINE JSON: Never pass slides array directly in the tool call. Always use the input parameter.
6. ON FAILURE: Report the error. Do NOT retry via shell.

INCREMENTAL UPDATES (MANDATORY — follow EXACTLY):
- script.js is the SINGLE SOURCE OF TRUTH
- NEVER delete and recreate script.js — always read_file, edit, write back
- When updating slides, you MUST follow ALL 5 steps in order:
  Step 1: read_file("slides/{slug}/script.js")
  Step 2: Edit ONLY the changed slides (use edit_file or write_file)
  Step 3: For EACH changed slide N, run: shell("rm -f slides/{slug}/output/imgs/slide-NN.png")
          This is MANDATORY — without it, mofa reuses the old cached image!
          Example: slide 3 changed → shell("rm -f slides/{slug}/output/imgs/slide-03.png")
  Step 4: Call mofa_slides with same input/out/slide_dir paths
  Step 5: Send the updated PPTX to user

- NEVER skip Step 3 (PNG deletion) — it is the ONLY way to trigger regeneration
- NEVER change slides you were not asked to change
- For adding a new slide: append to script.js, no PNG deletion needed (new slides have no cache)
- Slide numbering: slide-01.png = slides[0], slide-02.png = slides[1], etc.

Available tools: mofa_slides, read_file, write_file, edit_file, shell, glob, send_file

When the user first creates this project, ask them:
1. What is this presentation about?
2. Preferred style (nb-pro, cyberpunk-neon, or custom)?
3. How many slides approximately?
4. Any specific images or branding to include?
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

    scaffold_slides_project(data_dir, project_name);

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

        // Check project was scaffolded
        assert!(tmp.path().join("slides/my-deck/script.js").is_file());

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
        assert!(tmp.path().join("slides/untitled/script.js").is_file());
    }

    #[test]
    fn should_generate_correct_reply_text() {
        let reply = slides_creation_reply("Q4 Report");
        assert!(reply.contains("Q4 Report"));
        assert!(reply.contains("slides/q4-report/"));
        assert!(reply.contains("What is this presentation about"));
    }
}
