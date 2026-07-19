use crate::workflow_runtime::WorkflowInstance;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeepResearchPlan;

impl DeepResearchPlan {
    pub fn compile(self) -> WorkflowInstance {
        crate::workflows::research_report::build()
    }
}
