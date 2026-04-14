//! Session project templates: scaffolding and prompt injection for structured
//! session types like `/new slides <name>`.

use std::path::{Path, PathBuf};
use std::process::Command;

use octos_agent::{
    WorkspacePolicy, WorkspaceProjectKind, initialize_and_commit, write_workspace_policy,
};
use serde::{Deserialize, Serialize};
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
///   history/       — optional manual exports (git is the primary history)
///   output/        — generated PPTX files
///   assets/        — images, logos, branding
///   memory.md      — project-level memory
///   changelog.md   — edit history
///   script.js      — slide generation script template
/// ```
pub fn scaffold_slides_project(data_dir: &Path, project_name: &str) -> Result<PathBuf, String> {
    let slug = slugify(project_name);
    let project_dir = data_dir.join("slides").join(&slug);
    std::fs::create_dir_all(project_dir.join("history"))
        .map_err(|e| format!("create slides history dir failed: {e}"))?;
    std::fs::create_dir_all(project_dir.join("output"))
        .map_err(|e| format!("create slides output dir failed: {e}"))?;
    std::fs::create_dir_all(project_dir.join("assets"))
        .map_err(|e| format!("create slides assets dir failed: {e}"))?;

    // Only write template files if they don't exist yet — avoid
    // overwriting LLM-written content on session actor restart.
    let memory_path = project_dir.join("memory.md");
    if !memory_path.exists() {
        let today = chrono::Utc::now().format("%Y-%m-%d");
        let memory = format!(
            "# {} -- Slides Project\n\n## Style decisions\n\n## User preferences\n\n## Current state\n- Created: {}\n- Slides: 0\n",
            project_name, today
        );
        std::fs::write(&memory_path, &memory)
            .map_err(|e| format!("write slides memory.md failed: {e}"))?;
    }

    if !project_dir.join("changelog.md").exists() {
        std::fs::write(project_dir.join("changelog.md"), "# Changelog\n\n")
            .map_err(|e| format!("write slides changelog.md failed: {e}"))?;
    }

    // Empty script.js — LLM MUST write real content before mofa_slides can run.
    let script_path = project_dir.join("script.js");
    if !script_path.exists() {
        let template = format!(
            r#"// {} -- Slides Generation Script
// version: v001_initial
// updated_at: {}
// change_summary: Initial scaffold created by /new slides
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
            project_name,
            chrono::Utc::now().format("%Y-%m-%d")
        );
        std::fs::write(&script_path, &template)
            .map_err(|e| format!("write slides script.js failed: {e}"))?;
    }

    write_workspace_policy(
        &project_dir,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .map_err(|e| format!("write slides workspace policy failed: {e}"))?;

    initialize_and_commit(
        &project_dir,
        WorkspaceProjectKind::Slides,
        "Initialize slides workspace",
    )
    .map_err(|e| format!("initialize slides git repo failed: {e}"))?;

    info!(project = %project_name, slug = %slug, "scaffolded slides project");
    Ok(project_dir)
}

/// Build the user-facing reply after scaffolding a slides project.
pub fn slides_creation_reply(project_name: &str) -> String {
    let slug = slugify(project_name);
    format!(
        "Slides project \"{project_name}\" created!\n\n\
         Project directory: slides/{slug}/\n\
         Script: slides/{slug}/script.js\n\
         Memory: slides/{slug}/memory.md\n\n\
         Workspace policy: slides/{slug}/.octos-workspace.toml\n\
         Local git history is enabled in slides/{slug}/.\n\n\
         Let me help you design your slides. I'll check available style templates first,\n\
         then we'll design the content together."
    )
}

/// Generate the slides-specific system prompt for a session.
fn slides_system_prompt(project_name: &str) -> String {
    let slug = slugify(project_name);
    let delete_cached_png_instruction = if cfg!(windows) {
        format!(
            "  shell(\"if exist slides/{slug}/output/imgs/slide-NN.png del /q slides\\\\{slug}\\\\output\\\\imgs\\\\slide-NN.png\") for each changed slide N"
        )
    } else {
        format!(
            "  shell(\"rm -f slides/{slug}/output/imgs/slide-NN.png\") for each changed slide N"
        )
    };
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
- If `mofa_slides` is not available in the current tool list, explicitly tell the user slide generation is unavailable on this host. Do NOT retry via shell, run_pipeline, or alternative binaries.
- Read slides/{slug}/memory.md before each response for context.
- Workspace policy lives at slides/{slug}/.octos-workspace.toml.
- Runtime owns workspace contract enforcement: git snapshots, required source files, and required output artifacts.
- Treat the workspace contract as authoritative for ready/not-ready state. Do NOT invent alternate completion criteria.
- Runtime-owned revision history lives in local git. Do NOT create ad hoc versioned JS filenames as the main history mechanism.

PROMPT-OWNED GUIDANCE:
- Maintain a version header at the top of slides/{slug}/script.js:
  // version: v{{NNN}}_{{desc}}
  // updated_at: YYYY-MM-DD
  // change_summary: <one line>
- When you intentionally record a human-readable revision, keep the script.js version header and changelog.md aligned.
- After edits: update memory.md.
- If the user asks for change history, inspect it with shell("git -C slides/{slug} log --oneline -- script.js changelog.md memory.md").

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
{delete_cached_png_instruction}
  (slide-01.png = slides[0], slide-02.png = slides[1], etc.)
- Skipping PNG deletion causes mofa to reuse stale images
- New slides need no PNG deletion (no cache yet)

TASK STATUS CHECK:
When user asks about progress ("做完了吗", "done?", "status"):
  use the workspace contract as the definition of ready/not-ready
  glob("slides/{slug}/output/imgs/slide-*.png") to count generated slides
  glob("slides/{slug}/output/*.pptx") to check if PPTX exists
  check_background_tasks({{"include_completed": true}}) to inspect the current session's background task state
Report: X/N slides done, PPTX ready/not ready, generation running/verifying/delivering/completed/failed based on supervisor state.

Tools: mofa_slides, read_file, write_file, edit_file, shell, glob, send_file, check_background_tasks
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

// ── Site project ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SitePlanPage {
    pub title: String,
    pub slug: String,
    pub goal: String,
    pub sections: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteProjectMetadata {
    pub version: u32,
    pub command: String,
    pub preset_key: String,
    pub template: String,
    pub site_kind: String,
    pub site_name: String,
    pub description: String,
    pub accent: String,
    pub reference: String,
    pub reference_label: String,
    pub site_slug: String,
    pub preview_base_path: String,
    pub preview_url: String,
    pub build_output_dir: String,
    pub project_dir: String,
    pub pages: Vec<SitePlanPage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SitePreset {
    preset_key: &'static str,
    template: &'static str,
    site_kind: &'static str,
    site_name: &'static str,
    description: &'static str,
    accent: &'static str,
    reference: &'static str,
    reference_label: &'static str,
}

const SITE_SESSION_FILE: &str = "mofa-site-session.json";

fn site_preset_from_topic(session_topic: &str) -> Option<SitePreset> {
    if !(session_topic == "site" || session_topic.starts_with("site ")) {
        return None;
    }

    let token = session_topic
        .strip_prefix("site")
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    let preset = match token.as_str() {
        "" | "learning" | "lesson" | "course" | "math" | "physics" => SitePreset {
            preset_key: "learning",
            template: "quarto-lesson",
            site_kind: "course",
            site_name: "Physics Learning Studio",
            description: "Lesson-driven math and physics site with chapter pages, diagrams, and explanatory notes.",
            accent: "#2563eb",
            reference: "/Users/yuechen/home/sophie/3b1b-calculus",
            reference_label: "3b1b-calculus",
        },
        "astro" | "docs" | "documentation" | "guide" => SitePreset {
            preset_key: "astro",
            template: "astro-site",
            site_kind: "docs",
            site_name: "Signal Atlas",
            description: "Structured content site for guides, onboarding, changelogs, and reference pages.",
            accent: "#d97706",
            reference: "/Users/yuechen/home/origin2025",
            reference_label: "origin2025",
        },
        "next" | "nextjs" | "app" | "product" | "event" => SitePreset {
            preset_key: "nextjs",
            template: "nextjs-app",
            site_kind: "product",
            site_name: "Vision Forum",
            description: "App-like landing shell for events, products, and structured call-to-action flows.",
            accent: "#0f766e",
            reference: "/Users/yuechen/home/ai-vision-forum-paris-2026",
            reference_label: "ai-vision-forum-paris-2026",
        },
        "react" | "vite" | "prototype" | "tool" => SitePreset {
            preset_key: "react",
            template: "react-vite",
            site_kind: "tool",
            site_name: "React Lab",
            description: "Lean React/Vite shell for prototypes, interface experiments, and lightweight tools.",
            accent: "#be123c",
            reference: "/Users/yuechen/home/adora-website",
            reference_label: "adora-website",
        },
        _ => SitePreset {
            preset_key: "learning",
            template: "quarto-lesson",
            site_kind: "course",
            site_name: "Physics Learning Studio",
            description: "Lesson-driven math and physics site with chapter pages, diagrams, and explanatory notes.",
            accent: "#2563eb",
            reference: "/Users/yuechen/home/sophie/3b1b-calculus",
            reference_label: "3b1b-calculus",
        },
    };

    Some(preset)
}

fn site_pages_for_preset(preset_key: &str) -> Vec<SitePlanPage> {
    match preset_key {
        "astro" => vec![
            SitePlanPage {
                title: "Overview".into(),
                slug: "overview".into(),
                goal: "Show the product story, navigation, and first-run guidance.".into(),
                sections: vec!["Hero".into(), "Why it exists".into(), "Quickstart".into()],
            },
            SitePlanPage {
                title: "Guide".into(),
                slug: "guide".into(),
                goal: "Lay out the primary walkthrough and the action sequence for new users."
                    .into(),
                sections: vec!["Install".into(), "Workflow".into(), "Examples".into()],
            },
            SitePlanPage {
                title: "Reference".into(),
                slug: "reference".into(),
                goal: "Hold the stable API, commands, and integration notes.".into(),
                sections: vec!["Routes".into(), "Settings".into(), "Troubleshooting".into()],
            },
        ],
        "nextjs" => vec![
            SitePlanPage {
                title: "Home".into(),
                slug: "home".into(),
                goal: "Present the main story, top CTA, and feature grid.".into(),
                sections: vec!["Hero".into(), "Program".into(), "Highlights".into()],
            },
            SitePlanPage {
                title: "Contact".into(),
                slug: "contact".into(),
                goal: "Collect inbound requests, venue info, and partnership details.".into(),
                sections: vec!["Contact form".into(), "Location".into(), "FAQ".into()],
            },
            SitePlanPage {
                title: "Privacy".into(),
                slug: "privacy".into(),
                goal: "Surface policy links and trust language for the site shell.".into(),
                sections: vec!["Policy".into(), "Data use".into(), "Contact".into()],
            },
        ],
        "react" => vec![
            SitePlanPage {
                title: "Home".into(),
                slug: "home".into(),
                goal: "Anchor the shell with a clear entry point and one primary CTA.".into(),
                sections: vec!["Header".into(), "Hero".into(), "Feature cards".into()],
            },
            SitePlanPage {
                title: "Workspace".into(),
                slug: "workspace".into(),
                goal: "Expose the main interactive surface for the prototype.".into(),
                sections: vec!["Canvas".into(), "Controls".into(), "Status".into()],
            },
            SitePlanPage {
                title: "Roadmap".into(),
                slug: "roadmap".into(),
                goal: "Document what comes next and what is still mocked.".into(),
                sections: vec!["Scope".into(), "Milestones".into(), "Notes".into()],
            },
        ],
        _ => vec![
            SitePlanPage {
                title: "Course Home".into(),
                slug: "home".into(),
                goal: "Frame the course arc and sequence the first chapters.".into(),
                sections: vec![
                    "Hero".into(),
                    "Learning path".into(),
                    "Course logistics".into(),
                ],
            },
            SitePlanPage {
                title: "Lesson 1".into(),
                slug: "lesson-1".into(),
                goal: "Introduce the opening concept with a visual explanation and one exercise."
                    .into(),
                sections: vec![
                    "Video".into(),
                    "Frames".into(),
                    "Core idea".into(),
                    "Interactive".into(),
                    "Recap".into(),
                ],
            },
            SitePlanPage {
                title: "Lesson 2".into(),
                slug: "lesson-2".into(),
                goal: "Extend the concept with a worked example and a practice prompt.".into(),
                sections: vec![
                    "Hook".into(),
                    "Example".into(),
                    "Visual proof".into(),
                    "Exercise".into(),
                ],
            },
        ],
    }
}

fn site_build_output_dir(template: &str) -> &'static str {
    match template {
        "astro-site" => "dist",
        "nextjs-app" => "out",
        "react-vite" => "dist",
        _ => "docs",
    }
}

fn site_preview_base_path(profile_id: &str, session_id: &str, site_slug: &str) -> String {
    format!("/api/preview/{profile_id}/{session_id}/{site_slug}")
}

fn site_preview_root_url(profile_id: &str, session_id: &str, site_slug: &str) -> String {
    format!(
        "{}/index.html",
        site_preview_base_path(profile_id, session_id, site_slug)
    )
}

fn site_system_prompt(session_topic: &str) -> Option<String> {
    let preset = site_preset_from_topic(session_topic)?;
    let site_slug = slugify(preset.site_name);
    let build_output_dir = site_build_output_dir(preset.template);

    Some(format!(
        r#"You are a website builder for the "{site_name}" project.
Project dir: sites/{site_slug}/

ON FIRST MESSAGE:
1. Read sites/{site_slug}/{session_file}
2. Read sites/{site_slug}/site-plan.json
3. Read the relevant source files before editing

WORKFLOW:
1. Keep edits inside sites/{site_slug}/ only
2. Preserve the selected framework: {template}
3. Maintain the extracted structure while removing private/source-specific branding
4. After source edits, rebuild the site so the iframe preview stays current
5. Local git is already initialized in sites/{site_slug}/; rely on git auto-commits for revision history instead of ad hoc backup files

BUILD RULES:
- {template}: build output dir is sites/{site_slug}/{build_output_dir}/
- Preview route shape: /api/preview/<profile-id>/<session-id>/{site_slug}/
- Workspace policy lives at sites/{site_slug}/.octos-workspace.toml.
- If template is quarto-lesson, run shell(\"quarto render\") from sites/{site_slug}/ when Quarto is available
- If template is astro-site, nextjs-app, or react-vite:
  - run shell(\"test -d node_modules || npm install\") from sites/{site_slug}/
  - then run shell(\"npm run build\") from sites/{site_slug}/
- Never write outside the project dir
- Treat sites/{site_slug}/{session_file} as the source of truth for template, preview path, and build output

EXPECTATIONS:
- Keep the preview working under a session-scoped subpath
- Prefer editing existing scaffold files over recreating the project
- When adding pages, keep navigation and internal links aligned with the build path
- If the user asks for change history, inspect it with shell("git -C sites/{site_slug} log --oneline -- .")

Tools: read_file, write_file, edit_file, shell, glob
"#,
        site_name = preset.site_name,
        site_slug = site_slug,
        session_file = SITE_SESSION_FILE,
        template = preset.template,
        build_output_dir = build_output_dir,
    ))
}

fn render_site_prompt(metadata: &SiteProjectMetadata) -> String {
    let mut lines = vec![
        format!("Template: {}", metadata.template),
        format!("Site kind: {}", metadata.site_kind),
        format!("Site name: {}", metadata.site_name),
        format!("Description: {}", metadata.description),
        format!("Reference pattern: {}", metadata.reference),
        format!("Accent: {}", metadata.accent),
        format!("Preview URL: {}", metadata.preview_url),
        String::new(),
        "Pages:".into(),
    ];

    for (index, page) in metadata.pages.iter().enumerate() {
        lines.push(format!(
            "{}. {} ({}) -> {} [{}]",
            index + 1,
            page.title,
            page.slug,
            page.goal,
            page.sections.join(", ")
        ));
    }

    lines.extend([
        String::new(),
        "Studio contract:".into(),
        "- build a starter scaffold first".into(),
        "- keep the file tree inspectable in real time".into(),
        format!(
            "- keep the preview route stable under {}",
            metadata.preview_url
        ),
    ]);

    lines.join("\n")
}

fn resolve_mofa_site_skill_dir(data_dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path) = std::env::var_os("MOFA_SITE_SKILL_DIR").map(PathBuf::from) {
        candidates.push(path);
    }
    candidates.push(data_dir.join("skills").join("mofa-site"));
    candidates
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../mofa-skills/mofa-site"));

    candidates
        .into_iter()
        .find(|path| path.join("scripts").join("bootstrap_template.sh").exists())
}

pub fn build_site_project_metadata(
    profile_id: &str,
    session_id: &str,
    session_topic: &str,
    _project_dir: &Path,
) -> Option<SiteProjectMetadata> {
    let preset = site_preset_from_topic(session_topic)?;
    let site_slug = slugify(preset.site_name);
    let preview_base_path = site_preview_base_path(profile_id, session_id, &site_slug);
    let preview_url = site_preview_root_url(profile_id, session_id, &site_slug);
    let build_output_dir = site_build_output_dir(preset.template).to_string();

    Some(SiteProjectMetadata {
        version: 1,
        command: format!("/new site {}", preset.preset_key),
        preset_key: preset.preset_key.to_string(),
        template: preset.template.to_string(),
        site_kind: preset.site_kind.to_string(),
        site_name: preset.site_name.to_string(),
        description: preset.description.to_string(),
        accent: preset.accent.to_string(),
        reference: preset.reference.to_string(),
        reference_label: preset.reference_label.to_string(),
        site_slug: site_slug.clone(),
        preview_base_path,
        preview_url,
        build_output_dir,
        project_dir: format!("sites/{site_slug}"),
        pages: site_pages_for_preset(preset.preset_key),
    })
}

pub fn read_site_project_metadata(project_dir: &Path) -> Option<SiteProjectMetadata> {
    let path = project_dir.join(SITE_SESSION_FILE);
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_site_support_files(
    project_dir: &Path,
    metadata: &SiteProjectMetadata,
) -> Result<(), String> {
    let content_dir = project_dir.join("content");
    std::fs::create_dir_all(&content_dir)
        .map_err(|e| format!("create site content dir failed: {e}"))?;

    std::fs::write(
        project_dir.join(SITE_SESSION_FILE),
        serde_json::to_string_pretty(metadata)
            .map_err(|e| format!("serialize {SITE_SESSION_FILE} failed: {e}"))?,
    )
    .map_err(|e| format!("write {SITE_SESSION_FILE} failed: {e}"))?;

    let site_plan = serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "template": metadata.template,
        "site_name": metadata.site_name,
        "pages": metadata.pages,
    });
    std::fs::write(
        project_dir.join("site-plan.json"),
        serde_json::to_string_pretty(&site_plan)
            .map_err(|e| format!("serialize site-plan.json failed: {e}"))?,
    )
    .map_err(|e| format!("write site-plan.json failed: {e}"))?;

    std::fs::write(
        project_dir.join("optimized-prompt.md"),
        render_site_prompt(metadata),
    )
    .map_err(|e| format!("write optimized-prompt.md failed: {e}"))?;

    let overview = format!(
        "# {site_name}\n\n{description}\n\n- template: {template}\n- site kind: {site_kind}\n- reference: {reference}\n- preview: {preview}\n\n## Pages\n{pages}\n",
        site_name = metadata.site_name,
        description = metadata.description,
        template = metadata.template,
        site_kind = metadata.site_kind,
        reference = metadata.reference,
        preview = metadata.preview_url,
        pages = metadata
            .pages
            .iter()
            .map(|page| format!("- {}: {}", page.title, page.goal))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    std::fs::write(content_dir.join("overview.md"), overview)
        .map_err(|e| format!("write content/overview.md failed: {e}"))?;

    for page in &metadata.pages {
        let doc = format!(
            "# {title}\n\nSlug: `{slug}`\n\nGoal: {goal}\n\nSections:\n{sections}\n",
            title = page.title,
            slug = page.slug,
            goal = page.goal,
            sections = page
                .sections
                .iter()
                .map(|section| format!("- {section}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        std::fs::write(content_dir.join(format!("{}.md", page.slug)), doc)
            .map_err(|e| format!("write content/{}.md failed: {e}", page.slug))?;
    }

    let today = chrono::Utc::now().format("%Y-%m-%d");
    std::fs::write(
        project_dir.join("memory.md"),
        format!(
            "# {} -- Site Project\n\n## Current state\n- Created: {}\n- Template: {}\n- Preview: {}\n",
            metadata.site_name, today, metadata.template, metadata.preview_url
        ),
    )
    .map_err(|e| format!("write memory.md failed: {e}"))?;

    std::fs::write(project_dir.join("changelog.md"), "# Changelog\n\n")
        .map_err(|e| format!("write changelog.md failed: {e}"))?;

    Ok(())
}

fn run_site_bootstrap(
    skill_dir: &Path,
    project_dir: &Path,
    metadata: &SiteProjectMetadata,
) -> Result<(), String> {
    let scripts_dir = skill_dir.join("scripts");
    std::fs::create_dir_all(project_dir)
        .map_err(|e| format!("create site project dir failed: {e}"))?;

    let status = if metadata.template == "quarto-lesson" {
        Command::new("bash")
            .arg(scripts_dir.join("bootstrap_quarto_lesson.sh"))
            .arg("--out-dir")
            .arg(project_dir)
            .arg("--title")
            .arg(&metadata.site_name)
            .arg("--description")
            .arg(&metadata.description)
            .status()
            .map_err(|e| format!("spawn Quarto bootstrap failed: {e}"))?
    } else {
        Command::new("bash")
            .arg(scripts_dir.join("bootstrap_template.sh"))
            .arg("--template")
            .arg(&metadata.template)
            .arg("--out-dir")
            .arg(project_dir)
            .arg("--site-name")
            .arg(&metadata.site_name)
            .arg("--description")
            .arg(&metadata.description)
            .arg("--accent")
            .arg(&metadata.accent)
            .arg("--locale")
            .arg("en")
            .arg("--base-path")
            .arg(&metadata.preview_base_path)
            .status()
            .map_err(|e| format!("spawn site bootstrap failed: {e}"))?
    };

    if !status.success() {
        return Err(format!(
            "site bootstrap failed for {} with status {}",
            metadata.template, status
        ));
    }

    Ok(())
}

pub fn scaffold_site_project(
    workspace_root: &Path,
    profile_id: &str,
    session_id: &str,
    session_topic: &str,
    data_dir: &Path,
) -> Result<SiteProjectMetadata, String> {
    let metadata =
        build_site_project_metadata(profile_id, session_id, session_topic, workspace_root)
            .ok_or_else(|| format!("unsupported site template request: {session_topic}"))?;

    let project_dir = workspace_root.join(&metadata.project_dir);
    if project_dir.exists() {
        std::fs::remove_dir_all(&project_dir)
            .map_err(|e| format!("clear site project dir failed: {e}"))?;
    }

    let skill_dir = resolve_mofa_site_skill_dir(data_dir)
        .ok_or_else(|| "mofa-site skill directory not found".to_string())?;
    run_site_bootstrap(&skill_dir, &project_dir, &metadata)?;
    write_site_support_files(&project_dir, &metadata)?;
    write_workspace_policy(
        &project_dir,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Sites),
    )
    .map_err(|e| format!("write site workspace policy failed: {e}"))?;
    initialize_and_commit(
        &project_dir,
        WorkspaceProjectKind::Sites,
        "Initialize site workspace",
    )
    .map_err(|e| format!("initialize site git repo failed: {e}"))?;

    info!(
        session_id = %session_id,
        preset = %metadata.preset_key,
        template = %metadata.template,
        slug = %metadata.site_slug,
        "scaffolded site project"
    );

    Ok(metadata)
}

pub fn site_creation_reply(metadata: &SiteProjectMetadata) -> String {
    format!(
        "Site project \"{site_name}\" created!\n\n\
         Project directory: {project_dir}/\n\
         Template: {template}\n\
         Preview route: {preview_url}\n\
         Session metadata: {project_dir}/{session_file}\n\n\
         Workspace policy: {project_dir}/.octos-workspace.toml\n\
         Local git history is enabled in {project_dir}/.\n\n\
         The scaffold is ready. Edit the source files and refresh the iframe preview to see the built site.",
        site_name = metadata.site_name,
        project_dir = metadata.project_dir,
        template = metadata.template,
        preview_url = metadata.preview_url,
        session_file = SITE_SESSION_FILE,
    )
}

pub fn try_activate_site_template(data_dir: &Path, session_topic: &str) -> Option<String> {
    let prompt = site_system_prompt(session_topic)?;
    if let Err(e) = write_session_prompt(data_dir, session_topic, &prompt) {
        tracing::warn!(error = %e, "failed to write site session prompt");
    }

    let metadata =
        build_site_project_metadata("profile-id", "session-id", session_topic, Path::new("."))?;
    Some(site_creation_reply(&metadata))
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
        let project_dir = scaffold_slides_project(tmp.path(), "test-deck").unwrap();

        assert!(project_dir.join("history").is_dir());
        assert!(project_dir.join("output").is_dir());
        assert!(project_dir.join("assets").is_dir());
        assert!(project_dir.join("memory.md").is_file());
        assert!(project_dir.join("changelog.md").is_file());
        assert!(project_dir.join("script.js").is_file());
        assert!(project_dir.join(".git").is_dir());
        assert!(project_dir.join(".gitignore").is_file());
        assert!(project_dir.join(".octos-workspace.toml").is_file());

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
        scaffold_slides_project(tmp.path(), "deck").unwrap();
        // Modify a file and ensure re-scaffold preserves user edits.
        let memory_path = tmp.path().join("slides/deck/memory.md");
        std::fs::write(&memory_path, "custom content").unwrap();

        // Re-scaffold keeps existing files intact.
        scaffold_slides_project(tmp.path(), "deck").unwrap();
        let content = std::fs::read_to_string(&memory_path).unwrap();
        assert_eq!(content, "custom content");
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
    fn slides_prompt_uses_supervisor_for_status_checks() {
        let prompt = slides_system_prompt("Deck");
        assert!(prompt.contains("check_background_tasks"));
        assert!(prompt.contains("If `mofa_slides` is not available"));
        assert!(prompt.contains("Runtime owns workspace contract enforcement"));
        assert!(prompt.contains("PROMPT-OWNED GUIDANCE"));
        assert!(!prompt.contains("On every meaningful edit: increment NNN"));
        assert!(!prompt.contains("ps aux | grep mofa_slides | grep -v grep"));
    }

    #[test]
    fn should_generate_correct_reply_text() {
        let reply = slides_creation_reply("Q4 Report");
        assert!(reply.contains("Q4 Report"));
        assert!(reply.contains("slides/q4-report/"));
        assert!(reply.contains("Let me help you design your slides"));
        assert!(reply.contains(".octos-workspace.toml"));
        assert!(reply.contains("Local git history is enabled"));
    }

    #[test]
    fn should_build_site_project_metadata_for_astro() {
        let metadata =
            build_site_project_metadata("dspfac", "site-session-123", "site astro", Path::new("."))
                .expect("astro metadata");

        assert_eq!(metadata.preset_key, "astro");
        assert_eq!(metadata.template, "astro-site");
        assert_eq!(metadata.site_slug, "signal-atlas");
        assert_eq!(
            metadata.preview_base_path,
            "/api/preview/dspfac/site-session-123/signal-atlas"
        );
        assert_eq!(
            metadata.preview_url,
            "/api/preview/dspfac/site-session-123/signal-atlas/index.html"
        );
        assert_eq!(metadata.build_output_dir, "dist");
    }

    #[test]
    fn should_activate_site_template_and_write_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let reply = try_activate_site_template(tmp.path(), "site nextjs").expect("site reply");

        assert!(reply.contains("Vision Forum"));
        assert!(reply.contains("sites/vision-forum/"));

        let prompt = read_session_prompt(tmp.path(), "site nextjs").expect("site prompt");
        assert!(prompt.contains("website builder"));
        assert!(prompt.contains("sites/vision-forum/mofa-site-session.json"));
        assert!(prompt.contains("/api/preview/<profile-id>/<session-id>/vision-forum/"));
    }
}
