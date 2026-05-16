use crate::workflow_runtime::WorkflowInstance;
use octos_agent::WorkspacePolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteTemplate {
    AstroSite,
    NextjsApp,
    ReactVite,
    Docs,
}

impl SiteTemplate {
    /// Lossy slug → template mapping. Unknown slugs collapse to
    /// `Docs`, which keeps non-security call-sites (e.g.
    /// `build_output_dir_for_template`, `workspace_policy_for_template`)
    /// from panicking when callers pass in legacy / future template
    /// slugs that should still be "scaffold a docs-ish workspace".
    ///
    /// **Security-sensitive callers MUST use [`from_slug_strict`]**.
    /// Codex round-2 BLOCKING #2 (issue #996 follow-up): treating
    /// unknown slugs as `Docs` was default-allow for the preview
    /// validator — a malicious `metadata.template: "anything-goes"`
    /// paired with `build_output_dir: "docs"` slipped past the
    /// per-template-equality gate because the synthetic `Docs`
    /// variant claims `output_dir() == "docs"`. The strict variant
    /// returns `None` on unknown slugs so the validator can reject.
    pub fn from_slug(slug: &str) -> Self {
        Self::from_slug_strict(slug).unwrap_or(Self::Docs)
    }

    /// Strict slug → template mapping. Returns `None` for any slug
    /// not produced by the in-tree scaffolders. Used by the
    /// `build_output_dir` validator to reject unknown templates
    /// instead of silently defaulting to `Docs`. The known set is:
    /// `astro-site`, `nextjs-app`, `react-vite`, `quarto-lesson`.
    ///
    /// Keep this in sync with the preset table in
    /// `crates/octos-cli/src/project_templates.rs::site_preset_from_topic`.
    pub fn from_slug_strict(slug: &str) -> Option<Self> {
        match slug.trim().to_ascii_lowercase().as_str() {
            "astro-site" => Some(Self::AstroSite),
            "nextjs-app" => Some(Self::NextjsApp),
            "react-vite" => Some(Self::ReactVite),
            "quarto-lesson" => Some(Self::Docs),
            _ => None,
        }
    }

    pub const fn output_dir(self) -> &'static str {
        match self {
            Self::AstroSite => "dist",
            Self::NextjsApp => "out",
            Self::ReactVite => "dist",
            Self::Docs => "docs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitePlan {
    pub template: SiteTemplate,
}

impl SitePlan {
    pub const fn new(template: SiteTemplate) -> Self {
        Self { template }
    }

    pub fn compile(self) -> WorkflowInstance {
        crate::workflows::site_delivery::build()
    }

    pub fn workspace_policy(self) -> WorkspacePolicy {
        crate::workflows::site_delivery::workspace_policy_for_template_kind(self.template)
    }
}
