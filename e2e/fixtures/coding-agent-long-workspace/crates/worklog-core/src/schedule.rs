use std::collections::{HashMap, HashSet};

use crate::model::Worklog;

pub fn validate_worklog(worklog: &Worklog) -> Result<(), String> {
    let ids: HashSet<&str> = worklog.tasks.iter().map(|task| task.id.as_str()).collect();

    for task in &worklog.tasks {
        if task.title.trim().is_empty() {
            return Err(format!("task {} has empty title", task.id));
        }
        if task.owner.trim().is_empty() {
            return Err(format!("task {} has empty owner", task.id));
        }
        if task.estimate_minutes == 0 {
            return Err(format!("task {} has empty estimate", task.id));
        }
        for dep in &task.deps {
            if !ids.contains(dep.as_str()) {
                return Err(format!("task {} depends on missing task {dep}", task.id));
            }
        }
    }

    Ok(())
}

pub fn ready_tasks(worklog: &Worklog, completed: &[&str]) -> Vec<String> {
    let completed: HashSet<&str> = completed.iter().copied().collect();
    let mut ready: Vec<&crate::model::Task> = worklog
        .tasks
        .iter()
        .filter(|task| !task.done)
        .filter(|task| !completed.contains(task.id.as_str()))
        .filter(|task| task.deps.iter().all(|dep| completed.contains(dep.as_str())))
        .collect();

    ready.sort_by_key(|task| (task.priority, task.id.as_str()));
    ready.into_iter().map(|task| task.id.clone()).collect()
}

pub fn schedule_order(worklog: &Worklog) -> Result<Vec<String>, String> {
    validate_worklog(worklog)?;

    let mut completed: HashSet<String> = worklog
        .tasks
        .iter()
        .filter(|task| task.done)
        .map(|task| task.id.clone())
        .collect();
    let mut remaining: HashMap<String, &crate::model::Task> = worklog
        .tasks
        .iter()
        .filter(|task| !task.done)
        .map(|task| (task.id.clone(), task))
        .collect();
    let mut ordered = Vec::new();

    while !remaining.is_empty() {
        let mut candidates: Vec<&crate::model::Task> = remaining
            .values()
            .copied()
            .filter(|task| task.deps.iter().all(|dep| completed.contains(dep)))
            .collect();

        if candidates.is_empty() {
            return Err("dependency cycle prevents scheduling".to_string());
        }

        candidates.sort_by_key(|task| (task.priority, task.id.as_str()));
        let next = candidates[0];
        remaining.remove(&next.id);
        completed.insert(next.id.clone());
        ordered.push(next.id.clone());
    }

    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_worklog;

    fn sample() -> Worklog {
        parse_worklog(
            r#"
            auth|Implement login flow|alex|90|45|-|false
            db|Add migration|bea|80|30|-|true
            api|Expose session endpoint|alex|70|25|auth,db|false
            docs|Update release notes|cy|10|15|api|false
            ui|Wire login form|bea|70|20|auth|false
            "#,
        )
        .expect("sample parses")
    }

    #[test]
    fn parser_preserves_all_task_fields() {
        let worklog = sample();
        let api = worklog.get("api").expect("api task exists");

        assert_eq!(api.title, "Expose session endpoint");
        assert_eq!(api.owner, "alex");
        assert_eq!(api.priority, 70);
        assert_eq!(api.estimate_minutes, 25);
        assert_eq!(api.deps, vec!["auth", "db"]);
        assert!(!api.done);
    }

    #[test]
    fn validation_rejects_missing_dependencies_and_bad_tasks() {
        let missing = parse_worklog("ship|Deploy|ops|1|15|build|false").unwrap();
        assert!(validate_worklog(&missing).unwrap_err().contains("build"));

        let empty_owner = parse_worklog("task|Needs owner| |1|15|-|false").unwrap();
        assert!(validate_worklog(&empty_owner)
            .unwrap_err()
            .contains("owner"));
    }

    #[test]
    fn ready_tasks_include_done_dependencies_and_sort_by_priority_desc() {
        let worklog = sample();

        assert_eq!(ready_tasks(&worklog, &["db"]), vec!["auth"]);
        assert_eq!(ready_tasks(&worklog, &["db", "auth"]), vec!["api", "ui"]);
    }

    #[test]
    fn schedule_uses_done_tasks_as_satisfied_dependencies() {
        let worklog = sample();

        assert_eq!(
            schedule_order(&worklog).unwrap(),
            vec!["auth", "api", "ui", "docs"]
        );
    }
}
