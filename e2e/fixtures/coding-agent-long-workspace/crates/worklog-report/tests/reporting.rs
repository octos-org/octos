use worklog_core::parse_worklog;
use worklog_report::{owner_loads, render_report, OwnerLoad};

fn sample() -> worklog_core::Worklog {
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
fn owner_loads_count_only_open_tasks() {
    assert_eq!(
        owner_loads(&sample()),
        vec![
            OwnerLoad {
                owner: "alex".to_string(),
                open_tasks: 2,
                minutes: 70,
            },
            OwnerLoad {
                owner: "bea".to_string(),
                open_tasks: 1,
                minutes: 20,
            },
            OwnerLoad {
                owner: "cy".to_string(),
                open_tasks: 1,
                minutes: 15,
            },
        ]
    );
}

#[test]
fn report_includes_next_task_and_owner_summary() {
    let report = render_report(&sample()).expect("report renders");

    assert_eq!(
        report,
        [
            "open: 4 tasks, 105 minutes",
            "next: auth",
            "owners:",
            "- alex: 2 tasks, 70 minutes",
            "- bea: 1 tasks, 20 minutes",
            "- cy: 1 tasks, 15 minutes",
        ]
        .join("\n")
    );
}
