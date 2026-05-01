use std::collections::HashSet;

use crate::model::{Task, Worklog};

pub fn parse_worklog(input: &str) -> Result<Worklog, String> {
    let mut tasks = Vec::new();
    let mut seen = HashSet::new();

    for (idx, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        if fields.len() != 7 {
            return Err(format!("line {} expected 7 fields", idx + 1));
        }

        let id = fields[0];
        if id.is_empty() {
            return Err(format!("line {} has empty task id", idx + 1));
        }
        if !seen.insert(id.to_string()) {
            return Err(format!("line {} duplicates task id {id}", idx + 1));
        }

        let priority = fields[3]
            .parse::<u8>()
            .map_err(|_| format!("line {} has invalid priority", idx + 1))?;
        let estimate_minutes = fields[4]
            .parse::<u32>()
            .map_err(|_| format!("line {} has invalid estimate", idx + 1))?;
        let done = parse_bool(fields[6])
            .ok_or_else(|| format!("line {} has invalid done flag", idx + 1))?;

        tasks.push(Task {
            id: id.to_string(),
            title: fields[1].to_string(),
            owner: fields[2].to_string(),
            priority,
            estimate_minutes,
            deps: parse_deps(fields[5]),
            done,
        });
    }

    Ok(Worklog { tasks })
}

fn parse_deps(raw: &str) -> Vec<String> {
    if raw == "-" || raw.is_empty() {
        return Vec::new();
    }

    raw.split(',')
        .map(str::trim)
        .filter(|dep| !dep.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}
