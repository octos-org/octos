use crate::Plan;

pub fn validate_plan(_plan: &Plan) -> Result<(), String> {
    Ok(())
}

pub fn execution_order(plan: &Plan) -> Result<Vec<String>, String> {
    Ok(plan.tasks.iter().map(|task| task.name.clone()).collect())
}

pub fn ready_tasks(plan: &Plan, _completed: &[&str]) -> Vec<String> {
    plan.tasks.iter().map(|task| task.name.clone()).collect()
}
