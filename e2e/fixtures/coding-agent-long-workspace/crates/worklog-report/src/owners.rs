use std::collections::BTreeMap;

use worklog_core::Worklog;

#[derive(Debug, PartialEq, Eq)]
pub struct OwnerLoad {
    pub owner: String,
    pub open_tasks: usize,
    pub minutes: u32,
}

pub fn owner_loads(worklog: &Worklog) -> Vec<OwnerLoad> {
    let mut totals: BTreeMap<String, OwnerLoad> = BTreeMap::new();

    for task in &worklog.tasks {
        let entry = totals.entry(task.owner.clone()).or_insert(OwnerLoad {
            owner: task.owner.clone(),
            open_tasks: 0,
            minutes: 0,
        });

        entry.open_tasks += 1;
        entry.minutes += task.estimate_minutes;
    }

    totals.into_values().collect()
}
