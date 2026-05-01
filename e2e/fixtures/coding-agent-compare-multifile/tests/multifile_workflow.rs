use coding_agent_compare_multifile::{
    execution_order, parse_tasks, ready_tasks, summarize, validate_plan,
};

fn sample_plan() -> &'static str {
    r#"
    # release workflow
    task lint
    command = cargo clippy --all-targets
    priority = 20
    deps =
    tags = rust, ci

    task test
    command = cargo test # inline comments are not part of the command
    priority = 30
    deps = lint
    tags = rust, ci

    task package
    command = cargo build --release
    priority = 10
    deps = test, lint
    tags = release
    "#
}

#[test]
fn parses_multitask_plan_with_comments_and_metadata() {
    let plan = parse_tasks(sample_plan()).expect("plan parses");

    assert_eq!(plan.tasks.len(), 3);
    assert_eq!(plan.tasks[0].name, "lint");
    assert_eq!(plan.tasks[0].command, "cargo clippy --all-targets");
    assert_eq!(plan.tasks[0].priority, 20);
    assert!(plan.tasks[0].deps.is_empty());
    assert_eq!(plan.tasks[0].tags, vec!["rust", "ci"]);

    assert_eq!(plan.tasks[1].name, "test");
    assert_eq!(plan.tasks[1].command, "cargo test");
    assert_eq!(plan.tasks[1].deps, vec!["lint"]);

    assert_eq!(plan.tasks[2].deps, vec!["test", "lint"]);
}

#[test]
fn rejects_duplicate_task_names_and_missing_command() {
    let duplicate = r#"
    task build
    command = cargo build

    task build
    command = cargo build --release
    "#;
    assert!(parse_tasks(duplicate).is_err());

    let missing_command = r#"
    task docs
    priority = 4
    "#;
    assert!(parse_tasks(missing_command).is_err());
}

#[test]
fn rejects_unknown_dependencies() {
    let plan = parse_tasks(
        r#"
        task deploy
        command = ./deploy.sh
        deps = build
        "#,
    )
    .expect("syntax parses");

    let err = validate_plan(&plan).expect_err("unknown dependency is rejected");
    assert!(
        err.contains("build"),
        "error should name missing dep: {err}"
    );
}

#[test]
fn execution_order_places_dependencies_first_and_breaks_ties() {
    let plan = parse_tasks(sample_plan()).expect("plan parses");
    let order = execution_order(&plan).expect("order resolves");
    assert_eq!(order, vec!["lint", "test", "package"]);

    let tied = parse_tasks(
        r#"
        task beta
        command = beta
        priority = 1

        task alpha
        command = alpha
        priority = 1
        "#,
    )
    .expect("tied plan parses");
    assert_eq!(execution_order(&tied).unwrap(), vec!["alpha", "beta"]);
}

#[test]
fn ready_tasks_respect_completed_dependencies_and_priority() {
    let plan = parse_tasks(sample_plan()).expect("plan parses");

    assert_eq!(ready_tasks(&plan, &[]), vec!["lint"]);
    assert_eq!(ready_tasks(&plan, &["lint"]), vec!["test"]);
    assert_eq!(ready_tasks(&plan, &["lint", "test"]), vec!["package"]);
}

#[test]
fn summary_counts_tasks_tags_and_blocked_work() {
    let plan = parse_tasks(sample_plan()).expect("plan parses");
    let summary = summarize(&plan);

    assert_eq!(summary.task_count, 3);
    assert_eq!(summary.max_priority, 30);
    assert_eq!(summary.unique_tags, vec!["ci", "release", "rust"]);
    assert_eq!(summary.blocked_task_count, 2);
}
