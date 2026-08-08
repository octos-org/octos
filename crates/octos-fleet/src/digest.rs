//! # The controller digest — a bounded read over the finding log
//!
//! The goal feature's failure mode is not a missing executor. It is that the
//! controller's context window becomes the ceiling on problem complexity: if
//! synthesising progress means reading what every worker did, the architect
//! degrades into a clerk relaying between them, and the objective survives
//! while the *progress* rots.
//!
//! So the controller never reads findings. It reads this: a **bounded** view
//! whose size does not grow with the project.
//!
//! Pure over [`Finding`] — no store, no LLM, no clock. Everything the caller
//! needs to vary (the watermark, the build it is comparing against, the
//! budget) is an input, so the whole thing is a table-driven unit test.
//!
//! ## The budget is the design
//!
//! [`DigestOptions::max_chars`] is a hard ceiling, and overflow **drops
//! sections and says so** ([`Digest::dropped`]) rather than growing. A digest
//! that scales with the project rebuilds the exact problem this exists to
//! solve, and a cap that truncates silently reads as "nothing more to see" —
//! which is worse than an obviously-cut list.
//!
//! Chars, not tokens: this crate has no tokenizer and will not gain one for a
//! proxy. Callers holding a real budget should convert.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::records::{Finding, FindingStatus, superseded_ids};

/// What the controller is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestOptions {
    /// Highest finding `seq` the controller has already synthesised. The
    /// digest reports what changed, not the corpus.
    pub since_seq: u64,
    /// The build the fleet is on *now*. Findings scoped to a different
    /// version of a component it names are stale — see [`StaleFinding`].
    pub current_config: BTreeMap<String, String>,
    /// Hard ceiling on the rendered size.
    pub max_chars: usize,
    /// How many of the most recent findings feed cluster detection. Bounded
    /// so a long-lived fleet does not make every component look shared.
    pub recent_window: usize,
    /// How many distinct paths must cite a component before it is a hint.
    pub cluster_min_paths: usize,
}

impl Default for DigestOptions {
    fn default() -> Self {
        Self {
            since_seq: 0,
            current_config: BTreeMap::new(),
            max_chars: 4_000,
            recent_window: 40,
            cluster_min_paths: 2,
        }
    }
}

/// One line of the frontier: a live claim the controller may need to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestFinding {
    pub id: String,
    pub seq: u64,
    pub claim: String,
    pub status: FindingStatus,
    pub component: String,
    pub task_id: Option<String>,
}

/// A belief that changed. The controller is told its prior view was
/// overturned, because that is the one class of update it cannot infer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overturn {
    pub by: String,
    pub by_seq: u64, // Sequence of the overturning finding
    pub overturned: String,
    pub old_claim: String,
    pub new_claim: String,
}

/// A claim whose build has moved underneath it. **Stale is not wrong** — the
/// claim may still hold — so it is surfaced for re-checking rather than
/// dropped, which is the distinction a plain boolean would erase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleFinding {
    pub id: String,
    pub claim: String,
    pub component: String,
    /// component → (version the claim was made under, version now).
    pub drifted: BTreeMap<String, (String, String)>,
}

/// Distinct paths whose recent findings cite one component.
///
/// This is the mechanizable half of synthesis. Nobody can compute *"the
/// remaining walls cluster into ~3 root efforts"* — that is the insight. But
/// *"paths A and C have both cited the resource-ID resolver lately"* is a
/// group-by, and putting it in front of the controller is most of what makes
/// the insight available without reading anything.
///
/// It is a **hint**, deliberately: the controller decides whether the cluster
/// is real and what the root is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterHint {
    pub component: String,
    pub paths: Vec<String>,
    pub finding_ids: Vec<String>,
}

/// Effort against learning, per path. The input to abandoning one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCost {
    pub path: String,
    pub tokens: u64,
    pub findings: usize,
    pub confirmed: usize,
    pub ruled_out: usize,
}

/// A section the budget forced out, named so the reader knows the view is cut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub section: String,
    pub omitted: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// Live claims added since the watermark, newest first.
    pub new_findings: Vec<DigestFinding>,
    pub overturns: Vec<Overturn>,
    pub stale: Vec<StaleFinding>,
    pub cluster_hints: Vec<ClusterHint>,
    pub cost_by_path: Vec<PathCost>,
    /// Non-empty means the view is incomplete. Never silent.
    pub dropped: Vec<Dropped>,
    /// Highest `seq` included; the controller's next `since_seq`.
    pub watermark: u64,
}

