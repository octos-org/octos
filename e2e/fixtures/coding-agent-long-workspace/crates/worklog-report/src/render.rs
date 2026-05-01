use worklog_core::{schedule_order, Worklog};

use crate::owner_loads;

pub fn render_report(worklog: &Worklog) -> Result<String, String> {
    let order = schedule_order(worklog)?;
    let loads = owner_loads(worklog);

    let mut lines = Vec::new();
    lines.push(format!(
        "open: {} tasks, {} minutes",
        order.len(),
        worklog.total_estimate_minutes()
    ));
    lines.push(format!(
        "next: {}",
        order.first().map(String::as_str).unwrap_or("none")
    ));
    lines.push("owners:".to_string());

    for load in loads {
        lines.push(format!(
            "- {}: {} tasks, {} minutes",
            load.owner, load.open_tasks, load.minutes
        ));
    }

    Ok(lines.join("\n"))
}
