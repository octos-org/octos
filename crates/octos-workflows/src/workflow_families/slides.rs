use crate::workflow_runtime::WorkflowInstance;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlidesPlan;

impl SlidesPlan {
    pub fn compile(self) -> WorkflowInstance {
        crate::workflows::slides_delivery::build()
    }
}