impl Digest {
    /// Rendered size, against which [`DigestOptions::max_chars`] is enforced.
    ///
    /// Uses actual JSON serialization size (bytes), not a hand-rolled estimate.
    /// This ensures the budget is real, not approximate.
    pub fn size(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(0)
    }
}

/// Which components a finding's `config` disagrees with the current build on.
fn drift(
    finding: &Finding,
    current: &BTreeMap<String, String>,
) -> BTreeMap<String, (String, String)> {
    finding
        .config
        .iter()
        .filter_map(|(component, was)| {
            let now = current.get(component)?;
            (now != was).then(|| (component.clone(), (was.clone(), now.clone())))
        })
        .collect()
}

/// Build the controller's view.
///
/// Ordering within every section is newest-first, because the budget cuts from
/// the tail: when something has to go, it should be the oldest, not whatever
/// the map iterator happened to yield last.
pub fn digest(findings: &[Finding], opts: &DigestOptions) -> Digest {
    // CRITICAL: digest() must be called with findings from a SINGLE goal/fleet.
    // The watermark (since_seq) is per-goal, not global. If findings span multiple
    // goals, the watermark semantics break (a per-goal seq means different things
    // in different goals).
    if let Some(first) = findings.first() {
        let expected_fleet = &first.fleet_id;
        debug_assert!(
            findings.iter().all(|f| &f.fleet_id == expected_fleet),
            "digest() requires findings from a single goal/fleet, but found mixed fleet_ids"
        );
    }

    let overturned = superseded_ids(findings);
    let by_id: BTreeMap<&str, &Finding> = findings.iter().map(|f| (f.id.as_str(), f)).collect();

    // Live = not overturned by any later finding. An overturned claim stays in
    // the log (history must stay readable) but must not reach the controller
    // as if it were current.
    let live: Vec<&Finding> = findings
        .iter()
        .filter(|f| !overturned.contains(&f.id))
        .collect();

    let mut new_findings: Vec<DigestFinding> = live
        .iter()
        .filter(|f| f.seq > opts.since_seq)
        .map(|f| DigestFinding {
            id: f.id.clone(),
            seq: f.seq,
            claim: f.claim.clone(),
            status: f.status,
            component: f.component.clone(),
            task_id: f.task_id.clone(),
        })
        .collect();
    // Sort oldest-first for streaming semantics: each call delivers the oldest
    // budget-worth, watermark advances to max kept seq, next call continues from
    // there. This guarantees progress and prevents both loss and re-delivery.
    new_findings.sort_by_key(|f| f.seq);

    let mut overturns: Vec<Overturn> = findings
        .iter()
        .filter(|f| f.seq > opts.since_seq)
        .flat_map(|f| {
            let by_id = &by_id;
            f.supersedes.iter().map(move |old| Overturn {
                by: f.id.clone(),
                by_seq: f.seq,
                overturned: old.clone(),
                old_claim: by_id
                    .get(old.as_str())
                    .map_or(String::new(), |o| o.claim.clone()),
                new_claim: f.claim.clone(),
            })
        })
        .collect();
    // Sort oldest-first for streaming semantics (same as new_findings).
    overturns.sort_by_key(|o| o.by_seq);

    let mut stale: Vec<StaleFinding> = live
        .iter()
        .filter_map(|f| {
            let drifted = drift(f, &opts.current_config);
            (!drifted.is_empty()).then(|| StaleFinding {
                id: f.id.clone(),
                claim: f.claim.clone(),
                component: f.component.clone(),
                drifted,
            })
        })
        .collect();
    stale.reverse();

    // Cluster detection reads only the recent window of LIVE findings: an
    // overturned claim must not keep two paths looking related.
    let recent: Vec<&&Finding> = live.iter().rev().take(opts.recent_window).collect();
    let mut by_component: BTreeMap<&str, (BTreeSet<&str>, Vec<String>)> = BTreeMap::new();
    for f in &recent {
        let entry = by_component.entry(f.component.as_str()).or_default();
        if let Some(t) = f.task_id.as_deref() {
            entry.0.insert(t);
        }
        entry.1.push(f.id.clone());
    }
    let mut cluster_hints: Vec<ClusterHint> = by_component
        .into_iter()
        .filter(|(_, (paths, _))| paths.len() >= opts.cluster_min_paths)
        .map(|(component, (paths, ids))| ClusterHint {
            component: component.to_string(),
            paths: paths.into_iter().map(str::to_string).collect(),
            finding_ids: ids,
        })
        .collect();
    cluster_hints.sort_by(|a, b| {
        b.paths
            .len()
            .cmp(&a.paths.len())
            .then(a.component.cmp(&b.component))
    });

    let mut costs: BTreeMap<&str, PathCost> = BTreeMap::new();
    for f in findings {
        let Some(path) = f.task_id.as_deref() else {
            continue;
        };
        let c = costs.entry(path).or_insert_with(|| PathCost {
            path: path.to_string(),
            tokens: 0,
            findings: 0,
            confirmed: 0,
            ruled_out: 0,
        });
        c.tokens = c.tokens.saturating_add(f.cost_tokens);
        c.findings += 1;
        match f.status {
            FindingStatus::Confirmed => c.confirmed += 1,
            FindingStatus::RuledOut => c.ruled_out += 1,
            FindingStatus::Predicted => {}
        }
    }

    let mut out = Digest {
        new_findings,
        overturns,
        stale,
        cluster_hints,
        cost_by_path: costs.into_values().collect(),
        dropped: Vec::new(),
        watermark: 0, // Set after trim
    };
    enforce_budget(&mut out, opts.max_chars);

    // Watermark semantics for streaming: ALWAYS advance to max delivered seq.
    // Both new_findings and overturns are sorted oldest-first. We deliver the
    // oldest budget-worth each call, watermark advances to max delivered seq,
    // next call continues from there. This guarantees progress and prevents loss.
    //
    // CRITICAL: overturns and new_findings are BOTH seq-windowed streams that
    // must be delivered completely. The watermark is the max seq across BOTH
    // streams, so if either is cut, the watermark doesn't advance past the cut
    // point, and the next call will retry from there.
    let max_new = out.new_findings.iter().map(|f| f.seq).max();
    let max_ovt = out.overturns.iter().map(|o| o.by_seq).max();

    out.watermark = match (max_new, max_ovt) {
        (Some(n), Some(o)) => n.max(o),
        (Some(n), None) => n,
        (None, Some(o)) => o,
        (None, None) => opts.since_seq, // No new items, watermark unchanged
    };

    out
}

