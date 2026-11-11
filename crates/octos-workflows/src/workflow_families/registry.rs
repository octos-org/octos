use crate::workflow_runtime::WorkflowKind;

use super::{SiteTemplate, WorkflowPlanRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowFamilyDescriptor {
    pub kind: WorkflowKind,
    pub name: &'static str,
    pub default_request: WorkflowPlanRequest,
}

pub const WORKFLOW_FAMILIES: [WorkflowFamilyDescriptor; 4] = [
    WorkflowFamilyDescriptor {
        kind: WorkflowKind::DeepResearch,
        name: "deep_research",
        default_request: WorkflowPlanRequest::DeepResearch,
    },
    WorkflowFamilyDescriptor {
        kind: WorkflowKind::ResearchPodcast,
        name: "research_podcast",
        default_request: WorkflowPlanRequest::ResearchPodcast,
    },
    WorkflowFamilyDescriptor {
        kind: WorkflowKind::Slides,
        name: "slides",
        default_request: WorkflowPlanRequest::Slides,
    },
    WorkflowFamilyDescriptor {
        kind: WorkflowKind::Site,
        name: "site",
        default_request: WorkflowPlanRequest::Site {
            template: SiteTemplate::Docs,
        },
    },
];

pub fn registry() -> &'static [WorkflowFamilyDescriptor] {
    &WORKFLOW_FAMILIES
}

pub fn default_request_for_kind(kind: WorkflowKind) -> WorkflowPlanRequest {
    registry()
        .iter()
        .find(|family| family.kind == kind)
        .map(|family| family.default_request)
        .expect("supported workflow kind")
}
