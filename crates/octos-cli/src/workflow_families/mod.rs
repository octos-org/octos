mod deep_research;
mod registry;
mod research_podcast;
mod site;
mod slides;

use self::deep_research::DeepResearchPlan;
use self::research_podcast::ResearchPodcastPlan;
use self::site::SitePlan;
use self::slides::SlidesPlan;
use crate::workflow_runtime::{WorkflowInstance, WorkflowKind};

pub use registry::{WorkflowFamilyDescriptor, registry};
pub use site::SiteTemplate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowPlanRequest {
    DeepResearch,
    ResearchPodcast,
    Slides,
    Site { template: SiteTemplate },
}

impl WorkflowPlanRequest {
    pub fn default_for_kind(kind: WorkflowKind) -> Self {
        registry::default_request_for_kind(kind)
    }

    pub fn compile(self) -> WorkflowPlan {
        match self {
            Self::DeepResearch => WorkflowPlan::DeepResearch(DeepResearchPlan),
            Self::ResearchPodcast => WorkflowPlan::ResearchPodcast(ResearchPodcastPlan),
            Self::Slides => WorkflowPlan::Slides(SlidesPlan),
            Self::Site { template } => WorkflowPlan::Site(SitePlan::new(template)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowPlan {
    DeepResearch(DeepResearchPlan),
    ResearchPodcast(ResearchPodcastPlan),
    Slides(SlidesPlan),
    Site(SitePlan),
}

impl WorkflowPlan {
    pub fn kind(&self) -> WorkflowKind {
        match self {
            Self::DeepResearch(_) => WorkflowKind::DeepResearch,
            Self::ResearchPodcast(_) => WorkflowKind::ResearchPodcast,
            Self::Slides(_) => WorkflowKind::Slides,
            Self::Site(_) => WorkflowKind::Site,
        }
    }

    pub fn into_instance(self) -> WorkflowInstance {
        match self {
            Self::DeepResearch(plan) => plan.compile(),
            Self::ResearchPodcast(plan) => plan.compile(),
            Self::Slides(plan) => plan.compile(),
            Self::Site(plan) => plan.compile(),
        }
    }
}

pub fn compile_default(kind: WorkflowKind) -> WorkflowInstance {
    WorkflowPlanRequest::default_for_kind(kind)
        .compile()
        .into_instance()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_all_supported_families() {
        let kinds: Vec<_> = registry().iter().map(|family| family.kind).collect();
        assert_eq!(
            kinds,
            vec![
                WorkflowKind::DeepResearch,
                WorkflowKind::ResearchPodcast,
                WorkflowKind::Slides,
                WorkflowKind::Site,
            ]
        );
    }

    #[test]
    fn bounded_site_template_defaults_to_docs() {
        assert_eq!(
            SiteTemplate::from_slug("unknown-template"),
            SiteTemplate::Docs
        );
    }

    #[test]
    fn typed_plan_request_compiles_to_matching_family() {
        let plan = WorkflowPlanRequest::Site {
            template: SiteTemplate::NextjsApp,
        }
        .compile();

        assert_eq!(plan.kind(), WorkflowKind::Site);
        let workflow = plan.into_instance();
        assert_eq!(workflow.kind, WorkflowKind::Site);
        assert_eq!(workflow.current_phase.as_str(), "scaffold");
    }
}
