// TEMPORARY adversarial probe — delete after review.
use octos_fleet::{Digest, DigestOptions, Finding, Goal, GoalLedger, Task, digest_from_ledger};

fn goal(id: &str) -> Goal {
    Goal {
        goal_id: id.into(),
        objective: "o".into(),
        status: "active".into(),
        tokens_used: 0,
        token_budget: 10_000,
        continuations_used: 0,
        revision: 0,
        created_at_ms: 1000,
        updated_at_ms: 1000,
    }
}

fn finding(id: &str, goal_id: &str, task_id: Option<&str>, cost: u64) -> Finding {
    Finding {
        rowid: None,
        finding_id: id.into(),
        seq: 1,
        task_id: task_id.map(str::to_string),
        goal_id: goal_id.into(),
        kind: "observation".into(),
        lifecycle: "verified".into(),
        confidence: "high".into(),
        review_state: "peer_reviewed".into(),
        assertion: "a".repeat(200),
        evidence: None,
        config_version: None,
        derived_from: None,
        supersedes: Vec::new(),
        cost_tokens: cost,
        created_at_ms: 2000,
        created_by: "peer-a".into(),
    }
}

// PROBE 1: are the declared FOREIGN KEYs actually enforced?
#[test]
fn probe_fk_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    // No goal "ghost" is ever created.
    let r = l.append_finding(&finding("f1", "ghost", None, 10));
    println!("PROBE1 finding into nonexistent goal => {:?}", r.is_ok());
    let t = l.create_task(&Task {
        task_id: "t1".into(),
        goal_id: "ghost".into(),
        title: "t".into(),
        detail: "d".into(),
        status: "pending".into(),
        assigned_peer: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    });
    println!("PROBE1 task into nonexistent goal => {:?}", t.is_ok());
}

// PROBE 2: does commit_state_with_audit honour finding.goal_id?
#[test]
fn probe_goal_id_rehoming() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    l.create_goal(&goal("g1")).unwrap();
    l.create_goal(&goal("g2")).unwrap();
    // Finding says it belongs to g2; we commit against g1.
    let f = finding("f1", "g2", None, 10);
    let r = l.commit_state_with_audit("g1", "complete", 0, 3000, Some(&f), None);
    println!("PROBE2 commit ok => {:?}", r.is_ok());
    println!(
        "PROBE2 landed in g1 => {}, landed in g2 => {}",
        l.list_findings_since("g1", 0).unwrap().len(),
        l.list_findings_since("g2", 0).unwrap().len()
    );
}

// PROBE 3: is max_chars actually a hard ceiling?
#[test]
fn probe_budget_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    l.create_goal(&goal("g1")).unwrap();
    l.create_task(&Task {
        task_id: "only-path".into(),
        goal_id: "g1".into(),
        title: "t".into(),
        detail: "d".into(),
        status: "pending".into(),
        assigned_peer: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    })
    .unwrap();
    // 30 findings, ALL on one path => cost_by_path has exactly 1 element.
    for i in 1..=30 {
        l.append_finding(&finding(&format!("f{i}"), "g1", Some("only-path"), 100))
            .unwrap();
    }
    let d: Digest = digest_from_ledger(
        &l,
        "g1",
        &DigestOptions {
            max_chars: 500,
            ..Default::default()
        },
    )
    .unwrap();
    println!(
        "PROBE3 max_chars=500 actual_size={} new_findings={} dropped={:?}",
        d.size(),
        d.new_findings.len(),
        d.dropped
            .iter()
            .map(|x| (x.section.clone(), x.omitted))
            .collect::<Vec<_>>()
    );
}

// PROBE 4: cost tracking when findings carry no task_id.
#[test]
fn probe_cost_without_task() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    l.create_goal(&goal("g1")).unwrap();
    for i in 1..=3 {
        l.append_finding(&finding(&format!("f{i}"), "g1", None, 5_000))
            .unwrap();
    }
    let d = digest_from_ledger(&l, "g1", &DigestOptions::default()).unwrap();
    let total: u64 = d.cost_by_path.iter().map(|p| p.tokens).sum();
    println!(
        "PROBE4 15000 tokens spent; digest reports total={} across {} paths",
        total,
        d.cost_by_path.len()
    );
}

// PROBE 5: cluster hints when fed from the ledger (component == kind).
#[test]
fn probe_cluster_component() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    l.create_goal(&goal("g1")).unwrap();
    for i in 1..=4 {
        l.create_task(&Task {
            task_id: format!("path-{i}"),
            goal_id: "g1".into(),
            title: "t".into(),
            detail: "d".into(),
            status: "pending".into(),
            assigned_peer: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .unwrap();
        l.append_finding(&finding(
            &format!("f{i}"),
            "g1",
            Some(&format!("path-{i}")),
            10,
        ))
        .unwrap();
    }
    let d = digest_from_ledger(
        &l,
        "g1",
        &DigestOptions {
            max_chars: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    println!(
        "PROBE5 4 unrelated paths => cluster_hints={:?}",
        d.cluster_hints
            .iter()
            .map(|c| (c.component.clone(), c.paths.len()))
            .collect::<Vec<_>>()
    );
}

// PROBE 6: config_version -> config (stale detection input).
#[test]
fn probe_config_drift() {
    let dir = tempfile::tempdir().unwrap();
    let l = GoalLedger::open(dir.path().join("l.db")).unwrap();
    l.create_goal(&goal("g1")).unwrap();
    let mut f = finding("f1", "g1", None, 10);
    f.config_version = Some("openharmony-5.0.1".into()); // realistic value
    l.append_finding(&f).unwrap();
    let mut cur = std::collections::BTreeMap::new();
    cur.insert("openharmony".to_string(), "5.1.0".to_string());
    let d = digest_from_ledger(
        &l,
        "g1",
        &DigestOptions {
            current_config: cur,
            max_chars: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    println!("PROBE6 stale entries detected = {}", d.stale.len());
}
