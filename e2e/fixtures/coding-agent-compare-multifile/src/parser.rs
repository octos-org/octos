#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    pub command: String,
    pub priority: u8,
    pub deps: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub tasks: Vec<Task>,
}

pub fn parse_tasks(input: &str) -> Result<Plan, String> {
    let mut tasks = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((name, command)) = line.split_once('=') {
            tasks.push(Task {
                name: name.trim().to_string(),
                command: command.trim().to_string(),
                priority: 0,
                deps: Vec::new(),
                tags: Vec::new(),
            });
        }
    }

    Ok(Plan { tasks })
}
