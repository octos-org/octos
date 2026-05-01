use crate::Plan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSummary {
    pub task_count: usize,
    pub max_priority: u8,
    pub unique_tags: Vec<String>,
    pub blocked_task_count: usize,
}

pub fn summarize(plan: &Plan) -> PlanSummary {
    PlanSummary {
        task_count: plan.tasks.len(),
        max_priority: 0,
        unique_tags: Vec::new(),
        blocked_task_count: 0,
    }
}
