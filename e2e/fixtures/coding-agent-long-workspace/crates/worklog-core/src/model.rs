#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub owner: String,
    pub priority: u8,
    pub estimate_minutes: u32,
    pub deps: Vec<String>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worklog {
    pub tasks: Vec<Task>,
}

impl Worklog {
    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn total_estimate_minutes(&self) -> u32 {
        self.tasks
            .iter()
            .filter(|task| !task.done)
            .map(|task| task.estimate_minutes)
            .sum()
    }
}
