use super::*;

#[test]
fn creation_rejects_unschedulable_cli_cron_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let add = |expr: &str| {
        cmd_add(
            &path,
            "fixture".into(),
            "message".into(),
            None,
            Some(expr.into()),
            None,
            false,
            None,
            None,
        )
    };
    for existing in [false, true] {
        if existing {
            add("0 0 9 * * 1").unwrap();
        }
        let before = std::fs::read(&path).ok();
        for expr in [
            "nope",
            "0 0 0 31 2 *",
            "0 0 0 31 2 * *",
            "0 0 0 1 1 * 2000",
            "0 0 0 1 1 * 2100",
        ] {
            assert!(add(expr).is_err(), "accepted {expr}");
            assert_eq!(std::fs::read(&path).ok(), before, "wrote {expr}");
        }
    }
    for expr in [
        "0 0, 30 8 - 9 * JAN, MAR MON",
        "@daily",
        "59 59 23 31 12 * 2099",
    ] {
        add(expr).unwrap();
        let store: CronStore = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(store.jobs.last().unwrap().state.next_run_at_ms.is_some());
    }
}