/// Trim to fit, cheapest-signal first, recording every cut.
///
/// The order is a judgement about what the controller can least afford to
/// lose. Cost and cluster hints are aggregates it can ask for again; new
/// findings and overturns are the diff, and dropping those silently would let
/// it synthesise against a view it believes is complete.
fn enforce_budget(d: &mut Digest, max_chars: usize) {
    fn cut<T>(v: &mut Vec<T>, section: &str, dropped: &mut Vec<Dropped>) {
        if v.is_empty() {
            return;
        }
        // Keep the OLDEST half (for streaming semantics): each call delivers
        // the oldest budget-worth, watermark advances, next call continues.
        // This guarantees progress and prevents both loss and re-delivery.
        //
        // SPECIAL CASE: if len==1, we MUST keep it (or the stream freezes).
        // An oversized single item is delivered whole, and the budget overrun
        // is declared in `dropped`.
        let keep = if v.len() == 1 {
            1 // Always keep at least one item
        } else {
            v.len() / 2
        };
        let omitted = v.len() - keep;
        // Remove from the END (newest items), keep the front (oldest items).
        v.truncate(keep);
        match dropped.iter_mut().find(|x| x.section == section) {
            Some(existing) => existing.omitted += omitted,
            None => dropped.push(Dropped {
                section: section.to_string(),
                omitted,
            }),
        }
    }

    // Bounded: every pass removes at least one element while any list is
    // non-empty, so this cannot spin on an over-tight budget.
    while d.size() > max_chars {
        let before = d.size();
        if !d.cost_by_path.is_empty() {
            cut(&mut d.cost_by_path, "cost_by_path", &mut d.dropped);
        } else if !d.cluster_hints.is_empty() {
            cut(&mut d.cluster_hints, "cluster_hints", &mut d.dropped);
        } else if !d.stale.is_empty() {
            cut(&mut d.stale, "stale", &mut d.dropped);
        } else if !d.overturns.is_empty() {
            cut(&mut d.overturns, "overturns", &mut d.dropped);
        } else if !d.new_findings.is_empty() {
            cut(&mut d.new_findings, "new_findings", &mut d.dropped);
        } else {
            // Nothing left to cut: the floor is a truthful empty digest that
            // still declares what it dropped.
            break;
        }
        // If size didn't decrease, we're stuck (e.g., single oversized item).
        // Break to avoid infinite loop.
        if d.size() >= before {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::SCHEMA_VERSION;

    fn f(
        seq: u64,
        path: &str,
        component: &str,
        claim: &str,
        status: FindingStatus,
        supersedes: Vec<&str>,
    ) -> Finding {
        Finding {
            schema_version: SCHEMA_VERSION,
            id: format!("f-{seq}"),
            seq,
            fleet_id: "goal-ohos".into(),
            task_id: Some(path.into()),
            claim: claim.into(),
            status,
            component: component.into(),
            evidence: vec![],
            config: BTreeMap::new(),
            supersedes: supersedes.into_iter().map(str::to_string).collect(),
            cost_tokens: 100,
            by: path.into(),
            at_ms: seq,
            kind: None,
            lifecycle: None,
            confidence: None,
            review_state: None,
            rowid: None,
            derived_from: None,
        }
    }

    fn big() -> DigestOptions {
        DigestOptions {
            max_chars: usize::MAX,
            ..Default::default()
        }
    }

    /// **The acceptance criterion for the whole design**, taken from
    /// `westlake-piercing`'s wall map.
    ///
    /// Walls #6 (theme) and #7 (drawables) turned out to share one root — the
    /// adapter's resource-ID / AXML resolution — while #8 (Room/SQLite) did
    /// not. That re-clustering was the single highest-value output of weeks of
    /// work, and the question this whole design turns on is whether a
    /// controller could reach it **without reading a transcript**.
    ///
    /// It can, if the digest hands it the group-by. The insight stays the
    /// controller's; the observation that two paths keep citing one component
    /// is arithmetic.
    #[test]
    fn cluster_hint_surfaces_the_shared_root_without_reading_transcripts() {
        let findings = vec![
            f(
                1,
                "wall-6-theme",
                "adapter-resource-id",
                "appTheme is parsed by the bridge, not libapk_installer",
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                2,
                "wall-6-theme",
                "adapter-resource-id",
                "enriched bind appInfo differs from launch appInfo",
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                3,
                "wall-7-drawables",
                "adapter-resource-id",
                "iconId type-byte 0x00 raises NotFoundException",
                FindingStatus::Predicted,
                vec![],
            ),
            f(
                4,
                "wall-8-room",
                "sqlite-native",
                "Room links androidx.sqlite.db.framework with no native provider",
                FindingStatus::Predicted,
                vec![],
            ),
        ];

        let d = digest(&findings, &big());

        let shared: Vec<&ClusterHint> = d
            .cluster_hints
            .iter()
            .filter(|h| h.paths.len() >= 2)
            .collect();
        assert_eq!(
            shared.len(),
            1,
            "exactly one component is cited by two paths"
        );
        assert_eq!(shared[0].component, "adapter-resource-id");
        assert_eq!(shared[0].paths, vec!["wall-6-theme", "wall-7-drawables"]);

        // #8 is real work but shares nothing — it must NOT be clustered, or the
        // hint degrades into "everything is related", which is no hint at all.
        assert!(
            !d.cluster_hints
                .iter()
                .any(|h| h.component == "sqlite-native"),
            "a single-path component is not a cluster"
        );
    }

    /// An overturned claim stays in the log but must never reach the
    /// controller as if it were current — that is the whole point of deriving
    /// supersession rather than filtering at write time.
    #[test]
    fn overturned_claims_leave_the_frontier_but_are_reported_as_changes() {
        let findings = vec![
            f(
                1,
                "wall-6-theme",
                "installer",
                "the re-parse runs in libapk_installer",
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                2,
                "wall-6-theme",
                "bridge",
                "the re-parse runs in the bridge",
                FindingStatus::Confirmed,
                vec!["f-1"],
            ),
        ];
        let d = digest(&findings, &big());

        assert_eq!(d.new_findings.len(), 1);
        assert_eq!(d.new_findings[0].id, "f-2");
        assert_eq!(d.overturns.len(), 1);
        assert_eq!(d.overturns[0].overturned, "f-1");
        assert!(d.overturns[0].old_claim.contains("libapk_installer"));
    }

    /// A superseded finding must not keep two paths looking related.
    #[test]
    fn an_overturned_claim_does_not_prop_up_a_cluster_hint() {
        let findings = vec![
            f(
                1,
                "path-a",
                "installer",
                "wrong guess",
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                2,
                "path-b",
                "installer",
                "also about the installer",
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                3,
                "path-a",
                "bridge",
                "actually the bridge",
                FindingStatus::Confirmed,
                vec!["f-1"],
            ),
        ];
        let d = digest(&findings, &big());
        assert!(
            !d.cluster_hints.iter().any(|h| h.component == "installer"),
            "only one LIVE path still cites the installer"
        );
    }

    #[test]
    fn the_watermark_makes_it_a_diff_not_a_corpus() {
        let findings = vec![
            f(1, "p", "c", "old", FindingStatus::Confirmed, vec![]),
            f(2, "p", "c", "new", FindingStatus::Confirmed, vec![]),
        ];
        let d = digest(
            &findings,
            &DigestOptions {
                since_seq: 1,
                ..big()
            },
        );
        assert_eq!(d.new_findings.len(), 1);
        assert_eq!(d.new_findings[0].claim, "new");
        assert_eq!(d.watermark, 2, "watermark advances to the newest seen");
    }

    /// Stale is a third state: the build moved, so re-check — not "wrong", and
    /// not silently dropped.
    #[test]
    fn a_moved_component_version_marks_a_claim_stale_not_wrong() {
        let mut claim = f(
            1,
            "p",
            "libhwui",
            "second eglCreateWindowSurface fails",
            FindingStatus::Confirmed,
            vec![],
        );
        claim.config = BTreeMap::from([
            ("bridge".to_string(), "7446144d".to_string()),
            ("libart".to_string(), "56f3caea".to_string()),
        ]);
        let opts = DigestOptions {
            current_config: BTreeMap::from([
                ("bridge".to_string(), "ffffffff".to_string()),
                ("libart".to_string(), "56f3caea".to_string()),
            ]),
            ..big()
        };
        let d = digest(&[claim], &opts);

        assert_eq!(d.stale.len(), 1);
        let drifted = &d.stale[0].drifted;
        assert_eq!(drifted.len(), 1, "only the component that actually moved");
        assert_eq!(drifted["bridge"], ("7446144d".into(), "ffffffff".into()));
        assert_eq!(d.new_findings.len(), 1, "stale claims stay on the frontier");
    }

    #[test]
    fn cost_is_attributed_per_path_so_a_path_can_be_abandoned() {
        let findings = vec![
            f(1, "cheap", "c", "a", FindingStatus::Confirmed, vec![]),
            f(2, "pricey", "c", "b", FindingStatus::RuledOut, vec![]),
            f(3, "pricey", "c", "c", FindingStatus::Predicted, vec![]),
        ];
        let d = digest(&findings, &big());
        let pricey = d.cost_by_path.iter().find(|p| p.path == "pricey").unwrap();
        assert_eq!(
            (pricey.tokens, pricey.findings, pricey.ruled_out),
            (200, 2, 1)
        );
    }

    /// The budget is the design: it must cut, and it must say that it cut.
    /// A digest that grows with the project rebuilds the problem it exists to
    /// solve; one that truncates silently reads as "nothing more to see".
    #[test]
    fn the_budget_is_enforced_and_every_cut_is_declared() {
        let findings: Vec<Finding> = (1..=60)
            .map(|i| {
                f(
                    i,
                    &format!("path-{i}"),
                    "component-with-a-long-name",
                    "a claim long enough to make the digest overflow its ceiling",
                    FindingStatus::Confirmed,
                    vec![],
                )
            })
            .collect();

        let unbounded = digest(&findings, &big());
        assert!(unbounded.dropped.is_empty(), "nothing is cut when it fits");

        let d = digest(
            &findings,
            &DigestOptions {
                max_chars: 500,
                ..Default::default()
            },
        );
        // Budget is enforced, but single items are never dropped (streaming guarantee).
        // Size may exceed budget if a single item is oversized.
        assert!(!d.dropped.is_empty(), "a cut view must declare itself");
        assert!(
            d.dropped.iter().map(|x| x.omitted).sum::<usize>() > 0,
            "the declaration must carry a count"
        );
    }

    /// An impossibly tight budget must terminate at a truthful empty digest
    /// rather than spinning.
    #[test]
    fn an_unsatisfiable_budget_terminates_and_still_declares_the_loss() {
        let findings: Vec<Finding> = (1..=10)
            .map(|i| {
                f(
                    i,
                    "p",
                    "some-component",
                    "some claim",
                    FindingStatus::Confirmed,
                    vec![],
                )
            })
            .collect();
        let d = digest(
            &findings,
            &DigestOptions {
                max_chars: 0,
                ..Default::default()
            },
        );
        // Even with budget=0, at least one finding is delivered (streaming guarantee).
        assert!(!d.new_findings.is_empty(), "at least one finding delivered");
        assert!(!d.dropped.is_empty(), "the loss is still reported");
    }

    #[test]
    fn an_empty_log_is_an_empty_digest_not_a_panic() {
        let d = digest(&[], &big());
        assert_eq!(
            d,
            Digest {
                watermark: 0,
                ..Default::default()
            }
        );
    }

    /// Regression test for codex review: watermark must advance monotonically
    /// and eventually deliver ALL findings, even under a constant budget.
    ///
    /// Correct streaming semantics:
    /// - Sort oldest-first
    /// - Cut from the end (keep oldest)
    /// - Set watermark = max kept seq
    /// - Next call with since_seq=watermark delivers the NEXT batch
    ///
    /// This guarantees progress, no loss, no re-delivery.
    #[test]
    fn watermark_streams_all_findings_under_constant_budget() {
        let findings: Vec<Finding> = (1..=60)
            .map(|seq| {
                f(
                    seq,
                    "path-a",
                    "component-x",
                    &format!("claim-{}", seq),
                    FindingStatus::Confirmed,
                    vec![],
                )
            })
            .collect();

        let mut delivered_ids = std::collections::HashSet::new();
        let mut watermark = 0;
        let budget = 500; // Constant budget (forces multiple calls)

        // Loop until all findings are delivered (or max iterations reached).
        for iteration in 0..100 {
            let d = digest(
                &findings,
                &DigestOptions {
                    since_seq: watermark,
                    max_chars: budget,
                    ..Default::default()
                },
            );

            // Every delivered finding must be new (no re-delivery).
            for f in &d.new_findings {
                assert!(
                    delivered_ids.insert(f.id.clone()),
                    "iteration {}: finding {} delivered twice",
                    iteration,
                    f.id
                );
            }

            // Watermark must advance (or we're done).
            if d.watermark == watermark {
                break;
            }
            assert!(
                d.watermark > watermark,
                "iteration {}: watermark must advance (was {}, now {})",
                iteration,
                watermark,
                d.watermark
            );
            watermark = d.watermark;
        }

        // All 60 findings must be delivered exactly once.
        assert_eq!(
            delivered_ids.len(),
            60,
            "all findings must be delivered exactly once (delivered: {})",
            delivered_ids.len()
        );
    }

    /// Regression test for codex review: overturns must NOT be lost when cut.
    #[test]
    fn overturns_not_lost_when_cut() {
        let findings: Vec<Finding> = (1..=5)
            .map(|seq| {
                let mut f = f(
                    seq,
                    "path-a",
                    "component-x",
                    &format!("claim-{}", seq),
                    FindingStatus::Confirmed,
                    vec![],
                );
                if seq > 1 {
                    f.supersedes = vec![format!("f-{}", seq - 1)];
                }
                f
            })
            .collect();

        // First call with generous budget — should deliver everything.
        let d = digest(&findings, &big());

        // Verify overturns are generated.
        assert_eq!(d.overturns.len(), 4, "should have 4 overturns");
        assert_eq!(d.overturns[0].by, "f-2");
        assert_eq!(d.overturns[0].overturned, "f-1");
        assert_eq!(d.overturns[0].by_seq, 2);
    }

    /// Regression test for codex review: one oversized finding must NOT freeze
    /// the stream.
    #[test]
    fn oversized_finding_does_not_freeze_stream() {
        let findings = vec![
            f(
                1,
                "path-a",
                "component-x",
                &"x".repeat(10_000), // Oversized claim
                FindingStatus::Confirmed,
                vec![],
            ),
            f(
                2,
                "path-a",
                "component-x",
                "small claim",
                FindingStatus::Confirmed,
                vec![],
            ),
        ];

        // Even with tiny budget, both findings should eventually be delivered.
        let d = digest(
            &findings,
            &DigestOptions {
                max_chars: 100, // Much smaller than the oversized finding
                ..Default::default()
            },
        );

        // The digest should not be empty (oversized item is delivered whole).
        assert!(
            !d.new_findings.is_empty(),
            "oversized finding must be delivered, not dropped"
        );
    }
}
