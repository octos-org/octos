use crate::workflow_runtime::WorkflowInstance;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResearchPodcastPlan;

impl ResearchPodcastPlan {
    pub fn compile(self) -> WorkflowInstance {
        crate::workflows::research_podcast::build()
    }
}
