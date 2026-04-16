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
    pub fn from_slug(slug: &str) -> Self {
        match slug.trim().to_ascii_lowercase().as_str() {
            "astro-site" => Self::AstroSite,
            "nextjs-app" => Self::NextjsApp,
            "react-vite" => Self::ReactVite,
            _ => Self::Docs,
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
